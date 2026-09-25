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
//! **A bounded buffer pool per output, reused forever.** Ported lesson from the
//! studied reference engines: pools that grow on demand high-water-mark the
//! process (wallr measured ~390 MiB retained after 90 rapid switches with
//! unbounded growth). The default is three slots, and the daemon's validated
//! `render.buffering.max_in_flight` can raise or lower that bound without ever
//! making the pool grow on demand.
//!
//! **The compositor's `configure` size is authoritative.** We never assume the
//! size we rendered at is the size we should present; when they differ we report
//! it and let the caller re-render. That is what makes scale/HiDPI/output changes
//! a normal event instead of a subtle wrong-size bug.
//!
//! **Two event sources, no polling (P2).** The loop waits on the Wayland socket
//! and on a command channel through `calloop` (ARCHITECTURE §4 names calloop as
//! the event loop). P1 polled its command channel every 50 ms, which cost 37
//! timer ticks and 0.6 % of a core over a minute *while doing nothing*; a
//! wallpaper engine that burns CPU with a static image on screen has failed its
//! own thesis. Now an idle daemon blocks in `poll(2)` and wakes for nothing.
//!
//! **The first present of an output is completed by its configure.** The old code
//! created the surface and then *blocked* in a roundtrip waiting for the
//! compositor to configure it, which needs the event queue mid-callback and forced
//! a nested dispatch. Instead the frame is parked, and the `configure` handler
//! finishes the present when it arrives — which is also what makes the loop a
//! single flat dispatch machine.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use calloop::channel::{
    Channel as CalloopChannel, Event as ChannelEvent, Sender as ChannelSender,
    channel as calloop_channel,
};
use calloop::{EventLoop, LoopHandle};
use calloop_wayland_source::WaylandSource;
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
const FRAME_CALLBACK_TIMEOUT: Duration = Duration::from_secs(2);
const CANCEL_TIMEOUT: Duration = Duration::from_millis(250);
const DEFAULT_MAX_BUFFERS: usize = 3;
const MAX_BUFFERS: usize = 8;

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
    /// An output was plugged in (FR-LIB-4). The daemon's supervisor turns this
    /// into "spawn a worker and apply the configured wallpaper".
    OutputAdded {
        /// Output name.
        output: String,
    },
    /// An output was unplugged. Its surface is released, but the daemon keeps the
    /// session entry so a replug restores the same wallpaper (PRD-F-08).
    OutputRemoved {
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
    #[error("all buffers for `{0}` are still in use by the compositor")]
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
        wait_for_frame: bool,
        reply: SyncSender<Result<PresentOutcome, PresentError>>,
    },
    Clear {
        output: String,
        reply: SyncSender<Result<(), PresentError>>,
    },
    CancelFrame {
        output: String,
        reply: SyncSender<Result<(), PresentError>>,
    },
    Shutdown,
}

