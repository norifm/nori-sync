//! The room's rules on a fake shell whose clock the test moves.

use std::collections::HashMap;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use crate::*;

const MINUTE: i64 = 60_000_000;
const DAY: i64 = 24 * 60 * MINUTE;
const START: i64 = 1_800_000_000_000_000;

/// Runs a room's call to its end: the fake shell never makes it wait.
fn run<F: Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("a room call waited on the fake shell"),
    }
}

/// One room as a shell holds it.
#[derive(Default)]
struct Shell {
    now: i64,
    open: Vec<u32>,
    members: HashMap<u32, Member>,
    inbox: HashMap<u32, Vec<Vec<u8>>>,
    closed: HashMap<u32, Refusal>,
    store: HashMap<Key, Vec<u8>>,
    alarm: Option<i64>,
    rooms_allowed: Option<u32>,
    store_broken: bool,
    counts: Vec<Count>,
}

impl Platform for Shell {
    type Conn = u32;
    fn now_us(&self) -> i64 {
        self.now
    }
    fn connections(&self) -> Vec<u32> {
        self.open.clone()
    }
    fn member(&self, conn: u32) -> Option<Member> {
        self.members.get(&conn).copied()
    }
    fn set_member(&mut self, conn: u32, member: &Member) {
        self.members.insert(conn, *member);
    }
    fn send(&mut self, conn: u32, frame: &[u8]) {
        assert!(!self.closed.contains_key(&conn), "sent to closed connection {conn}");
        self.inbox.entry(conn).or_default().push(frame.to_vec());
    }
    fn close(&mut self, conn: u32, refusal: Refusal) {
        self.closed.insert(conn, refusal);
    }
    fn count(&mut self, what: Count) {
        self.counts.push(what);
    }
    async fn load(&mut self, key: Key) -> Result<Option<Vec<u8>>, Unavailable> {
        if self.store_broken { Err(Unavailable) } else { Ok(self.store.get(&key).cloned()) }
    }
    async fn save(&mut self, key: Key, value: &[u8]) -> Result<(), Unavailable> {
        self.store.insert(key, value.to_vec());
        Ok(())
    }
    async fn erase(&mut self, keys: &[Key]) -> Result<(), Unavailable> {
        keys.iter().for_each(|k| _ = self.store.remove(k));
        Ok(())
    }
    async fn set_alarm(&mut self, at_us: Option<i64>) -> Result<(), Unavailable> {
        self.alarm = at_us;
        Ok(())
    }
    async fn may_open_room(&mut self, _address: &str) -> bool {
        match &mut self.rooms_allowed {
            Some(0) => false,
            Some(n) => {
                *n -= 1;
                true
            }
            None => true,
        }
    }
}

struct Rig {
    shell: Shell,
    limits: Limits,
    next: u32,
}

impl Rig {
    fn new() -> Rig {
        Rig::with(Limits::default())
    }

    fn with(limits: Limits) -> Rig {
        Rig { shell: Shell { now: START, ..Shell::default() }, limits, next: 0 }
    }

    fn room(&mut self) -> Room<'_, Shell> {
        Room::new(&mut self.shell, &self.limits)
    }

    /// A connection accepted and handed to the room; its frames so far are taken.
    fn join(&mut self, kind: Kind) -> u32 {
        self.next += 1;
        let conn = self.next;
        self.shell.open.push(conn);
        run(self.room().join(conn, kind, "198.51.100.7"));
        conn
    }

    /// Joins and returns the welcome; panics when refused.
    fn member(&mut self, kind: Kind) -> (u32, Welcome) {
        let conn = self.join(kind);
        assert_eq!(self.shell.closed.get(&conn), None, "refused");
        match self.take(conn).as_slice() {
            [first] => match Answer::parse(first) {
                Some(Answer::Welcome(w)) => (conn, w),
                other => panic!("first frame {other:?}"),
            },
            other => panic!("frames {other:?}"),
        }
    }

    fn refused(&mut self, kind: Kind) -> Refusal {
        let conn = self.join(kind);
        self.leave(conn);
        assert!(self.take(conn).is_empty());
        self.shell.closed[&conn]
    }

    fn send(&mut self, conn: u32, request: Request) {
        let frame = request.encode();
        run(self.room().frame(conn, &frame));
    }

    fn take(&mut self, conn: u32) -> Vec<Vec<u8>> {
        self.shell.inbox.remove(&conn).unwrap_or_default()
    }

    /// The connection closed: the shell tells the room and forgets it.
    fn leave(&mut self, conn: u32) {
        self.room().closed(conn);
        self.shell.open.retain(|c| *c != conn);
        self.shell.members.remove(&conn);
    }

    /// Moves the clock, ringing the alarm at its time on the way.
    fn wait(&mut self, us: i64) {
        let end = self.shell.now + us;
        while let Some(at) = self.shell.alarm.filter(|at| *at <= end) {
            self.shell.now = self.shell.now.max(at);
            self.shell.alarm = None;
            run(self.room().alarm()).unwrap();
        }
        self.shell.now = end;
    }
}

