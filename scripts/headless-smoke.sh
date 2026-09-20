#!/usr/bin/env bash
# Headless-Wayland smoke test (docs/IMPLEMENTATION-PLAN.md, P0 gate).
#
# What it proves today (P0):
#   1. a headless wlroots compositor can be started in a sandbox
#   2. a Wayland client can connect and enumerate globals (layer-shell included)
#   3. `owed` validates config, serves IPC, answers `hello`, and shuts down cleanly
#
# What it deliberately does NOT prove yet: surface creation and rendering. There
# is no surface code before P1, and asserting "a surface appeared" without one
# would be a lie. P1 extends this script with a screencopy check.
#
# Two modes:
#   ./scripts/headless-smoke.sh            headless verdict (needs: sway, wayland-utils)
#   ./scripts/headless-smoke.sh --session  same assertions against the compositor
#                                          already running (needs: wayland-utils)
#
# The second mode exists so the maintainer can run the identical checks on real
# Hyprland without installing sway or needing root. CI uses the headless mode,
# because a deterministic compositor is what makes the job meaningful there.

set -euo pipefail

# Captured before the sandbox redirects XDG_RUNTIME_DIR: the session-mode
# compositor socket is discovered relative to the *real* runtime directory.
orig_runtime_dir="${XDG_RUNTIME_DIR:-}"
orig_wayland_display="${WAYLAND_DISPLAY:-}"

mode="${1:-headless}"
case "$mode" in
  headless) ;;
  --session) ;;
  *) echo "usage: $0 [--session]" >&2; exit 2 ;;
esac

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d)"
owe="$root/target/debug/owed"
ctl="$root/target/debug/owectl"

compositor_pid=""
daemon_pid=""

