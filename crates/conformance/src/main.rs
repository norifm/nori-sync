//! Checks a running nori sync server against PROTOCOL.md over real WebSockets. The same checks run
//! against the self-hosted program and the Cloudflare Worker (`wrangler dev`); tools/conformance.sh
//! starts either with the short limits the checks expect.
//!
//! ```text
//! nori-sync-conformance <ws://host:port> [--events-log FILE] [--expect-unload] [--kept-seconds N] [--only NAME,...]
//! ```
//!
//! `--events-log` is the server's output with debug events on: a `room event` line each time a room's
//! logic ran, and on Cloudflare a `room loaded` line each time a room was built in memory.

mod client;

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use client::{Check, Client, Got, http_get, room_name};
use nori_sync_room::{Answer, Kind, Refusal, Request, RequestKind, SECRET_LEN, To};
use tokio_tungstenite::tungstenite::{Bytes, Message};

struct Options {
    base: String,
    events_log: Option<String>,
    expect_unload: bool,
    kept_seconds: Option<u64>,
}

type CheckFn = for<'a> fn(&'a Options) -> Pin<Box<dyn Future<Output = Check> + 'a>>;

macro_rules! checks {
    ($($name:ident),* $(,)?) => { [$((stringify!($name), (|o| Box::pin($name(o))) as CheckFn)),*] };
}

/// In order: the rooms limit last, since it uses up this address's rooms for a minute.
const CHECKS: [(&str, CheckFn); 14] = checks![
    pages,
    connect_refusals,
    members_and_messages,
    numbers_are_not_reused,
    time,
    kinds_and_sizes,
    frame_limits,
    keep_alive,
    kept_copy,
    meeting_lifetime,
    jam_leaves_nothing,
    unloaded_room_carries_on,
    kept_copy_expires,
    rooms_per_address,
];

fn main() {
    let mut args = std::env::args().skip(1);
    let mut options = Options { base: String::new(), events_log: None, expect_unload: false, kept_seconds: None };
    let mut only: Option<Vec<String>> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--events-log" => options.events_log = args.next(),
            "--expect-unload" => options.expect_unload = true,
            "--kept-seconds" => options.kept_seconds = args.next().and_then(|s| s.parse().ok()),
            "--only" => only = args.next().map(|s| s.split(',').map(str::to_string).collect()),
            base if !base.starts_with("--") => options.base = base.trim_end_matches('/').to_string(),
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
    }
    if options.base.is_empty() {
        eprintln!("usage: nori-sync-conformance <ws://host:port> [--events-log FILE] [--expect-unload] [--kept-seconds N] [--only NAME,...]");
        std::process::exit(2);
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let mut failed = 0;
    for (name, check) in CHECKS {
        if only.as_ref().is_some_and(|o| !o.iter().any(|n| n == name)) {
            continue;
        }
        let started = std::time::Instant::now();
        let result = runtime.block_on(check(&options));
        let took = started.elapsed().as_secs_f32();
        match result {
            Ok(()) => println!("ok    {name} ({took:.1} s)"),
            Err(why) => {
                failed += 1;
                println!("FAIL  {name} ({took:.1} s): {why}");
            }
        }
    }
    println!("{}", if failed == 0 { "all passed".to_string() } else { format!("{failed} failed") });
    std::process::exit(i32::from(failed > 0));
}

fn ensure(condition: bool, why: impl FnOnce() -> String) -> Check {
    if condition { Ok(()) } else { Err(why()) }
}

fn now_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as i64)
}

fn http_base(o: &Options) -> String {
    o.base.replacen("ws://", "http://", 1)
}

async fn pages(o: &Options) -> Check {
    let base = http_base(o);
    let (status, _, body) = http_get(&base, "/").await?;
    ensure(status == 200 && body == nori_sync_room::ABOUT, || format!("GET / gave {status} {body:?}"))?;
    let (status, head, body) = http_get(&base, "/jam").await?;
    ensure(status == 200 && body == nori_sync_room::INVITE_PAGE, || format!("GET /jam gave {status}, not the page"))?;
    ensure(head.contains("content-type: text/html"), || "the page is not HTML".into())?;
    ensure(head.contains("content-security-policy: default-src 'none'"), || "the page has no Content-Security-Policy".into())?;
    let (status, _, _) = http_get(&base, "/nothing-here").await?;
    ensure(status == 404, || format!("an unknown path gave {status}"))?;
    let (status, _, _) = http_get(&base, "/.well-known/assetlinks.json").await?;
    ensure(status == 404, || format!("asset links without fingerprints gave {status}"))
}

