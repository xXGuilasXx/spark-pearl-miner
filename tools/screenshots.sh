#!/bin/bash
# Regenerates the manual's screenshots, docs/images/{en,pt-BR}/*.png, from a SIMULATED miner:
#
#   tools/screenshots.sh [--out DIR] [--lang en|pt-BR] [--only NAME]
#
# Safe on a machine whose real miner is running: everything lives in a throwaway directory under
# /tmp (config, state, sockets), on free ports (never 4078), with worker.simulate = true and mock
# pools on localhost (the failover_demo example). It never touches ~/.config/spark-pearl-miner,
# never starts a GPU worker and only kills the processes it started.
#
# States: the setup wizard (a daemon with an empty wallet), the login screen (a daemon with
# api.trust_local_user = false), and the dashboard, the Settings dialog, the failover line and the
# alerts (failover_demo: pool 1 refuses connections at first, then comes back), plus "no pool
# reachable" (failover_demo --all-down). The display flags ?lang= and ?shot= pick the language and
# open a dialog or a section; ?shot= only works on a simulated miner and never saves anything.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT="$ROOT/docs/images"
LANGS=(en pt-BR)
ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT=$2; shift 2 ;;
    --lang) LANGS=("$2"); shift 2 ;;
    --only) ONLY=$2; shift 2 ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
