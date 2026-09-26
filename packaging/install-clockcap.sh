#!/usr/bin/env bash
# Installs the optional boot-time GPU clock cap (systemd/system/spark-pearl-clockcap.service).
#
# By default this only PRINTS the commands, ready to paste with sudo. It runs them itself only
# with --apply AND when it is already running as root:
#   packaging/install-clockcap.sh                 # print the commands
#   sudo packaging/install-clockcap.sh --apply    # run them
# See docs/en/POWER-THERMAL.md for why the cap exists.
set -euo pipefail

usage() {
  cat <<'EOF'
usage: install-clockcap.sh [--mhz N] [--apply]
  --mhz N   SM clock cap in MHz (default 2200, what the Balanced and Max profiles expect;
            the Eco profile recommends 2000)
  --apply   run the commands (must run as root); without it they are only printed
EOF
}

APPLY=0
MHZ=2200
while [ $# -gt 0 ]; do
  case "$1" in
    --apply) APPLY=1 ;;
    --mhz) shift; MHZ=${1:-} ;;
    --mhz=*) MHZ=${1#--mhz=} ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

case "$MHZ" in
  '' | *[!0-9]*) echo "--mhz needs a number of MHz" >&2; exit 2 ;;
esac
if [ "$MHZ" -lt 1000 ] || [ "$MHZ" -gt 2500 ]; then
  echo "--mhz must be between 1000 and 2500" >&2
  exit 2
fi

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
UNIT=spark-pearl-clockcap.service
SRC="$HERE/systemd/system/$UNIT"
DST="/etc/systemd/system/$UNIT"
[ -f "$SRC" ] || { echo "missing $SRC" >&2; exit 1; }
if [ ! -x /usr/bin/nvidia-smi ]; then
  echo "warning: /usr/bin/nvidia-smi not found; the unit would be skipped at boot" >&2
fi

if [ "$APPLY" -eq 1 ] && [ "$(id -u)" -ne 0 ]; then
  printf 'refusing --apply without root; run: sudo %q --apply' "$0" >&2
  if [ "$MHZ" != 2200 ]; then printf " --mhz %s" "$MHZ" >&2; fi
  printf '\n' >&2
  exit 1
fi

# Prints one command as it would be typed (single-quoted where needed, with sudo) and runs it
# under --apply.
SQ="'"
step() {
  local arg out=sudo
  for arg in "$@"; do
    case "$arg" in
      *[!A-Za-z0-9_./=:,@%+-]*) out+=" '${arg//$SQ/$SQ\\$SQ$SQ}'" ;;
      *) out+=" $arg" ;;
    esac
  done
  printf '%s\n' "$out"
  if [ "$APPLY" -eq 1 ]; then
    "$@"
  fi
}

if [ "$APPLY" -eq 1 ]; then
  echo "# installing the clock cap ($MHZ MHz):"
else
  echo "# commands to install the clock cap ($MHZ MHz); nothing was changed:"
fi
step install -m 0644 "$SRC" "$DST"
if [ "$MHZ" != 2200 ]; then
  step sed -i "s/-lgc 300,2200/-lgc 300,$MHZ/;s/300-2200 MHz/300-$MHZ MHz/" "$DST"
fi
step systemctl daemon-reload
step systemctl enable --now "$UNIT"
# A unit that is already active keeps its old clock until restarted.
if systemctl is-active --quiet "$UNIT" 2>/dev/null; then
  step systemctl restart "$UNIT"
fi
echo "# check: nvidia-smi --query-gpu=clocks.sm,clocks.max.sm --format=csv"
echo "#        (the SM clock should now stay at or below $MHZ MHz, even at idle)"