/// The handle the daemon uses to present frames.
pub struct Presenter {
    /// A calloop channel sender, not a `std::sync::mpsc` one: the receiving end is
    /// registered as an event-loop source, so the presenter thread blocks until a
    /// command actually arrives instead of waking up to look for one.
    tx: Option<ChannelSender<Command>>,
    /// `None` once the receiver has been handed to the daemon's hotplug thread
    /// (see [`Presenter::take_events`]).
    events: Option<Receiver<PresenterEvent>>,
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
        Self::start_with_buffers(namespace, DEFAULT_MAX_BUFFERS)
    }

    /// Start the presenter with a bounded per-output buffer count.
    pub fn start_with_buffers(namespace: &str, max_buffers: usize) -> Result<Self, PresentError> {
        if !(1..=MAX_BUFFERS).contains(&max_buffers) {
            return Err(PresentError::Wayland(format!(
                "max in-flight buffers must be between 1 and {MAX_BUFFERS}, got {max_buffers}"
            )));
        }
        // The command channel is calloop's, so the presenter thread can block on it
        // as an event source rather than polling it.
        let (command_tx, command_rx) = calloop_channel::<Command>();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<PresenterEvent>();
        let (ready_tx, ready_rx) = sync_channel::<Result<Vec<String>, PresentError>>(1);

        let namespace = namespace.to_string();
        let join = std::thread::Builder::new()
            .name("owe-presenter".to_string())
            .spawn(move || run_presenter(&namespace, max_buffers, command_rx, &event_tx, &ready_tx))
            .map_err(|error| {
                PresentError::NoSession(format!("cannot spawn presenter thread: {error}"))
            })?;

        // Wait for setup to finish so failures are reported to the caller rather
        // than silently swallowed by a background thread.
        match ready_rx.recv() {
            Ok(Ok(outputs)) => Ok(Self {
                tx: Some(command_tx),
                events: Some(event_rx),
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
        self.present_inner(output, frame, false)
    }

    pub fn present_frame(
        &self,
        output: &str,
        frame: Frame,
    ) -> Result<PresentOutcome, PresentError> {
        self.present_inner(output, frame, true)
    }

    fn present_inner(
        &self,
        output: &str,
        frame: Frame,
        wait_for_frame: bool,
    ) -> Result<PresentOutcome, PresentError> {
        let tx = self.tx.as_ref().ok_or(PresentError::NotRunning)?;
        let (reply_tx, reply_rx) = sync_channel(1);
        tx.send(Command::Present {
            output: output.to_string(),
            frame,
            wait_for_frame,
            reply: reply_tx,
        })
        .map_err(|error| PresentError::Wayland(format!("cannot reach the presenter: {error}")))?;
        let timeout = if wait_for_frame {
            FRAME_CALLBACK_TIMEOUT
        } else {
            DEFAULT_TIMEOUT * 4
        };
        match reply_rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                if wait_for_frame {
                    let _ = self.cancel_frame(output);
                }
                Err(PresentError::ConfigureTimeout {
                    output: output.to_string(),
                    timeout,
                })
            }
            Err(RecvTimeoutError::Disconnected) => Err(PresentError::NotRunning),
        }
    }

    pub fn cancel_frame(&self, output: &str) -> Result<(), PresentError> {
        let tx = self.tx.as_ref().ok_or(PresentError::NotRunning)?;
        let (reply_tx, reply_rx) = sync_channel(1);
        tx.send(Command::CancelFrame {
            output: output.to_string(),
            reply: reply_tx,
        })
        .map_err(|error| PresentError::Wayland(format!("cannot reach the presenter: {error}")))?;
        match reply_rx.recv_timeout(CANCEL_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(PresentError::ConfigureTimeout {
                output: output.to_string(),
                timeout: CANCEL_TIMEOUT,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(PresentError::NotRunning),
        }
    }

    /// Remove OWE's surface from an output.
    pub fn clear(&self, output: &str) -> Result<(), PresentError> {
        let tx = self.tx.as_ref().ok_or(PresentError::NotRunning)?;
        let (reply_tx, reply_rx) = sync_channel(1);
        tx.send(Command::Clear {
            output: output.to_string(),
            reply: reply_tx,
        })
        .map_err(|error| PresentError::Wayland(format!("cannot reach the presenter: {error}")))?;
        match reply_rx.recv_timeout(DEFAULT_TIMEOUT * 2) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(PresentError::ConfigureTimeout {
                output: output.to_string(),
                timeout: DEFAULT_TIMEOUT * 2,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(PresentError::NotRunning),
        }
    }

    /// Take everything the presenter has reported since the last call.
    ///
    /// Returns nothing once the receiver has been taken by [`Self::take_events`],
    /// because there is only one receiver: the caller either polls or blocks, and
    /// doing both is how events get lost.
    pub fn drain_events(&self) -> Vec<PresenterEvent> {
        let mut events = Vec::new();
        if let Some(receiver) = self.events.as_ref() {
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
        }
        events
    }

    /// Hand the event stream to a blocking consumer (the daemon's hotplug thread).
    ///
    /// Blocking on the channel is what keeps hotplug-free sessions at zero wakeups:
    /// the supervisor sleeps until the compositor says something.
    pub fn take_events(&mut self) -> Option<Receiver<PresenterEvent>> {
        self.events.take()
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
    buffers: Vec<Option<Buffer>>,
    next_slot: usize,
    size: Option<(u32, u32)>,
}

/// A present whose surface does not exist (or is not configured) yet.
///
/// Parked here rather than waited for: the configure arrives on the Wayland socket
/// a few microseconds later, and the loop is already going to be woken by it.
/// Blocking for it would need the event queue inside a callback.
struct PendingPresent {
    frame: Frame,
    wait_for_frame: bool,
    reply: SyncSender<Result<PresentOutcome, PresentError>>,
}

struct FrameWaiter {
    size: (u32, u32),
    reply: SyncSender<Result<PresentOutcome, PresentError>>,
}

struct PresenterState {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    namespace: String,
    max_buffers: usize,
    surfaces: HashMap<String, OutputSurface>,
    /// Output name by `wl_output` protocol id, for hotplug callbacks.
    output_names: HashMap<u32, String>,
    events: Sender<PresenterEvent>,
    /// Presents waiting for their surface's first configure, by output.
    pending: HashMap<String, PendingPresent>,
    frame_waiters: HashMap<String, FrameWaiter>,
    /// Set by the shutdown command; the loop checks it after each dispatch.
    stop: bool,
    /// A clone of the connection, for flushing after commits. Cheap: the
    /// underlying socket is shared.
    connection: Connection,
    /// A handle to the event queue, stored so protocol objects can be created from
    /// inside a callback (the queue itself belongs to the Wayland event source).
    queue_handle: QueueHandle<PresenterState>,
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
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        let id = surface.id().protocol_id();
        let Some(output) = self
            .surfaces
            .iter()
            .find(|(_, candidate)| candidate.surface_id == id)
            .map(|(output, _)| output.clone())
        else {
            return;
        };
        if let Some(waiter) = self.frame_waiters.remove(&output) {
            let size = self
                .surfaces
                .get(&output)
                .and_then(|surface| surface.size)
                .unwrap_or(waiter.size);
            let _ = waiter.reply.send(Ok(PresentOutcome::Presented { size }));
        }
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
            self.output_names
                .insert(output.id().protocol_id(), name.clone());
            // FR-LIB-4: the daemon has to give this output a worker and a
            // wallpaper, and it can only know that from this event.
            let _ = self
                .events
                .send(PresenterEvent::OutputAdded { output: name });
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
        output: wl_output::WlOutput,
    ) {
        let Some(name) = self.output_names.remove(&output.id().protocol_id()) else {
            return;
        };
        tracing_note(format!("output disappeared: {name}"));

        // Release the surface and its buffers right away: a torn-down output is
        // exactly where a pool leak would hide (the P2 gate measures RSS across
        // twenty plug cycles).
        if let Some(surface) = self.surfaces.remove(&name) {
            surface.layer.wl_surface().destroy();
        }
        if let Some(pending) = self.pending.remove(&name) {
            let _ = pending
                .reply
                .send(Err(PresentError::UnknownOutput(name.clone())));
        }
        if let Some(waiter) = self.frame_waiters.remove(&name) {
            let _ = waiter
                .reply
                .send(Err(PresentError::UnknownOutput(name.clone())));
        }
        let _ = self
            .events
            .send(PresenterEvent::OutputRemoved { output: name });
    }
}

impl LayerShellHandler for PresenterState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let id = layer.wl_surface().id().protocol_id();
        if let Some(name) = self
            .surfaces
            .iter()
            .find(|(_, surface)| surface.surface_id == id)
            .map(|(name, _)| name.clone())
        {
            self.surfaces.remove(&name);
            if let Some(pending) = self.pending.remove(&name) {
                let _ = pending
                    .reply
                    .send(Err(PresentError::UnknownOutput(name.clone())));
            }
            if let Some(waiter) = self.frame_waiters.remove(&name) {
                let _ = waiter
                    .reply
                    .send(Err(PresentError::UnknownOutput(name.clone())));
            }
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

        // The output name is copied out (not borrowed) so the rest of the handler
        // can borrow `self` mutably: `finish_present` needs the whole state.
        let Some(output) = self
            .surfaces
            .iter_mut()
            .find(|(_, surface)| surface.surface_id == id)
            .map(|(name, _)| name.clone())
        else {
            return;
        };

        let mut changed = false;
        if let Some(surface) = self.surfaces.get_mut(&output)
            && surface.size != Some(size)
        {
            // A different size invalidates the parked buffers (and their pool).
            surface.size = Some(size);
            let buffer_count = surface.buffers.len();
            surface.buffers = (0..buffer_count).map(|_| None).collect();
            surface.pool = match SlotPool::new(pool_bytes(size), &self.shm) {
                Ok(pool) => pool,
                Err(error) => {
                    tracing_note(format!("cannot resize the shm pool for {output}: {error}"));
                    return;
                }
            };
            changed = true;
        }

        // A parked present finishes here: this is the whole reason the first
        // present of an output does not block waiting for a configure.
        if let Some(pending) = self.pending.remove(&output) {
            let outcome = finish_present(
                self,
                &output,
                &pending.frame,
                size,
                pending.wait_for_frame,
                &pending.reply,
            );
            if !pending.wait_for_frame || !matches!(&outcome, Ok(PresentOutcome::Presented { .. }))
            {
                let _ = pending.reply.send(outcome);
            }
        }

        if changed {
            let _ = self.events.send(PresenterEvent::Configured {
                output: output.clone(),
                size,
            });
        }
    }
}

/// Upload a frame to an already-configured surface, or report the size the
/// compositor actually wants.
fn finish_present(
    state: &mut PresenterState,
    output: &str,
    frame: &Frame,
    configured: (u32, u32),
    wait_for_frame: bool,
    reply: &SyncSender<Result<PresentOutcome, PresentError>>,
) -> Result<PresentOutcome, PresentError> {
    if configured != frame.size() {
        return Ok(PresentOutcome::ResizeRequired { size: configured });
    }
    if wait_for_frame {
        arm_frame_callback(state, output, configured, reply.clone())?;
    }
    let connection = state.connection.clone();
    if let Err(error) = upload(&connection, state, output, frame) {
        if wait_for_frame {
            state.frame_waiters.remove(output);
        }
        return Err(error);
    }
    Ok(PresentOutcome::Presented { size: configured })
}

fn arm_frame_callback(
    state: &mut PresenterState,
    output: &str,
    size: (u32, u32),
    reply: SyncSender<Result<PresentOutcome, PresentError>>,
) -> Result<(), PresentError> {
    let Some(surface) = state.surfaces.get(output) else {
        return Err(PresentError::UnknownOutput(output.to_string()));
    };
    surface
        .layer
        .wl_surface()
        .frame(&state.queue_handle, surface.layer.wl_surface().clone());
    if let Some(previous) = state
        .frame_waiters
        .insert(output.to_string(), FrameWaiter { size, reply })
    {
        let _ = previous
            .reply
            .send(Err(PresentError::Busy(output.to_string())));
    }
    Ok(())
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
    max_buffers: usize,
    commands: CalloopChannel<Command>,
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
        max_buffers,
        surfaces: HashMap::new(),
        output_names: HashMap::new(),
        events: events.clone(),
        pending: HashMap::new(),
        frame_waiters: HashMap::new(),
        stop: false,
        connection: connection.clone(),
        // Cloned before the queue is moved into the event source.
        queue_handle: queue.handle(),
    };

    // First roundtrip: we need the output list before we can name one. Before the
    // loop exists, so the queue is still ours to use directly.
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

    // --- the event loop -------------------------------------------------------
    //
    // Two sources, both blocking: the Wayland socket and the command channel.
    // `dispatch` parks in `poll(2)` until one of them has work, which is what makes
    // an idle wallpaper cost nothing at all (NFR-PERF-1, re-measured in P2).
    let mut event_loop: EventLoop<PresenterState> = match EventLoop::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            let _ = ready.send(Err(PresentError::Wayland(format!(
                "cannot create the event loop: {error}"
            ))));
            return;
        }
    };
    let handle: LoopHandle<PresenterState> = event_loop.handle();

    // Wayland first: its source owns the queue and dispatches pending events, so a
    // configure or an output change wakes the loop (and runs the handlers) without
    // a single wakeup of our own.
    if let Err(error) = WaylandSource::new(connection.clone(), queue).insert(handle.clone()) {
        let _ = ready.send(Err(PresentError::Wayland(format!(
            "cannot register the wayland source: {error}"
        ))));
        return;
    }

    if let Err(error) = handle.insert_source(commands, |event, _, state: &mut PresenterState| {
        match event {
            ChannelEvent::Msg(command) => handle_command(state, command),
            // The sender lives in the `Presenter` handle, so this only fires when
            // the daemon dropped it: stop rather than spin on a dead channel.
            ChannelEvent::Closed => state.stop = true,
        }
    }) {
        let _ = ready.send(Err(PresentError::Wayland(format!(
            "cannot register the command source: {error}"
        ))));
        return;
    }

    if let Err(error) = ready.send(Ok(outputs.clone())) {
        tracing_note(format!("cannot report readiness: {error}"));
        return;
    }
    tracing_note(format!("presenter ready, outputs: {}", outputs.join(", ")));

    while !state.stop {
        // `None` means "block until something happens", which is the point.
        if let Err(error) = event_loop.dispatch(None, &mut state) {
            tracing_note(format!("event loop failed: {error}"));
            break;
        }
    }

    tracing_note("presenter stopped".to_string());
    state.surfaces.clear();
    for (_, pending) in state.pending.drain() {
        let _ = pending.reply.send(Err(PresentError::NotRunning));
    }
    for (_, waiter) in state.frame_waiters.drain() {
        let _ = waiter.reply.send(Err(PresentError::NotRunning));
    }
    let _ = connection.flush();
}

