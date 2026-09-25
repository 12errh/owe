# OWE — Open Wallpaper Engine

A low-resource live-wallpaper engine and manager for Wayland Linux, targeting **Hyprland** and
**Caelestia Shell** first. GPL-3.0-or-later.

- **`owed`** — the daemon that renders still images, animated images, and video on
  layer-shell background surfaces, with GPU transitions and per-output frame clocks.
  WGSL shader wallpapers are planned for Phase 5, not implemented yet.
- **`owe`** — Tauri v2 + React desktop app (library, per-monitor assignment, settings).
- **`owectl`** — thin CLI for scripts and keybinds.

> **Status: alpha — Phases 0–4 implemented in the development worktree (package version
> remains 0.3.0 until the P4 gate closes).** Static images render on
> Hyprland layer-shell background surfaces, with GPU transitions, an indexed wallpaper
> library with cached thumbnails, hotplug handling, session restore, Caelestia routing,
> and typed Hyprland events. **Animated images and video now decode and play:** the daemon
> has a per-output frame clock, Wayland frame-callback pacing, bounded frame memory,
> play/pause/seek/loop IPC, CLI controls, GUI playback controls, decode statistics, and
> live Hyprland GIF/video probes. Video uses GStreamer with an FFmpeg fallback; hardware
> decode is reported only when the local VA-API driver actually negotiates it. **Zero-copy
> dma-buf import, the long-duration P4 performance gate, and fuzz targets remain explicit
> follow-ups.** Shader wallpapers and the automatic resource governor are still Phases 5–6.
> Performance targets remain `UNVERIFIED` until the Phase 6 benchmark publishes measurements.
>
> Evidence tables and the remaining P4 gate items are in
> [`docs/IMPLEMENTATION-PLAN.md`](./docs/IMPLEMENTATION-PLAN.md) §0.1, §1.1, §2.1, §3.1,
> and §4.1.

## Implemented and remaining

**Implemented and exercised:** static/animated/video decoding, bounded frame caching,
per-output playback clocks, compositor frame-callback presentation, GPU transitions,
library indexing and thumbnails, hotplug reconciliation, session restore, Hyprland events,
Caelestia shell-routed and daemon-drawn modes, IPC/CLI/Tauri/React playback controls,
decode statistics, and live compositor smoke probes.

**Not complete yet:**

- **P4 release gate:** dma-buf zero-copy import, ten-minute/4K RSS and multi-cap stability
  measurements, and fuzz targets remain open.
- **P5:** WGSL shader packs, shader previews, and shader GUI controls are not implemented.
- **P6:** the automatic resource governor, published benchmark report, systemd packaging,
  and tray/rotation features are not implemented; the current pause/resume path is a
  manual override.
- **P7:** release packaging, plugin ABI, and ecosystem features are not implemented.

## Documentation

The complete doc set lives in [`docs/`](./docs/README.md) — PRD, TRD, architecture + ADRs,
backend design, implementation plan with phase gates, UI design (stub), strategy, and the
reference-code map.

## Repository layout

```
crates/owe-core    config model + validation, content model   (pure logic, heavily tested)
crates/owe-ipc     IPC protocol v1: framing, server, client
crates/owe-media   still/animated/video decoding, frame cache, runtime probes
crates/owe-render  wgpu rendering, transitions, and layer-shell presentation
crates/owed        the daemon binary
crates/owectl      control CLI
crates/owe-shell-* shell backends: hyprland, caelestia, generic-layer-shell
app/               Tauri v2 + React GUI (app/src-tauri is built by its own CI job)
docs/              the doc set (start at docs/README.md)
reference/         pinned clones of studied repos (gitignored, never built — see docs/REFERENCE-CODE-MAP.md)
```

## Build

```bash
# Rust workspace
cargo build --workspace
cargo test --workspace

# Tauri/React GUI (kept outside the Rust workspace because of GTK/WebKit)
cd app && pnpm install --frozen-lockfile && pnpm build
(cd src-tauri && cargo test)

# Validate a config file without starting the daemon
cargo run -p owed -- --check-config --config ./docs/examples/config.toml

# Daemon + CLI round trip (the daemon is session-scoped: give it a sandbox)
./target/debug/owed --config ./docs/examples/config.toml &
./target/debug/owectl hello
./target/debug/owectl kill
```

