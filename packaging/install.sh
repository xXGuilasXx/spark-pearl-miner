#!/usr/bin/env bash
# spark-pearl-miner installer for the NVIDIA DGX Spark (GB10). One command, as your normal user:
#
#   curl -fsSL https://raw.githubusercontent.com/xXGuilasXx/spark-pearl-miner/main/packaging/install.sh | bash
#   ./packaging/install.sh                 # from a clone
#   ./install.sh                           # from an extracted release tarball
#
# What it does (idempotent; running it again repairs or upgrades):
#   1. checks the machine (aarch64, GB10, driver, CUDA 13 runtime);
#   2. gets the program: the release tarball next to this script, else the latest GitHub release
#      for aarch64 (verified against its SHA256SUMS), else builds from source (installs rustup if
#      needed, finds nvcc, builds with CARGO_TARGET_DIR outside paths with spaces);
#   3. installs ~/.local/bin/spark-pearl-miner, the systemd --user unit, the app-menu entry and
#      ~/.local/share/spark-pearl-miner/ (clock-cap scripts, this installer, docs);
#   4. asks, once each and default Yes, for the two optional root steps: the 2000 MHz GPU clock
#      cap (recommended safety net) and lingering (mining after logout and at boot); sudo asks
#      for your password. --yes accepts both, --no-sudo skips both, and without a terminal to
#      ask they are skipped; a skipped step is printed in the summary for later;
#   5. starts the service and prints the GUI address http://127.0.0.1:4078/.
# It never creates, rewrites or deletes ~/.config/spark-pearl-miner/config.toml or the API token
# (the daemon writes the commented Spark defaults on its first start), runs sudo only for the two
# steps above after you say yes, and never touches the developer fee.
#
# The service always runs, but a fresh install does not mine: open the GUI, paste your wallet,
# accept the 2 % developer fee and press Start mining. Stop is remembered across reboots.
set -euo pipefail

REPO_SLUG="xXGuilasXx/spark-pearl-miner"
REPO_URL="https://github.com/$REPO_SLUG"
RAW_INSTALL="https://raw.githubusercontent.com/$REPO_SLUG/main/packaging/install.sh"
APP="spark-pearl-miner"
ASSET_SUFFIX="-linux-aarch64.tar.gz"
UNIT="spark-pearl-miner.service"
CLOCKCAP_UNIT="spark-pearl-clockcap.service"

usage() {
  cat <<EOF
usage: install.sh [options]

  (no option)      install, or repair/upgrade an existing install
  --upgrade        same, but says so: fetches the newest release (or rebuilds from source) and
                   restarts the service; settings, wallet and token are kept
  --uninstall      stop and remove the program, the unit, the menu entry and the share dir;
                   settings (~/.config/$APP) and state are kept unless --purge
  --purge          with --uninstall: also delete the settings (wallet, API token) and state
  --rollback       go back to the previous binary kept by the last upgrade

  --from-source    build from source instead of downloading a release (from this clone when the
                   script runs from one, else from a clone in ~/.cache/$APP/src)
  --tarball FILE   install from a local release tarball (checked against a SHA256SUMS next to it)
  --release TAG    install this release tag (default: the newest one, pre-releases included)
  --prefix DIR     install under DIR (default ~/.local: DIR/bin, DIR/share/$APP)
  --no-start       do not enable, start or restart the service (no systemctl, no loginctl)
  --no-linger      do not try to enable lingering (mining after logout/reboot without login)
  --no-sudo        never ask for sudo: skip the optional clock cap and lingering steps (their
                   commands are printed in the summary)
  --no-open        do not open the browser at the end
  --dry-run        print what would be done and change nothing (never asks, never runs sudo)
  --yes            answer yes to every question: the clock cap, lingering and the --purge
                   confirmation
  --force          continue when a machine check fails (not the root check)
  -h, --help       this help

From a pipe, pass options after "bash -s --":
  curl -fsSL $RAW_INSTALL | bash -s -- --upgrade
EOF
}

# ---------------------------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------------------------
if [ -t 1 ]; then
  B=$'\e[1m' G=$'\e[32m' Y=$'\e[33m' R=$'\e[31m' N=$'\e[0m'
else
  B="" G="" Y="" R="" N=""
fi
say() { printf '%s\n' "$*"; }
step() { printf '\n%s==> %s%s\n' "$B" "$*" "$N"; }
ok() { printf '  %s✓%s %s\n' "$G" "$N" "$*"; }
warn() { printf '  %s!%s %s\n' "$Y" "$N" "$*" >&2; }
die() {
  printf '%serror:%s %s\n' "$R" "$N" "$*" >&2
  exit 1
}

# One command as it would be typed (single-quoted where needed).
SQ="'"
quote() {
  local arg out=""
  for arg in "$@"; do
    case "$arg" in
      "" | *[!A-Za-z0-9_./=:,@%+~-]*) out+=" '${arg//$SQ/$SQ\\$SQ$SQ}'" ;;
      *) out+=" $arg" ;;
    esac
  done
  printf '%s' "${out# }"
}

# Every change goes through run: printed under --dry-run, executed otherwise.
run() {
  if [ "$DRY_RUN" -eq 1 ]; then
    printf '  [dry-run] %s\n' "$(quote "$@")"
  else
    "$@"
  fi
}