async fn connect_refusals(o: &Options) -> Check {
    let name = room_name();
    for (query, refusal) in [
        ("v=2&kind=group", Refusal::VersionUnsupported),
        ("kind=group", Refusal::VersionUnsupported),
        ("v=1&kind=party", Refusal::BadRequest),
        ("v=1", Refusal::BadRequest),
    ] {
        let mut client = Client::connect_query(&o.base, &name, query).await?;
        client.expect_closed(refusal).await.map_err(|e| format!("{query}: {e}"))?;
    }
    let mut client = Client::connect_query(&o.base, "too-short", "v=1&kind=jam").await?;
    client.expect_closed(Refusal::BadRequest).await.map_err(|e| format!("a short name: {e}"))
}

async fn members_and_messages(o: &Options) -> Check {
    let name = room_name();
    let (mut a, wa) = Client::join(&o.base, &name, Kind::Group).await?;
    ensure(wa.version == 1 && wa.kind == Kind::Group && wa.others.is_empty() && wa.closes_at.is_none(), || format!("{wa:?}"))?;
    let (mut b, wb) = Client::join(&o.base, &name, Kind::Group).await?;
    ensure(wb.others == [wa.you] && wb.you != wa.you, || format!("{wb:?}"))?;
    a.expect(Answer::Joined(wb.you)).await?;
    let (mut c, wc) = Client::join(&o.base, &name, Kind::Group).await?;
    a.expect(Answer::Joined(wc.you)).await?;
    b.expect(Answer::Joined(wc.you)).await?;

    let payload: Vec<u8> = (0..=255).collect();
    a.send(Request::SendAll(&payload)).await?;
    b.expect(Answer::Message { from: wa.you, to: To::Everyone, payload: &payload }).await?;
    c.expect(Answer::Message { from: wa.you, to: To::Everyone, payload: &payload }).await?;
    a.nothing_else().await.map_err(|e| format!("the sender: {e}"))?;
    c.send(Request::SendTo { member: wb.you, payload: b"" }).await?;
    b.expect(Answer::Message { from: wc.you, to: To::You, payload: b"" }).await?;
    a.nothing_else().await?;
    c.send(Request::SendTo { member: 999, payload: b"x" }).await?;
    c.expect(Answer::Refused { request: RequestKind::SendTo, refusal: Refusal::NoMember, retry_after_ms: 0 }).await?;
    c.close().await?;
    a.expect(Answer::Left(wc.you)).await?;
    b.expect(Answer::Left(wc.you)).await?;
    a.close().await?;
    b.close().await
}

async fn numbers_are_not_reused(o: &Options) -> Check {
    let name = room_name();
    let (mut a, wa) = Client::join(&o.base, &name, Kind::Jam).await?;
    let (b, wb) = Client::join(&o.base, &name, Kind::Jam).await?;
    a.expect(Answer::Joined(wb.you)).await?;
    b.close().await?;
    a.expect(Answer::Left(wb.you)).await?;
    let (c, wc) = Client::join(&o.base, &name, Kind::Jam).await?;
    ensure(wc.you != wb.you && wc.you != wa.you && wc.others == [wa.you], || format!("{wc:?} after {wa:?} and {wb:?}"))?;
    c.close().await?;
    a.close().await
}

