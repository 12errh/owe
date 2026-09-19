# OWE — Backend Design

**Status:** DRAFT v1. This is the implementation-level design of `owed` and its crates. It implements [ARCHITECTURE](./ARCHITECTURE.md); requirement IDs come from [TRD](./TRD.md); phase numbers from [IMPLEMENTATION-PLAN](./IMPLEMENTATION-PLAN.md).
**Companions:** [PRD](./PRD.md) · [TRD](./TRD.md) · [ARCHITECTURE](./ARCHITECTURE.md) · [IMPLEMENTATION-PLAN](./IMPLEMENTATION-PLAN.md) · [UI-DESIGN](./UI-DESIGN.md) · [STRATEGY](./STRATEGY.md)

---

## 1. Crate decomposition & responsibilities

| Crate | Contains | Must not contain |
|---|---|---|
| `owe-core` | config model + validation, content model (`WallpaperRef`, `ContentKind`), library scanner + DB, governor rule engine, session state, error types | any Wayland/wgpu/GStreamer call, any socket |
| `owe-ipc` | protocol types (serde), JSON-lines framing, `Client`, `Server`, schema versioning, chaos-testable framing | business logic |
| `owe-shell-*` | `ShellBackend` impls: output metadata, event streams, environment-specific apply semantics | rendering |
| `owe-render` | wgpu device/surface per output, transition engine, WGSL runtime, buffer pools, golden-image test helpers | decoding, config ownership |
| `owe-media` | `MediaDecoder` trait: `ImageDecoder` (image crate), `AnimatedDecoder` (frames), `VideoDecoder` (GStreamer primary, FFmpeg fallback) | Wayland |
| `owed` | calloop wiring, output workers, supervisor, IPC server host | algorithms (they live in the crates above) |
| `owectl` | CLI → `owe-ipc::Client` | anything else |
| `owe-bench` | workload runner, `/proc` + RAPL sampling, report generation | rendering paths of its own |

`app/src-tauri` depends only on `owe-ipc` (+ Tauri plugins).

---

## 2. Core trait definitions (the "nothing hardcoded" contract)

Signatures are design-level (exact generics/async may shift during implementation; the *shape* is the contract).

```rust
// owe-core
pub type OutputId = Arc<str>;            // e.g. "DP-1"
pub type BackendId = Arc<str>;           // "hyprland" | "caelestia" | "generic-layer-shell" | …

pub struct MonitorInfo {
    pub id: OutputId,
    pub description: Option<String>,     // EDID-ish, for config matching like wpaperd
    pub size_px: (u32, u32),
    pub scale: f64,
    pub refresh_mhz: u32,
}

/// A wallpaper reference: anything the daemon can render, resolved from the library.
pub struct WallpaperRef { pub kind: ContentKind, pub source: Source }
pub enum ContentKind { StaticImage, AnimatedImage, Video, Shader, Plugin /* post-1.0 */ }
pub enum Source { Path(PathBuf), LibraryItem(Uuid), ShaderPack(PathBuf) }

/// Policy the governor emits; output workers *only* obey, never decide.
#[derive(Clone, Debug, PartialEq)]
pub enum RenderPolicy {
    Run { fps_cap: FrameCap, dim: Option<f32> },   // FrameCap::Unlimited | Max(u32)
    Paused { reason: PauseReason },                 // shown in GUI stats
}
```

```rust
// owe-shell-<x>
pub trait ShellBackend: Send + Sync {
    fn id(&self) -> BackendId;
    fn detect(&self) -> DetectionConfidence;        // None | Weak | Strong
    fn monitors(&self) -> Vec<MonitorInfo>;
    /// Environment events (fullscreen, focus, workspace, battery-adjacent).
    /// Environments without an event socket return an empty stream.
    fn events(&self) -> BoxStream<'static, ShellEvent>;
    /// Environment-specific apply semantics (e.g. Caelestia shell-routed set).
    /// Daemon-rendered mode returns NotApplicable and the layer-shell path proceeds.
    fn apply_wallpaper(&self, out: &OutputId, wp: &WallpaperRef) -> Result<ApplyOutcome>;
    fn clear(&self, out: Option<&OutputId>) -> Result<()>;
}
```

