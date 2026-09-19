# OWE — Implementation Plan (Phase Gates & TDD)

**Status:** DRAFT v1. This is the *execution contract*: every phase has tasks and **hard exit-criteria gates**. A gate that is not met blocks the next phase — no exceptions without an ADR recording the decision and the debt taken.
**Companions:** [PRD](./PRD.md) · [TRD](./TRD.md) · [ARCHITECTURE](./ARCHITECTURE.md) · [BACKEND-DESIGN](./BACKEND-DESIGN.md) · [UI-DESIGN](./UI-DESIGN.md) · [STRATEGY](./STRATEGY.md)

---

## 0. How we work (the TDD loop — ADR-008, Pragmatic TDD)

**Every task below is marked `[T]` (TDD: write the failing test first) or `[I]` (implementation-first: graphics/glue where test-first is not meaningful).** This marking is the honest application of Pragmatic TDD, not an escape hatch:

- `[T]` cycle: write failing test → minimal implementation → refactor. Applies to: config, IPC, library, governor, shell-backend logic, CLI, state files — everything in `owe-core`, `owe-ipc`, `owe-shell-*` logic, `owectl`.
- `[I]` items: Wayland surface plumbing, wgpu pipelines, GStreamer graph wiring, Tauri scaffolding, GUI components. These get **integration tests, golden-image tests, or manual matrix checks written immediately after** (same phase, not "later"), per TRD's Verify column.
- Golden images: reference PNGs can't exist before the renderer does. Pattern: implement `[I]` → generate snapshot → **human reviews the snapshot once** → freeze as golden → subsequent changes are `[T]` against it. Documented here so nobody mistakes it for skipped testing.
- Each task merges with its tests green; `clippy -D warnings` + fmt enforced (TRD NFR-CODE-1).

**Gate review ritual (end of each phase):** run full suite + phase checklist → record results in this file's phase table (date + commit hash) → fix or ADR → only then start the next phase's first task.

---

## Phase 0 — Foundations (skeleton, CI, test harness)

**Goal:** repo, workspace, CI, and the *shapes* of the two core traits — so every later phase lands in a prepared field.

**Tasks** — all complete as of 2026-09-19; the measured evidence is in §0.1 below.
- [x] `[I]` Workspace scaffold per ARCHITECTURE §2; `LICENSE` (GPL-3.0), rustfmt/clippy config, `cargo-deny` skeleton, conventional commits, GitHub Actions: build+test+lint.
- [x] `[I]` Git setup: the repo **already exists at `https://github.com/12errh/owe`** (maintainer-created; do not create a new one and never rename it) — `git init` if needed, `origin` → `git@github.com:12errh/owe.git`, default branch `main`, initial push of the scaffold; CI workflows activate on push (ADR-016). *(Local repo initialised on `main` with `origin` set; the initial commit/push is left to the maintainer, so nothing was staged or committed.)*
- [x] `[I]` Tauri v2 + React app scaffold (`app/`); `pnpm` scripts; empty window launches; tauri plugins: dialog, fs, notification, autostart, process, store.
- [x] `[T]` `owe-core::config`: TOML parse → typed config → validation errors (unknown key, bad enum, bad path) — full test table from BACKEND-DESIGN §4 schema.
- [x] `[T]` `owe-core::model`: `ContentKind` from path/mime mapping; `WallpaperRef` resolution rules.
- [x] `[T]` `owe-ipc`: framing (encode/decode lines, 1 MiB cap, malformed → error frame), schema version negotiation — property tests + chaos client tests (NFR-REL-2).
- [x] `[I]` `owed` binary skeleton: calloop loop, IPC server hosting, `--check-config`, logging; exits with defined codes (BACKEND-DESIGN §8).
- [x] `[I]` Headless-Wayland CI job: wlroots headless + smoke script (connect, create surface, screenshot via wlr-screencopy) — proves the harness before P1 needs it.
- [x] `[I]` Golden-image helper in `owe-render` (offscreen texture → PNG diff tooling) — no golden images yet.
- [x] `[I]` `scripts/fetch-reference.sh` + `ATTRIBUTION.md` (ADR-013): pinned shallow clones of swww/awww, wpaperd, wallr, phonto, waywe-rs, we-layerd into gitignored `reference/` (never compiled); the ledger records repo, commit, license, and every ported piece. Ported code keeps its original license headers. *(Done pre-P0, 2026-09-19: all six cloned + pinned, licenses verified; the per-file port map with PORT/READ-ONLY/AVOID verdicts lives in [REFERENCE-CODE-MAP](./REFERENCE-CODE-MAP.md). Key finding: we-layerd has NO license → READ-ONLY, never port its code.)*