async fn time(o: &Options) -> Check {
    let (mut a, _) = Client::join(&o.base, &room_name(), Kind::Jam).await?;
    let mut best: Option<(i64, i64)> = None;
    for i in 0..6 {
        let t1 = now_us();
        a.send(Request::Time { t1: t1 + i }).await?;
        let Got::Frame(f) = a.next().await? else { return Err("no time answer".into()) };
        let t4 = now_us();
        let Some(Answer::Time { t1: echoed, t2, t3 }) = Answer::parse(&f) else { return Err(format!("not a time answer: {f:?}")) };
        ensure(echoed == t1 + i && t2 <= t3, || format!("t1 {t1} came back as {echoed}, t2 {t2}, t3 {t3}"))?;
        let trip = (t4 - t1) - (t3 - t2);
        let offset = ((t2 - t1) + (t3 - t4)) / 2;
        if best.is_none_or(|(b, _)| trip < b) {
            best = Some((trip, offset));
        }
    }
    let (trip, offset) = best.unwrap_or_default();
    // A server on this machine shares its clock: the offset is within the round trip and the clock's step.
    ensure(offset.abs() <= trip + 2_000, || format!("offset {offset} µs with a round trip of {trip} µs"))?;
    a.close().await
}

async fn kinds_and_sizes(o: &Options) -> Check {
    let name = room_name();
    let (a, wa) = Client::join(&o.base, &name, Kind::Meeting).await?;
    ensure(wa.max_members == 2 && wa.closes_at.is_some(), || format!("{wa:?}"))?;
    Client::refused(&o.base, &name, Kind::Group, Refusal::WrongKind).await?;
    let (b, _) = Client::join(&o.base, &name, Kind::Meeting).await?;
    Client::refused(&o.base, &name, Kind::Meeting, Refusal::RoomFull).await?;
    b.close().await?;
    a.close().await?;

    let name = room_name();
    let (first, w) = Client::join(&o.base, &name, Kind::Jam).await?;
    let mut members = vec![first];
    for _ in 1..w.max_members {
        members.push(Client::join(&o.base, &name, Kind::Jam).await?.0);
    }
    Client::refused(&o.base, &name, Kind::Jam, Refusal::RoomFull).await?;
    for m in members {
        m.close().await?;
    }
    Ok(())
}

async fn frame_limits(o: &Options) -> Check {
    let name = room_name();
    let (mut a, wa) = Client::join(&o.base, &name, Kind::Jam).await?;
    let (mut b, wb) = Client::join(&o.base, &name, Kind::Jam).await?;
    a.expect(Answer::Joined(wb.you)).await?;
    let big = vec![0u8; wa.max_frame as usize];
    a.send(Request::SendAll(&big)).await?;
    a.expect(Answer::Refused { request: RequestKind::SendAll, refusal: Refusal::TooLarge, retry_after_ms: 0 }).await?;
    a.send(Request::SendAll(&big[1..])).await?;
    b.expect(Answer::Message { from: wa.you, to: To::Everyone, payload: &big[1..] }).await?;
    // Two frames are used; the rest of the minute's allowance passes, the next is refused.
    let allowed = wa.frames_per_minute - 2;
    for _ in 0..allowed {
        a.send(Request::SendAll(b"x")).await?;
    }
    for _ in 0..allowed {
        b.expect(Answer::Message { from: wa.you, to: To::Everyone, payload: b"x" }).await?;
    }
    a.send(Request::SendAll(b"over")).await?;
    let Got::Frame(f) = a.next().await? else { return Err("no refusal".into()) };
    match Answer::parse(&f) {
        Some(Answer::Refused { request: RequestKind::SendAll, refusal: Refusal::TooFast, retry_after_ms }) if retry_after_ms <= 60_000 => {}
        other => return Err(format!("expected too_fast, got {other:?}")),
    }
    for _ in 0..wa.frames_per_minute {
        a.send(Request::SendAll(b"over")).await?;
    }
    a.expect_closed(Refusal::TooFast).await?;
    b.expect(Answer::Left(wa.you)).await.map_err(|e| format!("the others: {e}"))?;
    b.close().await
}

async fn keep_alive(o: &Options) -> Check {
    let (mut a, w) = Client::join(&o.base, &room_name(), Kind::Group).await?;
    // Pings never count against the limits: twice the minute's frames of each kind, and the member stays.
    for _ in 0..2 * w.frames_per_minute {
        a.send_raw(Message::Ping(Bytes::from_static(b"k"))).await?;
        ensure(a.next().await? == Got::Pong, || "a WebSocket ping got no pong".into())?;
        a.send_raw(Message::text(nori_sync_room::PING)).await?;
        ensure(a.next().await? == Got::Text(nori_sync_room::PONG.into()), || "the text ping got no pong".into())?;
    }
    a.nothing_else().await?;
    a.send_raw(Message::text("hello")).await?;
    a.expect_closed(Refusal::BadRequest).await.map_err(|e| format!("other text: {e}"))
}

