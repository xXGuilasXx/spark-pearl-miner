#!/bin/bash
# MB1 — tensor-core peak ladder on the DGX Spark. Run ONLY with the GPU free (vLLM stopped).
# Usage: bench/mb1.sh [clock_mhz ...]   (default ladder: stock 2200 2000 1800). Clock locking needs sudo:
#   sudo nvidia-smi -lgc 300,<mhz>   and   sudo nvidia-smi -rgc   afterwards.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${CARGO_TARGET_DIR:-$HOME/.cache/spark-pearl-miner/target}/imma_peak
mkdir -p "$(dirname "$BIN")"
nvcc -O3 -gencode arch=compute_121a,code=sm_121a -lnvidia-ml cuda/probes/imma_peak.cu -o "$BIN"
if nvidia-smi --query-compute-apps=pid,process_name --format=csv,noheader | grep -q .; then
  echo "GPU busy (compute processes present) — stop vLLM first (spark-modo)." >&2; exit 2
fi
LADDER=("$@"); [ ${#LADDER[@]} -eq 0 ] && LADDER=(stock 2200 2000 1800)
mkdir -p docs/benchmarks; OUT=docs/benchmarks/mb1-$(date -u +%Y%m%dT%H%M%SZ).txt
{ echo "# MB1 $(date -u +%FT%TZ) driver $(nvidia-smi --query-gpu=driver_version --format=csv,noheader) binary sha256 $(sha256sum "$BIN" | cut -c1-16)"; } | tee "$OUT"
for c in "${LADDER[@]}"; do
  if [ "$c" != stock ]; then sudo nvidia-smi -lgc 300,"$c" >/dev/null; else sudo nvidia-smi -rgc >/dev/null; fi
  echo "## clock=$c" | tee -a "$OUT"; "$BIN" 4 | tee -a "$OUT"
done
sudo nvidia-smi -rgc >/dev/null; echo "saved: $OUT"