**Exit gate**
- [ ] CI green (lint, test, coverage, app, headless smoke) — **cannot be verified locally; it requires the first push.** Every job's commands were run by hand on the reference machine and pass (see §0.1); the gate item closes when the maintainer pushes, per ADR-016's honest caveat that gates are only auto-enforced once the repo is hosted.
- [x] `owed --check-config` validates a sample config; invalid configs exit 1 with precise message. *(Verified against the shipped `docs/examples/config.toml` and two invalid configs — §0.1.)*
- [x] A test client can connect to the daemon socket, `hello`, receive capabilities, disconnect — daemon logs it. *(Verified with the real binaries; `owectl hello` + daemon log excerpt in §0.1.)*
- [x] Config/IPC coverage ≥ 70% (NFR-CODE-1 baseline measured, enforced from here on). *(Measured 2026-09-19 with the CI command `cargo llvm-cov --package owe-core --package owe-ipc --fail-under-lines 70`: **92.64 % lines**, 92.31 % regions, gate exit 0. Per file: `config.rs` 94.51 %, `model.rs` 95.53 %, `path.rs` 94.37 %, `error.rs` 100 %, `frame.rs` 99.23 %, `protocol.rs` 96.76 %, `server.rs` 76.05 %, `client.rs` 78.79 %.)* **Honest caveat:** that scope is the two logic crates, by design. The integration tests that exercise `client.rs` hardest live in `owed`, so the true client coverage is higher than 78.79 % — and `owectl`/`owed` binaries are excluded from the floor entirely. The number is a floor to defend, not a claim about the whole tree.
- [x] Reference fetch script is idempotent; `ATTRIBUTION.md` initialized with the six repos + their licenses. *(Done pre-P0, 2026-09-19.)*

### 0.1 P0 evidence (measured on the reference machine, 2026-09-19)

| Gate item | Command | Observed result |
|---|---|---|
| Full suite | `cargo test --workspace` | **136 tests, 0 failed** across 9 test binaries |
| Coverage floor | `cargo llvm-cov -p owe-core -p owe-ipc --fail-under-lines 70` | **92.64 % lines** / 92.31 % regions, gate exit 0 (per-file breakdown in the gate item above) |
| Lint | `cargo fmt --all --check` + `cargo clippy --workspace --all-targets --all-features` | clean, zero warnings |
| Config validation (valid) | `owed --check-config --config docs/examples/config.toml` | `config OK: … (schema 1)`, exit 0 |
| Config validation (bad enum) | same, `backend = "nonsense"` | exit 1, `shell.backend: unknown value `nonsense` (expected one of: auto, caelestia, hyprland, generic-layer-shell)` |
| Config validation (unknown key) | same, `not_a_key = 1` | exit 1, `unknown field `not_a_key`, expected one of `backend`, `detect_order`, `hyprland`, `caelestia`` |
| IPC handshake | `owed` + `owectl hello` | `daemon 0.1.0 (ipc schema 1.0)` + capabilities; daemon logged `ipc client connected client=owectl` |
| Socket security | `ls -l $XDG_RUNTIME_DIR/owe/socket` | `srw-------` (0600, NFR-SEC-1) |
| Clean shutdown | `owectl kill` then `wait` on the daemon | `daemon exit=0` |
| Smoke harness | `./scripts/headless-smoke.sh --session` | PASS against the live Hyprland session; the layer-shell probe was **skipped** (no `wayland-info` locally) and the script now says so instead of over-claiming |
| GUI window | `app/src-tauri/target/debug/owe-app` | launched on the live session and stayed up (exit 124 from `timeout 8`), empty stderr |
| Frontend | `pnpm build` (tsc + vite) | typecheck clean, 227 kB JS / 71 kB gzip |
| App crate | `cargo build` in `app/src-tauri` | compiled clean (first build 8m09s) |
| GPU path | `cargo test -p owe-render` | 10/10; the two offscreen-render tests **ran for real on the Haswell iGPU** (Vulkan, `MESA-INTEL: warning: Haswell Vulkan support is incomplete`) rather than skipping |

