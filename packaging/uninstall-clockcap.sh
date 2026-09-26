#!/usr/bin/env bash
# Removes the boot-time GPU clock cap and restores the default clocks.
#
# By default this only PRINTS the commands, ready to paste with sudo. It runs them itself only
# with --apply AND when it is already running as root:
#   packaging/uninstall-clockcap.sh                 # print the commands
#   sudo packaging/uninstall-clockcap.sh --apply    # run them
set -euo pipefail

APPLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --apply) APPLY=1 ;;
    -h | --help) echo "usage: uninstall-clockcap.sh [--apply]"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

UNIT=spark-pearl-clockcap.service
DST="/etc/systemd/system/$UNIT"

if [ "$APPLY" -eq 1 ] && [ "$(id -u)" -ne 0 ]; then
  printf 'refusing --apply without root; run: sudo %q --apply\n' "$0" >&2
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
  echo "# removing the clock cap:"
else
  echo "# commands to remove the clock cap; nothing was changed:"
fi
# Stopping the unit runs its ExecStop (nvidia-smi -rgc), which restores the default clocks.
step systemctl disable --now "$UNIT"
step rm -f "$DST"
step systemctl daemon-reload
echo "# check: nvidia-smi --query-gpu=clocks.sm,clocks.max.sm --format=csv"
