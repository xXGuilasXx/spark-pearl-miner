#!/usr/bin/env bash
# Smoke test of packaging/install.sh and packaging/make-release.sh, safe on a machine that is
# mining: everything happens in a throwaway HOME under /tmp, with --no-start (no systemctl
# --user, no loginctl), --no-open and --no-sudo; the optional sudo steps are exercised only with
# --dry-run or with a fake sudo on PATH that records its arguments and runs nothing. Nothing
# binds a port and the GPU is not used.
#
#   tools/test-install.sh          # with a stub binary (seconds)
#   tools/test-install.sh --real   # package the real binary (cargo build --profile dist first)
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REAL=0
[ "${1:-}" = --real ] && REAL=1

T=$(mktemp -d /tmp/spm-install.XXXXXX)
trap 'rm -rf "$T"' EXIT
case "$T" in /tmp/spm-install.*) ;; *) echo "unexpected temp dir $T" >&2; exit 1 ;; esac

# The throwaway account. XDG_RUNTIME_DIR/DBUS are dropped so that even a mistaken systemctl
# --user could not reach the real session manager.
REAL_HOME=$HOME
export HOME="$T/home"
export XDG_CONFIG_HOME="$HOME/.config" XDG_DATA_HOME="$HOME/.local/share"
export XDG_STATE_HOME="$HOME/.local/state" XDG_CACHE_HOME="$HOME/.cache"
unset XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS DISPLAY WAYLAND_DISPLAY
mkdir -p "$HOME"

APP=spark-pearl-miner
VERSION=$(awk '/^\[workspace\.package\]/{p=1;next} /^\[/{p=0} p&&$1=="version"{gsub(/"/,"",$3);print $3;exit}' "$ROOT/Cargo.toml")
PREFIX="$T/prefix"
BIN="$PREFIX/bin/$APP"
SHARE="$PREFIX/share/$APP"
UNIT="$XDG_CONFIG_HOME/systemd/user/$APP.service"
DESKTOP="$XDG_DATA_HOME/applications/$APP.desktop"
CFG="$XDG_CONFIG_HOME/$APP/config.toml"
INSTALL=(bash "$ROOT/packaging/install.sh" --no-start --no-open --no-linger --no-sudo)

PASS=0
pass() { PASS=$((PASS + 1)); printf '  ok   %s\n' "$*"; }
fail() { printf '  FAIL %s\n' "$*" >&2; exit 1; }
check() { local what=$1; shift; if "$@"; then pass "$what"; else fail "$what"; fi; }

# A stand-in for the real binary: answers --version and `config check` (invalid when the config
# contains the word INVALID), like the real one.
stub() {
  local out=$1 flavour=$2
  mkdir -p "$(dirname "$out")"
  cat >"$out" <<EOF
#!/usr/bin/env bash
# stub $flavour
case "\${1:-}" in
  --version | version) echo "$APP $VERSION (commit stub-$flavour)"; echo "fee constants hash (BLAKE3): stub" ;;
  config)
    f="\${XDG_CONFIG_HOME:-\$HOME/.config}/$APP/config.toml"
    if [ -f "\$f" ] && grep -q INVALID "\$f"; then echo "miner.wallet: not a Pearl mainnet address"; exit 1; fi
    echo "OK: \$f" ;;
  *) exit 0 ;;
esac
EOF
  chmod 0755 "$out"
}

# Builds a release tarball + SHA256SUMS into $1 from binary $2.
package() {
  bash "$ROOT/packaging/make-release.sh" --allow-dirty --out "$1" --binary "$2" >"$T/package.log" 2>&1 ||
    { cat "$T/package.log" >&2; fail "make-release.sh"; }
}

echo "== packaging"
if [ "$REAL" -eq 1 ]; then
  # The real build uses my own toolchain and build cache.
  HOME="$REAL_HOME" bash "$ROOT/packaging/make-release.sh" --allow-dirty --out "$T/dist1" >"$T/package.log" 2>&1 ||
    { tail -n 30 "$T/package.log" >&2; fail "make-release.sh (real build)"; }
else
  stub "$T/stub1/$APP" one
  package "$T/dist1" "$T/stub1/$APP"
fi
TARBALL="$T/dist1/$APP-$VERSION-linux-aarch64.tar.gz"
check "tarball and SHA256SUMS written" test -f "$TARBALL" -a -f "$T/dist1/SHA256SUMS"
check "SHA256SUMS verifies" bash -c "cd '$T/dist1' && sha256sum -c --quiet SHA256SUMS"
LIST=$(tar tzf "$TARBALL")
for f in bin/$APP install.sh VERSION LICENSE NOTICE README.md README.pt-BR.md \
  packaging/install-clockcap.sh packaging/uninstall-clockcap.sh packaging/$APP.desktop packaging/$APP.svg \
  packaging/systemd/user/$APP.service packaging/systemd/system/spark-pearl-clockcap.service; do
  grep -qx "$APP-$VERSION-linux-aarch64/$f" <<<"$LIST" || fail "tarball lacks $f"