fn joined(n: u16) -> Vec<u8> {
    Answer::Joined(n).encode()
}
fn left(n: u16) -> Vec<u8> {
    Answer::Left(n).encode()
}
fn message(from: u16, to: To, payload: &[u8]) -> Vec<u8> {
    Answer::Message { from, to, payload }.encode()
}
fn refused(request: RequestKind, refusal: Refusal, retry_after_ms: u32) -> Vec<u8> {
    Answer::Refused { request, refusal, retry_after_ms }.encode()
}

#[test]
fn members_hear_who_comes_and_goes() {
    let mut rig = Rig::new();
    let (a, wa) = rig.member(Kind::Group);
    assert_eq!((wa.you, wa.others.as_slice(), wa.closes_at), (1, &[][..], None));
    let (b, wb) = rig.member(Kind::Group);
    assert_eq!((wb.you, wb.others.as_slice()), (2, &[1][..]));
    assert_eq!(rig.take(a), [joined(2)]);
    rig.leave(b);
    assert_eq!(rig.take(a), [left(2)]);
}

#[test]
fn a_number_is_not_given_twice_while_the_room_has_members() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Jam);
    let (b, _) = rig.member(Kind::Jam);
    rig.leave(b);
    let (c, wc) = rig.member(Kind::Jam);
    assert_eq!(wc.you, 3, "2 left but was given");
    rig.leave(c);
    // The member that remembers 3 is the only one left; a newcomer still gets 4.
    let (d, wd) = rig.member(Kind::Jam);
    assert_eq!((wd.you, wd.others.as_slice()), (4, &[1][..]));
    rig.leave(a);
    rig.leave(d);
    let (_, we) = rig.member(Kind::Jam);
    assert_eq!(we.you, 1, "an empty room numbers from 1 again");
}

#[test]
fn messages_pass_unchanged_to_the_others_or_to_one() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Jam);
    let (b, _) = rig.member(Kind::Jam);
    let (c, _) = rig.member(Kind::Jam);
    rig.take(a);
    rig.take(b);
    rig.send(a, Request::SendAll(b"\x00sealed\xff"));
    assert_eq!(rig.take(b), [message(1, To::Everyone, b"\x00sealed\xff")]);
    assert_eq!(rig.take(c), [message(1, To::Everyone, b"\x00sealed\xff")]);
    assert!(rig.take(a).is_empty(), "the sender gets its own message back");
    rig.send(c, Request::SendTo { member: 2, payload: b"you" });
    assert_eq!(rig.take(b), [message(3, To::You, b"you")]);
    assert!(rig.take(a).is_empty());
    for member in [9, 3] {
        rig.send(c, Request::SendTo { member, payload: b"x" });
        assert_eq!(rig.take(c), [refused(RequestKind::SendTo, Refusal::NoMember, 0)], "to {member}");
    }
}

#[test]
fn a_frame_over_the_limit_is_refused_and_the_member_stays() {
    let mut rig = Rig::with(Limits { max_frame: 100, ..Limits::default() });
    let (a, _) = rig.member(Kind::Group);
    let (b, _) = rig.member(Kind::Group);
    rig.take(a);
    rig.send(a, Request::SendAll(&[0; 100]));
    assert_eq!(rig.take(a), [refused(RequestKind::SendAll, Refusal::TooLarge, 0)]);
    assert!(rig.take(b).is_empty());
    rig.send(a, Request::SendAll(&[0; 99]));
    assert_eq!(rig.take(b).len(), 1);
}

