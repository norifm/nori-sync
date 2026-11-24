//! One room's rules: who may join, what each frame does, lifetimes. Every call handles one event and
//! returns; all it knows comes from the connections' [`Member`]s and the store.

use sha2::{Digest, Sha256};

use crate::frame::{Answer, Request, RequestKind, To, Welcome};
use crate::limits::Limits;
use crate::platform::{Count, Key, Platform, Unavailable};
use crate::{Kind, Refusal, VERSION};

/// What travels with a connection: its member number and its use of the rate limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Member {
    pub number: u16,
    pub kind: Kind,
    /// The highest number given since the room was last empty, as far as this member was told; with the
    /// others' numbers it keeps a number from being given twice.
    highest: u16,
    window_start_us: i64,
    window_frames: u32,
    window_bytes: u32,
    /// This window's `too_fast` refusal was sent.
    warned: bool,
    /// The server closed this connection and announced that it left.
    gone: bool,
}

enum Pace {
    Within,
    /// Over a limit: the frame is dropped, refused once per window with the wait in ms.
    Over { refuse: Option<u32> },
    /// Twice over: the connection is closed.
    Far,
}

impl Member {
    fn new(number: u16, highest: u16, kind: Kind, now_us: i64) -> Member {
        Member { number, kind, highest: highest.max(number), window_start_us: now_us, window_frames: 0, window_bytes: 0, warned: false, gone: false }
    }

    fn top(&self) -> u16 {
        self.number.max(self.highest)
    }

    fn pace(&mut self, len: usize, now_us: i64, limits: &Limits) -> Pace {
        if !(self.window_start_us..self.window_start_us + Limits::WINDOW_US).contains(&now_us) {
            *self = Member { window_start_us: now_us, window_frames: 0, window_bytes: 0, warned: false, ..*self };
        }
        self.window_frames = self.window_frames.saturating_add(1);
        self.window_bytes = self.window_bytes.saturating_add(u32::try_from(len).unwrap_or(u32::MAX));
        let over = |used: u32, limit: u32, times: u64| u64::from(used) > u64::from(limit) * times;
        if over(self.window_frames, limits.frames_per_minute, 2) || over(self.window_bytes, limits.bytes_per_minute, 2) {
            Pace::Far
        } else if over(self.window_frames, limits.frames_per_minute, 1) || over(self.window_bytes, limits.bytes_per_minute, 1) {
            let wait_ms = (self.window_start_us + Limits::WINDOW_US - now_us + 999) / 1000;
            Pace::Over { refuse: (!std::mem::replace(&mut self.warned, true)).then(|| u32::try_from(wait_ms).unwrap_or(u32::MAX)) }
        } else {
            Pace::Within
        }
    }
}

/// A room handling one event on its shell's [`Platform`].
pub struct Room<'a, P: Platform> {
    p: &'a mut P,
    limits: &'a Limits,
}

