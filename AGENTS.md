# nori sync server

The relay nori's devices pair, sync and jam through (README.md). PROTOCOL.md is the contract with every
nori client; a change to it is a change to the clients too, and a new `v`.

## Commands

```sh
cargo test -j4 --workspace                    # never -j above 4 (the machine runs out of memory)
cargo clippy -j4 --workspace --all-targets -- -D warnings
(cd cloudflare && cargo clippy -j4 --target wasm32-unknown-unknown -- -D warnings)
cargo fmt --all && (cd cloudflare && cargo fmt)
tools/conformance.sh self|docker|cloudflare   # the suite against one shell; cloudflare runs `wrangler dev` locally
```

Before committing: the tests, both clippy runs, and `tools/conformance.sh self` and `cloudflare` when a
change reaches the room logic or a shell. Never deploy, `wrangler login` or push unless the owner asks.

## Layout

| Path | What |
|---|---|
| `crates/room` | Every rule: members, delivery, kinds, lifetimes, limits, time, the kept copy, the frames |
| `crates/server` | The self-hosted shell: one event loop owning every room, a file per stored room |
| `crates/conformance` | The suite over real WebSockets, the same for every shell |
| `cloudflare` | The Cloudflare shell: Worker and Durable Object, its own workspace |

## Boundaries

- Every rule lives once, in `crates/room`, tested there. A shell only converts: sockets, storage,
  alarms, the clock, its configuration.
- The room logic keeps nothing in memory between events. What it needs travels with each connection
  (`Member`, a Durable Object's attachment) or sits in the room's store. The self-hosted shell obeys the
  same rule, so both behave alike.
- The protocol stays quiet: nothing is sent on a timer, keep-alives never reach the room logic, and
  nothing keeps a Cloudflare room awake (no timers, no pending fetches).
- The server never looks inside a payload, never logs a room's name or a network address, and stores
  only what PROTOCOL.md says it stores.

## Code

- American English in prose, comments, docs and user-facing text.
- Comments are short and factual: what a thing is, or why a non-obvious choice was made. No history, no
  restating the code. Prefer a clear name over a comment.
- "None" or "special" is an `Option` or an enum, not a magic value; a value on the wire may keep a
  sentinel, converted at that boundary.
- State lives in the struct that owns it and is passed in; no globals holding state.
- Fix the cause of a bug, not its symptom: reproduce it, prove it with a failing test, fix it there. No
  flags steering other code, no retry loops or sleeps, no safeguards for states that should not arise.
  Then look for the same cause everywhere else.
- No abstraction with one implementation or caller, no dead code, no unused `pub`, no copied blocks.
- Touch only what the task needs; prefer the change that removes code while keeping the behavior.

## Tests

- Room behavior is tested in `crates/room` on a clock the test moves (`tests.rs`), never with sleeps.
- The conformance suite checks what only a real server shows: the wire, the shells, hibernation.
- A bug fix starts with a test that fails on the bug.
- Every test can fail on a real bug. No asserting constants or defaults, no asserts an `if` can skip.
  Near-duplicates become one table test.

## Commits

One line, conventional: `<type>: <what is different now>`, type in `feat fix perf refactor docs build
test chore`, lowercase, no full stop, under 72 characters. No body, no trailers, no `Co-Authored-By`.
One commit per piece of work; fix your own unpushed work by amending the commit it fixes.