```rust
// owe-render
pub trait ContentRenderer: Send {
    fn kind(&self) -> ContentKind;
    /// Prepare GPU resources for a surface. Called on the output worker thread.
    fn prepare(&mut self, ctx: &SurfaceCtx) -> Result<(), RenderError>;
    /// Produce the next frame. `frame.pacing` already encodes the governor's cap;
    /// return Want::Sleep when there is nothing to draw (static path = always Sleep after first).
    fn render(&mut self, frame: &mut FrameCtx) -> Result<Want, RenderError>;
    fn handle(&mut self, cmd: &PlaybackCmd) -> Result<(), RenderError>; // play/pause/seek/loop
    fn stats(&self) -> RendererStats;               // fps, decode path, cache bytes…
    fn release(&mut self);
}
```

```rust
// owe-media
pub trait MediaDecoder: Send {
    fn probe(path: &Path) -> Result<MediaInfo>;     // kind, dimensions, duration, codec, hw-capable?
    fn frames(&mut self) -> BoxStream<'static, DecodedFrame>; // bounded backpressure
}
// Impls: GStreamerDecoder (default), FfmpegDecoder (fallback), ImageDecoder, AnimatedImageDecoder.
// Selection: config `media.backend = "auto" | "gstreamer" | "ffmpeg"`.
```

**Registry rule (TRD FR-SHELL-1):** `owed` holds `Vec<Box<dyn ShellBackend>>` and `HashMap<ContentKind, RendererFactory>`, populated at startup. Config references entries by id; unknown id = config-validation error listing valid options.

---

## 3. IPC protocol v1 (authoritative spec)

- Transport: Unix stream socket `$XDG_RUNTIME_DIR/owe/<pid>/socket`, mode 0600 (TRD NFR-SEC-1).
- Framing: UTF-8 JSON, one object per line, `\n`-terminated. Max frame 1 MiB (larger → error frame).
- Every message: `{"v": 1, "id": "<client-uuid>", ...}`. Replies reference `id`.

### Requests (daemon ← clients)

| Method | Payload | Reply | Notes |
|---|---|---|---|
| `hello` | `{client: "gui"|"cli", version}` | `{server_version, schema, outputs, capabilities}` | first message on connect, mandatory |
| `outputs.list` | — | `{outputs: [MonitorInfo + current wallpaper + policy]}` | |
| `wallpaper.set` | `{output: OutputId|"all", source: {path} | {library_id}, transition?: {name, duration_ms, fps}}` | `{applied}` | async apply → `ok` only after first frame of new content is queued |
| `wallpaper.clear` | `{output?}` | `{ok}` | |
| `playback.cmd` | `{output, cmd: play|pause|seek{t}|loop{a,b}}` | `{ok, state}` | idempotent (TRD FR-LIVE-5) |
| `library.scan` | `{paths?}` | `{started: scan_id}` | progress via `event` frames |
| `library.list` | `{filter?, page?}` | `{items}` | |
| `governor.policy` | `{output}` | `{RenderPolicy + active rules}` | GUI status view |
| `governor.override` | `{output, policy, until?}` | `{ok}` | manual pause from tray = this |
| `stats.get` | `{output?}` | `{per-output fps, decode path, RSS, buffers}` | TRD FR-GOV-6 |
| `config.get` / `config.patch` | `{patch}` | `{config}` | patch validated then hot-applied |
| `daemon.kill` | — | connection close, graceful shutdown | |

### Events (daemon → clients, unsolicited)

`{"v":1,"event":"outputs_changed"|"wallpaper_changed{...}"|"policy_changed{...}"|"scan_progress{...}"|"error{...}"}`

### Error model

`{"v":1,"id":"…","err":{"code":"CONFIG_INVALID|NOT_FOUND|OUTPUT_UNKNOWN|UNSUPPORTED|BUSY|INTERNAL","msg":"…","details":{…}}}` — one error per request, socket stays open (TRD FR-CORE-6).

### Versioning (TRD NFR-COMPAT-2)

`hello` negotiates schema: client sends supported `{major, minor}` list; server picks. Major bump = breaking; both binaries in one release always share a major.

