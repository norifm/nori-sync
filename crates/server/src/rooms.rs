//! Every room, owned by one task that handles one event at a time, as a Durable Object does. A room is
//! dropped from memory when it has no connection and nothing stored.

use std::collections::{BTreeSet, HashMap};
use std::hash::{BuildHasher, RandomState};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use nori_sync_room::{Count, Key, Kind, Limits, Member, Platform, Refusal, Room, Unavailable};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::config::Config;
use crate::store::{Folder, RoomKey, Stored};

/// Frames queued for one connection before it counts as too slow and is dropped.
pub const QUEUE: usize = 256;

/// A failed store or alarm call is tried again this much later.
const RETRY_US: i64 = 60_000_000;

pub fn room_key(name: &str) -> RoomKey {
    use sha2::Digest;
    sha2::Sha256::digest(name.as_bytes()).into()
}

fn now_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as i64)
}

/// What a connection's task hands to its socket.
pub enum Out {
    Frame(Vec<u8>),
    Close(u16, &'static str),
}

pub enum Event {
    Open { room: RoomKey, conn: u64, kind: Kind, address: String, out: mpsc::Sender<Out> },
    Frame { room: RoomKey, conn: u64, data: Bytes },
    Text { room: RoomKey, conn: u64 },
    Closed { room: RoomKey, conn: u64 },
    Shutdown,
}

/// The operator's counts, read by `/stats`.
#[derive(Default)]
pub struct Counts {
    next_conn: AtomicU64,
    connections: AtomicU64,
    rooms: AtomicU64,
    joined: AtomicU64,
    left: AtomicU64,
    rooms_opened: AtomicU64,
    messages: AtomicU64,
    refused: [AtomicU64; Refusal::ALL.len()],
}

impl Counts {
    pub fn new_connection(&self) -> u64 {
        self.next_conn.fetch_add(1, Relaxed)
    }

    pub fn add(&self, what: Count) {
        let counter = match what {
            Count::Joined => &self.joined,
            Count::Left => &self.left,
            Count::RoomOpened => &self.rooms_opened,
            Count::Message => &self.messages,
            Count::Refused(r) => &self.refused[usize::from(r.code() - 4000)],
        };
        counter.fetch_add(1, Relaxed);
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut line = |name: &str, kind: &str, value: &AtomicU64| {
            out += &format!("# TYPE nori_sync_{name} {kind}\nnori_sync_{name} {}\n", value.load(Relaxed));
        };
        line("connections", "gauge", &self.connections);
        line("rooms", "gauge", &self.rooms);
        line("joined_total", "counter", &self.joined);
        line("left_total", "counter", &self.left);
        line("rooms_opened_total", "counter", &self.rooms_opened);
        line("messages_total", "counter", &self.messages);
        out += "# TYPE nori_sync_refused_total counter\n";
        for (refusal, value) in Refusal::ALL.iter().zip(&self.refused) {
            out += &format!("nori_sync_refused_total{{reason=\"{}\"}} {}\n", refusal.name(), value.load(Relaxed));
        }
        out
    }
}

struct Conn {
    id: u64,
    /// None once the connection fell too far behind: its task then closes it.
    out: Option<mpsc::Sender<Out>>,
    member: Option<Member>,
}

#[derive(Default)]
struct RoomState {
    conns: Vec<Conn>,
    stored: Stored,
}

/// Rooms opened per network address in the current minute, under a hash keyed at random when the server
/// starts: no address is kept, and none is written anywhere.
struct Openers {
    hasher: RandomState,
    minute_start: i64,
    opened: HashMap<u64, u32>,
}

impl Openers {
    fn allow(&mut self, address: &str, now: i64, limit: u32) -> bool {
        if !(self.minute_start..self.minute_start + 60_000_000).contains(&now) {
            self.minute_start = now;
            self.opened.clear();
        }
        let count = self.opened.entry(self.hasher.hash_one(address)).or_default();
        *count += 1;
        *count <= limit
    }
}

pub struct Rooms {
    rooms: HashMap<RoomKey, RoomState>,
    alarms: BTreeSet<(i64, RoomKey)>,
    limits: Limits,
    folder: Folder,
    openers: Openers,
    counts: Arc<Counts>,
    debug_events: bool,
}

/// One room's view of the server, for one event.
struct Here<'a> {
    room: &'a RoomKey,
    state: &'a mut RoomState,
    folder: &'a Folder,
    openers: &'a mut Openers,
    counts: &'a Counts,
    limits: &'a Limits,
}

