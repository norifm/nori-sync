//! The server's limits, the same names in both shells (environment variables, or a Worker's vars).

use crate::Kind;

const MINUTE_US: i64 = 60_000_000;
const SECOND_US: i64 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Largest frame a member may send, its tag included.
    pub max_frame: u32,
    pub frames_per_minute: u32,
    pub bytes_per_minute: u32,
    /// Members of a group or a jam room. A meeting room holds two.
    pub max_members: u16,
    /// Rooms one network address may open per minute (a shell's own counter).
    pub rooms_per_minute: u32,
    pub meeting_lifetime_us: i64,
    /// How long a closed meeting room refuses new connections.
    pub meeting_closed_us: i64,
    /// How long a kept copy outlives the last connection to its room.
    pub kept_lifetime_us: i64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_frame: 65_536,
            frames_per_minute: 240,
            bytes_per_minute: 4 << 20,
            max_members: 20,
            rooms_per_minute: 30,
            meeting_lifetime_us: 5 * MINUTE_US,
            meeting_closed_us: 60 * MINUTE_US,
            kept_lifetime_us: 56 * 24 * 60 * MINUTE_US,
        }
    }
}

/// How a setting's value is read into [`Limits`]; None when it is out of range.
type Apply = fn(&mut Limits, u64) -> Option<()>;

/// Each setting's name and how it applies.
const SETTINGS: [(&str, Apply); 8] = [
    ("NORI_SYNC_MAX_FRAME", |l, v| set(&mut l.max_frame, v).filter(|_| v > 33)),
    ("NORI_SYNC_FRAMES_PER_MINUTE", |l, v| set(&mut l.frames_per_minute, v)),
    ("NORI_SYNC_BYTES_PER_MINUTE", |l, v| set(&mut l.bytes_per_minute, v)),
    ("NORI_SYNC_MAX_MEMBERS", |l, v| set(&mut l.max_members, v).filter(|_| v >= 2)),
    ("NORI_SYNC_ROOMS_PER_MINUTE", |l, v| set(&mut l.rooms_per_minute, v)),
    ("NORI_SYNC_MEETING_SECONDS", |l, v| seconds(&mut l.meeting_lifetime_us, v)),
    ("NORI_SYNC_MEETING_CLOSED_SECONDS", |l, v| seconds(&mut l.meeting_closed_us, v)),
    ("NORI_SYNC_KEPT_SECONDS", |l, v| seconds(&mut l.kept_lifetime_us, v)),
];

fn set<T: TryFrom<u64>>(field: &mut T, value: u64) -> Option<()> {
    *field = T::try_from(value).ok()?;
    Some(())
}

fn seconds(field: &mut i64, value: u64) -> Option<()> {
    *field = i64::try_from(value).ok()?.checked_mul(SECOND_US).filter(|&us| us > 0)?;
    Some(())
}

impl Limits {
    /// The defaults with each setting `var` names replaced. An error names a setting whose value is not
    /// a whole number in range.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Limits, String> {
        let mut limits = Limits::default();
        for (name, apply) in SETTINGS {
            if let Some(value) = var(name) {
                value.trim().parse().ok().and_then(|v| apply(&mut limits, v)).ok_or_else(|| format!("{name}: {value:?} is not a valid value"))?;
            }
        }
        Ok(limits)
    }

    pub fn members(&self, kind: Kind) -> u16 {
        match kind {
            Kind::Meeting => 2,
            Kind::Group | Kind::Jam => self.max_members,
        }
    }

    /// The largest kept copy: a frame less the tag and the secret.
    pub fn max_kept(&self) -> usize {
        self.max_frame as usize - 1 - crate::SECRET_LEN
    }

    pub(crate) const WINDOW_US: i64 = MINUTE_US;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_replace_defaults() {
        let limits = Limits::from_vars(|name| match name {
            "NORI_SYNC_MAX_FRAME" => Some("1000".into()),
            "NORI_SYNC_MEETING_SECONDS" => Some(" 3 ".into()),
            "NORI_SYNC_KEPT_SECONDS" => Some("10".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(limits, Limits { max_frame: 1000, meeting_lifetime_us: 3 * SECOND_US, kept_lifetime_us: 10 * SECOND_US, ..Limits::default() });
    }

    #[test]
    fn bad_settings_are_named() {
        for (name, value) in [
            ("NORI_SYNC_MAX_FRAME", "big"),
            ("NORI_SYNC_MAX_FRAME", "10"),
            ("NORI_SYNC_MAX_MEMBERS", "70000"),
            ("NORI_SYNC_MAX_MEMBERS", "1"),
            ("NORI_SYNC_MEETING_SECONDS", "0"),
            ("NORI_SYNC_FRAMES_PER_MINUTE", "-1"),
        ] {
            let error = Limits::from_vars(|n| (n == name).then(|| value.to_string())).unwrap_err();
            assert!(error.starts_with(name), "{error}");
        }
    }
}
