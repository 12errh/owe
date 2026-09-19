//! Layer-shell presentation: how a rendered frame reaches the screen.
//!
//! # Design decisions (with the reason, not just the rule)
//!
//! **One Wayland connection, one thread.** Wayland connections are not usable
//! from multiple threads, and several output surfaces on one connection is the
//! normal case, so the presenter owns a connection and a thread, and the rest of
//! the daemon talks to it over channels.
//!
//! **Shared memory, not a wgpu swapchain.** The frame is rendered on the GPU (see
//! [`crate::image`]) and copied into a `wl_shm` buffer for presentation. For a
//! *static* wallpaper this is the low-resource choice: one copy per wallpaper
//! change, then nothing at all. A zero-copy `dma-buf`/EGL path only pays off for
//! per-frame content, and it arrives in P4 with video — where it is actually
//! needed. This is a deliberate P1 deviation, recorded in the plan.
//!
//! **Exactly two buffers per output, reused forever.** Ported lesson from the
//! studied reference engines: pools that grow on demand high-water-mark the
//! process (wallr measured ~390 MiB retained after 90 rapid switches with
//! unbounded growth). Two slots is enough to keep the compositor fed and bounds
//! memory at 2 × width × height × 4 bytes per output.
//!
//! **The compositor's `configure` size is authoritative.** We never assume the
//! size we rendered at is the size we should present; when they differ we report
//! it and let the caller re-render. That is what makes scale/HiDPI/output changes
//! a normal event instead of a subtle wrong-size bug.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, channel, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use thiserror::Error;
use wayland_client::{
    Connection, Proxy, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_shm, wl_surface},
};

/// Wayland's `XRGB8888`: on little-endian this is byte order B, G, R, X.
const PIXEL_FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;

/// How long a present request waits for the compositor, and how long surface
/// creation waits for the first configure before giving up.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// A rendered frame, ready to hand to the compositor.
///
/// Pixels are BGRA (`XRGB8888`) — produced directly by
/// [`crate::image::render`] with [`crate::image::PixelFormat::Bgra8`], so no
/// channel swizzle happens on this path.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes in BGRA order.
    pub pixels: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.pixels.len())
            .finish()
    }
}

impl Frame {
    /// A frame of one solid colour (used by tests and by the "blank" fill).
    pub fn solid(width: u32, height: u32, bgr: [u8; 3]) -> Self {
        let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for _ in 0..(width as usize * height as usize) {
            pixels.extend_from_slice(&[bgr[0], bgr[1], bgr[2], 0xff]);
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    /// Pixel size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// What happened to a present request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentOutcome {
    /// The frame is on screen at `size`.
    Presented {
        /// Presented size.
        size: (u32, u32),
    },
    /// The compositor wants a different size than the frame provides; re-render
    /// at `size` and present again. Nothing was uploaded, so the previous
    /// wallpaper stays visible rather than flashing a stretched frame.
    ResizeRequired {
        /// Size the caller must render at.
        size: (u32, u32),
    },
}

/// Things the presenter tells the rest of the daemon about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenterEvent {
    /// An output changed its surface size.
    Configured {
        /// Output name.
        output: String,
        /// New size.
        size: (u32, u32),
    },
    /// The compositor destroyed one of our surfaces.
    Closed {
        /// Output name.
        output: String,
    },
}

/// Presenter failures.
#[derive(Debug, Error)]
pub enum PresentError {
    /// No Wayland session is reachable (headless CI, or `WAYLAND_DISPLAY` unset).
    #[error("no Wayland session available: {0}")]
    NoSession(String),

    /// The compositor does not implement `wlr-layer-shell`.
    #[error("this compositor does not support wlr-layer-shell: {0}")]
    NoLayerShell(String),

    /// The requested output is not connected.
    #[error("no output named `{0}` is connected")]
    UnknownOutput(String),

    /// The compositor never configured a new surface within the deadline.
    #[error("compositor did not configure the surface for `{output}` within {:?}", .timeout)]
    ConfigureTimeout {
        /// Output name.
        output: String,
        /// How long we waited.
        timeout: Duration,
    },

    /// Every buffer for the output was still held by the compositor.
    #[error("both buffers for `{0}` are still in use by the compositor")]
    Busy(String),

