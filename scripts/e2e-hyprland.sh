#!/usr/bin/env bash
# e2e-hyprland.sh — P1 end-to-end gate on a REAL Hyprland session.
#
# Unlike scripts/headless-smoke.sh (which runs in a nested headless compositor),
# this drives the daemon on the session you are already logged into and proves
# the wallpaper actually reached the screen by sampling pixels from a grim
# screenshot. It restores nothing but the *empty* state: when it finishes the
# output has no OWE wallpaper, so run `swww img ...` (or your shell's own
# wallpaper command) afterwards if your desktop was using a wallpaper.
#
# Requirements: owed + owectl built (cargo build), hyprctl, grim, python3+PIL.
#
# Usage: scripts/e2e-hyprland.sh [--keep]
#   --keep   leave the last wallpaper applied instead of clearing at the end.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DAEMON="$ROOT/target/debug/owed"
CLI="$ROOT/target/debug/owectl"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

fail() { echo "FAIL: $*" >&2; exit 1; }
step() { echo; echo "== $* =="; }

for tool in hyprctl grim python3; do
  command -v "$tool" >/dev/null || fail "$tool is required"
done
[ -x "$DAEMON" ] || fail "$DAEMON not built (run: cargo build)"
[ -x "$CLI" ] || fail "$CLI not built (run: cargo build)"
[ -n "${WAYLAND_DISPLAY:-}" ] || fail "not in a Wayland session"

sandbox="$(mktemp -d)"
real_runtime="${XDG_RUNTIME_DIR:?}"
export XDG_RUNTIME_DIR="$sandbox/run"
export XDG_CONFIG_HOME="$sandbox/config"
export XDG_STATE_HOME="$sandbox/state"
export XDG_CACHE_HOME="$sandbox/cache"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/owe" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"

# The sandbox isolates OWE's socket, config and state — but the compositor's
# sockets live in the *real* runtime dir, and both the Wayland client (surface.rs)
# and `hyprctl` (the Hyprland backend) find them through XDG_RUNTIME_DIR. Without
# these symlinks the daemon legitimately reports "no outputs": there is no
# compositor to talk to. The wayland-N.lock file is deliberately not linked —
# clients only need the socket path.
for sock in "$real_runtime"/wayland-*; do
  case "$sock" in
    *.lock) continue ;;
  esac
  [ -e "$sock" ] && ln -sf "$sock" "$XDG_RUNTIME_DIR/"
done
[ -d "$real_runtime/hypr" ] && ln -sfn "$real_runtime/hypr" "$XDG_RUNTIME_DIR/hypr"

config="$sandbox/config.toml"
printf '%s\n' \
  'schema = 1' \
  '[shell]' \
  'backend = "hyprland"' \
  '[shell.caelestia]' \
  'mode = "daemon-drawn"' >"$config"