#[test]
fn the_rate_limits_refuse_once_then_close_at_twice() {
    for limits in [Limits { frames_per_minute: 3, ..Limits::default() }, Limits { bytes_per_minute: 30, ..Limits::default() }] {
        let mut rig = Rig::with(limits.clone());
        let (a, _) = rig.member(Kind::Jam);
        let (b, _) = rig.member(Kind::Jam);
        rig.take(a);
        // Frames of 10 bytes: the fourth is over either limit, the seventh twice over.
        let frame = Request::SendAll(&[7; 9]);
        for _ in 0..3 {
            rig.send(a, frame.clone());
        }
        assert_eq!(rig.take(b).len(), 3, "{limits:?}");
        rig.shell.now += 20_000_000;
        rig.send(a, frame.clone());
        assert_eq!(rig.take(a), [refused(RequestKind::SendAll, Refusal::TooFast, 40_000)]);
        rig.send(a, frame.clone());
        rig.send(a, frame.clone());
        assert!(rig.take(a).is_empty(), "refused again in the same minute");
        assert!(rig.take(b).is_empty());
        rig.send(a, frame.clone());
        assert_eq!(rig.shell.closed.get(&a), Some(&Refusal::TooFast));
        assert_eq!(rig.take(b), [left(1)]);
    }
}

#[test]
fn a_new_minute_starts_the_count_again() {
    let mut rig = Rig::with(Limits { frames_per_minute: 2, ..Limits::default() });
    let (a, _) = rig.member(Kind::Jam);
    let (b, _) = rig.member(Kind::Jam);
    rig.take(a);
    for _ in 0..3 {
        rig.send(a, Request::SendAll(b"x"));
    }
    assert_eq!(rig.take(b).len(), 2);
    rig.wait(MINUTE);
    for _ in 0..2 {
        rig.send(a, Request::SendAll(b"x"));
    }
    assert_eq!(rig.take(b).len(), 2);
    assert_eq!(rig.shell.closed.get(&a), None);
}

#[test]
fn time_answers_with_the_server_clock() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Jam);
    rig.send(a, Request::Time { t1: -42 });
    assert_eq!(rig.take(a), [Answer::Time { t1: -42, t2: START, t3: START }.encode()]);
}

#[test]
fn rooms_refuse_the_wrong_kind_and_the_member_too_many() {
    let mut rig = Rig::with(Limits { max_members: 3, ..Limits::default() });
    for _ in 0..3 {
        rig.member(Kind::Group);
    }
    assert_eq!(rig.refused(Kind::Jam), Refusal::WrongKind);
    assert_eq!(rig.refused(Kind::Group), Refusal::RoomFull);

    let mut rig = Rig::new();
    rig.member(Kind::Meeting);
    rig.member(Kind::Meeting);
    assert_eq!(rig.refused(Kind::Meeting), Refusal::RoomFull);
}

#[test]
fn a_meeting_room_closes_after_its_lifetime_and_stays_closed_an_hour() {
    let mut rig = Rig::new();
    let (a, wa) = rig.member(Kind::Meeting);
    assert_eq!(wa.closes_at, Some(START + 5 * MINUTE));
    rig.wait(4 * MINUTE);
    let (b, wb) = rig.member(Kind::Meeting);
    assert_eq!(wb.closes_at, wa.closes_at, "the second member opened it again");
    rig.take(a);
    rig.wait(MINUTE - 1);
    assert!(rig.shell.closed.is_empty());
    rig.wait(1);
    assert_eq!((rig.shell.closed.get(&a), rig.shell.closed.get(&b)), (Some(&Refusal::RoomClosed), Some(&Refusal::RoomClosed)));
    rig.leave(a);
    rig.leave(b);
    rig.wait(59 * MINUTE);
    assert_eq!(rig.refused(Kind::Meeting), Refusal::RoomClosed);
    assert_eq!(rig.refused(Kind::Group), Refusal::WrongKind);
    rig.wait(MINUTE);
    assert!(rig.shell.store.is_empty());
    let (_, wc) = rig.member(Kind::Meeting);
    assert_eq!(wc.closes_at, Some(rig.shell.now + 5 * MINUTE));
}

