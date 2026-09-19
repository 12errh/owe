# Reference-Code Map — how OWE uses the studied repos

**Status:** ACTIVE. This is the single section that explains, end to end, how the agent (or a human) gets and uses reference code during development. Companion to [ATTRIBUTION.md](../ATTRIBUTION.md) (provenance ledger) and ADR-013 in [ARCHITECTURE.md](./ARCHITECTURE.md).

---

## 1. The workflow (how reference code reaches development)

1. **Clones live inside the workspace.** `./scripts/fetch-reference.sh` shallow-clones all six studied repos into `reference/` (gitignored, **never compiled**, never committed). Because `reference/` is inside the project, the agent reads those files directly with its normal file tools — no special access, no network calls during development.
2. **This map is the index.** Each repo's section below lists the exact files that implement what OWE needs, which OWE crate/phase consumes them, and a **verdict**:
   - **PORT** — copying this code is planned and legal; at port time: preserve the original license/copyright header in the ported file, add a row to `ATTRIBUTION.md` §2, and check the box here.
   - **READ-ONLY** — learn the design, write our own code from scratch. No copying of code, including "with small changes".
   - **AVOID** — do not port; recorded reasons (usually license or scope).
3. **Two usage modes, both visible in the ledger:** reading is free and needs no entry; copying always produces an `ATTRIBUTION.md` §2 row.
4. **Mechanical notes for the agent:** repo-wide code search skips gitignored paths (like `reference/`), so **navigate by the exact paths in this map + direct file reads** (e.g. `read_files reference/swww/common/src/cache.rs`). Direct reads always work.
5. **Gate reviews** (IMPLEMENTATION-PLAN §0) include: "ATTRIBUTION ledger still accurate? Any port missing a header or row?" — provenance drift is treated as a gate failure.
6. **Re-pinning** a repo (rare) = update pin in `scripts/fetch-reference.sh` + `ATTRIBUTION.md` §1 + re-verify all verdicts of that repo here, in one change.

---

## 2. Port map by repo

Paths verified against the pinned commits (see ATTRIBUTION §1). File names can drift upstream — re-pin procedure above applies.

### 2.1 swww (awww) — GPL-3.0 → PORT

| Reference file | What it contains | Feeds OWE | Phase | Verdict |
|---|---|---|---|---|
| `reference/swww/common/src/cache.rs` | GIF/APNG decoded-frame cache with lz4 compression | `owe-media` animated cache | P4 | **PORT** |
| `reference/swww/common/src/compression/` | lz4 frame-compression helpers | `owe-media` | P4 | **PORT** |
| `reference/swww/common/src/ipc/` | socket protocol: messages, serialization, client/daemon handshake | `owe-ipc` (design + code) | P0/P1 | **PORT** |
| `reference/swww/common/src/mmap.rs` | shared-memory buffer handling | `owe-render` (wl_shm path) | P1/P4 | **PORT** |
| `reference/swww/daemon/src/animations.rs` + `animations/` | transition engine: wipe/fade/center/outer, angle/step/fps | `owe-render` transitions | P2 | **PORT** |
| `reference/swww/daemon/src/wallpaper.rs` + `wallpaper/` | per-output wallpaper rendering state machine | `owed` output workers | P1 | **PORT** |
| `reference/swww/daemon/src/wayland.rs` | SCTK wiring: layer-shell surfaces, outputs, event queue | `owed` + `owe-shell-generic` | P1 | **PORT** |
| `reference/swww/client/src/imgproc.rs` | image decode/resize before caching | `owe-media::ImageDecoder` | P1 | **PORT** |
| `reference/swww/daemon/src/output_info.rs` | output enumeration/metadata | `owe-shell-*` | P1 | **PORT** |

### 2.2 wpaperd — GPL-3.0 → PORT