    /// The frame does not match the surface's expected buffer size.
    #[error("frame is {actual_w}x{actual_h} but {expected_w}x{expected_h} bytes were expected")]
    FrameSize {
        /// Expected width.
        expected_w: u32,
        /// Expected height.
        expected_h: u32,
        /// Frame width.
        actual_w: u32,
        /// Frame height.
        actual_h: u32,
    },

    /// A Wayland request failed.
    #[error("wayland error: {0}")]
    Wayland(String),

    /// The presenter thread is gone (daemon shutting down, or it crashed).
    #[error("the presenter is no longer running")]
    NotRunning,
}

/// Commands sent to the presenter thread.
enum Command {
    Present {
        output: String,
        frame: Frame,
        reply: SyncSender<Result<PresentOutcome, PresentError>>,
    },
    Clear {
        output: String,
        reply: SyncSender<Result<(), PresentError>>,
    },
    Shutdown,
}

/// The handle the daemon uses to present frames.
pub struct Presenter {
    tx: Option<Sender<Command>>,
    events: Receiver<PresenterEvent>,
    join: Option<JoinHandle<()>>,
    outputs: Vec<String>,
}

impl std::fmt::Debug for Presenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presenter")
            .field("outputs", &self.outputs)
            .finish_non_exhaustive()
    }
}

impl Presenter {
    /// Start the presenter.
    ///
    /// Fails fast with [`PresentError::NoSession`] when there is no session —
    /// that is a normal state for CI, not a crash.
    pub fn start(namespace: &str) -> Result<Self, PresentError> {
        let (command_tx, command_rx) = channel::<Command>();
        let (event_tx, event_rx) = channel::<PresenterEvent>();
        let (ready_tx, ready_rx) = sync_channel::<Result<Vec<String>, PresentError>>(1);

        let namespace = namespace.to_string();
        let join = std::thread::Builder::new()
            .name("owe-presenter".to_string())
            .spawn(move || run_presenter(&namespace, &command_rx, &event_tx, &ready_tx))
            .map_err(|error| {
                PresentError::NoSession(format!("cannot spawn presenter thread: {error}"))
            })?;

        // Wait for setup to finish so failures are reported to the caller rather
        // than silently swallowed by a background thread.
        match ready_rx.recv() {
            Ok(Ok(outputs)) => Ok(Self {
                tx: Some(command_tx),
                events: event_rx,
                join: Some(join),
                outputs,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                let _ = join.join();
                Err(PresentError::NotRunning)
            }
        }
    }

    /// Output names the presenter can draw on.
    pub fn outputs(&self) -> &[String] {
        &self.outputs
    }

    /// Present a frame on one output, waiting up to [`DEFAULT_TIMEOUT`].
    pub fn present(&self, output: &str, frame: Frame) -> Result<PresentOutcome, PresentError> {
        let tx = self.tx.as_ref().ok_or(PresentError::NotRunning)?;
        let (reply_tx, reply_rx) = sync_channel(1);
        tx.send(Command::Present {
            output: output.to_string(),
            frame,
            reply: reply_tx,
        })
        .map_err(|_| PresentError::NotRunning)?;
        reply_rx
            .recv_timeout(DEFAULT_TIMEOUT * 4)
            .map_err(|_| PresentError::NotRunning)?
    }

    /// Remove OWE's surface from an output.
    pub fn clear(&self, output: &str) -> Result<(), PresentError> {
        let tx = self.tx.as_ref().ok_or(PresentError::NotRunning)?;
        let (reply_tx, reply_rx) = sync_channel(1);
        tx.send(Command::Clear {
            output: output.to_string(),
            reply: reply_tx,
        })
        .map_err(|_| PresentError::NotRunning)?;
        reply_rx
            .recv_timeout(DEFAULT_TIMEOUT * 2)
            .map_err(|_| PresentError::NotRunning)?
    }

    /// Take everything the presenter has reported since the last call.
    pub fn drain_events(&self) -> Vec<PresenterEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }

    /// Stop the presenter thread.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Command::Shutdown);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// --- the presenter thread ----------------------------------------------------

struct OutputSurface {
    /// `wl_surface` protocol id — how a callback finds its output.
    surface_id: u32,
    layer: LayerSurface,
    pool: SlotPool,
    buffers: [Option<Buffer>; 2],
    next_slot: usize,
    size: Option<(u32, u32)>,
}

struct PresenterState {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    namespace: String,
    surfaces: HashMap<String, OutputSurface>,
    /// Output name by `wl_output` protocol id, for hotplug callbacks.
    output_names: HashMap<u32, String>,
    events: Sender<PresenterEvent>,
}

impl ProvidesRegistryState for PresenterState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    // Only hot-pluggable state belongs here (see the SCTK simple_layer example).
    registry_handlers![OutputState];
}

delegate_noop!(PresenterState: ignore wl_region::WlRegion);

impl CompositorHandler for PresenterState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Deliberately empty: a static wallpaper requests no frames. P4's video
        // path is where `frame` callbacks become meaningful.
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for PresenterState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(name) = self.output_state.info(&output).and_then(|info| info.name) {
            tracing_note(format!("output appeared: {name}"));
            self.output_names.insert(output.id().protocol_id(), name);
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(name) = self.output_state.info(&output).and_then(|info| info.name) {
            self.output_names.insert(output.id().protocol_id(), name);
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for PresenterState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let id = layer.wl_surface().id().protocol_id();
        if let Some((name, _)) = self
            .surfaces
            .iter()
            .find(|(_, surface)| surface.surface_id == id)
            .map(|(name, surface)| (name.clone(), surface))
        {
            self.surfaces.remove(&name);
            let _ = self.events.send(PresenterEvent::Closed { output: name });
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let id = layer.wl_surface().id().protocol_id();
        let size = configure.new_size;
        if size.0 == 0 || size.1 == 0 {
            // The compositor is asking us to choose; we anchor to all edges so a
            // zero size should not happen, and guessing here would paper over a
            // real protocol problem.
            tracing_note(format!(
                "compositor configured a {}x{} surface; ignoring",
                size.0, size.1
            ));
            return;
        }

        let Some((name, surface)) = self
            .surfaces
            .iter_mut()
            .find(|(_, surface)| surface.surface_id == id)
        else {
            return;
        };

        let changed = surface.size != Some(size);
        if changed {
            // A different size invalidates the parked buffers (and their pool).
            surface.size = Some(size);
            surface.buffers = [None, None];
            surface.pool = match SlotPool::new(pool_bytes(size), &self.shm) {
                Ok(pool) => pool,
                Err(error) => {
                    tracing_note(format!("cannot resize the shm pool for {name}: {error}"));
                    return;
                }
            };
            let _ = self.events.send(PresenterEvent::Configured {
                output: name.clone(),
                size,
            });
        }
    }
}

impl ShmHandler for PresenterState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(PresenterState);
delegate_output!(PresenterState);
delegate_shm!(PresenterState);
delegate_layer!(PresenterState);
delegate_registry!(PresenterState);

/// Pool size for one buffer of `size` (plus slack for alignment).
fn pool_bytes(size: (u32, u32)) -> usize {
    let stride = stride_of(size.0);
    (stride as usize) * (size.1 as usize)
}

/// Stride in bytes for a width (4 bytes per pixel, as Wayland requires).
fn stride_of(width: u32) -> u32 {
    width * 4
}

/// Highest frequency log sink this crate needs. Kept local so `owe-render` does
/// not depend on a logging stack; the daemon sets `OWE_RENDER_DEBUG=1` when it
/// wants the presenter's narration.
fn tracing_note(message: String) {
    if std::env::var_os("OWE_RENDER_DEBUG").is_some() {
        eprintln!("owe-render: {message}");
    }
}

fn run_presenter(
    namespace: &str,
    commands: &Receiver<Command>,
    events: &Sender<PresenterEvent>,
    ready: &SyncSender<Result<Vec<String>, PresentError>>,
) {
    let connection = match Connection::connect_to_env() {
        Ok(connection) => connection,
        Err(error) => {
            let _ = ready.send(Err(PresentError::NoSession(error.to_string())));
            return;
        }
    };

    let (globals, mut queue) = match registry_queue_init::<PresenterState>(&connection) {
        Ok(pair) => pair,
        Err(error) => {
            let _ = ready.send(Err(PresentError::NoSession(error.to_string())));
            return;
        }
    };
    let qh = queue.handle();

    let compositor_state = match CompositorState::bind(&globals, &qh) {
        Ok(state) => state,
        Err(error) => {
            let _ = ready.send(Err(PresentError::NoSession(error.to_string())));
            return;
        }
    };
    let shm = match Shm::bind(&globals, &qh) {
        Ok(shm) => shm,
        Err(error) => {
            let _ = ready.send(Err(PresentError::NoSession(error.to_string())));
            return;
        }
    };
    let layer_shell = match LayerShell::bind(&globals, &qh) {
        Ok(shell) => shell,
        Err(error) => {
            let _ = ready.send(Err(PresentError::NoLayerShell(error.to_string())));
            return;
        }
    };

    let mut state = PresenterState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor_state,
        shm,
        layer_shell,
        namespace: namespace.to_string(),
        surfaces: HashMap::new(),
        output_names: HashMap::new(),
        events: events.clone(),
    };

    // First roundtrip: we need the output list before we can name one.
    if let Err(error) = queue.roundtrip(&mut state) {
        let _ = ready.send(Err(PresentError::NoSession(error.to_string())));
        return;
    }

    let outputs: Vec<String> = state
        .output_state
        .outputs()
        .filter_map(|output| {
            let name = state
                .output_state
                .info(&output)
                .and_then(|info| info.name)?;
            state
                .output_names
                .insert(output.id().protocol_id(), name.clone());
            Some(name)
        })
        .collect();

    let _ = ready.send(Ok(outputs.clone()));
    tracing_note(format!("presenter ready, outputs: {}", outputs.join(", ")));

    loop {
        // Drain protocol events before waiting for work: this is what keeps
        // configure/resize notices from piling up while we sleep.
        if let Err(error) = queue.dispatch_pending(&mut state) {
            tracing_note(format!("dispatch failed: {error}"));
            break;
        }
        let _ = connection.flush();

        match commands.recv_timeout(Duration::from_millis(50)) {
            Ok(Command::Present {
                output,
                frame,
                reply,
            }) => {
                let result = present_frame(&connection, &mut queue, &mut state, &output, frame);
                let _ = reply.send(result);
            }
            Ok(Command::Clear { output, reply }) => {
                let removed = state.surfaces.remove(&output).is_some();
                let _ = connection.flush();
                if removed {
                    tracing_note(format!("cleared {output}"));
                }
                let _ = reply.send(Ok(()));
            }
            Ok(Command::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    tracing_note("presenter stopped".to_string());
    state.surfaces.clear();
    let _ = connection.flush();
}

fn present_frame(
    connection: &Connection,
    queue: &mut wayland_client::EventQueue<PresenterState>,
    state: &mut PresenterState,
    output: &str,
    frame: Frame,
) -> Result<PresentOutcome, PresentError> {
    // Create the surface on first use.
    if !state.surfaces.contains_key(output) {
        create_surface(queue, state, output)?;
        wait_for_configure(queue, state, output, DEFAULT_TIMEOUT)?;
    }

    let size = match state.surfaces.get(output).and_then(|surface| surface.size) {
        Some(size) => size,
        None => {
            return Err(PresentError::ConfigureTimeout {
                output: output.to_string(),
                timeout: DEFAULT_TIMEOUT,
            });
        }
    };

    if size != frame.size() {
        // Hand the truth back to the caller instead of uploading a wrongly sized
        // buffer (which the compositor would stretch).
        return Ok(PresentOutcome::ResizeRequired { size });
    }

    upload(connection, state, output, &frame)?;
    Ok(PresentOutcome::Presented { size })
}

fn create_surface(
    queue: &mut wayland_client::EventQueue<PresenterState>,
    state: &mut PresenterState,
    output: &str,
) -> Result<(), PresentError> {
    let target = state
        .output_state
        .outputs()
        .find(|candidate| {
            state
                .output_state
                .info(candidate)
                .and_then(|info| info.name)
                .as_deref()
                == Some(output)
        })
        .ok_or_else(|| PresentError::UnknownOutput(output.to_string()))?;

    let qh = queue.handle();
    let surface = state.compositor_state.create_surface(&qh);
    let layer = state.layer_shell.create_layer_surface(
        &qh,
        surface,
        // Background: below every window, above the desktop. The whole point.
        Layer::Background,
        Some(&state.namespace),
        Some(&target),
    );
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    // -1 means "do not reserve space": a wallpaper is not a panel.
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // Empty input region: clicks must reach the desktop, not the wallpaper
    // (TRD FR-CORE-5). The region is applied at commit and then destroyed.
    let region = state
        .compositor_state
        .wl_compositor()
        .create_region(&qh, ());
    layer.wl_surface().set_input_region(Some(&region));

    // Buffer scale is fixed at 1 in P1: we render exactly at the compositor's
    // configured (logical) size. HiDPI/fractional scaling needs physical-size
    // rendering and is tracked for P2.
    layer.wl_surface().set_buffer_scale(1);
    layer.commit();
    region.destroy();

    let surface_id = layer.wl_surface().id().protocol_id();

    // A deliberately tiny initial pool: the real size is only known once the
    // compositor configures the surface, and the configure handler replaces this
    // pool with one sized for the answer. Guessing a size here would either waste
    // megabytes or (worse) look authoritative.
    let initial = (8_u32, 8_u32);
    let pool = SlotPool::new(pool_bytes(initial), &state.shm)
        .map_err(|error| PresentError::Wayland(format!("shm pool: {error}")))?;

    state.surfaces.insert(
        output.to_string(),
        OutputSurface {
            surface_id,
            layer,
            pool,
            buffers: [None, None],
            next_slot: 0,
            size: None,
        },
    );
    tracing_note(format!("created a background surface on {output}"));
    Ok(())
}

/// Block until the compositor has configured `output`'s surface (or the deadline
/// passes).
///
/// `dispatch_pending` alone is **not** enough here, and this function used to get
/// that wrong: it only dispatches events that have already been read off the
/// socket, and nothing reads the socket unless we ask. Since the configure is
/// produced *by* the commit we just made, a loop of `dispatch_pending` can wait
/// forever for an event that is sitting unread in the kernel buffer. The E2E gate
/// caught exactly that: "compositor did not configure the surface within 2s" on a
/// session where the same surface maps instantly.
///
/// `roundtrip` reads the socket and blocks for the sync callback. A Wayland
/// compositor emits events in the order it processes requests, and our commit went
/// out before the sync request, so by the time the callback arrives the configure
/// has already been read and dispatched.
fn wait_for_configure(
    queue: &mut wayland_client::EventQueue<PresenterState>,
    state: &mut PresenterState,
    output: &str,
    timeout: Duration,
) -> Result<(u32, u32), PresentError> {
    let deadline = Instant::now() + timeout;
    loop {
        // Anything already read (a configure that raced with our commit).
        let _ = queue.dispatch_pending(state);
        if let Some(size) = state.surfaces.get(output).and_then(|surface| surface.size) {
            return Ok(size);
        }
        if Instant::now() >= deadline {
            break;
        }
        if let Err(error) = queue.roundtrip(state) {
            return Err(PresentError::Wayland(format!(
                "roundtrip while waiting for the configure of {output}: {error}"
            )));
        }
    }
    // One last look after the final roundtrip, then give up honestly.
    let _ = queue.dispatch_pending(state);
    state
        .surfaces
        .get(output)
        .and_then(|surface| surface.size)
        .ok_or_else(|| PresentError::ConfigureTimeout {
            output: output.to_string(),
            timeout,
        })
}

fn upload(
    connection: &Connection,
    state: &mut PresenterState,
    output: &str,
    frame: &Frame,
) -> Result<(), PresentError> {
    let size = frame.size();
    let stride = stride_of(frame.width);

    let surface = state
        .surfaces
        .get_mut(output)
        .ok_or_else(|| PresentError::UnknownOutput(output.to_string()))?;

    let slot = acquire_slot(surface, size, stride)?;

    // Take the buffer out of the array so the pool can be borrowed mutably while
    // we write into its canvas (the borrow checker is right to insist).
    let buffer = surface
        .buffers
        .get_mut(slot)
        .and_then(Option::take)
        .ok_or_else(|| PresentError::Wayland("buffer slot vanished".to_string()))?;

    {
        let canvas = buffer
            .canvas(&mut surface.pool)
            .ok_or_else(|| PresentError::Busy(output.to_string()))?;
        let needed = (stride as usize) * (size.1 as usize);
        if canvas.len() < needed {
            return Err(PresentError::FrameSize {
                expected_w: (canvas.len() / 4 / size.1.max(1) as usize) as u32,
                expected_h: size.1,
                actual_w: frame.width,
                actual_h: frame.height,
            });
        }
        canvas[..needed].copy_from_slice(&frame.pixels[..needed]);
    }

    let wl_surface = surface.layer.wl_surface();
    buffer
        .attach_to(wl_surface)
        .map_err(|error| PresentError::Wayland(error.to_string()))?;
    wl_surface.damage_buffer(
        0,
        0,
        i32::try_from(size.0).unwrap_or(i32::MAX),
        i32::try_from(size.1).unwrap_or(i32::MAX),
    );
    surface.layer.commit();

    // Hand the buffer back to the pool's slot: dropping it would destroy the
    // wl_buffer while the compositor may still be reading it.
    surface.buffers[slot] = Some(buffer);

    connection
        .flush()
        .map_err(|error| PresentError::Wayland(error.to_string()))?;
    Ok(())
}

/// Find a slot whose buffer the compositor has released, creating one if the slot
/// is empty. Bounded at two slots — never grows the pool.
fn acquire_slot(
    surface: &mut OutputSurface,
    size: (u32, u32),
    stride: u32,
) -> Result<usize, PresentError> {
    for _ in 0..2 {
        let slot = surface.next_slot % 2;
        surface.next_slot = surface.next_slot.wrapping_add(1);

        match surface.buffers[slot].take() {
            None => {
                let (buffer, _canvas) = surface
                    .pool
                    .create_buffer(
                        i32::try_from(size.0).unwrap_or(i32::MAX),
                        i32::try_from(size.1).unwrap_or(i32::MAX),
                        i32::try_from(stride).unwrap_or(i32::MAX),
                        PIXEL_FORMAT,
                    )
                    .map_err(|error| PresentError::Wayland(error.to_string()))?;
                surface.buffers[slot] = Some(buffer);
                return Ok(slot);
            }
            Some(buffer) => {
                let free = buffer.canvas(&mut surface.pool).is_some();
                surface.buffers[slot] = Some(buffer);
                if free {
                    return Ok(slot);
                }
            }
        }
    }

    // Both slots are still compositor-held. Give the protocol a moment to release
    // one before declaring failure.
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(5));
        for slot in 0..2 {
            let Some(buffer) = surface.buffers[slot].as_mut() else {
                continue;
            };
            if buffer.canvas(&mut surface.pool).is_some() {
                return Ok(slot);
            }
        }
    }

    Err(PresentError::Busy("an OWE output".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_solid_frame_has_the_expected_layout() {
        let frame = Frame::solid(2, 3, [1, 2, 3]);
        assert_eq!(frame.size(), (2, 3));
        assert_eq!(frame.pixels.len(), 2 * 3 * 4);
        assert_eq!(&frame.pixels[0..4], &[1, 2, 3, 0xff]);
    }

    #[test]
    fn stride_is_four_bytes_per_pixel() {
        assert_eq!(stride_of(1), 4);
        assert_eq!(stride_of(1366), 5464);
    }

    #[test]
    fn pool_size_covers_one_full_buffer() {
        // 1366x768 is the reference machine's panel.
        assert_eq!(pool_bytes((1366, 768)), 5464 * 768);
    }

    #[test]
    fn starting_either_connects_or_reports_no_session_cleanly() {
        // Two environments matter here and both must be clean:
        //
        // - CI and any headless box: there is no session, so `start` must return
        //   `NoSession` (never panic, never hang).
        // - A developer's live desktop: `start` succeeds, reports its outputs, and
        //   draws *nothing* — repainting someone's screen as a side effect of
        //   `cargo test` would be hostile. The drawing paths are exercised by
        //   scripts/e2e-hyprland.sh, which screenshots the result.
        match Presenter::start("owe-test") {
            Err(PresentError::NoSession(message)) => assert!(!message.is_empty()),
            Err(other) => panic!("expected NoSession without a session, got {other}"),
            Ok(mut presenter) => {
                eprintln!(
                    "live session detected; presenter reported {} output(s) and drew nothing",
                    presenter.outputs().len()
                );
                presenter.shutdown();
            }
        }
    }
}