cleanup() {
  [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null || true
  [ -n "$compositor_pid" ] && kill "$compositor_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  echo "[smoke] FAIL: $*" >&2
  [ -f "$work/sway.log" ] && { echo "--- sway.log ---"; tail -30 "$work/sway.log"; }
  [ -f "$work/owed.log" ] && { echo "--- owed.log ---"; tail -30 "$work/owed.log"; }
  # An empty sway.log is itself a diagnostic: it means the compositor process
  # never got far enough to log, or never started. Show whether the process is
  # even alive and whether the binary could run at all, so CI failures stop
  # being a mystery wrapped in a missing file (three CI runs needed for this).
  if [ -n "${compositor_pid:-}" ] && kill -0 "$compositor_pid" 2>/dev/null; then
    echo "--- compositor process $compositor_pid is alive but produced no socket ---"
    ps -p "$compositor_pid" -o pid,stat,wchan:30,etime,cmd 2>/dev/null || true
    # A process in Z (zombie) state means its parent (this script) never reaped
    # it; D means uninterruptible IO. Both say "dead, not slow". S/R at 10+s
    # with no output says hung-in-startup, and the window may just be too short
    # on a cold runner (fontconfig cache building alone can take >10s).
  else
    echo "--- compositor process is gone ---"
  fi
  echo "--- environment ---"
  echo "WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-unset} XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
  command -v sway >/dev/null 2>&1 && echo "sway: $(sway --version 2>&1 | head -1)" || echo "sway: NOT FOUND"
  [ -d /usr/lib/wlroots ] 2>/dev/null && ls /usr/lib/wlroots 2>/dev/null | head -3
  exit 1
}

# --- sandbox -----------------------------------------------------------------
export XDG_RUNTIME_DIR="$work/run"
export XDG_CONFIG_HOME="$work/config"
export XDG_CACHE_HOME="$work/cache"
export XDG_STATE_HOME="$work/state"
export HOME="$work/home"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_STATE_HOME" "$HOME"
chmod 700 "$XDG_RUNTIME_DIR"

[ -x "$owe" ] || fail "missing $owe — run: cargo build --workspace"
[ -x "$ctl" ] || fail "missing $ctl — run: cargo build --workspace"

# --- compositor --------------------------------------------------------------
if [ "$mode" = "--session" ]; then
  [ -n "$orig_wayland_display" ] || fail "--session needs a running Wayland session"
  # An absolute WAYLAND_DISPLAY is a valid libwayland value and keeps the client
  # able to find the real compositor after the sandbox moves XDG_RUNTIME_DIR.
  case "$orig_wayland_display" in
    /*) wayland_socket="$orig_wayland_display" ;;
    *)  wayland_socket="$orig_runtime_dir/$orig_wayland_display" ;;
  esac
  [ -S "$wayland_socket" ] || fail "no compositor socket at $wayland_socket"
  export WAYLAND_DISPLAY="$wayland_socket"
  echo "[smoke] using the running session's compositor: $wayland_socket"
else
  command -v sway >/dev/null 2>&1 \
    || fail "sway is not installed (use --session to test against the running compositor)"
  echo "[smoke] starting headless wlroots compositor (sway, headless backend, pixman renderer)"
  # CI runners have no Xwayland binary and a headless compositor needs none;
  # leaving Xwayland on made sway abort before creating its socket (CI run
  # 35461889214). Disabled in scripts/headless-sway.conf (`xwayland disable`) —
  # the config is the documented lever, not WLR_XWAYLAND, which points at a
  # binary path rather than an on/off switch.
  WLR_BACKENDS=headless \
  WLR_RENDERER=pixman \
  WLR_LIBINPUT_NO_DEVICES=1 \
    sway -c "$root/scripts/headless-sway.conf" >"$work/sway.log" 2>&1 &
  compositor_pid=$!

  # 30s, not 10s: a cold CI runner builds fontconfig caches, links the seat
  # manager, and warms wlroots' backends on first start — runs 4 and 5 found the
  # compositor *alive but socket-less* at exactly 10s, which is a slow start at
  # least as likely as a hang. If it still fails at 30s, the failure diagnostics
  # now include the process state (zombie? uninterruptible IO?) to say which.
  #
  # Any socket name counts (wayland-0, wayland-1, …) — hardcoding one name made
  # this loop blind to what the compositor actually did (CI runs 4–7 all waited
  # for wayland-0 while the truth about the process was in the diagnostics).
  wayland_socket=""
  for _ in $(seq 1 300); do
    for candidate in "$XDG_RUNTIME_DIR"/wayland-*; do
      # The `.lock` companion file is not the socket.
      case "$candidate" in *.lock) continue ;; esac
      if [ -S "$candidate" ]; then
        wayland_socket="$candidate"
        break 2
      fi
    done
    sleep 0.1
  done
  [ -n "$wayland_socket" ] || fail "compositor never created any wayland-* socket"
  echo "[smoke] compositor up: $wayland_socket"
  # Session mode exports this above; headless mode must too, or every client
  # after this point (wayland-info, owed) has no display to connect to. Found in
  # CI run 8: the compositor was up at wayland-1 and the probe still failed.
  export WAYLAND_DISPLAY="$wayland_socket"
fi

# --- protocol probe ----------------------------------------------------------
# In headless mode a missing probe tool is a hard failure: CI installs
# wayland-utils, and a gate that can silently skip its central assertion is not a
# gate. In session mode it is reported as skipped, and the final PASS line says so.
probe="skipped"
if ! command -v wayland-info >/dev/null 2>&1 && [ "$mode" != "--session" ]; then
  fail "wayland-info is missing — install wayland-utils (CI does, and the layer-shell check depends on it)"
fi

if command -v wayland-info >/dev/null 2>&1; then
  echo "[smoke] probing globals with wayland-info"
  wayland-info >"$work/wayland-info.txt" 2>"$work/wayland-info.err" \
    || fail "wayland-info could not talk to the compositor"
  if grep -q 'zwlr_layer_shell_v1' "$work/wayland-info.txt"; then
    echo "[smoke]   wlr-layer-shell advertised ✓"
    probe="ok"
  else
    fail "compositor does not advertise wlr-layer-shell — P1 needs it"
  fi
  if grep -q 'wl_output' "$work/wayland-info.txt"; then
    echo "[smoke]   wl_output advertised ✓"
  else
    echo "[smoke]   note: no wl_output advertised by this headless setup"
  fi
else
  echo "[smoke] wayland-info not installed; layer-shell probe SKIPPED (not a pass)"
fi

# --- daemon + CLI round trip -------------------------------------------------
echo "[smoke] owed --check-config (shipped example)"
"$owe" --check-config --config "$root/docs/examples/config.toml" \
  || fail "--check-config rejected the shipped example"

echo "[smoke] starting owed"
"$owe" --config "$root/docs/examples/config.toml" >"$work/owed.log" 2>&1 &
daemon_pid=$!

# Starting `owed` is not instant on a machine with no GPU: Vulkan is probed,
# found wanting (Zink cannot create an instance), and the compositor falls back
# to a software path. That took over 10 s on a loaded runner once and failed this
# check, so the budget is a startup-sanity number, not a latency gate — NFR-PERF-1
# is measured by scripts/idle-cpu.sh, not here.
socket="$XDG_RUNTIME_DIR/owe/socket"
waited=0
for _ in $(seq 1 600); do
  [ -S "$socket" ] && break
  sleep 0.1
  waited=$((waited + 1))
done
if [ ! -S "$socket" ]; then
  fail "owed never listened on $socket after $((waited / 10)) s"
fi
echo "[smoke] owed is listening after $((waited / 10)) s"

mode="$(stat -c '%a' "$socket")"
[ "$mode" = "600" ] || fail "socket mode is $mode, expected 600"

echo "[smoke] owectl hello"
"$ctl" hello | tee "$work/hello.txt" || fail "owectl hello failed"
# Any 1.x minor is a pass: minors are additive by design (a new capability field
# must not require a protocol bump), and pinning the exact minor here meant this
# check failed the moment a later phase added one.
grep -qE 'ipc schema 1\.[0-9]+' "$work/hello.txt" || fail "hello did not report an ipc schema 1.x"

echo "[smoke] owectl kill"
"$ctl" kill >/dev/null || fail "owectl kill failed"

exit_code=0
wait "$daemon_pid" || exit_code=$?
daemon_pid=""
[ "$exit_code" = "0" ] || fail "owed exited with $exit_code instead of 0"

if [ "$probe" = "ok" ]; then
  echo "[smoke] PASS: compositor + wlr-layer-shell + ipc round trip + clean shutdown"
else
  echo "[smoke] PASS (partial): compositor + ipc round trip + clean shutdown"
  echo "[smoke] layer-shell probe was SKIPPED — this run does not prove layer-shell"
fi
if [ "$mode" = "--session" ]; then
  echo "[smoke] note: ran against the live session compositor, not the headless one"
fi
