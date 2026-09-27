#!/usr/bin/env bash
# Builds the release tarball and its SHA256SUMS on the DGX Spark (aarch64). It never uploads or
# pushes anything: at the end it prints the `gh release create` command for me to run.
#
#   packaging/make-release.sh                  # build --profile dist, stage, pack into dist/
#   packaging/make-release.sh --binary PATH    # package an existing binary (tests, re-packs)
#
# Output (dist/ by default):
#   spark-pearl-miner-<version>-linux-aarch64.tar.gz
#     spark-pearl-miner-<version>-linux-aarch64/
#       bin/spark-pearl-miner   install.sh   VERSION   LICENSE   NOTICE   README.md   README.pt-BR.md
#       packaging/{install-clockcap.sh, uninstall-clockcap.sh, spark-pearl-miner.desktop,
#                  spark-pearl-miner.svg, systemd/user/spark-pearl-miner.service,
#                  systemd/system/spark-pearl-clockcap.service}
#       docs/{en,pt-BR}/MANUAL.md, docs/images/   (when present)
#   SHA256SUMS
# The archive is deterministic for a given binary: sorted names, root owner, mtime of the commit.
set -euo pipefail

APP=spark-pearl-miner
REPO_SLUG=xXGuilasXx/spark-pearl-miner

usage() {
  cat <<'EOF'
usage: make-release.sh [--out DIR] [--binary PATH] [--allow-dirty]
  --out DIR       where to write the tarball and SHA256SUMS (default: dist/ in the repository)
  --binary PATH   package this binary instead of building one (it must report the same version)
  --allow-dirty   package a tree with uncommitted changes (the commit gets a -dirty suffix)
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
OUT="$ROOT/dist"
BINARY=""
ALLOW_DIRTY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --out) shift; OUT=${1:-} ;;
    --out=*) OUT=${1#*=} ;;
    --binary) shift; BINARY=${1:-} ;;
    --binary=*) BINARY=${1#*=} ;;
    --allow-dirty) ALLOW_DIRTY=1 ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
  shift
done
[ -n "$OUT" ] || die "--out needs a directory"

cd "$ROOT"
VERSION=$(awk '
  /^\[workspace\.package\]/ { inpkg = 1; next }
  /^\[/ { inpkg = 0 }
  inpkg && $1 == "version" { gsub(/"/, "", $3); print $3; exit }
' Cargo.toml)
[ -n "$VERSION" ] || die "cannot read [workspace.package] version from Cargo.toml"

COMMIT=$(git rev-parse --short=12 HEAD)
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  [ "$ALLOW_DIRTY" -eq 1 ] || die "the tree has uncommitted changes; commit them or pass --allow-dirty"
  COMMIT="$COMMIT-dirty"
fi
# Deterministic archive timestamps: the commit time.
EPOCH=$(git log -1 --format=%ct)

NAME="$APP-$VERSION-linux-aarch64"
TARBALL="$NAME.tar.gz"

if [ -z "$BINARY" ]; then
  [ "$(uname -m)" = aarch64 ] || die "release binaries are built on the DGX Spark (aarch64)"
  NVCC=${NVCC:-}
  if [ -z "$NVCC" ]; then
    if command -v nvcc >/dev/null 2>&1; then NVCC=$(command -v nvcc); else NVCC=/usr/local/cuda/bin/nvcc; fi
  fi
  [ -x "$NVCC" ] || die "nvcc not found (set NVCC)"
  CUDA_HOME=${CUDA_HOME:-$(dirname "$(dirname "$(readlink -f "$NVCC")")")}
  TARGET=${CARGO_TARGET_DIR:-$HOME/.cache/$APP/target}
  case "$TARGET" in
    *[[:space:]]*) die "CARGO_TARGET_DIR must not contain spaces (jemalloc refuses them): $TARGET" ;;
  esac
  if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi
  echo "==> cargo build --locked --profile dist -p $APP ($VERSION, commit $COMMIT)"
  env NVCC="$NVCC" CUDA_HOME="$CUDA_HOME" PATH="$(dirname "$NVCC"):$PATH" SPM_GIT_COMMIT="$COMMIT" \
    CARGO_TARGET_DIR="$TARGET" cargo build --locked --profile dist -p "$APP"
  BINARY="$TARGET/dist/$APP"
fi
[ -x "$BINARY" ] || die "no executable at $BINARY"

REPORTED=$("$BINARY" --version | awk 'NR == 1')
case "$REPORTED" in
  "$APP $VERSION "*) ;;
  *) die "the binary reports \"$REPORTED\", expected version $VERSION" ;;
esac
# Captured first: `ldd | grep -q` can fail under pipefail when grep exits early (SIGPIPE).
if command -v ldd >/dev/null 2>&1 && grep -q 'not found' <<<"$(ldd "$BINARY" 2>/dev/null || true)"; then
  ldd "$BINARY" >&2
  die "the binary has unresolved libraries"
fi

STAGE=$(mktemp -d "${TMPDIR:-/tmp}/spm-release.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
D="$STAGE/$NAME"
echo "==> staging $NAME"
install -Dm0755 "$BINARY" "$D/bin/$APP"
install -Dm0755 packaging/install.sh "$D/install.sh"
install -Dm0755 packaging/install-clockcap.sh "$D/packaging/install-clockcap.sh"
install -Dm0755 packaging/uninstall-clockcap.sh "$D/packaging/uninstall-clockcap.sh"
install -Dm0644 packaging/spark-pearl-miner.desktop "$D/packaging/$APP.desktop"
install -Dm0644 webui/favicon.svg "$D/packaging/$APP.svg"
install -Dm0644 packaging/systemd/user/spark-pearl-miner.service "$D/packaging/systemd/user/spark-pearl-miner.service"
install -Dm0644 packaging/systemd/system/spark-pearl-clockcap.service "$D/packaging/systemd/system/spark-pearl-clockcap.service"
for f in LICENSE NOTICE README.md README.pt-BR.md; do
  install -Dm0644 "$f" "$D/$f"
done
printf '%s\n' "$VERSION" >"$D/VERSION"
for f in docs/en/MANUAL.md docs/pt-BR/MANUAL.md; do
  if [ -f "$f" ]; then install -Dm0644 "$f" "$D/$f"; else echo "warning: $f is missing; the tarball ships without it" >&2; fi
done
if [ -d docs/images ]; then
  mkdir -p "$D/docs/images"
  cp -R docs/images/. "$D/docs/images/"
else
  echo "warning: docs/images/ is missing; the manual's screenshots are not in the tarball" >&2
fi

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
echo "==> packing $OUT/$TARBALL"
find "$D" -exec touch -h -d "@$EPOCH" {} +
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$EPOCH" --format=gnu \
  -C "$STAGE" -cf - "$NAME" | gzip -9 -n >"$OUT/$TARBALL.tmp"
mv -f "$OUT/$TARBALL.tmp" "$OUT/$TARBALL"
(cd "$OUT" && sha256sum "$TARBALL" >SHA256SUMS)

echo
cat "$OUT/SHA256SUMS"
ls -l "$OUT/$TARBALL"
PRE=""
case "$VERSION" in *-*) PRE=" --prerelease" ;; esac
cat <<EOF

Nothing was uploaded. Check the tarball on a clean account (tar tzf, then ./install.sh), then:

  gh release create v$VERSION \\
    "$OUT/$TARBALL" "$OUT/SHA256SUMS" \\
    --repo $REPO_SLUG --target $(git rev-parse HEAD) \\
    --title "spark-pearl-miner $VERSION"$PRE --generate-notes
EOF
