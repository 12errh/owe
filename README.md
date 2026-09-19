# OWE — Open Wallpaper Engine

A low-resource live-wallpaper engine and manager for Wayland Linux, targeting **Hyprland** and
**Caelestia Shell** first. GPL-3.0-or-later.

- **`owed`** — the daemon that renders wallpapers (images, animated images, hardware-decoded
  video, WGSL shaders) on layer-shell background surfaces.
- **`owe`** — Tauri v2 + React desktop app (library, per-monitor assignment, settings).
- **`owectl`** — thin CLI for scripts and keybinds.

> **Status: pre-alpha — Phase 0 complete (foundations).** The workspace, IPC, config model, CI
> harness, and GUI shell exist and are tested; the daemon does not render wallpapers yet.
> Nothing here is usable as a wallpaper engine today. Performance targets in the docs are
> marked `UNVERIFIED` until the Phase 6 benchmark publishes measurements.
>
> Phase 0's evidence table (commands + observed results) is in
> [`docs/IMPLEMENTATION-PLAN.md` §0.1](./docs/IMPLEMENTATION-PLAN.md).

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
```

## License

GPL-3.0-or-later — see [`LICENSE`](./LICENSE). Ported code from reference projects is credited in
[`ATTRIBUTION.md`](./ATTRIBUTION.md).
