# OWE — Technical Requirements Document (TRD)

**Status:** DRAFT v1. Requirements here are the *testable* form of the PRD: every requirement has an ID, a verification method, and a phase where it is proven. Nothing in this document is aspirational unless explicitly tagged `TARGET`.
**Companions:** [PRD](./PRD.md) · [ARCHITECTURE](./ARCHITECTURE.md) · [BACKEND-DESIGN](./BACKEND-DESIGN.md) · [IMPLEMENTATION-PLAN](./IMPLEMENTATION-PLAN.md) · [UI-DESIGN](./UI-DESIGN.md) · [STRATEGY](./STRATEGY.md)

**Verification method legend:** `UT` = unit test · `IT` = integration test · `GIT` = golden-image test (rendered pixels diffed against reference) · `HW` = manual verification on the Support Matrix hardware · `BENCH` = measured by the benchmark harness (`PRD-F-31`).

---

## 1. System context

```
owe (Tauri GUI) ─┐
owectl (CLI) ────┤─ JSON-lines over Unix socket ($XDG_RUNTIME_DIR/owe/socket)
                 ▼
              owed (daemon) ── owns: config, library DB, governor, renderer registry,
                 │                    per-output layer-shell surfaces, wgpu device
                 ├── Hyprland sockets  ($HIS/.socket.sock, .socket2.sock)
                 ├── Caelestia CLI/IPC (caelestia wallpaper …)
                 ├── UPower (D-Bus via zbus)
                 └── Wayland compositor (wlr-layer-shell, wlr-output-management events via SCTK)
```

Trust model: all IPC is local, same-user, socket-permission restricted (`0600`, under `$XDG_RUNTIME_DIR`). The daemon never executes shell commands interpolated from untrusted input; shader packs and content files are data, not code (the only code-load path is the post-1.0 plugin ABI, which is sandbox-gated — TRD FR-SEC-3).

---

## 2. Functional requirements

Each requirement lists its source PRD feature and the phase that proves it.

### FR-CORE — daemon & rendering

| ID | Requirement | Source | Verify | Phase |
|---|---|---|---|---|
| FR-CORE-1 | `owed` runs as a standalone process with zero GUI/webview dependencies; killing it is the only way to stop rendering | PRD-F-01 | IT (GUI process kill → daemon unaffected) | P1 |
| FR-CORE-2 | Static images render to a background-layer surface; after present + frame-callback completion, the daemon performs no scheduled rendering work for static content | PRD-F-01 | IT + HW (CPU sampling shows 0 scheduled frames for 60 s) | P1 |
| FR-CORE-3 | Per-output wallpaper state is maintained for every connected output; `set` on one output never disturbs others | PRD-F-02 | IT (fake-output harness) | P1 |
| FR-CORE-4 | Wallpaper changes apply at runtime without daemon restart | PRD-F-02 | IT | P1 |
| FR-CORE-5 | Layer-shell surface uses `Layer::Background`, anchor all edges, exclusive zone −1, `KeyboardInteractivity::None`, empty input region | PRD-F-03 | IT (surface config assertions) + HW | P1 |
| FR-CORE-6 | IPC server: versioned JSON-lines protocol, one request → one response; schema version negotiation; malformed message gets an error frame, never a disconnect of the socket | PRD-F-04 | UT + IT | P1 |
| FR-CORE-7 | `owectl set <path> [-m <output>]`, `get`, `monitors`, `pause`, `resume`, `kill` implemented against the same IPC client crate the GUI uses | PRD-F-05 | UT + IT | P1 |

### FR-LIB — library & transitions

| ID | Requirement | Source | Verify | Phase |
|---|---|---|---|---|
| FR-LIB-1 | Library scanner indexes configured folders (formats per PRD-F-01/14/15) into SQLite (`rusqlite`) with mtime-based incremental rescan | PRD-F-06 | UT + IT | P2 |
| FR-LIB-2 | Thumbnails are generated off-thread, cached under `$XDG_CACHE_HOME/owe/thumbs`, never blocking the render loop | PRD-F-06 | IT | P2 |
| FR-LIB-3 | Transitions render on the GPU between old and new content; per-transition duration/fps configurable; unsupported transition names rejected at config-validation time | PRD-F-07 | UT + GIT | P2 |
| FR-LIB-4 | Output hotplug: new output receives its configured (or default) wallpaper; removed output releases its surface and buffers | PRD-F-08 | IT (headless hotplug test via wlr-output-management or synthetic SCTK events) | P2 |
| FR-LIB-5 | Session restore: last wallpaper per output persisted under `$XDG_STATE_HOME/owe/` and re-applied at daemon start | PRD-F-09 | IT | P2 |
| FR-LIB-6 | Tauri GUI performs library browse, per-output assignment, and apply via the daemon IPC client — GUI contains no rendering logic | PRD-F-10 | IT (mock daemon) | P2 |

### FR-SHELL — environment backends