# Writes stdin to a file (0644) through run.
write_file() {
  local dst=$1 tmp
  tmp=$(mktemp "$TMP/write.XXXXXX")
  cat >"$tmp"
  run install -Dm0644 "$tmp" "$dst"
}

have() { command -v "$1" >/dev/null 2>&1; }

# Whether there is a terminal to ask on (/dev/tty, not stdin, which is the script under curl|bash).
have_tty() {
  [ -r /dev/tty ] && { : </dev/tty; } 2>/dev/null
}

# Reads a yes/no answer from the terminal (never from stdin, which is the script under curl|bash).
tty_read() {
  local prompt=$1 answer=""
  if have_tty; then
    printf '%s' "$prompt" >/dev/tty
    IFS= read -r answer </dev/tty || true
  fi
  printf '%s' "$answer"
}

# ---------------------------------------------------------------------------------------------
# Options and paths
# ---------------------------------------------------------------------------------------------
MODE=install
PURGE=0
FROM_SOURCE=0
TARBALL=""
RELEASE_TAG=""
PREFIX=""
START=1
LINGER=1
SUDO=1
OPEN=1
DRY_RUN=0
YES=0
FORCE=0

parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --upgrade) MODE=upgrade ;;
      --uninstall) MODE=uninstall ;;
      --rollback) MODE=rollback ;;
      --purge) PURGE=1 ;;
      --from-source) FROM_SOURCE=1 ;;
      --tarball) shift; TARBALL=${1:-} ;;
      --tarball=*) TARBALL=${1#*=} ;;
      --release) shift; RELEASE_TAG=${1:-} ;;
      --release=*) RELEASE_TAG=${1#*=} ;;
      --prefix) shift; PREFIX=${1:-} ;;
      --prefix=*) PREFIX=${1#*=} ;;
      --no-start) START=0 ;;
      --no-linger) LINGER=0 ;;
      --no-sudo) SUDO=0 ;;
      --no-open) OPEN=0 ;;
      --dry-run) DRY_RUN=1 ;;
      --yes | -y) YES=1 ;;
      --force) FORCE=1 ;;
      -h | --help) usage; exit 0 ;;
      *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
  done
  if [ "$PURGE" -eq 1 ] && [ "$MODE" != uninstall ]; then
    die "--purge only goes with --uninstall"
  fi
  if [ -n "$TARBALL" ] && [ "$FROM_SOURCE" -eq 1 ]; then
    die "--tarball and --from-source exclude each other"
  fi
}

setup_paths() {
  [ -n "${HOME:-}" ] && [ -d "$HOME" ] || die "HOME is not set to a directory"
  USER_NAME=${USER:-$(id -un)}
  PREFIX=${PREFIX:-$HOME/.local}
  case "$PREFIX" in
    /*) ;;
    *) PREFIX="$PWD/$PREFIX" ;;
  esac
  PREFIX=${PREFIX%/}
  case "$PREFIX" in
    *[[:space:]]*) die "the prefix must not contain spaces: $PREFIX" ;;
  esac
  BINDIR="$PREFIX/bin"
  BIN="$BINDIR/$APP"
  SHAREDIR="$PREFIX/share/$APP"
  local cfg=${XDG_CONFIG_HOME:-$HOME/.config} data=${XDG_DATA_HOME:-$HOME/.local/share}
  local state=${XDG_STATE_HOME:-$HOME/.local/state} cache=${XDG_CACHE_HOME:-$HOME/.cache}
  UNIT_DIR="$cfg/systemd/user"
  UNIT_FILE="$UNIT_DIR/$UNIT"
  CONFIG_DIR="$cfg/$APP"
  CONFIG_FILE="$CONFIG_DIR/config.toml"
  STATE_DIR="$state/$APP"
  APPS_DIR="$data/applications"
  DESKTOP_FILE="$APPS_DIR/$APP.desktop"
  ICON_FILE="$data/icons/hicolor/scalable/apps/$APP.svg"
  CACHE_DIR="$cache/$APP"
  PORT=$(config_port)
  GUI_URL="http://127.0.0.1:$PORT/"
}

# [api] port from an existing config.toml, else 4078.
config_port() {
  local p=""
  if [ -r "${CONFIG_FILE:-}" ]; then
    p=$(awk '
      /^[[:space:]]*\[/ { sec = $0; gsub(/[[:space:]]/, "", sec); next }
      sec == "[api]" && $1 == "port" { v = $0; sub(/^[^=]*=[[:space:]]*/, "", v); sub(/[^0-9].*$/, "", v); print v; exit }
    ' "$CONFIG_FILE" 2>/dev/null || true)
  fi
  case "$p" in
    '' | *[!0-9]*) p=4078 ;;
  esac
  printf '%s' "$p"
}

# Where this script lives, when it runs from a file (empty under curl | bash).
script_dir() {
  local src=${BASH_SOURCE[0]:-}
  if [ -n "$src" ] && [ -f "$src" ]; then
    (cd "$(dirname "$src")" && pwd)
  fi
}

# ---------------------------------------------------------------------------------------------
# Machine checks
# ---------------------------------------------------------------------------------------------
check_fail() {
  if [ "$FORCE" -eq 1 ]; then
    warn "$1 (continuing: --force)"
  else
    printf '  %s✗%s %s\n' "$R" "$N" "$1" >&2
    [ -n "${2:-}" ] && printf '    %s\n' "$2" >&2
    exit 1
  fi
}

