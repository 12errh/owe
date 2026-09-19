# OWE — Architecture & ADRs

**Status:** DRAFT v1. This document is the *how it fits together*: components, data flow, and the decision log. It implements the requirements in [TRD](./TRD.md) and the product scope in [PRD](./PRD.md).
**Companions:** [PRD](./PRD.md) · [TRD](./TRD.md) · [BACKEND-DESIGN](./BACKEND-DESIGN.md) · [IMPLEMENTATION-PLAN](./IMPLEMENTATION-PLAN.md) · [UI-DESIGN](./UI-DESIGN.md) · [STRATEGY](./STRATEGY.md)

---

## 1. High-level architecture

```
┌────────────────────────────────────────────┐
│                 owe (GUI)                  │
│        Tauri v2 · React 18 · TS · Vite     │
│                                            │
│  Library grid · Monitor assignment ·       │
│  Settings · Governor status · Preview      │
└───────────────┬────────────────────────────┘
                │ Tauri commands (Rust bridge)
┌───────────────▼────────────────────────────┐
│      owe-gui-backend (Tauri Rust side)     │
│      thin: IPC client + file dialogs +     │
│      autostart/tray plugins. No rendering. │
└───────────────┬────────────────────────────┘
                │ JSON-lines / Unix socket (IPC v1)
┌───────────────▼────────────────────────────────────────────┐
│                        owed (daemon)                       │
│                                                            │
│  ┌────────────┐ ┌──────────────┐ ┌──────────────────────┐  │
│  │  config    │ │   library    │ │       governor       │  │
│  │ TOML + hot │ │ SQLite +     │ │ rules → RenderPolicy │  │
│  │ reload     │ │ thumbs       │ │ per output           │  │
│  └────────────┘ └──────────────┘ └──────────▲───────────┘  │
│                                             │ events       │
│  ┌───────────────────────────┐  ┌───────────┴────────────┐ │
│  │      renderer registry    │  │  shell-backend registry│ │
│  │  trait ContentRenderer    │  │  trait ShellBackend    │ │
│  │  ├ StaticImage            │  │  ├ hyprland            │ │
│  │  ├ AnimatedImage          │  │  ├ caelestia           │  │
│  │  ├ Video (GStreamer/FFmpeg)│ │  ├ generic-layer-shell │ │
│  │  ├ Shader (WGSL)          │  │  └ (future: kde, niri) │ │
│  │  └ (future: plugin)       │  │                        │ │
│  └─────────────┬─────────────┘  └────────────────────────┘ │
│                │                                            │
│  ┌─────────────▼─────────────────────────────────────────┐ │
│  │  output workers (one thread per Wayland output)       │ │
│  │  layer-shell surface · wgpu device · frame pacing ·   │ │
│  │  transition engine · bounded buffer pool              │ │
│  └──────────────────────────┬────────────────────────────┘ │
└─────────────────────────────┼──────────────────────────────┘
                              │
              ┌───────────────┼───────────────────┐
              ▼               ▼                   ▼
        Hyprland IPC     Caelestia CLI      Wayland compositor
        (sockets 1&2)    (caelestia …)      (wlr-layer-shell)
```

**Component ownership rules**

1. The daemon owns *all* state: config, library, playback, governor. GUI and CLI are stateless clients (their settings live in daemon config; UI prefs live in tauri-plugin-store).
2. Only output workers touch Wayland/wgpu. Everything else is pure logic — this is what makes the logic TDD-able and the workers integration-testable.
3. Registries, not if-chains: shell backends and renderers register by string id; config selects them; unknown ids are config errors, never silent fallbacks.
4. The GUI never renders wallpaper content, not even previews-of-record — previews are thumbnails (P2) or a small shader-snapshot (P5), both produced by the daemon on request.

---

## 2. Repository layout (Cargo workspace)

