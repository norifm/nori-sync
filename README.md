# nori sync server

A small public relay for [nori](https://norifm.com), the music player. It lets the devices a person
paired talk to each other from anywhere, and it carries a jam's messages between its host and guests.
Devices encrypt every message before they send it. The server passes the messages on; it cannot read
them.

The server keeps rooms. Each room passes messages between its members, answers time requests, and can
keep one encrypted copy of a group's profiles. It also serves the page that a jam invite opens. It does
nothing else, and it never carries music.

[PROTOCOL.md](PROTOCOL.md) is the full protocol, enough to write a client.

There are two ways to run it, from one implementation:

| Shell | Where | Rooms | Store |
|---|---|---|---|
| [Cloudflare](#deploy-to-cloudflare) | A Worker and one Durable Object per room | Unloaded from memory while idle | The Durable Object's storage |
| [Self-hosted](#run-it-yourself-in-docker) | One small program, in Docker or not | All in one process | One file per room that stores something |

## What the server can see and store

| | Sees | Stores |
|---|---|---|
| A member's network address | While it is connected | Never. The limit on new rooms counts addresses in memory for one minute (self-hosted: under a hash keyed at random at each start) |
| Room names | Yes, the name is the room's address | Self-hosted: the file name is the SHA-256 of the name. Cloudflare: Durable Objects are named by it |
| How many members a room has, when messages pass and how large they are | Yes | No |
| Message contents | Encrypted by the devices; the server never looks inside | No |
| A group's kept copy | An encrypted blob and its size | Yes, if the group asked for one: the blob, the SHA-256 of its write secret, and when a member last connected. Deleted 8 weeks after the last connection |
| A pairing (meeting) room | | When it opened, for 65 minutes |

It keeps no logs about users. The operator sees counts only: connections, messages and refusals by
reason (self-hosted: `GET /stats`; Cloudflare: its dashboard, and one `refused <reason>` log line per
refusal).

## Limits

The limits keep the server free to run and keep music off it. Each one refuses with a code the client
shows (see [PROTOCOL.md](PROTOCOL.md#refusals)). Change them with these variables: environment variables
for the self-hosted program, `[vars]` in `cloudflare/wrangler.toml` for Cloudflare.

| Variable | Default | What |
|---|---|---|
| `NORI_SYNC_MAX_FRAME` | 65536 | The largest frame a member may send, in bytes. A kept copy is at most this less 33 |
| `NORI_SYNC_FRAMES_PER_MINUTE` | 240 | Frames per connection per minute. Twice as many close the connection |
| `NORI_SYNC_BYTES_PER_MINUTE` | 4194304 | Bytes per connection per minute, likewise |
| `NORI_SYNC_MAX_MEMBERS` | 20 | Members of a group room or a jam room. A meeting room always holds 2 |
| `NORI_SYNC_ROOMS_PER_MINUTE` | 30 | New rooms per network address per minute. Self-hosted only: on Cloudflare, set `limit` under `[[ratelimits]]` in `wrangler.toml` |
| `NORI_SYNC_MEETING_SECONDS` | 300 | How long a meeting room is open after its first connection |
| `NORI_SYNC_MEETING_CLOSED_SECONDS` | 3600 | How long a closed meeting room refuses connections |
| `NORI_SYNC_KEPT_SECONDS` | 4838400 | How long a kept copy outlives the last connection to its room (8 weeks) |
| `NORI_SYNC_APP_LINK_FINGERPRINTS` | | SHA-256 fingerprints of the certificates nori's Android app is signed with, separated by commas. When set, `/.well-known/assetlinks.json` lets the app open `https://<server>/jam` links directly |

The new-rooms limit is generous on purpose: a carrier or a campus puts thousands of people behind one
address. The limits per connection and per room do the real work.

## Run it yourself in Docker

```sh
docker compose up -d --build
```

The server then listens on port 8080 and keeps its stored rooms in the volume `nori-sync-data`. Put a
reverse proxy with TLS in front of it, because clients connect with `wss://`. With Caddy:

```
sync.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Behind a proxy, set `NORI_SYNC_TRUST_PROXY=1` so the server takes the client's address from
`X-Forwarded-For`. In nori, set the sync server to `https://sync.example.com`.

The program's own settings, besides the limits:

| Variable | Default | What |
|---|---|---|
| `NORI_SYNC_BIND` | `0.0.0.0` | The address to listen on |
| `NORI_SYNC_PORT` | `8080` | The port |
| `NORI_SYNC_DATA` | `data` (`/data` in Docker) | The folder for stored rooms |
| `NORI_SYNC_TRUST_PROXY` | off | `1`: the client's address is the last `X-Forwarded-For` entry |
| `NORI_SYNC_DEBUG_EVENTS` | off | `1`: one `room event` line for each event a room handles. For the conformance suite |

Without Docker: `cargo run --release -p nori-sync-server`.

## Deploy to Cloudflare

You need a Cloudflare account, Rust with the `wasm32-unknown-unknown` target
(`rustup target add wasm32-unknown-unknown`) and Node.js.

1. In `cloudflare/wrangler.toml`, set `routes` to a domain on your account (the line marked
   **SET THIS**). Without it the server answers at `nori-sync.<account>.workers.dev`.
2. Optionally set the limits under `[vars]`.
3. Deploy:

   ```sh
   cd cloudflare
   npm ci
   npx wrangler login
   npx wrangler deploy
   ```

The first build installs `worker-build` into `cloudflare/.tools`. `worker`, `worker-build` and
`wrangler` are pinned together; update them together.

Durable Objects need no paid plan: the free plan runs this server. When the free plan's daily limit is
reached, connections are refused with `daily_limit` until 00:00 UTC. Clients treat that as a normal
state, and devices on one network keep talking directly. If that happens often, move to the paid plan.

### What Cloudflare bills

The design sends nothing on a timer, so a connected device that does nothing costs nothing.

| What | Billed as | By this design |
|---|---|---|
| Opening a connection | One Worker request and one Durable Object request | A device reconnects about 20 times a day |
| A frame a member sends | One twentieth of a Durable Object request | Messages, time requests and kept-copy requests |
| A frame the server sends | Free | Welcomes, deliveries, answers |
| A WebSocket ping | Free, and the room stays asleep | The keep-alive of every client but a browser |
| A browser's text `ping` | The room stays asleep. Cloudflare does not say whether it counts as an incoming message | At most 3.6 requests an hour per browser |
| Time while a room is idle | Nothing: idle rooms are unloaded (hibernation) | |
| Time while a room handles an event | Duration (GB-s) | A few milliseconds per event |
| An alarm | One request | Two per pairing; a group with a kept copy, about one a week |
| Storage | Rows read and written | Two small reads per connection; writes only for pairings and kept copies |

One paired device, by this design, a day: 20 connections, 6 time requests and one kept-copy read per
connection, and about 100 messages of sync and remote control. That is 20 connection requests plus
about 240 incoming frames, which bill as 12 requests: **about 32 Durable Object requests and 20 Worker
requests a day**, and well under 1 GB-s. A browser adds at most 14 requests for 4 connected hours.

- **Free plan:** 100,000 Durable Object requests a day, so about 3,000 devices a day. If the free plan
  counts every incoming frame as a whole request (Cloudflare's documents leave this open), about 380.
- **Paid plan ($5 a month):** 1 million Durable Object requests a month included, about 1,000 devices;
  each further 1,000 devices cost about $0.15 a month.

A bug that keeps a room awake (a timer, a pending fetch) would multiply the cost about 30 times. The
conformance suite checks that pings do not wake a room.

## Check it

```sh
cargo test -j4 --workspace          # the room logic on a clock the tests move, the codec, the store
tools/conformance.sh self           # the suite against the self-hosted program
tools/conformance.sh docker         # against its Docker image
tools/conformance.sh cloudflare     # against the Worker in `wrangler dev`, locally
```

The conformance suite (`crates/conformance`) speaks the protocol over real WebSockets and runs
unchanged against each shell, started with short limits. It checks connecting and refusals, members and
messages, member numbers, time, kinds and full rooms, frame and rate limits, keep-alive, the kept copy
and its expiry, a meeting room's end, a jam room leaving nothing, a room unloaded while idle carrying on
(and that pings alone do not wake it), and the new-rooms limit.

## Layout

| Path | What |
|---|---|
| `crates/room` | The room logic, platform-free: members, delivery, kinds and lifetimes, limits, time, the kept copy, the frames, the invite page |
| `crates/server` | The self-hosted program |
| `crates/conformance` | The conformance suite |
| `cloudflare` | The Worker and its Durable Object; its own Cargo workspace, built to WebAssembly |
| `tools/conformance.sh` | Starts a shell and runs the suite against it |

The license is MIT: see `LICENSE`.