impl Here<'_> {
    fn conn(&mut self, id: u64) -> Option<&mut Conn> {
        self.state.conns.iter_mut().find(|c| c.id == id)
    }

    fn write(&mut self, change: impl FnOnce(&mut Stored)) -> Result<(), Unavailable> {
        let mut stored = self.state.stored.clone();
        change(&mut stored);
        self.folder.write(self.room, &stored).map_err(|e| eprintln!("nori-sync: a room file could not be written: {e}")).map_err(|_| Unavailable)?;
        self.state.stored = stored;
        Ok(())
    }
}

impl Platform for Here<'_> {
    type Conn = u64;

    fn now_us(&self) -> i64 {
        now_us()
    }

    fn connections(&self) -> Vec<u64> {
        self.state.conns.iter().map(|c| c.id).collect()
    }

    fn member(&self, conn: u64) -> Option<Member> {
        self.state.conns.iter().find(|c| c.id == conn).and_then(|c| c.member)
    }

    fn set_member(&mut self, conn: u64, member: &Member) {
        if let Some(c) = self.conn(conn) {
            c.member = Some(*member);
        }
    }

    fn send(&mut self, conn: u64, frame: &[u8]) {
        if let Some(c) = self.conn(conn)
            && c.out.as_ref().is_some_and(|out| out.try_send(Out::Frame(frame.to_vec())).is_err())
        {
            c.out = None;
        }
    }

    fn close(&mut self, conn: u64, refusal: Refusal) {
        if let Some(out) = self.conn(conn).and_then(|c| c.out.take()) {
            let _ = out.try_send(Out::Close(refusal.code(), refusal.name()));
        }
    }

    fn count(&mut self, what: Count) {
        self.counts.add(what);
    }

    async fn load(&mut self, key: Key) -> Result<Option<Vec<u8>>, Unavailable> {
        Ok(self.state.stored.values.get(&key).cloned())
    }

    async fn save(&mut self, key: Key, value: &[u8]) -> Result<(), Unavailable> {
        self.write(|s| _ = s.values.insert(key, value.to_vec()))
    }

    async fn erase(&mut self, keys: &[Key]) -> Result<(), Unavailable> {
        self.write(|s| keys.iter().for_each(|k| _ = s.values.remove(k)))
    }

    async fn set_alarm(&mut self, at_us: Option<i64>) -> Result<(), Unavailable> {
        self.write(|s| s.alarm = at_us)
    }

    async fn may_open_room(&mut self, address: &str) -> bool {
        self.openers.allow(address, now_us(), self.limits.rooms_per_minute)
    }
}

impl Rooms {
    pub fn load(config: &Config, counts: Arc<Counts>) -> std::io::Result<Rooms> {
        let folder = Folder::open(&config.data)?;
        let mut rooms = HashMap::new();
        let mut alarms = BTreeSet::new();
        for (room, stored) in folder.load_all()? {
            if let Some(at) = stored.alarm {
                alarms.insert((at, room));
            }
            rooms.insert(room, RoomState { conns: Vec::new(), stored });
        }
        counts.rooms.store(rooms.len() as u64, Relaxed);
        let openers = Openers { hasher: RandomState::new(), minute_start: 0, opened: HashMap::new() };
        Ok(Rooms { rooms, alarms, limits: config.limits.clone(), folder, openers, counts, debug_events: config.debug_events })
    }