```
owe/
├── Cargo.toml                 # workspace
├── crates/
│   ├── owe-core/              # config model, content model, library, governor logic (pure, no IO deps beyond std+serde)
│   ├── owe-ipc/               # protocol types, framing, client + server (socket-agnostic where possible)
│   ├── owe-shell-hyprland/    # ShellBackend impl (sockets, events)
│   ├── owe-shell-caelestia/   # ShellBackend impl (CLI/IPC)
│   ├── owe-shell-generic/     # layer-shell fallback backend
│   ├── owe-render/            # wgpu device/surface mgmt, transition engine, shader runtime, buffer pools
│   ├── owe-media/             # image/animated/video decode behind a MediaDecoder trait (GStreamer + FFmpeg impls)
│   ├── owed/                  # daemon binary: event loop, output workers, supervisor
│   ├── owectl/                # CLI binary (uses owe-ipc client)
│   └── owe-bench/             # benchmark harness (PRD-F-31)
├── app/                       # Tauri v2 app (React/TS in app/src, Rust in app/src-tauri)
├── assets/shader-packs/       # seed shader packs (license-checked per pack)
├── packaging/                 # systemd unit, .deb, AUR PKGBUILD, rpm spec, nix flake
├── scripts/fetch-reference.sh # pinned shallow clones of studied repos → reference/ (gitignored, never built)
├── ATTRIBUTION.md             # provenance ledger: repo + commit + license + what was ported (ADR-013)
├── docs/                      # this doc set
└── tests/                     # cross-crate integration tests, fixtures, golden images
```

Dependency direction (enforced by review + `cargo-deny` later): `owed → *`, `app/src-tauri → owe-ipc` only; nothing depends on `owed`.

---

## 3. Key data flows

### 3.1 Set wallpaper (static image)

```
GUI/CLI → IPC {set, output: "DP-1", source: LibraryItem|Path}
  → daemon validates → governor consulted (is output paused?)
  → output worker: transition engine loads old+new textures → renders N frames
  → final frame presented → worker sleeps (static path)
  → daemon replies {ok, applied: {output, wallpaper_id, transition_ms}}
  → session-state file updated ($XDG_STATE_HOME/owe/)
```

### 3.2 Governor event → policy change

```
Hyprland socket2 event "fullscreen>>>" 
  → shell backend parses → event bus → governor
  → rule table match → RenderPolicy {Paused} for focused output
  → output worker: stops scheduling frames (video: pauses decoder; shader: stops ticking)
  → on "fullscreen>>0" reverse event → policy recomputed → resumes
```

### 3.3 Output hotplug

```
SCTK output event → supervisor: spawn/tear down output worker
  → new worker reads config (output section → "any" → default)
  → applies last-known or configured wallpaper → IPC broadcast {outputs_changed}
```

---

## 4. Technology choices summary (details in BACKEND-DESIGN)

| Area | Choice | Fallback |
|---|---|---|
| Wayland | smithay-client-toolkit 0.13+, wayland-client | — (hard requirement: layer-shell) |
| GPU | wgpu (Vulkan → GL fallback) | EGL/software (llvmpipe) for CI only |
| Video | GStreamer + VA-API | FFmpeg (`ffmpeg-next`) behind same trait |
| Images | `image` crate (+dav1d) | — |
| Event loop | calloop (no tokio) | — |
| Config | TOML + inotify hot-reload | — |
| Library DB | SQLite via rusqlite | — |
| IPC | JSON-lines over Unix socket, schema v1 | binary codec behind same trait (post-1.0) |
| GUI | Tauri v2, React 18, TS, Vite, Tailwind, TanStack Query, Zustand | — |
| Battery | UPower via zbus | `/sys/class/power_supply` reader |
| Hyprland | raw socket client in owe-shell-hyprland (2 sockets, trivial protocol) | hyprland-rs if our client hits protocol gaps |

---

## 5. Threading & lifecycle model

- **Main thread:** calloop event loop — IPC server, config watcher, shell event bus, governor.
- **Per output:** one worker thread owning its layer-shell surface + wgpu surface. Wayland requires per-connection or carefully synchronized access; per-output threads keep each output isolated (NFR-REL-1) and let one output's renderer crash without the others noticing (supervisor restarts it).
- **Decode threads:** media decoding is always off-thread (image decode, GIF cache fill, GStreamer streaming threads); workers only consume ready frames.
- **Lifecycle:** daemon start → load config → detect shell backend → enumerate outputs → spawn workers → apply session restore → serve IPC. GUI/CLI connect/disconnect freely. Daemon shutdown: drain IPC, pause all decoders, destroy surfaces, save session state, exit.

