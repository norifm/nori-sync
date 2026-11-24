//! The nori sync server on Cloudflare. The Worker serves the pages and routes each room's connections to
//! one Durable Object named by the room. The object accepts hibernating WebSockets, so an idle room is
//! unloaded and costs nothing; each event builds a [`Here`] from the object's connections and storage and
//! hands it to the room logic, which keeps nothing in memory between events.

use nori_sync_room::{Count, Key, Limits, Member, Platform, Refusal, Unavailable};
use worker::*;

#[event(start)]
fn start() {
    std::panic::set_hook(Box::new(|info| console_error!("panic: {info}")));
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    match url.path() {
        "/" => text(nori_sync_room::ABOUT),
        "/jam" => invite_page(),
        "/.well-known/assetlinks.json" => {
            let fingerprints = env.var("NORI_SYNC_APP_LINK_FINGERPRINTS").map(|v| nori_sync_room::fingerprints(&v.to_string())).unwrap_or_default();
            match nori_sync_room::asset_links(&fingerprints) {
                Some(json) => {
                    let mut response = Response::ok(json)?;
                    response.headers_mut().set("Content-Type", "application/json")?;
                    Ok(response)
                }
                None => Response::error("not found", 404),
            }
        }
        path => match nori_sync_room::parse_connect(path, url.query()) {
            None => Response::error("not found", 404),
            Some(_) if req.headers().get("Upgrade")?.is_none_or(|u| !u.eq_ignore_ascii_case("websocket")) => Response::error("expected a WebSocket", 426),
            Some(Err(refusal)) => refuse(refusal),
            Some(Ok(connect)) => {
                let room = env.durable_object("ROOMS")?.get_by_name(connect.name)?;
                match room.fetch_with_request(req).await {
                    Ok(response) => Ok(response),
                    // The free plan's daily limit fails the call into the object.
                    Err(error) if error.to_string().contains("xceeded") => refuse(Refusal::DailyLimit),
                    Err(error) => {
                        console_error!("a room could not be reached: {error}");
                        refuse(Refusal::Unavailable)
                    }
                }
            }
        },
    }
}

fn text(body: &str) -> Result<Response> {
    let mut response = Response::ok(body)?;
    response.headers_mut().set("Content-Type", "text/plain; charset=utf-8")?;
    Ok(response)
}

fn invite_page() -> Result<Response> {
    let mut response = Response::from_html(nori_sync_room::INVITE_PAGE)?;
    let headers = response.headers_mut();
    headers.set("Cache-Control", "no-cache")?;
    headers.set("Content-Security-Policy", nori_sync_room::INVITE_PAGE_POLICY)?;
    headers.set("Referrer-Policy", "no-referrer")?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    Ok(response)
}

/// Completes the handshake only to close at once with the refusal, so a browser can read why.
fn refuse(refusal: Refusal) -> Result<Response> {
    console_log!("refused {}", refusal.name());
    let pair = WebSocketPair::new()?;
    pair.server.accept()?;
    pair.server.close(Some(refusal.code()), Some(refusal.name()))?;
    Response::from_websocket(pair.client)
}

#[durable_object]
pub struct Room {
    state: State,
    env: Env,
    limits: Limits,
    /// Logs `room loaded` and `room event` lines for the conformance suite.
    debug_events: bool,
}

impl Room {
    /// The room's connections for one event, `ws` (the event's own, which may have closed) among them.
    fn here(&self, ws: Option<&WebSocket>) -> (Here<'_>, usize) {
        let mut sockets = self.state.get_websockets();
        let index = ws.map_or(0, |ws| match sockets.iter().position(|s| s == ws) {
            Some(i) => i,
            None => {
                sockets.push(ws.clone());
                sockets.len() - 1
            }
        });
        if self.debug_events {
            console_log!("room event");
        }
        (Here { room: self, sockets }, index)
    }
}

impl DurableObject for Room {
    fn new(state: State, env: Env) -> Self {
        // The text keep-alive is answered by the runtime without waking the object.
        if let Ok(pair) = worker_sys::WebSocketRequestResponsePair::new(nori_sync_room::PING, nori_sync_room::PONG) {
            state.set_websocket_auto_response(&pair);
        }
        let limits = Limits::from_vars(|name| env.var(name).ok().map(|v| v.to_string())).unwrap_or_else(|error| {
            console_error!("{error}; the defaults are used");
            Limits::default()
        });
        let debug_events = env.var("NORI_SYNC_DEBUG_EVENTS").is_ok_and(|v| v.to_string() == "1");
        if debug_events {
            console_log!("room loaded");
        }
        Room { state, env, limits, debug_events }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        let url = req.url()?;
        let Some(Ok(connect)) = nori_sync_room::parse_connect(url.path(), url.query()) else { return Response::error("not a room", 400) };
        let address = req.headers().get("CF-Connecting-IP")?.unwrap_or_default();
        let pair = WebSocketPair::new()?;
        self.state.accept_web_socket(&pair.server);
        let (mut here, conn) = self.here(Some(&pair.server));
        nori_sync_room::Room::new(&mut here, &self.limits).join(conn, connect.kind, &address).await;
        Response::from_websocket(pair.client)
    }

