//! The server's settings, from environment variables.

use std::net::IpAddr;
use std::path::PathBuf;

use nori_sync_room::Limits;

pub struct Config {
    pub bind: IpAddr,
    pub port: u16,
    /// Where rooms with something stored keep their file.
    pub data: PathBuf,
    /// Behind a reverse proxy: the client's address is the last `X-Forwarded-For` entry.
    pub trust_proxy: bool,
    pub app_link_fingerprints: Vec<String>,
    /// Writes a line for each event a room handles, for the conformance suite.
    pub debug_events: bool,
    pub limits: Limits,
    /// Frames larger than this end the connection before a room sees them.
    pub max_websocket_message: usize,
}

impl Config {
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let parse = |name: &str, default: &str| var(name).unwrap_or_else(|| default.to_string());
        let switch = |name: &str| matches!(var(name).as_deref(), Some("1" | "true" | "yes"));
        let limits = Limits::from_vars(&var)?;
        Ok(Config {
            bind: parse("NORI_SYNC_BIND", "0.0.0.0").parse().map_err(|_| "NORI_SYNC_BIND is not an IP address")?,
            port: parse("NORI_SYNC_PORT", "8080").parse().map_err(|_| "NORI_SYNC_PORT is not a port")?,
            data: PathBuf::from(parse("NORI_SYNC_DATA", "data")),
            trust_proxy: switch("NORI_SYNC_TRUST_PROXY"),
            app_link_fingerprints: nori_sync_room::fingerprints(&parse("NORI_SYNC_APP_LINK_FINGERPRINTS", "")),
            debug_events: switch("NORI_SYNC_DEBUG_EVENTS"),
            max_websocket_message: (limits.max_frame as usize).max(1 << 20),
            limits,
        })
    }
}
