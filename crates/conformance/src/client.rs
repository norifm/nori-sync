//! A member as the suite plays it: a WebSocket speaking PROTOCOL.md.

use std::hash::{BuildHasher, RandomState};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nori_sync_room::{Answer, Kind, Refusal, Request, Welcome};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type Check = Result<(), String>;

/// How long the suite waits for something the server should send at once.
pub const PROMPT: Duration = Duration::from_secs(5);

/// A fresh room name: 32 random bytes in base64url.
pub fn room_name() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let random = RandomState::new();
    let bytes: Vec<u8> = (0..4u8).flat_map(|i| random.hash_one((i, std::time::Instant::now())).to_le_bytes()).collect();
    let mut bits = 0u32;
    let mut held = 0;
    let mut name = String::new();
    for b in bytes {
        bits = (bits << 8) | u32::from(b);
        held += 8;
        while held >= 6 {
            held -= 6;
            name.push(char::from(ALPHABET[((bits >> held) & 63) as usize]));
        }
    }
    name.push(char::from(ALPHABET[((bits << (6 - held)) & 63) as usize]));
    name
}

/// What the server sent next.
#[derive(Debug, PartialEq)]
pub enum Got {
    Frame(Vec<u8>),
    Text(String),
    Pong,
    Closed(Option<u16>),
}

pub struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Client {
    pub async fn connect_query(base: &str, name: &str, query: &str) -> Result<Client, String> {
        let url = format!("{base}/room/{name}?{query}");
        let (ws, _) = tokio::time::timeout(PROMPT, tokio_tungstenite::connect_async(url.as_str()))
            .await
            .map_err(|_| "the handshake timed out".to_string())?
            .map_err(|e| format!("handshake failed: {e}"))?;
        Ok(Client { ws })
    }

    pub async fn connect(base: &str, name: &str, kind: Kind) -> Result<Client, String> {
        Client::connect_query(base, name, &format!("v=1&kind={}", kind.query())).await
    }

    /// Connects and reads the welcome.
    pub async fn join(base: &str, name: &str, kind: Kind) -> Result<(Client, Welcome), String> {
        let mut client = Client::connect(base, name, kind).await?;
        match client.next().await? {
            Got::Frame(f) => match Answer::parse(&f) {
                Some(Answer::Welcome(w)) => Ok((client, w)),
                other => Err(format!("expected a welcome, got {other:?}")),
            },
            other => Err(format!("expected a welcome, got {other:?}")),
        }
    }

    /// Connects and expects to be refused with `refusal`.
    pub async fn refused(base: &str, name: &str, kind: Kind, refusal: Refusal) -> Check {
        let mut client = Client::connect(base, name, kind).await?;
        client.expect_closed(refusal).await
    }

    pub async fn send(&mut self, request: Request<'_>) -> Check {
        self.send_raw(Message::Binary(request.encode().into())).await
    }

    pub async fn send_raw(&mut self, message: Message) -> Check {
        self.ws.send(message).await.map_err(|e| format!("send failed: {e}"))
    }

    pub async fn next(&mut self) -> Result<Got, String> {
        loop {
            let message = match tokio::time::timeout(PROMPT, self.ws.next()).await {
                Err(_) => return Err("nothing came".into()),
                Ok(None) => return Ok(Got::Closed(None)),
                Ok(Some(Err(e))) => return Err(format!("read failed: {e}")),
                Ok(Some(Ok(m))) => m,
            };
            return Ok(match message {
                Message::Binary(b) => Got::Frame(b.to_vec()),
                Message::Text(t) => Got::Text(t.to_string()),
                Message::Pong(_) => Got::Pong,
                Message::Close(frame) => Got::Closed(frame.map(|f: CloseFrame| u16::from(f.code))),
                Message::Ping(_) | Message::Frame(_) => continue,
            });
        }
    }

    /// The next frame, which must be `expected`.
    pub async fn expect(&mut self, expected: Answer<'_>) -> Check {
        let got = self.next().await?;
        match &got {
            Got::Frame(f) if *f == expected.encode() => Ok(()),
            Got::Frame(f) => Err(format!("expected {expected:?}, got {:?}", Answer::parse(f))),
            other => Err(format!("expected {expected:?}, got {other:?}")),
        }
    }

    pub async fn expect_closed(&mut self, refusal: Refusal) -> Check {
        match self.next().await? {
            Got::Closed(Some(code)) if code == refusal.code() => Ok(()),
            other => Err(format!("expected a close with {} ({}), got {other:?}", refusal.code(), refusal.name())),
        }
    }

    /// Proves nothing else is queued for this member: the answer to a time request comes next.
    pub async fn nothing_else(&mut self) -> Check {
        self.send(Request::Time { t1: 7 }).await?;
        match self.next().await? {
            Got::Frame(f) if matches!(Answer::parse(&f), Some(Answer::Time { t1: 7, .. })) => Ok(()),
            Got::Frame(f) => Err(format!("unexpected {:?}", Answer::parse(&f))),
            other => Err(format!("unexpected {other:?}")),
        }
    }

    pub async fn kept(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.send(Request::KeptGet).await?;
        match self.next().await? {
            Got::Frame(f) => match Answer::parse(&f) {
                Some(Answer::Kept(blob)) => Ok(blob.map(<[u8]>::to_vec)),
                other => Err(format!("expected kept, got {other:?}")),
            },
            other => Err(format!("expected kept, got {other:?}")),
        }
    }

    pub async fn close(mut self) -> Check {
        self.ws.close(None).await.map_err(|e| format!("close failed: {e}"))?;
        while let Ok(Some(Ok(_))) = tokio::time::timeout(PROMPT, self.ws.next()).await {}
        Ok(())
    }
}

/// A plain HTTP GET: the status, the headers in lower case and the body.
pub async fn http_get(base: &str, path: &str) -> Result<(u16, String, String), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let host = base.split_once("://").map_or(base, |(_, h)| h).trim_end_matches('/');
    let mut stream = TcpStream::connect(host).await.map_err(|e| format!("connect failed: {e}"))?;
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    tokio::time::timeout(PROMPT, stream.read_to_end(&mut raw)).await.map_err(|_| "no answer".to_string())?.map_err(|e| e.to_string())?;
    let raw = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = raw.split_once("\r\n\r\n").ok_or("no header end")?;
    let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).ok_or("no status")?;
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { unchunk(body) } else { body.to_string() };
    Ok((status, head.to_ascii_lowercase(), body))
}

fn unchunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else { break };
        if size == 0 || rest.len() < size {
            break;
        }
        out += &rest[..size];
        body = rest[size..].trim_start_matches("\r\n");
    }
    out
}