preflight() {
  step "Checking this machine"
  local arch gpu driver major libs
  arch=$(uname -m)
  if [ "$arch" = aarch64 ]; then ok "CPU architecture: aarch64"; else
    check_fail "this is $arch; the miner is built for the NVIDIA DGX Spark (aarch64)"
  fi
  # No `cmd | head` / `cmd | grep -q` under pipefail: the reader may exit before the writer is
  # done, the writer then dies of SIGPIPE and the whole check fails on a healthy machine.
  # Capture the output first, then look at it.
  if have nvidia-smi && gpu=$(nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>/dev/null) &&
    gpu=${gpu%%$'\n'*} && [ -n "$gpu" ]; then
    driver=${gpu##*, }
    major=${driver%%.*}
    case "$gpu" in
      *GB10*) ok "GPU: ${gpu%%,*}" ;;
      *) check_fail "GPU is \"${gpu%%,*}\", not the GB10 of the DGX Spark" \
        "This build is only for the NVIDIA DGX Spark (GB10). Developers: see --from-source and --force." ;;
    esac
    case "$major" in
      '' | *[!0-9]*) check_fail "cannot read the NVIDIA driver version ($driver)" ;;
      *)
        if [ "$major" -ge 580 ]; then ok "NVIDIA driver $driver"; else
          check_fail "NVIDIA driver $driver is too old (580 or newer, as shipped with DGX OS 7)"
        fi
        ;;
    esac
  else
    check_fail "nvidia-smi does not answer: no NVIDIA GPU or driver found" \
      "The DGX Spark ships with the driver; check with: nvidia-smi"
  fi
  # `ldconfig -p` prints ~100 KB on DGX OS: more than a pipe holds (see above).
  libs=$(ldconfig -p 2>/dev/null) || libs=""
  if grep -q 'libcudart\.so\.13' <<<"$libs"; then ok "CUDA 13 runtime (libcudart.so.13)"; else
    check_fail "the CUDA 13 runtime (libcudart.so.13) is not in the loader path" \
      "DGX OS 7 ships it under /usr/local/cuda; check with: ldconfig -p | grep libcudart"
  fi
  if [ "$START" -eq 1 ] && [ "$DRY_RUN" -eq 0 ]; then
    if systemctl --user show-environment >/dev/null 2>&1; then ok "systemd user session"; else
      check_fail "no systemd user session (systemctl --user does not answer)" \
        "Log in on the desktop, or run: sudo loginctl enable-linger $USER_NAME, then log in again. Or use --no-start."
    fi
    check_port
  fi
}

# The GUI port must be free, or already ours (upgrade).
check_port() {
  local body rc=0
  body=$(curl -s -m 3 "http://127.0.0.1:$PORT/api/v1/about" 2>/dev/null) || rc=$?
  if [ "$rc" -eq 7 ]; then
    ok "port $PORT is free"
  elif grep -q "$APP" <<<"$body"; then
    ok "port $PORT: $APP is already running (it will be upgraded)"
  else
    check_fail "port $PORT is used by another program" \
      "Stop that program, or change [api] port in $CONFIG_FILE."
  fi
}

# ---------------------------------------------------------------------------------------------
# Getting the program: a directory laid out like the release tarball
#   bin/spark-pearl-miner, install.sh, VERSION, packaging/…, LICENSE, NOTICE, README*, docs/
# ---------------------------------------------------------------------------------------------
PAYLOAD=""

payload_ok() {
  [ -x "$1/bin/$APP" ] && [ -f "$1/packaging/systemd/user/$UNIT" ]
}

extract_tarball() {
  local file=$1 dest="$TMP/payload" top
  mkdir -p "$dest"
  tar -xzf "$file" -C "$dest" --no-same-owner
  top=$(find "$dest" -mindepth 1 -maxdepth 1 -type d -print -quit)
  payload_ok "$top" || die "$file does not look like a $APP release tarball"
  PAYLOAD=$top
}

# Checks FILE against the SHA256SUMS in the same directory; the file must be listed.
verify_sum() {
  local file=$1 sums=$2 name line
  name=$(basename "$file")
  line=$(grep -m1 -E "^[0-9a-f]{64} [ *]$name\$" "$sums") || true
  [ -n "$line" ] || die "$name is not listed in $(basename "$sums")"
  (cd "$(dirname "$file")" && printf '%s\n' "$line" | sha256sum -c --quiet -) >/dev/null ||
    die "SHA256 mismatch for $name: the download is corrupt or was tampered with; nothing was installed"
  ok "SHA256 verified: $name"
}

from_local_tarball() {
  [ -f "$TARBALL" ] || die "no such file: $TARBALL"
  step "Using the local tarball $TARBALL"
  local sums
  sums="$(dirname "$TARBALL")/SHA256SUMS"
  if [ -f "$sums" ]; then
    verify_sum "$TARBALL" "$sums"
  else
    warn "no SHA256SUMS next to it: not verified (you chose this file)"
  fi
  extract_tarball "$TARBALL"
}

