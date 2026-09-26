#!/usr/bin/env bash
# Soak logger for the DGX Spark power-off hunt. Every 10 s it appends one CSV row with the GPU
# power, SM clock, GPU temperature and clock event reasons (nvidia-smi) plus the hottest acpitz
# zone. It only reads telemetry: no root, no CUDA context, it does not start or stop anything.
# Run it in a second terminal next to the miner.
#
# usage: bench/soak-log.sh [--interval S] [--duration S] [--out FILE]
#        bench/soak-log.sh --summarize FILE
#
# Default output: docs/benchmarks/soak-<UTC timestamp>.csv. Ctrl-C (or --duration) stops it and
# prints a summary: max power, min clock, max temperatures, clock event reasons, and every gap
# longer than 20 s between rows (a suspected power-off). Rows are appended one by one, so a
# power-off loses at most one interval; a log whose last session has no "# end ... clean" line
# did not stop cleanly. After a power-off, run --summarize on the log, or pass the same --out to
# keep logging into it (the gap then shows up in the summary).
set -euo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)

INTERVAL=10
DURATION=0
OUT=""
SUMMARIZE=""

usage() {
  echo "usage: bench/soak-log.sh [--interval S] [--duration S] [--out FILE]"
  echo "       bench/soak-log.sh --summarize FILE"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --interval) shift; INTERVAL=${1:-} ;;
    --duration) shift; DURATION=${1:-} ;;
    --out) shift; OUT=${1:-} ;;
    --summarize) shift; SUMMARIZE=${1:-} ;;
    -h | --help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

for n in "$INTERVAL" "$DURATION"; do
  case "$n" in
    '' | *[!0-9]*) echo "--interval and --duration take whole seconds" >&2; exit 2 ;;
  esac
done
[ "$INTERVAL" -ge 1 ] || { echo "--interval must be at least 1 s" >&2; exit 2; }
GAP_S=$((INTERVAL * 2 > 20 ? INTERVAL * 2 : 20))

HEADER="epoch_s,timestamp,power_draw_w,clocks_sm_mhz,temperature_gpu_c,clocks_event_reasons_active,acpitz_max_c"

summarize() {
  awk -F, -v gap="$GAP_S" '
    /^# start/ {
      # A new session after one that never wrote its "# end" line: the logger died with the box.
      if (rows > 0 && !clean) {
        unclean++
        ul = ul sprintf("  session stopped without a clean end after %s: suspected power-off (or kill)\n", prevts)
      }
      clean = 0; brk = 1; sessions++; next
    }
    /^# end .* clean/ { clean = 1; brk = 1; next }
    /^#/ || $1 == "epoch_s" || NF < 7 { next }
    {
      e = $1 + 0; rows++
      if (rows == 1) first = e
      if (rows > 1 && !brk && e - prev > gap) {
        gaps++
        gl = gl sprintf("  %d s without a row after %s: suspected power-off\n", e - prev, prevts)
      }
      brk = 0; prev = e; prevts = ($2 != "" ? $2 : "epoch " $1)
      if ($3 == "" || $3 ~ /N\/A|ERR/) { missing++ } else {
        p = $3 + 0
        if (!hp || p > maxp) { maxp = p; maxpts = $2; hp = 1 }
        sump += p; np++
      }
      if ($4 != "" && $4 !~ /N\/A/) {
        c = $4 + 0
        if (!hc || c < minc) { minc = c; mincts = $2; hc = 1 }
        if ($3 + 0 >= 30 && (!hl || c < minl)) { minl = c; hl = 1 }
      }
      if ($5 != "" && $5 + 0 > maxt) maxt = $5 + 0
      if ($6 != "" && $6 !~ /^0x0+$/ && $6 !~ /N\/A/) { thr++; reasons[$6]++ }
      if ($7 != "" && $7 + 0 > maxa) maxa = $7 + 0
    }
    END {
      if (rows == 0) { print "no samples"; exit }
      printf "rows %d over %.1f h (%d session(s))\n", rows, (prev - first) / 3600, sessions
      if (hp) printf "max power %.2f W at %s; mean %.2f W\n", maxp, maxpts, sump / np
      if (hc) printf "min SM clock %d MHz at %s", minc, mincts
      if (hl) printf "; min under load (>= 30 W) %d MHz", minl
      if (hc) printf "\n"
      printf "max GPU temperature %d C; max acpitz %.1f C\n", maxt, maxa
      printf "rows with active clock event reasons: %d\n", thr
      for (r in reasons) printf "  %s x%d\n", r, reasons[r]
      if (missing) printf "rows without nvidia-smi data: %d\n", missing
      if (gaps) printf "GAPS > %d s: %d\n%s", gap, gaps, gl; else printf "no gap > %d s\n", gap
      if (unclean) printf "SESSIONS WITHOUT A CLEAN END: %d\n%s", unclean, ul
      printf "last session ended cleanly: %s\n", clean ? "yes" : "NO (power-off, kill, or still running)"
    }' "$1"
}

if [ -n "$SUMMARIZE" ]; then
  [ -f "$SUMMARIZE" ] || { echo "no such file: $SUMMARIZE" >&2; exit 1; }
  summarize "$SUMMARIZE"
  exit 0
fi

command -v nvidia-smi >/dev/null || { echo "nvidia-smi not found" >&2; exit 1; }
if [ -z "$OUT" ]; then
  mkdir -p "$REPO/docs/benchmarks"
  OUT=$REPO/docs/benchmarks/soak-$(date -u +%Y%m%dT%H%M%SZ).csv
fi
[ -s "$OUT" ] || echo "$HEADER" >>"$OUT"

acpitz_max() {
  local z max=""
  local t
  for z in /sys/class/thermal/thermal_zone*; do
    [ "$(cat "$z/type" 2>/dev/null)" = acpitz ] || continue
    t=$(cat "$z/temp" 2>/dev/null) || continue
    case "$t" in '' | *[!0-9-]*) continue ;; esac
    if [ -z "$max" ] || [ "$t" -gt "$max" ]; then max=$t; fi
  done
  [ -n "$max" ] && awk -v m="$max" 'BEGIN { printf "%.1f", m / 1000 }'
  return 0
}

sample() {
  local q
  q=$(timeout 5 nvidia-smi \
    --query-gpu=timestamp,power.draw,clocks.sm,temperature.gpu,clocks_event_reasons.active \
    --format=csv,noheader,nounits 2>/dev/null | head -n 1 | sed 's/, */,/g') || q=""
  [ -n "$q" ] || q=",ERR,,,"
  printf '%s,%s,%s\n' "$(date +%s)" "$q" "$(acpitz_max)" >>"$OUT"
}

STARTED=0
finish() {
  if [ "$STARTED" -eq 1 ]; then
    echo "# end $(date +%s) clean" >>"$OUT"
    echo
    echo "log: $OUT"
    summarize "$OUT"
  fi
}
trap finish EXIT
trap 'exit 0' INT TERM

DRIVER=$(timeout 5 nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null | head -n 1 || true)
echo "# start $(date +%s) $(date -u +%FT%TZ) host=$(hostname) driver=${DRIVER:-?} interval_s=$INTERVAL" >>"$OUT"
STARTED=1
echo "logging every ${INTERVAL} s to $OUT (Ctrl-C to stop)"
START=$(date +%s)
while :; do
  sample
  tail -n 1 "$OUT"
  if [ "$DURATION" -gt 0 ] && [ $(($(date +%s) - START)) -ge "$DURATION" ]; then
    break
  fi
  sleep "$INTERVAL"
done