| ID | Requirement | Source | Verify | Phase |
|---|---|---|---|---|
| FR-SHELL-1 | `ShellBackend` trait (see BACKEND-DESIGN §3.2) with registered implementations selectable by config id; adding a backend requires no core changes | PRD-F-13 | UT (registry test) | P3 (trait lands P1) |
| FR-SHELL-2 | Hyprland backend: enumerate outputs (incl. metadata), subscribe to socket2 events (`activewindow`, `activewindowv2`, `fullscreen`, `workspace`), expose them to the governor | PRD-F-11 | IT (recorded-socket replay harness) | P1 detect/P3 events |
| FR-SHELL-3 | Caelestia backend: detect running shell, `set` routes through the Caelestia CLI (`caelestia wallpaper -f <file> [-m <monitor>]` — exact flags pinned at P3 entry, OQ-2), honors `CAELESTIA_WALLPAPERS_DIR`, optional theme-refresh hook | PRD-F-12 | IT (CLI stub in tests) + HW | P3 |
| FR-SHELL-4 | Auto-detect order `caelestia → hyprland → generic-layer-shell`; config `shell.backend` overrides; `auto` is the default | PRD-F-13 | UT + IT | P3 |
| FR-SHELL-5 | Generic layer-shell backend: full set/get/pause on any layer-shell compositor with no environment-specific calls | PRD-F-13 | IT (headless wlroots) | P3 |

### FR-LIVE — animated content

| ID | Requirement | Source | Verify | Phase |
|---|---|---|---|---|
| FR-LIVE-1 | GIF/APNG renderer decodes off the render thread into a **bounded** frame cache (cap in MB, configurable), compressed (lz4 or zstd); exceeding the cap evicts oldest frames and falls back to stream-decode | PRD-F-14 | UT (cache cap unit tests) + GIT | P4 |
| FR-LIVE-2 | Video pipeline uses GStreamer (`uridecodebin`/`playbin` → GL or dma-buf caps) with VA-API when available; decoder name is queryable and surfaced to the GUI/CLI | PRD-F-15 | IT (pipeline capability probe test) + BENCH | P4 |
| FR-LIVE-3 | If hardware decode is unavailable, video still plays via software decode and the daemon reports `decode: software` in `get`/GUI status | PRD-F-15 | IT | P4 |
| FR-LIVE-4 | Fit modes fill/fit/center/stretch implemented per TRD NFR-UX-2 | PRD-F-15 | GIT | P4 |
| FR-LIVE-5 | Playback IPC: `play`, `pause`, `seek <t>`, `loop <a> <b>`, per output; commands are idempotent | PRD-F-16 | UT + IT | P4 |
| FR-LIVE-6 | Video frame production is paced by Wayland frame callbacks and the governor FPS cap — never free-running | PRD-F-15/20 | IT (frame-count assertions over a window) | P4 |
| FR-LIVE-7 | WGSL shader packs: `shader.toml` (name, params, fps-hint) + `shader.wgsl`; params validated against declared types; uniform buffer generated from params; hot-reload on file change with inotify | PRD-F-17 | UT + GIT | P5 |
| FR-LIVE-8 | Shader parameter UI is generated from `shader.toml` (sliders/colors/bools) without code changes | PRD-F-17 | IT (GUI mock) | P5 |

### FR-GOV — resource governor

| ID | Requirement | Source | Verify | Phase |
|---|---|---|---|---|
| FR-GOV-1 | Governor evaluates rules over an event stream (Hyprland events, UPower, DPMS, idle timer, CPU sampler) and emits `RenderPolicy` per output (FPS cap / pause / dim) | PRD-F-20..24 | UT (rule-table tests) | P6 |
| FR-GOV-2 | Fullscreen-on-focused-output ⇒ pause that output by default; configurable to `dim`, `fps <n>`, or `ignore` | PRD-F-21 | UT + IT (replayed events) | P6 |
| FR-GOV-3 | Battery rules: `pause-on-battery`, `pause-below <pct>`, `fps-on-battery <n>` via UPower | PRD-F-22 | UT + IT (fake UPower) | P6 |
| FR-GOV-4 | DPMS-off output ⇒ zero frame production; on resume, content restores without reload | PRD-F-23 | IT | P6 |
| FR-GOV-5 | Eco mode applies a named preset atomically; every rule it sets is user-visible and individually overridable | PRD-F-24 | UT | P6 |
| FR-GOV-6 | Daemon exposes per-output stats (FPS, decode path, buffer count, RSS) over IPC | PRD-F-25 | IT | P6 |
| FR-GOV-7 | Governor state changes are event-driven; the governor never polls faster than 1 Hz | PRD-F-20 | UT | P6 |

### FR-OPS — packaging & lifecycle