    async fn websocket_message(&self, ws: WebSocket, message: WebSocketIncomingMessage) -> Result<()> {
        let (mut here, conn) = self.here(Some(&ws));
        let mut room = nori_sync_room::Room::new(&mut here, &self.limits);
        match message {
            WebSocketIncomingMessage::Binary(frame) => room.frame(conn, &frame).await,
            WebSocketIncomingMessage::String(_) => room.text(conn),
        }
        Ok(())
    }

    async fn websocket_close(&self, ws: WebSocket, _code: usize, _reason: String, _clean: bool) -> Result<()> {
        let (mut here, conn) = self.here(Some(&ws));
        nori_sync_room::Room::new(&mut here, &self.limits).closed(conn);
        // Answers the member's close frame; it may have been answered already.
        let _ = ws.close(Some(1000), Some("bye"));
        Ok(())
    }

    async fn websocket_error(&self, ws: WebSocket, _error: Error) -> Result<()> {
        let (mut here, conn) = self.here(Some(&ws));
        nori_sync_room::Room::new(&mut here, &self.limits).closed(conn);
        Ok(())
    }

    async fn alarm(&self) -> Result<Response> {
        let (mut here, _) = self.here(None);
        // An error makes the runtime ring the alarm again later.
        nori_sync_room::Room::new(&mut here, &self.limits).alarm().await.map_err(|_| Error::RustError("the room's storage failed".into()))?;
        Response::ok("")
    }
}

/// One room's view of Cloudflare, for one event.
struct Here<'a> {
    room: &'a Room,
    sockets: Vec<WebSocket>,
}

fn unavailable(error: Error) -> Unavailable {
    console_error!("storage: {error}");
    Unavailable
}

impl Platform for Here<'_> {
    type Conn = usize;

    fn now_us(&self) -> i64 {
        Date::now().as_millis() as i64 * 1000
    }

    fn connections(&self) -> Vec<usize> {
        (0..self.sockets.len()).collect()
    }

    fn member(&self, conn: usize) -> Option<Member> {
        self.sockets[conn].deserialize_attachment::<Member>().ok().flatten()
    }

    fn set_member(&mut self, conn: usize, member: &Member) {
        let _ = self.sockets[conn].serialize_attachment(member);
    }

    fn send(&mut self, conn: usize, frame: &[u8]) {
        let _ = self.sockets[conn].send_with_bytes(frame);
    }

    fn close(&mut self, conn: usize, refusal: Refusal) {
        let _ = self.sockets[conn].close(Some(refusal.code()), Some(refusal.name()));
    }

    fn count(&mut self, what: Count) {
        // Cloudflare counts connections and messages itself; refusals are counted from these lines.
        if let Count::Refused(refusal) = what {
            console_log!("refused {}", refusal.name());
        }
    }

    async fn load(&mut self, key: Key) -> std::result::Result<Option<Vec<u8>>, Unavailable> {
        let value = self.room.state.storage().get::<serde_bytes::ByteBuf>(key.name()).await.map_err(unavailable)?;
        Ok(value.map(serde_bytes::ByteBuf::into_vec))
    }

    async fn save(&mut self, key: Key, value: &[u8]) -> std::result::Result<(), Unavailable> {
        self.room.state.storage().put_raw(key.name(), js_sys::Uint8Array::from(value)).await.map_err(unavailable)
    }

    async fn erase(&mut self, keys: &[Key]) -> std::result::Result<(), Unavailable> {
        self.room.state.storage().delete_multiple(keys.iter().map(|k| k.name()).collect()).await.map(|_| ()).map_err(unavailable)
    }

    async fn set_alarm(&mut self, at_us: Option<i64>) -> std::result::Result<(), Unavailable> {
        let storage = self.room.state.storage();
        match at_us {
            Some(at) => storage.set_alarm(ScheduledTime::new(js_sys::Date::new(&(at as f64 / 1000.0).into()))).await,
            None => storage.delete_alarm().await,
        }
        .map_err(unavailable)
    }

    async fn may_open_room(&mut self, address: &str) -> bool {
        let Ok(limiter) = self.room.env.rate_limiter("NEW_ROOMS") else {
            console_error!("no NEW_ROOMS rate limiter: rooms are opened without a limit");
            return true;
        };
        // A limiter that fails lets the room open: this limit is a guard, not a gate.
        match limiter.limit(address.to_string()).await {
            Ok(outcome) => outcome.success,
            Err(error) => {
                console_error!("rate limiter: {error}");
                true
            }
        }
    }
}
