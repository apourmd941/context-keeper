#!/usr/bin/env bash
#
# context-keeper shutdown — reads the app's assigned port block from the
# shared registry and kills whatever is running on it. Safe to run anytime.
set -uo pipefail

APP_ID="context-keeper"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PID_FILE="$SCRIPT_DIR/.app.pid"
REGISTRY="http://127.0.0.1:11999"

# Resolve our assigned port block (read-only get; fall back to ensure).
result=$(curl -sf -m 6 "$REGISTRY/v1/apps/$APP_ID" 2>/dev/null) \
  || result=$(curl -sf -m 6 "$REGISTRY/v1/ensure" \
                -H "Content-Type: application/json" \
                -H "X-Selran-Local: 1" \
                -d "{\"app_id\":\"$APP_ID\",\"path\":\"$SCRIPT_DIR\",\"description\":\"context-keeper\"}" 2>/dev/null) \
  || result=""
PORT_START=$(printf '%s' "$result" | python3 -c "import sys,json;print(json.load(sys.stdin)['range'][0])" 2>/dev/null || true)

# Layer 1: PID file.
if [ -f "$PID_FILE" ]; then
  while read -r pid; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done < "$PID_FILE"
  rm -f "$PID_FILE"
fi

# Layer 2: assigned ports (backend = slot 0, frontend = slot 1).
if [ -n "${PORT_START:-}" ]; then
  for p in "$PORT_START" "$((PORT_START + 1))"; do
    pids=$(lsof -ti tcp:"$p" 2>/dev/null || true)
    [ -n "$pids" ] && { echo "▸ killing pid(s) on port $p: $pids"; echo "$pids" | xargs kill 2>/dev/null || true; }
  done
else
  echo "WARN: could not resolve ports from registry; killed PID-file processes only" >&2
fi

echo "context-keeper stopped."