    pub async fn run(mut self, mut inbox: mpsc::UnboundedReceiver<Event>) {
        loop {
            let next = self.alarms.first().map(|(at, _)| *at);
            let ring = async {
                match next {
                    Some(at) => tokio::time::sleep_until(Instant::now() + Duration::from_micros((at - now_us()).max(0) as u64)).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                event = inbox.recv() => match event {
                    Some(event) => self.handle(event).await,
                    None => return,
                },
                () = ring => self.ring_alarms().await,
            }
        }
    }

    async fn handle(&mut self, event: Event) {
        let room = match &event {
            Event::Open { room, .. } | Event::Frame { room, .. } | Event::Text { room, .. } | Event::Closed { room, .. } => *room,
            Event::Shutdown => {
                for conn in self.rooms.values_mut().flat_map(|r| &mut r.conns) {
                    if let Some(out) = conn.out.take() {
                        let _ = out.try_send(Out::Close(1001, "going away"));
                    }
                }
                return;
            }
        };
        if self.debug_events {
            eprintln!("room event");
        }
        let state = self.rooms.entry(room).or_default();
        let before = state.stored.alarm;
        let mut here = Here { room: &room, state, folder: &self.folder, openers: &mut self.openers, counts: &self.counts, limits: &self.limits };
        match event {
            Event::Open { conn, kind, address, out, .. } => {
                self.counts.connections.fetch_add(1, Relaxed);
                here.state.conns.push(Conn { id: conn, out: Some(out), member: None });
                Room::new(&mut here, &self.limits).join(conn, kind, &address).await;
            }
            Event::Frame { conn, data, .. } => Room::new(&mut here, &self.limits).frame(conn, &data).await,
            Event::Text { conn, .. } => Room::new(&mut here, &self.limits).text(conn),
            Event::Closed { conn, .. } => {
                self.counts.connections.fetch_sub(1, Relaxed);
                Room::new(&mut here, &self.limits).closed(conn);
                here.state.conns.retain(|c| c.id != conn);
            }
            Event::Shutdown => unreachable!(),
        }
        self.settle(room, before);
    }

    async fn ring_alarms(&mut self) {
        let now = now_us();
        while let Some(&(at, room)) = self.alarms.first().filter(|(at, _)| *at <= now) {
            self.alarms.pop_first();
            let Some(state) = self.rooms.get_mut(&room) else { continue };
            state.stored.alarm = None;
            if self.debug_events {
                eprintln!("room event");
            }
            let mut here = Here { room: &room, state, folder: &self.folder, openers: &mut self.openers, counts: &self.counts, limits: &self.limits };
            if Room::new(&mut here, &self.limits).alarm().await.is_err() {
                here.state.stored.alarm = Some(now + RETRY_US);
            }
            self.settle(room, Some(at));
        }
    }

    /// After an event: the alarm list follows the room's alarm, and an empty room leaves memory.
    fn settle(&mut self, room: RoomKey, alarm_before: Option<i64>) {
        let Some(state) = self.rooms.get(&room) else { return };
        if state.stored.alarm != alarm_before {
            if let Some(at) = alarm_before {
                self.alarms.remove(&(at, room));
            }
            if let Some(at) = state.stored.alarm {
                self.alarms.insert((at, room));
            }
        }
        if state.conns.is_empty() && state.stored.is_empty() {
            self.rooms.remove(&room);
        }
        self.counts.rooms.store(self.rooms.len() as u64, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_address_opens_its_limit_per_minute() {
        let mut openers = Openers { hasher: RandomState::new(), minute_start: 0, opened: HashMap::new() };
        let start = 1_000_000_000;
        assert!(openers.allow("a", start, 2));
        assert!(openers.allow("a", start + 1, 2));
        assert!(!openers.allow("a", start + 2, 2));
        assert!(openers.allow("b", start + 3, 2));
        assert!(openers.allow("a", start + 60_000_000, 2));
    }
}
