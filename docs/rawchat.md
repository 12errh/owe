Yes — this is very doable, and **Tauri v2 + Rust can be a good architecture for a low-resource Linux live-wallpaper manager**, but I would *not* make Tauri responsible for rendering the wallpaper itself.

The key design decision is:

> **Tauri = GUI/settings/control plane. Rust wallpaper daemon/renderer = actual wallpaper engine.**

That lets you keep the UI convenient while keeping the always-running part extremely lightweight.

### What I would build

![Image](https://images.openai.com/static-rsc-4/KemTQvGPMIuC26qxyUOgsyYUBS4rUDvDT_HT2RxOxt0wLrcX04_j3BNT0_zzME9zHFWSo5a38gozP493rGxcq5XRRViOW4Z4JQ95G_Q0npvWVZSgjGZOodK3vhhQXZo16Qx4nfAO51yzt23V4fEjMoupjcybjr65qU_SQfX-rY6l_l-LMN8CO8dOxU0MFhGh?purpose=fullsize)

![Image](https://images.openai.com/static-rsc-4/N2ibtvmh6DIa9IIoKkhE_0Z7mBkROLzKCcL_WB27h8smVe-8p3NLEQoXlfVykHFkeOfuNWYHJcVkGA-2lawJVGHA1JUW_U_KN2wO-ba4tlShzWzFmRosa0JUEkBisazmJiaCeuYNEHyNtUU1GTgk7viLimRKFScRVIsuEANw6mxE608gLdrgY6RgttOyQ3N2?purpose=fullsize)

![Image](https://images.openai.com/static-rsc-4/fwVq17IkxNgZ0GLB8_lKk90TmirVUZ9gouBTt5hzTyadSvDCMM0N4gNespLMhe6aKDTARlbap2bobZtsb7WKp0FQhr1xm5H6x9Pr0PZcgV7lGF1NLBLDRD0OmGXSBbbaG5mb_UJIb8vlhnwLPrkjzyor-7hdrYwRK7Yzt2PtR4tCRFUdixv0S8WbIxJRwJRa?purpose=fullsize)

![Image](https://images.openai.com/static-rsc-4/SpRXdOZ1uSQmZMbhRnHRDmfQG3MsRhkqfE3T-xJIqMPVSe28SJl6PKlhXuCBPiwt2cSSiWr0S_kMLp6gKpV3SzPTwRqicLiH8r82-mhprkbl15UEcaaLOK5E6HarvUAuFXdPh66BsujdGxgaWZWHkop1N4fs15AZtKWcIKnQEi-mc3nXtozXn7IHe5CZuAzf?purpose=fullsize)

![Image](https://images.openai.com/static-rsc-4/wX6_CnhygonlruzkETNXDce3D7-w1Un7sLcIf3eYE3B3fN9xrLIXezmHFvAwPjAPiDmlPxIcuZcg0yseb71owqsyTHsOf22Afo-VShw74BRbuS8LoYQFuj6ttaGJ6XyAYgbBxlevUCeHRtneAp2Y4qraItRIqQS6K0QBhHFBmFegYisLeXr5bwPsGgmP7FPS?purpose=fullsize)

```text
                    ┌──────────────────────────┐
                    │       Tauri v2 GUI       │
                    │                          │
                    │  Wallpaper browser       │
                    │  Settings                │
                    │  Monitor selection       │
                    │  FPS / quality            │
                    │  Playback controls       │
                    └────────────┬─────────────┘
                                 │
                          Rust commands / IPC
                                 │
                    ┌────────────▼─────────────┐
                    │    Rust Wallpaper Core   │
                    │                          │
                    │  media management        │
                    │  monitor detection       │
                    │  playback state          │
                    │  configuration           │
                    └───────┬───────────┬──────┘
                            │           │
                   Hyprland IPC      Wayland
                            │           │
                     ┌──────▼─────┐   ┌▼───────────┐
                     │  Hyprland  │   │  Renderer  │
                     │            │   │            │
                     └────────────┘   │ GPU/OpenGL │
                                      │ Vulkan/etc │
                                      └────────────┘
```

## First important thing: what do you mean by "live wallpaper"?

There are actually **three different levels of difficulty**.

| Type                               | Difficulty |  Resource usage | My recommendation |
| ---------------------------------- | ---------: | --------------: | ----------------- |
| Static images                      |          ⭐ |        Very low | Definitely        |
| Animated GIF/WebP/video            |         ⭐⭐ |      Low–medium | Definitely        |
| HTML/WebGL live wallpaper          |       ⭐⭐⭐⭐ |     Medium–high | Maybe             |
| Custom GPU shaders                 |       ⭐⭐⭐⭐ | Very low–medium | **Excellent**     |
| Full browser rendering per monitor |      ⭐⭐⭐⭐⭐ |            High | Avoid             |

If your goal is **very low CPU/RAM**, I would focus on:

**video + GPU shaders + image sequences**, rather than running a browser/webview as the wallpaper.

---

# Why Tauri alone isn't ideal

Tauri itself is lightweight compared with Electron because the application uses the system webview rather than shipping Chromium.

But your wallpaper is different.

Imagine you create:

```text
Tauri window
   ↓
WebView
   ↓
HTML
   ↓
JavaScript animation
   ↓
GPU
```

It can work, but you're essentially keeping a browser rendering engine alive permanently.

That's unnecessary for a wallpaper.

Instead:

```text
Tauri UI
   ↓
Rust
   ↓
GPU renderer
   ↓
Wayland
```

The Tauri application can even **close its UI while the wallpaper continues running**.

That's the architecture I'd aim for.

---

# Hyprland makes this particularly interesting

Hyprland already has a wallpaper ecosystem.

The official Hyprland documentation lists tools such as `hyprpaper`, `mpvpaper`, `awww`, etc. ([Hypr Land Wiki][1])

`hyprpaper`, for example, is specifically designed as an IPC-controlled wallpaper utility and can set wallpapers through:

```bash
hyprctl hyprpaper wallpaper "DP-1,/path/to/wallpaper"
```

([Hypr Land Wiki][2])

Hyprland also exposes UNIX sockets for programmatic control and event monitoring. ([Hypr Land Wiki][3])

That means your Rust application can integrate with Hyprland rather than fighting it.

---

# Caelestia is even easier

Caelestia already exposes wallpaper functionality.

Its shell IPC has:

```text
wallpaper list
wallpaper get
wallpaper set(path)
```

according to the current Caelestia shell documentation. ([GitHub][4])

So your application could have a **Caelestia integration layer**:

```text
Your app
   │
   ├── Hyprland backend
   │
   └── Caelestia backend
```

For example:

```rust
trait WallpaperBackend {
    fn set_wallpaper(&self, monitor: &str, path: &Path);
    fn get_monitors(&self);
    fn remove_wallpaper(&self, monitor: &str);
}
```

Then:

```text
HyprlandBackend
CaelestiaBackend
```

---

# I would NOT make two completely separate applications

I'd structure the project like this:

```text
my-wallpaper/
│
├── src-tauri/
│   ├── main.rs
│   │
│   ├── commands/
│   │   ├── wallpaper.rs
│   │   ├── monitors.rs
│   │   └── settings.rs
│   │
│   ├── core/
│   │   ├── wallpaper.rs
│   │   ├── media.rs
│   │   ├── config.rs
│   │   └── backend.rs
│   │
│   ├── backends/
│   │   ├── hyprland.rs
│   │   └── caelestia.rs
│   │
│   └── renderer/
│       ├── video.rs
│       ├── shader.rs
│       └── renderer.rs
│
├── src/
│   ├── App.tsx
│   ├── pages/
│   ├── components/
│   └── styles/
│
├── wallpapers/
│
└── Cargo.toml
```

You could use:

```text
Tauri v2
Rust
TypeScript
React/Svelte/Vue
```

I'd personally choose **Svelte + Tauri** if your priority is a simple, lightweight configuration UI.

---

# But there's an even better architecture

I'd actually separate the application into **three pieces**:

```text
             ┌───────────────────┐
             │    Tauri GUI      │
             │                   │
             │  wallpaper UI     │
             │  settings         │
             │  library          │
             └─────────┬─────────┘
                       │
                    IPC/API
                       │
             ┌─────────▼─────────┐
             │ Wallpaper daemon  │
             │                   │
             │ Rust              │
             │ always running    │
             │ tiny memory       │
             └─────────┬─────────┘
                       │
                ┌──────▼──────┐
                │ GPU renderer │
                └─────────────┘
```

The daemon could be something like:

```text
mywallpaperd
```

and the UI:

```text
mywallpaper
```

When you close the GUI:

```text
mywallpaper
     X
     
mywallpaperd
     ↓
still running
     ↓
wallpaper continues
```

That's a much better desktop architecture.

---

# How much RAM?

I wouldn't promise a specific number without benchmarking because it depends heavily on the renderer, media, GPU driver and number of monitors.

But the important distinction is:

### Tauri GUI

Potentially tens/hundreds of MB depending on the frontend and WebView usage.

That's fine because it doesn't need to remain open.

### Rust daemon

Can be extremely small compared with a browser-based renderer.

Something like:

```text
mywallpaperd
     ↓
configuration
     ↓
Wayland connection
     ↓
GPU resources
```

The GPU memory used by a wallpaper can actually matter more than the Rust process's RAM.

For example, a 3840×2160 RGBA framebuffer is approximately:

```text
3840 × 2160 × 4
≈ 33 MB
```

and double/triple buffering can multiply that.

So **resolution and buffering are more important than whether Rust uses 5 MB vs 15 MB of CPU-side memory.**

---

# CPU usage is the interesting part

For video:

```text
CPU
 ↓
decode video
 ↓
GPU
 ↓
display
```

You want **hardware video decoding** where possible.

For shaders:

```text
CPU
 ↓
send parameters
 ↓
GPU
 ↓
render
```

The CPU can remain almost idle.

For example, a shader wallpaper could just update:

```rust
time += delta;
```

and the GPU does the actual animation.

This is potentially a fantastic fit for your goal.

---

# I would support these wallpaper types

Your application could have:

### 1. Image

```text
PNG
JPEG
WebP
AVIF
JXL
```

Very low resource usage.

### 2. Video

```text
MP4
WebM
MKV
```

Ideally hardware decoded.

### 3. Shader

Something like:

```glsl
void main() {
    vec2 uv = ...;
    float t = time;

    // animation

    fragColor = ...;
}
```

This is probably the most interesting feature for a low-resource Hyprland application.

### 4. Procedural wallpaper

Instead of storing a 500 MB video:

```text
particles
waves
gradient
noise
matrix
aurora
fluid simulation
```

generated directly on GPU.

---

# The killer feature: adaptive FPS

Don't run the wallpaper at 144 FPS just because your monitor supports 144 Hz.

Give the user:

```text
Wallpaper FPS

15
24
30
45
60
90
120
144
```

And possibly:

```text
Eco mode
```

For example:

```text
Desktop idle:
30 FPS

Normal:
60 FPS

Battery:
20 FPS

Fullscreen application:
pause

Gaming:
pause
```

This could dramatically reduce unnecessary GPU work.

---

# Even better: automatically pause

Hyprland provides events through its IPC system. ([Hypr Land Wiki][3])

You could monitor things such as:

```text
workspace
focusedmon
activewindow
```

and potentially integrate with your own application logic.

For example:

```text
User opens fullscreen game
        ↓
detect fullscreen
        ↓
pause wallpaper
        ↓
GPU usage ↓
```

Then:

```text
Game closes
      ↓
wallpaper resumes
```

That's exactly the kind of optimization that makes sense for your project.

---

# Tauri's role

Tauri gives you things such as:

```text
Settings UI
File picker
Wallpaper browser
Configuration
System tray
Keyboard shortcuts
Notifications
Autostart
```

Its Linux window configuration also includes options such as `alwaysOnBottom`, background color, etc. ([Tauri][5])

But I wouldn't use a Tauri window as the actual wallpaper surface unless you're deliberately building an HTML/WebGL wallpaper system.

---

# Renderer choices

This is probably the hardest technical decision.

You have several options.

### Option A — use an existing wallpaper engine

Your application becomes:

```text
Tauri
 ↓
Rust
 ↓
mpvpaper / awww / hyprpaper
```

This is **much easier**.

Hyprland itself documents `mpvpaper` and `awww` as wallpaper options. ([Hypr Land Wiki][1])

Your application simply manages them.

Difficulty:

**3/10**

---

### Option B — write your own renderer

```text
Tauri
 ↓
Rust
 ↓
Wayland
 ↓
OpenGL/Vulkan
```

Difficulty:

**8/10**

But you get complete control.

---

### Option C — Rust + wgpu

This is probably what I'd investigate first if you want your own renderer.

Architecture:

```text
Rust
  │
  ├── Wayland
  │
  └── wgpu
        │
        ├── Vulkan
        ├── OpenGL
        └── other backend
```

Then:

```text
shader
  ↓
wgpu
  ↓
GPU
```

You don't have to directly manage every Vulkan detail.

---

# The biggest difficulty isn't Tauri

This is important.

If you've never worked with Wayland graphics, the difficulty is approximately:

```text
Tauri UI                2/10
Rust application        4/10
Hyprland IPC            3/10
Caelestia integration   3/10
Media library            4/10
GPU shader renderer     6/10
Wayland wallpaper       8/10
Video hardware decode   8/10
Multi-monitor handling  7/10
Power/resource handling 7/10
```

So the **actual difficult part is the wallpaper rendering backend**, not the GUI.

---

# I would build it in stages

Don't start with the complicated renderer.

## Phase 1 — MVP

Build:

```text
Tauri
+
Rust
+
Hyprland IPC
```

Features:

```text
[ Wallpaper ]

wallpaper1.jpg
wallpaper2.jpg
wallpaper3.jpg

[Set Wallpaper]
```

Rust executes the appropriate Hyprland IPC operation.

At this point you've proven:

```text
Tauri → Rust → Hyprland
```

---

# Phase 2 — wallpaper library

Add:

```text
~/Pictures/Wallpapers
```

scan:

```text
*.jpg
*.png
*.webp
*.jxl
*.mp4
*.webm
```

Generate thumbnails.

Database:

```text
SQLite
```

Something like:

```text
wallpapers
──────────────
id
path
type
name
thumbnail
favorite
last_used
```

---

# Phase 3 — Caelestia integration

Detect:

```text
Caelestia running?
```

Then:

```text
Backend:
    Hyprland
    Caelestia
    Auto
```

For Caelestia, you can use its wallpaper IPC functionality. ([GitHub][4])

---

# Phase 4 — animated wallpaper

Now add:

```text
video
 ↓
decoder
 ↓
GPU
 ↓
Wayland
```

This is where I'd seriously investigate using an existing mature media stack rather than implementing a video decoder yourself.

---

# Phase 5 — shader wallpapers

This is where your project could become much more interesting.

Something like:

```text
MyWallpaper
│
├── Images
├── Videos
└── Shaders
      ├── Aurora
      ├── Fluid
      ├── Matrix
      ├── Particles
      └── Plasma
```

The user can select:

```text
Aurora

FPS: 30
Quality: Medium
GPU limit: 20%
```

---

# Phase 6 — resource manager

Add:

```text
pause_on_fullscreen = true
pause_on_battery = true
max_fps = 60
max_gpu = ...
```

And:

```text
Monitor 1:
    Aurora
    30 FPS

Monitor 2:
    Static image
```

This is where the application becomes genuinely useful.

---

# Phase 7 — daemon

Finally split:

```text
mywallpaper
mywallpaperd
```

The GUI talks to:

```text
mywallpaperd
```

using a local Unix socket.

For example:

```text
~/.local/state/mywallpaper/socket
```

Messages:

```json
{
    "command": "set_wallpaper",
    "monitor": "DP-1",
    "wallpaper": "aurora"
}
```

or preferably a compact binary/protobuf-like protocol if you really care about efficiency.

For this sort of local communication, **JSON is perfectly fine initially**. Don't optimize prematurely.

---

# Recommended technology stack

If I were building this project today, I'd start with:

```text
Frontend
──────────────
Tauri v2
Svelte
TypeScript
CSS

Backend
──────────────
Rust
Tokio
serde
serde_json
SQLite

Linux
──────────────
Wayland
Hyprland IPC
Caelestia IPC

Graphics
──────────────
wgpu
WGSL shaders

Media
──────────────
FFmpeg / GStreamer
```

And potentially:

```text
zbus
```

if you later need D-Bus integration.

---

# One thing I would NOT do

Don't make this:

```text
Tauri
  ↓
React
  ↓
HTML video
  ↓
WebView
  ↓
wallpaper
```

and call it a low-resource Rust wallpaper application.

You've basically made a browser-based wallpaper engine with Rust controlling it.

Instead:

```text
                    Tauri
                      │
                 control only
                      │
                      ▼
                Rust daemon
                      │
              ┌───────┴───────┐
              ▼               ▼
          Wayland           GPU
              │               │
              └───────┬───────┘
                      ▼
                   screen
```

That's much closer to your stated goal.

---

# Difficulty overall

For a **simple wallpaper manager**:

**~3/10**

For a polished application with:

* Hyprland
* Caelestia
* multiple monitors
* images
* videos
* playlists
* system tray
* autostart
* IPC
* settings

I'd say:

**~6/10**

For your own **high-performance GPU wallpaper engine**:

**~8/10**

For:

> "I want a Rust program that renders shader/video wallpapers directly onto Wayland surfaces, handles multi-monitor correctly, hardware decoding, fullscreen detection, power management, and has extremely low overhead."

That's a serious systems/graphics project:

**~9/10**

But you don't need to build all of that at once.

---

## The architecture I'd recommend for your exact goal

```text
                    ┌─────────────────┐
                    │    Tauri v2     │
                    │                 │
                    │ Wallpaper UI    │
                    │ Settings        │
                    │ Library         │
                    │ Preview         │
                    └────────┬────────┘
                             │
                         Unix IPC
                             │
                    ┌────────▼────────┐
                    │ mywallpaperd    │
                    │                 │
                    │ Rust            │
                    │ config          │
                    │ media manager   │
                    │ monitor manager │
                    └──────┬──────────┘
                           │
             ┌─────────────┼──────────────┐
             │             │              │
             ▼             ▼              ▼
        Hyprland       Caelestia       Renderer
           IPC            IPC          wgpu/etc.
             │             │              │
             └─────────────┴──────────────┘
                           │
                           ▼
                        Wayland
                           │
                           ▼
                         GPU
                           │
                           ▼
                        Display
```

**This is the direction I'd take.**

And there's a particularly nice advantage: you can initially make `mywallpaperd` use **existing Hyprland wallpaper engines as backends**, then later replace the backend with your own renderer without rewriting the Tauri application. Hyprland explicitly supports several wallpaper utilities, so this incremental approach fits its ecosystem well. ([Hypr Land Wiki][1])

If you want, I can next lay out the **actual project from zero — exact Cargo dependencies, Tauri v2 setup, directory structure, Rust traits, Hyprland IPC code, Caelestia backend, and then the first working MVP**, step by step.

[1]: https://wiki.hypr.land/0.55.0/Useful-Utilities/Wallpapers/?utm_source=chatgpt.com "Wallpapers – Hyprland Wiki"
[2]: https://wiki.hypr.land/hypr-ecosystem/user/hyprpaper/?utm_source=chatgpt.com "hyprpaper – Hyprland Wiki"
[3]: https://wiki.hypr.land/0.48.0/IPC/?utm_source=chatgpt.com "IPC – Hyprland Wiki"
[4]: https://github.com/liperium/caelestia-shell/blob/main/README.md?utm_source=chatgpt.com "caelestia-shell/README.md at main · liperium/caelestia-shell · GitHub"
[5]: https://v2.tauri.app/reference/config/?utm_source=chatgpt.com "Configuration | Tauri"
