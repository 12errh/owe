#!/usr/bin/env bash
# Record Hyprland `socket2` event streams, and build the deterministic 10k-event
# fixture the P3 replay gate measures (IMPLEMENTATION-PLAN, Phase 3 exit gate).
#
#   ./scripts/record-shell-events.sh                 # passive capture, 20 s
#   ./scripts/record-shell-events.sh --seconds 60    # longer capture
#   ./scripts/record-shell-events.sh --synthesize    # rebuild the 10k fixture
#
# ## Why there are two fixtures and not one
#
# A passive capture of an idle desktop produces a handful of events: the socket
# only speaks when something happens. Ten thousand *real* events would mean ten
# thousand real window switches — on the machine its owner is working on. So:
#
#   .../fixtures/events/socket2-session.log   verbatim `--seconds` capture
#   .../fixtures/events/socket2-10k.log       built by `--synthesize`
#
# The 10k stream is generated from the committed session log plus the documented
# event catalogue, and the manifest in its header records exactly which lines are
# verbatim and which are generated. The parser test asserts the two properties a
# synthetic stream *can* prove — 10 000 lines in, 10 000 typed events out, no
# malformed line, indistinguishable round-trip — and the sample test asserts the
# meanings by hand. Neither claims to be the other.
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The parser lives in owe-shell-hyprland (socket2 is Hyprland's protocol), so its
# fixtures live beside it, the way the monitor JSON already does.
fixtures="$root/crates/owe-shell-hyprland/tests/fixtures/events"
mode="record"
seconds=20

while [ $# -gt 0 ]; do
  case "$1" in
    --seconds)
      seconds="${2:?--seconds needs a number}"
      shift 2
      ;;
    --synthesize)
      mode="synthesize"
      shift
      ;;
    *)
      echo "record-shell-events: unknown argument $1" >&2
      exit 2
      ;;
  esac
done

mkdir -p "$fixtures"

socket_path() {
  local sig="${HYPRLAND_INSTANCE_SIGNATURE:-}"
  [ -n "$sig" ] || return 1
  local path="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/hypr/$sig/.socket2.sock"
  [ -S "$path" ] || return 1
  echo "$path"
}

if [ "$mode" = "record" ]; then
  socket="$(socket_path)" || {
    echo "record-shell-events: no Hyprland socket2 found." >&2
    echo "  HYPRLAND_INSTANCE_SIGNATURE must be set and the session running." >&2
    exit 2
  }
  out="$fixtures/socket2-session.log"
  echo "record-shell-events: listening on $socket for ${seconds}s"
  # Passive: connect and read. No interaction, so nothing on screen changes.
  python3 - "$socket" "$seconds" "$out" <<'PY'
import socket, sys, time

path, seconds, out = sys.argv[1], float(sys.argv[2]), sys.argv[3]
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(1.0)
sock.connect(path)

lines, deadline = [], time.monotonic() + seconds
buffer = b""
while time.monotonic() < deadline:
    try:
        chunk = sock.recv(65536)
    except socket.timeout:
        continue
    if not chunk:
        break
    buffer += chunk
    while b"\n" in buffer:
        line, buffer = buffer.split(b"\n", 1)
        text = line.decode("utf-8", "replace").rstrip("\r")
        if text.strip():
            lines.append(text)

with open(out, "w", encoding="utf-8") as handle:
    handle.write("\n".join(lines) + ("\n" if lines else ""))

kinds = {}
for line in lines:
    kinds[line.split(">>", 1)[0]] = kinds.get(line.split(">>", 1)[0], 0) + 1
print(f"record-shell-events: captured {len(lines)} event(s) into {out}")
for kind, count in sorted(kinds.items(), key=lambda item: -item[1]):
    print(f"  {count:6d}  {kind}")
PY
  exit $?
fi

# --- synthesize: the deterministic 10k stream --------------------------------
session="$fixtures/socket2-session.log"
out="$fixtures/socket2-10k.log"
[ -f "$session" ] || {
  echo "record-shell-events: $session is missing; run a capture first" >&2
  exit 2
}