impl<'a, P: Platform> Room<'a, P> {
    pub fn new(platform: &'a mut P, limits: &'a Limits) -> Room<'a, P> {
        Room { p: platform, limits }
    }

    /// The connections that are members now.
    fn members(&self) -> Vec<(P::Conn, Member)> {
        self.p.connections().into_iter().filter_map(|c| self.p.member(c).filter(|m| !m.gone).map(|m| (c, m))).collect()
    }

    async fn load_time(&mut self, key: Key) -> Result<Option<i64>, Unavailable> {
        Ok(self.p.load(key).await?.and_then(|v| Some(i64::from_be_bytes(v.try_into().ok()?))))
    }

    /// A connection the shell accepted for this room, from `address`, asking for a `kind` of room: it
    /// becomes a member, or is closed with the reason.
    pub async fn join(&mut self, conn: P::Conn, kind: Kind, address: &str) {
        if let Err(refusal) = self.admit(conn, kind, address).await {
            self.p.count(Count::Refused(refusal));
            self.p.close(conn, refusal);
        }
    }

    async fn admit(&mut self, conn: P::Conn, kind: Kind, address: &str) -> Result<(), Refusal> {
        let others: Vec<(P::Conn, Member)> = self.members().into_iter().filter(|(c, _)| *c != conn).collect();
        let now = self.p.now_us();
        let unavailable = |_: Unavailable| Refusal::Unavailable;
        let opened = self.load_time(Key::Opened).await.map_err(unavailable)?;
        let seen = self.load_time(Key::Seen).await.map_err(unavailable)?;
        let existing = others.first().map(|(_, m)| m.kind).or(opened.map(|_| Kind::Meeting)).or(seen.map(|_| Kind::Group));
        if existing.is_some_and(|k| k != kind) {
            return Err(Refusal::WrongKind);
        }
        if opened.is_some_and(|o| now >= o + self.limits.meeting_lifetime_us) {
            return Err(Refusal::RoomClosed);
        }
        if others.len() >= usize::from(self.limits.members(kind)) {
            return Err(Refusal::RoomFull);
        }
        if others.is_empty() && !self.p.may_open_room(address).await {
            return Err(Refusal::TooManyRooms);
        }
        let closes_at = match kind {
            Kind::Meeting => {
                let opened = match opened {
                    Some(opened) => opened,
                    None => {
                        self.p.save(Key::Opened, &now.to_be_bytes()).await.map_err(unavailable)?;
                        self.p.set_alarm(Some(now + self.limits.meeting_lifetime_us)).await.map_err(unavailable)?;
                        now
                    }
                };
                Some(opened + self.limits.meeting_lifetime_us)
            }
            Kind::Group => {
                // Use keeps a kept copy; written at most eight times per lifetime.
                if seen.is_some_and(|s| now - s >= self.limits.kept_lifetime_us / 8) {
                    self.p.save(Key::Seen, &now.to_be_bytes()).await.map_err(unavailable)?;
                    self.p.set_alarm(Some(now + self.limits.kept_lifetime_us)).await.map_err(unavailable)?;
                }
                None
            }
            Kind::Jam => None,
        };
        let highest = others.iter().map(|(_, m)| m.top()).max().unwrap_or(0);
        let number = match highest.checked_add(1) {
            Some(next) => next,
            None => (1..=u16::MAX).find(|n| others.iter().all(|(_, m)| m.number != *n)).ok_or(Refusal::RoomFull)?,
        };
        let me = Member::new(number, highest, kind, now);
        self.p.set_member(conn, &me);
        let welcome = Welcome {
            version: VERSION,
            kind,
            you: number,
            max_frame: self.limits.max_frame,
            frames_per_minute: self.limits.frames_per_minute,
            bytes_per_minute: self.limits.bytes_per_minute,
            max_members: self.limits.members(kind),
            closes_at,
            others: others.iter().map(|(_, m)| m.number).collect(),
        };
        self.p.send(conn, &Answer::Welcome(welcome).encode());
        let joined = Answer::Joined(number).encode();
        for (c, _) in &others {
            self.p.send(*c, &joined);
        }
        self.p.count(Count::Joined);
        if others.is_empty() {
            self.p.count(Count::RoomOpened);
        }
        Ok(())
    }

    /// A binary frame from a connection.
    pub async fn frame(&mut self, conn: P::Conn, frame: &[u8]) {
        let Some(mut me) = self.p.member(conn).filter(|m| !m.gone) else { return };
        let now = self.p.now_us();
        let request = RequestKind::of(frame);
        match me.pace(frame.len(), now, self.limits) {
            Pace::Within => self.p.set_member(conn, &me),
            Pace::Over { refuse } => {
                self.p.set_member(conn, &me);
                if let Some(wait_ms) = refuse {
                    self.refuse(conn, request, Refusal::TooFast, wait_ms);
                }
                return;
            }
            Pace::Far => return self.drop_member(conn, me, Refusal::TooFast),
        }
        if frame.len() > self.limits.max_frame as usize {
            return self.refuse(conn, request, Refusal::TooLarge, 0);
        }
        let Some(parsed) = Request::parse(frame) else { return self.drop_member(conn, me, Refusal::BadRequest) };
        if matches!(parsed, Request::KeptGet | Request::KeptPut { .. } | Request::KeptDelete { .. }) && me.kind != Kind::Group {
            return self.drop_member(conn, me, Refusal::BadRequest);
        }
        let result = match parsed {
            Request::SendAll(payload) => {
                let message = Answer::Message { from: me.number, to: To::Everyone, payload }.encode();
                for (c, _) in self.members().into_iter().filter(|(c, _)| *c != conn) {
                    self.p.send(c, &message);
                }
                self.p.count(Count::Message);
                Ok(())
            }
            Request::SendTo { member, payload } => {
                match self.members().into_iter().find(|(c, m)| m.number == member && *c != conn) {
                    Some((c, _)) => {
                        self.p.send(c, &Answer::Message { from: me.number, to: To::You, payload }.encode());
                        self.p.count(Count::Message);
                    }
                    None => self.refuse(conn, request, Refusal::NoMember, 0),
                }
                Ok(())
            }
            Request::Time { t1 } => {
                let t3 = self.p.now_us();
                self.p.send(conn, &Answer::Time { t1, t2: now, t3 }.encode());
                Ok(())
            }
            Request::KeptGet => self.p.load(Key::Kept).await.map(|kept| self.p.send(conn, &Answer::Kept(kept.as_deref()).encode())),
            Request::KeptPut { secret, blob } => self.put_kept(conn, secret, blob, now).await,
            Request::KeptDelete { secret } => self.delete_kept(conn, secret).await,
        };
        if result.is_err() {
            self.refuse(conn, request, Refusal::Unavailable, 0);
        }
    }

    async fn put_kept(&mut self, conn: P::Conn, secret: &[u8], blob: &[u8], now: i64) -> Result<(), Unavailable> {
        let hash = Sha256::digest(secret);
        let stored = self.p.load(Key::Secret).await?;
        if stored.as_deref().is_some_and(|s| !same(s, &hash)) {
            self.refuse(conn, RequestKind::KeptPut, Refusal::WrongSecret, 0);
            return Ok(());
        }
        if stored.is_none() {
            self.p.save(Key::Secret, &hash).await?;
        }
        self.p.save(Key::Kept, blob).await?;
        self.p.save(Key::Seen, &now.to_be_bytes()).await?;
        self.p.set_alarm(Some(now + self.limits.kept_lifetime_us)).await?;
        self.p.send(conn, &Answer::Done(RequestKind::KeptPut).encode());
        Ok(())
    }

    async fn delete_kept(&mut self, conn: P::Conn, secret: &[u8]) -> Result<(), Unavailable> {
        match self.p.load(Key::Secret).await? {
            Some(stored) if !same(&stored, &Sha256::digest(secret)) => {
                self.refuse(conn, RequestKind::KeptDelete, Refusal::WrongSecret, 0);
                return Ok(());
            }
            Some(_) => {
                self.p.erase(&[Key::Kept, Key::Secret, Key::Seen]).await?;
                self.p.set_alarm(None).await?;
            }
            None => {}
        }
        self.p.send(conn, &Answer::Done(RequestKind::KeptDelete).encode());
        Ok(())
    }

    /// A text frame other than the keep-alive.
    pub fn text(&mut self, conn: P::Conn) {
        if let Some(me) = self.p.member(conn).filter(|m| !m.gone) {
            self.drop_member(conn, me, Refusal::BadRequest);
        }
    }

    /// The connection closed, or the network dropped it. The shell may still list it afterwards.
    pub fn closed(&mut self, conn: P::Conn) {
        if let Some(me) = self.p.member(conn).filter(|m| !m.gone) {
            self.leave(conn, me);
        }
    }

    /// The room's alarm rang: a meeting room closes or is forgotten, an unused kept copy is deleted.
    pub async fn alarm(&mut self) -> Result<(), Unavailable> {
        let now = self.p.now_us();
        if let Some(opened) = self.load_time(Key::Opened).await? {
            let closes = opened + self.limits.meeting_lifetime_us;
            let forgotten = closes + self.limits.meeting_closed_us;
            if now >= forgotten {
                self.p.erase(&Key::ALL).await?;
                return self.p.set_alarm(None).await;
            }
            if now >= closes {
                for (c, mut m) in self.members() {
                    m.gone = true;
                    self.p.set_member(c, &m);
                    self.p.count(Count::Left);
                    self.p.close(c, Refusal::RoomClosed);
                }
                return self.p.set_alarm(Some(forgotten)).await;
            }
            return self.p.set_alarm(Some(closes)).await;
        }
        if let Some(seen) = self.load_time(Key::Seen).await? {
            if !self.members().is_empty() {
                self.p.save(Key::Seen, &now.to_be_bytes()).await?;
                return self.p.set_alarm(Some(now + self.limits.kept_lifetime_us)).await;
            }
            if now >= seen + self.limits.kept_lifetime_us {
                self.p.erase(&[Key::Kept, Key::Secret, Key::Seen]).await?;
                return self.p.set_alarm(None).await;
            }
            return self.p.set_alarm(Some(seen + self.limits.kept_lifetime_us)).await;
        }
        self.p.set_alarm(None).await
    }

    fn refuse(&mut self, conn: P::Conn, request: RequestKind, refusal: Refusal, retry_after_ms: u32) {
        self.p.count(Count::Refused(refusal));
        self.p.send(conn, &Answer::Refused { request, refusal, retry_after_ms }.encode());
    }

    /// Closes a member's connection for a refusal; the others hear that it left.
    fn drop_member(&mut self, conn: P::Conn, me: Member, refusal: Refusal) {
        self.leave(conn, me);
        self.p.count(Count::Refused(refusal));
        self.p.close(conn, refusal);
    }

    fn leave(&mut self, conn: P::Conn, mut me: Member) {
        me.gone = true;
        self.p.set_member(conn, &me);
        let left = Answer::Left(me.number).encode();
        for (c, mut m) in self.members() {
            self.p.send(c, &left);
            if m.highest < me.top() {
                m.highest = me.top();
                self.p.set_member(c, &m);
            }
        }
        self.p.count(Count::Left);
    }
}

/// Compares two hashes in time that does not depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}