async fn kept_copy(o: &Options) -> Check {
    let name = room_name();
    let (right, wrong) = ([3u8; SECRET_LEN], [4u8; SECRET_LEN]);
    let (mut a, wa) = Client::join(&o.base, &name, Kind::Group).await?;
    let (mut b, wb) = Client::join(&o.base, &name, Kind::Group).await?;
    a.expect(Answer::Joined(wb.you)).await?;
    ensure(a.kept().await?.is_none(), || "a new group room has a kept copy".into())?;
    let blob = vec![0xC5; wa.max_frame as usize - 1 - SECRET_LEN];
    a.send(Request::KeptPut { secret: &right, blob: &blob }).await?;
    a.expect(Answer::Done(RequestKind::KeptPut)).await?;
    ensure(b.kept().await?.as_deref() == Some(&blob[..]), || "the other member reads another copy".into())?;
    b.send(Request::KeptPut { secret: &wrong, blob: b"no" }).await?;
    b.expect(Answer::Refused { request: RequestKind::KeptPut, refusal: Refusal::WrongSecret, retry_after_ms: 0 }).await?;
    b.send(Request::KeptDelete { secret: &wrong }).await?;
    b.expect(Answer::Refused { request: RequestKind::KeptDelete, refusal: Refusal::WrongSecret, retry_after_ms: 0 }).await?;
    b.send(Request::KeptPut { secret: &right, blob: b"second" }).await?;
    b.expect(Answer::Done(RequestKind::KeptPut)).await?;
    a.close().await?;
    b.close().await?;

    // The copy outlives its members, and keeps the room a group room.
    Client::refused(&o.base, &name, Kind::Jam, Refusal::WrongKind).await?;
    let (mut c, _) = Client::join(&o.base, &name, Kind::Group).await?;
    ensure(c.kept().await?.as_deref() == Some(b"second"), || "the copy did not outlive its members".into())?;
    c.send(Request::KeptDelete { secret: &right }).await?;
    c.expect(Answer::Done(RequestKind::KeptDelete)).await?;
    ensure(c.kept().await?.is_none(), || "deleted, yet still there".into())?;
    c.close().await?;

    let (mut jam, _) = Client::join(&o.base, &room_name(), Kind::Jam).await?;
    jam.send(Request::KeptGet).await?;
    jam.expect_closed(Refusal::BadRequest).await.map_err(|e| format!("kept_get in a jam: {e}"))
}

async fn meeting_lifetime(o: &Options) -> Check {
    let name = room_name();
    let (mut a, wa) = Client::join(&o.base, &name, Kind::Meeting).await?;
    let closes_at = wa.closes_at.ok_or("a meeting room without an end")?;
    let wait = Duration::from_micros((closes_at - now_us()).max(0) as u64);
    ensure(wait < Duration::from_secs(30), || format!("the meeting room closes in {wait:?}: run the server with NORI_SYNC_MEETING_SECONDS=4"))?;
    let (mut b, wb) = Client::join(&o.base, &name, Kind::Meeting).await?;
    ensure(wb.closes_at == wa.closes_at, || "the second member moved the end".into())?;
    a.expect(Answer::Joined(wb.you)).await?;
    tokio::time::sleep(wait).await;
    a.expect_closed(Refusal::RoomClosed).await?;
    b.expect_closed(Refusal::RoomClosed).await?;
    Client::refused(&o.base, &name, Kind::Meeting, Refusal::RoomClosed).await
}

async fn jam_leaves_nothing(o: &Options) -> Check {
    let name = room_name();
    let (mut a, _) = Client::join(&o.base, &name, Kind::Jam).await?;
    a.send(Request::SendAll(b"x")).await?;
    a.close().await?;
    // The server may not have handled the close yet: while it still holds the jam, a group is refused.
    for _ in 0..100 {
        let mut c = Client::connect(&o.base, &name, Kind::Group).await?;
        match c.next().await? {
            Got::Frame(f) => {
                ensure(matches!(Answer::parse(&f), Some(Answer::Welcome(w)) if w.others.is_empty()), || format!("{:?}", Answer::parse(&f)))?;
                return c.close().await;
            }
            Got::Closed(Some(code)) if code == Refusal::WrongKind.code() => tokio::time::sleep(Duration::from_millis(20)).await,
            other => return Err(format!("the emptied jam room: {other:?}")),
        }
    }
    Err("the jam room still had its member 2 s after it left".into())
}