---

## 6. Testing architecture (supports the Pragmatic TDD strategy — STRATEGY §2)

| Layer | Tool | What it proves |
|---|---|---|
| Pure logic (config, governor rules, IPC framing, library scan) | cargo-nextest + rstest + insta snapshots | red-green-refactor TDD applies fully here |
| IPC client/server | in-process socketpair tests + chaos tests (NFR-REL-2) | protocol robustness |
| Shell backends | recorded-event replay harnesses (Hyprland socket2 logs, Caelestia CLI stub) | backend behavior without a live shell |
| Rendering | golden-image tests: wgpu renders to an offscreen texture on a headless/lavapipe device, diffed against reference PNGs | visual correctness without a compositor |
| Compositor integration | wlroots headless (`WLR_BACKENDS=headless`) in CI running owed for real; screenshots via wlr-screencopy | end-to-end surface correctness |
| Performance | owe-bench harness + CI perf smoke (regression alerts, not absolute gates) | no silent perf regressions |

CI matrix (GitHub Actions — ADR-016): Linux x86_64 (build+test on ubuntu-24.04 runners, matching the Zorin/Ubuntu 24.04 base exactly), headless-Wayland job (smoke), fuzz job (nightly), release packaging job (`.deb` first, P7+).

---

## 7. Configuration & state locations (XDG-compliant)

| What | Where |
|---|---|
| Main config | `$XDG_CONFIG_HOME/owe/config.toml` |
| Shader packs | `$XDG_CONFIG_HOME/owe/shaders/` + bundled system dir |
| Library folders | configured in config.toml |
| Library DB | `$XDG_DATA_HOME/owe/library.db` |
| Thumbnails | `$XDG_CACHE_HOME/owe/thumbs/` |
| Session state | `$XDG_STATE_HOME/owe/session.toml` |
| IPC socket | `$XDG_RUNTIME_DIR/owe/<HIS-or-pid>/socket` (0600) |
| Logs | journald (systemd) or stderr; `tracing` with env-filter |

---

## 8. Performance design commitments (mechanism, not promises)

1. **Render-once-then-sleep** for static content; no timer, no polling (wallr-proven pattern).
2. **Frame-callback pacing** for all animation; governor FPS caps implemented as minimum-frame-interval gates, never busy loops.
3. **Bounded memory everywhere:** buffer pools sized per output; GIF caches capped+compressed; video never fully buffered (TRD NFR-RES-1).
4. **Zero-copy path when possible:** dma-buf import of decoded video frames into wgpu; shm fallback always available (we-layerd lesson).
5. **CPU-idle governor:** all policy evaluation is event-driven; ≥ 1 Hz polling ceiling only for non-event sources (TRD FR-GOV-7).
6. Numbers are published only from the bench harness (PRD §4 honesty rule).

---

## 9. ADR log (decision record — every entry is vetoable by the user)