done
pass "tarball layout"
if [ "$REAL" -eq 0 ]; then
  cp "$TARBALL" "$T/again.tar.gz"
  package "$T/dist1" "$T/stub1/$APP"
  check "tarball is reproducible" cmp -s "$TARBALL" "$T/again.tar.gz"
fi

echo "== dry run"
"${INSTALL[@]}" --tarball "$TARBALL" --prefix "$PREFIX" --dry-run >"$T/dry.log" 2>&1 || { cat "$T/dry.log"; fail "dry run"; }
check "dry run prints the install" grep -q "\[dry-run\] install -Dm0755 .*$APP.new" "$T/dry.log"
check "dry run changes nothing" test ! -e "$PREFIX" -a ! -e "$UNIT" -a ! -e "$DESKTOP"

echo "== preflight is not flaky under pipefail"
# A loader cache much larger than a pipe buffer, with the CUDA runtime on the first line: a
# `ldconfig -p | grep -q` would let grep exit at once and ldconfig die of SIGPIPE, and pipefail
# would then report the runtime as missing (it used to fail ~6 % of real runs on the Spark).
mkdir -p "$T/bigldconfig"
cat >"$T/bigldconfig/ldconfig" <<'EOF2'
#!/usr/bin/env bash
echo "	libcudart.so.13 (libc6,AArch64) => /usr/local/cuda/lib64/libcudart.so.13"
for i in $(seq 1 20000); do echo "	libfiller$i.so.1 (libc6,AArch64) => /usr/lib/aarch64-linux-gnu/libfiller$i.so.1"; done
EOF2
chmod 0755 "$T/bigldconfig/ldconfig"
PATH="$T/bigldconfig:$PATH" "${INSTALL[@]}" --tarball "$TARBALL" --prefix "$PREFIX" --dry-run >"$T/bigld.log" 2>&1 ||
  { cat "$T/bigld.log"; fail "dry run with a large loader cache"; }
check "a large loader cache still finds libcudart.so.13" grep -q "CUDA 13 runtime (libcudart.so.13)" "$T/bigld.log"
# No early-exit reader at the end of a pipe in the installer (it runs under pipefail).
check "install.sh has no '| head' or '| grep -q' pipelines" \
  bash -c "! grep -nE '\\|[[:space:]]*(head|grep[[:space:]]+(-[a-zA-Z]*q|-m|--quiet))' '$ROOT/packaging/install.sh' | grep -v '^[0-9]*:[[:space:]]*#' | grep -v 'check with:'"

echo "== piped like curl | bash (dry run)"
bash -s -- --tarball "$TARBALL" --prefix "$PREFIX" --no-start --no-open --dry-run <"$ROOT/packaging/install.sh" >"$T/pipe.log" 2>&1 ||
  { cat "$T/pipe.log"; fail "piped dry run"; }
check "piped script runs and verifies the tarball" grep -q "SHA256 verified" "$T/pipe.log"
check "piped dry run changes nothing" test ! -e "$PREFIX"

echo "== the optional sudo steps (clock cap): asked, never run for real here"
# A fake sudo that only records its arguments: nothing runs as root, whatever the installer does.
mkdir -p "$T/fakebin"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >>"%s"\nexit 0\n' "$T/sudo.calls" >"$T/fakebin/sudo"
chmod 0755 "$T/fakebin/sudo"
NOSUDO_INSTALL=(bash "$ROOT/packaging/install.sh" --no-start --no-open --no-linger)
SP="$T/prefix-sudo"
if systemctl is-active --quiet spark-pearl-clockcap.service 2>/dev/null; then
  echo "  (skipped: the clock cap is already active on this machine, so the installer does not ask)"
