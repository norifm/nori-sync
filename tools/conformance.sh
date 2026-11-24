#!/usr/bin/env bash
# Runs the conformance suite against one shell, started here with the short limits the checks expect,
# and stops it afterwards.
#   tools/conformance.sh self          the self-hosted program (cargo build)
#   tools/conformance.sh cloudflare    the Worker under `wrangler dev` (local only, nothing deployed)
# Extra arguments go to the suite (for example --only keep_alive,time).
set -euo pipefail
cd "$(dirname "$0")/.."
shell=${1:?usage: tools/conformance.sh self|cloudflare [suite options]}
shift
work=$(mktemp -d "${TMPDIR:-/tmp}/nori-sync-conformance.XXXXXX")
log="$work/server.log"
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
server=
cleanup() {
  if [ -n "$server" ]; then
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
  fi
  if [ -n "${NORI_SYNC_KEEP_LOG:-}" ]; then cp "$log" "$NORI_SYNC_KEEP_LOG"; fi
  rm -rf "$work"
}
trap cleanup EXIT

# The limits the checks are written for; tools/../cloudflare/wrangler.toml's [env.test] holds the same.
export NORI_SYNC_FRAMES_PER_MINUTE=30
export NORI_SYNC_MEETING_SECONDS=4
export NORI_SYNC_MEETING_CLOSED_SECONDS=4
export NORI_SYNC_KEPT_SECONDS=3
export NORI_SYNC_ROOMS_PER_MINUTE=100
export NORI_SYNC_MAX_MEMBERS=5
export NORI_SYNC_DEBUG_EVENTS=1

cargo build -j4 --release -p nori-sync-conformance -p nori-sync-server
suite=(target/release/nori-sync-conformance "ws://127.0.0.1:$port" --events-log "$log" --kept-seconds "$NORI_SYNC_KEPT_SECONDS")
case "$shell" in
  self)
    NORI_SYNC_BIND=127.0.0.1 NORI_SYNC_PORT=$port NORI_SYNC_DATA="$work/data" target/release/nori-sync-server >"$log" 2>&1 &
    server=$!
    ;;
  cloudflare)
    (cd cloudflare && npm ci --silent --no-audit --no-fund)
    (cd cloudflare && exec npx wrangler dev --env test --local --ip 127.0.0.1 --port "$port" --show-interactive-dev-session=false) >"$log" 2>&1 &
    server=$!
    suite+=(--expect-unload)
    ;;
  *) echo "unknown shell $shell" >&2; exit 2 ;;
esac

for _ in $(seq 1 1800); do
  if curl -fsS "http://127.0.0.1:$port/" >/dev/null 2>&1; then break; fi
  if ! kill -0 "$server" 2>/dev/null; then cat "$log" >&2; echo "the server did not start" >&2; exit 1; fi
  sleep 0.1
done
"${suite[@]}" "$@"