case "$OUT" in /*) ;; *) OUT="$ROOT/$OUT" ;; esac

# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/spark-pearl-miner/target}"
export NVCC="${NVCC:-/usr/local/cuda/bin/nvcc}" CUDA_HOME="${CUDA_HOME:-/usr/local/cuda}"
unset SPM_GPU_TESTS

BROWSER=""
for b in google-chrome chromium chromium-browser; do
  if command -v "$b" >/dev/null 2>&1; then BROWSER=$b; break; fi
done
FIREFOX=$(command -v firefox || true)
if [ -z "$BROWSER" ] && [ -z "$FIREFOX" ]; then echo "needs google-chrome, chromium or firefox" >&2; exit 1; fi

T=$(mktemp -d /tmp/spm-shots.XXXX)
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
  rm -rf "$T"
}
trap cleanup EXIT

free_port() {
  python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'
}

echo "building (release)…"
cargo build --release -q -p spark-pearl-miner --bin spark-pearl-miner --example failover_demo
# Private copies: a concurrent build in the shared target directory cannot swap them mid-run.
mkdir -p "$T/bin/examples"
cp "$CARGO_TARGET_DIR/release/spark-pearl-miner" "$T/bin/"
cp "$CARGO_TARGET_DIR/release/examples/failover_demo" "$T/bin/examples/"

wait_http() { # port
  for _ in $(seq 1 100); do
    # Any HTTP answer will do (the login daemon answers 401 to a client without a token).
    [ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$1/api/v1/about" 2>/dev/null)" != 000 ] && return 0
    sleep 0.2
  done
  echo "nothing answers on port $1" >&2
  return 1
}

# A daemon of its own: HOME and every XDG directory under $T/$1 (the control socket too).
daemon() { # name port miner-section [api-extra]
  local d="$T/$1"
  mkdir -p "$d/config/spark-pearl-miner" "$d/state" "$d/run"
  chmod 700 "$d/run"
  # One local pool on a closed port: these daemons never contact a real pool.
  printf 'schema_version = 1\n%s\n[worker]\nsimulate = true\n\n[api]\nport = %s\n%s\n\n[[pools]]\nname = "local"\nhost = "127.0.0.1"\nport = 9\ntls = "off"\n' \
    "$3" "$2" "${4:-}" > "$d/config/spark-pearl-miner/config.toml"
  HOME="$d" XDG_CONFIG_HOME="$d/config" XDG_STATE_HOME="$d/state" XDG_RUNTIME_DIR="$d/run" \
    "$T/bin/spark-pearl-miner" daemon >"$d/daemon.log" 2>&1 &
  PIDS+=($!)
  wait_http "$2"
}

demo() { # name port args…
  local name=$1 port=$2
  shift 2
  "$T/bin/examples/failover_demo" --port "$port" --dir "$T/$name" "$@" >"$T/$name.log" 2>&1 &
  PIDS+=($!)
  wait_http "$port"
}

shot() { # lang name port shot-flag
  # Every URL carries a ?shot= flag (default "page", which opens nothing): on a simulated miner
  # it makes the page poll instead of holding the event stream open, so the headless browser's
  # virtual-time budget can expire and it takes the picture once the data is on screen.
  local lang=$1 name=$2 port=$3 flag=${4:-page}
  [ -n "$ONLY" ] && [ "$ONLY" != "$name" ] && return 0
  local url="http://127.0.0.1:$port/?lang=$lang${flag:+&shot=$flag}#/dashboard"
  local file="$OUT/$lang/$name.png"
  mkdir -p "$OUT/$lang"
  if [ -n "$BROWSER" ]; then
    # A fresh profile per shot: no remembered language, no cached session.
    local prof="$T/chrome-$lang-$name"
    timeout 60 "$BROWSER" --headless=new --no-first-run --no-default-browser-check --disable-gpu \
      --user-data-dir="$prof" --lang="$lang" --virtual-time-budget=5000 \
      --window-size=1280,900 --hide-scrollbars --screenshot="$file" "$url" >/dev/null 2>&1 || true
  fi
  if [ ! -s "$file" ] && [ -n "$FIREFOX" ]; then
    timeout 60 "$FIREFOX" --headless --profile "$(mktemp -d "$T/ff.XXXX")" --window-size=1280,900 --screenshot "$file" "$url" >/dev/null 2>&1 || true
  fi
  if [ -s "$file" ]; then echo "  $lang/$name.png"; else echo "  FAILED: $lang/$name.png" >&2; fi
}

# Log in to a daemon with its token (for the few states the script drives through the API).
api_post() { # port token path
  local jar="$T/jar-$1"
  local csrf
  csrf=$(curl -fs -c "$jar" -H 'Content-Type: application/json' -d "{\"token\":\"$2\"}" "http://127.0.0.1:$1/api/v1/session" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["csrf"])')
  curl -fs -b "$jar" -H "X-SPM-CSRF: $csrf" -H 'Content-Type: application/json' -d '{}' "http://127.0.0.1:$1$3" >/dev/null
}

token_of() { # demo name
  grep -m1 -o 'token=[0-9a-f]*' "$T/$1.log" | cut -d= -f2
}

wait_status() { # port python-expression-on-s (the status JSON)
  for _ in $(seq 1 150); do
    if curl -fs "http://127.0.0.1:$1/api/v1/status" 2>/dev/null | python3 -c "import sys,json;s=json.load(sys.stdin);sys.exit(0 if ($2) else 1)" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  echo "timed out waiting for: $2" >&2
  return 1
}

P=$(free_port); [ "$P" != 4078 ]
echo "setup wizard (port $P)"
daemon wiz "$P" '[miner]
wallet = ""'
for l in "${LANGS[@]}"; do
  shot "$l" wizard-1-welcome "$P"
  shot "$l" wizard-2-wallet "$P" wizard-2
  shot "$l" wizard-2-wallet-error "$P" wallet-error
  shot "$l" wizard-3-fee "$P" wizard-3
  shot "$l" wizard-3-fee-presets "$P" wizard-3-presets
done

P=$(free_port); [ "$P" != 4078 ]
echo "login screen (port $P, api.trust_local_user = false)"
daemon login "$P" '[miner]
wallet = "prl1pg69hxg0gx3dhlqj0nvxt4w833px6vmx6v45esqw8vayn7ky8jxjswf035d"
disclosure_accepted = true' 'trust_local_user = false'
for l in "${LANGS[@]}"; do shot "$l" login "$P"; done

P=$(free_port); [ "$P" != 4078 ]
echo "failover demo (port $P): pool 1 down for 45 s, then back"
demo fo "$P" --pool1-down-s 45
TOKEN=$(token_of fo)
wait_status "$P" 's.get("active_pool") == 2 and s["shares"]["accepted"] >= 2'
for l in "${LANGS[@]}"; do
  shot "$l" dashboard-failover "$P"
  shot "$l" dashboard-failover-timeline "$P" timeline
  shot "$l" settings "$P" settings
  shot "$l" settings-error "$P" settings-error
  shot "$l" settings-wallet-confirm "$P" settings-wallet-confirm
  shot "$l" dashboard-stop-confirm "$P" stop-confirm
done
# An invalid hand edit of config.toml: the miner keeps the previous settings and raises an alert.
CFG="$T/fo/config/config.toml"
cp "$CFG" "$T/fo.config.good"
sed -i 's/^worker = "demo"$/worker = "not a valid name"/' "$CFG"
wait_status "$P" 'any("edited but is invalid" in a["msg"] for a in s["alerts"])'
cp "$T/fo.config.good" "$CFG"
for l in "${LANGS[@]}"; do shot "$l" dashboard-alerts "$P" alerts; done
echo "waiting for the return to pool 1 (probe + 10 s stable)…"
wait_status "$P" 's.get("active_pool") == 1 and s["state"] == "mining"'
sleep 3
for l in "${LANGS[@]}"; do shot "$l" dashboard-mining "$P"; done
api_post "$P" "$TOKEN" /api/v1/mining/stop
wait_status "$P" 'not s["running"]'
for l in "${LANGS[@]}"; do shot "$l" dashboard-stopped "$P"; done

P=$(free_port); [ "$P" != 4078 ]
echo "no pool reachable (port $P)"
demo down "$P" --all-down
wait_status "$P" 's["state"] == "all_down"'
sleep 2
for l in "${LANGS[@]}"; do shot "$l" dashboard-all-down "$P"; done

echo "screenshots in $OUT"