python3 - "$session" "$out" "$root" <<'PY'
import datetime, hashlib, sys

session, out, root = sys.argv[1], sys.argv[2], sys.argv[3]
TOTAL = 10_000

with open(session, encoding="utf-8") as handle:
    recorded = [line for line in (line.rstrip("\n") for line in handle) if line.strip()]

# Documented socket2 events, with payloads in the shape Hyprland emits. Kept in
# one table so the fixture reads as a catalogue rather than as noise, and so a
# reader can check a line against Hyprland's own docs.
CYCLE = [
    "activewindow>>kitty,~/Projects/owe — nvim",
    "activewindow>>firefox,Wikipedia, the free encyclopedia",
    "activewindow>>,",                       # no window focused
    "activewindowv2>>55b1c0e0e2a0",
    "activewindowv2>>",
    "fullscreen>>0",
    "fullscreen>>1",
    "workspace>>1",
    "workspace>>2,web",
    "workspacev2>>2,web",
    "focusedmon>>eDP-1,2",
    "monitoradded>>HDMI-A-1",
    "monitoraddedv2>>1,HDMI-A-1,Dell Inc. DELL U2720Q",
    "monitorremoved>>HDMI-A-1",
    "openwindow>>55b1c0e0e2a0,2,kitty,~/Projects/owe — nvim",
    "closewindow>>55b1c0e0e2a0",
    "windowtitle>>55b1c0e0e2a0,~/Projects/owe — nvim",
    "submap>>resize",
    "submap>>",
    "openlayer>>notifications",
    "closelayer>>notifications",
    "activelayout>>kitty,English (US)",
    "configreloaded>>",
    "createworkspace>>3",
    "destroyworkspace>>3",
    "renameworkspace>>2,web",
    "movewindow>>55b1c0e0e2a0,2",
    "changefloatingmode>>55b1c0e0e2a0,1",
    "urgent>>55b1c0e0e2a0",
    "minimize>>55b1c0e0e2a0,1",
    "togglegroup>>55b1c0e0e2a0",
    "pin>>55b1c0e0e2a0,1",
]

# The stream is the recorded lines (in their original order, so the fixture keeps
# whatever real interleaving the session produced) followed by the catalogue in
# round-robin order. Deterministic: same session log + same script = same bytes.
stream = list(recorded)
index = 0
while len(stream) < TOTAL:
    stream.append(CYCLE[index % len(CYCLE)])
    index += 1
stream = stream[:TOTAL]

kinds = {}
for line in stream:
    kind = line.split(">>", 1)[0]
    kinds[kind] = kinds.get(kind, 0) + 1

body = "\n".join(stream) + "\n"
digest = hashlib.sha256(body.encode("utf-8")).hexdigest()

header = [
    "# socket2 event stream for the P3 replay gate (10 000 events).",
    "#",
    "# Generated by scripts/record-shell-events.sh --synthesize — do not hand-edit.",
    "# Lines beginning with '#' are the manifest and are not part of the stream:",
    "# the replay harness skips them, and asserts that exactly one file in this",
    "# directory has one.",
    "#",
    f"# source-session: socket2-session.log ({len(recorded)} verbatim lines, in order)",
    f"# generated-lines: {TOTAL - len(recorded)} (catalogue round-robin, deterministic)",
    f"# total-events: {TOTAL}",
    f"# sha256: {digest}",
    f"# generated-at: {datetime.datetime.now(datetime.UTC).strftime('%Y-%m-%dT%H:%M:%SZ')}",
    "#",
    "# expected-histogram (kind: count) — the replay test recomputes this and fails",
    "# on any difference, so a parser change that reclassifies a line is visible:",
]
for kind, count in sorted(kinds.items()):
    header.append(f"#   {kind}: {count}")

with open(out, "w", encoding="utf-8") as handle:
    handle.write("\n".join(header) + "\n")
    handle.write(body)

print(f"record-shell-events: wrote {TOTAL} events ({len(recorded)} verbatim) to {out}")
print(f"  sha256 {digest}")
print(f"  kinds: {len(kinds)}")
PY
