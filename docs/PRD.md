# OWE — Product Requirements Document (PRD)

**Status:** DRAFT v1 — baseline for development. Change through the ADR process (see `docs/ARCHITECTURE.md` §9).
**Companion docs:** [TRD](./TRD.md) · [ARCHITECTURE](./ARCHITECTURE.md) · [BACKEND-DESIGN](./BACKEND-DESIGN.md) · [IMPLEMENTATION-PLAN](./IMPLEMENTATION-PLAN.md) · [UI-DESIGN](./UI-DESIGN.md) · [STRATEGY](./STRATEGY.md) · [historical research](./PLAN.md)

---

## 1. Product overview

**OWE (Open Wallpaper Engine)** is a low-resource live-wallpaper engine and manager for Wayland Linux, shipping first-class support for **Hyprland** and **Caelestia Shell** from day one.

- **`owed`** — a small Rust daemon that actually renders wallpapers (images, animated images, hardware-decoded video, WGSL shaders) on `wlr-layer-shell` background surfaces.
- **`owe`** — a Tauri v2 + React desktop app for browsing the library, assigning wallpapers per monitor, and controlling resources. The GUI is optional and disposable: closing it never stops the wallpaper.
- **`owectl`** — a thin CLI for scripts and keybinds.

**The product thesis:** existing tools force a choice between "tiny but feature-frozen" (swww/awww, hyprpaper), "featureful but closed/heavy" (Wallpaper Engine via Wine/Proton), or "modern but unpolished" (wallr, Phonto, waywe-rs). OWE combines the daemon discipline of the tiny tools with the content breadth of the heavy ones, under one configurable resource governor, with a real management GUI.

### Honest scope statement

OWE is **not** a Wallpaper Engine (Steam) replacement at launch. Workshop scene-format support is a post-1.0 optional plugin (`PRD-F-14`), not a core promise. Web-HTML wallpapers are **rejected** for resource reasons (`§6 Non-goals`) — the raw research chat is explicit that a browser engine must never be the wallpaper surface.

---

## 2. Target users

| Persona | Description | What they need |
|---|---|---|
| **The ricer** | Hyprland/Caelestia user who curates their desktop, comfortable with TOML and keybinds | Per-monitor wallpapers, transitions, shader content, theme hooks (matugen/pywal), scripting via `owectl` |
| **The battery-squeezed laptop user** | Wants a live wallpaper without killing battery | Governor defaults that pause on fullscreen/battery, honest CPU/RAM numbers |
| **The multi-monitor workstation user** | 2–3 outputs, mixed needs | Independent content + FPS per output, hotplug correctness, session restore |
| **The packager/distro maintainer** | Packages OWE for Arch/Fedora/Nix | Clean build deps, systemd units, no vendored blobs, deterministic tests |

Non-users (explicit): GNOME users (no layer-shell in Mutter — rejected by every comparable tool; see TRD §3).

---

## 3. Features (release-mapped, traceable)

IDs are stable and referenced by TRD requirements, the implementation plan's tasks, and the test plan. A feature without a phase reference does not ship.

### 3.1 Core daemon (v0.1 — "Milestone 1")

| ID | Feature | Phase |
|---|---|---|
| `PRD-F-01` | Static image wallpapers (png, jpeg, webp, gif, tga, bmp, pnm; avif if dav1d present) rendered once, then idle at ~0% CPU | P1 |
| `PRD-F-02` | Per-output wallpapers; output enumeration; wallpaper change at runtime without daemon restart | P1 |
| `PRD-F-03` | Background-layer layer-shell surface with no input region (never steals clicks/keys) | P1 |
| `PRD-F-04` | IPC control (Unix socket, versioned JSON-lines) used by GUI and CLI alike | P1 |
| `PRD-F-05` | `owectl` CLI: `set`, `get`, `monitors`, `pause`, `resume`, `kill` | P1 |

### 3.2 Library & transitions (v0.2)

| ID | Feature | Phase |
|---|---|---|
| `PRD-F-06` | Library: scan configured folders, SQLite metadata cache, disk-cached thumbnails, GUI grid | P2 |
| `PRD-F-07` | GPU transitions on wallpaper change (fade, wipe, slide, grow, wave, outer + gl-transitions catalog), per-change configurable | P2 |
| `PRD-F-08` | Output hotplug: new monitor gets configured wallpaper; unplug/replug restores without restart | P2 |
| `PRD-F-09` | Session restore: last wallpaper per output re-applied on daemon start | P2 |
| `PRD-F-10` | Tauri GUI v1: library page, monitor assignment, apply; simple-but-clean styling (see `UI-DESIGN.md`) | P2 |

