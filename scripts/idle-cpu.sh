#!/usr/bin/env bash
# idle-cpu.sh — NFR-PERF-1: a static wallpaper must cost ~nothing while idle.
#
# The PRD claims a static wallpaper schedules **zero frames**. This measures the
# only thing that can be measured from outside the daemon: CPU time. A daemon that
# presented one frame every 50 ms would burn measurable CPU (a wakeup, a render,
# an upload, a commit each time); a daemon that presented once and stopped cannot.
#
# It needs a live Wayland session to actually put a wallpaper up — that is the
# point, since "idle" must mean "idle with a wallpaper on screen", not "idle with
# nothing to do".
#
# Usage: scripts/idle-cpu.sh [seconds]   (default 60)
#
# Exit codes: 0 = pass, 1 = the daemon burned more CPU than a static wallpaper
# should, 2 = could not run the measurement (no session, not built).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DAEMON="$ROOT/target/debug/owed"
CLI="$ROOT/target/debug/owectl"
SECONDS_TO_SAMPLE="${1:-60}"
# NFR-PERF-1 as written in the TRD: under 1% of one core while idle on the
# Reference Profile. 1% of 60 s = 0.6 s = 60 ticks at 100 Hz.
TICK_BUDGET="${TICK_BUDGET:-60}"

fail() { echo "FAIL: $*" >&2; exit 1; }
step() { echo; echo "== $* =="; }

[ -x "$DAEMON" ] || fail "build first: cargo build"
[ -x "$CLI" ] || fail "build first: cargo build"
[ -n "${WAYLAND_DISPLAY:-}" ] || { echo "SKIP: not in a Wayland session"; exit 2; }

sandbox="$(mktemp -d)"
real_runtime="$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR="$sandbox/run"
export XDG_CONFIG_HOME="$sandbox/config"
export XDG_STATE_HOME="$sandbox/state"
export XDG_CACHE_HOME="$sandbox/cache"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/owe" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"
for s in "$real_runtime"/wayland-*; do
  case "$s" in *.lock) continue ;; esac
  [ -e "$s" ] && ln -sf "$s" "$XDG_RUNTIME_DIR/"
done
[ -d "$real_runtime/hypr" ] && ln -sfn "$real_runtime/hypr" "$XDG_RUNTIME_DIR/hypr"

# A real image, not a 2x2: the wallpaper must be decoded, GPU-scaled and presented
# once, exactly as a user would leave it.
image="$sandbox/wallpaper.png"
if [ -f "$HOME/Pictures/wallpapers/$(ls "$HOME/Pictures/wallpapers" 2>/dev/null | head -1)" ]; then
  image="$HOME/Pictures/wallpapers/$(ls "$HOME/Pictures/wallpapers" | head -1)"
  echo "using an existing wallpaper: $image"
else
  python3 - "$image" <<'PY'
import sys
from PIL import Image, ImageDraw
w, h = 1920, 1080
img = Image.new("RGB", (w, h), (18, 18, 26))
draw = ImageDraw.Draw(img)
for i in range(0, w, 40):
    draw.line([(i, 0), (i, h)], fill=(30 + (i % 200), 40, 90), width=3)
img.save(sys.argv[1])
PY
  echo "generated a test wallpaper: $image"
fi

cleanup() {
  [ -n "${daemon_pid:-}" ] && kill -0 "$daemon_pid" 2>/dev/null && {
    "$CLI" clear >/dev/null 2>&1 || true
    "$CLI" kill >/dev/null 2>&1 || true
    wait "$daemon_pid" 2>/dev/null || true
  }
}
trap cleanup EXIT

step "start the daemon and apply a wallpaper"
"$DAEMON" >"$sandbox/daemon.log" 2>&1 &
daemon_pid=$!
for _ in $(seq 50); do
  [ -S "$XDG_RUNTIME_DIR/owe/socket" ] && break
  sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/owe/socket" ] || { cat "$sandbox/daemon.log"; fail "daemon did not start"; }
"$CLI" set "$image"
"$CLI" monitors

# Everything above is setup: settle before measuring so the startup work is not
# counted as idle cost.
sleep 2

read_cpu_ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }
read_rss_kb() { awk '/VmRSS/ {print $2}' "/proc/$1/status"; }

hz="$(getconf CLK_TCK)"
ticks_before="$(read_cpu_ticks "$daemon_pid")"
rss_before="$(read_rss_kb "$daemon_pid")"
threads_before="$(awk '/Threads/ {print $2}' "/proc/$daemon_pid/status")"
sockets_before="$(ls -1 /proc/$daemon_pid/fd 2>/dev/null | wc -l)"

step "sampling ${SECONDS_TO_SAMPLE}s of idle (do not touch the machine)"
for _ in $(seq "$SECONDS_TO_SAMPLE"); do sleep 1; done

ticks_after="$(read_cpu_ticks "$daemon_pid")"
rss_after="$(read_rss_kb "$daemon_pid")"
threads_after="$(awk '/Threads/ {print $2}' "/proc/$daemon_pid/status")"
sockets_after="$(ls -1 /proc/$daemon_pid/fd 2>/dev/null | wc -l)"

cpu_ticks=$((ticks_after - ticks_before))
cpu_seconds="$(awk -v t="$cpu_ticks" -v hz="$hz" 'BEGIN {printf "%.3f", t / hz}')"
cpu_percent="$(awk -v t="$cpu_ticks" -v hz="$hz" -v s="$SECONDS_TO_SAMPLE" \
  'BEGIN {printf "%.3f", 100 * (t / hz) / s}')"

cat <<EOF

== result ==
  sample window      ${SECONDS_TO_SAMPLE}s
  CPU time used      ${cpu_seconds}s  (${cpu_ticks} ticks at ${hz} Hz)
  CPU load           ${cpu_percent}% of one core
  RSS                $((rss_before / 1024)) MiB -> $((rss_after / 1024)) MiB
  threads            ${threads_before} -> ${threads_after}
  open descriptors   ${sockets_before} -> ${sockets_after}
  budget             ${TICK_BUDGET} ticks (1% of one core at ${SECONDS_TO_SAMPLE}s)
EOF

status=0
if [ "$cpu_ticks" -gt "$TICK_BUDGET" ]; then
  echo "FAIL: ${cpu_ticks} ticks of CPU while idle exceeds the ${TICK_BUDGET}-tick budget;"
  echo "      a static wallpaper must schedule no frames at all."
  status=1
fi
if [ "$threads_before" != "$threads_after" ]; then
  echo "FAIL: thread count changed while idle (${threads_before} -> ${threads_after}) — a leak or a spinner."
  status=1
fi
if [ "$sockets_after" -gt "$sockets_before" ]; then
  echo "FAIL: open descriptors grew while idle (${sockets_before} -> ${sockets_after})."
  status=1
fi
if [ "$rss_after" -gt $((rss_before + 2048)) ]; then
  echo "NOTE: RSS grew by $(( (rss_after - rss_before) / 1024 )) MiB while idle — worth a look."
fi

[ "$status" -eq 0 ] && echo
[ "$status" -eq 0 ] && echo "PASS: static wallpaper idle cost is within NFR-PERF-1"
step "daemon log"
cat "$sandbox/daemon.log"
exit "$status"
