//! A room's store and alarm, kept in a file named by the hash of the room's name while it holds anything.
//! The format: `NSR1`, then entries of a tag (a [`Key`]'s place in [`Key::ALL`], or 0xFF for the alarm),
//! a big-endian u32 length and the value.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use nori_sync_room::Key;

const MAGIC: &[u8] = b"NSR1";
const ALARM: u8 = 0xFF;

/// The hash of a room's name: rooms are known by it, never by the name.
pub type RoomKey = [u8; 32];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stored {
    pub values: HashMap<Key, Vec<u8>>,
    pub alarm: Option<i64>,
}

impl Stored {
    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.alarm.is_none()
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        let mut entry = |tag: u8, value: &[u8]| {
            out.push(tag);
            out.extend_from_slice(&(value.len() as u32).to_be_bytes());
            out.extend_from_slice(value);
        };
        for (tag, key) in Key::ALL.iter().enumerate() {
            if let Some(value) = self.values.get(key) {
                entry(tag as u8, value);
            }
        }
        if let Some(at) = self.alarm {
            entry(ALARM, &at.to_be_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Stored> {
        let mut rest = bytes.strip_prefix(MAGIC)?;
        let mut stored = Stored::default();
        while let Some((&tag, after)) = rest.split_first() {
            let (len, after) = after.split_first_chunk::<4>()?;
            let (value, after) = after.split_at_checked(u32::from_be_bytes(*len) as usize)?;
            match tag {
                ALARM => stored.alarm = Some(i64::from_be_bytes(value.try_into().ok()?)),
                _ => _ = stored.values.insert(*Key::ALL.get(usize::from(tag))?, value.to_vec()),
            }
            rest = after;
        }
        Some(stored)
    }
}

/// The folder of room files.
pub struct Folder(PathBuf);

impl Folder {
    pub fn open(path: &Path) -> io::Result<Folder> {
        fs::create_dir_all(path)?;
        Ok(Folder(path.to_path_buf()))
    }

    fn file(&self, room: &RoomKey) -> PathBuf {
        let hex: String = room.iter().map(|b| format!("{b:02x}")).collect();
        self.0.join(format!("{hex}.room"))
    }

    /// Every room with a file. A file that does not read is reported and left alone.
    pub fn load_all(&self) -> io::Result<Vec<(RoomKey, Stored)>> {
        let mut rooms = Vec::new();
        for entry in fs::read_dir(&self.0)? {
            let path = entry?.path();
            let Some(room) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".room")).and_then(room_from_hex) else { continue };
            match fs::read(&path).ok().and_then(|b| Stored::decode(&b)) {
                Some(stored) => rooms.push((room, stored)),
                None => eprintln!("nori-sync: skipped an unreadable room file {}", path.display()),
            }
        }
        Ok(rooms)
    }

    /// Writes a room's file whole, or removes it when nothing is stored.
    pub fn write(&self, room: &RoomKey, stored: &Stored) -> io::Result<()> {
        let file = self.file(room);
        if stored.is_empty() {
            return match fs::remove_file(&file) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let partial = file.with_extension("partial");
        fs::write(&partial, stored.encode())?;
        fs::rename(&partial, &file)
    }
}

fn room_from_hex(hex: &str) -> Option<RoomKey> {
    let bytes = hex.as_bytes();
    let mut room = [0; 32];
    (bytes.len() == 64).then_some(())?;
    for (i, pair) in bytes.chunks(2).enumerate() {
        room[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(room)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_room_file_reads_back_and_goes_when_empty() {
        let dir = std::env::temp_dir().join(format!("nori-sync-store-{}", std::process::id()));
        let folder = Folder::open(&dir).unwrap();
        let room = [0xA5; 32];
        let stored = Stored { values: HashMap::from([(Key::Kept, vec![1, 2, 3]), (Key::Seen, vec![0; 8]), (Key::Opened, vec![])]), alarm: Some(-7) };
        folder.write(&room, &stored).unwrap();
        assert_eq!(folder.load_all().unwrap(), [(room, stored)]);
        folder.write(&room, &Stored::default()).unwrap();
        assert!(folder.load_all().unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_damaged_file_is_not_read() {
        let stored = Stored { values: HashMap::from([(Key::Kept, vec![9; 10])]), alarm: None };
        let bytes = stored.encode();
        assert_eq!(Stored::decode(&bytes), Some(stored));
        assert_eq!(Stored::decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(Stored::decode(b"NSR1\x09\0\0\0\0"), None);
    }
}