# Prints "tag<TAB>tarball-url<TAB>sums-url" of the newest release with an aarch64 tarball.
find_release() {
  local api="https://api.github.com/repos/$REPO_SLUG/releases?per_page=20" json
  [ -n "$RELEASE_TAG" ] && api="https://api.github.com/repos/$REPO_SLUG/releases/tags/$RELEASE_TAG"
  json=$(curl -fsSL -m 20 -H 'Accept: application/vnd.github+json' "$api" 2>/dev/null) || return 1
  have python3 || return 1
  printf '%s' "$json" | python3 -c '
import json, sys
suffix = sys.argv[1]
data = json.load(sys.stdin)
for rel in (data if isinstance(data, list) else [data]):
    if rel.get("draft"):
        continue
    assets = {a.get("name", ""): a.get("browser_download_url", "") for a in rel.get("assets", [])}
    tars = [n for n in assets if n.startswith("spark-pearl-miner-") and n.endswith(suffix)]
    if tars and "SHA256SUMS" in assets:
        print("\t".join([rel.get("tag_name", "?"), assets[tars[0]], assets["SHA256SUMS"]]))
        break
' "$ASSET_SUFFIX"
}

from_release() {
  local info=$1 tag url sums_url dir
  IFS=$'\t' read -r tag url sums_url <<<"$info"
  ok "release $tag"
  dir="$TMP/download"
  mkdir -p "$dir"
  curl -fL --proto '=https' --tlsv1.2 --retry 3 -o "$dir/SHA256SUMS" "$sums_url" ||
    die "could not download SHA256SUMS of $tag"
  curl -fL --proto '=https' --tlsv1.2 --retry 3 -o "$dir/$(basename "$url")" "$url" ||
    die "could not download $(basename "$url")"
  verify_sum "$dir/$(basename "$url")" "$dir/SHA256SUMS"
  extract_tarball "$dir/$(basename "$url")"
}

ensure_cargo() {
  if ! have cargo && [ -x "${CARGO_HOME:-$HOME/.cargo}/bin/cargo" ]; then
    PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
  fi
  if ! have cargo; then
    say "  Rust is not installed: installing rustup (minimal profile, no root; the toolchain"
    say "  pinned in rust-toolchain.toml is fetched by the build)"
    run bash -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none"
    PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
  fi
  if [ "$DRY_RUN" -eq 0 ]; then
    have cargo || die "cargo is still missing after installing rustup"
  fi
}

find_nvcc() {
  NVCC=${NVCC:-}
  if [ -z "$NVCC" ]; then
    if have nvcc; then NVCC=$(command -v nvcc); elif [ -x /usr/local/cuda/bin/nvcc ]; then NVCC=/usr/local/cuda/bin/nvcc; fi
  fi
  [ -n "$NVCC" ] && [ -x "$NVCC" ] ||
    die "nvcc (CUDA 13 compiler) not found in PATH or /usr/local/cuda/bin; DGX OS ships it, or set NVCC=/path/to/nvcc"
  CUDA_HOME=${CUDA_HOME:-$(dirname "$(dirname "$(readlink -f "$NVCC")")")}
  ok "nvcc: $NVCC (CUDA_HOME=$CUDA_HOME)"
}

# The source tree: the clone this script runs from, else ~/.cache/spark-pearl-miner/src.
# The CUTLASS submodule is not fetched: the miner's kernels do not include it (only a layout test
# does), so a shallow clone without submodules builds the binary.
source_tree() {
  local here=$1
  if [ -n "$here" ] && [ -f "$here/../Cargo.toml" ] && [ -d "$here/../crates/spm" ] && [ -e "$here/../.git" ]; then
    SRC=$(cd "$here/.." && pwd)
    ok "building this clone: $SRC"
    return
  fi
  have git || die "git is needed to build from source (sudo apt install git)"
  SRC="$CACHE_DIR/src"
  if [ -d "$SRC/.git" ]; then
    ok "updating $SRC"
    run git -C "$SRC" fetch --depth 1 origin main
    run git -C "$SRC" checkout --force --detach FETCH_HEAD
  else
    ok "cloning $REPO_URL into $SRC"
    run mkdir -p "$CACHE_DIR"
    run git clone --depth 1 "$REPO_URL.git" "$SRC"
  fi
}

from_source() {
  step "Building from source"
  have cc || die "a C compiler is needed (sudo apt install build-essential)"
  find_nvcc
  ensure_cargo
  source_tree "$1"
  local target=${CARGO_TARGET_DIR:-$CACHE_DIR/target} commit="unknown"
  case "$target" in
    *[[:space:]]*) die "the build directory must not contain spaces (jemalloc refuses them): $target; set CARGO_TARGET_DIR" ;;
  esac
  if [ "$DRY_RUN" -eq 0 ]; then
    commit=$(git -C "$SRC" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
    # rustup fetches the toolchain pinned in rust-toolchain.toml.
    if have rustup; then (cd "$SRC" && { rustup toolchain install >/dev/null 2>&1 || rustup show >/dev/null; }); fi
  fi
  say "  cargo build --profile dist (about 5–15 minutes the first time)"
  # From inside the tree, so rustup honours rust-toolchain.toml; nvcc's directory goes first on
  # PATH for the CUDA build scripts.
  (
    if [ "$DRY_RUN" -eq 1 ]; then say "  [dry-run] in $SRC:"; else cd "$SRC"; fi
    PATH="$(dirname "$NVCC"):$PATH"
    run env NVCC="$NVCC" CUDA_HOME="$CUDA_HOME" SPM_GIT_COMMIT="$commit" CARGO_TARGET_DIR="$target" \
      cargo build --locked --profile dist -p "$APP"
  )
  if [ "$DRY_RUN" -eq 1 ]; then
    PAYLOAD=""
    return
  fi
  # Stage the same layout as a release tarball.
  local p="$TMP/payload/$APP-source"
  mkdir -p "$p/bin"
  install -m0755 "$target/dist/$APP" "$p/bin/$APP"
  cp -R "$SRC/packaging" "$p/packaging"
  install -m0755 "$SRC/packaging/install.sh" "$p/install.sh"
  [ -f "$SRC/webui/favicon.svg" ] && install -m0644 "$SRC/webui/favicon.svg" "$p/packaging/$APP.svg"
  for f in LICENSE NOTICE README.md README.pt-BR.md; do
    [ -f "$SRC/$f" ] && install -m0644 "$SRC/$f" "$p/$f"
  done
  # The same docs as a release tarball (packaging/make-release.sh): the manual and its images.
  for f in docs/en/MANUAL.md docs/pt-BR/MANUAL.md; do
    [ -f "$SRC/$f" ] && install -Dm0644 "$SRC/$f" "$p/$f"
  done
  if [ -d "$SRC/docs/images" ]; then mkdir -p "$p/docs/images" && cp -R "$SRC/docs/images/." "$p/docs/images/"; fi
  "$p/bin/$APP" --version | awk 'NR == 1 {print $2}' >"$p/VERSION"
  PAYLOAD=$p
}

