//! What a room needs from the shell it runs in: one room's connections, its store and alarm, the clock.

use crate::{Member, Refusal};

/// What a room keeps in its store. Nothing else is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A group room's kept copy.
    Kept,
    /// The SHA-256 of the kept copy's write secret.
    Secret,
    /// When a member last connected to a group room that holds a kept copy, µs, big-endian.
    Seen,
    /// When a meeting room opened, µs, big-endian.
    Opened,
}

impl Key {
    pub const ALL: [Key; 4] = [Key::Kept, Key::Secret, Key::Seen, Key::Opened];

    pub fn name(self) -> &'static str {
        match self {
            Key::Kept => "kept",
            Key::Secret => "secret",
            Key::Seen => "seen",
            Key::Opened => "opened",
        }
    }
}

/// The store could not be read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable;

/// An event the operator counts. Never anything that names a room or a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    Joined,
    Left,
    RoomOpened,
    /// A message delivered, to one member or to all.
    Message,
    Refused(Refusal),
}

/// One room as its shell holds it. A connection is in [`Platform::connections`] from the moment the shell
/// accepts it until the shell learns it closed; its [`Member`] is what the room set on it.
#[allow(async_fn_in_trait)]
pub trait Platform {
    type Conn: Copy + Eq + core::fmt::Debug;

    /// The server's clock, µs since the epoch.
    fn now_us(&self) -> i64;
    fn connections(&self) -> Vec<Self::Conn>;
    fn member(&self, conn: Self::Conn) -> Option<Member>;
    fn set_member(&mut self, conn: Self::Conn, member: &Member);
    fn send(&mut self, conn: Self::Conn, frame: &[u8]);
    /// Closes a connection with a refusal's code and name.
    fn close(&mut self, conn: Self::Conn, refusal: Refusal);
    fn count(&mut self, what: Count);

    async fn load(&mut self, key: Key) -> Result<Option<Vec<u8>>, Unavailable>;
    async fn save(&mut self, key: Key, value: &[u8]) -> Result<(), Unavailable>;
    async fn erase(&mut self, keys: &[Key]) -> Result<(), Unavailable>;
    /// Sets the room's one alarm, or clears it with None. When it rings, the shell calls [`crate::Room::alarm`].
    async fn set_alarm(&mut self, at_us: Option<i64>) -> Result<(), Unavailable>;
    /// Whether the network address `address` may open one more room now.
    async fn may_open_room(&mut self, address: &str) -> bool;
}
