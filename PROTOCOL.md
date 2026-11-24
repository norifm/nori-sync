# The nori sync protocol, version 1

This page is the whole contract between a client and a nori sync server. A client written from it alone
works with both shells: the Cloudflare Worker and the self-hosted program.

The server keeps rooms. Members of a room send each other opaque binary payloads through it. The server
never looks inside a payload, stores none and replays none. It also answers time requests, keeps one
opaque blob for a group room if asked, and serves the jam invite page.

## Connecting

A member opens a WebSocket to:

```
GET /room/<name>?v=1&kind=<kind>
```

over `wss://` (or `ws://` for a local server). The query's order does not matter. Other query
parameters are ignored.

| Part | Value |
|---|---|
| `<name>` | The room's name: exactly 43 characters of base64url without padding (`A-Z a-z 0-9 - _`), which is 32 bytes. The client makes it (random, or a one-way fingerprint of a key). Whoever knows it can join, so it is a secret; the server never logs it |
| `v` | The protocol version, `1`. Missing or another value: the server refuses with `version_unsupported` |
| `kind` | `group`, `meeting` or `jam` (below). Missing or another value: `bad_request` |

The server always completes the WebSocket handshake, also to refuse: it then closes at once with a
close code from [Refusals](#refusals), so that a browser, which cannot read an HTTP status, still learns
why. When the server cannot run at all (Cloudflare's own daily limit for the Worker), the handshake fails
with an HTTP error instead (often 429 or 503). A client treats that as `unavailable`.

When the server admits the connection, its first frame is a `welcome`.

### Kinds of room

The kind is chosen by the connection that opens the room. A connection that asks for another kind while
the room exists is refused with `wrong_kind`.

| Kind | For | Members | Lifetime |
|---|---|---|---|
| `group` | A person's paired devices | at most 20 | Lives as long as it has members. It may hold one kept copy, which outlives the members (see [The kept copy](#the-kept-copy)) |
| `meeting` | Pairing a new device | at most 2 | Opens at its first connection and closes 5 minutes later: every connection is closed with `room_closed`. For 1 hour after that, a new connection is refused with `room_closed`; then the name is free again |
| `jam` | A jam's host and guests | at most 20 | Lives while it has members. When the last member leaves, nothing of it is left, so the next connection opens it again |

The numbers are the defaults of the public server; a server may set others and states its own in the
`welcome`.

A room "exists" for `wrong_kind` while it has members, a group room also while it holds a kept copy, and a
meeting room until its hour after closing ends.

## Frames

Everything except keep-alive is a **binary** frame. The first byte is the frame's tag. Integers are
big-endian. `u16` is 2 bytes, `u32` 4, `i64` 8 (two's complement). Times are microseconds since
1970-01-01 UTC on the server's clock. `payload` and `blob` are the rest of the frame, possibly empty.

A frame is at most `max_frame` bytes, tag included (65,536 by default, stated in the `welcome`).

### From a member to the server

| Tag | Name | Body after the tag | The server answers |
|---|---|---|---|
| `0x01` | `send_all` | `payload` | Nothing. Every other member gets a `message` |
| `0x02` | `send_to` | `u16 member`, `payload` | Nothing. That member gets a `message`. Refused with `no_member` when no such member is in the room, or when it is the sender |
| `0x03` | `time` | `i64 t1`: the member's own clock, returned unchanged | `time` |
| `0x04` | `kept_get` | nothing | `kept` |
| `0x05` | `kept_put` | 32 bytes `secret`, `blob` | `done` (`0x05`) or `refused` |
| `0x06` | `kept_delete` | 32 bytes `secret` | `done` (`0x06`) or `refused` |

`kept_get`, `kept_put` and `kept_delete` work only in a group room. In another room, and for any frame
that does not parse (an unknown tag, a body too short), the server closes the connection with
`bad_request`. A text frame other than `ping` is also `bad_request`.

### From the server to a member

| Tag | Name | Body after the tag |
|---|---|---|
| `0x80` | `welcome` | `u8 version` (1), `u8 kind` (1 group, 2 meeting, 3 jam), `u16 you`, `u32 max_frame`, `u32 messages_per_minute`, `u32 bytes_per_minute`, `u16 max_members`, `i64 closes_at` (a meeting room: when it closes; other rooms: 0), `u16 count`, then `count` × `u16 member`: the others in the room now |
| `0x81` | `joined` | `u16 member` |
| `0x82` | `left` | `u16 member` |
| `0x83` | `message` | `u16 from`, `u8 to` (0 everyone, 1 you alone), `payload` exactly as sent |
| `0x84` | `time` | `i64 t1` (as sent), `i64 t2` (when the request arrived), `i64 t3` (when the answer left) |
| `0x85` | `kept` | `u8 present` (0 nothing stored, 1 stored), then the `blob` if present |
| `0x86` | `done` | `u8 tag` of the request it answers (`0x05` or `0x06`) |
| `0x87` | `refused` | `u8 tag` of the refused request, `u16 code`, `u32 retry_after_ms` (0 when waiting does not help) |

A server may add tags in a later version only with a new `v`. A client ignores nothing silently: an
unknown tag from a server speaking `v=1` is a bug.

### Members

The server gives each admitted connection a member number, 1 to 65,535. A number is not given again
while the room has members, so a `send_to` never reaches the wrong device. (After 65,535 joins without
the room ever being empty, the server takes the lowest free number.) When the room has been empty,
numbering may start again. Numbers mean nothing outside one room and one stay in it; a reconnecting
device gets a new number and says who it is inside its own payloads.

Order, for each member: its `welcome` comes first; then every `joined`, `left` and `message` in the order
the server handled them. A new member is announced with `joined` to the others after its `welcome` is
sent. A member that closes, or that the server closes, is announced with `left` to the rest.

`message` frames are not stored: a member that is not connected misses them. A sender does not get its
own `send_all` back.

### Time

`time` lets a client set its clock by the server's, as NTP does. The member sends its clock as `t1`, notes
its clock again as `t4` when the answer comes, and takes the server's offset as
`((t2 - t1) + (t3 - t4)) / 2`, with the round trip `(t4 - t1) - (t3 - t2)`. Of several exchanges, the one
with the shortest round trip is the most accurate.

On Cloudflare, `t2` equals `t3` and both have millisecond resolution (the platform's clock moves only
between events). The self-hosted server gives microseconds.

Ask a few times when joining (for example 4 to 8 exchanges) and rarely after (every few minutes while a
jam plays). Every request is a billed incoming message on Cloudflare: never use `time` as a heartbeat.

### The kept copy

A group room can hold one opaque `blob` of at most `max_frame - 33` bytes, the group's encrypted
profiles. The server sees only its length.

- `kept_put` with no copy stored: the server stores the blob and the SHA-256 of `secret`. The secret is
  the write secret from then on; the server never stores it, only its hash.
- `kept_put` with a copy stored: the `secret` must hash to the stored one, or the answer is
  `refused` `wrong_secret`. A match replaces the blob.
- `kept_delete`: the same check; a match deletes the blob and the secret's hash, so the next put sets a
  new secret. Deleting when nothing is stored answers `done`.
- `kept_get`: needs no secret. Any member of the group room may read it.
- A copy is deleted 8 weeks after the last time any member connected to the room. While members are
  connected it is kept.
- A group room that never puts a copy stores nothing at all.

The 32-byte secret is the client's choice, for example a key derived from the group key. Two puts at once
are applied in the order they arrive; the last one wins.

### Keep-alive

Idle WebSockets get closed by networks and by Cloudflare. A member keeps its connection up with:

- **WebSocket pings** (opcode 0x9), from every client that can send them. The server answers with a pong.
  On Cloudflare the platform answers without waking the room: no request is billed and no time is used.
- **The text frame `ping`**, from a browser, which cannot send WebSocket pings. The server answers with
  the text frame `pong`. On Cloudflare this is the room's automatic response: it does not wake the room
  and uses no billed time. Cloudflare's documents do not say whether it counts as an incoming message
  (1/20 of a request); at worst, one every 50 s costs 3.6 requests an hour.

Send one after about 50 s without traffic. Neither ping counts against the rate limits, and neither is a
member event: a room that only gets pings stays asleep.

Never send anything else on a timer. Cloudflare bills every incoming message.

### Reconnecting

Cloudflare closes WebSockets when it updates its runtime (close code 1001, or 1006 without a close
frame). A client reconnects quietly after a random wait of 1 to 5 s, then backs off (doubling up to a few
minutes) while connecting fails. After a refusal, a client follows the refusal's advice below and does
not retry at once.

## Limits

Each admitted connection is told its limits in the `welcome`.

| Limit | Default | What happens |
|---|---|---|
| Frame size | 65,536 bytes | Larger: `refused` `too_large`. Far larger (over 1 MiB): the connection may be closed with 1009 |
| Frames per connection per minute | 240 | Over: `refused` `too_fast` with the wait until the minute ends. Twice over: closed with `too_fast` |
| Bytes per connection per minute | 4 MiB | As for frames |
| Members per room | 20 (2 in a meeting room) | Closed with `room_full` at connect |
| New rooms per network address per minute | 30 | Closed with `too_many_rooms` at connect. On Cloudflare each of its locations counts apart, so the limit is approximate |
| The server's daily budget (Cloudflare's free plan) | | Closed with `daily_limit` at connect; resets at 00:00 UTC |

Every binary frame a member sends counts against its per-minute limits, the refused ones too. The
minute is counted from the connection's first frame in it. Pings do not count.

Opening a room means connecting when it has no members. Rejoining a group room that still has members
does not open it.

## Refusals

A refusal is a normal state. Clients show it, keep working without the server (devices on one network
still talk directly) and try again later.

| Code | Name | Sent as | Meaning, and what a client does |
|---|---|---|---|
| 4000 | `bad_request` | close | The client sent something this version does not allow. A bug: report it, do not retry unchanged |
| 4001 | `version_unsupported` | close | The server does not speak this `v`. The client or the server needs an update |
| 4002 | `wrong_kind` | close | The room exists as another kind |
| 4003 | `room_full` | close | The room has its maximum of members |
| 4004 | `room_closed` | close | A meeting room past its 5 minutes. Make a new pairing code |
| 4005 | `too_many_rooms` | close | Too many rooms opened from this network address. Retry after a minute |
| 4006 | `daily_limit` | close | The server has used its daily budget. Retry after 00:00 UTC |
| 4007 | `unavailable` | close, or `refused` for a kept-copy request | The room or its store could not be reached. Retry after a backoff |
| 4008 | `too_large` | `refused` | A frame over `max_frame`. The frame was dropped |
| 4009 | `too_fast` | `refused`, then close | Over a per-minute limit. The frame was dropped; wait `retry_after_ms` |
| 4010 | `wrong_secret` | `refused` | A `kept_put` or `kept_delete` with a secret that does not match |
| 4011 | `no_member` | `refused` | A `send_to` to a member who is not in the room |

The close frame's reason is the code's name, for logs. Standard close codes can also come: 1001 (the
server is going away), 1006 (the network dropped), 1009 (a frame far too large), 1011 (a server error).

`refused` for `too_fast` is sent once per minute; further frames in that minute are dropped without an
answer, and a connection that sends twice its limit in a minute is closed.

## The invite page

`GET /jam` serves one static HTML page with its script and style inline. It makes no other request and
holds no analytics. It is the same on every server.

An invite link has this shape:

```
https://<server>/jam#s=<server>&k=<key>
```

| Part | Value |
|---|---|
| `<server>` before `/jam` | The sync server's address, scheme and host (and port, and a path prefix if it has one) |
| `s` | The same address, percent-encoded (`https%3A%2F%2Fsync.example`) |
| `k` | The jam's key, as the client encodes it (base64url fits without escaping). The client derives the jam room's name from it |
| More `name=value` pairs | Allowed; the page passes them on unchanged |

Everything after `#` stays in the browser: the server never receives it.

The page hands the fragment, unchanged, to the app:

| Where | How |
|---|---|
| Android | It navigates at once to `intent://jam?<fragment>#Intent;scheme=nori;package=dev.nori.music;end`, which opens nori as `nori://jam?<fragment>`. Its **Open in nori** button carries the same intent with `S.browser_fallback_url` set to where to get nori |
| Elsewhere | **Open in nori** is the link `nori://jam?<fragment>`. **Copy link** copies the whole invite link, for a client that takes a pasted link |
| No `s` or no `k` in the fragment | The page says the link is incomplete |

So an app receives `nori://jam?s=<server>&k=<key>[&...]`, the same query as today's octo-fiesta invite
(`/nori/jam#s=...&k=...`). A client accepts both the page link and the `nori://jam` link.

When the server is configured with Android app-link fingerprints (see the README), it also serves
`/.well-known/assetlinks.json` for `dev.nori.music`. An app that declares `https://<server>/jam` as a
verified app link then opens such links directly, without the page.

## Other paths

| Path | What |
|---|---|
| `GET /` | A one-line text that this is a nori sync server, and the protocol versions it speaks: `nori-sync v1` |
| `GET /jam` | The invite page |
| `GET /.well-known/assetlinks.json` | Android app links, when configured; 404 otherwise |
| `GET /stats` | Self-hosted only: the operator's counts, in Prometheus text format. Never anything about a user |

Everything else is 404.