else
  PATH="$T/fakebin:$PATH" "${NOSUDO_INSTALL[@]}" --tarball "$TARBALL" --prefix "$SP" --dry-run >"$T/s1.log" 2>&1 ||
    { cat "$T/s1.log"; fail "sudo steps dry run"; }
  check "dry run shows the clock-cap question and command" grep -q "\[dry-run\] sudo $SP/share/$APP/install-clockcap.sh --apply" "$T/s1.log"
  check "dry run never calls sudo" test ! -e "$T/sudo.calls"
  PATH="$T/fakebin:$PATH" "${NOSUDO_INSTALL[@]}" --tarball "$TARBALL" --prefix "$SP" --dry-run --no-sudo >"$T/s2.log" 2>&1 ||
    { cat "$T/s2.log"; fail "--no-sudo dry run"; }
  check "--no-sudo skips the question" grep -q -- "--no-sudo: skipped" "$T/s2.log"
  check "--no-sudo prints no sudo run" bash -c "! grep -q '\[dry-run\] sudo' '$T/s2.log'"
  if command -v setsid >/dev/null 2>&1; then
    # No controlling terminal (like a cron job or a pipe from ssh without -t): nothing is asked.
    PATH="$T/fakebin:$PATH" setsid -w "${NOSUDO_INSTALL[@]}" --tarball "$TARBALL" --prefix "$SP" </dev/null >"$T/s3.log" 2>&1 ||
      { cat "$T/s3.log"; fail "install without a terminal"; }
    check "no terminal: the sudo steps are skipped" grep -q "no terminal to ask on: skipped" "$T/s3.log"
    check "no terminal: sudo is never called" test ! -e "$T/sudo.calls"
    check "no terminal: summary says the cap is not installed" grep -q "clock cap    .*NOT installed" "$T/s3.log"
    check "no terminal: summary prints the clock-cap command" grep -q "sudo $SP/share/$APP/install-clockcap.sh --apply" "$T/s3.log"
  fi
  PATH="$T/fakebin:$PATH" "${NOSUDO_INSTALL[@]}" --tarball "$TARBALL" --prefix "$SP" --yes </dev/null >"$T/s4.log" 2>&1 ||
    { cat "$T/s4.log"; fail "install --yes"; }
  check "--yes runs the clock-cap script through sudo" grep -qx "$SP/share/$APP/install-clockcap.sh --apply" "$T/sudo.calls"
  check "--yes with --no-start does not touch lingering" bash -c "! grep -q loginctl '$T/sudo.calls'"
  # The fake sudo did nothing, so the cap is still missing: the installer must say so.
  check "a cap that did not come up is reported" grep -q "the clock cap was not installed" "$T/s4.log"
  rm -rf "$SP"
fi

echo "== fresh install from the tarball"
"${INSTALL[@]}" --tarball "$TARBALL" --prefix "$PREFIX" >"$T/i1.log" 2>&1 || { cat "$T/i1.log"; fail "install"; }
check "binary installed" test -x "$BIN"
check "installer, clock-cap scripts and unit in the share dir" test -x "$SHARE/install.sh" -a -x "$SHARE/install-clockcap.sh" \
  -a -x "$SHARE/uninstall-clockcap.sh" -a -f "$SHARE/systemd/system/spark-pearl-clockcap.service"
check "unit ExecStart points into the prefix" grep -qx "ExecStart=$BIN daemon" "$UNIT"
check "unit ExecStartPre checks the config" grep -qx "ExecStartPre=$BIN config check" "$UNIT"
check ".desktop Exec is absolute" grep -qx "Exec=$BIN gui" "$DESKTOP"
check ".desktop TryExec is absolute" grep -qx "TryExec=$BIN" "$DESKTOP"
check "icon installed" test -f "$XDG_DATA_HOME/icons/hicolor/scalable/apps/$APP.svg"
check "no config.toml created" test ! -e "$CFG"
check "nothing under ~/.local/bin" test ! -e "$HOME/.local/bin"
check "summary shows the GUI URL" grep -q "http://127.0.0.1:4078/" "$T/i1.log"
check "summary shows the clock-cap command" grep -q "sudo $SHARE/install-clockcap.sh --apply" "$T/i1.log"
# The clock-cap script from the share dir finds its unit (prints only, no root).
check "installed install-clockcap.sh finds its unit" grep -q 'enable --now spark-pearl-clockcap.service' <<<"$("$SHARE/install-clockcap.sh")"

echo "== idempotent re-run"
"${INSTALL[@]}" --tarball "$TARBALL" --prefix "$PREFIX" >"$T/i2.log" 2>&1 || { cat "$T/i2.log"; fail "re-run"; }
check "same binary is not replaced" grep -q "is already" "$T/i2.log"
check "no .prev on a no-op" test ! -e "$BIN.prev"

if [ "$REAL" -eq 1 ]; then
  check "the real binary validates a missing config" "$BIN" config check
  echo "test-install --real: $PASS checks passed (the upgrade/rollback checks need the stubs)"
  exit 0
fi

