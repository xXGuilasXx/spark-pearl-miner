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
# Keep trust scoped to this checkout and this process. Reading a Docker Desktop
# bind mount as the smoke account can otherwise leave cached ownership that Git
# rejects in the parent process, even when the smoke never writes the index.
ci_git_config_count=${GIT_CONFIG_COUNT:-0}
case "$ci_git_config_count" in
  *[!0-9]* | '') echo 'error: GIT_CONFIG_COUNT must be a non-negative integer' >&2; exit 1 ;;
esac
ci_git_config_count=$((10#$ci_git_config_count))
export "GIT_CONFIG_KEY_$ci_git_config_count=safe.directory" "GIT_CONFIG_VALUE_$ci_git_config_count=$ROOT"
export GIT_CONFIG_COUNT=$((ci_git_config_count + 1))
unset ci_git_config_count

installer_smoke() {
  if [ "$(id -u)" -eq 0 ]; then
    # CUDA's build container is root, but the production installer must stay a
    # per-user operation. The smoke account receives no credentials or session.
    runuser -u nobody -- env -i \
      PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
      HOME=/nonexistent USER=nobody LOGNAME=nobody \
      LD_LIBRARY_PATH="${CUDA_HOME:-/usr/local/cuda}/lib64" \
      GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0="$ROOT" \
      GIT_OPTIONAL_LOCKS=0 \
      SPM_GPU_TESTS=0 SPM_NVML_TESTS=0 CUDA_VISIBLE_DEVICES=-1 \
      bash "$ROOT/tools/test-install.sh" "$@"
  else
    bash "$ROOT/tools/test-install.sh" "$@"
  fi
}

case "${1:-}" in
  cpu)
    cargo test --locked --workspace --exclude spm-gpu --exclude spm-worker --exclude spark-pearl-miner
    cargo clippy --locked --workspace --exclude spm-gpu --exclude spm-worker --exclude spark-pearl-miner --all-targets -- -D warnings
    python3 tools/check-fee-consistency.py
    (cd tests/fixtures && sha256sum -c --quiet SHA256SUMS)
    python3 -B tools/test-publish-release.py
    python3 -B tools/test-bump-version.py
    installer_smoke
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
    installer_smoke
    bash packaging/make-release.sh
    installer_smoke --real --binary "$CARGO_TARGET_DIR/dist/spark-pearl-miner"
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
