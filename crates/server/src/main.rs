//! The self-hosted nori sync server: one process, every room in memory, rooms with something stored in a
//! file each. Configured by environment variables (README.md).

mod config;
mod connection;
mod rooms;
mod store;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::sync::mpsc;

use config::Config;
use rooms::{Counts, Rooms};

/// What every request handler shares.
struct Server {
    config: Config,
    rooms: mpsc::UnboundedSender<rooms::Event>,
    counts: Arc<Counts>,
    /// Each connection holds a clone; shutting down waits until all are dropped.
    open: mpsc::Sender<()>,
}

fn main() {
    let config = match Config::from_env(|name| std::env::var(name).ok()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("nori-sync: {error}");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    if let Err(error) = runtime.block_on(serve(config)) {
        eprintln!("nori-sync: {error}");
        std::process::exit(1);
    }
}

async fn serve(config: Config) -> std::io::Result<()> {
    let counts = Arc::new(Counts::default());
    let rooms = Rooms::load(&config, counts.clone())?;
    let (events, inbox) = mpsc::unbounded_channel();
    let event_loop = tokio::spawn(rooms.run(inbox));
    let (open, mut all_closed) = mpsc::channel(1);
    let listener = tokio::net::TcpListener::bind((config.bind, config.port)).await?;
    eprintln!("nori-sync: listening on {}", listener.local_addr()?);
    let server = Arc::new(Server { config, rooms: events.clone(), counts, open });
    let app = Router::new()
        .route("/", get(|| async { nori_sync_room::ABOUT }))
        .route("/jam", get(invite_page))
        .route("/.well-known/assetlinks.json", get(asset_links))
        .route("/stats", get(stats))
        .route("/room/{name}", get(room))
        .with_state(server.clone());
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown_signal()).await?;
    // Tell every member the server is going away, then give their connections a moment to close.
    let _ = events.send(rooms::Event::Shutdown);
    drop(server);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), all_closed.recv()).await;
    event_loop.abort();
    Ok(())
}

async fn shutdown_signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = interrupt => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = interrupt.await;
}

async fn invite_page() -> Response {
    let mut response = nori_sync_room::INVITE_PAGE.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(nori_sync_room::INVITE_PAGE_POLICY));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

async fn asset_links(State(server): State<Arc<Server>>) -> Response {
    match nori_sync_room::asset_links(&server.config.app_link_fingerprints) {
        Some(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn stats(State(server): State<Arc<Server>>) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], server.counts.render()).into_response()
}

async fn room(
    State(server): State<Arc<Server>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let address = match server.config.trust_proxy {
        true => forwarded_for(&headers).unwrap_or_else(|| peer.ip().to_string()),
        false => peer.ip().to_string(),
    };
    let connect = nori_sync_room::parse_connect(uri.path(), uri.query()).map(|c| c.map(|c| (rooms::room_key(c.name), c.kind)));
    let Some(connect) = connect else { return StatusCode::NOT_FOUND.into_response() };
    let max = server.config.max_websocket_message;
    let open = server.open.clone();
    upgrade.max_message_size(max).max_frame_size(max).on_upgrade(move |socket| async move {
        connection::run(socket, connect, address, server.rooms.clone(), server.counts.clone(), server.config.debug_events).await;
        drop(open);
    })
}

/// The client's address as the last proxy saw it: the rightmost `X-Forwarded-For` entry.
fn forwarded_for(headers: &HeaderMap) -> Option<String> {
    let all = headers.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(",");
    all.rsplit(',').map(str::trim).find(|s| !s.is_empty()).map(str::to_string)
}
