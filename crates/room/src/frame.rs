//! The binary frames of PROTOCOL.md, both directions: the server parses what members send and encodes
//! what it sends them; a client (the conformance suite) does the reverse.

use crate::{Kind, Refusal};

/// Length of the write secret a `kept_put` or `kept_delete` carries.
pub const SECRET_LEN: usize = 32;

mod tag {
    pub const SEND_ALL: u8 = 0x01;
    pub const SEND_TO: u8 = 0x02;
    pub const TIME: u8 = 0x03;
    pub const KEPT_GET: u8 = 0x04;
    pub const KEPT_PUT: u8 = 0x05;
    pub const KEPT_DELETE: u8 = 0x06;

    pub const WELCOME: u8 = 0x80;
    pub const JOINED: u8 = 0x81;
    pub const LEFT: u8 = 0x82;
    pub const MESSAGE: u8 = 0x83;
    pub const TIME_ANSWER: u8 = 0x84;
    pub const KEPT: u8 = 0x85;
    pub const DONE: u8 = 0x86;
    pub const REFUSED: u8 = 0x87;
}

/// A frame from a member to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request<'a> {
    SendAll(&'a [u8]),
    SendTo { member: u16, payload: &'a [u8] },
    Time { t1: i64 },
    KeptGet,
    KeptPut { secret: &'a [u8; SECRET_LEN], blob: &'a [u8] },
    KeptDelete { secret: &'a [u8; SECRET_LEN] },
}

/// Which request a `done` or `refused` answers: the request's tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    SendAll,
    SendTo,
    Time,
    KeptGet,
    KeptPut,
    KeptDelete,
    /// A frame whose tag is none of the above.
    Unknown(u8),
}

impl RequestKind {
    pub fn of(frame: &[u8]) -> RequestKind {
        match frame.first().copied().unwrap_or(0) {
            tag::SEND_ALL => RequestKind::SendAll,
            tag::SEND_TO => RequestKind::SendTo,
            tag::TIME => RequestKind::Time,
            tag::KEPT_GET => RequestKind::KeptGet,
            tag::KEPT_PUT => RequestKind::KeptPut,
            tag::KEPT_DELETE => RequestKind::KeptDelete,
            other => RequestKind::Unknown(other),
        }
    }

    fn tag(self) -> u8 {
        match self {
            RequestKind::SendAll => tag::SEND_ALL,
            RequestKind::SendTo => tag::SEND_TO,
            RequestKind::Time => tag::TIME,
            RequestKind::KeptGet => tag::KEPT_GET,
            RequestKind::KeptPut => tag::KEPT_PUT,
            RequestKind::KeptDelete => tag::KEPT_DELETE,
            RequestKind::Unknown(t) => t,
        }
    }
}

impl<'a> Request<'a> {
    pub fn parse(frame: &'a [u8]) -> Option<Request<'a>> {
        let (&tag, body) = frame.split_first()?;
        Some(match tag {
            tag::SEND_ALL => Request::SendAll(body),
            tag::SEND_TO => {
                let (member, payload) = body.split_first_chunk::<2>()?;
                Request::SendTo { member: u16::from_be_bytes(*member), payload }
            }
            tag::TIME => Request::Time { t1: i64::from_be_bytes(*body.first_chunk::<8>().filter(|_| body.len() == 8)?) },
            tag::KEPT_GET if body.is_empty() => Request::KeptGet,
            tag::KEPT_PUT => {
                let (secret, blob) = body.split_first_chunk::<SECRET_LEN>()?;
                Request::KeptPut { secret, blob }
            }
            tag::KEPT_DELETE if body.len() == SECRET_LEN => Request::KeptDelete { secret: body.first_chunk()? },
            _ => return None,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Request::SendAll(payload) => {
                out.push(tag::SEND_ALL);
                out.extend_from_slice(payload);
            }
            Request::SendTo { member, payload } => {
                out.push(tag::SEND_TO);
                out.extend_from_slice(&member.to_be_bytes());
                out.extend_from_slice(payload);
            }
            Request::Time { t1 } => {
                out.push(tag::TIME);
                out.extend_from_slice(&t1.to_be_bytes());
            }
            Request::KeptGet => out.push(tag::KEPT_GET),
            Request::KeptPut { secret, blob } => {
                out.push(tag::KEPT_PUT);
                out.extend_from_slice(*secret);
                out.extend_from_slice(blob);
            }
            Request::KeptDelete { secret } => {
                out.push(tag::KEPT_DELETE);
                out.extend_from_slice(*secret);
            }
        }
        out
    }
}

/// Who a `message` was sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum To {
    Everyone,
    You,
}

/// What a `welcome` tells a new member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub version: u8,
    pub kind: Kind,
    pub you: u16,
    pub max_frame: u32,
    pub frames_per_minute: u32,
    pub bytes_per_minute: u32,
    pub max_members: u16,
    /// When a meeting room closes, µs since the epoch.
    pub closes_at: Option<i64>,
    pub others: Vec<u16>,
}

/// A frame from the server to a member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer<'a> {
    Welcome(Welcome),
    Joined(u16),
    Left(u16),
    Message { from: u16, to: To, payload: &'a [u8] },
    Time { t1: i64, t2: i64, t3: i64 },
    Kept(Option<&'a [u8]>),
    Done(RequestKind),
    Refused { request: RequestKind, refusal: Refusal, retry_after_ms: u32 },
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|[b]| b)
    }
    fn u16(&mut self) -> Option<u16> {
        self.take().map(u16::from_be_bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_be_bytes)
    }
    fn i64(&mut self) -> Option<i64> {
        self.take().map(i64::from_be_bytes)
    }
    fn end(self) -> Option<()> {
        self.0.is_empty().then_some(())
    }
}

