#!/usr/bin/env bash
# Build sauron, serve the board, and open it in a browser.
#
# The browser is where the agents run now -- `serve` opens a pty per agent and
# the page is the terminal on the end of it. Closing the tab does not stop them;
# Ctrl-C here does.
#
#   ./run.sh                    watch the repo you are standing in
#   ./run.sh /path/to/repo      watch a specific one
#   ./run.sh --port 8080        bind somewhere particular
#   ./run.sh --agents 0         serve without reopening in-flight sessions
#   ./run.sh --no-open          serve without launching a browser
#   ./run.sh --tui              the terminal front end instead, no server
#
# Anything else is passed through to sauron, so `--codex`, `--bind`, and the
# rest still work.
#
# WHY THIS WAITS BEFORE IT OPENS
# ------------------------------
# `open` on a port nothing is listening to yet lands on a connection-refused
# page, and the browser will not retry it. So the script polls until the server
# actually answers, and only then hands the URL over. The alternative -- a sleep
# long enough to usually work -- fails on a cold cargo build and looks like a
# bug in sauron rather than in the launcher.
#
# WHY THE PORT MAY NOT BE THE ONE YOU ASKED FOR
# ---------------------------------------------
# sauron is normally run several at a time, one per repo, so a busy default port
# is the common case and not an error. With no explicit --port, the script walks
# upward from the default until it finds a free one and prints which it took. An
# explicit --port is a request, not a hint, and a clash there is reported rather
# than worked around.

set -euo pipefail

DEFAULT_PORT=7373
PORT=""
OPEN=1
TUI=0
REPO=""
PASS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --port)     PORT="$2"; shift 2 ;;
    --port=*)   PORT="${1#*=}"; shift ;;
    --no-open)  OPEN=0; shift ;;
    --tui)      TUI=1; shift ;;
    -h|--help)  sed -n '2,12p' "$0" | sed 's|^# \{0,1\}||'; exit 0 ;;
    --*)        PASS+=("$1"); shift ;;
    *)          REPO="$1"; shift ;;
  esac
done

# Where the script lives, so it can be run from anywhere; and where it was run
# from, which is the repo the user means unless they named one.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FROM="$PWD"
BIN="$HERE/sauron/target/release/sauron"

echo "building…" >&2
cargo build --release --manifest-path "$HERE/sauron/Cargo.toml" >&2

cd "$FROM"
[ -n "$REPO" ] && PASS+=("$REPO")

if [ "$TUI" = 1 ]; then
  exec "$BIN" "${PASS[@]+"${PASS[@]}"}"
fi

# Free-port probe. bash's /dev/tcp needs no nc, no lsof, and no python.
free() {
  ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

EXPLICIT=1
if [ -z "$PORT" ]; then
  EXPLICIT=0
  PORT=$DEFAULT_PORT
  while ! free "$PORT"; do
    PORT=$((PORT + 1))
    if [ "$PORT" -gt $((DEFAULT_PORT + 20)) ]; then
      echo "run.sh: no free port in ${DEFAULT_PORT}..$((DEFAULT_PORT + 20))" >&2
      exit 1
    fi
  done
  [ "$PORT" != "$DEFAULT_PORT" ] && echo "run.sh: $DEFAULT_PORT was busy, taking $PORT" >&2
elif ! free "$PORT"; then
  # Asked for, not available. Say so here rather than letting sauron fail its
  # bind, because the person who typed a port wants to know it was taken.
  echo "run.sh: port $PORT is already in use" >&2
  exit 1
fi

"$BIN" serve --port "$PORT" "${PASS[@]+"${PASS[@]}"}" &
SERVER=$!
# Ctrl-C, or the terminal closing, takes the server with it. Without this a
# backgrounded sauron would outlive the script and hold the port.
trap 'kill "$SERVER" 2>/dev/null || true' EXIT INT TERM

URL="http://127.0.0.1:$PORT"

# Poll rather than sleep. Two seconds of tries at 100ms, which is far longer
# than a bind takes and short enough that a server that died on startup is
# reported promptly instead of hanging the launcher.
ready=0
for _ in $(seq 1 20); do
  if ! kill -0 "$SERVER" 2>/dev/null; then
    wait "$SERVER" || true
    echo "run.sh: sauron exited before it was listening" >&2
    exit 1
  fi
  if ! free "$PORT"; then ready=1; break; fi
  sleep 0.1
done

if [ "$ready" != 1 ]; then
  echo "run.sh: gave up waiting for $URL" >&2
  exit 1
fi

if [ "$OPEN" = 1 ]; then
  # No portable "open a URL", so try each host's own in turn and shrug if none
  # of them is there -- the URL is printed either way, and a launcher that
  # aborts because it could not find a browser has thrown away a working server.
  if command -v open        >/dev/null 2>&1; then open "$URL"
  elif command -v xdg-open  >/dev/null 2>&1; then xdg-open "$URL" >/dev/null 2>&1
  elif command -v wslview   >/dev/null 2>&1; then wslview "$URL"
  elif command -v powershell.exe >/dev/null 2>&1; then powershell.exe -NoProfile -Command "Start-Process '$URL'"
  else echo "run.sh: no browser launcher found — open $URL yourself" >&2
  fi
fi

echo "run.sh: $URL — Ctrl-C here stops sauron and every agent it opened" >&2
wait "$SERVER"