/// Handle one command from a client.
fn handle_command(state: &mut PresenterState, command: Command) {
    match command {
        Command::Present {
            output,
            frame,
            wait_for_frame,
            reply,
        } => match begin_present(state, &output, &frame, wait_for_frame, &reply) {
            PresentStart::Done(result) => {
                if !wait_for_frame || !matches!(&result, Ok(PresentOutcome::Presented { .. })) {
                    let _ = reply.send(result);
                }
            }
            PresentStart::Parked => {
                if let Some(previous) = state.pending.insert(
                    output.clone(),
                    PendingPresent {
                        frame,
                        wait_for_frame,
                        reply,
                    },
                ) {
                    let _ = previous.reply.send(Err(PresentError::Busy(output)));
                }
            }
        },
        Command::Clear { output, reply } => {
            let removed = state.surfaces.remove(&output).is_some();
            if let Some(pending) = state.pending.remove(&output) {
                let result = if pending.wait_for_frame {
                    Err(PresentError::UnknownOutput(output.clone()))
                } else {
                    Ok(PresentOutcome::Presented {
                        size: pending.frame.size(),
                    })
                };
                let _ = pending.reply.send(result);
            }
            if let Some(waiter) = state.frame_waiters.remove(&output) {
                let _ = waiter
                    .reply
                    .send(Err(PresentError::UnknownOutput(output.clone())));
            }
            let _ = state.connection.flush();
            if removed {
                tracing_note(format!("cleared {output}"));
            }
            let _ = reply.send(Ok(()));
        }
        Command::CancelFrame { output, reply } => {
            state.frame_waiters.remove(&output);
            if let Some(pending) = state.pending.remove(&output) {
                let _ = pending.reply.send(Err(PresentError::NotRunning));
            }
            let _ = reply.send(Ok(()));
        }
        Command::Shutdown => state.stop = true,
    }
    let _ = state.connection.flush();
}

