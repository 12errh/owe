# Project Plan — Low-Resource Live Wallpaper Engine

> ⚠️ **SUPERSEDED** — this was the original research + first plan. The authoritative doc set is now: [README](./README.md) → [PRD](./PRD.md) → [TRD](./TRD.md) → [ARCHITECTURE](./ARCHITECTURE.md) → [BACKEND-DESIGN](./BACKEND-DESIGN.md) → [IMPLEMENTATION-PLAN](./IMPLEMENTATION-PLAN.md) → [UI-DESIGN](./UI-DESIGN.md) → [STRATEGY](./STRATEGY.md). Where this document conflicts with those, those win. Kept for research provenance.

**Working name:** `lwpe` (rename later — nothing in the plan depends on the name)

A Wayland-native live wallpaper engine for Linux built with **Tauri v2 (React + Rust)** where:

- **Tauri v2 GUI (React)** = the control plane: wallpaper library, settings, previews, playback controls.
- **Rust wallpaper daemon** = the always-running part: renders the actual wallpaper on a `wlr-layer-shell` background surface.
- The daemon keeps running when the GUI is closed. The GUI is disposable; the wallpaper is not.

**Launch targets:**
1. **Hyprland** (wlroots-compatible, `wlr-layer-shell` + Hyprland IPC)
2. **Caelestia Shell** (Quickshell desktop shell on Hyprland)

Everything else is added through the plugin/registry layer below — **no compositor, shell, or renderer type is hardcoded anywhere.**

---

## 1. What the raw chat actually asked for (requirements extracted from `docs/rawchat.md`)

1. Tech stack is **Tauri v2**: Rust backend + React frontend (decided; do not relitigate).
2. **Do not build everything from scratch** — research existing wallpaper engines in Rust, take proven pieces from them.
3. Goal: **lowest resource usage achievable** — the raw chat is emphatic that the wallpaper must NOT be a webview/HTML renderer; it must be a native GPU renderer (wgpu) or a hardware-decoded video path.
4. Tauri = GUI/settings only. Never render the wallpaper through the webview.
5. Support **Hyprland first**, then **Caelestia Shell** — via a backend abstraction (`trait WallpaperBackend`), never hardcoded.
6. Follow the phased plan from the raw chat: MVP → library → animated → shader/procedural → resource manager → daemon split. Keep GUI and daemon as separate processes.
7. Recommended architecture from the chat is kept: `Tauri GUI ⇄ Unix IPC ⇄ Rust daemon ⇄ {Hyprland IPC, Caelestia IPC, wgpu renderer} → Wayland`.

### Resource targets (to verify with benchmarks, never promised blindly)

| State | Target |
|---|---|
| Static image idle | 0% CPU steady-state (render once, sleep) |
| Animated wallpaper | Frame-paced, ≤ ~2–5% of a core at 30 FPS (benchmark on real HW) |
| Daemon RSS | Small constant (config + Wayland buffers + GPU handles); no caching of whole video files in RAM |
| GUI closed | Wallpaper keeps running; GUI process fully exitable |
| Fullscreen app / battery | Wallpaper auto-pauses (configurable) |

> Rule from the raw chat kept as an engineering invariant: **the webview must never be the wallpaper surface.**

---

## 2. Research summary — existing engines and what we take from each

All of these are Rust or Rust-adjacent Wayland wallpaper engines. The takeaway table is the important part: what we **reuse / borrow / avoid** from each.

### 2.1 swww / awww — LGFae (Rust, GPL) — `github.com/LGFae/swww` (renamed awww, moved to Codeberg)

- Efficient animated-wallpaper **daemon + thin client**, controlled at runtime over a socket.
- Animated GIF/APNG wallpapers: **caches decoded frames** (lz4-compressed) then animates with low CPU.
- Runtime wallpaper switching without restarting the daemon; smooth **transition effects** (wipe, fade, center, outer, random) with configurable fps/step.
- Built on **smithay-client-toolkit**; originally grown out of SCTK's layer-shell example.
- Multi-format image support via the `image` crate (+ optional dav1d for AVIF).
- Status: mature but in maintenance mode — "new features will not be added"; author recommends forking for features.