get_payload() {
  local here
  here=$(script_dir)
  if [ -n "$TARBALL" ]; then
    from_local_tarball
  elif [ "$FROM_SOURCE" -eq 0 ] && [ -n "$here" ] && payload_ok "$here" && [ -f "$here/VERSION" ]; then
    step "Using the release files next to this script"
    PAYLOAD=$here
  elif [ "$FROM_SOURCE" -eq 1 ]; then
    from_source "$here"
  else
    step "Looking for a release on $REPO_URL/releases"
    local info
    info=$(find_release) || info=""
    if [ -n "$info" ]; then
      from_release "$info"
    else
      [ -n "$RELEASE_TAG" ] && die "release $RELEASE_TAG has no ${ASSET_SUFFIX#-} tarball with SHA256SUMS"
      say "  no release with an aarch64 tarball yet (or GitHub is unreachable): building from source"
      from_source "$here"
    fi
  fi
}

version_of() {
  "$1" --version 2>/dev/null | awk 'NR == 1 {print $2, $3, $4}' || true
}

# ---------------------------------------------------------------------------------------------
# Installing
# ---------------------------------------------------------------------------------------------
service_active() {
  [ "$START" -eq 1 ] && systemctl --user is-active --quiet "$UNIT" 2>/dev/null
}

STOPPED=0
stop_service() {
  if [ "$STOPPED" -eq 0 ] && service_active; then
    say "  stopping the service (your Start/Stop choice is kept and restored)"
    run systemctl --user stop "$UNIT"
    STOPPED=1
  fi
}

install_binary() {
  step "Installing the program"
  local new=$PAYLOAD/bin/$APP newv oldv=""
  newv=$(version_of "$new")
  [ -n "$newv" ] || die "$new does not run here (missing CUDA 13 runtime or wrong architecture?)"
  if [ -x "$BIN" ]; then
    oldv=$(version_of "$BIN")
    if cmp -s "$new" "$BIN"; then
      ok "$BIN is already $newv"
      BINARY_CHANGED=0
      return
    fi
    say "  upgrade: ${oldv:-unknown} → $newv"
    stop_service
    run cp -f "$BIN" "$BIN.prev"
    ok "previous binary kept as $BIN.prev (install.sh --rollback)"
  else
    say "  fresh install: $newv"
  fi
  run install -Dm0755 "$new" "$BIN.new"
  run mv -f "$BIN.new" "$BIN"
  ok "$BIN"
  BINARY_CHANGED=1
}

install_share() {
  local p=$PAYLOAD f
  run install -d "$SHAREDIR/systemd/system" "$SHAREDIR/systemd/user"
  run install -m0755 "$p/install.sh" "$SHAREDIR/install.sh"
  for f in install-clockcap.sh uninstall-clockcap.sh; do
    run install -m0755 "$p/packaging/$f" "$SHAREDIR/$f"
  done
  run install -m0644 "$p/packaging/systemd/system/$CLOCKCAP_UNIT" "$SHAREDIR/systemd/system/$CLOCKCAP_UNIT"
  run install -m0644 "$p/packaging/systemd/user/$UNIT" "$SHAREDIR/systemd/user/$UNIT"
  for f in VERSION LICENSE NOTICE README.md README.pt-BR.md; do
    [ -f "$p/$f" ] && run install -m0644 "$p/$f" "$SHAREDIR/$f"
  done
  if [ -d "$p/docs" ]; then
    run rm -rf "$SHAREDIR/docs"
    run cp -R "$p/docs" "$SHAREDIR/docs"
  fi
  ok "$SHAREDIR (clock-cap scripts, this installer, docs)"
}

UNIT_CHANGED=0
install_unit() {
  local src=$PAYLOAD/packaging/systemd/user/$UNIT rendered="$TMP/$UNIT"
  if [ "$BINDIR" = "$HOME/.local/bin" ]; then
    cp "$src" "$rendered"
  else
    sed "s|%h/\.local/bin/|$BINDIR/|g" "$src" >"$rendered"
  fi
  if ! cmp -s "$rendered" "$UNIT_FILE"; then
    UNIT_CHANGED=1
    write_file "$UNIT_FILE" <"$rendered"
  fi
  ok "$UNIT_FILE"
}

