//! One WebSocket: frames to its room's event loop, frames from it to the socket. The keep-alive text
//! `ping` is answered here and never reaches the room, as Cloudflare's automatic response does.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use nori_sync_room::{Kind, PING, PONG, Refusal};
use tokio::sync::mpsc;

use crate::rooms::{Counts, Event, Out, QUEUE};
use crate::store::RoomKey;

/// How long a closing connection waits for the other side's close frame.
const CLOSING: Duration = Duration::from_secs(5);

pub async fn run(
    mut socket: WebSocket,
    connect: Result<(RoomKey, Kind), Refusal>,
    address: String,
    rooms: mpsc::UnboundedSender<Event>,
    counts: Arc<Counts>,
    debug_events: bool,
) {
    let (room, kind) = match connect {
        Ok(connect) => connect,
        Err(refusal) => {
            counts.add(nori_sync_room::Count::Refused(refusal));
            return close(&mut socket, refusal.code(), refusal.name()).await;
        }
    };
    let conn = counts.new_connection();
    let (out, mut queue) = mpsc::channel(QUEUE);
    if rooms.send(Event::Open { room, conn, kind, address, out }).is_err() {
        return;
    }
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Binary(data))) => _ = rooms.send(Event::Frame { room, conn, data }),
                Some(Ok(Message::Text(text))) if text.as_str() == PING => {
                    if debug_events {
                        eprintln!("ping answered");
                    }
                    if socket.send(Message::text(PONG)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(_))) => _ = rooms.send(Event::Text { room, conn }),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            },
            outgoing = queue.recv() => match outgoing {
                Some(Out::Frame(frame)) => {
                    if socket.send(Message::Binary(frame.into())).await.is_err() {
                        break;
                    }
                }
                Some(Out::Close(code, reason)) => {
                    close(&mut socket, code, reason).await;
                    break;
                }
                // The room gave up on this connection: it fell too far behind.
                None => {
                    close(&mut socket, 1008, "too slow").await;
                    break;
                }
            },
        }
    }
    let _ = rooms.send(Event::Closed { room, conn });
}

/// Sends a close frame and waits a little for the answer, so the client reads the code.
async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    if socket.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await.is_err() {
        return;
    }
    let _ = tokio::time::timeout(CLOSING, async {
        while let Some(Ok(message)) = socket.recv().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
}