---

## 4. Config specification (`config.toml`, hot-reloaded)

Defaults shown; all keys validated (`owe-core::config::validate`) with precise errors; unknown keys are errors (catches typos), following wpaperd's strictness.

```toml
schema = 1

[shell]
backend = "auto"                # auto | caelestia | hyprland | generic-layer-shell
detect_order = ["caelestia", "hyprland", "generic-layer-shell"]

[shell.hyprland]
hyprpaper = "warn"              # warn | stop | ignore  (TRD §4)
event_socket = true             # feed socket2 events to governor

[shell.caelestia]
mode = "shell-routed"           # shell-routed | daemon-drawn
wallpapers_dir = "$CAELESTIA_WALLPAPERS_DIR"
theme_hook = true               # let Caelestia run its theming; OWE skips matugen (TRD §4)

[render]
default_transition = { name = "fade", duration_ms = 300, fps = 60 }
allow_transitions = ["fade","wipe","slide","grow","wave","outer","none", "...gl-transitions ids"]

[render.buffering]
max_in_flight = 3               # per output, hard cap (TRD NFR-RES-1)
shm_fallback = true

[media]
backend = "auto"                # auto | gstreamer | ffmpeg
hw_decode_required = false      # true ⇒ refuse software decode with loud error

[media.cache]
animated_frame_cap_mb = 96      # per wallpaper (TRD FR-LIVE-1)
compression = "zstd"            # zstd | lz4 | none

[library]
paths = ["~/Pictures/Wallpapers"]
thumbnail_size = 512
rescan_interval_secs = 0        # 0 = only on IPC/FS events

[governor]
fullscreen = { focused = "pause", others = "ignore" }   # pause | dim | fps(30) | ignore
battery = { on_battery = "ignore", below_pct = null, fps_on_battery = null }
dpms_off = "pause"
idle = { after_secs = 0, action = "none" }               # none | static | pause
cpu_busy = { pct = 0, for_secs = 0, action = "none" }    # opt-in, 1 Hz sampler max
eco_mode = false                # flips a bundled preset (TRD FR-GOV-5)

[outputs."DP-1"]                # output sections: exact name, description, or "any"
wallpaper = "library:uuid-or-path"
fps_cap = 60
transition = { name = "slide", duration_ms = 450 }

[outputs."any"]
wallpaper = "~/Pictures/Wallpapers/default.png"
```

Resolution order per output: exact name → description match → `any`. Precedence conflicts are resolved `exact > description > any` and logged. `config.patch` IPC applies the same validation, then hot-reload semantics.

**State files:** session (`$XDG_STATE_HOME/owe/session.toml`) written on every successful apply; library DB per ARCHITECTURE §7.

---

## 5. Output worker (the only Wayland-aware code)

Per output thread:

1. Connect via shared Wayland connection (SCTK); create layer-shell surface: `Layer::Background`, anchors all, exclusive zone −1, no input region (TRD FR-CORE-5).
2. Create wgpu surface (Vulkan→GL→lavapipe order); size = output logical size × scale, refreshed on `output` geometry/scale events.
3. Loop: await calloop message (new frame request / policy change / IPC command / exit). Draw only when: policy = Run, frame callback armed, and content has a new frame. Static content draws once then arms no callbacks (`Want::Sleep`) — NFR-PERF-1.
4. Transitions: crossfade/move between two textures on the GPU; interruption replaces the target and continues from current state (no snap).
5. Buffer pool: ≤ `max_in_flight` wl_frames per output; dma-buf import preferred, shm fallback always compiled in.

Fault isolation: worker panic → supervisor catches (catch_unwind at thread boundary), marks output degraded, respawns worker, re-applies last wallpaper; other outputs untouched (NFR-REL-1). Three restarts in 60 s ⇒ output marked failed + IPC event + GUI banner; daemon stays up.

---

## 6. Media pipelines

### 6.1 Video (GStreamer primary — ADR-006)

```
uridecodebin → (hw decode: vaapidecodebin / vapostproc)
            → GLUpload / dmabuf caps negotiation
            → appsink(last-sample, max-bytes bounded)
            → owe-media DecodedFrame{dmabuf|memory}
            → owe-render: import EGLImage/dma-buf → wgpu texture → draw
```