install_desktop() {
  local src=$PAYLOAD/packaging/$APP.desktop icon=$PAYLOAD/packaging/$APP.svg icon_name=utilities-system-monitor
  if [ -f "$icon" ]; then
    run install -Dm0644 "$icon" "$ICON_FILE"
    icon_name=$APP
  fi
  # .desktop files do not expand ~ or %h: the absolute path keeps the menu entry working even
  # when ~/.local/bin is not on the session PATH.
  sed -e "s|^Exec=.*|Exec=$BIN gui|" -e "s|^TryExec=.*|TryExec=$BIN|" -e "s|^Icon=.*|Icon=$icon_name|" "$src" |
    write_file "$DESKTOP_FILE"
  if have update-desktop-database; then run update-desktop-database -q "$APPS_DIR" || true; fi
  ok "app menu entry: $DESKTOP_FILE"
}

# On upgrade, the new binary must accept the existing config.toml before it replaces a working
# install; the file itself is never changed.
check_config() {
  [ -f "$CONFIG_FILE" ] || return 0
  [ "$DRY_RUN" -eq 1 ] && { run "$BIN" config check; return 0; }
  local out rc=0
  out=$("$BIN" config check 2>&1) || rc=$?
  case "$rc" in
    0) ok "config.toml is valid for the new version (kept as is)" ;;
    1)
      printf '%s\n' "$out" >&2
      if [ -x "$BIN.prev" ]; then
        warn "the new version refuses your config.toml: putting the previous version back"
        mv -f "$BIN.prev" "$BIN"
        [ "$STOPPED" -eq 1 ] && systemctl --user start "$UNIT" || true
      fi
      die "fix $CONFIG_FILE (spark-pearl-miner config check), then run the installer again"
      ;;
    *) warn "could not check config.toml with the new binary (status $rc): $out" ;;
  esac
}

wait_for_gui() {
  local i
  for i in $(seq 1 30); do
    # Any HTTP answer counts (curl without -f fails only when nothing listens).
    if curl -s -m 1 -o /dev/null "http://127.0.0.1:$PORT/api/v1/about" 2>/dev/null; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

start_service() {
  step "Service"
  if [ "$START" -eq 0 ]; then
    say "  --no-start: the service was not enabled or started. Later:"
    say "    systemctl --user daemon-reload && systemctl --user enable --now $APP"
    return
  fi
  run systemctl --user daemon-reload
  run systemctl --user enable --quiet "$UNIT"
  # A new clock cap also restarts a running daemon: it reports "not capped" from the first
  # reading above the cap until it restarts.
  if [ "$STOPPED" -eq 1 ] || { { [ "$UNIT_CHANGED" -eq 1 ] || [ "$CAP_INSTALLED" -eq 1 ]; } && service_active; }; then
    run systemctl --user restart "$UNIT"
  else
    run systemctl --user start "$UNIT"
  fi
  if [ "$DRY_RUN" -eq 1 ]; then return; fi
  if wait_for_gui; then
    ok "running; the GUI answers on $GUI_URL"
  else
    warn "the service did not answer within 15 s; see: journalctl --user -u $APP -n 50"
    warn "and: $BIN status"
  fi
}

LINGER_STATE=unknown
linger_state() {
  loginctl show-user "$USER_NAME" -p Linger --value 2>/dev/null || echo unknown
}

# A question with default Yes, asked on the terminal. --yes answers it; returns 1 for no.
ask_yes() {
  local answer
  if [ "$YES" -eq 1 ]; then
    say "  $1 [Y/n] yes (--yes)"
    return 0
  fi
  answer=$(tty_read "  $1 [Y/n] ")
  case "$answer" in
    '' | [Yy] | [Yy][Ee][Ss] | [Ss] | [Ss][Ii][Mm]) return 0 ;;
    *) return 1 ;;
  esac
}

# Runs one root command through sudo (sudo asks for the password on the terminal); stdin is
# never the script, so nothing can read the rest of it under curl | bash.
as_root() {
  sudo "$@" </dev/null
}