impl<'a> Answer<'a> {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Answer::Welcome(w) => {
                out.push(tag::WELCOME);
                out.push(w.version);
                out.push(w.kind.code());
                out.extend_from_slice(&w.you.to_be_bytes());
                out.extend_from_slice(&w.max_frame.to_be_bytes());
                out.extend_from_slice(&w.frames_per_minute.to_be_bytes());
                out.extend_from_slice(&w.bytes_per_minute.to_be_bytes());
                out.extend_from_slice(&w.max_members.to_be_bytes());
                out.extend_from_slice(&w.closes_at.unwrap_or(0).to_be_bytes());
                out.extend_from_slice(&(w.others.len() as u16).to_be_bytes());
                for m in &w.others {
                    out.extend_from_slice(&m.to_be_bytes());
                }
            }
            Answer::Joined(m) => {
                out.push(tag::JOINED);
                out.extend_from_slice(&m.to_be_bytes());
            }
            Answer::Left(m) => {
                out.push(tag::LEFT);
                out.extend_from_slice(&m.to_be_bytes());
            }
            Answer::Message { from, to, payload } => {
                out.reserve(4 + payload.len());
                out.push(tag::MESSAGE);
                out.extend_from_slice(&from.to_be_bytes());
                out.push(match to {
                    To::Everyone => 0,
                    To::You => 1,
                });
                out.extend_from_slice(payload);
            }
            Answer::Time { t1, t2, t3 } => {
                out.push(tag::TIME_ANSWER);
                for t in [t1, t2, t3] {
                    out.extend_from_slice(&t.to_be_bytes());
                }
            }
            Answer::Kept(blob) => {
                out.push(tag::KEPT);
                match blob {
                    Some(blob) => {
                        out.push(1);
                        out.extend_from_slice(blob);
                    }
                    None => out.push(0),
                }
            }
            Answer::Done(request) => out.extend_from_slice(&[tag::DONE, request.tag()]),
            Answer::Refused { request, refusal, retry_after_ms } => {
                out.extend_from_slice(&[tag::REFUSED, request.tag()]);
                out.extend_from_slice(&refusal.code().to_be_bytes());
                out.extend_from_slice(&retry_after_ms.to_be_bytes());
            }
        }
        out
    }

    pub fn parse(frame: &'a [u8]) -> Option<Answer<'a>> {
        let (&tag, body) = frame.split_first()?;
        let mut r = Reader(body);
        let answer = match tag {
            tag::WELCOME => {
                let version = r.u8()?;
                let kind = Kind::from_code(r.u8()?)?;
                let you = r.u16()?;
                let max_frame = r.u32()?;
                let frames_per_minute = r.u32()?;
                let bytes_per_minute = r.u32()?;
                let max_members = r.u16()?;
                let closes_at = Some(r.i64()?).filter(|&t| t != 0);
                let count = r.u16()?;
                let others = (0..count).map(|_| r.u16()).collect::<Option<Vec<u16>>>()?;
                Answer::Welcome(Welcome { version, kind, you, max_frame, frames_per_minute, bytes_per_minute, max_members, closes_at, others })
            }
            tag::JOINED => Answer::Joined(r.u16()?),
            tag::LEFT => Answer::Left(r.u16()?),
            tag::MESSAGE => {
                let from = r.u16()?;
                let to = match r.u8()? {
                    0 => To::Everyone,
                    1 => To::You,
                    _ => return None,
                };
                return Some(Answer::Message { from, to, payload: r.0 });
            }
            tag::TIME_ANSWER => Answer::Time { t1: r.i64()?, t2: r.i64()?, t3: r.i64()? },
            tag::KEPT => {
                return match r.u8()? {
                    0 => r.end().map(|_| Answer::Kept(None)),
                    1 => Some(Answer::Kept(Some(r.0))),
                    _ => None,
                };
            }
            tag::DONE => Answer::Done(RequestKind::of(&[r.u8()?])),
            tag::REFUSED => Answer::Refused {
                request: RequestKind::of(&[r.u8()?]),
                refusal: Refusal::from_code(r.u16()?)?,
                retry_after_ms: r.u32()?,
            },
            _ => return None,
        };
        r.end().map(|_| answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_read_back() {
        let secret = [7u8; SECRET_LEN];
        for request in [
            Request::SendAll(b"hi"),
            Request::SendAll(b""),
            Request::SendTo { member: 0x1234, payload: b"x" },
            Request::Time { t1: -5 },
            Request::KeptGet,
            Request::KeptPut { secret: &secret, blob: b"blob" },
            Request::KeptDelete { secret: &secret },
        ] {
            assert_eq!(Request::parse(&request.encode()), Some(request.clone()));
        }
    }

    #[test]
    fn answers_read_back() {
        let welcome = Welcome {
            version: 1,
            kind: Kind::Meeting,
            you: 3,
            max_frame: 65536,
            frames_per_minute: 240,
            bytes_per_minute: 1 << 22,
            max_members: 2,
            closes_at: Some(1_700_000_000_000_000),
            others: vec![1, 2],
        };
        for answer in [
            Answer::Welcome(welcome),
            Answer::Joined(9),
            Answer::Left(9),
            Answer::Message { from: 2, to: To::You, payload: b"abc" },
            Answer::Time { t1: 1, t2: 2, t3: 3 },
            Answer::Kept(None),
            Answer::Kept(Some(b"")),
            Answer::Done(RequestKind::KeptPut),
            Answer::Refused { request: RequestKind::SendTo, refusal: Refusal::NoMember, retry_after_ms: 0 },
        ] {
            assert_eq!(Answer::parse(&answer.encode()), Some(answer.clone()));
        }
    }

    #[test]
    fn malformed_requests_are_none() {
        for frame in [&[][..], &[0x00], &[0x02, 1], &[0x03, 0, 0], &[0x03; 10], &[0x04, 0], &[0x05; 32], &[0x06; 32], &[0x81]] {
            assert_eq!(Request::parse(frame), None, "{frame:?}");
        }
    }
}
