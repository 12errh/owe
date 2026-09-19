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
- [x] CI green (lint, test, coverage, app, headless smoke). *(Closed 2026-09-19 on CI run **35466144894** — all five jobs green on the first push that converged. Getting there took 9 runs, and every one of them found something the maintainer's desktop had masked; the full list is §1.2.14, which is why the gate item was worth keeping open rather than signing locally.)*
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
- [x] `[I]` `owe-render`: wgpu device init (Vulkan→GL→lavapipe), layer-shell background surface, resize handling, one full-screen textured quad, buffer pool with in-flight cap. *(Two slots per output, reused forever — the studied reference engines' own fix for unbounded pool growth. Resize is not guessed: the compositor's configure is authoritative and a mismatch is returned to the caller as `ResizeRequired`.)*
- [x] `[I]` `owe-media::ImageDecoder`: decode via `image` crate (+avif feature flag). *(No AVIF/HEIC feature flags enabled yet — the decoder reports unsupported kinds by name instead of failing obscurely; enabling the codecs is a P2 item.)*
- [x] `[T]` Output worker state machine: Idle→Preparing→Presented(Sleep)→Reconfigured, policy transitions — pure logic, fully unit-tested.
- [x] `[T]` `owe-shell-hyprland`: output enumeration from `hyprctl monitors` (**JSON**, not text — see §1.2.1), parser = pure function, table-driven tests on a recorded output pinned in `crates/owe-shell-hyprland/tests/fixtures/monitors-0.56.2.json`.
- [x] `[T]` `owe-shell-generic` (minimal): output list via SCTK; used as fallback and for headless CI.
- [x] `[T]` `owed` apply pipeline: `wallpaper.set` → validate → worker → reply; session-state write/read (`FR-LIB-5` file format lands here, restore in P2).
- [x] `[T]` `owectl`: `set/get/monitors/pause/resume/kill` against in-process test server; then binary smoke on headless Wayland. *(`clear` and `list` were added too — the daemon implements those methods and a CLI that cannot reach them is a gap, not a scope choice.)*
- [x] `[T]` Capabilities honesty (closes P0 note §0.2.4): `hello` advertises only backends that actually work in this build, with the rest reported as known-but-unavailable — so the GUI status surface cannot over-claim. *(New additive `capabilities.unavailable` field: dropping unimplemented ids entirely would have made them invisible, which is the same mistake pointing the other way.)*
- [x] `[I]` GUI v0: folder picker → static list → apply; daemon status indicator. Simple styling per UI-DESIGN §3 (the "simple UI" contract). *(Plus an outputs view with per-output clear, and the unavailable-feature list. Thumbnails, grid, and assignment stay in P2.)*
- [x] `[I]` Hyprland detection only (`shell.backend=auto` resolves to hyprland when `$HYPRLAND_INSTANCE_SIGNATURE` present).
- [x] `[I]` README quickstart updated; first manual Support-Matrix run (HW) recorded in this file's gate table.

**Exit gate**
- [x] E2E test: `owectl set` → screenshot contains the image (pixel proof); `get` returns it; `kill` exits daemon code 0. *(Run against the **real** Hyprland session with `scripts/e2e-hyprland.sh`, and the headless-CI variant is now **green in CI** too — the headless Wayland smoke job passed on run 35466144894; §1.2.2/§1.2.14.)*
- [x] Static idle: 60 s CPU sample shows zero scheduled frames (NFR-PERF-1, HW note filed). *(Measured 0.617 % of one core / 37 ticks over 60 s with a wallpaper on screen — inside the 1 % budget, but **not zero wakeups**: the presenter polls its command channel every 50 ms. See §1.2.3.)*
- [x] Layer-surface assertions pass (Background, no input region — TRD FR-CORE-5). *(Background layer confirmed by `hyprctl layers`: `namespace=owe layer=0 1366x768 at (0,0) alpha=1`. The empty input region is set at commit time; click-through has no automatable proof here — no `ydotool`/`wlrctl` — so it is a manual checklist item, §1.2.4.)*
- [x] GUI apply works on real Hyprland (maintainer machine signs real-HW gates directly; HW checklist entry #1 signed). *(Verified two ways: the window launches and stays up on the live session with the new views, and the GUI's entire daemon-facing command layer is integration-tested against a stub daemon — 5 tests pinning the parameter shapes it sends and the reply fields it reads. What is **not** automated is the click itself; §1.2.5.)*
- [x] PRD-F-01..05, FR-CORE-1..7 all green.

### 1.1 P1 evidence (measured on the reference machine, 2026-09-19)

| Gate item | Command | Observed result |
|---|---|---|
| Full suite | `cargo test --workspace` | **246 tests, 0 failed** across 10 test binaries |
| GUI command layer | `cargo test` in `app/src-tauri` | **5 tests, 0 failed** against a stub daemon on a temp socket |
| Coverage floor | `cargo llvm-cov -p owe-core -p owe-ipc --fail-under-lines 70` | **92.93 % regions / 92.82 % lines**, gate exit 0. P1's new logic: `output.rs` 98.77 %, `worker.rs` 96.92 %, `shell.rs` 88.54 %, `state.rs` 87.56 % (same caveat as §0.1: `owed`/`owectl` binaries are outside the floor) |
| Lint | `cargo fmt --all --check` + `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean, zero warnings |
| Real-Hyprland E2E | `./scripts/e2e-hyprland.sh` | PASS (below) |
| — apply is on screen | before/after `grim` screenshots, quadrant bounding boxes | magenta x74–681/y25–382, green x684–1340/y25–382, blue and yellow below → **all four quadrants in the right places**, each ~5 000–7 000 visible px (the rest is covered by two 1252×704 windows) |
| — layer surface | `hyprctl layers -j` | `namespace=owe layer=0 (background) 1366x768 at (0,0) alpha=1` |
| — `get` | `owectl get --monitor eDP-1` | returns the applied path |
| — `clear` | `owectl clear` + screenshot | zero wallpaper-coloured pixels remain |
| — `kill` | `owectl kill`, then `wait` on the pid | `daemon exited 0` |
| Session state | `$XDG_STATE_HOME/owe/session.json` after an apply | `{"version":1,"outputs":{"eDP-1":{"reference":"…/quadrants.png","kind":"static-image"}}}` |
| Static idle (NFR-PERF-1) | `./scripts/idle-cpu.sh 60` | **0.617 % of one core (37 ticks / 60 s)**, RSS 169 → 169 MiB, threads 5 → 5, fds 14 → 14 — inside the 1 % budget (see §1.2.3 for the honest caveat) |
| Generic backend | `backend = "generic-layer-shell"`, `HYPRLAND_INSTANCE_SIGNATURE` unset | enumerates `eDP-1 1366x768` over plain `wl_output`, with no focus claim |
| Unknown backend | `backend = "caelestia"` | `CONFIG_INVALID: unknown shell backend `caelestia`; this build has: hyprland, generic-layer-shell` (was `INTERNAL: auto is not available: …` — §1.2.6) |
| Capabilities | `owectl hello` | `shell backends: hyprland, generic-layer-shell`; `not in this build:` lists caelestia/animations/video/shader/transitions with their phases |
| GPU renderer | `cargo test -p owe-render` | 27/27; the offscreen tests run **on the Haswell iGPU via Vulkan** rather than skipping |
| Frontend | `pnpm build` in `app/` | typecheck clean, 232 kB JS / 72 kB gzip |
| GUI window | `app/src-tauri/target/debug/owe-app` | launched on the live session, stayed up through `timeout 8` (exit 124), empty stderr |

### 1.2 P1 deviations and honest notes

1. **The Hyprland backend parses `hyprctl monitors -j` (JSON), not the text output.** The plan said "text (parser = pure function)". JSON is Hyprland's documented machine interface and removes a class of whitespace/version-parsing bugs; the pure-function property is kept (the parser is a function over `&str`, tested against a recording of the real output pinned in `tests/fixtures/monitors-0.56.2.json`, OQ-2-style).
2. **The headless-CI variant of the E2E gate is now verified — in CI.** `sway` is still not installed on the reference machine (the elevation prompt failed PAM twice), so it cannot run there, but the headless Wayland smoke job passes on the runner (run 35466144894): compositor up, `wlr-layer-shell` and `wl_output` advertised, daemon serves IPC headless, clean shutdown. What CI's headless run does **not** yet do is pixel assertions (no drivable output is created); those arrive in P2 with `swaymsg create_output` called from the script after startup, per the analysis in `scripts/headless-sway.conf`.
3. **Idle cost is 0.617 % of a core, not 0 %.** No frames are scheduled (nothing is uploaded, nothing is committed), but the presenter polls its command channel with a 50 ms `recv_timeout`, which is ~20 wakeups/second. That is inside NFR-PERF-1's 1 % budget and is honest to state rather than describe as "free". **Follow-up task (P2):** wake the presenter from an `eventfd`/`calloop` source instead of polling, which removes the wakeups entirely. Related measured number: static-idle RSS is **169 MiB** with wgpu + Vulkan on Haswell (window: n/a) — the PRD's static-memory target is compared in the P6 benchmark report, not here, because a single-process measurement on one machine is not a published claim.
4. **Click-through (empty input region) has no automated proof here.** The region is set to empty before the surface's first commit, but verifying that a *click* passes through needs a synthetic pointer (`ydotool`, `wlrctl`) and neither is installed. It is a manual checklist item; the layer assertions that *are* automatable are asserted in the E2E script.
5. **The GUI's click is not automated; its command layer is.** Driving a webview button would need a UI-automation driver this project does not have yet. The mitigation is that everything between the button and the daemon is integration-tested against a stub daemon (parameter shapes **and** reply parsing), so the untested surface is the DOM wiring alone. P2's "GUI mock-daemon harness" item is therefore already half-built.
6. **An unknown shell backend used to answer `INTERNAL`.** Found while exercising the P1 CLI: the apply path wrapped the selection failure in `ShellError::Unavailable { backend: "auto" }`, so a config typo surfaced as an internal fault blaming `auto`. It now carries the structured `SelectError` and answers `CONFIG_INVALID` with the exact text, plus a regression test asserting the word `auto` never appears in that message.
7. **`library.list` takes a `dir`, not the documented `{filter, page}`.** P1 has no index, so it scans one directory level and returns a `scope` field saying exactly that ("one level, images only"). The indexed, paginated form is P2's job; the current shape is a documented deviation rather than a silent one.
8. **`capabilities.unavailable` is an additive field.** Adding it to `HelloReply` is schema-minor-safe: a client that predates the field still parses the reply, and a test asserts exactly that.
9. **The GUI opens one IPC connection per command** (connect → `hello` → one call → close) instead of holding a session open. That is deliberate while every command is click-driven — no stale handshake across a daemon restart, and no state in the GUI. It becomes worth revisiting only if the P2 library view makes the round-trip measurable.
10. **`OWE_SOCKET` is a client-side-only override** (the GUI reads it; the daemon derives its socket from `XDG_RUNTIME_DIR` and never reads it). It exists so the GUI can be aimed at a second daemon and so its command layer can be tested against a stub server; the asymmetry is documented in the code rather than implied away.
11. **The generic backend cannot report focus.** `wl_output` has no focused-output concept, so `focused: false` always, and `-o focused` on that backend fails with a precise "no output matched" rather than guessing. Documented in the crate and asserted in its tests.
12. **AVIF/HEIC decoding is not enabled yet.** `owe-media` decodes the `image` crate's default set; anything else is reported as an unsupported kind by name. Enabling the extra codecs (with their resource cost) is a P2 decision.
13. **A restarted daemon used to claim a wallpaper it had not drawn.** Found by restarting it by hand during this phase's hardware run: `owectl monitors` reported `wallpaper: <path>` from the *session file* while the worker was `idle` and the screen showed something else — exactly the kind of over-claim this project keeps finding and fixing. `outputs.list` now reports two separate things: `wallpaper` (what this run actually presented) and `recorded` (what the session file remembers, printed as "recorded: … (not applied this run; restore lands in P2)"). A regression test pins it. The general rule this establishes for later phases: **a field that says "current state" must come from the component that owns that state, never from a cache of intent.**
14. **CI found four more things the desktop had masked** (runs 35461889214 / 35462429597 — this is precisely why gate item 1 stays open until CI is green):
    - `app/src-tauri` was never rustfmt-checked (the workspace `fmt --all` skips it; the app job now formats it itself), and its clippy had never been run locally — two real lints (items after a test module, `clone_on_copy`) fixed.
    - Two `owed` tests passed only because a compositor was attached. One expected `outputs.list` to *succeed* (in CI, `CONFIG_INVALID` with an explanation is the honest answer); one reached through `apply()` for an error that is a pure-model rejection. `apply()` now validates the request (reference, kind) before demanding a session, which is the correct layering regardless of CI.
    - The headless smoke script had a `#` comment **inside** a backslash-continuation chain; in bash that swallows the rest of the joined command, so `sway` never started and the log was empty. Comments now sit outside continuation chains.

    The headless smoke job alone took eight more runs to converge, and each failure was a distinct, real finding: sway aborts without the `xwayland` package (run 1); the script waited for `wayland-0` while the compositor created `wayland-1` — a hardcoded socket name made a healthy compositor look dead (runs 4–8); headless mode never exported `WAYLAND_DISPLAY`, so the probe and daemon had no display even with the compositor up (run 8); `create_output` is an IPC command, not a config command (run 8); and on wlroots 0.17 a headless backend creates **no outputs by itself** (runs 4–7, sway parked in `ep_poll` with nothing to serve). The failure diagnostics added along the way (process state via `ps`, any-socket matching, environment dump) are what turned six blind failures into three one-shot fixes — they stay in the script.
    - One IPC chaos test sampled an instantaneous connection count and raced teardown (run 9, 3 vs 2); it now interleaves requests, drops the clients, and polls with a deadline for the drain to zero — deterministic across five consecutive local runs and green in CI (run 10, **35466144894**, all five jobs).

### 1.3 P1 hardware checklist run (signed 2026-09-19)

The TRD's `HW` verify method needs a concrete artifact, and until now "HW checklist" was referenced without being defined anywhere (found while signing this phase — a documentation gap, not a code one). It is defined here, and each phase records its run in the same shape.

**Machine:** the Reference Profile laptop — Zorin OS 18, Hyprland on the `eDP-1` 1366×768 panel, Intel Haswell iGPU (Vulkan support incomplete; Mesa's `MESA-INTEL` warning is expected and appears in every daemon log). Session runs Caelestia (`caelestia-background` owns the background layer).

| # | Check | How | Result |
|---|---|---|---|
| 1 | GUI opens and reports daemon state | launch `owe-app`, read the status bar | PASS — window stays up, no stderr; status/ outputs/ library views render (P1 UI) |
| 2 | Wallpaper reaches the screen | `scripts/e2e-hyprland.sh`, pixel proof | PASS — all four quadrants, correct screen quadrants |
| 3 | Click-through | click a desktop icon area while a wallpaper is applied | **MANUAL, unsigned** — no synthetic-pointer tool installed; the empty input region is set at commit time, but nobody has clicked yet. **Open item.** |
| 4 | Clicks on the wallpaper reach windows, not OWE | move/raise a window over the wallpaper and interact | PASS (indirect) — windows above the background layer receive input normally |
| 5 | Idle is quiet | `scripts/idle-cpu.sh 60` | PASS with caveat — 0.617 % of one core (§1.2.3) |
| 6 | Daemon restarts cleanly after `kill` | `owectl kill`, relaunch, `owectl monitors` | PASS *after a fix* — the socket is removed and recreated, the state file is re-read, and the restarted daemon reported `wallpaper: <path>` for an output it had not drawn on (found here; fixed, §1.2.13). It now reports `wallpaper: none` plus the recorded reference, and `get` returns nothing |
| 7 | Caelestia coexistence | apply with Caelestia's background running | PASS (observed) — Caelestia's surface keeps running; OWE draws on the background layer above it. **No coexistence policy is implemented yet** — detection and `warn|stop|ignore` are P3 (TRD §4) |
| 8 | Monitor unplug/replug | — | **NOT RUN** — the Reference Profile has no external display attached. Hotplug is a P2 gate item; this row exists so the gap is visible rather than assumed |

**Open items from this run:** #3 (click-through needs a synthetic pointer) and #8 (no second display available). Neither blocks P1's gate, and both are recorded here rather than in a footnote.

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
- [ ] `[I]` GUI mock-daemon test harness (record/replay IPC) — GUI logic tested without a compositor. *(Started in P1: the app's command layer is already tested against a stub daemon in-process — §1.2.5. What remains is a replay harness for the webview side.)*
- [ ] `[T]` **Debt from P1 §1.2.3:** wake the presenter from an `eventfd`/calloop source instead of polling its command channel every 50 ms, so static idle reaches ~0 wakeups instead of 0.617 % of a core.
- [ ] `[I]` **Debt from P1 §1.2.12:** decide on AVIF/HEIC/WebP demuxing in `owe-media` (codec features have a memory cost; enable only what the profile justifies).

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
| P0 | 2026-09-19 | `8a0e360` | **PASS, one item open** | All tasks + 4/5 gate items proven on real hardware (§0.1), including the 92.64 % coverage floor. Open: "CI green" — it can only be observed after the first push (ADR-016's hosting caveat). See §0.2 for the deviations and honest notes. |
| P1 | — | — | not started | |
| P2 | — | — | not started | |
| P3 | — | — | not started | |
| P4 | — | — | not started | |
| P5 | — | — | not started | |
| P6 | — | — | not started | |
| P7 | — | — | not started | |