### 3.3 Environments (v0.3)

| ID | Feature | Phase |
|---|---|---|
| `PRD-F-11` | Hyprland backend: monitor metadata, event socket (fullscreen/focus/workspace) feeding the governor | P1* (detection) / P3 (events) |
| `PRD-F-12` | Caelestia backend: detection, shell-routed set via `caelestia wallpaper`, wallpapers-dir honor, theme-refresh hook | P3 |
| `PRD-F-13` | Backend registry + auto-detection (`caelestia → hyprland → generic-layer-shell`), config-overridable — **no hardcoded environment anywhere** | P3 |

### 3.4 Live content (v0.4–v0.5)

| ID | Feature | Phase |
|---|---|---|
| `PRD-F-14` | Animated GIF/APNG wallpapers with bounded, compressed frame cache | P4 |
| `PRD-F-15` | Video wallpapers (mp4/webm/mkv) with hardware decode (GStreamer+VA-API primary, FFmpeg fallback), fit/fill/center/stretch modes | P4 |
| `PRD-F-16` | Playback control over IPC: play/pause/seek/loop-section, per-output | P4 |
| `PRD-F-17` | WGSL shader wallpapers: pack format (`shader.toml` + `shader.wgsl`), auto-generated parameter UI, hot-reload | P5 |
| `PRD-F-18` | Seed shader packs (aurora, particles, plasma, fluid, matrix) with per-shader license files | P5 |

### 3.5 Resource governor (v0.6 — the differentiator)

| ID | Feature | Phase |
|---|---|---|
| `PRD-F-20` | Per-output FPS caps and quality profiles | P6 |
| `PRD-F-21` | Pause/dim on fullscreen application (event-driven via `PRD-F-11`) | P6 |
| `PRD-F-22` | Battery policies: pause on battery, pause below charge %, drop-FPS profile (UPower) | P6 |
| `PRD-F-23` | DPMS/display-off awareness; idle static-after-N-minutes mode | P6 |
| `PRD-F-24` | Eco mode: one switch applying a bundled policy set | P6 |
| `PRD-F-25` | Governor observability: daemon-reported per-output FPS, CPU%, buffer stats surfaced in GUI | P6 |

### 3.6 Distribution & polish (v1.0)

| ID | Feature | Phase |
|---|---|---|
| `PRD-F-30` | systemd user unit + autostart integration | P6 |
| `PRD-F-31` | Published benchmark report vs awww/hyprpaper/mpvpaper on the Reference Profile | P6 |
| `PRD-F-32` | Packages: `.deb` for Zorin OS 18 / Ubuntu 24.04 (first-class), AUR, rpm/COPR, Nix flake, cargo-install | P7 |
| `PRD-F-33` | Tray menu (pause/resume/next/random) — optional, daemon-independent | P7 |
| `PRD-F-34` | Plugin ABI v0 for third-party content (Rust dylib or WASM, sandboxed) | P7+ |
| `PRD-F-35` | Playlists & timed rotation (wpaperd-style duration/sorting semantics) | P7 |
| `PRD-F-36` | Reactive uniforms (MPRIS now-playing, audio spectrum) | P7+ |

### 3.7 UI redesign wave (post-core, see `UI-DESIGN.md`)

| ID | Feature | Phase |
|---|---|---|
| `PRD-F-40` | UI redesign pass executed against the staged plan in `UI-DESIGN.md` §5, after the core ships with the simple UI | after P6 |

---

## 4. Success criteria

Honesty rules: every number below is either a **hard functional criterion** (testable, no hardware dependence) or a **performance target** tagged `UNVERIFIED` until the Phase 6 benchmark (`PRD-F-31`) measures it on the Reference Profile. We do not publish performance claims before that.

**Reference Profile (pinned — ADR-014):** the maintainer's **Dell Latitude E5440** — Zorin OS 18 (Ubuntu 24.04 LTS base), Hyprland + Caelestia Shell daily-driven, Haswell-era Intel CPU (dual-core class; exact model recorded by the harness, not assumed), Intel HD 4400/4600-class iGPU, 8 GB RAM, spinning HDD. Deliberately old hardware: if OWE is cheap on a 2013 laptop, the low-resource claim is real. Unknowns (exact CPU model, panel resolution, VA-API driver name) are recorded automatically by the bench harness at run time. Known caveat, recorded honestly: VA-API on this machine is the legacy `i965` driver (upstream-archived 2023, still packaged by Ubuntu) — the P4 capability probe decides hardware-vs-software decode per machine, and both paths stay gated.

### Functional (must pass before v1.0)