- Decoder name queried from pipeline (VA-API vs software) and surfaced via `stats.get` (TRD FR-LIVE-2/3).
- The capability probe runs at pipeline-build time; legacy VA-API drivers (e.g. `i965` on Haswell-era iGPUs — the Reference Profile machine, ADR-014) are handled by the same probe: hw name when the driver negotiates, software decode + warning otherwise (TRD FR-LIVE-3). No hard assumption that any specific driver works.
- GStreamer bus runs on its own thread, bridged into calloop via channel — no async runtime (ADR-004).
- FFmpeg fallback implements the same `MediaDecoder` trait; `media.backend` selects; `auto` prefers GStreamer when plugins load.
- Audio: **dropped at the pipeline** (`audioconvert ! fakesink`), wallpapers are silent by default; an explicit `enable_audio` escape hatch exists but defaults false.

### 6.2 Animated GIF/APNG

- Off-thread decode into ring buffer of frames, each zstd/lz4-compressed, total ≤ `animated_frame_cap_mb`; on overflow: stop pre-caching, switch to streaming decode at render fps (slower CPU, bounded RAM — documented tradeoff, GUI-visible in stats).
- Frame timing from container metadata, clamped by governor cap.

### 6.3 Shaders (P5)

- Pack = `shader.toml` (`name, author, license, params[], fps_hint, preview`) + `shader.wgsl` (WGSL, uniform block auto-generated from declared params).
- Params: `float {min,max,default}`, `color (vec4)`, `bool`, `int`; GUI generates controls (TRD FR-LIVE-8); values stored per-pack in daemon config under `[shader_params."<pack>"]`.
- Built-in uniforms: `u_time`, `u_resolution`, `u_cursor` (opt-in privacy-gated), `u_fps`.
- Hot-reload: inotify on pack dir → recompile → transition swap.

### 6.4 Thumbnails / previews (P2, P5)

- Images: decode→resize→PNG cache, off-thread.
- Video: first-frame poster via GStreamer `ximageproc`-style snapshot (FFmpeg CLI fallback if plugins missing).
- Shader: daemon renders 1 frame at 512px offscreen → PNG.

---

## 7. Governor engine

- Inputs (event stream, all ≤1 Hz poll ceiling — TRD FR-GOV-7): shell events (Hyprland socket2), UPower changes, DPMS state, idle timer, optional CPU sampler.
- Engine: ordered rule table (config order), each rule `when(predicate) → action(output_scope, policy_override)`; last-match-wins per output; result = `RenderPolicy` per output; diffed before dispatch (no spurious unpause).
- Defaults are the conservative set in §4 (`fullscreen focused = pause`, everything else opt-in) — the safe-by-default principle from PRD.
- Manual overrides (`governor.override`) shadow rules until `until` expiry or manual clear; GUI shows shadowed state.

---

## 8. Error handling & logging

- `owe-core::Error` enum with `thiserror`; every IPC-visible error maps to §3 error codes; internal errors log full context via `tracing`, external replies carry sanitized messages.
- Exit codes: 0 clean, 1 config invalid, 2 Wayland/compositor unavailable, 3 backend required but missing — systemd unit uses these for `Restart=on-failure` policy.
- Logging: `tracing` spans per output worker + per IPC client; journald-friendly; `--verbose` and `RUST_LOG` supported; **no secrets** (paths fine, env values never dumped).

---

## 9. Memory budget (design allocations, verified at P6 — TRD NFR-PERF-3)

| Component | Static path | Video path |
|---|---|---|
| Config + library + governor | < 5 MiB | < 5 MiB |
| wgpu device + pipelines | ~15 MiB | ~15 MiB |
| Output buffers (2×1080p, in-flight capped) | ~16 MiB | ~16 MiB |
| Decode (hw path) | — | ~40–70 MiB (driver-owned, estimated) |
| **Target RSS** | **≤ 30 MiB** | **≤ 120 MiB** |

Driver-owned allocations may exceed our control; if P6 measurements show the video target unreachable, the benchmark report says so explicitly and we consider the target revised via ADR — no quiet goalpost moves.
