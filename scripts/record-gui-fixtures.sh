#!/usr/bin/env bash
# Record a real daemon's IPC replies into the GUI's replay fixture.
#
# WHY THIS EXISTS
#   The GUI's unit tests use a hand-written stub daemon, which proves the GUI sends
#   and reads the shapes *we believe* the daemon speaks. This script closes the
#   other half of that gap: it captures the real `owed` binary's actual replies
#   (headless, no compositor needed) and commits them, so the replay test asserts
#   against bytes the daemon really produced. If the daemon's wire shape drifts,
#   re-running this and committing the diff is the intended workflow — a reviewable
#   diff of real traffic instead of a silently updated stub.
#
# WHAT IT RECORDS
#   hello, library.scan, library.list, library.thumb, outputs.list — every method
#   the GUI v1 screens call.
#
# THE `$FIXTURE_DIR` PLACEHOLDER
#   The recorded thumbnail path points into the recorder's temporary directory, so
#   the reply is rewritten to `$FIXTURE_DIR/thumb.png` and the PNG itself is copied
#   next to the fixture. The replay daemon substitutes the real fixture directory
#   at serve time, which is what lets the thumbnail path be tested end to end.

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
fixtures="$root/app/src-tauri/tests/fixtures"
mkdir -p "$fixtures"

echo "building owed + owectl…"
cargo build --manifest-path "$root/Cargo.toml" -p owed -p owectl

tmp=$(mktemp -d)
owed_pid=""
cleanup() {
  if [ -n "$owed_pid" ]; then kill "$owed_pid" 2>/dev/null || true; wait "$owed_pid" 2>/dev/null || true; fi
  rm -rf "$tmp"
}
trap cleanup EXIT

# A sandbox of our own: never the developer's real daemon, socket, or state.
export XDG_RUNTIME_DIR="$tmp/run"
export XDG_STATE_HOME="$tmp/state"
export XDG_CACHE_HOME="$tmp/cache"
export XDG_CONFIG_HOME="$tmp/config"
export XDG_DATA_HOME="$tmp/data"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME/owe" "$XDG_DATA_HOME"
chmod 700 "$XDG_RUNTIME_DIR"

walls="$tmp/walls"
mkdir -p "$walls"
# Two real PNGs from the committed golden set: the recorder should index real files
# with real dimensions, not fixtures it also invented.
cp "$root/tests/golden/transitions/fade-1280x720.png" "$walls/aurora.png"
cp "$root/tests/golden/transitions/wipe-1920x1080.png" "$walls/forest.png"

config="$XDG_CONFIG_HOME/owe/config.toml"
cat > "$config" <<EOF
schema = 1

[library]
paths = ["$walls"]
thumbnail_size = 256
rescan_interval_secs = 0
EOF

owed="$root/target/debug/owed"
owectl="$root/target/debug/owectl"

echo "starting owed (headless)…"
"$owed" --config "$config" > "$tmp/owed.log" 2>&1 &
owed_pid=$!

socket="$XDG_RUNTIME_DIR/owe/socket"
for _ in $(seq 1 100); do
  [ -S "$socket" ] && break
  if ! kill -0 "$owed_pid" 2>/dev/null; then
    echo "owed exited before creating its socket; log:" >&2
    cat "$tmp/owed.log" >&2
    exit 1
  fi
  sleep 0.1
done
if [ ! -S "$socket" ]; then
  echo "owed never created $socket; log:" >&2
  cat "$tmp/owed.log" >&2
  exit 1
fi

echo "recording replies…"
"$owectl" hello --json                > "$tmp/hello.json"
"$owectl" library scan --json         > "$tmp/scan.json"
"$owectl" library list --json         > "$tmp/list.json"
"$owectl" monitors --json             > "$tmp/outputs.json"

item_id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["items"][0]["id"])' "$tmp/list.json")
"$owectl" library thumb "$item_id" --json > "$tmp/thumb.json"

thumb_path=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["path"])' "$tmp/thumb.json")
if [ ! -f "$thumb_path" ]; then
  echo "the daemon reported a thumbnail at $thumb_path but wrote nothing there" >&2
  exit 1
fi
cp "$thumb_path" "$fixtures/thumb.png"

version=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["server_version"])' "$tmp/hello.json")

python3 - "$fixtures/daemon-session.json" "$version" "$tmp" <<'PY'
import json, sys

out_path, version, tmp = sys.argv[1], sys.argv[2], sys.argv[3]

def load(name):
    with open(f"{tmp}/{name}") as handle:
        return json.load(handle)

thumb = load("thumb.json")
# The recorder's temp path is meaningless to a future test run; the PNG travels
# with the fixture and the daemon's own reply is preserved apart from this one
# substitution.
thumb["path"] = "$FIXTURE_DIR/thumb.png"

fixture = {
    "recorded_with": f"owed {version}",
    "note": (
        "Recorded by scripts/record-gui-fixtures.sh against the real owed binary "
        "running headless (no compositor). Replies are verbatim daemon output; "
        "$FIXTURE_DIR is substituted by the replay daemon at serve time. Re-run the "
        "script and commit the diff when the wire shape changes."
    ),
    "replies": {
        "hello": load("hello.json"),
        "library.scan": load("scan.json"),
        "library.list": load("list.json"),
        "library.thumb": thumb,
        "outputs.list": load("outputs.json"),
    },
}

with open(out_path, "w") as handle:
    json.dump(fixture, handle, indent=2, sort_keys=False)
    handle.write("\n")
PY

echo "wrote $fixtures/daemon-session.json and $fixtures/thumb.png (owed $version)"