echo "== upgrade from the extracted tarball (./install.sh)"
stub "$T/stub2/$APP" two
package "$T/dist2" "$T/stub2/$APP"
mkdir -p "$T/x2"
tar xzf "$T/dist2/$APP-$VERSION-linux-aarch64.tar.gz" -C "$T/x2"
mkdir -p "$(dirname "$CFG")"
printf '# my settings\n[miner]\nwallet = ""\n' >"$CFG"
SUM=$(sha256sum "$CFG")
(cd "$T/x2/$APP-$VERSION-linux-aarch64" && ./install.sh --no-start --no-open --no-sudo --prefix "$PREFIX") >"$T/i3.log" 2>&1 ||
  { cat "$T/i3.log"; fail "upgrade"; }
check "upgrade used the files next to the script" grep -q "release files next to this script" "$T/i3.log"
check "new binary installed" grep -q "stub two" "$BIN"
check "previous binary kept" grep -q "stub one" "$BIN.prev"
check "config.toml untouched" test "$(sha256sum "$CFG")" = "$SUM"

echo "== upgrade refused by config check rolls back"
printf 'INVALID\n' >>"$CFG"
SUM=$(sha256sum "$CFG")
if "${INSTALL[@]}" --tarball "$TARBALL" --prefix "$PREFIX" >"$T/i4.log" 2>&1; then cat "$T/i4.log"; fail "an invalid config must stop the upgrade"; fi
check "the working binary is back" grep -q "stub two" "$BIN"
check "config.toml still untouched" test "$(sha256sum "$CFG")" = "$SUM"
sed -i '/INVALID/d' "$CFG"

echo "== rollback"
"${INSTALL[@]}" --tarball "$TARBALL" --prefix "$PREFIX" >"$T/i5.log" 2>&1 || { cat "$T/i5.log"; fail "upgrade to one"; }
check "back on stub one" grep -q "stub one" "$BIN"
bash "$SHARE/install.sh" --rollback --no-start --prefix "$PREFIX" >"$T/r.log" 2>&1 || { cat "$T/r.log"; fail "rollback"; }
check "rollback restored stub two" grep -q "stub two" "$BIN"
check "rollback keeps the other as .prev" grep -q "stub one" "$BIN.prev"

echo "== bad checksum"
mkdir -p "$T/bad"
cp "$TARBALL" "$T/bad/"
printf '%064d  %s\n' 0 "$(basename "$TARBALL")" >"$T/bad/SHA256SUMS"
if "${INSTALL[@]}" --tarball "$T/bad/$(basename "$TARBALL")" --prefix "$T/prefix-bad" >"$T/bad.log" 2>&1; then fail "a wrong checksum must be refused"; fi
check "wrong checksum refused, nothing installed" test ! -e "$T/prefix-bad"

echo "== default prefix keeps %h in the unit"
"${INSTALL[@]}" --tarball "$TARBALL" >"$T/d.log" 2>&1 || { cat "$T/d.log"; fail "default prefix"; }
check "binary in ~/.local/bin" test -x "$HOME/.local/bin/$APP"
check "unit keeps %h/.local/bin" grep -qx "ExecStart=%h/.local/bin/$APP daemon" "$UNIT"
bash "$HOME/.local/share/$APP/install.sh" --uninstall --no-start >"$T/u0.log" 2>&1 || { cat "$T/u0.log"; fail "uninstall default"; }
check "default-prefix uninstall" test ! -e "$HOME/.local/bin/$APP" -a ! -e "$HOME/.local/share/$APP"

echo "== options"
if bash "$ROOT/packaging/install.sh" --purge >/dev/null 2>&1; then fail "--purge alone must be refused"; fi
pass "--purge without --uninstall refused"

echo "== uninstall keeps settings, --purge removes them"
"${INSTALL[@]}" --tarball "$TARBALL" --prefix "$PREFIX" >/dev/null 2>&1 || fail "reinstall"
bash "$SHARE/install.sh" --uninstall --no-start --prefix "$PREFIX" >"$T/u1.log" 2>&1 || { cat "$T/u1.log"; fail "uninstall"; }
check "program, unit, menu entry and share dir removed" test ! -e "$BIN" -a ! -e "$BIN.prev" -a ! -e "$UNIT" -a ! -e "$DESKTOP" -a ! -e "$SHARE"
check "settings kept" test -f "$CFG"
"${INSTALL[@]}" --tarball "$TARBALL" --prefix "$PREFIX" >/dev/null 2>&1 || fail "reinstall"
mkdir -p "$XDG_STATE_HOME/$APP"
bash "$SHARE/install.sh" --uninstall --purge --yes --no-start --prefix "$PREFIX" >"$T/u2.log" 2>&1 || { cat "$T/u2.log"; fail "purge"; }
check "--purge removed settings and state" test ! -e "$XDG_CONFIG_HOME/$APP" -a ! -e "$XDG_STATE_HOME/$APP"
check "prefix is empty again" test -z "$(find "$PREFIX" -type f)"

echo "test-install: $PASS checks passed"