#[test]
fn a_jam_room_keeps_nothing_after_its_last_member() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Jam);
    let (b, _) = rig.member(Kind::Jam);
    rig.send(a, Request::SendAll(b"x"));
    rig.send(b, Request::Time { t1: 0 });
    rig.leave(a);
    rig.leave(b);
    assert!(rig.shell.store.is_empty());
    assert_eq!(rig.shell.alarm, None);
    rig.member(Kind::Group);
    assert_eq!(rig.shell.counts.iter().filter(|c| **c == Count::RoomOpened).count(), 2);
}

#[test]
fn a_group_that_never_keeps_a_copy_stores_nothing() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Group);
    let (b, _) = rig.member(Kind::Group);
    rig.send(a, Request::SendAll(b"x"));
    rig.send(b, Request::KeptGet);
    assert_eq!(rig.take(b).last(), Some(&Answer::Kept(None).encode()));
    rig.leave(a);
    rig.leave(b);
    assert!(rig.shell.store.is_empty());
    assert_eq!(rig.shell.alarm, None);
}

#[test]
fn the_kept_copy_needs_its_secret_to_change() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Group);
    let (b, _) = rig.member(Kind::Group);
    rig.take(a);
    let (right, wrong) = ([1; SECRET_LEN], [2; SECRET_LEN]);
    rig.send(a, Request::KeptPut { secret: &right, blob: b"v1" });
    assert_eq!(rig.take(a), [Answer::Done(RequestKind::KeptPut).encode()]);
    assert!(!rig.shell.store.values().any(|v| v == &right), "the secret itself was stored");
    rig.send(b, Request::KeptGet);
    assert_eq!(rig.take(b), [Answer::Kept(Some(b"v1")).encode()]);
    rig.send(b, Request::KeptPut { secret: &wrong, blob: b"evil" });
    rig.send(b, Request::KeptDelete { secret: &wrong });
    assert_eq!(rig.take(b), [refused(RequestKind::KeptPut, Refusal::WrongSecret, 0), refused(RequestKind::KeptDelete, Refusal::WrongSecret, 0)]);
    rig.send(b, Request::KeptPut { secret: &right, blob: b"v2" });
    rig.send(b, Request::KeptGet);
    assert_eq!(rig.take(b), [Answer::Done(RequestKind::KeptPut).encode(), Answer::Kept(Some(b"v2")).encode()]);
    rig.send(a, Request::KeptDelete { secret: &right });
    rig.send(a, Request::KeptGet);
    assert_eq!(rig.take(a), [Answer::Done(RequestKind::KeptDelete).encode(), Answer::Kept(None).encode()]);
    assert!(rig.shell.store.is_empty());
    rig.send(b, Request::KeptPut { secret: &wrong, blob: b"v3" });
    assert_eq!(rig.take(b), [Answer::Done(RequestKind::KeptPut).encode()], "a new secret after the delete");
}

#[test]
fn a_kept_copy_is_deleted_weeks_after_the_last_connection() {
    let mut rig = Rig::new();
    let lifetime = rig.limits.kept_lifetime_us;
    let (a, _) = rig.member(Kind::Group);
    rig.send(a, Request::KeptPut { secret: &[1; SECRET_LEN], blob: b"copy" });
    rig.leave(a);
    rig.wait(lifetime / 2);
    // A connection keeps it for another whole lifetime.
    let (b, _) = rig.member(Kind::Group);
    rig.leave(b);
    rig.wait(lifetime - 1);
    assert!(rig.shell.store.contains_key(&Key::Kept));
    rig.wait(1);
    assert!(rig.shell.store.is_empty());
    assert_eq!(rig.shell.alarm, None);
}

#[test]
fn a_kept_copy_stays_while_a_member_is_connected() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Group);
    rig.send(a, Request::KeptPut { secret: &[1; SECRET_LEN], blob: b"copy" });
    rig.wait(3 * rig.limits.kept_lifetime_us);
    assert!(rig.shell.store.contains_key(&Key::Kept));
    rig.leave(a);
    rig.wait(rig.limits.kept_lifetime_us);
    assert!(rig.shell.store.is_empty());
}

#[test]
fn a_kept_copy_keeps_its_room_a_group() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Group);
    rig.send(a, Request::KeptPut { secret: &[1; SECRET_LEN], blob: b"copy" });
    rig.leave(a);
    assert_eq!(rig.refused(Kind::Jam), Refusal::WrongKind);
    rig.wait(DAY);
    rig.member(Kind::Group);
}