# A four-quadrant image: solid, saturated colours make pixel proof unambiguous
# (a photo would make "did our render land?" a judgement call).
image="$sandbox/quadrants.png"
python3 - "$image" <<'PY'
import sys
from PIL import Image
w, h = 1280, 720
img = Image.new("RGB", (w, h))
px = img.load()
quadrants = {
    (0, 0): (255, 0, 255),          # magenta
    (w // 2, 0): (0, 255, 0),       # green
    (0, h // 2): (0, 0, 255),       # blue
    (w // 2, h // 2): (255, 255, 0),  # yellow
}
for (x0, y0), colour in quadrants.items():
    for y in range(y0, y0 + h // 2):
        for x in range(x0, x0 + w // 2):
            px[x, y] = colour
img.save(sys.argv[1])
print(f"wrote {sys.argv[1]} ({w}x{h})")
PY

cleanup() {
  if [ "$KEEP" -eq 0 ] && [ -n "${daemon_pid:-}" ] && kill -0 "$daemon_pid" 2>/dev/null; then
    "$CLI" clear >/dev/null 2>&1 || true
    "$CLI" kill >/dev/null 2>&1 || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

step "starting the daemon"
"$DAEMON" --config "$config" >"$sandbox/daemon.log" 2>&1 &
daemon_pid=$!
for _ in $(seq 50); do
  [ -S "$XDG_RUNTIME_DIR/owe/socket" ] && break
  sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/owe/socket" ] || { cat "$sandbox/daemon.log"; fail "no socket at $XDG_RUNTIME_DIR/owe/socket"; }
echo "socket up, pid $daemon_pid"

step "owectl hello"
"$CLI" hello

step "owectl monitors (before)"
"$CLI" monitors
"$CLI" monitors | grep -q . || fail "no outputs enumerated on a live Hyprland session"

# Baseline BEFORE anything of ours is on screen: the pixel proof compares against it.
before="$sandbox/before.png"
grim "$before"

step "apply the quadrant image to every output"
"$CLI" set "$image"

step "owectl get eDP-1"
"$CLI" get --monitor eDP-1

step "owectl monitors (after)"
"$CLI" monitors

step "layer-shell assertions (TRD FR-CORE-5)"
# Ask the compositor where our surface actually landed, rather than trusting our
# own code: Background level, full output size, opaque, and no input region.
hyprctl layers -j >"$sandbox/layers.json"
python3 - "$sandbox/layers.json" <<'PY'
import json, sys

data = json.load(open(sys.argv[1]))
found = []
for monitor, node in data.items():
    for level, entries in node.get("levels", {}).items():
        for entry in entries:
            if entry.get("namespace") == "owe":
                found.append((monitor, level, entry))

if not found:
    print("FAIL: the daemon has no layer surface on any output")
    sys.exit(1)

problems = []
for monitor, level, entry in found:
    if level != "0":
        problems.append(f"{monitor}: on layer {level}, not 0 (background)")
    if entry.get("x") != 0 or entry.get("y") != 0:
        problems.append(f"{monitor}: at ({entry.get('x')},{entry.get('y')}), not anchored to all edges")
    if entry.get("alpha") != 1:
        problems.append(f"{monitor}: alpha {entry.get('alpha')}, expected opaque")
    print(
        f"  {monitor}: namespace=owe layer={level} (background)"
        f" {entry.get('w')}x{entry.get('h')} at ({entry.get('x')},{entry.get('y')}) alpha={entry.get('alpha')}"
    )

if problems:
    print("FAIL: layer-shell assertions failed:")
    print("\n".join("  " + p for p in problems))
    sys.exit(1)
print("  surface is a full-output background layer")
PY

step "screenshot proof"
sleep 0.5
# Two screenshots, not one. Fixed sample points are not a proof: the first run of
# this script failed while the wallpaper was perfectly rendered, because the sample
# points (screen quarters) sat under two 1252x704 windows. Layout-independent
# evidence instead: each quadrant colour must appear where it was absent before,
# its visible centre must fall in the matching screen quadrant, and `clear` must
# remove every one of those pixels again. That is true whether your desktop is
# empty or completely covered in windows.
after="$sandbox/after.png"
sleep 0.4
grim "$after"
python3 - "$before" "$after" <<'PY'
import sys
from PIL import Image

before = Image.open(sys.argv[1]).convert("RGB")
after = Image.open(sys.argv[2]).convert("RGB")
w, h = after.size

# (colour, expected screen quadrant) — quadrants of the source image, scaled with
# `cover` to the output, so each colour must land in the same corner.
quadrants = {
    "magenta": ((255, 0, 255), "top-left"),
    "green": ((0, 255, 0), "top-right"),
    "blue": ((0, 0, 255), "bottom-left"),
    "yellow": ((255, 255, 0), "bottom-right"),
}
before_px = before.load()
after_px = after.load()

# One pass, collecting counts and bounding boxes for every colour at once.
stats = {name: {"count": 0, "x0": w, "y0": h, "x1": -1, "y1": -1, "before": 0}
         for name in quadrants}
for y in range(h):
    for x in range(w):
        for name, (colour, _) in quadrants.items():
            if after_px[x, y] == colour:
                s = stats[name]
                s["count"] += 1
                s["x0"] = min(s["x0"], x)
                s["y0"] = min(s["y0"], y)
                s["x1"] = max(s["x1"], x)
                s["y1"] = max(s["y1"], y)
            if before_px[x, y] == colour:
                stats[name]["before"] += 1

problems = []
for name, (colour, quadrant) in quadrants.items():
    s = stats[name]
    if s["before"]:
        problems.append(
            f"{name}: {s['before']} pixels of this colour existed before we applied "
            "anything, so this screenshot cannot prove anything"
        )
        continue
    if s["count"] < 1000:
        problems.append(
            f"{name}: only {s['count']} pixels on screen (is the surface mapped?)"
        )
        continue
    cx = (s["x0"] + s["x1"]) // 2
    cy = (s["y0"] + s["y1"]) // 2
    want_left = "left" in quadrant
    want_top = "top" in quadrant
    if (cx < w // 2) != want_left or (cy < h // 2) != want_top:
        problems.append(
            f"{name}: visible centre ({cx},{cy}) is not in the {quadrant} quadrant"
        )
        continue
    print(
        f"  {name:8s} {s['count']:7d} px  bbox x{s['x0']}..{s['x1']} y{s['y0']}..{s['y1']}"
        f"  centre ({cx},{cy}) -> {quadrant}  ok"
    )

if problems:
    print("FAIL: the rendered wallpaper is not correctly on screen:")
    print("\n".join("  " + p for p in problems))
    sys.exit(1)
print("  all four quadrants rendered in the right places")
PY

step "pause / resume"
"$CLI" pause
"$CLI" resume

step "session state file"
state_file="$XDG_STATE_HOME/owe/session.json"
[ -f "$state_file" ] || fail "no session state written at $state_file"
cat "$state_file"
grep -q "$image" "$state_file" || fail "session state does not record the applied wallpaper"

step "clear"
"$CLI" clear
sleep 0.5
grim "$sandbox/after-clear.png"
python3 - "$sandbox/after-clear.png" <<'PY'
import sys
from PIL import Image
img = Image.open(sys.argv[1]).convert("RGB")
w, h = img.size
colour_px = 0
px = img.load()
for y in range(h):
    for x in range(w):
        if px[x, y] in ((255, 0, 255), (0, 255, 0), (0, 0, 255), (255, 255, 0)):
            colour_px += 1
if colour_px:
    print(f"FAIL: {colour_px} wallpaper pixels still on screen after clear")
    sys.exit(1)
print("  no wallpaper pixels remain after clear")
PY

step "kill"
"$CLI" kill
for _ in $(seq 50); do
  kill -0 "$daemon_pid" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$daemon_pid" 2>/dev/null; then
  fail "daemon did not exit after daemon.kill"
fi
wait "$daemon_pid" 2>/dev/null && exit_code=0 || exit_code=$?
[ "$exit_code" -eq 0 ] || fail "daemon exited with $exit_code"
echo "daemon exited 0"

step "daemon log"
grep -E "ipc client|wallpaper (applying|applied)|shutdown" "$sandbox/daemon.log" || cat "$sandbox/daemon.log"

echo
echo "PASS: real-Hyprland end to end (apply verified by pixels, clear, clean shutdown)"