| ID | Requirement | Source | Verify | Phase |
|---|---|---|---|---|
| FR-OPS-1 | systemd user unit `owe.service` (daemon) with socket-activation-ready design; GUI autostart optional via tauri-plugin-autostart | PRD-F-30 | IT (systemd --user in CI container) | P6 |
| FR-OPS-2 | Packages build reproducibly from source for `.deb` (Zorin OS 18 / Ubuntu 24.04 — first-class), Arch (AUR), Fedora (rpm/COPR), Nix (flake), cargo | PRD-F-32 | CI release job | P7 |
| FR-OPS-3 | Bench harness `owe bench` runs awww/hyprpaper/mpvpaper/owed over identical workloads and emits JSON + Markdown report | PRD-F-31 | BENCH | P6 |

---

## 3. Non-functional requirements

| ID | Requirement | Verify | Phase |
|---|---|---|---|
| NFR-PERF-1 | Static idle: zero scheduled render work after initial present (PRD §4 table) | IT + HW | P1, re-run P6 |
| NFR-PERF-2 | 1080p30 video ≤ 5% of one core with hw decode on Reference Profile | BENCH | P6 — `UNVERIFIED` until then |
| NFR-PERF-3 | Daemon RSS ≤ 30 MiB static / ≤ 120 MiB video (Reference Profile) | BENCH | P6 — `UNVERIFIED` |
| NFR-PERF-4 | Switch latency < 300 ms to first frame | BENCH | P6 — `UNVERIFIED` |
| NFR-PERF-5 | All numbers published with hardware + compositor versions, workload scripts, and raw data | BENCH | P6 |
| NFR-REL-1 | Daemon crash on one output must not kill other outputs; supervisor restarts the output worker in place | IT (fault injection) | P2 |
| NFR-REL-2 | Any IPC client may crash/disconnect at any time without daemon state corruption | IT (chaos test) | P1 |
| NFR-REL-3 | Daemon exits nonzero with a readable error on unsupported compositor; never renders on a wrong layer | IT + HW | P1 |
| NFR-SEC-1 | Socket under `$XDG_RUNTIME_DIR/owe/`, mode 0600; identical-user check on connect | UT + IT | P1 |
| NFR-SEC-2 | Config/library/shader parsing is fuzzed (cargo-fuzz targets for config + IPC frames + shader.toml) | fuzz CI job | P4+ |
| NFR-SEC-3 | Plugin ABI (post-1.0) runs code out-of-process or in WASM sandbox; in-process dylib mode requires an explicit config escape hatch | design review | P7+ |
| NFR-RES-1 | Frame cache / buffer pools have hard byte caps from config; OOM-under-normal-workloads is a bug | UT | P4 |
| NFR-COMPAT-1 | Support matrix (PRD §5): Hyprland + Caelestia CI-tested per release; generic layer-shell smoke-tested in headless CI | CI + HW | every phase |
| NFR-COMPAT-2 | IPC protocol is versioned; minor versions backward compatible; breaking changes bump major and both binaries negotiate | UT | P1 |
| NFR-CODE-1 | Workspace builds warning-free (`clippy -D warnings`), rustfmt clean, ≥ 70% line coverage on `owe-core`, `owe-governor`, `owe-ipc` (coverage is a hygiene metric, not a goal in itself) | CI | from P0 |
| NFR-A11Y-1 | GUI: keyboard-navigable, respects light/dark, min contrast 4.5:1 — enforced in the UI redesign wave (UI-DESIGN §5) | manual + axe | P2 simple UI is exempt; post-core wave enforces |
| NRF-I18N-1 | GUI strings are externalized from day one (i18n-ready) even if only English ships | UT | P2 |

---

## 4. Compatibility & migration notes

- **hyprpaper coexistence:** the Hyprland backend must detect a running hyprpaper and either warn (default) or stop it, per config `shell.hyprland.hyprpaper = warn | stop | ignore`. We do not silently kill another tool's process.
- **swww/awww coexistence:** detection + warning only; OWE never modifies another daemon's state.
- **Caelestia themes:** when routing through `caelestia wallpaper`, Caelestia's own theming pipeline runs — OWE must not also fire matugen in that mode (double-theme bug class; tested in P3).
- **Tauri v2:** pinned major; minor bumps allowed after GUI test pass. WebViewKit/WebKitGTK version differences on distros are a known risk (STRATEGY §3).
- **Dev platform:** Zorin OS 18 (Ubuntu 24.04 LTS base, ADR-015) — `webkit2gtk-4.1` (Tauri v2 requirement) and the GStreamer/VA-API plugin sets are available via apt; dependency install lists in docs use apt naming first, Arch second. CI runners (ubuntu-24.04) match this base exactly.

---

## 5. Acceptance & traceability

- A requirement is **done** when its Verify method passes in CI (or HW checklist for matrix items) and the phase gate in `IMPLEMENTATION-PLAN.md` is signed off.
- Coverage matrix: PRD-F-* ↔ FR-* ↔ phase tasks ↔ tests is maintained in `IMPLEMENTATION-PLAN.md` §9; a PRD feature with no requirement ID is not shippable.
- Any deviation from these requirements mid-development requires an ADR in `ARCHITECTURE.md` §9 — no silent drift.