| Reference file | What it contains | Feeds OWE | Phase | Verdict |
|---|---|---|---|---|
| `reference/wpaperd/daemon/src/config.rs` | TOML config model, validation, per-output sections, hot-reload | `owe-core::config` | P0/P2 | **PORT** |
| `reference/wpaperd/daemon/src/render/` | OpenGL-ES render loop, gl-transitions integration | `owe-render` transitions (port transition *shaders* + logic to wgpu) | P2 | **PORT (adapt to wgpu)** |
| `reference/wpaperd/daemon/src/surface.rs` | layer-shell surface + EGL setup | `owed` output workers | P1 | **PORT** |
| `reference/wpaperd/daemon/src/image_loader.rs` | async image decode/resize | `owe-media` | P1/P2 | **PORT** |
| `reference/wpaperd/daemon/src/filelist_cache.rs`, `image_picker.rs` | directory cycling, random queue, sorting | `owe-core` playlists (P7) | P7 | **PORT later** |
| `reference/wpaperd/ipc/src/lib.rs` | `wpaperctl` IPC protocol | `owe-ipc` design comparison | P0 | READ-ONLY (swww's IPC is our port base) |
| `reference/wpaperd/daemon/src/wallpaper_info.rs`, `display_info.rs`, `wallpaper_groups.rs` | wallpaper metadata, output info, groups | `owe-core` model | P2 | **PORT** |

### 2.3 wallr — MIT → PORT

| Reference file | What it contains | Feeds OWE | Phase | Verdict |
|---|---|---|---|---|
| `reference/wallr/wallr-core/src/renderer/` | **wgpu** renderer on layer-shell | `owe-render` | P1/P2 | **PORT** (closest to our wgpu choice) |
| `reference/wallr/wallr-core/src/daemon/` | daemon lifecycle, Unix-socket server, supervisor | `owed` | P0/P1 | **PORT** |
| `reference/wallr/wallr-core/src/ipc/` | socket protocol + CLI client | `owe-ipc` + `owectl` | P0/P1 | **PORT** |
| `reference/wallr/wallr-core/src/shader/` | WGSL shader-pack runtime | `owe-render` shader runtime | P5 | **PORT** |
| `reference/wallr/wallr-core/src/video/` | FFmpeg (libav*) hw-decode integration | `owe-media::FfmpegDecoder` | P4 | **PORT** |
| `reference/wallr/wallr-core/src/animated/` | GIF pacing to frame boundaries | `owe-media` + workers | P4 | **PORT** |
| `reference/wallr/wallr-core/src/config/`, `theme/` | config model; matugen/wallust/pywal hooks | `owe-core::config`; theme hook (post-1.0) | P0 | **PORT** |
| `reference/wallr/benchmarks/`, `scripts/` | benchmark harness vs awww | `owe-bench` | P6 | **PORT (pattern + scripts)** |

### 2.4 phonto — GPL-3.0 → PORT

| Reference file | What it contains | Feeds OWE | Phase | Verdict |
|---|---|---|---|---|
| `reference/phonto/src/backend/wayland/` | GStreamer→EGL zero-copy video pipeline (VA-API), layer-shell surface | `owe-media::GStreamerDecoder` + `owe-render` dma-buf import | P4 | **PORT** |
| `reference/phonto/src/plan.rs`, `displays.rs`, `scale.rs` | multi-display planning, aliases, fit/scale modes | `owe-core` model + `owe-render` fit modes | P2/P4 | **PORT** |
| `reference/phonto/src/config.rs` | TOML config (search paths, `[[display]]` blocks) | `owe-core::config` shape reference | P0/P2 | READ-ONLY (wpaperd is our port base) |
| `reference/phonto/src/backend/macos/`, `src/macos_live_lockscreen/` | macOS specifics | — | — | AVOID (out of scope, PRD §6) |

### 2.5 waywe-rs — MIT → PORT (selective)

| Reference file | What it contains | Feeds OWE | Phase | Verdict |
|---|---|---|---|---|
| `reference/waywe-rs/crates/waywe-daemon/src/shaders/` | Shadertoy-style scene/shader runtime | `owe-render` shader runtime (with wallr's) | P5 | **PORT** |
| `reference/waywe-rs/crates/video/src/acceleration/`, `hardware.rs` | libva/VA-API hw-accel selection logic | `owe-media` capability probe | P4 | **PORT** |
| `reference/waywe-rs/crates/waywe-daemon/src/wallpaper/`, `wallpaper_app.rs`, `event_loop.rs` | daemon render loop, scene apps | `owed` workers | P1 | READ-ONLY (heavy unsafe; we keep swww/wallr as port bases) |
| `reference/waywe-rs/crates/waywe-ipc/` | command/profile protocol | `owe-ipc` design comparison | P0 | READ-ONLY |
| `reference/waywe-rs/crates/extractor/`, `project-parser/`, `dxt/` | Wallpaper Engine asset parsing | post-1.0 WE-plugin (PRD-F-34) | post-1.0 | DEFERRED (revisit via ADR) |

### 2.6 we-layerd — ⚠️ NO LICENSE → READ-ONLY, never copy

**All rights reserved at pin (82f9a06).** Value is architectural; every lesson below is implemented by writing our own code (the listed files are *reading material*, not ports):

| Read (never copy) | Lesson taken |
|---|---|
| `reference/we-layerd/crates/we-renderer/src/` | dma-buf-first with shm fallback, bounded in-flight frames, per-output worker isolation, hotplug reconciliation |
| `reference/we-layerd/crates/we-core/src/config.rs`, `playlist.rs`, `profile.rs`, `wallpaper/` | governor ruleset shape (fullscreen/focus pause), per-wallpaper settings, playlists |
| `reference/we-layerd/apps/we-gui/` | GUI↔runtime split UX: apply/switch without restart, tray, user-property UI generation |
| `reference/we-layerd/contrib/systemd/`, `package/ubuntu/` | unit + Ubuntu packaging reference (distro = our `.deb` target, ADR-015) |

If upstream adds a license later: re-verify via the re-pin procedure; any change of verdict requires an ADR.

---

## 3. Usage examples (the loop in practice)

**During P4 (video):** open this map §2.4 → `PORT` row points at `reference/phonto/src/backend/wayland/` → read the pipeline construction → port the pieces into `owe-media::GStreamerDecoder`, keeping GPL headers → add ATTRIBUTION §2 row → check the box here if the row is complete.

**During P0 (IPC):** §2.1 points at swww's `common/src/ipc/` and §2.3 at wallr's `ipc/` → port swww's framing as the base (GPL), compare against wallr's (MIT) → record both.

**When unsure about a file not listed here:** treat as READ-ONLY until a maintainer extends this map — extending the map is a docs change, not a code change.
