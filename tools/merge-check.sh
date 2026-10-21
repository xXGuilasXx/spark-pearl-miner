#!/bin/bash
# Pre-merge gate: the whole workspace must pass, plus the fee-consistency guard, fixture checksums
# and the SASS gate of the fused kernel (tools/check-sass.py on the libspm_cuda.a the test build
# produced; skipped with a message when cuobjdump is not installed).
# Exit status is non-zero on ANY failure (pipefail), so it is safe to chain: tools/merge-check.sh && git merge ...
set -euo pipefail
cd "$(dirname "$0")/.."
source "$HOME/.cargo/env" 2>/dev/null || true
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/spark-pearl-miner/target}"
LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT
if ! cargo test --locked --release --workspace --all-features 2>&1 | tee "$LOG" | grep -E '^test result|FAILED|panicked|error(\[|:)' | sort | uniq -c; then
  echo "MERGE-CHECK: FAIL (cargo test; last 200 log lines follow)" >&2
  tail -n 200 "$LOG" >&2
  exit 1
fi
if grep -qE 'FAILED|error\[|error: could not compile' "$LOG"; then echo "MERGE-CHECK: FAIL (see above)"; rm -f "$LOG"; exit 1; fi
rm -f "$LOG"
if command -v cuobjdump >/dev/null 2>&1 || [ -x "${CUDA_HOME:-/usr/local/cuda}/bin/cuobjdump" ]; then
  python3 tools/check-sass.py
else
  echo "SASS gate: SKIPPED (cuobjdump not found; install the CUDA toolkit to run tools/check-sass.py)"
fi
python3 tools/check-fee-consistency.py
( cd tests/fixtures && sha256sum -c --quiet SHA256SUMS )
if ! cargo clippy --locked --release --workspace --all-features --all-targets -- -D warnings 2>&1 | tee "$LOG" | tail -n 1; then
  echo "MERGE-CHECK: FAIL (Clippy; last 200 log lines follow)" >&2
  tail -n 200 "$LOG" >&2
  exit 1
fi
echo "MERGE-CHECK: OK"