#[test]
fn kept_requests_outside_a_group_room_close_the_connection() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Jam);
    let (b, _) = rig.member(Kind::Jam);
    rig.send(b, Request::KeptGet);
    assert_eq!(rig.shell.closed.get(&b), Some(&Refusal::BadRequest));
    assert_eq!(rig.take(a), [joined(2), left(2)]);
    assert!(rig.shell.store.is_empty());
}

#[test]
fn malformed_frames_and_text_close_the_connection() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Group);
    let (b, _) = rig.member(Kind::Group);
    let (c, _) = rig.member(Kind::Group);
    rig.take(a);
    run(rig.room().frame(b, &[0x03, 1, 2]));
    rig.room().text(c);
    assert_eq!((rig.shell.closed.get(&b), rig.shell.closed.get(&c)), (Some(&Refusal::BadRequest), Some(&Refusal::BadRequest)));
    assert_eq!(rig.take(a), [left(2), left(3)]);
    // The shell hears of the close later: nothing more is announced, and nothing reaches the closed ones.
    rig.leave(b);
    rig.send(a, Request::SendAll(b"x"));
    assert!(rig.take(a).is_empty());
}

#[test]
fn opening_rooms_is_limited_per_address_but_joining_is_not() {
    let mut rig = Rig::new();
    rig.shell.rooms_allowed = Some(1);
    rig.member(Kind::Group);
    rig.member(Kind::Group);

    let mut rig = Rig::new();
    rig.shell.rooms_allowed = Some(0);
    assert_eq!(rig.refused(Kind::Jam), Refusal::TooManyRooms);
    assert!(rig.shell.counts.contains(&Count::Refused(Refusal::TooManyRooms)));
}

#[test]
fn a_broken_store_refuses_without_losing_members() {
    let mut rig = Rig::new();
    let (a, _) = rig.member(Kind::Group);
    rig.shell.store_broken = true;
    assert_eq!(rig.refused(Kind::Group), Refusal::Unavailable);
    rig.send(a, Request::KeptGet);
    assert_eq!(rig.take(a), [refused(RequestKind::KeptGet, Refusal::Unavailable, 0)]);
    assert_eq!(rig.shell.closed.get(&a), None);
}

#[test]
fn connect_requests_are_read_or_refused() {
    let name = "A".repeat(43);
    let path = format!("/room/{name}");
    let plus = format!("/room/{}", "+".repeat(43));
    let path = path.as_str();
    assert_eq!(parse_connect(path, Some("kind=meeting&v=1")), Some(Ok(Connect { name: &name, kind: Kind::Meeting })));
    for (path, query, refusal) in [
        (path, Some("v=2&kind=jam"), Refusal::VersionUnsupported),
        (path, Some("kind=jam"), Refusal::VersionUnsupported),
        (path, None, Refusal::VersionUnsupported),
        (path, Some("v=1&kind=party"), Refusal::BadRequest),
        (path, Some("v=1"), Refusal::BadRequest),
        ("/room/short", Some("v=1&kind=jam"), Refusal::BadRequest),
        (&plus, Some("v=1&kind=jam"), Refusal::BadRequest),
    ] {
        assert_eq!(parse_connect(path, query), Some(Err(refusal)), "{path} {query:?}");
    }
    assert_eq!(parse_connect("/jam", None), None);
}

#[test]
fn refusal_codes_read_back() {
    for refusal in Refusal::ALL {
        assert_eq!(Refusal::from_code(refusal.code()), Some(refusal));
    }
    assert_eq!((Refusal::BadRequest.code(), Refusal::NoMember.code()), (4000, 4011));
}

#[test]
fn asset_links_name_the_app_and_only_well_formed_fingerprints() {
    let good = ["ab"; 32].join(":");
    let json = asset_links(&fingerprints(&format!("{good}, {good}"))).unwrap();
    assert!(json.contains("\"package_name\":\"dev.nori.music\""), "{json}");
    assert_eq!(json.matches(&good.to_ascii_uppercase()).count(), 2, "{json}");
    for bad in ["", "ab:cd", &format!("{}:\"x", ["ab"; 31].join(":"))] {
        assert_eq!(asset_links(&fingerprints(bad)), None, "{bad}");
    }
}