1. All P1–P6 exit criteria in `IMPLEMENTATION-PLAN.md` are green (they are hard gates).
2. Closing/killing the GUI never affects playback (automated test).
3. No input interception on any compositor in the support matrix (automated layer-shell config test + manual matrix).
4. Daemon survives monitor hotplug, config edits, and IPC client crashes without restarting.
5. `owed` runs with the Tauri app never installed (CLI-only mode fully functional).

### Performance (targets, `UNVERIFIED` until `PRD-F-31`)

| Metric | Target (Reference Profile) | Status |
|---|---|---|
| Static image idle CPU | ~0% steady-state (event-driven, render-once) | design-guaranteed, benchmark pending |
| 1080p30 video CPU | ≤ 5% of one core with hw decode active | UNVERIFIED |
| 1080p30 video CPU, software decode fallback | ≤ 25% of one core, with a GUI/CLI warning shown | UNVERIFIED |
| Daemon RSS, static image | ≤ 30 MiB | UNVERIFIED |
| Daemon RSS, video | ≤ 120 MiB (decode buffers dominate) | UNVERIFIED |
| GUI RSS (while open) | ≤ 250 MiB; 0 when closed | UNVERIFIED |
| GPU shader wallpaper CPU | ≤ 1% of one core at 30 FPS | UNVERIFIED |
| Wallpaper switch latency | < 300 ms to first frame of new content | UNVERIFIED |

Failure posture: if a target is missed at P6, the release notes state the measured numbers and the gap — we do not ship marketing numbers.

### Adoption (leading indicators, not vanity metrics)

- Install base via packaging channels; issue median-response time; packaged-in ≥ 1 major distro repo by 6 months post-1.0. These are tracked, not gated.

---

## 5. Platform support matrix

| Environment | Support at v1.0 | Mechanism |
|---|---|---|
| Hyprland | **Primary — CI-tested** | `hyprland` shell backend + layer-shell |
| Caelestia Shell | **Primary — CI-tested** | `caelestia` shell backend (CLI/IPC integration) |
| Zorin OS 18 (Ubuntu 24.04 base) | **Dev & benchmark platform** | apt/`.deb` first-class packaging; maintainer machine runs Hyprland + Caelestia |
| Other layer-shell compositors (sway, niri, labwc) | Best-effort, community-tested | `generic-layer-shell` fallback backend |
| KDE Plasma 6 | Stretch (P7) | layer-shell works; needs Plasma-specific testing |
| GNOME | **Unsupported — non-goal** | Mutter lacks wlr-layer-shell; same rejection as swww/wpaperd/wallr |
| X11 | **Unsupported — non-goal** | Wayland-only project |

---

## 6. Non-goals (explicit rejections, with reasons)

1. **Web/HTML wallpapers** — a browser engine as wallpaper surface contradicts the core resource thesis (raw-chat decision, kept).
2. **X11 support** — halves the Wayland-specific work for a shrinking audience.
3. **GNOME support** — technically blocked (no layer-shell); reconsider only via a future GNOME extension shell backend.
4. **Wallpaper Engine Workshop compatibility at launch** — post-1.0 plugin (we-layerd proves feasibility; scope is huge).
5. **Bundling a full browser/CEF** — rejected same as (1).
6. **Daemon-in-GUI process** — non-negotiable architectural separation (TRD FR-ARC-1).
7. **Multi-OS (macOS/Windows)** — Phonto does macOS; we stay Linux to keep quality high.

---

## 7. Known open questions (tracked, not assumed)

| # | Question | Owner | Resolved by |
|---|---|---|---|
| OQ-1 | "OWE / Open Wallpaper Engine" name-vs-trademark check against the Wallpaper Engine brand before any public announcement | maintainer | before first public release (STRATEGY §6) |
| OQ-2 | Caelestia CLI surface is fast-moving: pin exact `caelestia wallpaper` flags/IPC by testing against a live shell | P3 dev | P3 entry |
| OQ-3 | Which shader-pack licenses permit redistribution per shader | P5 dev | P5 entry |
| OQ-4 | Benchmark silicon — **RESOLVED:** pinned to the maintainer's Dell Latitude E5440 (ADR-014); the harness records exact CPU model, panel resolution, and VA-API driver name at run time | — | done |

---

## 8. Traceability

- Every `PRD-F-*` maps to phase gates in `IMPLEMENTATION-PLAN.md` and requirement IDs in `TRD.md` §2.
- Features dropped or deferred require an ADR (see `ARCHITECTURE.md` §9).
- The superseded research doc (`PLAN.md`) remains for provenance of decisions; where it conflicts with these docs, these docs win.
