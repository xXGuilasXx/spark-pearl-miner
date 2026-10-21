#!/usr/bin/env bash
# CI host checks. CUDA is compiled for GB10 in full mode; no GPU is accessed and no pool is contacted.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
if [ -f "$HOME/.cargo/env" ]; then
  source "$HOME/.cargo/env"
fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export SPM_GPU_TESTS=0 SPM_NVML_TESTS=0 CUDA_VISIBLE_DEVICES=-1

case "${1:-}" in
  cpu)
    cargo test --locked --workspace --exclude spm-gpu --exclude spm-worker --exclude spark-pearl-miner
    cargo clippy --locked --workspace --exclude spm-gpu --exclude spm-worker --exclude spark-pearl-miner --all-targets -- -D warnings
    python3 tools/check-fee-consistency.py
    (cd tests/fixtures && sha256sum -c --quiet SHA256SUMS)
    python3 -B tools/test-publish-release.py
    python3 -B tools/test-bump-version.py
    bash tools/test-install.sh
    ;;
  full)
    # Fail instead of silently skipping CUTLASS layout or SASS validation.
    test -f third_party/cutlass/include/cute/atom/mma_traits_sm80.hpp || {
      echo 'error: initialize the pinned CUTLASS submodule before the full gate' >&2
      exit 1
    }
    command -v "${NVCC:-nvcc}" >/dev/null 2>&1 || {
      echo 'error: CUDA 13 nvcc is required for the full gate' >&2
      exit 1
    }
    if ! command -v cuobjdump >/dev/null 2>&1 && [ ! -x "${CUDA_HOME:-/usr/local/cuda}/bin/cuobjdump" ]; then
      echo 'error: cuobjdump is required for the full SASS gate' >&2
      exit 1
    fi
    bash tools/merge-check.sh
    python3 -B tools/test-publish-release.py
    python3 -B tools/test-bump-version.py
    bash tools/test-install.sh
    bash tools/test-install.sh --real
    bash packaging/make-release.sh
    python3 - <<'PY'
import json
from pathlib import Path
import subprocess
import tomllib

version = tomllib.loads(Path("Cargo.toml").read_text())["workspace"]["package"]["version"]
metadata = {
    "schema_version": 1,
    "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "version": version,
    "target": "linux-aarch64",
    "gpu_validation": "not-run",
}
dist = Path("dist")
expected = {f"spark-pearl-miner-{version}-linux-aarch64.tar.gz", "SHA256SUMS", "RELEASE-METADATA.json"}
unexpected = {p.name for p in dist.iterdir()} - expected
if unexpected:
    raise SystemExit(f"unexpected files in dist/: {sorted(unexpected)}")
(dist / "RELEASE-METADATA.json").write_text(json.dumps(metadata, indent=2) + "\n")
PY
    (cd dist && sha256sum -c --strict SHA256SUMS)
    echo 'CI full: host tests, CUDA compilation, SASS and installer checks passed; GB10 GPU validation NOT RUN.'
    ;;
  *)
    echo 'usage: tools/ci-check.sh cpu|full' >&2
    exit 2
    ;;
esac