# The two optional root steps, asked before the service (re)starts, so the daemon never runs
# with the GPU uncapped when you accepted the cap:
#   (a) the 2000 MHz GPU clock cap at every boot (the safety net under the power governor);
#   (b) lingering: the service keeps running after logout and starts at boot before login.
# Default Yes; --yes accepts both; --no-sudo, or no terminal to ask on, skips both; --dry-run
# prints the commands. A skipped step is printed again in the summary.
CAP_INSTALLED=0
sudo_steps() {
  local want_cap=0 want_linger=0
  clock_cap_active || want_cap=1
  [ "$START" -eq 1 ] && LINGER_STATE=$(linger_state)
  if [ "$START" -eq 1 ] && [ "$LINGER" -eq 1 ]; then
    # Without a password when the system allows it (polkit); never prompts.
    if [ "$LINGER_STATE" != yes ] && [ "$DRY_RUN" -eq 0 ] &&
      loginctl --no-ask-password enable-linger "$USER_NAME" >/dev/null 2>&1; then
      LINGER_STATE=yes
      ok "lingering enabled: the miner keeps running after logout and starts at boot"
    fi
    [ "$LINGER_STATE" = yes ] || want_linger=1
  fi
  [ "$want_cap" -eq 1 ] || [ "$want_linger" -eq 1 ] || return 0
  step "Two optional steps that need sudo (your password, once)"
  if [ "$SUDO" -eq 0 ]; then
    say "  --no-sudo: skipped; the commands are in the summary below"
    return 0
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    if [ "$want_cap" -eq 1 ]; then
      say "  would ask: install the 2000 MHz GPU clock cap? [Y/n]"
      run sudo "$SHAREDIR/install-clockcap.sh" --apply
    fi
    if [ "$want_linger" -eq 1 ]; then
      say "  would ask: keep mining after logout and at boot? [Y/n]"
      run sudo loginctl enable-linger "$USER_NAME"
    fi
    return 0
  fi
  if [ "$YES" -eq 0 ] && ! have_tty; then
    say "  no terminal to ask on: skipped; the commands are in the summary below"
    return 0
  fi
  if [ "$want_cap" -eq 1 ]; then
    say "  The GB10 has no software power limit and is known to power off at about 88–92 W. The"
    say "  clock cap keeps the GPU at about 63 W (2000 MHz, measured); without it only the power"
    say "  governor guards the Spark. It is reversible (uninstall-clockcap.sh)."
    if ask_yes "Install the 2000 MHz GPU clock cap (recommended)?"; then
      if as_root "$SHAREDIR/install-clockcap.sh" --apply && clock_cap_active; then
        CAP_INSTALLED=1
        ok "GPU clock cap installed: 2000 MHz now and at every boot"
      else
        warn "the clock cap was not installed; run later: sudo $SHAREDIR/install-clockcap.sh --apply"
      fi
    else
      say "  skipped; the dashboard shows a reminder until you install it"
    fi
  fi
  if [ "$want_linger" -eq 1 ]; then
    if ask_yes "Keep mining after you log out and start at boot, before you log in?"; then
      if as_root loginctl enable-linger "$USER_NAME"; then
        LINGER_STATE=$(linger_state)
        ok "lingering enabled: the miner keeps running after logout and starts at boot"
      else
        warn "lingering was not enabled; run later: sudo loginctl enable-linger $USER_NAME"
      fi
    else
      say "  skipped; the miner runs while you are logged in"
    fi
  fi
}

open_gui() {
  [ "$START" -eq 1 ] && [ "$OPEN" -eq 1 ] && [ "$DRY_RUN" -eq 0 ] || return 0
  if [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
    "$BIN" gui >/dev/null 2>&1 || true
  fi
}

clock_cap_active() {
  systemctl is-active --quiet "$CLOCKCAP_UNIT" 2>/dev/null
}

on_path() {
  case ":$PATH:" in *":$BINDIR:"*) return 0 ;; esac
  return 1
}

summary() {
  step "Done"
  local v
  v=$([ -x "$BIN" ] && version_of "$BIN" || true)
  say "  version      ${v:-(dry run)}"
  say "  program      $BIN"
  say "  settings     $CONFIG_FILE (written by the daemon on its first start; never by this installer)"
  if [ "$START" -eq 1 ] && [ "$DRY_RUN" -eq 0 ]; then
    say "  service      $(systemctl --user is-active "$UNIT" 2>/dev/null || true), $(systemctl --user is-enabled "$UNIT" 2>/dev/null || true) at login"
    say "  lingering    $([ "$LINGER_STATE" = yes ] && echo "on (mines after logout and at boot)" || echo "off (mines while you are logged in)")"
  fi
  if clock_cap_active; then
    say "  clock cap    active (GPU SM clock capped at boot)"
  else
    say "  clock cap    ${Y}NOT installed${N} (the GPU is not capped at 2000 MHz)"
  fi
  say ""
  say "  ${B}Open the GUI:${N} $GUI_URL   (or \"Spark Pearl Miner\" in the app menu, or: $APP gui)"
  say "  A fresh install does not mine yet: pick a language, paste your prl1p… wallet, accept the"
  say "  2 % developer fee and press Start mining. Stop is remembered across reboots."
  say "  From another computer: ssh -L $PORT:127.0.0.1:$PORT you@your-spark, then open $GUI_URL"
  if [ "$START" -eq 1 ] && [ "$LINGER_STATE" != yes ]; then
    say ""
    say "  To keep mining after you log out and start at boot before login (needs sudo once):"
    say "    sudo loginctl enable-linger $USER_NAME"
  fi
  if ! clock_cap_active; then
    say ""
    say "  ${B}Recommended, before you press Start mining:${N} cap the GPU clock at 2000 MHz at every boot"
    say "  (about 63 W, far from the ~88–92 W at which the Spark powers off; reversible). One command:"
    say "    sudo $SHAREDIR/install-clockcap.sh --apply"
  fi
  say ""
  say "  status       $APP status"
  say "  logs         journalctl --user -u $APP -f"
  say "  upgrade      $SHAREDIR/install.sh --upgrade"
  say "  rollback     $SHAREDIR/install.sh --rollback"
  say "  uninstall    $SHAREDIR/install.sh --uninstall [--purge]"
  if ! on_path; then
    say ""
    if [ "$BINDIR" = "$HOME/.local/bin" ]; then
      say "  Note: $BINDIR is not on your PATH yet; ~/.profile adds it at your next login."
    else
      say "  Note: $BINDIR is not on your PATH; use the full path or add it to PATH."
    fi
  fi
}