### Use it

The daemon renders on a background layer surface; the CLI talks to it over the session socket.

```bash
owed &

owectl hello                                  # version, capabilities, what this build cannot do yet
owectl monitors                               # every output: size, state, current wallpaper

# Index your folders once, then browse them by reference.
owectl library scan                           # walks `library.paths` from the config
owectl library scan ~/Pictures/Wallpapers    # or an explicit folder
owectl library list --filter aurora --page 1

# Apply: a path or a `library:<id>` reference, to one output or all of them.
owectl set ~/Pictures/wallpapers/one.png      # every output
owectl set library:12 -m eDP-1                # one output, by its stable library id
owectl set library:12 --transition wave --duration-ms 450
owectl get --monitor eDP-1                    # prints the path, or nothing if OWE applied none
owectl library thumb 12                       # materialise (or find) one cached thumbnail
owectl pause  # / resume: governor override
owectl playback pause -m eDP-1
owectl playback seek 1.5 -m eDP-1
owectl playback loop 0 5 -m eDP-1
owectl playback play -m eDP-1
owectl stats
owectl clear -m eDP-1
owectl kill
```

`--json` prints the raw reply for scripting. Exit codes: `0` success, `1` the daemon refused
(with a protocol code such as `CONFIG_INVALID` or `NOT_FOUND`), `2` the daemon could not be
reached.

The GUI (`cd app && pnpm tauri dev`) shows the same state as a library grid: thumbnail per
wallpaper, a transition picker fed from the daemon's own config, per-output assignment
(“Apply selected” on each output), apply-all, rescan, search, playback play/pause/seek/loop
controls, and per-output decode/FPS/cache statistics — and says when the daemon is not
running instead of failing silently.

After `owectl clear` the output has no OWE wallpaper, so a previous wallpaper tool
(`swaybg`, `swww`, Caelestia) needs to be re-run if you were using one.

### GUI

Needs **Node ≥ 22** (pnpm 11 refuses to run on older Node), pnpm, and `webkit2gtk-4.1`:

```bash
cd app && pnpm install && pnpm tauri dev
```

### Test harness

```bash
# Wayland + layer-shell + IPC smoke test. `--session` runs the same checks
# against the compositor you are already in (no root, no sway needed).
# `wayland-info` is optional; without it the layer-shell probe is reported as
# skipped rather than silently counted as a pass.
./scripts/headless-smoke.sh            # headless (needs sway + wayland-utils)
./scripts/headless-smoke.sh --session  # against the running session

# Regenerate the placeholder app icons (deterministic, no image editor involved)
cargo run -p owe-render --example gen-app-icons

# End to end on the session you are logged into: apply a four-colour test image,
# prove it reached the screen by sampling a screenshot, assert the layer surface,
# check session state, clear, and shut down. Needs grim + python3+PIL.
./scripts/e2e-hyprland.sh

# Live P4 probe: GIF and video frame clocks, play/pause/seek/loop, stats, clear,
# and clean shutdown. Requires a Wayland session, grim, python3+PIL, and at
# least one installed video runtime (GStreamer or FFmpeg).
./scripts/e2e-playback.sh

# NFR-PERF-1: 60 s of idle with a wallpaper on screen must stay under 1% of a core.
./scripts/idle-cpu.sh 60

# The GUI's daemon-facing layer: a stub daemon, plus a replay of recorded real
# daemon traffic (no compositor, no running daemon needed)
cd app/src-tauri && cargo test

# Re-record that fixture against the real daemon binary. Writes a reviewable diff
# into app/src-tauri/tests/fixtures/ — commit it when the wire shape changes.
./scripts/record-gui-fixtures.sh
```

## License

GPL-3.0-or-later — see [`LICENSE`](./LICENSE). Ported code from reference projects is credited in
[`ATTRIBUTION.md`](./ATTRIBUTION.md).