/// Whether a present finished immediately or had to wait for a configure.
enum PresentStart {
    /// The frame was uploaded (or the size matches not): answer the caller now.
    Done(Result<PresentOutcome, PresentError>),
    /// No configure yet: park the frame and answer from the configure handler.
    Parked,
}

/// Start presenting `frame` on `output`.
fn begin_present(
    state: &mut PresenterState,
    output: &str,
    frame: &Frame,
    wait_for_frame: bool,
    reply: &SyncSender<Result<PresentOutcome, PresentError>>,
) -> PresentStart {
    // Create the surface on first use. Its configure arrives on the Wayland
    // socket right after, which is what wakes the loop to finish the present.
    if !state.surfaces.contains_key(output) {
        return match create_surface(state, output) {
            Ok(()) => PresentStart::Parked,
            Err(error) => PresentStart::Done(Err(error)),
        };
    }

    match state.surfaces.get(output).and_then(|surface| surface.size) {
        Some(size) => PresentStart::Done(finish_present(
            state,
            output,
            frame,
            size,
            wait_for_frame,
            reply,
        )),
        None => PresentStart::Parked,
    }
}

fn create_surface(state: &mut PresenterState, output: &str) -> Result<(), PresentError> {
    let queue_handle = state.queue_handle.clone();
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

    let surface = state.compositor_state.create_surface(&queue_handle);
    let layer = state.layer_shell.create_layer_surface(
        &queue_handle,
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
        .create_region(&queue_handle, ());
    layer.wl_surface().set_input_region(Some(&region));

    // Buffer scale is fixed at 1: we render exactly at the compositor's configured
    // (logical) size. Fractional scaling is applied by the compositor, so the
    // buffer it asks for is already the right size.
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
            buffers: (0..state.max_buffers).map(|_| None).collect(),
            next_slot: 0,
            size: None,
        },
    );
    tracing_note(format!("created a background surface on {output}"));
    Ok(())
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
/// is empty. The slot count is fixed when the output is created.
fn acquire_slot(
    surface: &mut OutputSurface,
    size: (u32, u32),
    stride: u32,
) -> Result<usize, PresentError> {
    let count = surface.buffers.len();
    for _ in 0..count {
        let slot = surface.next_slot % count;
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

    // All slots are still compositor-held. Give the protocol a moment to release
    // one before declaring failure.
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(5));
        for slot in 0..count {
            let Some(buffer) = surface.buffers[slot].as_mut() else {
                continue;
            };
            if buffer.canvas(&mut surface.pool).is_some() {
                return Ok(slot);
            }
        }
    }

    Err(PresentError::Busy(format!(
        "all {count} buffers for an OWE output are still in use"
    )))
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