| ADR | Decision | Status | Rationale & consequences |
|---|---|---|---|
| ADR-001 | License: **GPL-3.0-or-later** | ACCEPTED (user delegated choice; project is open source) | Enables legally copying code from swww/awww (GPL) and wpaperd (GPL-3) — our two biggest accelerators. Consequence: derivative tools must also be GPL; acceptable for a Linux desktop tool. |
| ADR-002 | Name: **OWE — Open Wallpaper Engine**; binaries `owe`, `owed`, `owectl` | ACCEPTED (user choice) | Renaming later is mechanical (crate names, socket path, config dir). ⚠️ Open trademark question recorded as PRD OQ-1: "Wallpaper Engine" is an existing commercial product; verify before public announcement. |
| ADR-003 | Tauri v2 = control plane only; daemon renders | ACCEPTED (from raw chat) | The webview never renders wallpaper content. Consequence: two processes, IPC contract, slightly more code — required for the resource thesis. |
| ADR-004 | **calloop, not tokio**, for the daemon | ACCEPTED (agent, user-informed) | A wallpaper daemon needs an event loop, not an async runtime. Smaller dep tree, lower RSS, simpler lifetimes. Consequence: GStreamer bus handling uses its own threads + calloop channels. |
| ADR-005 | wgpu (Vulkan-first, GL fallback) for rendering | ACCEPTED | Portability across Mesa drivers + WGSL shader story. Consequence: slightly heavier init than raw EGL; acceptable. |
| ADR-006 | Video decode: GStreamer+VA-API primary, FFmpeg fallback | ACCEPTED | Phonto proves the zero-copy GStreamer path; FFmpeg fallback derisks distro plugin gaps. Both behind one `MediaDecoder` trait. |
| ADR-007 | Own thin Hyprland socket client instead of hyprland-rs | ACCEPTED (agent, revisit-if-painful) | The protocol is two UNIX sockets with trivial text messages; a thin client is ~200 lines and zero extra deps. If protocol gaps appear, adopt hyprland-rs (contained swap behind ShellBackend). |
| ADR-008 | Pragmatic TDD (user choice) | ACCEPTED | Strict red-green for logic; integration/golden-image for graphics; documented honestly in STRATEGY §2. |
| ADR-009 | Reference Profile: generic iGPU laptop; absolute targets marked UNVERIFIED until P6 bench | ACCEPTED (user choice) | Prevents dishonest performance claims; relative deltas vs awww/hyprpaper always reported alongside. |
| ADR-010 | UI ships simple at P2; full redesign wave post-core per UI-DESIGN §5 | ACCEPTED (user choice) | Core-first, polish-later; the redesign doc exists from day one so the wave is structured, not vibes. |
| ADR-011 | JSON-lines IPC v1; binary codec only if measured | ACCEPTED | Raw chat's "don't prematurely optimize" advice; versioned schema keeps the door open. |
| ADR-012 | GNOME and X11 are non-goals | ACCEPTED | No layer-shell in Mutter; X11 halves focus. Reconsidering requires an ADR + shell-backend design, not a config flag. |
| ADR-013 | Reference-repo policy: pinned shallow clones via `scripts/fetch-reference.sh` into gitignored `reference/` (never compiled); `ATTRIBUTION.md` is the provenance ledger; ported code keeps original license headers; the file-level **port map** with PORT/READ-ONLY/AVOID verdicts per repo lives in [REFERENCE-CODE-MAP](./REFERENCE-CODE-MAP.md) | ACCEPTED (user choice) | Legal under GPL-3.0 (swww/awww, wpaperd, phonto) and MIT (wallr, waywe-rs) with attribution; we-layerd carries **no license** at pin → READ-ONLY, never port its code. Keeps provenance auditable and the build clean of foreign code. |
| ADR-014 | Reference/benchmark machine = maintainer's Dell Latitude E5440 — Zorin OS 18, Hyprland + Caelestia daily-driven, Haswell-era iGPU, 8 GB RAM, HDD | ACCEPTED (user input) | Old hardware is the honest testbed for low-resource claims. Known risk recorded: legacy `i965` VA-API driver (upstream-archived 2023) — the P4 capability probe + software-decode fallback handle it; targets stay UNVERIFIED until measured. |
| ADR-015 | Packaging priority: `.deb` (Zorin/Ubuntu 24.04) first-class, then AUR, rpm, Nix, cargo | ACCEPTED (user input) | The maintainer and the first user cohort share the Ubuntu-24.04-based family; first users = people on the same distro family. |
| ADR-016 | Hosting: **`https://github.com/12errh/owe`** (repo already created by the maintainer — no repo-creation step exists anywhere in the plan; development happens in it, `origin` points here) + GitHub Actions (unit, headless-Wayland, nightly fuzz, release jobs) | ACCEPTED (user choice) | ubuntu-24.04 CI runners match the Zorin/Ubuntu base exactly, so the `.deb` path and the headless-Wayland smoke job run on the same base as the Reference machine. Trade-off noted honestly: CI enforces gates only while the repo is hosted; local-only development weakens the gate contract. |

---

## 10. Open questions (same list as PRD §7 — single source of truth is PRD)

OQ-1 name/trademark check · OQ-2 Caelestia CLI pinning · OQ-3 shader-pack licenses · OQ-4 benchmark silicon. Tracked in [PRD §7](./PRD.md#7-known-open-questions-tracked-not-assumed).
