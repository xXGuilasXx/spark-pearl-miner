#!/usr/bin/env bash
# GPU validation run for a release candidate (M13): selftest, 10 min bench, 1 h pool soak with
# the soak logger, fee-test. Writes a JSON report naming the SHA-256 of the binary under test.
#
# Skeleton: the GPU worker (M5/M6) does not exist yet, so every step prints TODO and is recorded
# as "todo". Only the environment block and the binary hash are real today.
#
# usage: tools/gpu-validate.sh [--bin PATH] [--out FILE]
#
# This script never stops vLLM, never locks clocks and does not mine by itself. The real steps
# will load the GPU and contact a pool: run them only in an announced GPU window with the clock
# cap in place (docs/en/POWER-THERMAL.md, docs/en/COEXISTENCE.md).
set -euo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)

BIN="${CARGO_TARGET_DIR:-$REPO/target}/release/spark-pearl-miner"
OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --bin) shift; BIN=${1:-} ;;
    --out) shift; OUT=${1:-} ;;
    -h | --help) echo "usage: tools/gpu-validate.sh [--bin PATH] [--out FILE]"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
if [ -z "$OUT" ]; then
  mkdir -p "$REPO/docs/benchmarks"
  OUT=$REPO/docs/benchmarks/gpu-validate-$(date -u +%Y%m%dT%H%M%SZ).json
fi

json_str() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\t'/\\t}
  printf '"%s"' "$s"
}
json_or_null() { if [ -n "$1" ]; then json_str "$1"; else printf 'null'; fi; }
smi() { timeout 5 nvidia-smi "$@" 2>/dev/null | head -n 1 || true; }

# --- environment -------------------------------------------------------------------------------
BIN_SHA=""
BIN_SIZE=""
if [ -f "$BIN" ]; then
  BIN_SHA=$(sha256sum "$BIN" | cut -d' ' -f1)
  BIN_SIZE=$(stat -c %s "$BIN")
  echo "binary: $BIN"
  echo "sha256: $BIN_SHA"
else
  echo "WARNING: binary not found at $BIN (build the release binary or pass --bin); sha256 = null" >&2
fi
GIT_COMMIT=$(git -C "$REPO" rev-parse HEAD 2>/dev/null || true)
GIT_DIRTY=false
if [ -n "$(git -C "$REPO" status --porcelain 2>/dev/null || true)" ]; then GIT_DIRTY=true; fi
GPU_NAME=$(smi --query-gpu=name --format=csv,noheader)
DRIVER=$(smi --query-gpu=driver_version --format=csv,noheader)
SM_NOW=$(smi --query-gpu=clocks.sm --format=csv,noheader,nounits)
case "$SM_NOW" in '' | *[!0-9]*) SM_NOW=null ;; esac
COMPUTE_APPS=$(timeout 5 nvidia-smi --query-compute-apps=process_name --format=csv,noheader 2>/dev/null | paste -sd ';' - || true)

# --- steps -------------------------------------------------------------------------------------
STEPS=()
# step NAME PLANNED_COMMAND NOTE
step() {
  echo "TODO [$1] needs the real GPU worker: $2"
  STEPS+=("{\"name\": $(json_str "$1"), \"status\": \"todo\", \"command\": $(json_str "$2"), \"note\": $(json_str "$3")}")
}
step selftest "$BIN selftest --json" \
  "known-answer test on the GPU + CPU canary; must pass before anything else"
step bench "$BIN bench --minutes 10 --json" \
  "credited MAC/s, SM clock, GPU W and temperatures at 2200 MHz; the Balanced governor must hold 75 +- 3 W"
step soak "bench/soak-log.sh --duration 3600 alongside 1 h of mining on HeroMiners" \
  "0 invalid shares, stale < 1 %, no gap in the soak log, no fault signature"
step fee-test "$BIN fee-test" \
  "dev-fee slice visible under the dev address, zero stale at the switches"

# --- report ------------------------------------------------------------------------------------
{
  printf '{\n'
  printf '  "report": "spark-pearl-miner gpu-validate",\n'
  printf '  "date_utc": %s,\n' "$(json_str "$(date -u +%FT%TZ)")"
  printf '  "binary": {"path": %s, "sha256": %s, "size_bytes": %s},\n' \
    "$(json_str "$BIN")" "$(json_or_null "$BIN_SHA")" "${BIN_SIZE:-null}"
  printf '  "git": {"commit": %s, "dirty": %s},\n' "$(json_or_null "$GIT_COMMIT")" "$GIT_DIRTY"
  printf '  "host": {"hostname": %s, "kernel": %s},\n' \
    "$(json_str "$(hostname)")" "$(json_str "$(uname -r)")"
  printf '  "gpu": {"name": %s, "driver": %s, "sm_clock_mhz_at_start": %s, "compute_apps_at_start": %s},\n' \
    "$(json_or_null "$GPU_NAME")" "$(json_or_null "$DRIVER")" "$SM_NOW" \
    "$(json_str "$COMPUTE_APPS")"
  printf '  "steps": [\n'
  for i in "${!STEPS[@]}"; do
    sep=","
    [ "$i" -eq $((${#STEPS[@]} - 1)) ] && sep=""
    printf '    %s%s\n' "${STEPS[$i]}" "$sep"
  done
  printf '  ],\n'
  printf '  "result": "incomplete"\n'
  printf '}\n'
} >"$OUT"
echo "report: $OUT"