**We take:** the daemon+client architecture, the socket IPC message design, the GIF frame-caching idea (with our own bounded cache), the transition-effect concept, and SCTK as the Wayland layer.
**We avoid:** its frame cache unboundedness for video, and its "no new features" stance (that's exactly why we exist).

### 2.2 wpaperd — Danilo Spinella (Rust, GPL-3.0) — `github.com/danyspin97/wpaperd`

- Daemon + `wpaperctl` CLI; **OpenGL ES** rendering with hardware-accelerated transitions.
- **Config hot-reload**, TOML config with per-output sections (`[DP-3]`, `[default]`, `[any]`, `re:` regex matching) — a very good config model.
- Directory-based cycling (duration, sorting, random queue), pause/resume, per-display wallpapers.
- **⚠️ Hyprland is explicitly NOT supported** by upstream (compositor quirks) — an important warning for us: our daemon must be tested *on Hyprland specifically*, since that is our primary target.

**We take:** TOML config shape + hot-reload, per-output config sections, directory cycling + pause/resume semantics, `wpaperctl`-style thin CLI, and the **gl-transitions** shader catalog for image transitions.
**We avoid:** OpenGL-ES/EGL rendering path (we use wgpu for Vulkan/GL portability) — but we keep EGL as a documented fallback backend.

### 2.3 wallr — programmersd21 (Rust, MIT) — `github.com/programmersd21/wallr`

- GPU wallpaper engine rendered with **wgpu on wlr-layer-shell**; daemon over a **Unix socket**; CLI auto-starts the daemon.
- Static → **render once and sleep** (near-zero idle overhead); GIF/video **pace to frame boundaries**.
- Video via **FFmpeg (libav*) with hardware-accelerated decode**; 6 GPU transitions; per-monitor wallpapers and scaling modes.
- Last wallpaper per output cached in `~/.cache/wallr/last_wallpaper/<OUTPUT>` for session restore.
- Correct layer-shell hygiene: `Layer::Background`, `KeyboardInteractivity::None`, empty input region.
- ships a **benchmark harness against awww** — a pattern we will copy.

**We take:** the wgpu-on-layer-shell rendering model, the event-driven "render once then sleep" static path, frame-boundary pacing, FFmpeg hw-decode integration approach, the socket daemon pattern, session-restore cache, layer-shell input hygiene, and the benchmark-harness idea.
**Why it matters:** wallr is the closest existing proof that our exact architecture (wgpu + layer-shell + daemon) works on Hyprland.

### 2.4 Phonto — museslabs (Rust) — `github.com/museslabs/phonto`

- GPU-accelerated **video** wallpaper: **GStreamer + EGL, zero-copy decode→render on the GPU** (VA-API); huge CPU win vs mpvpaper-style approaches.
- Streams/YouTube via yt-dlp; per-display config (`[[display]]` blocks + aliases); hotplug-aware ("display appears later" is not an error).
- **GLSL ES fragment-shader post-processing of video frames** (`u_tex`, `v_uv`, `u_resolution`) — shaders-on-video.
- **Battery-aware playback:** `--pause-on-battery`, `--pause-below PERCENT`.
- Writes current wallpaper path to a cache file; `--layer` selection (background/bottom/top/overlay).
- The reddit author explicitly built it because **mpv-paper used too much CPU** — evidence for our decode-path choice.

**We take:** GStreamer+VA-API as the video decode backend (Phase 4), the "pause on battery / below threshold" resource policy, GLSL-post-processing-on-video idea, current-wallpaper cache file convention, per-display config blocks with aliases.
**We avoid:** macOS specifics, YouTube/yt-dlp (maybe later, opt-in).

### 2.5 waywe-rs — hack3rmann (Rust) — `github.com/hack3rmann/waywe-rs`

- Daemon + CLI; images, h264/h265 video (libva hw-accel), transitions, **Shadertoy-style scene wallpapers**.
- **Scene wallpapers as packaged shared libraries** (`.ww` archives: compiled `.so` + assets) loaded at runtime — a plugin model for procedural wallpapers.
- Heavy unsafe/hardware-specific code; Dhall config; explicitly inspired by swww.

**We take:** the *concept* of a procedural/scene plugin API (we expose a **WGSL shader + optional Rust plugin** ABI instead of raw C FFI), the daemon+CLI split, and its libva experience as reference for hw-decode.
**We avoid:** heavy-unsafe-everything philosophy and Dhall config.

### 2.6 we-layerd — Aromatic05 (Rust) — `github.com/Aromatic05/we-layerd`

- A native **Wallpaper Engine (Steam Workshop) runtime** for Wayland without Wine: renders scene/video/web wallpapers; `we-gui` desktop app for browsing/config.
- Directly relevant systems knowledge: **DMA-BUF zero-copy presentation with shm fallback**, per-output isolated worker threads, hotplug reconciliation by output identity, **frame-callback-paced rendering with bounded in-flight buffers**, fractional scaling/viewport handling, MPRIS metadata + desktop-audio spectrum for reactive wallpapers, per-output **fullscreen/focus rules that pause or mute** the wallpaper, per-wallpaper user-property UI auto-generated from wallpaper-defined properties.
- GUI/runtime split exactly like our plan: GUI applies/switches wallpapers, runtime keeps playing.

**We take:** the resource-management ruleset (fullscreen/focus pause rules, bounded buffers, frame-callback pacing), DMA-BUF-first presentation strategy, output hotplug reconciliation, per-wallpaper settings store, and the "GUI closes, tray/runtime lives" UX.
**We avoid:** Wallpaper Engine asset parsing itself (huge scope; possible future plugin), CEF web wallpapers (violates our low-resource goal).

### 2.7 Reference C/C++ / non-Rust tools (context, not code)

- **hyprpaper** (hyprwm, C++) — official Hyprland static wallpaper utility, `hyprctl hyprpaper` IPC, preload+wallpaper commands. Our daemon must **coexist or replace** it cleanly; also the model for Hyprland-ecosystem conventions.
- **mpvpaper** (C, wlroots) — mpv as wallpaper; flexible but heavier CPU; the baseline we want to beat.
- **swaybg / wbg** (C) — minimal static wallpaper setters; proof of how tiny a static path can be.
- **Hyprland wiki "Wallpapers" page** lists hyprpaper, awww/swww, mpvpaper — our research aligns with the official ecosystem list.

### 2.8 What we build ourselves (the genuinely new part)

1. **Unified content model:** one daemon handling images + video + shaders/procedural + (later) plugin scenes, with per-content-type renderer modules behind one trait.
2. **Adaptive resource governor:** FPS caps, idle dimming/pause, fullscreen/battery/busy-CPU policies, per-monitor quality — combining ideas from Phonto (battery), we-layerd (focus rules), wallr (frame pacing) into one rules engine.
3. **Caelestia Shell integration** — no existing engine does this; it's our differentiator.
4. **Tauri v2 + React GUI over the daemon IPC** — modern management UX (library, live preview, per-monitor assignment) that the CLI-first tools above lack.

### 2.9 License notes (check before borrowing code, not just ideas)

- **MIT / permissive:** wallr (MIT), Phonto (permissive), swaybg (MIT), mpvpaper (GPL-2+ but we only interact via protocol/CLI), hyprland-rs, wgpu, smithay-client-toolkit (MIT).
- **GPL:** swww/awww (GPL), wpaperd (GPL-3). If we *copy code* from these, the daemon becomes GPL — fine if the whole project is GPL, **not fine if we want a permissive license**. Decision: keep our own code original, borrow **designs and small MIT-licensed snippets**; read GPL sources as documentation only, unless we decide the whole project ships GPL-3 (cheapest legally, very common for Linux desktop tools — decide before Phase 1 code).
- gl-transitions shaders: MIT — safe to bundle.
- ⚠️ Action item before Phase 1: **choose project license** (GPL-3 recommended for a Linux desktop tool that reads GPL code for reference).

---

## 3. Open-source projects & crates we use (the actual shopping list)

### Rust crates — daemon

| Crate | Purpose |
|---|---|
| `smithay-client-toolkit` (SCTK) | Wayland client toolkit: layer-shell surface, outputs, buffers, frame callbacks, hotplug |
| `wayland-client`, `wayland-protocols` | Underlying Wayland protocol machinery |
| `wgpu` | GPU renderer (Vulkan/GL fallback) for transitions, shaders, procedural content |
| `image` (+ `dav1d`/`libavif` feature) | Image decode (png, jpeg, webp, gif, avif, …) |
| `gstreamer`, `gstreamer-app`, `gstreamer-video` (+ `gst-plugin-va` runtime) | Hardware-decoded video pipeline (Phase 4) |
| `ffmpeg-next` *(fallback option)* | Alternative video decode backend if GStreamer proves too heavy in benchmarks |
| `lz4` (or `zstd`) | Compress cached animation frames (swww's trick) |
| `tokio` + `tokio::net::UnixListener` | Async daemon event loop + control socket |
| `serde`, `serde_json`, `toml` | Config + IPC serialization |
| `hyprland-rs` (or thin hand-rolled `hyprctl` socket client) | Hyprland IPC: monitors, events, dispatch |
| `inotify` / `notify` | Config hot-reload, library folder watching |
| `upower` via `zbus` | Battery state for pause-on-battery policy |
| `tracing`, `tracing-subscriber` | Structured logging |
| `clap` | Thin control CLI (`lwpe set …`, `lwpe pause`, …) |

### Rust crates — Tauri app

| Crate / plugin | Purpose |
|---|---|
| `tauri` v2 | App shell, IPC commands, window management |
| `tauri-plugin-shell` | (Only if ever needed) spawn/inspect the daemon — prefer direct socket |
| `tauri-plugin-dialog` | Native file/folder pickers |
| `tauri-plugin-fs` | Library folder access |
| `tauri-plugin-notification` | Desktop notifications |
| `tauri-plugin-autostart` | Start daemon at login |
| `tauri-plugin-store` (optional) | Small UI-prefs store (main config stays TOML in the daemon) |
| `tauri-plugin-process` | Daemon lifecycle control from GUI |

### Frontend (React)

| Package | Purpose |
|---|---|
| React 18 + TypeScript | GUI |
| Vite | Build tooling (Tauri v2 default template) |
| TanStack Query | Async state for daemon IPC calls |
| Zustand | UI state |
| Tailwind CSS v4 | Styling (fast to iterate, tiny output) |
| Radix UI (optional) | Accessible primitives where needed |
| `lucide-react` | Icons |

### Runtime integrations (non-Rust)

| Project | Role |
|---|---|
| **Hyprland** IPC sockets (`hyprctl` + event socket) | Monitor info, fullscreen/active-window events, workspace events for the governor |
| **Caelestia Shell** CLI/IPC (`caelestia wallpaper -f <file> [-m <monitor>]`, launcher `>wallpaper` flow, `$CAELESTIA_WALLPAPERS_DIR`) | Shell-native wallpaper switching + theme pipeline trigger |
| **quickshell** | The QML layer Caelestia runs on; we talk to the shell's CLI/IPC, not quickshell directly |
| **gl-transitions** (MIT) | Transition shader catalog for image swaps |
| **Shadertoy** community shaders | Seed content for the procedural renderer (respect author licenses per shader) |
| **matugen / wallust / pywal** (optional) | Theme generation hook when wallpaper changes (wallr/Phonto pattern) |
| **UPower** (D-Bus) | Battery state |
| **libnotify** | Notifications (via tauri plugin at the GUI level) |

### Media/thumbnail tooling (system deps, invoked or linked)

| Tool | Role |
|---|---|
| `ffmpeg` CLI (optional) | Thumbnail extraction for the library GUI (first-frame poster), probe metadata |
| `gstreamer` VA plugins runtime | hw decode — required for the video path (`gst-plugins-{base,good,bad,libav}`, `gst-plugin-va`) |

### Prior art we reuse as *documentation* (GPL — no code copying unless we go GPL)

- swww/awww frame-cache + transition design
- wpaperd config model + cycling semantics

---

## 4. Architecture

```
┌─────────────────────────────┐
│        Tauri v2 GUI         │
│   React + TS (Vite)         │
│                             │
│  • wallpaper library/grid   │
│  • per-monitor assignment   │
│  • settings / governor UI   │
│  • live preview             │
│  • tray (optional)          │
└──────────────┬──────────────┘
               │ Tauri IPC (commands)
┌──────────────▼──────────────┐
│   Tauri Rust backend        │
│   (control-plane commands,  │
│    thin client for daemon)  │
└──────────────┬──────────────┘
               │ Unix socket JSON (upgradeable later)
┌──────────────▼────────────────────────────────────┐
│              lwpe daemon (pure Rust, no GUI deps)  │
│                                                    │
│  ┌────────────┐  ┌───────────────┐  ┌───────────┐ │
│  │ config     │  │ content model │  │ governor  │ │
│  │ TOML +     │  │ library,      │  │ fps caps, │ │
│  │ hot-reload │  │ playlists,    │  │ pause     │ │
│  │            │  │ per-monitor   │  │ rules     │ │
│  └────────────┘  └──────┬────────┘  └─────▲─────┘ │
│                         │                 │       │
│  ┌──────────────────────▼─────────────┐   │       │
│  │        renderer registry           │   │       │
│  │  trait ContentRenderer { … }       │   │       │
│  │  ├─ StaticImage  (render 1×, sleep)│   │       │
│  │  ├─ AnimatedImage (cached frames)  │   │       │
│  │  ├─ Video (GStreamer → GPU)        │   │       │
│  │  ├─ Shader (WGSL)                  │   │       │
│  │  └─ Plugin (future, .so ABI)       │   │       │
│  └──────────────┬─────────────────────┘   │       │
│                 │                         │       │
│  ┌──────────────▼───────────┐   ┌─────────┴─────┐ │
│  │ wgpu surface per output  │   │ event bus     │ │
│  │ (wlr-layer-shell, BG     │   │ Hyprland/Cae- │ │
│  │  layer, no input region) │   │ lestia events │ │
│  └──────────────────────────┘   └───────────────┘ │
└───────────────────────┬───────────────────────────┘
                        │
        ┌───────────────┼──────────────────┐
        ▼               ▼                  ▼
   Hyprland IPC    Caelestia IPC     Wayland (SCTK)
        │               │                  │
        └───────────────┴────────┬─────────┘
                                 ▼
                             GPU / display
```

### Core abstractions (nothing hardcoded — this is the rule)

```rust
// A desktop environment integration. Hyprland and Caelestia implement this;
// new environments (niri, KDE, labwc…) are new crates/entries, zero core changes.
trait ShellBackend {
    fn name(&self) -> &'static str;
    fn detect(&self) -> bool;                     // is this environment active?
    fn monitors(&self) -> Vec<MonitorInfo>;
    fn events(&self) -> EventStream;              // fullscreen, focus, workspace…
    fn apply_wallpaper(&self, mon: &MonitorId, wp: &WallpaperRef) -> Result<()>;
    fn clear(&self, mon: Option<&MonitorId>) -> Result<()>;
}

// A wallpaper content type. Static/animated/video/shader/plugin implement this.
trait ContentRenderer {
    fn kind(&self) -> ContentKind;
    fn prepare(&mut self, surface: &SurfaceCtx) -> Result<()>;
    fn render(&mut self, frame: &mut FrameCtx);   // governor decides IF we render
    fn release(&mut self);
}

// Registry-driven backends; daemon picks via detection + config, never if-chains.
//   [shell]
//   backend = "auto"   # auto | hyprland | caelestia | <custom registered id>
```

- `ShellBackend` implementations: `hyprland` (via hyprland-rs / raw sockets), `caelestia` (CLI + its IPC), `layer-shell-generic` (fallback for any layer-shell compositor — covers sway/niri for free).
- `ContentRenderer` implementations map 1:1 to the phases below.
- Caelestia note: when Caelestia is active we can either (a) let our daemon draw on the background layer as usual and *notify* Caelestia for theme purposes, or (b) route through `caelestia wallpaper` for shell-managed switching. **Both are supported via `ShellBackend`; the mode is config, not code.**

### IPC contract (GUI ⇄ daemon)

- Unix socket at `$XDG_RUNTIME_DIR/lwpe/{pid}/socket` (mirrors Hyprland convention; no `/tmp` litter).
- Phase 1–5: newline-delimited JSON (`serde_json`) — simple, debuggable. Message schema versioned from day 1 (`{"v":1, …}`).
- Phase 6 (optional): swap to a binary codec (`postcard`/protobuf) behind the same trait if profiling says JSON matters. Don't prematurely optimize (raw chat's advice).

---

## 5. Resource-savings strategy (the reason this project exists)

1. **Event-driven, not timer-driven.** Static images: render once, commit, sleep the thread; no frame loop at all (wallr-proven). All animation paced by Wayland **frame callbacks**, never busy timers.
2. **Frame pacing & bounded work.** Cap FPS per content/monitor (15/24/30/60…). Never produce frames faster than the compositor consumes (bounded in-flight buffers — we-layerd lesson).
3. **GPU stays busy, CPU stays idle.** Video: GStreamer + VA-API decode directly into GPU memory (Phonto-proven; mpvpaper-style CPU decode is our benchmark to beat). Shaders: pure GPU, CPU only updates a time uniform.
4. **Bounded memory.** GIF/animated caches are capped (compressed with lz4/zstd, evictable); video frames are never fully cached; buffers sized to output — not to texture-max.
5. **Governor policies (all user-configurable, defaults conservative):**
   - pause on fullscreen app (Hyprland `activewindow*`/fullscreen events; we-layerd ruleset),
   - pause or drop-FPS on battery / below-charge-threshold (Phonto policy, UPower),
   - pause when display is DPMS-off,
   - idle dim/static-after-N-minutes mode,
   - per-monitor content + FPS (e.g., ultrawide = shader @30, laptop panel = static),
   - eco profile: one config switch applying a bundle of the above.
6. **GUI is disposable.** Closing Tauri changes nothing about playback; RAM the webview used is returned to the OS.
7. **Measure everything.** Ship a `lwpe bench` harness (hyperfine + `/proc` sampling, wallr-style script) comparing against awww/hyprpaper/mpvpaper; publish numbers in the README. No resource claims without a benchmark.

---

## 6. Roadmap (phases — each ends with something usable)

### Phase 0 — Foundations (days, not weeks)
- [ ] Decide license (recommend GPL-3.0; allows reading swww/wpaperd sources without legal ambiguity).
- [ ] Repo scaffold: Cargo workspace `crates/{lwped, lwpe-cli, lwpe-core, lwpe-shell-hyprland, lwpe-shell-caelestia, lwpe-render}` + `app/` (Tauri v2 + React).
- [ ] CI: cargo clippy/test + tauri build; rustfmt; conventional commits.
- [ ] `ShellBackend` + `ContentRenderer` traits, config schema (TOML), IPC message schema v1, skeleton daemon + CLI (`lwped`, `lwpe`).

### Phase 1 — MVP (the raw chat's Phase 1): static images, Hyprland
- [ ] SCTK layer-shell background surface (Background layer, no input), single monitor.
- [ ] StaticImage renderer: decode with `image`, draw via wgpu (and a pure `wl_shm` fast-path), render-once-then-sleep.
- [ ] Hyprland `ShellBackend`: monitor enumeration, set/clear, `hyprctl`-compatible coexistence checks.
- [ ] CLI: `lwpe set <img> [-m DP-1]`, `lwpe get`, `lwpe kill`.
- [ ] Tauri GUI skeleton: pick folder → grid of images → set. Daemon IPC v1 working end-to-end.
- [ ] **Exit criteria:** set a wallpaper on Hyprland from the GUI; daemon RSS small; 0% idle CPU.

### Phase 2 — Library + multi-monitor + transitions
- [ ] Scan library dirs (jpg/png/webp/avif/gif), SQLite (via `rusqlite`) metadata cache + thumbnails (small GPU/ffmpeg-generated, cached on disk).
- [ ] Per-monitor assignment UI; monitor hotplug re-apply (SCTK output events + governor).
- [ ] gl-transitions GPU transitions on wallpaper change (wgpu pipeline, wpaPERD-style catalog).
- [ ] Session restore: last-wallpaper cache per output (wallr convention).
- [ ] GUI: library page, monitor page, transitions preview, "apply to all".

### Phase 3 — Caelestia Shell backend
- [ ] `lwpe-shell-caelestia`: detect Quickshell/Caelestia; `caelestia wallpaper -f <file> [-m <monitor>]` integration; honor `$CAELESTIA_WALLPAPERS_DIR`; theme-refresh hook.
- [ ] Backend auto-detection chain: Caelestia → Hyprland → generic layer-shell (config overridable).
- [ ] GUI: backend status + mode selector (daemon-drawn vs shell-routed).
- [ ] **Exit criteria:** wallpaper set via Caelestia's own flow updates theme; daemon-drawn mode coexists.

### Phase 4 — Animated content: video + GIF/APNG
- [x] VideoRenderer: bounded GStreamer subprocess pipeline with FFmpeg fallback, software/hardware decode reporting, CPU RGBA frames, and wgpu/Wayland presentation. dma-buf zero-copy import remains a follow-up.
- [x] AnimatedImage renderer: decoded frame cache (bounded, optionally compressed), timing-aware playback, and frame-callback presentation.
- [x] Playback controls over IPC: play/pause/seek/loop, stats, CLI/Tauri/GUI surfaces, and session-safe replacement/clear.
- [ ] Governor integration: the playback clock exposes a hold hook, but fullscreen/battery policy belongs to Phase 6 and is not implemented yet.
- [x] FFmpeg backend module as fallback behind the shared media interface.
- [ ] **Exit criteria:** live GIF/video playback is verified; long-duration 1080p/4K RSS, multi-cap stability, hardware-reference performance, and zero-copy benchmarks remain open.

### Phase 5 — Shaders & procedural wallpapers
- [ ] WGSL shader-pack format: `shader.toml` (name, params, fps hint) + `shader.wgsl`; hot-reload on save.
- [ ] Uniform interface: `time`, `resolution`, `cursor` (optional), `audio` (later), user params auto-generated into GUI sliders/color pickers (we-layerd's user-property pattern).
- [ ] Bundle seed packs: aurora, fluid, particles, matrix, plasma (Shadertoy-derived, license-checked).
- [ ] Optional: Shadertoy-compatible GLSL→WGSL import path for community content.
- [ ] GUI: shader gallery with live preview at reduced resolution.

### Phase 6 — Resource governor + daemon hardening (the "killer feature" phase)
- [ ] Governor rules engine: fullscreen pause, battery/charge thresholds, DPMS, idle, busy-CPU (read `/proc/stat`), per-monitor profiles, eco mode.
- [ ] Hyprland event socket listener (`activewindow`, `fullscreen`, workspace) driving the governor.
- [ ] Systemd user unit (`lwped.service`) + Tauri autostart plugin; graceful socket handoff on reload.
- [ ] Tray menu (pause/resume/next/random) — optional, still zero-GUI-deps for the daemon.
- [ ] Benchmarks vs awww/hyprpaper/mpvpaper published; resource HUD in GUI (daemon-reported FPS/CPU/buffer stats).

### Phase 7 — Ecosystem & stretch goals (explicitly not hardcoded anywhere)
- [ ] Plugin ABI v0 (Rust `dylib` or WASM sandbox) for third-party content (waywe-rs `.ww` concept, safer).
- [ ] Wallpaper Engine (Steam Workshop) scene-format support as an optional plugin (we-layerd proves feasibility; huge scope — keep out of core).
- [ ] Playlists, time-of-day/random rotation (wpaperd semantics), image-of-the-hour.
- [ ] Additional `ShellBackend`s: niri, KDE (layer-shell + plasma API), labwc — registry entries only.
- [ ] Reactive wallpapers: MPRIS + audio spectrum uniforms (we-layerd pattern).
- [ ] Portals/screensaver integration, multi-GPU (PRIME) testing.

---

## 7. Concrete stack summary (final answer to "what do we use")

```
App & GUI      Tauri v2 · React 18 · TypeScript · Vite · Tailwind · TanStack Query · Zustand
Daemon         Rust (stable) · tokio · serde/JSON IPC · TOML config · rusqlite · tracing
Wayland        smithay-client-toolkit · wayland-client · wayland-protocols · wlr-layer-shell
Graphics       wgpu (Vulkan, GL fallback) · WGSL · gl-transitions (MIT)
Video          GStreamer (+gst-plugin-va) primary · FFmpeg fallback · `image` for stills
Integrations   hyprland-rs / Hyprland sockets · caelestia CLI/IPC · UPower (zbus) · matugen (opt)
Tooling        clap · inotify/notify · hyperfine benchmarks · systemd user units
```

**Chosen defaults (config-overridable, never hardcoded):**
- Shell backends shipped: `hyprland`, `caelestia`, `generic-layer-shell`; `auto` detection order: Caelestia → Hyprland → generic.
- Video backend: `gstreamer` (fallback `ffmpeg`).
- IPC: JSON-lines v1 (binary later if measured).
- GUI may be absent entirely — the daemon is the product; the GUI is its best friend.

---

## 8. Risks & open questions

| Risk | Mitigation |
|---|---|
| wpaperd explicitly doesn't support Hyprland (layer-shell quirks) | Our primary target IS Hyprland: test on it from Phase 1; wallr/we-layerd prove Hyprland+layer-shell works |
| GStreamer pipeline complexity / plugin availability across distros | FFmpeg fallback behind the same trait; document distro package lists (Phonto's lists are the template) |
| dma-buf import differences across drivers | Ship shm fallback from day 1 (we-layerd approach); hw path behind capability probe |
| JSON IPC overhead at high event rates | Schema-versioned from day 1; binary codec swap is a contained change |
| GPL contamination from reading swww/wpaperd | License decision before Phase 1; either go GPL-3 or treat GPL sources as docs only |
| Caelestia is fast-moving (dots repo, CLI surface changes) | Wrap its CLI in one module; integration tests pin behavior; degrade gracefully to generic backend |
| Scope creep (WE support, web wallpapers, plugins) | Keep them out of core phases; plugin ABI is Phase 7 |

## 9. Immediate next actions (when we start coding)

1. `license decision` → set `LICENSE` + crate headers.
2. Scaffold workspace + Tauri v2 React app; wire `lwped` skeleton + IPC v1.
3. Phase 1 static-image path end-to-end on Hyprland (daemon-only first, GUI second).
4. Benchmark harness stub from day 1 so every phase reports numbers.
