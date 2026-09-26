#!/usr/bin/env bash
# G1 soak: run the real PearlHash kernel at the production shape for a long time with the GPU clock
# capped, logging power/clock/temperature every 10 s, to prove the box does not hard-power-off.
#
# Needs: the GPU free of other compute processes (stop the vLLM runtime first) and sudo for the
# clock cap. Everything else runs unprivileged. The bench itself refuses to start if another CUDA
# process is present.
#
# usage: bench/g1-soak.sh [--minutes 60] [--mhz 2200] [--manage-vllm]
#   --manage-vllm  runs 'sudo systemctl stop spark-vllm.service' before and 'start' after (asks sudo)
#
# Output: docs/benchmarks/g1-<UTC>-{soak.csv,chunks.csv,bench.log,summary.txt}
set -euo pipefail
cd "$(dirname "$0")/.."
MIN=60; MHZ=2200; MANAGE=0
while [ $# -gt 0 ]; do case "$1" in
  --minutes) shift; MIN=$1;; --mhz) shift; MHZ=$1;; --manage-vllm) MANAGE=1;; -h|--help) sed -n 2,12p "$0"; exit 0;;
  *) echo "unknown arg $1" >&2; exit 2;; esac; shift; done
source "$HOME/.cargo/env" 2>/dev/null || true
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/spark-pearl-miner/target}"
TS=$(date -u +%Y%m%dT%H%M%SZ); mkdir -p docs/benchmarks; P="docs/benchmarks/g1-$TS"
SECS=$((MIN*60))
if [ "$MANAGE" = 1 ]; then echo "== stopping spark-vllm.service (sudo)"; sudo systemctl stop spark-vllm.service; fi
if nvidia-smi --query-compute-apps=process_name --format=csv,noheader | grep -q .; then
  echo "GPU busy: $(nvidia-smi --query-compute-apps=process_name --format=csv,noheader | tr '\n' ' ') — stop it first (spark-modo / systemctl stop spark-vllm.service)" >&2; exit 3
fi
echo "== building bench"; cargo build --release -p spm-gpu --features gpu --example bench >/dev/null
BENCH=$(ls -t "$CARGO_TARGET_DIR"/release/examples/bench 2>/dev/null | head -1)
echo "== validating sudo (kept alive every 5 min so the cleanup never waits for a password)"; sudo -v
( while true; do sleep 300; sudo -n true 2>/dev/null || exit; done ) & KEEP=$!
echo "== locking SM clock at $MHZ MHz (sudo)"; sudo nvidia-smi -lgc 300,"$MHZ" >/dev/null
START_UP=$(cut -d' ' -f1 /proc/uptime)
cleanup() {
  set +e
  [ -n "${LOGPID:-}" ] && kill "$LOGPID" 2>/dev/null; [ -n "${KEEP:-}" ] && kill "$KEEP" 2>/dev/null; sleep 1
  sudo nvidia-smi -rgc >/dev/null 2>&1 || true
  bench/soak-log.sh --summarize "$P-soak.csv" 2>/dev/null | tee "$P-summary.txt" || true
  END_UP=$(cut -d' ' -f1 /proc/uptime); echo "uptime start=${START_UP}s end=${END_UP}s (a smaller end than start = the machine rebooted)" | tee -a "$P-summary.txt"
  if [ "$MANAGE" = 1 ]; then echo "== starting spark-vllm.service (sudo)"; sudo systemctl start spark-vllm.service || true; fi
  echo "== files: $P-{soak.csv,chunks.csv,bench.log,summary.txt}"
}
trap cleanup EXIT INT TERM
echo "== soak logger every 10 s for $SECS s -> $P-soak.csv"; bench/soak-log.sh --interval 10 --duration $((SECS+120)) --out "$P-soak.csv" >/dev/null 2>&1 & LOGPID=$!
echo "== bench: 131072x131072x4096 for $MIN min at $MHZ MHz -> $P-bench.log"; date -u | tee "$P-bench.log"
# The bench prints its summary only when it finishes on its own; give it the full time and let it exit.
"$BENCH" --seconds "$SECS" --m 131072 --n 131072 --k 4096 --csv "$P-chunks.csv" 2>&1 | tee -a "$P-bench.log"
echo "== bench finished $(date -u)" | tee -a "$P-bench.log"