do_install() {
  preflight
  get_payload
  if [ -z "$PAYLOAD" ]; then
    # Dry run of a source build: nothing was built, so there is nothing more to show.
    say ""
    say "  [dry-run] then: install the built binary to $BIN, the unit to $UNIT_FILE,"
    say "  the menu entry to $DESKTOP_FILE and $SHAREDIR; enable and start $UNIT"
    return
  fi
  install_binary
  install_share
  install_unit
  install_desktop
  if [ "$BINARY_CHANGED" -eq 1 ]; then check_config; fi
  sudo_steps
  start_service
  open_gui
  summary
}

# ---------------------------------------------------------------------------------------------
# Uninstall and rollback
# ---------------------------------------------------------------------------------------------
do_uninstall() {
  step "Removing $APP"
  case "$SHAREDIR" in
    */share/"$APP") ;;
    *) die "refusing to remove an unexpected directory: $SHAREDIR" ;;
  esac
  if [ "$START" -eq 1 ] && [ -f "$UNIT_FILE" ]; then
    run systemctl --user disable --now "$UNIT" || true
  fi
  local f
  for f in "$UNIT_FILE" "$BIN" "$BIN.prev" "$BIN.new" "$DESKTOP_FILE" "$ICON_FILE"; do
    if [ -e "$f" ]; then run rm -f "$f" && ok "removed $f"; fi
  done
  if [ -d "$SHAREDIR" ]; then run rm -rf "$SHAREDIR" && ok "removed $SHAREDIR"; fi
  if [ -d "$CACHE_DIR/src" ]; then run rm -rf "$CACHE_DIR/src" && ok "removed the source clone $CACHE_DIR/src"; fi
  if have update-desktop-database && [ -d "$APPS_DIR" ]; then run update-desktop-database -q "$APPS_DIR" || true; fi
  if [ "$START" -eq 1 ]; then run systemctl --user daemon-reload || true; fi

  if [ "$PURGE" -eq 1 ]; then
    local answer=yes
    if [ "$YES" -ne 1 ]; then
      answer=$(tty_read "  Delete your settings, wallet address and API token ($CONFIG_DIR) and state ($STATE_DIR)? Type delete: ")
      [ "$answer" = delete ] && answer=yes
    fi
    if [ "$answer" = yes ]; then
      for f in "$CONFIG_DIR" "$STATE_DIR"; do
        if [ -e "$f" ]; then run rm -rf "$f" && ok "removed $f"; fi
      done
    else
      say "  settings kept: $CONFIG_DIR"
    fi
  elif [ -d "$CONFIG_DIR" ]; then
    say "  settings kept (wallet, token): $CONFIG_DIR  (remove them with --uninstall --purge)"
  fi
  if [ -d "$CACHE_DIR/target" ]; then
    say "  build cache kept: $CACHE_DIR/target  (rm -rf it to free the space)"
  fi
  if clock_cap_active; then
    say ""
    say "  The GPU clock cap is still installed. To remove it and restore the default clocks:"
    say "    sudo systemctl disable --now $CLOCKCAP_UNIT"
    say "    sudo rm -f /etc/systemd/system/$CLOCKCAP_UNIT && sudo systemctl daemon-reload"
  fi
  if [ "$START" -eq 1 ] && [ "$(loginctl show-user "$USER_NAME" -p Linger --value 2>/dev/null || true)" = yes ]; then
    say "  Lingering stays on (other services may use it); to turn it off: sudo loginctl disable-linger $USER_NAME"
  fi
}

do_rollback() {
  step "Rolling back to the previous version"
  [ -x "$BIN.prev" ] || die "no previous version at $BIN.prev (it is kept by an upgrade)"
  say "  $(version_of "$BIN") → $(version_of "$BIN.prev")"
  stop_service
  run mv -f "$BIN" "$BIN.new"
  run mv -f "$BIN.prev" "$BIN"
  run mv -f "$BIN.new" "$BIN.prev"
  ok "$BIN is the previous version again (the newer one is now $BIN.prev)"
  if [ -f "$CONFIG_FILE" ] && [ "$DRY_RUN" -eq 0 ] && ! "$BIN" config check >/dev/null 2>&1; then
    warn "the previous version may not accept config.toml; check: $BIN status"
  fi
  if [ "$START" -eq 1 ]; then
    run systemctl --user start "$UNIT"
    ok "service started"
  fi
}

main() {
  parse_args "$@"
  if [ "$(id -u)" -eq 0 ]; then
    die "run as your normal user, not root: the miner is a per-user service and never needs root to mine"
  fi
  setup_paths
  TMP=$(mktemp -d "${TMPDIR:-/tmp}/spm-install.XXXXXX")
  trap 'rm -rf "$TMP"' EXIT
  [ "$DRY_RUN" -eq 1 ] && say "${B}dry run: nothing is changed${N}"
  case "$MODE" in
    install | upgrade)
      if [ "$MODE" = upgrade ] && [ ! -x "$BIN" ]; then say "  --upgrade: nothing installed at $BIN yet, doing a fresh install"; fi
      do_install
      ;;
    uninstall) do_uninstall ;;
    rollback) do_rollback ;;
  esac
}

# Everything runs from main, so a download cut short under curl | bash executes nothing; the
# exit on the same line keeps bash from reading this file again after main (an upgrade replaces
# the installed copy of this script while it runs).
main "$@"; exit $?