### 0.2 P0 deviations and honest notes

1. **CI has five jobs, not three.** The gate text used to say "unit, headless smoke, lint"; the Tauri app needs its own job (it is not a workspace member) and NFR-CODE-1's coverage floor needs one. Gate wording updated above accordingly.
2. **`app/src-tauri` is excluded from the Cargo workspace** (`exclude = ["app/src-tauri"]`). Adding it would drag webkit/GTK into every daemon lint/test run. It depends on `owe-ipc` by path only (ARCHITECTURE §2) and is built by its own CI job.
3. **The socket-path rule now exists in two crates on purpose.** Clients use `owe_ipc::socket_path_in`/`default_socket_path` (so the GUI can depend on `owe-ipc` alone, per ARCHITECTURE §2) while the daemon keeps `XdgPaths::socket_path`. Two copies of a path rule is how daemon and client stop finding each other, so `crates/owed/tests/socket_path_agreement.rs` pins them together across the real XDG matrix.
4. **Capabilities currently advertise the registry, not working backends.** At P0 `hello` reports `shell backends: caelestia, hyprland, generic-layer-shell` and `media backends: auto, gstreamer, ffmpeg` even though none of them render yet — the field means "ids this build knows", as documented in `protocol.rs`, but a reader can hear a stronger claim. **P1 task added:** advertise only functional backends (or mark them `available: false`) so the status surface cannot mislead.
5. **`wayland-info` is mandatory in headless mode.** A gate that can silently skip its central assertion is not a gate; the script fails instead, and only `--session` mode is allowed to report a skipped probe.
6. **Tailwind is deferred to P1.** UI-DESIGN §3's simple-UI contract names Tailwind and arrives with GUI v0; P0 ships ~120 lines of plain CSS rather than adding a build-time dependency for a status bar.
7. **The app icons are generated placeholders** (`cargo run -p owe-render --example gen-app-icons`), committed so nothing is a mystery blob. Real branding still waits on the OQ-1 name check. Windows `.ico`/macOS `.icns` are intentionally absent until those platforms are targeted.
8. **Git state:** the local repo exists on `main` with `origin` set; the initial commit is made on `main` and **not pushed** (maintainer's decision), which is also what keeps gate item 1 honest.
9. **Local test tooling:** `cargo-llvm-cov` (0.9.1) and the `llvm-tools-preview` component were installed on the reference machine so the coverage floor is measurable locally, not only in CI. `sway`/`wayland-utils` are still absent there — the headless job runs in CI, and locally the smoke script's `--session` mode covers everything except the `wayland-info` layer-shell probe, which the script reports as skipped rather than passing off.

---

## Phase 1 — MVP: static images on Hyprland (v0.1)

**Goal:** `owectl set ~/img.png` and it appears on Hyprland; GUI shows a folder picker and applies. The raw chat's Phase-1 proof: Tauri → Rust → Wayland works.

**Tasks**
- [ ] `[I]` `owe-render`: wgpu device init (Vulkan→GL→lavapipe), layer-shell background surface, resize handling, one full-screen textured quad, buffer pool with in-flight cap.
- [ ] `[I]` `owe-media::ImageDecoder`: decode via `image` crate (+avif feature flag).
- [ ] `[T]` Output worker state machine: Idle→Preparing→Presented(Sleep)→Reconfigured, policy transitions — pure logic, fully unit-tested.
- [ ] `[T]` `owe-shell-hyprland`: output enumeration from `hyprctl monitors` text (parser = pure function, table-driven tests on recorded outputs; OQ-2-style pinning).
- [ ] `[T]` `owe-shell-generic` (minimal): output list via SCTK; used as fallback and for headless CI.
- [ ] `[T]` `owed` apply pipeline: `wallpaper.set` → validate → worker → reply; session-state write/read (`FR-LIB-5` file format lands here, restore in P2).
- [ ] `[T]` `owectl`: `set/get/monitors/pause/resume/kill` against in-process test server; then binary smoke on headless Wayland.
- [ ] `[T]` Capabilities honesty (closes P0 note §0.2.4): `hello` advertises only backends that actually work in this build, with the rest reported as known-but-unavailable — so the GUI status surface cannot over-claim.
- [ ] `[I]` GUI v0: folder picker → static list → apply; daemon status indicator. Simple styling per UI-DESIGN §3 (the "simple UI" contract).
- [ ] `[I]` Hyprland detection only (`shell.backend=auto` resolves to hyprland when `$HYPRLAND_INSTANCE_SIGNATURE` present).
- [ ] `[I]` README quickstart updated; first manual Support-Matrix run (HW) recorded in this file's gate table.

**Exit gate**
- [ ] E2E test (headless Wayland + synthetic Hyprland env): `owectl set` → screenshot contains the image (golden diff); `get` returns it; `kill` exits daemon code 0.
- [ ] Static idle: 60 s CPU sample shows zero scheduled frames (NFR-PERF-1, HW note filed).
- [ ] Layer-surface assertions pass (Background, no input region — TRD FR-CORE-5).
- [ ] GUI apply works on real Hyprland (maintainer machine: Zorin OS 18 daily-drives Hyprland + Caelestia, so real-HW gates are directly signable; HW checklist entry #1 signed).
- [ ] PRD-F-01..05, FR-CORE-1..7 all green.

---

## Phase 2 — Library, transitions, multi-monitor, GUI v1 (v0.2)

**Goal:** the app becomes *usable*: library with thumbnails, smooth GPU transitions, correct per-monitor behavior, a GUI worth opening.

**Tasks**
- [ ] `[T]` Library scanner: folder walk → SQLite rows; mtime incremental rescan; format filtering (owe-core, tempfile-based tests).
- [ ] `[T]` Thumbnail scheduler: dedup, off-thread queue, cache path rules (`FR-LIB-2`).
- [ ] `[T]` Output config resolution: exact → description → `any` precedence; conflict logging (table-driven).
- [ ] `[I]` Transition engine in `owe-render`: fade/wipe/slide/grow/wave/outer; interruptible mid-transition; per-change params.
- [ ] `[I]`→`[T]` Golden images for the 6 transitions (generate → review → freeze; then TDD against them) — TRD FR-LIB-3 GIT.
- [ ] `[T]` Hotplug supervisor: synthetic output events → worker spawn/teardown, wallpaper re-apply (`FR-LIB-4`); fault-injection test for NFR-REL-1.
- [ ] `[T]` Session restore on daemon start (`FR-LIB-5`).
- [ ] `[I]` GUI v1 (still simple UI per UI-DESIGN §3): library grid with thumbnails, per-output assignment, transition picker, apply-all; i18n string externalization starts (NRF-I18N-1).
- [ ] `[I]` GUI mock-daemon test harness (record/replay IPC) — GUI logic tested without a compositor.

**Exit gate**
- [ ] 500-file library scan < 2 s warm; rescan with 1 changed file touches only that row (measured in CI, not HW-dependent).
- [ ] All 6 transitions pass golden diffs at 3 sizes (1280×720, 1920×1080, 2560×1440).
- [ ] Hotplug: headless test plugs/unplugs output 20× — no leak (RSS stable ±5%), no lost outputs, restore correct.
- [ ] GUI v1 e2e: assign different wallpapers to 2 outputs, both applied (headless + HW).
- [ ] PRD-F-06..10, FR-LIB-1..6 green.

---

## Phase 3 — Caelestia backend & shell registry completion (v0.3)

**Goal:** the second launch target works natively; auto-detection chain complete; environment events start flowing (governor eats them in P6).

**Tasks**
- [ ] `[I]` **Gate prerequisite OQ-2:** pin Caelestia CLI surface against a live shell — record `caelestia wallpaper --help`, IPC behavior, `$CAELESTIA_WALLPAPERS_DIR` handling into a fixture file committed here.
- [ ] `[T]` `owe-shell-caelestia`: detection logic (quickshell process + caelestia binary + env); command construction from pinned fixtures; stubbed-CLI integration tests.
- [ ] `[T]` Shell registry + `auto` chain `caelestia → hyprland → generic-layer-shell` (FR-SHELL-4); unknown-backend config error text.
- [ ] `[T]` ShellBackend event bus: parse recorded Hyprland socket2 streams into typed events (replay harness; FR-SHELL-2).
- [ ] `[I]` Daemon-drawn vs shell-routed modes for Caelestia (`mode` config); double-theme guard test (TRD §4: when shell-routed, OWE skips matugen hooks).
- [ ] `[T]` hyprpaper/swww coexistence detection + `warn|stop|ignore` behavior (TRD §4).
- [ ] `[I]` GUI: backend status card, mode selector, detection override dropdown.
- [ ] `[I]` HW matrix runs: Caelestia + Hyprland both signed (checklist entries #2, #3).

**Exit gate**
- [ ] On a live Caelestia shell (maintainer runs Caelestia on Zorin 18 — signable directly): `owectl set` (shell-routed) changes wallpaper **and** Caelestia's theme updates once (no double-theme) — recorded screenshot.
- [ ] Auto-detect picks the right backend in 3 environments: Caelestia, Hyprland, headless generic (CI + HW).
- [ ] Registry test: registering a fake backend id + selecting it works; unknown id = precise config error (FR-SHELL-1 proof).
- [ ] Event replay: 10k recorded events parsed with zero mismatches vs fixture expectations.
- [ ] PRD-F-11..13, FR-SHELL-1..5 green.

---

## Phase 4 — Animated images & video (v0.4)

**Goal:** live wallpapers, decoded on the GPU path, with bounded memory — the resource thesis starts being measurable.

**Tasks**
- [ ] `[T]` `AnimatedImageDecoder`: frame stream, timing from container; ring-buffer cache with `animated_frame_cap_mb` + zstd/lz4 eviction (unit tests with synthetic tiny GIFs; FR-LIVE-1).
- [ ] `[T]` Cache overflow policy: stream-decode fallback triggers at cap; stats report mode `cached|streaming`.
- [ ] `[I]` Animated renderer in `owe-render`: frame upload pacing vs governor cap; golden-image frames for a fixed 5-frame fixture GIF.
- [ ] `[I]` `GStreamerDecoder`: uridecodebin→GL/dmabuf→appsink (bounded), audio dropped; decoder-name query. Capability-probe test (VA-API present ⇒ hw name; absent ⇒ software name — FR-LIVE-2/3).
- [ ] `[I]` dma-buf import path into wgpu with shm fallback; golden images for both paths on lavapipe (fallback) + HW spot-check.
- [ ] `[T]` `FfmpegDecoder` behind same trait; `media.backend=auto|gstreamer|ffmpeg` selection tests with fake plugin loader.
- [ ] `[T]` Playback IPC: play/pause/seek/loop idempotency + state machine tests (FR-LIVE-5).
- [ ] `[I]` Frame pacing integration test: count presented frames over 5 s at cap 30 ⇒ ≤151±2 frames (FR-LIVE-6).
- [ ] `[I]` GUI: playback controls, decode-path badge (hw/sw warning per FR-LIVE-3), fit-mode selector.
- [ ] `[I]` Fuzz targets live: IPC frames + config + `shader.toml` parser stub (NFR-SEC-2 starts early).

**Exit gate**
- [ ] 1080p video (H.264) plays 10 min on headless + HW: RSS within NFR-PERF-3 video budget ±10%, no unbounded growth (RSS graph filed).
- [ ] Frame pacing test green at caps 15/30/60 (three CI runs stable).
- [ ] Cache cap enforced: 4K GIF forced into `streaming` mode at default cap; RSS ≤ cap + overhead (UT + IT).
- [ ] decode-path reporting correct in `stats.get` + GUI badge (hw on VA-API machine, sw on lavapipe CI).
- [ ] **Hardware-decode reality check (Reference Profile, ADR-014):** the E5440's Haswell iGPU uses the legacy `i965` VA-API driver (upstream-archived 2023). If the capability probe resolves to software decode, this gate still passes via FR-LIVE-3 with the warning surfaced — and the hw-decode targets are re-evaluated in the P6 benchmark report rather than silently kept.
- [ ] PRD-F-14..16, FR-LIVE-1..6 green.

---

## Phase 5 — Shader wallpapers (v0.5)

**Goal:** procedural content — the feature that makes OWE interesting beyond managing other people's media.

**Tasks**
- [ ] `[T]` Shader-pack loader: `shader.toml` schema, param validation (types, ranges), error catalog; fuzz target (NFR-SEC-2).
- [ ] `[T]` Uniform-buffer layout generator from params (pure function, table tests).
- [ ] `[I]` WGSL runtime in `owe-render`: compile, built-ins (`u_time`, `u_resolution`, `u_fps`, opt-in `u_cursor`), hot-reload via inotify → recompile → transition swap.
- [ ] `[I]`→`[T]` Golden images for 3 reference shaders (generate→review→freeze; then TDD).
- [ ] `[I]` Daemon-side 512px offscreen preview render → PNG (feeds GUI gallery; BACKEND-DESIGN §6.4).
- [ ] `[I]` GUI: shader gallery with previews, auto-generated param controls (sliders/colors/bools — FR-LIVE-8), per-pack param persistence (`[shader_params]`).
- [ ] `[I]` Seed packs: aurora, particles, plasma, fluid, matrix — each with license file (OQ-3 resolved before merge).
- [ ] `[I]` Shadertoy GLSL→WGSL import helper (best-effort, documented limitations — no promise of full compatibility).

**Exit gate**
- [ ] Pack with 5 params: GUI renders matching controls; values persist across daemon restart (e2e).
- [ ] Hot-reload: edit `shader.wgsl` → new frame without restart ≤ 2 s (CI timer test + HW).
- [ ] All 5 seed packs render + pass golden diffs at 2 resolutions.
- [ ] Shader wallpaper idle CPU: GUI-reported and `/proc`-sampled ≤ 1% of one core at 30 FPS cap (first *local* verification of a PRD §4 target; full publish still at P6).
- [ ] PRD-F-17..18, FR-LIVE-7..8 green.

---

## Phase 6 — Governor, packaging & benchmarks (v0.6)

**Goal:** the differentiator phase — the resource governor works end-to-end, and we finally *publish* honest numbers.

**Tasks**
- [ ] `[T]` Governor rule table engine: ordered rules, last-match-wins, diffing before dispatch, manual override shadowing (BACKEND-DESIGN §7; FR-GOV-1, FR-GOV-7's 1 Hz ceiling).
- [ ] `[T]` Fullscreen rule against replayed Hyprland events (FR-GOV-2); battery rules with fake UPower (FR-GOV-3); DPMS + idle rules (FR-GOV-4); eco preset composition (FR-GOV-5).
- [ ] `[I]` Wire live event sources: shell event bus, UPower (zbus), DPMS, idle timer, opt-in CPU sampler (≤1 Hz).
- [ ] `[I]` Pause semantics in workers: video = decoder pause; shader = stop ticking; static = nothing to do; resume = no content reload (FR-GOV-4 restore test).
- [ ] `[T]` `stats.get` assembly per output (FR-GOV-6).
- [ ] `[I]` GUI: governor dashboard (policies per output, active rules, override button), eco toggle.
- [ ] `[I]` systemd user unit + socket-permission hardening (FR-OPS-1); CI test in systemd-enabled container.
- [ ] `[I]` `owe-bench`: workloads (static idle, 1080p30 video, shader, switch-latency), samplers (`/proc/<pid>/stat`, RSS, RAPL if available), competitor adapters (awww, hyprpaper, mpvpaper), JSON + Markdown report (FR-OPS-3).
- [ ] `[I]` **Run the benchmark on the Reference Profile; resolve OQ-4; publish report** in-repo (`docs/benchmarks/`).

**Exit gate**
- [ ] Governor replay suite: every rule has ≥1 passing scenario incl. override interplay; no policy flapping under event storms (10k events, policy changes ≤ expected count).
- [ ] Fullscreen open on Hyprland (HW): focused output pauses ≤ 100 ms after event; resumes with no reload — recorded.
- [ ] Battery: fake-UPower CI + one real HW unplug test.
- [ ] systemd: daemon starts via unit, socket perms 0600, restart-on-failure verified by kill test.
- [ ] Benchmark report published with raw data; every PRD §4 `UNVERIFIED` target either met with numbers or formally revised via ADR — **this is the honesty checkpoint**.
- [ ] PRD-F-20..25, FR-GOV-1..7, FR-OPS-1/3 green.

---

## Phase 7 — Release engineering & ecosystem (v1.0)

**Goal:** 1.0 that strangers can install and package.

**Tasks**
- [ ] `[I]` Packaging: `.deb` for Zorin OS 18 / Ubuntu 24.04 (first-class, ADR-015), AUR PKGBUILD, Fedora rpm spec (+COPR), Nix flake, cargo-install docs (FR-OPS-2); CI release job builds all.
- [ ] `[I]` Tray menu (pause/resume/next/random) via Tauri — daemon-independent (FR / PRD-F-33).
- [ ] `[T]` Playlists & timed rotation (wpaperd semantics: duration, sorting, queue) in owe-core (PRD-F-35).
- [ ] `[I]` Docs: user guide, shell-backend authoring guide (so "no hardcoding" is extensible by outsiders), troubleshooting per distro (Phonto-style dep lists).
- [ ] `[I]` UI redesign wave kickoff if core lands earlier than expected (see UI-DESIGN §5 — otherwise immediately post-1.0).
- [ ] `[I]` **OQ-1 name check resolved** before public announcement.
- [ ] `[I]` Security pass: dependency audit (cargo-audit), fuzz runs clean for 24 h, socket permissions reviewed.

**Exit gate**
- [ ] Fresh-VM install guide works on Zorin/Ubuntu 24.04 + Arch (HW checklist entries #4, #5).
- [ ] All packages build in CI from a clean checkout.
- [ ] v1.0 tag + release notes contain only benchmark-backed claims.
- [ ] PRD-F-30..33, 35 green (34/36 explicitly deferred with ADR if not done).

---

## Post-1.0 backlog (not gated, ADR-gated per item)

Plugin ABI v0 (PRD-F-34) · reactive uniforms (PRD-F-36) · Wallpaper Engine scene-format plugin · additional shell backends (niri/KDE/labwc) · UI redesign wave (PRD-F-40).

---

## 9. Traceability matrix (PRD feature → phase → proof)

| PRD feature | Phase | Proof lives in |
|---|---|---|
| F-01..05 | P1 | P1 gate + TRD FR-CORE-1..7 |
| F-06..10 | P2 | P2 gate + TRD FR-LIB-1..6 |
| F-11..13 | P1*/P3 | P3 gate + TRD FR-SHELL-1..5 |
| F-14..16 | P4 | P4 gate + TRD FR-LIVE-1..6 |
| F-17..18 | P5 | P5 gate + TRD FR-LIVE-7..8 |
| F-20..25 | P6 | P6 gate + TRD FR-GOV-1..7 |
| F-30, 31 | P6 | P6 gate + FR-OPS-1/3 |
| F-32, 33, 35 | P7 | P7 gate |
| F-34, 36, 40 | post-1.0 | backlog |

A feature appearing in a release without its gate row green is a process violation — fix the doc or revert the feature; no silent drift (TRD §5).

---

## 10. Gate sign-off log

| Phase | Date | Commit | Result | Notes / ADRs |
|---|---|---|---|---|
| P0 | 2026-09-19 | (initial commit pending maintainer) | **PASS, one item open** | All tasks + 4/5 gate items proven on real hardware (§0.1), including the 92.64 % coverage floor. Open: "CI green" — it can only be observed after the first push (ADR-016's hosting caveat). See §0.2 for the deviations and honest notes. |
| P1 | — | — | not started | |
| P2 | — | — | not started | |
| P3 | — | — | not started | |
| P4 | — | — | not started | |
| P5 | — | — | not started | |
| P6 | — | — | not started | |
| P7 | — | — | not started | |
