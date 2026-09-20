# OWE — Open Wallpaper Engine

A low-resource live-wallpaper engine and manager for Wayland Linux, targeting **Hyprland** and
**Caelestia Shell** first. GPL-3.0-or-later.

- **`owed`** — the daemon that renders wallpapers (images, animated images, hardware-decoded
  video, WGSL shaders) on layer-shell background surfaces.
- **`owe`** — Tauri v2 + React desktop app (library, per-monitor assignment, settings).
- **`owectl`** — thin CLI for scripts and keybinds.

> **Status: alpha — Phases 0–2 complete (v0.2).** Static images render on Hyprland
> layer-shell background surfaces, with GPU transitions on change, an indexed wallpaper
> library with cached thumbnails, hotplug handling, and session restore; the GUI is a
> library-grid-and-assignment window. **Animated images, video, shader wallpapers, and the
> resource governor are not implemented** (Phases 4–6) — the daemon reports them as
> `not in this build` instead of pretending. Performance targets stay `UNVERIFIED` until the
> Phase 6 benchmark publishes measurements.
>
> Evidence tables (commands + observed results) are in
> [`docs/IMPLEMENTATION-PLAN.md`](./docs/IMPLEMENTATION-PLAN.md) §0.1, §1.1, and §2.1.

## Documentation

The complete doc set lives in [`docs/`](./docs/README.md) — PRD, TRD, architecture + ADRs,
backend design, implementation plan with phase gates, UI design (stub), strategy, and the
reference-code map.

## Repository layout

```
crates/owe-core    config model + validation, content model   (pure logic, heavily tested)
crates/owe-ipc     IPC protocol v1: framing, server, client
crates/owe-render  wgpu rendering helpers (headless golden-image tooling)
crates/owed        the daemon binary
crates/owectl      control CLI
app/               Tauri v2 + React GUI
docs/              the doc set (start at docs/README.md)
reference/         pinned clones of studied repos (gitignored, never built — see docs/REFERENCE-CODE-MAP.md)
```

## Build

```bash
# Rust workspace
cargo build --workspace
cargo test --workspace

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
owectl clear -m eDP-1
owectl kill
```

`--json` prints the raw reply for scripting. Exit codes: `0` success, `1` the daemon refused
(with a protocol code such as `CONFIG_INVALID` or `NOT_FOUND`), `2` the daemon could not be
reached.

The GUI (`cd app && pnpm tauri dev`) shows the same state as a library grid: thumbnail per
wallpaper, a transition picker fed from the daemon's own config, per-output assignment
(“Apply selected” on each output), apply-all, rescan, and search — and says when the daemon
is not running instead of failing silently.

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
./scripts/headless-smoke.sh            # headless (needs sway + wayland-utils)
./scripts/headless-smoke.sh --session  # against the running session

# Regenerate the placeholder app icons (deterministic, no image editor involved)
cargo run -p owe-render --example gen-app-icons

# End to end on the session you are logged into: apply a four-colour test image,
# prove it reached the screen by sampling a screenshot, assert the layer surface,
# check session state, clear, and shut down. Needs grim + python3+PIL.
./scripts/e2e-hyprland.sh

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
