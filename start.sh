#!/usr/bin/env bash
#
# context-keeper launcher — ports come ONLY from the shared port registry
# (service at 127.0.0.1:11999). No hardcoded ports, no fallback ranges.
#
#   slot 0 = backend  (ck daemon, HTTP/WS API)
#   slot 1 = frontend (Vite mind-map UI, proxies /v1 -> daemon)
#
# Idempotent: running it again safely restarts both services. Ctrl-C
# (or ./stop.sh) tears them down cleanly.
set -euo pipefail

APP_ID="context-keeper"
APP_DESCRIPTION="context-keeper — local cross-session memory + mind-map for Claude Code (ck daemon backend + Vite React UI)"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PID_FILE="$SCRIPT_DIR/.app.pid"
REGISTRY="http://127.0.0.1:11999"

# --- Step 1: resolve ports from the registry (create-if-missing) ----------
resolve_ports() {
  local result
  result=$(curl -sf -m 6 "$REGISTRY/v1/ensure" \
            -H "Content-Type: application/json" \
            -H "X-Selran-Local: 1" \
            -d "{\"app_id\":\"$APP_ID\",\"path\":\"$SCRIPT_DIR\",\"description\":\"$APP_DESCRIPTION\"}" 2>/dev/null) \
    || result=$(app-port-registry ensure "$APP_ID" "$SCRIPT_DIR" --description "$APP_DESCRIPTION" --json 2>/dev/null) \
    || { echo "ERROR: port registry unreachable at $REGISTRY — refusing to pick arbitrary ports" >&2; exit 1; }
  PORT_START=$(printf '%s' "$result" | python3 -c "import sys,json;print(json.load(sys.stdin)['range'][0])" 2>/dev/null || true)
  [ -n "${PORT_START:-}" ] || { echo "ERROR: could not parse a port from the registry response" >&2; exit 1; }
}
resolve_ports
BACKEND_PORT=$PORT_START
FRONTEND_PORT=$((PORT_START + 1))

# --- Step 2: kill stale instances -----------------------------------------
# Layer 1: PID file written by a previous run (trustworthy).
if [ -f "$PID_FILE" ]; then
  while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PID_FILE"
  rm -f "$PID_FILE"
fi
# Layer 2: anything sitting on OUR assigned ports is ours to reclaim.
for p in "$BACKEND_PORT" "$FRONTEND_PORT"; do
  pids=$(lsof -ti tcp:"$p" 2>/dev/null || true)
  [ -n "$pids" ] && { echo "▸ reclaiming port $p (killing: $pids)"; echo "$pids" | xargs kill 2>/dev/null || true; }
done
sleep 1

# --- Step 3-5: start services on the assigned ports -----------------------
cd "$SCRIPT_DIR"

if [ ! -x "./target/release/ck" ]; then
  echo "ERROR: ./target/release/ck not found — build it first: cargo build --release --bin ck" >&2
  exit 1
fi

# Build the web UI once if missing — the daemon serves it (dist is built at
# install; build here only for a standalone checkout).
if [ ! -d "$SCRIPT_DIR/apps/web/dist" ]; then
  echo "▸ apps/web/dist missing — building once…"
  corepack pnpm --filter ck-web build > "$SCRIPT_DIR/.web.log" 2>&1 \
    || echo "WARN: web build failed (see .web.log)" >&2
fi

# One process: the ck daemon serves BOTH the /v1 API/WS AND the built web UI
# (apps/web/dist, via CK_WEB_DIST) on $BACKEND_PORT — no separate Vite dev
# server, no proxy, no second port. The UI's /v1 calls are same-origin.
echo "▸ starting context-keeper (UI + API) → 127.0.0.1:$BACKEND_PORT"
CK_WEB_DIST="$SCRIPT_DIR/apps/web/dist" \
  ./target/release/ck daemon --bind "127.0.0.1:$BACKEND_PORT" > "$SCRIPT_DIR/.daemon.log" 2>&1 &
BACKEND_PID=$!
printf '%s\n' "$BACKEND_PID" > "$PID_FILE"

# Publish the serving port so the Launchpad's then_open_url {port} resolves to
# the one process (UI + API both on the backend port). The launcher never did
# this before, so the Launchpad couldn't resolve {port}.
python3 - "$APP_ID" "$BACKEND_PORT" <<'PY' || echo "WARN: could not publish activation port" >&2
import json, os, sys, tempfile, time
app_id, port = sys.argv[1], int(sys.argv[2])
path = os.path.expanduser("~/.selran/activation.json")
os.makedirs(os.path.dirname(path), exist_ok=True)
state = {"active_apps": [], "last_updated": None}
if os.path.exists(path):
    try:
        state = json.load(open(path)) or state
    except Exception:
        pass
ports = state.get("ports") or {}
ports[app_id] = {"backend_port": port, "frontend_port": port, "updated_at": f"@{int(time.time())}Z"}
state["ports"] = ports
fd, tmp = tempfile.mkstemp(prefix=".activation-", dir=os.path.dirname(path))
with os.fdopen(fd, "w") as f:
    json.dump(state, f, indent=2)
    f.write("\n")
os.replace(tmp, path)
PY

cat <<EOF

  context-keeper (UI + API, one process) → http://127.0.0.1:$BACKEND_PORT

  The daemon re-scans transcripts on boot — give it up to a minute to
  serve data (the UI will populate once it's ready).

  logs: .daemon.log  .web.log     stop: ./stop.sh   (or press Ctrl-C)

EOF

# --- Step 6: clean shutdown -----------------------------------------------
cleanup() {
  echo; echo "▸ shutting down…"
  kill "$BACKEND_PID" 2>/dev/null || true
  pids=$(lsof -ti tcp:"$BACKEND_PORT" 2>/dev/null || true)
  [ -n "$pids" ] && echo "$pids" | xargs kill 2>/dev/null || true
  rm -f "$PID_FILE"
}
trap cleanup INT TERM
wait