fn events(o: &Options, what: &str) -> Result<usize, String> {
    let Some(log) = &o.events_log else { return Ok(0) };
    let text = std::fs::read_to_string(log).map_err(|e| format!("the events log {log}: {e}"))?;
    Ok(text.lines().filter(|l| l.contains(what)).count())
}

async fn unloaded_room_carries_on(o: &Options) -> Check {
    let name = room_name();
    let (mut a, wa) = Client::join(&o.base, &name, Kind::Group).await?;
    let (mut b, wb) = Client::join(&o.base, &name, Kind::Group).await?;
    a.expect(Answer::Joined(wb.you)).await?;
    // Cloudflare unloads an idle room after about ten seconds.
    tokio::time::sleep(Duration::from_secs(15)).await;
    let (events_before, loads_before) = (events(o, "room event")?, events(o, "room loaded")?);
    for _ in 0..5 {
        a.send_raw(Message::Ping(Bytes::from_static(b"k"))).await?;
        ensure(a.next().await? == Got::Pong, || "no pong".into())?;
        b.send_raw(Message::text(nori_sync_room::PING)).await?;
        ensure(b.next().await? == Got::Text(nori_sync_room::PONG.into()), || "no text pong".into())?;
    }
    ensure(events(o, "room event")? == events_before, || "pings woke the room".into())?;
    ensure(events(o, "room loaded")? == loads_before, || "pings loaded the room".into())?;
    a.send(Request::SendAll(b"after")).await?;
    b.expect(Answer::Message { from: wa.you, to: To::Everyone, payload: b"after" }).await?;
    if o.expect_unload {
        ensure(events(o, "room loaded")? > loads_before, || "the room was not unloaded while idle, so this proved nothing".into())?;
    }
    let (c, wc) = Client::join(&o.base, &name, Kind::Group).await?;
    ensure(wc.others.len() == 2 && wc.others.contains(&wa.you) && wc.others.contains(&wb.you) && wc.you > wb.you, || format!("{wc:?}"))?;
    a.expect(Answer::Joined(wc.you)).await?;
    for client in [a, b, c] {
        client.close().await?;
    }
    Ok(())
}

async fn kept_copy_expires(o: &Options) -> Check {
    let Some(seconds) = o.kept_seconds else { return Ok(()) };
    let name = room_name();
    let secret = [5u8; SECRET_LEN];
    let (mut a, _) = Client::join(&o.base, &name, Kind::Group).await?;
    a.send(Request::KeptPut { secret: &secret, blob: b"old" }).await?;
    a.expect(Answer::Done(RequestKind::KeptPut)).await?;
    a.close().await?;
    tokio::time::sleep(Duration::from_secs(seconds + 2)).await;
    let (mut b, _) = Client::join(&o.base, &name, Kind::Jam).await.map_err(|e| format!("the room is still a group's: {e}"))?;
    b.send(Request::SendAll(b"x")).await?;
    b.close().await
}

async fn rooms_per_address(o: &Options) -> Check {
    let name = room_name();
    let (mut held, _) = Client::join(&o.base, &name, Kind::Group).await?;
    for opened in 0..1000 {
        let mut client = Client::connect(&o.base, &room_name(), Kind::Jam).await?;
        match client.next().await? {
            Got::Frame(_) => client.close().await?,
            Got::Closed(Some(code)) if code == Refusal::TooManyRooms.code() => {
                // Joining a room that has members opens nothing.
                let (other, _) = Client::join(&o.base, &name, Kind::Group).await.map_err(|e| format!("joining after {opened} rooms: {e}"))?;
                held.expect(Answer::Joined(2)).await?;
                other.close().await?;
                return held.close().await;
            }
            other => return Err(format!("after {opened} rooms: {other:?}")),
        }
    }
    Err("1000 rooms opened from one address in a minute".into())
}
