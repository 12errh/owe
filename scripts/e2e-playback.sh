#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
daemon="$root/target/debug/owed"
cli="$root/target/debug/owectl"
sandbox="$(mktemp -d)"
real_runtime="${XDG_RUNTIME_DIR:?}"
daemon_pid=""

cleanup() {
    if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
        "$cli" kill >/dev/null 2>&1 || true
        for _ in $(seq 1 50); do
            kill -0 "$daemon_pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf "$sandbox"
}
trap cleanup EXIT

export XDG_RUNTIME_DIR="$sandbox/run"
export XDG_CONFIG_HOME="$sandbox/config"
export XDG_STATE_HOME="$sandbox/state"
export XDG_CACHE_HOME="$sandbox/cache"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/owe" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"

for socket in "$real_runtime"/wayland-*; do
    case "$socket" in
        *.lock) continue ;;
    esac
    [ -e "$socket" ] && ln -sf "$socket" "$XDG_RUNTIME_DIR/"
done
[ -d "$real_runtime/hypr" ] && ln -sfn "$real_runtime/hypr" "$XDG_RUNTIME_DIR/hypr"

config="$sandbox/config.toml"
printf '%s\n' \
    'schema = 1' \
    '[shell]' \
    'backend = "hyprland"' \
    '[media]' \
    'backend = "auto"' >"$config"

"$daemon" --config "$config" >"$sandbox/daemon.log" 2>&1 &
daemon_pid=$!
for _ in $(seq 1 100); do
    [ -S "$XDG_RUNTIME_DIR/owe/socket" ] && break
    sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/owe/socket" ] || {
    printf '%s\n' 'daemon did not create its socket' >&2
    exit 1
}

check_advances() {
    local first="$1"
    local second="$2"
    local kind="$3"
    python3 - "$first" "$second" "$kind" <<'PY'
import json
import sys

first = json.load(open(sys.argv[1]))["stats"][0]
second = json.load(open(sys.argv[2]))["stats"][0]
kind = sys.argv[3]
assert first["kind"] == kind, first
assert first["playing"] is True, first
assert second["frames_presented"] > first["frames_presented"], (first, second)
assert second["fps"] > 0, second
print(
    f"{kind}: {first['frames_presented']} -> {second['frames_presented']} frames, "
    f"{second['fps']:.2f} fps, {second['decode']} decode"
)
PY
}

gif="$root/crates/owe-media/tests/fixtures/anim-3frame.gif"
"$cli" set "$gif" >/dev/null
sleep 1
"$cli" --json stats >"$sandbox/gif-first.json"
sleep 0.5
"$cli" --json stats >"$sandbox/gif-second.json"
check_advances "$sandbox/gif-first.json" "$sandbox/gif-second.json" animated-image
"$cli" playback pause --monitor eDP-1 >/dev/null
"$cli" playback play --monitor eDP-1 >/dev/null
"$cli" playback seek 0.1 --monitor eDP-1 >/dev/null
"$cli" playback loop 0 0.2 --monitor eDP-1 >/dev/null
"$cli" clear eDP-1 >/dev/null

video="$root/crates/owe-media/tests/fixtures/video-5frame.mp4"
"$cli" set "$video" >/dev/null
sleep 1
"$cli" --json stats >"$sandbox/video-first.json"
sleep 0.5
"$cli" --json stats >"$sandbox/video-second.json"
check_advances "$sandbox/video-first.json" "$sandbox/video-second.json" video
"$cli" playback seek 0.1 --monitor eDP-1 >/dev/null
"$cli" playback loop 0 0.2 --monitor eDP-1 >/dev/null
"$cli" clear eDP-1 >/dev/null

"$cli" kill >/dev/null
wait "$daemon_pid"
daemon_pid=""
printf '%s\n' 'PASS: live GIF/video playback, controls, clear, and shutdown'
