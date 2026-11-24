//! The nori sync server's rooms, platform-free (PROTOCOL.md is the contract). A shell (the Cloudflare
//! Worker, the self-hosted program) owns the connections, a small store and an alarm per room, and hands
//! each event to [`Room`]. Nothing lives in memory between events: what a room needs travels with each
//! connection ([`Member`]) or sits in its store, so a room can be unloaded whenever it is idle.

mod frame;
mod limits;
mod platform;
mod room;
#[cfg(test)]
mod tests;

pub use frame::{Answer, Request, RequestKind, SECRET_LEN, To, Welcome};
pub use limits::Limits;
pub use platform::{Count, Key, Platform, Unavailable};
pub use room::{Member, Room};

/// The protocol version this server speaks.
pub const VERSION: u8 = 1;

/// The jam invite page, served at `/jam` by every shell.
pub const INVITE_PAGE: &str = include_str!("invite.html");

/// What `GET /` answers.
pub const ABOUT: &str = "nori-sync v1\n";

/// The text frame a browser sends to keep its connection up, and the answer. A shell answers it without
/// waking the room.
pub const PING: &str = "ping";
pub const PONG: &str = "pong";

/// A room's kind, chosen by the connection that opens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Group,
    Meeting,
    Jam,
}

impl Kind {
    pub fn from_query(value: &str) -> Option<Kind> {
        match value {
            "group" => Some(Kind::Group),
            "meeting" => Some(Kind::Meeting),
            "jam" => Some(Kind::Jam),
            _ => None,
        }
    }

    pub fn query(self) -> &'static str {
        match self {
            Kind::Group => "group",
            Kind::Meeting => "meeting",
            Kind::Jam => "jam",
        }
    }

    fn code(self) -> u8 {
        match self {
            Kind::Group => 1,
            Kind::Meeting => 2,
            Kind::Jam => 3,
        }
    }

    fn from_code(code: u8) -> Option<Kind> {
        [Kind::Group, Kind::Meeting, Kind::Jam].into_iter().find(|k| k.code() == code)
    }
}

/// Why the server refused a connection or a request. The same codes close a connection and fill a
/// `refused` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    BadRequest,
    VersionUnsupported,
    WrongKind,
    RoomFull,
    RoomClosed,
    TooManyRooms,
    DailyLimit,
    Unavailable,
    TooLarge,
    TooFast,
    WrongSecret,
    NoMember,
}

impl Refusal {
    pub const ALL: [Refusal; 12] = [
        Refusal::BadRequest,
        Refusal::VersionUnsupported,
        Refusal::WrongKind,
        Refusal::RoomFull,
        Refusal::RoomClosed,
        Refusal::TooManyRooms,
        Refusal::DailyLimit,
        Refusal::Unavailable,
        Refusal::TooLarge,
        Refusal::TooFast,
        Refusal::WrongSecret,
        Refusal::NoMember,
    ];

    /// The WebSocket close code, also the code in a `refused` frame.
    pub fn code(self) -> u16 {
        4000 + Refusal::ALL.iter().position(|r| *r == self).unwrap_or_default() as u16
    }

    pub fn from_code(code: u16) -> Option<Refusal> {
        Refusal::ALL.get(usize::from(code.checked_sub(4000)?)).copied()
    }

    /// The name in PROTOCOL.md, sent as the close reason and counted for the operator.
    pub fn name(self) -> &'static str {
        match self {
            Refusal::BadRequest => "bad_request",
            Refusal::VersionUnsupported => "version_unsupported",
            Refusal::WrongKind => "wrong_kind",
            Refusal::RoomFull => "room_full",
            Refusal::RoomClosed => "room_closed",
            Refusal::TooManyRooms => "too_many_rooms",
            Refusal::DailyLimit => "daily_limit",
            Refusal::Unavailable => "unavailable",
            Refusal::TooLarge => "too_large",
            Refusal::TooFast => "too_fast",
            Refusal::WrongSecret => "wrong_secret",
            Refusal::NoMember => "no_member",
        }
    }
}

/// A connect request that names a room: `/room/<name>?v=1&kind=<kind>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect<'a> {
    pub name: &'a str,
    pub kind: Kind,
}

/// Reads a connect request's path and query. None when the path is not a room's; a refusal when it is
/// one but the request is not acceptable.
pub fn parse_connect<'a>(path: &'a str, query: Option<&str>) -> Option<Result<Connect<'a>, Refusal>> {
    let name = path.strip_prefix("/room/")?;
    let param = |key: &str| query.unwrap_or_default().split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v);
    let version = param("v").and_then(|v| v.parse::<u8>().ok());
    Some(if version != Some(VERSION) {
        Err(Refusal::VersionUnsupported)
    } else {
        match (valid_name(name), param("kind").and_then(Kind::from_query)) {
            (true, Some(kind)) => Ok(Connect { name, kind }),
            _ => Err(Refusal::BadRequest),
        }
    })
}

/// A room's name: 43 characters of base64url without padding, 32 bytes.
pub fn valid_name(name: &str) -> bool {
    name.len() == 43 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The invite page's Content-Security-Policy: its own inline style and script, nothing fetched.
pub const INVITE_PAGE_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; base-uri 'none'; form-action 'none'";

/// The Android app the invite page and app links open.
pub const ANDROID_PACKAGE: &str = "dev.nori.music";

/// `/.well-known/assetlinks.json` for nori's Android app signed with the certificates `fingerprints`
/// (SHA-256, hex with colons); None when there are none or one is not of that form.
pub fn asset_links(fingerprints: &[String]) -> Option<String> {
    let valid = |f: &String| f.len() == 95 && f.split(':').all(|b| b.len() == 2 && b.bytes().all(|c| c.is_ascii_hexdigit()));
    if fingerprints.is_empty() || !fingerprints.iter().all(valid) {
        return None;
    }
    let list = fingerprints.iter().map(|f| format!("\"{}\"", f.to_ascii_uppercase())).collect::<Vec<_>>().join(",");
    Some(format!(
        "[{{\"relation\":[\"delegate_permission/common.handle_all_urls\"],\"target\":{{\"namespace\":\"android_app\",\"package_name\":\"{ANDROID_PACKAGE}\",\"sha256_cert_fingerprints\":[{list}]}}}}]"
    ))
}

/// The fingerprints in a setting: separated by commas or spaces.
pub fn fingerprints(setting: &str) -> Vec<String> {
    setting.split([',', ' ']).filter(|f| !f.is_empty()).map(str::to_string).collect()
}
