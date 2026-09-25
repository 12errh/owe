//! The apply pipeline: from a wallpaper reference to pixels on a real output.
//!
//! This is the only place in the daemon that owns state, and it is deliberately
//! boring: resolve the target, decode once, render per output, present, record
//! what happened. The interesting logic lives in testable crates —
//! `owe_core::worker` decides *when* to render, `owe_core::output` decides
//! *where*, `owe_render` decides *how* — so this module mostly sequences calls.
//!
//! Startup is lazy and fallible on purpose: a machine with no Wayland session (a
//! CI container, an SSH login) must still run `owed` and answer its IPC, because
//! `config.get` and `daemon.kill` are useful there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use owe_core::config::{FPS_RANGE, MAX_TRANSITION_MS, Transition};
use owe_core::model::{ContentKind, WallpaperRef, WallpaperSource};
use owe_core::output::{OutputInfo, OutputTarget};
use owe_core::outputs as output_config;
use owe_core::path::XdgPaths;
use owe_core::shell::{ApplyOutcome, DrawMode, Registry, SelectError, ShellBackend, ShellError};
use owe_core::state::{SessionState, StateError};
use owe_core::supervisor::{HotplugEvent, Supervisor, SupervisorAction};
use owe_core::{Config, OutputWorker, WorkerAction, WorkerEvent};
use owe_media::{DecodedImage, MediaError};
use owe_render::gpu::{HeadlessGpu, RenderError};
use owe_render::image::{self, PixelFormat, Scaling};
use owe_render::surface::{Frame, PresentError, PresentOutcome, Presenter, PresenterEvent};
use owe_render::transition::{Schedule, TransitionKind, TransitionRenderer};
use serde::Serialize;
use thiserror::Error;

use crate::playback::{FrameSink, PlaybackCmd, PlaybackError, PlaybackRegistry, PlaybackSnapshot};

/// The playback clock's draw target: the [`Engine`] the clocks belong to.
///
/// `FrameSink` is implemented on this cloneable handle, which holds a
/// `Weak<Engine>`, rather than on the engine itself for one reason: the registry
/// (and so the clocks) is built in [`Engine::new`], before anything can hand out
/// an `Arc<Engine>`. [`Engine::attach_playback`] fills the handle in once, and a
/// frame then takes the engine's own path — same GPU, same presenter, same fit
/// mode as a still.
///
/// A `Weak` rather than an `Arc` also keeps the clocks from being the last owners
/// of the engine: the daemon decides when it stops, not a wallpaper.
#[derive(Clone)]
struct EngineSink {
    /// The engine the clocks draw through, once it has attached.
    engine: Arc<Mutex<Weak<Engine>>>,
}

impl EngineSink {
    /// A sink that is not attached to an engine yet.
    fn new() -> Self {
        Self {
            engine: Arc::new(Mutex::new(Weak::new())),
        }
    }

    /// Point the sink at the running engine.
    fn attach(&self, engine: &Arc<Engine>) {
        match self.engine.lock() {
            Ok(mut slot) => *slot = Arc::downgrade(engine),
            Err(poisoned) => *poisoned.into_inner() = Arc::downgrade(engine),
        }
    }

    /// The engine, or the reason a frame cannot be drawn.
    fn engine(&self) -> Result<Arc<Engine>, String> {
        self.engine
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .upgrade()
            .ok_or_else(|| "the playback clock is not attached to a running engine".to_string())
    }
}

impl FrameSink for EngineSink {
    fn show(&self, output: &str, frame: &owe_media::DecodedFrame) -> Result<(u32, u32), String> {
        let engine = self.engine()?;
        let result = engine.show_frame(output, frame);
        if result.is_err() {
            engine
                .playback_sizes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(output);
        }
        result.map_err(|error| error.to_string())
    }

    fn cancel(&self, output: &str) {
        if let Ok(engine) = self.engine() {
            engine.cancel_playback_frame(output);
        }
    }
}

/// Resolves `library:<id>` references to files, so the engine can apply (and
/// restore) library items without owning a database.
///
/// Injected rather than built in: the engine stays testable with a fake resolver,
/// and the library service stays the only thing that knows SQLite exists.
pub trait LibraryResolver: Send + Sync {
    /// Absolute path of a library item, or the reason it cannot be resolved.
    fn resolve(&self, id: &str) -> Result<PathBuf, String>;

    /// The indexed content kind of a library item, or the reason it cannot be
    /// resolved. A `library:` reference names a row, not a file, so there is no
    /// extension to inspect: the kind can only come from what the scanner
    /// recorded.
    fn kind_of(&self, id: &str) -> Result<ContentKind, String>;
}

/// Background colour behind a `contain`-scaled wallpaper (opaque black, so the
/// surface never shows stale pixels).
const BACKGROUND: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Everything the apply pipeline can fail with, named so the IPC layer can map it
/// to a protocol error code without guessing.
#[derive(Debug, Error)]
pub enum EngineError {
    /// No shell backend could be selected for this session.
    #[error("{0}")]
    ShellSelection(#[from] SelectError),

    /// The shell backend failed to list outputs.
    #[error("{0}")]
    Shell(#[from] ShellError),

    /// The wallpaper reference itself is invalid.
    #[error("{0}")]
    Model(#[from] owe_core::ModelError),

    /// The effective configuration is invalid.
    #[error("{0}")]
    Config(String),

    /// The target output could not be resolved.
    #[error("{0}")]
    Target(#[from] owe_core::OutputSelectError),

    /// The file could not be decoded.
    #[error("{0}")]
    Media(#[from] MediaError),

    /// GPU rendering failed.
    #[error("{0}")]
    Render(#[from] RenderError),

    /// Handing the frame to the compositor failed.
    #[error("{0}")]
    Present(#[from] PresentError),

    /// Session state could not be persisted.
    #[error("{0}")]
    State(#[from] StateError),

    /// The GPU is unavailable (no adapter at all).
    #[error("no GPU adapter is available, so nothing can be rendered: {0}")]
    NoGpu(String),

    /// A transition name (or its timing) is not usable in this build.
    #[error("{0}")]
    Transition(String),

    /// A `library:<id>` reference could not be resolved to a file.
    #[error("{0}")]
    Library(String),

    /// A feature this build does not implement yet, named honestly rather than
    /// dressed up as a decode failure.
    #[error("{0}")]
    Unsupported(String),

    /// Every selected output was taken over by a newer change before this one
    /// reached the screen.
    #[error("{0}")]
    Superseded(String),

    /// The frame clock could not start or a playback command failed.
    #[error("{0}")]
    Playback(#[from] PlaybackError),
}

/// One output as the GUI and CLI see it.
#[derive(Debug, Clone, Serialize)]
pub struct OutputView {
    /// Connector name.
    pub name: String,
    /// Make/model description.
    pub description: String,
    /// Logical width in physical pixels.
    pub width: u32,
    /// Logical height in physical pixels.
    pub height: u32,
    /// Whether this output has focus.
    pub focused: bool,
    /// Wallpaper **this daemon run has actually presented**, if any.
    ///
    /// Deliberately not "what the session file says": those differ after a
    /// restart until session restore lands in P2, and reporting the recorded
    /// intent here would tell the user a wallpaper is on screen while the screen
    /// shows something else.
    pub wallpaper: Option<String>,
    /// Resolved content kind of that wallpaper.
    pub kind: Option<String>,
    /// Worker state name (`idle`, `presented`, `preparing`, `reconfigured`, `failed`).
    pub state: String,
    /// Reference recorded in the session file for this output that this run has
    /// **not** applied (restore is P2). Never merged into [`Self::wallpaper`].
    pub recorded: Option<String>,
    /// Failure reason when the state is `failed`.
    pub error: Option<String>,
}

/// What a successful apply did.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    /// The reference as given by the user.
    pub reference: String,
    /// Resolved content kind.
    pub kind: String,
    /// Outputs that now show it.
    pub outputs: Vec<String>,
    /// Pixel size presented per output.
    pub sizes: Vec<(String, u32, u32)>,
    /// The transition actually run, per output (`none` when the change was
    /// instant). Reported because "which transition did it use" is otherwise
    /// guesswork for a user with per-output sections and a global default.
    pub transitions: Vec<(String, String)>,
    /// Frames rendered per output — zero for an instant change.
    pub frames: Vec<(String, u32)>,
    /// Non-fatal notes: a transition skipped because there was nothing to fade
    /// from, a previous file that vanished, a reply that took a surprising path.
    /// Never empty-but-silent: a note exists only when something needs saying.
    pub notes: Vec<String>,
    /// Whether the change came from the session file rather than a live request.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub restored: bool,
    /// The shell's own theming pipeline was asked to run, and how many times OWE
    /// ran a theme refresh of its own.
    ///
    /// `runs` is always 0 or 1: OWE never runs a theme pipeline in either mode (a
    /// double theme is a bug class, TRD §4), and the shell either does (routed with
    /// `theme_hook`) or does not. Reported so "the theme updated once" is a
    /// measurable statement in a test and in `owectl`'s output, not a claim about a
    /// side effect the user has to eyeball.
    #[serde(skip_serializing_if = "ThemeRuns::is_zero")]
    pub theme: ThemeRuns,
    /// Whether this apply was carried out by the shell rather than drawn by OWE.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub routed: bool,
}

/// One backend's answer to "do you belong in this session?", for `shell.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendStatus {
    /// Backend id.
    pub id: String,
    /// `strong`, `weak` or `none`.
    pub confidence: String,
    /// Why, in the backend's own words.
    pub reason: String,
    /// Whether this is the backend the daemon is using.
    pub selected: bool,
    /// The draw mode this backend would use with the live config.
    pub mode: String,
}

/// The shell situation, as `shell.status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellStatus {
    /// Selected backend id, or `None` when none is usable here.
    pub backend: Option<String>,
    /// Why that backend was selected (or why none was).
    pub reason: Option<String>,
    /// Draw mode in effect: `daemon-drawn` or `shell-routed`.
    pub mode: String,
    /// Whether the selected backend routes wallpapers to a shell instead of drawing
    /// them, which is what decides whether a transition is possible at all.
    pub routed: bool,
    /// The `auto` chain, in the order it is probed.
    pub detect_order: Vec<String>,
    /// Every registered backend and what it says about this session.
    pub backends: Vec<BackendStatus>,
    /// Whether the live shell's own configuration was overridden at runtime
    /// (`config.patch`) rather than read from the file.
    pub patched: bool,
}

/// A runtime-only change to `[shell]` (BACKEND-DESIGN §3, `config.patch`).
///
/// Every field is optional: a patch names what changes and leaves the rest alone.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellPatch {
    /// `auto` or a registered backend id.
    #[serde(default)]
    pub backend: Option<String>,
    /// `daemon-drawn` or `shell-routed`.
    #[serde(default)]
    pub caelestia_mode: Option<String>,
    /// Let the shell's theming pipeline run after a routed change.
    #[serde(default)]
    pub theme_hook: Option<bool>,
    /// Where the shell's wallpapers live (relative references resolve here).
    #[serde(default)]
    pub wallpapers_dir: Option<String>,
    /// `auto` chain, in order.
    #[serde(default)]
    pub detect_order: Option<Vec<String>>,
    /// Subscribe to the Hyprland event socket.
    #[serde(default)]
    pub event_socket: Option<bool>,
    /// `warn`, `stop` or `ignore`.
    #[serde(default)]
    pub hyprpaper: Option<String>,
}

impl ShellPatch {
    /// Whether the patch asks for anything at all.
    ///
    /// A patch with no recognised keys is a client mistake, not a no-op: answering
    /// `ok` to a request that changed nothing is how a user concludes the feature is
    /// broken.
    pub fn is_empty(&self) -> bool {
        self.backend.is_none()
            && self.caelestia_mode.is_none()
            && self.theme_hook.is_none()
            && self.wallpapers_dir.is_none()
            && self.detect_order.is_none()
            && self.event_socket.is_none()
            && self.hyprpaper.is_none()
    }
}

/// Theme work done for one apply (see [`Applied::theme`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThemeRuns {
    /// Times OWE invoked a theme pipeline itself. Structurally zero: there is no
    /// such pipeline in this codebase, and this field exists so that a test can
    /// assert that rather than take it on faith.
    pub by_owe: u32,
    /// Whether the shell's pipeline ran because OWE asked it to switch wallpaper.
    pub by_shell: bool,
}

impl ThemeRuns {
    /// Nothing ran; the field is omitted from replies.
    fn is_zero(&self) -> bool {
        self.by_owe == 0 && !self.by_shell
    }

    /// Total theme refreshes caused by this apply.
    pub fn total(&self) -> u32 {
        self.by_owe + u32::from(self.by_shell)
    }
}

/// A wallpaper reference this run has presented on an output.
#[derive(Debug, Clone)]
struct AppliedRef {
    reference: String,
    kind: String,
}

/// The live transition on one output.
///
/// Shared so a *newer* apply can keep the no-snap promise: it bakes the frame the
/// user is looking at and continues from there, instead of restarting the blend
/// from the old wallpaper.
struct InFlight {
    /// Apply that owns the animation. A newer apply bumps this, and the older loop
    /// notices on its next frame and stops without presenting again.
    epoch: u64,
    /// The renderer, under a lock held across render+present so a retarget can
    /// never land between "computed the frame" and "showed it".
    renderer: Arc<Mutex<TransitionRenderer>>,
    /// Progress of the last frame that reached the screen (`f32::to_bits`).
    progress: Arc<AtomicU32>,
}

/// Where an output's wallpaper came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PlanSource {
    /// Recorded in the session file by an earlier apply (FR-LIB-5).
    Session,
    /// From an `[outputs.*]` section in the config.
    OutputSection(String),
}

/// What OWE intends to put on an output, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedWallpaper {
    /// The reference to apply.
    pub reference: String,
    /// Which input produced it.
    pub source: PlanSource,
}

/// Result of reconciling the output list with reality (FR-LIB-4).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ReconcileReport {
    /// Outputs that appeared since the last reconciliation.
    pub added: Vec<String>,
    /// Outputs that disappeared.
    pub removed: Vec<String>,
    /// Outputs whose wallpaper was (re-)applied: name → reference.
    pub applied: Vec<(String, String)>,
    /// Outputs that now show a wallpaper this run applied *because* of hotplug.
    pub cleared: Vec<String>,
    /// Failures, as name → reason. Never dropped: a monitor that came back blank
    /// is the bug this report exists to make visible.
    pub failures: Vec<(String, String)>,
    /// Output-resolution conflicts worth logging (a losing `[outputs.*]` section).
    pub conflicts: Vec<String>,
    /// Supervised outputs after reconciliation.
    pub tracked: Vec<String>,
}

impl ReconcileReport {
    /// Whether anything changed at all (used to skip logging a no-op).
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.applied.is_empty()
            && self.cleared.is_empty()
            && self.failures.is_empty()
    }

    /// One-line summary for the log.
    pub fn summary(&self) -> String {
        format!(
            "outputs: +{} -{}, re-applied {}, cleared {}, {} failure(s)",
            self.added.len(),
            self.removed.len(),
            self.applied.len(),
            self.cleared.len(),
            self.failures.len()
        )
    }
}

/// Result of restoring the recorded session at startup (FR-LIB-5).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RestoreReport {
    /// Outputs that now show their recorded wallpaper: name → reference.
    pub restored: Vec<(String, String)>,
    /// Outputs intentionally left alone, with the reason.
    pub skipped: Vec<(String, String)>,
    /// Outputs whose restore failed, with the reason. The desktop stays usable;
    /// the user is told exactly which monitor is blank and why.
    pub failures: Vec<(String, String)>,
}

/// The transition resolved for one change, before any pixels move.
#[derive(Debug, Clone, PartialEq)]
struct ResolvedTransition {
    kind: TransitionKind,
    schedule: Schedule,
}

/// What happened to one output during a change.
#[derive(Debug, Clone, PartialEq)]
enum OutputChange {
    /// The output now shows the new wallpaper.
    Presented {
        /// Size that actually reached the screen.
        size: (u32, u32),
        /// Frames rendered (1 for an instant change).
        frames: u32,
        /// Transition used.
        kind: TransitionKind,
        /// Anything worth telling the user (a skipped or cut-short animation).
        note: Option<String>,
    },
    /// A newer change took the output over; this one did not reach the screen.
    Superseded,
}

/// Everything one animation needs, grouped into a struct.
///
/// Six positional arguments plus `self` was over clippy's limit, but the real
/// reason to group them is that two images and a size are exactly the kind of
/// same-shaped arguments a caller can silently swap. Naming them makes that
/// impossible and reads better at the call site.
struct Animation<'a> {
    /// Output whose transition slot this animation occupies.
    output: &'a str,
    /// Target size; must match the renderer's, or it is rebuilt.
    size: (u32, u32),
    /// The image being blended out.
    from: &'a DecodedImage,
    /// The image being blended in.
    to: &'a DecodedImage,
    /// Catalogue entry plus the frame schedule it runs on.
    transition: &'a ResolvedTransition,
    /// Epoch guard: a newer apply bumps it and this animation stops.
    epoch: u64,
}

/// How an animation ended.
#[derive(Debug, Clone, PartialEq)]
enum AnimatedOutcome {
    /// The animation ran to completion, final frame on screen.
    Completed {
        /// Target size.
        size: (u32, u32),
        /// Frames presented.
        frames: u32,
    },
    /// A newer change owned the output before the animation ended.
    Superseded,
    /// The compositor reconfigured the surface mid-animation.
    ResizeRequired {
        /// The size it asked for.
        size: (u32, u32),
    },
}

/// Report from starting the engine, so the daemon can log one honest summary.
///
/// **Startup never fails.** A daemon that refuses to run because a compositor is
/// missing is useless exactly when it is most needed (a CI container, an SSH
/// session, a broken login) — `config.get` and `daemon.kill` must still answer.
/// Whatever could not be set up is reported in [`StartupReport::warnings`] and
/// fails later, at the call that actually needs it.
#[derive(Debug, Clone)]
pub struct StartupReport {
    /// Selected shell backend id, or `None` when none is usable here.
    pub backend: Option<String>,
    /// Why no backend was selected, as a sentence for logs.
    pub backend_error: Option<String>,
    /// The same failure in structured form.
    ///
    /// Kept alongside the message so `wallpaper.set` can answer with the *real*
    /// cause: the old code wrapped it in `ShellError::Unavailable { backend: "auto" }`,
    /// which reached users as `INTERNAL: auto is not available: unknown shell
    /// backend \`caelestia\`` — an internal error code for a config mistake, with
    /// a prefix that names a backend the user never asked for.
    pub selection_error: Option<SelectError>,
    /// Outputs the backend reported.
    pub outputs: Vec<OutputInfo>,
    /// Whether a presenter (Wayland session) is available.
    pub presenter: bool,
    /// Wayland-side outputs the presenter can draw on.
    pub presentable: Vec<String>,
    /// Non-fatal startup problems (session-state damage, etc.).
    pub warnings: Vec<String>,
}

impl StartupReport {
    /// Fail with the real reason when no backend is usable, preserving the
    /// structured cause so the IPC layer can choose the right error code.
    fn backend_failure(&self) -> Result<(), EngineError> {
        if let Some(error) = &self.selection_error {
            return Err(EngineError::ShellSelection(error.clone()));
        }
        if let Some(detail) = &self.backend_error {
            // Should be unreachable while selection is the only source of
            // `backend_error`, but a silent Ok here would hide a real failure.
            return Err(EngineError::Shell(ShellError::Backend {
                backend: self.backend.clone().unwrap_or_else(|| "none".to_string()),
                detail: detail.clone(),
            }));
        }
        Ok(())
    }
}

/// The daemon's state and pipelines.
pub struct Engine {
    config: Config,
    paths: XdgPaths,
    /// The frame clock that makes animated-image and video content move (P4).
    /// One per engine, one clock thread per playing output.
    playback: PlaybackRegistry,
    /// The clock's draw target. [`Engine::attach_playback`] fills it in; a build
    /// that never attaches it answers with that reason rather than a wrong one.
    playback_sink: EngineSink,
    /// The surface size each output's clock last had accepted, seeded from the
    /// output's own pixel size when a clock starts. Kept so a clock frame does not
    /// have to rediscover the size the compositor already answered with.
    playback_sizes: Mutex<HashMap<String, (u32, u32)>>,
    /// Behind a lock rather than a plain field so a backend can be registered
    /// while the daemon runs: the routing tests replace `caelestia` with a
    /// recording stub, and a `config.patch` that changes `shell.backend` will do
    /// the same for real.
    registry: RwLock<Registry>,
    /// A runtime `[shell]` patch, when one has been applied (`config.patch`).
    /// `None` means the file's value is in force.
    shell_override: RwLock<Option<owe_core::config::ShellConfig>>,
    shell_patch: Mutex<()>,
    generation: AtomicU64,
    /// What each output is actually showing, as of this process's lifetime.
    /// The session file is intent; this is fact.
    applied: Mutex<HashMap<String, AppliedRef>>,
    started: Mutex<Option<StartupReport>>,
    outputs: Mutex<Vec<OutputInfo>>,
    workers: Mutex<HashMap<String, OutputWorker>>,
    session: Mutex<SessionState>,
    /// Warnings raised while loading the session file at construction time.
    ///
    /// Kept here because the load happens in [`Engine::new`], not in
    /// [`Engine::start`]: loading there keeps a write made *before* start (a test
    /// fixture, a future config-driven seed) from being clobbered by the file on
    /// disk, and the warnings still have to reach the startup report.
    session_warnings: Mutex<Vec<String>>,
    gpu: Mutex<Option<HeadlessGpu>>,
    presenter: Mutex<Option<Presenter>>,
    /// Manual governor override; reported over IPC and honoured from P4 onwards,
    /// where pausing actually stops frames.
    paused: Mutex<bool>,
    /// Live transitions, one per output (FR-LIB-3 interruption).
    transitions: Mutex<HashMap<String, InFlight>>,
    /// Bumped on every apply: a running animation whose epoch is stale stops.
    epoch: AtomicU64,
    /// Hotplug bookkeeping: which outputs exist, which are failed, how often a
    /// worker has been restarted (NFR-REL-1).
    supervisor: Mutex<Supervisor>,
    /// `library:<id>` resolution, injected by the daemon once the library is open.
    library: Mutex<Option<Arc<dyn LibraryResolver>>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("backend", &self.config.shell.backend)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Build the engine, loading the persisted session immediately.
    ///
    /// The session file is the daemon's intent for the *next* run (FR-LIB-5,
    /// PRD-F-09), so it is read here — once, before anything can write to the
    /// in-memory state. Loading it inside [`Engine::start`] instead meant that any
    /// session write that happened first was silently discarded when the file on
    /// disk was assigned over it.
    pub fn new(config: Config, paths: XdgPaths) -> Self {
        let mut registry = Registry::new();
        // Caelestia first in registration so the default `auto` chain
        // (caelestia → hyprland → generic-layer-shell) is a property of the
        // registry rather than of a config file.
        registry.register(Arc::new(owe_shell_caelestia::CaelestiaBackend::new()));
        registry.register(Arc::new(owe_shell_hyprland::HyprlandBackend::new()));
        // The floor: any compositor that speaks plain Wayland. Last in the detect
        // order because it knows the least (no focus, no workspace events).
        registry.register(Arc::new(owe_shell_generic::GenericBackend::new()));

        // A damaged session file is a warning, never a failure: a daemon that
        // refuses to start because its cache is corrupt is useless exactly when it
        // is needed.
        let loaded = SessionState::load(&paths.state_dir.join("session.json"));

        let playback_sink = EngineSink::new();
        let registry_sink = playback_sink.clone();
        Self {
            config,
            paths,
            playback: PlaybackRegistry::new(registry_sink),
            playback_sink,
            playback_sizes: Mutex::new(HashMap::new()),
            registry: RwLock::new(registry),
            shell_override: RwLock::new(None),
            shell_patch: Mutex::new(()),
            generation: AtomicU64::new(1),
            started: Mutex::new(None),
            outputs: Mutex::new(Vec::new()),
            workers: Mutex::new(HashMap::new()),
            session: Mutex::new(loaded.state),
            session_warnings: Mutex::new(loaded.warnings),
            applied: Mutex::new(HashMap::new()),
            gpu: Mutex::new(None),
            presenter: Mutex::new(None),
            paused: Mutex::new(false),
            transitions: Mutex::new(HashMap::new()),
            epoch: AtomicU64::new(0),
            supervisor: Mutex::new(Supervisor::default()),
            library: Mutex::new(None),
        }
    }

    /// Register (or replace) a shell backend at runtime.
    ///
    /// Public because it is the seam that makes backend selection testable without
    /// a compositor: a test registers a backend whose `id()` is the one its config
    /// names, and the whole apply path — routing decisions, session recording,
    /// theme accounting — runs for real.
    #[allow(
        dead_code,
        reason = "the seam the routing tests register through; the first non-test caller is \
                  the future backend-override path"
    )]
    pub fn register_backend(&self, backend: Arc<dyn ShellBackend>) {
        let mut registry = self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.register(backend);
    }

    /// A registered backend by id.
    fn backend(&self, id: &str) -> Option<Arc<dyn ShellBackend>> {
        self.registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
    }

    /// The `[shell]` configuration in force: a runtime patch if one was applied,
    /// otherwise the file's value.
    ///
    /// One function rather than a field read at each call site, because "which
    /// config is live" must have exactly one answer for the daemon and for the IPC
    /// layer that reports it.
    pub fn shell_config(&self) -> owe_core::config::ShellConfig {
        self.shell_override
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| self.config.shell.clone())
    }

    pub fn effective_config(&self) -> Config {
        let mut config = self.config.clone();
        config.shell = self.shell_config();
        config
    }

    /// The shell situation, for `shell.status` and the GUI's status card.
    ///
    /// Runs each backend's detection, which is process/env only (no subprocess): a
    /// status call must be cheap enough for a UI to call on every page load.
    pub fn shell_status(&self) -> ShellStatus {
        let shell = self.shell_config();
        let registry = self
            .registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let selected = registry.select_with_reason(&shell, &env_lookup);
        let (backend, reason) = match &selected {
            Ok((backend, detection)) => (
                Some(backend.id().to_string()),
                Some(detection.reason.clone()),
            ),
            Err(error) => (None, Some(error.to_string())),
        };

        let mut backends: Vec<BackendStatus> = registry
            .detections(&shell, &env_lookup)
            .into_iter()
            .map(|(backend, detection)| BackendStatus {
                id: backend.id().to_string(),
                confidence: detection.confidence.as_str().to_string(),
                reason: detection.reason,
                selected: false,
                mode: backend.draw_mode(&shell).as_str().to_string(),
            })
            .collect();
        if let Some(id) = &backend {
            for row in &mut backends {
                row.selected = &row.id == id;
            }
        }

        let mode = selected
            .as_ref()
            .map(|(backend, _)| backend.draw_mode(&shell).as_str().to_string())
            .unwrap_or_else(|_| DrawMode::DaemonDrawn.as_str().to_string());

        ShellStatus {
            routed: mode == DrawMode::ShellRouted.as_str(),
            backend,
            reason,
            mode,
            detect_order: registry.detect_order(&shell),
            backends,
            patched: self
                .shell_override
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
        }
    }

    /// Apply a runtime patch to `[shell]`, and re-select the backend.
    ///
    /// Deliberately narrow: `shell.backend`, the Caelestia mode/theme switch, the
    /// wallpapers directory, the `auto` chain and the two Hyprland switches. That is
    /// what the GUI needs to let a user try a different backend without editing a
    /// file, and nothing more — a patch that could change `render.allow_transitions`
    /// or `library.paths` would be a config editor wearing an IPC method's name.
    ///
    /// The override lives in memory. OWE does not rewrite the user's config file as a
    /// side effect of a click: `config.patch` says so in its reply, and the file the
    /// user edits stays authoritative across restarts.
    ///
    /// A validation or selection failure leaves the previous override in place, so a
    /// typo cannot leave the daemon without a working backend.
    pub fn patch_shell(&self, patch: &ShellPatch) -> Result<ShellStatus, EngineError> {
        let _patch = self
            .shell_patch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut shell = self.shell_config();

        if let Some(backend) = &patch.backend {
            let trimmed = backend.trim();
            if trimmed != "auto" && !self.backend_ids().iter().any(|id| id == trimmed) {
                return Err(EngineError::ShellSelection(SelectError::UnknownBackend {
                    requested: trimmed.to_string(),
                    available: self.backend_ids().join(", "),
                }));
            }
            shell.backend = trimmed.to_string();
        }
        if let Some(mode) = &patch.caelestia_mode {
            if DrawMode::parse(mode).is_none() {
                return Err(EngineError::Shell(ShellError::Backend {
                    backend: owe_shell_caelestia::ID.to_string(),
                    detail: format!(
                        "`{mode}` is not a draw mode; expected `daemon-drawn` or `shell-routed`"
                    ),
                }));
            }
            shell.caelestia.mode = mode.trim().to_string();
        }
        if let Some(theme_hook) = patch.theme_hook {
            shell.caelestia.theme_hook = theme_hook;
        }
        if let Some(dir) = &patch.wallpapers_dir {
            shell.caelestia.wallpapers_dir = Some(dir.clone());
        }
        if let Some(order) = &patch.detect_order {
            if order.is_empty() {
                return Err(EngineError::Config(
                    "shell.detect_order: must list at least one backend id".to_string(),
                ));
            }
            let mut seen = Vec::new();
            for id in order {
                if !self.backend_ids().iter().any(|known| known == id) {
                    return Err(EngineError::ShellSelection(SelectError::UnknownBackend {
                        requested: id.clone(),
                        available: self.backend_ids().join(", "),
                    }));
                }
                if seen.contains(id) {
                    return Err(EngineError::Config(format!(
                        "shell.detect_order: `{id}` is listed twice"
                    )));
                }
                seen.push(id.clone());
            }
            shell.detect_order = order.clone();
        }
        if let Some(event_socket) = patch.event_socket {
            shell.hyprland.event_socket = event_socket;
        }
        if let Some(policy) = &patch.hyprpaper {
            if owe_core::shell::CoexistencePolicy::parse(policy).is_none() {
                return Err(EngineError::Shell(ShellError::Backend {
                    backend: "hyprland".to_string(),
                    detail: format!(
                        "`{policy}` is not a coexistence policy; expected `warn`, `stop` or \
                         `ignore`"
                    ),
                }));
            }
            shell.hyprland.hyprpaper = policy.trim().to_string();
        }

        let mut candidate = self.config.clone();
        candidate.shell = shell.clone();
        let validation_backend = candidate.shell.backend.clone();
        let validation_order = candidate.shell.detect_order.clone();
        candidate.shell.backend = "auto".to_string();
        candidate.shell.detect_order = vec!["hyprland".to_string()];
        candidate
            .validate()
            .map_err(|error| EngineError::Config(error.to_string()))?;
        if validation_backend.trim().is_empty() {
            return Err(EngineError::Config(
                "shell.backend: must not be empty".to_string(),
            ));
        }
        if validation_order.is_empty() {
            return Err(EngineError::Config(
                "shell.detect_order: must list at least one backend id".to_string(),
            ));
        }

        let selected = self.select_backend(&shell)?;
        let outputs = self.effective_backend_outputs(&selected)?;
        *self
            .shell_override
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(shell);
        self.publish_backend_selection(&selected, outputs.clone());
        self.reconcile_outputs(outputs);
        Ok(self.shell_status())
    }

    fn select_backend(
        &self,
        shell: &owe_core::config::ShellConfig,
    ) -> Result<Arc<dyn ShellBackend>, EngineError> {
        self.registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .select(shell, &env_lookup)
            .map_err(EngineError::ShellSelection)
    }

    fn effective_backend_outputs(
        &self,
        backend: &Arc<dyn ShellBackend>,
    ) -> Result<Vec<OutputInfo>, EngineError> {
        Ok(effective_outputs(backend.list_outputs()?))
    }

    fn publish_backend_selection(&self, backend: &Arc<dyn ShellBackend>, outputs: Vec<OutputInfo>) {
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(report) = started.as_mut() {
            report.backend = Some(backend.id().to_string());
            report.backend_error = None;
            report.selection_error = None;
            report.outputs = outputs;
        }
    }

    /// Inject the library resolver. Called once, after the library service opens.
    ///
    /// Safe to call before or after [`Engine::start`]: resolution only happens
    /// inside an apply or a restore.
    pub fn set_library(&self, resolver: Arc<dyn LibraryResolver>) {
        *self
            .library
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(resolver);
    }

    /// Whether the engine has been started.
    ///
    /// Contract accessor: used by the tests today, and by the P3 event bus, which
    /// needs to know whether startup already happened before subscribing.
    #[allow(
        dead_code,
        reason = "part of the engine's contract; first non-test caller is P3"
    )]
    pub fn is_started(&self) -> bool {
        self.started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Whether a manual pause is in effect.
    pub fn is_paused(&self) -> bool {
        *self
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Set or clear the manual pause override.
    pub fn set_paused(&self, paused: bool) {
        *self
            .paused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = paused;
        // P4 makes the override real for moving content: a held clock presents
        // nothing (static wallpapers were always motionless).
        self.playback.set_held(paused);
    }

    /// Start the engine: load session state, select the backend, list outputs,
    /// and bring up the presenter. Idempotent, and never fatal (see
    /// [`StartupReport`]).
    pub fn start(&self) -> StartupReport {
        if let Some(report) = self
            .started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return report;
        }

        // Session warnings were collected in `new()`; see the field docs.
        let mut warnings = self
            .session_warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        // The registry is read once and the selected backend used directly: looking
        // it up a second time by id would be a second chance to disagree with
        // itself if a backend were replaced in between.
        let shell_config = self.shell_config();
        let registry = self
            .registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let selected = registry.select(&shell_config, &env_lookup);

        let (backend, backend_error, selection_error) = match &selected {
            Ok(backend) => (Some(backend.id().to_string()), None, None),
            Err(error) => {
                warnings.push(format!("no shell backend is usable: {error}"));
                (None, Some(error.to_string()), Some(error.clone()))
            }
        };

        let outputs = match &selected {
            Ok(backend) => match backend.list_outputs() {
                Ok(outputs) => effective_outputs(outputs),
                Err(error) => {
                    // A missing `hyprctl` must not stop the daemon either.
                    warnings.push(format!("cannot list outputs: {error}"));
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        drop(registry);

        // Session entries for outputs that are not connected right now are kept on
        // purpose. PRD-F-08 requires that a monitor unplugged and replugged gets
        // its wallpaper back without restarting the daemon, and booting with a
        // laptop lid shut must not erase the external panel's entry (FR-LIB-5). A
        // stale entry is invisible — `output_views` only reports connected outputs
        // — and is overwritten the moment that connector appears again.

        let presentable = match Presenter::start_with_buffers(
            "owe",
            self.config.render.buffering.max_in_flight as usize,
        ) {
            Ok(presenter) => {
                let names = presenter.outputs().to_vec();
                *self
                    .presenter
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(presenter);
                names
            }
            Err(error) => {
                warnings.push(format!(
                    "wallpaper rendering is unavailable this session: {error}"
                ));
                Vec::new()
            }
        };

        let mut gpu_slot = self
            .gpu
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match HeadlessGpu::new() {
            Ok(Some(gpu)) => *gpu_slot = Some(gpu),
            Ok(None) => warnings.push("no GPU adapter available; nothing can be rendered".into()),
            Err(error) => warnings.push(format!("gpu initialisation failed: {error}")),
        }
        drop(gpu_slot);

        // One worker per output: the state machine is per-output by design.
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let connected: Vec<String> = outputs.iter().map(|output| output.name.clone()).collect();
        workers.retain(|name, _| connected.contains(name));
        for output in &outputs {
            workers
                .entry(output.name.clone())
                .or_insert_with(|| OutputWorker::new(output.name.clone()));
        }
        drop(workers);
        *self
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = outputs.clone();

        let report = StartupReport {
            backend,
            backend_error,
            selection_error,
            outputs,
            presenter: self
                .presenter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            presentable,
            warnings,
        };
        *self
            .started
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(report.clone());
        report
    }

    /// Outputs plus what OWE currently believes about each.
    pub fn output_views(&self) -> Result<Vec<OutputView>, EngineError> {
        let report = self.start();
        report.backend_failure()?;

        let outputs = self
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .into_iter()
            .filter(|output| output.active)
            .collect::<Vec<_>>();
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let session = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let applied = self
            .applied
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        Ok(outputs
            .into_iter()
            .map(|output| {
                let shown = applied.get(&output.name);
                let recorded = session.get(&output.name);
                let worker = workers.get(&output.name);
                let (state, error) = match worker.map(OutputWorker::state) {
                    Some(owe_core::WorkerState::Idle) | None => ("idle".to_string(), None),
                    Some(owe_core::WorkerState::Preparing { .. }) => {
                        ("preparing".to_string(), None)
                    }
                    Some(owe_core::WorkerState::Presented { .. }) => {
                        ("presented".to_string(), None)
                    }
                    Some(owe_core::WorkerState::Reconfigured { .. }) => {
                        ("reconfigured".to_string(), None)
                    }
                    Some(owe_core::WorkerState::Failed { reason }) => {
                        ("failed".to_string(), Some(reason.clone()))
                    }
                };
                OutputView {
                    name: output.name.clone(),
                    description: output.description.clone(),
                    width: output.width,
                    height: output.height,
                    focused: output.focused,
                    wallpaper: shown.map(|shown| shown.reference.clone()),
                    kind: shown.map(|shown| shown.kind.clone()),
                    state,
                    recorded: recorded
                        .filter(|entry| {
                            shown.is_none_or(|shown| shown.reference != entry.reference)
                        })
                        .map(|entry| entry.reference.clone()),
                    error,
                }
            })
            .collect())
    }

    /// Apply a wallpaper reference to the resolved outputs, using the transition
    /// each output's configuration asks for.
    ///
    /// Convenience wrapper over [`Engine::apply_with`]; the IPC layer always calls
    /// `apply_with` (a request may carry a transition), so this is the entry point
    /// for callers that have no request to honour.
    #[allow(
        dead_code,
        reason = "part of the engine's contract: the tests and the P3 event bus apply without a \
                  request"
    )]
    pub fn apply(&self, spec: &str, target: &OutputTarget) -> Result<Applied, EngineError> {
        self.apply_with(spec, target, None)
    }

    /// Apply a wallpaper with a caller-supplied transition request.
    ///
    /// Precedence (BACKEND-DESIGN §4): the request beats the output's own
    /// `[outputs.*]` section, which beats `render.default_transition`. The name
    /// must be one this build renders *and* one the configuration allows; anything
    /// else is refused with the allowed list in the message, because silently
    /// substituting a transition is how a user concludes the feature is broken.
    pub fn apply_with(
        &self,
        spec: &str,
        target: &OutputTarget,
        requested: Option<&Transition>,
    ) -> Result<Applied, EngineError> {
        self.apply_inner(spec, target, requested, false)
    }

    /// The one implementation, shared by live requests and session restore.
    fn apply_inner(
        &self,
        spec: &str,
        target: &OutputTarget,
        requested: Option<&Transition>,
        restored: bool,
    ) -> Result<Applied, EngineError> {
        // Validate the *request* before demanding a session: a bad reference, an
        // unrenderable kind, or a target that cannot match is a property of the
        // request, not of the environment. Reporting it correctly must not depend
        // on a compositor happening to be attached — the first CI runs failed two
        // tests on exactly that, because they only passed on a desktop.
        let reference = WallpaperRef::parse(spec)?;
        // Resolve the source before validating the kind. A `library:` reference
        // has no extension to inspect — its kind lives in the library row — so
        // demanding a resolved kind first made every library reference
        // unreachable, answered with the error that *describes the fix* instead
        // of performing it. (Found live in P3 on the reference session.)
        let (kind, path) = match reference.source() {
            WallpaperSource::Path(path) => (reference.resolved_kind()?, path.clone()),
            WallpaperSource::LibraryItem(id) => {
                let kind = self.resolve_library_kind(id)?;
                let path = self.resolve_library_reference(id)?;
                (kind, path)
            }
            WallpaperSource::ShaderPack(name) => {
                // Shader packs arrive in P5. This is an honest "not yet" answer
                // that names both the pack and the phase, not a decode error that
                // sends the user looking for a broken file.
                return Err(EngineError::Unsupported(format!(
                    "shader pack `{name}` cannot be rendered yet: WGSL packs land in P5 \
                     (docs/IMPLEMENTATION-PLAN.md)"
                )));
            }
        };

        // The transition *request* is validated on the same rule, and it has to
        // happen before `start()` for that rule to hold: asking for a transition
        // the config disallows is the client's mistake whether or not a compositor
        // is attached, and the CLI is how a user discovers the allow-list. This
        // sat below `start()` once and two handler tests passed on a desktop while
        // failing in CI, where there is no session to start.
        if let Some(requested) = requested {
            self.validate_transition(requested)?;
        }

        let report = self.start();
        report.backend_failure()?;

        // Resolve the target *before* decoding. Two reasons: a typo'd monitor
        // name should not cost a 4K decode, and "which output" is the mistake a
        // user can actually fix on the spot. (Found by a test that expected the
        // output error and got a file error instead.)
        let outputs = self
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let selected = owe_core::output::resolve(&outputs, target)?;

        let moving = matches!(kind, ContentKind::AnimatedImage | ContentKind::Video);
        let shell = self.shell_config();
        let shell_routed = report
            .backend
            .as_deref()
            .and_then(|id| self.backend(id))
            .is_some_and(|backend| backend.draw_mode(&shell) == DrawMode::ShellRouted);
        let decoded = if !moving && !shell_routed {
            Some(owe_media::decode_file(&path)?)
        } else {
            None
        };

        if let Some(routed) =
            self.route_to_shell(spec, kind, &path, &selected, requested, restored)?
        {
            self.stop_transitions_for_outputs(&selected);
            self.stop_playback_for_outputs(&selected);
            self.clear_presented_outputs(&selected);
            return Ok(routed);
        }

        let presentable = self.content_kinds();
        if !presentable.contains(&kind) {
            let rendered = presentable
                .iter()
                .map(|kind| kind.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(EngineError::Unsupported(format!(
                "`{}` wallpapers are not supported by this build yet: it renders {rendered} \
                 (docs/IMPLEMENTATION-PLAN.md records what is still open)",
                kind.as_str()
            )));
        }

        self.stop_transitions_for_outputs(&selected);
        if !moving {
            self.stop_playback_for_outputs(&selected);
        }

        if moving {
            return self.apply_playback(spec, kind, &path, &selected, restored);
        }

        let decoded = match decoded {
            Some(decoded) => decoded,
            None => owe_media::decode_file(&path)?,
        };

        let gpu_guard = self
            .gpu
            .lock()
            .map_err(|_| EngineError::NoGpu("poisoned".into()))?;
        let gpu = gpu_guard
            .as_ref()
            .ok_or_else(|| EngineError::NoGpu("no adapter was found at startup".to_string()))?;

        let mut applied = Applied {
            reference: spec.trim().to_string(),
            kind: kind.as_str().to_string(),
            outputs: Vec::new(),
            sizes: Vec::new(),
            transitions: Vec::new(),
            frames: Vec::new(),
            notes: Vec::new(),
            restored,
            theme: ThemeRuns::default(),
            routed: false,
        };

        for output in selected {
            let transition = self.transition_for(output, requested)?;
            let change = self.change_output(gpu, output, &decoded, &transition)?;
            match change {
                OutputChange::Superseded => {
                    // A newer apply took this output over mid-animation. It owns
                    // the wallpaper now, so this (older) apply must neither claim
                    // it nor write it to the session file — that is exactly how a
                    // session ends up recording a wallpaper that was never shown.
                    applied.notes.push(format!(
                        "{} was superseded by a newer change while its transition ran",
                        output.name
                    ));
                }
                OutputChange::Presented {
                    size,
                    frames,
                    kind,
                    note,
                } => {
                    if let Some(note) = note {
                        applied.notes.push(format!("{}: {note}", output.name));
                    }
                    applied.outputs.push(output.name.clone());
                    applied.sizes.push((output.name.clone(), size.0, size.1));
                    applied
                        .transitions
                        .push((output.name.clone(), kind.as_str().to_string()));
                    applied.frames.push((output.name.clone(), frames));
                }
            }
        }

        if applied.outputs.is_empty() {
            // Nothing reached a screen: report it instead of persisting a lie.
            // Reachable only when a concurrent change superseded every selected
            // output, which is a retryable condition — hence `Busy` at the IPC
            // layer rather than `Internal`.
            return Err(EngineError::Superseded(format!(
                "no output was changed: a newer change took over before this one reached the \
                 screen ({})",
                applied.notes.join("; ")
            )));
        }

        // Persist only what actually reached the screen.
        self.record_applied(&applied)?;

        Ok(applied)
    }

    /// How the configured fit mode maps onto the renderer's scaling modes
    /// (TRD FR-LIVE-4).
    ///
    /// The config validator already rejects an unknown value, so the fallback here
    /// is only reachable for a config built in code; it keeps the renderer's own
    /// default rather than inventing one.
    fn fit(&self) -> Scaling {
        Scaling::parse(&self.config.render.fit).unwrap_or_default()
    }

    /// Whether a resolved selection covers every drawable output.
    ///
    /// A shell that sets one wallpaper for the whole session answers exactly the
    /// same question as `-o all` when every output is selected, so the two are one
    /// request. A strict subset is not: it asks for per-output ownership, which is
    /// a different question and (for Caelestia) not an expressible one.
    fn selects_every_output(&self, selected: &[&OutputInfo]) -> bool {
        let active = self
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|output| output.active)
            .count();
        active > 0 && selected.len() == active
    }

    /// Apply an animated-image or video wallpaper by starting a frame clock on
    /// every selected output (FR-LIVE-5/6).
    ///
    /// `PlaybackRegistry::start` returns only after the first frame is presented,
    /// so an apply either lands on screen or fails with the decoder's own reason —
    /// the same "ok means shown" contract the still path keeps. There is no
    /// transition blend for moving content: the clock's first frame replaces what
    /// was there, and blending every subsequent tick would be a permanent
    /// crossfade, not a transition.
    fn apply_playback(
        &self,
        reference: &str,
        kind: ContentKind,
        path: &Path,
        selected: &[&OutputInfo],
        restored: bool,
    ) -> Result<Applied, EngineError> {
        let mut applied = Applied {
            reference: reference.trim().to_string(),
            kind: kind.as_str().to_string(),
            outputs: Vec::new(),
            sizes: Vec::new(),
            transitions: Vec::new(),
            frames: Vec::new(),
            notes: Vec::new(),
            restored,
            theme: ThemeRuns::default(),
            routed: false,
        };

        for output in selected {
            let section = output_config::resolve(&self.config.outputs, output);
            let cap = output_config::fps_cap_for(section.as_ref());
            // The clock renders at the output's own pixel size from its first frame:
            // the compositor's answer then governs, but starting from the size the
            // shell reported avoids a reconfigure round-trip on every apply.
            let previous_size = self
                .playback_sizes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(output.name.clone(), output.pixel_size());
            let snapshot =
                self.playback
                    .start(&output.name, path, kind, self.config.media.clone(), cap);
            match snapshot {
                Ok(snapshot) => {
                    applied.outputs.push(output.name.clone());
                    applied
                        .sizes
                        .push((output.name.clone(), snapshot.width, snapshot.height));
                    applied
                        .frames
                        .push((output.name.clone(), snapshot.frames_presented as u32));
                    let mode_note = format!(
                        "{}: playing ({} {}, {} fps cap, decode {})",
                        output.name,
                        snapshot.kind,
                        snapshot.mode,
                        snapshot
                            .fps_cap
                            .map(|fps| fps.to_string())
                            .unwrap_or_else(|| "no".to_string()),
                        snapshot.decode,
                    );
                    applied.notes.push(mode_note);
                }
                Err(error) => {
                    for started in &applied.outputs {
                        self.stop_playback_output(started);
                    }
                    if self.playback.snapshot(&output.name).is_none() {
                        self.stop_playback_output(&output.name);
                    } else if let Some(previous_size) = previous_size {
                        self.playback_sizes
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .insert(output.name.clone(), previous_size);
                    } else {
                        self.playback_sizes
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&output.name);
                    }
                    return Err(EngineError::Playback(error));
                }
            }
        }

        if let Err(error) = self.record_applied(&applied) {
            for output in &applied.outputs {
                self.stop_playback_output(output);
            }
            return Err(error);
        }
        Ok(applied)
    }

    fn stop_transitions_for_outputs(&self, selected: &[&OutputInfo]) {
        for output in selected {
            self.stop_transition(&output.name);
        }
    }

    fn stop_playback_for_outputs(&self, selected: &[&OutputInfo]) {
        for output in selected {
            self.stop_playback_output(&output.name);
        }
    }

    fn stop_playback_output(&self, output: &str) {
        self.playback.stop(output);
        self.playback_sizes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(output);
    }

    fn clear_presented_outputs(&self, selected: &[&OutputInfo]) {
        let Ok(presenter) = self.presenter.try_lock() else {
            return;
        };
        let Some(presenter) = presenter.as_ref() else {
            return;
        };
        for output in selected {
            if let Err(error) = presenter.clear(&output.name) {
                tracing::warn!(output = %output.name, %error, "could not clear the OWE surface");
            }
        }
    }

    fn cancel_playback_frame(&self, output: &str) {
        let Ok(presenter) = self.presenter.try_lock() else {
            return;
        };
        if let Some(presenter) = presenter.as_ref() {
            let _ = presenter.cancel_frame(output);
        }
    }

    /// Point the frame clocks at this engine.
    ///
    /// Called once, by `main`, as soon as the engine has an `Arc` of its own — the
    /// clocks outlive the request that starts them, so they need a handle rather
    /// than a borrow. A daemon that never calls this (every unit test) reports the
    /// missing attachment as the reason a clock frame could not be drawn, instead
    /// of blaming the GPU or the compositor.
    pub fn attach_playback(self: &Arc<Self>) {
        self.playback_sink.attach(self);
    }

    /// Present one frame of animated content on `output`.
    ///
    /// This is the clock's [`FrameSink`]: the engine's own GPU scales the frame
    /// into the output's surface size with the configured fit mode (FR-LIVE-4) and
    /// the engine's own presenter puts it on screen, so a moving wallpaper and a
    /// still take the same path. Returns the size the compositor accepted.
    fn show_frame(
        &self,
        output: &str,
        frame: &owe_media::DecodedFrame,
    ) -> Result<(u32, u32), EngineError> {
        let gpu_guard = self
            .gpu
            .lock()
            .map_err(|_| EngineError::NoGpu("poisoned".into()))?;
        let gpu = gpu_guard
            .as_ref()
            .ok_or_else(|| EngineError::NoGpu("no adapter was found at startup".to_string()))?;

        let source = (frame.width(), frame.height());
        // The size a clock starts at is the output's own, recorded when the clock
        // started; the compositor's answer replaces it. A frame whose size nobody
        // recorded falls back to the frame's own pixels, and the first reply from
        // the compositor corrects it — the same two-attempt shape `present_direct`
        // uses for stills, for the same reason.
        let mut size = self
            .playback_sizes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(output)
            .copied()
            .unwrap_or(source);

        for attempt in 0..2 {
            let pixels = image::render(
                gpu,
                size,
                source,
                frame.pixels(),
                self.fit(),
                PixelFormat::Bgra8,
                BACKGROUND,
            )?;
            let presented = self.present_frame(
                output,
                Frame {
                    width: size.0,
                    height: size.1,
                    pixels,
                },
            )?;
            match presented {
                PresentOutcome::Presented { size: shown } => {
                    self.playback_sizes
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(output.to_string(), shown);
                    return Ok(shown);
                }
                PresentOutcome::ResizeRequired { size: resized } => {
                    if attempt == 1 {
                        break;
                    }
                    size = resized;
                }
            }
        }

        Err(EngineError::Present(PresentError::ConfigureTimeout {
            output: output.to_string(),
            timeout: Duration::from_secs(2),
        }))
    }

    /// A playback command (`playback.cmd`) on one output.
    pub fn playback_command(
        &self,
        output: &str,
        cmd: PlaybackCmd,
    ) -> Result<PlaybackSnapshot, PlaybackError> {
        self.playback.command(output, cmd)
    }

    /// One output's playback state (`stats.get`).
    pub fn playback_snapshot(&self, output: &str) -> Option<PlaybackSnapshot> {
        self.playback.snapshot(output)
    }

    /// The outputs that currently have a frame clock, ordered.
    ///
    /// `playback.cmd` with no output named acts on these: a clock is the only
    /// thing a playback command can act on, and inventing a state for an output
    /// that shows a still would be reporting something nobody changed.
    pub fn playback_outputs(&self) -> Vec<String> {
        self.playback.outputs()
    }

    /// Stop every clock (shutdown, presenter loss).
    pub fn stop_playback(&self) {
        self.playback.stop_all();
        self.playback_sizes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Record what this run has on screen (fact), then persist it (intent for the
    /// next run, which restore reads at startup).
    ///
    /// Shared by the drawn and shell-routed paths on purpose: "which wallpapers
    /// come back after a restart" must not depend on which mode produced them.
    fn record_applied(&self, applied: &Applied) -> Result<(), EngineError> {
        let session_path = self.paths.state_dir.join("session.json");
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut next_session = session.clone();
        for name in &applied.outputs {
            next_session.set(name.clone(), applied.reference.clone(), &applied.kind);
        }
        next_session.save(&session_path)?;

        let mut shown = self
            .applied
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for name in &applied.outputs {
            shown.insert(
                name.clone(),
                AppliedRef {
                    reference: applied.reference.clone(),
                    kind: applied.kind.clone(),
                },
            );
        }
        *session = next_session;
        Ok(())
    }

    /// Route a change to a shell that owns the pixels (FR-SHELL-3, TRD §4).
    ///
    /// Returns `Ok(None)` when the selected backend draws its own surfaces — every
    /// backend except Caelestia in `shell-routed` mode, and Caelestia itself in
    /// `daemon-drawn` — so the caller carries on with the layer-shell path.
    ///
    /// A refusal from the shell is returned as an error and never downgraded into
    /// "draw it myself": the two modes exist because the user chose one, and
    /// quietly painting over a shell that declined would hide the failure behind a
    /// wallpaper that appears anyway.
    fn route_to_shell(
        &self,
        reference: &str,
        kind: ContentKind,
        path: &Path,
        selected: &[&OutputInfo],
        requested: Option<&Transition>,
        restored: bool,
    ) -> Result<Option<Applied>, EngineError> {
        let report = self.start();
        let Some(backend_id) = report.backend else {
            return Ok(None);
        };
        let Some(backend) = self.backend(&backend_id) else {
            return Err(EngineError::Shell(ShellError::Backend {
                backend: backend_id,
                detail: "the selected shell backend disappeared before the request could be routed"
                    .to_string(),
            }));
        };
        let shell = self.shell_config();
        if backend.draw_mode(&shell) != DrawMode::ShellRouted {
            return Ok(None);
        }

        // `None` = the whole session, which is what every output being selected
        // means. Anything narrower is a per-output request the shell must answer
        // for itself.
        let shell_output = if self.selects_every_output(selected) {
            None
        } else {
            selected.first().map(|output| output.name.as_str())
        };

        let outcome = backend.apply_wallpaper(&shell, shell_output, &path.display().to_string())?;
        let (detail, theme_refreshed) = match outcome {
            ApplyOutcome::Routed {
                detail,
                theme_refreshed,
            } => (detail, theme_refreshed),
            ApplyOutcome::NotApplicable => {
                return Err(EngineError::Shell(ShellError::Backend {
                    backend: backend.id().to_string(),
                    detail: "the backend claims shell-routed ownership but declined the \
                              wallpaper request; refusing to fall back to daemon playback"
                        .to_string(),
                }));
            }
        };

        let mut applied = Applied {
            reference: reference.trim().to_string(),
            kind: kind.as_str().to_string(),
            outputs: selected.iter().map(|output| output.name.clone()).collect(),
            // Nothing was decoded or presented here, so there is no surface size
            // and no frame count to report. Empty is the truthful answer.
            sizes: Vec::new(),
            transitions: Vec::new(),
            frames: Vec::new(),
            notes: vec![format!("the shell was asked to switch: {detail}")],
            restored,
            theme: ThemeRuns {
                by_owe: 0,
                by_shell: theme_refreshed,
            },
            routed: true,
        };

        // The double-theme guard, stated where it can be read (TRD §4): the shell
        // either runs its pipeline or does not, and OWE runs none either way.
        applied.notes.push(if theme_refreshed {
            "the shell ran its own theming pipeline (one theme update); OWE ran none".to_string()
        } else {
            "shell.caelestia.theme_hook = false: the change carried --no-smart, so the \
             shell's colour scheme is untouched"
                .to_string()
        });

        if let Some(transition) = requested {
            applied.notes.push(format!(
                "transition `{}` was not used: in shell-routed mode the shell performs the \
                 change itself",
                transition.name
            ));
        }

        self.record_applied(&applied)?;
        Ok(Some(applied))
    }

    /// Validate a transition request against the catalogue and the config.
    fn validate_transition(&self, transition: &Transition) -> Result<TransitionKind, EngineError> {
        let kind = TransitionKind::parse(&transition.name).ok_or_else(|| {
            let known: Vec<&str> = TransitionKind::ALL
                .iter()
                .map(|kind| kind.as_str())
                .collect();
            EngineError::Transition(format!(
                "`{}` is not a transition this build renders (it has: {})",
                transition.name,
                known.join(", ")
            ))
        })?;

        if !self
            .config
            .render
            .allow_transitions
            .iter()
            .any(|allowed| allowed == &transition.name)
        {
            return Err(EngineError::Transition(format!(
                "transition `{}` is not in render.allow_transitions ({})",
                transition.name,
                self.config.render.allow_transitions.join(", ")
            )));
        }

        if transition.duration_ms > MAX_TRANSITION_MS {
            return Err(EngineError::Transition(format!(
                "transition duration {} ms exceeds the {MAX_TRANSITION_MS} ms maximum",
                transition.duration_ms
            )));
        }
        if !FPS_RANGE.contains(&transition.fps) {
            return Err(EngineError::Transition(format!(
                "transition fps {} is out of range {}..={}",
                transition.fps,
                FPS_RANGE.start(),
                FPS_RANGE.end()
            )));
        }
        Ok(kind)
    }

    /// The transition to use for a change on one output.
    ///
    /// The request wins, then the output's own section, then the global default —
    /// and the losing `[outputs.*]` sections are reported so a user with two
    /// description sections that both match can see which one won.
    fn transition_for(
        &self,
        output: &OutputInfo,
        requested: Option<&Transition>,
    ) -> Result<ResolvedTransition, EngineError> {
        let section = output_config::resolve(&self.config.outputs, output);
        let chosen = match requested {
            Some(requested) => requested.clone(),
            None => output_config::transition_for(section.as_ref(), &self.config.render),
        };
        let kind = self.validate_transition(&chosen)?;
        Ok(ResolvedTransition {
            kind,
            schedule: Schedule::new(chosen.duration_ms, chosen.fps),
        })
    }

    /// Indexed content kind of a `library:<id>` reference.
    ///
    /// The sibling of [`Self::resolve_library_reference`]: the kind cannot be
    /// derived from the reference itself, so it comes from the same row.
    fn resolve_library_kind(&self, id: &str) -> Result<ContentKind, EngineError> {
        let resolver = self
            .library
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| {
                EngineError::Library(format!(
                    "`library:{id}` cannot be resolved: this daemon has no library index open"
                ))
            })?;
        resolver.kind_of(id).map_err(EngineError::Library)
    }

    /// Resolve a `library:<id>` reference through the injected resolver.
    fn resolve_library_reference(&self, id: &str) -> Result<PathBuf, EngineError> {
        let resolver = self
            .library
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| {
                EngineError::Library(format!(
                    "`library:{id}` cannot be resolved: this daemon has no library index open"
                ))
            })?;
        resolver.resolve(id).map_err(EngineError::Library)
    }

    /// Decoded pixels of whatever `output` is currently showing, if any.
    ///
    /// Re-decoded on demand rather than kept resident: a transition needs the old
    /// image for a few hundred milliseconds, and holding a decoded 4K frame per
    /// output forever would cost more RAM than the whole point of this project.
    /// A file that has since been moved or deleted simply means "no transition",
    /// with a note — never a failed change.
    fn previous_pixels(&self, output: &str) -> Result<Option<DecodedImage>, String> {
        let reference = self
            .applied
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(output)
            .map(|entry| entry.reference.clone());
        let Some(reference) = reference else {
            return Ok(None);
        };
        let parsed = WallpaperRef::parse(&reference).map_err(|error| error.to_string())?;
        let path = match parsed.source() {
            WallpaperSource::Path(path) => path.clone(),
            WallpaperSource::LibraryItem(id) => self
                .resolve_library_reference(id)
                .map_err(|e| e.to_string())?,
            WallpaperSource::ShaderPack(_) => return Ok(None),
        };
        owe_media::decode_file(&path)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    /// Change what one output shows, running the configured transition.
    ///
    /// The worker state machine still decides *when* work happens; this function
    /// decides *what* happens: an instant swap, or an animation that can be taken
    /// over by a newer change without a snap (FR-LIB-3).
    fn change_output(
        &self,
        gpu: &HeadlessGpu,
        output: &OutputInfo,
        decoded: &DecodedImage,
        transition: &ResolvedTransition,
    ) -> Result<OutputChange, EngineError> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst);
        let name = output.name.as_str();
        let mut worker = {
            let mut workers = self
                .workers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            workers
                .entry(name.to_string())
                .or_insert_with(|| OutputWorker::new(name))
                .clone()
        };

        let requested_size = output.pixel_size();
        let action = worker.on(WorkerEvent::Apply {
            generation,
            size: requested_size,
        });
        let WorkerAction::Render { size, .. } = action else {
            // Nothing to do: this generation is already on screen.
            return Ok(OutputChange::Presented {
                size: requested_size,
                frames: 0,
                kind: TransitionKind::None,
                note: None,
            });
        };

        // A transition needs something to transition *from*.
        let mut note = None;
        let mut from = if transition.kind.is_animated() {
            match self.previous_pixels(name) {
                Ok(pixels) => pixels,
                Err(reason) => {
                    note = Some(format!(
                        "no transition: the previous wallpaper could not be re-read ({reason})"
                    ));
                    None
                }
            }
        } else {
            None
        };

        if transition.kind.is_animated() && from.is_none() && note.is_none() {
            note = Some(
                "no transition: this output had no previous wallpaper to blend from".to_string(),
            );
        }

        // Animation is only attempted when there is a source image *and* a
        // surface whose size we already know; every other case takes the direct
        // path, which is also the fallback when a retarget cannot be done.
        let animated = from.is_some() && transition.kind.is_animated();

        if animated {
            let from_pixels = from.take().expect("checked above");
            let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
            let animation = Animation {
                output: name,
                size,
                from: &from_pixels,
                to: decoded,
                transition,
                epoch,
            };
            match self.animate(gpu, animation) {
                Ok(AnimatedOutcome::Completed { size, frames }) => {
                    worker.on(WorkerEvent::Presented { generation, size });
                    self.store_worker(name, worker);
                    self.finish_transition(name, epoch);
                    return Ok(OutputChange::Presented {
                        size,
                        frames,
                        kind: transition.kind,
                        note,
                    });
                }
                Ok(AnimatedOutcome::Superseded) => {
                    // Do not touch the worker or the session: the newer change owns
                    // this output and will say what it is showing.
                    return Ok(OutputChange::Superseded);
                }
                Ok(AnimatedOutcome::ResizeRequired { size: resized }) => {
                    // The compositor reconfigured mid-animation. Cutting the
                    // animation short and drawing the final image is the honest
                    // answer: the user gets the wallpaper they asked for, and the
                    // note says the animation was interrupted.
                    note = Some(format!(
                        "transition was cut short: the compositor resized the surface to \
                         {}x{}",
                        resized.0, resized.1
                    ));
                    let (presented, frames) =
                        self.present_direct(gpu, name, &worker, decoded, resized, generation)?;
                    return Ok(OutputChange::Presented {
                        size: presented,
                        frames,
                        kind: transition.kind,
                        note,
                    });
                }
                Err(error) => return Err(error),
            }
        }

        let (presented, frames) =
            self.present_direct(gpu, name, &worker, decoded, size, generation)?;
        Ok(OutputChange::Presented {
            size: presented,
            frames,
            kind: transition.kind,
            note,
        })
    }

    /// Store a worker back into the map (a poisoned map is not worth failing the
    /// wallpaper change over; the state machine will simply start fresh).
    fn store_worker(&self, output: &str, worker: OutputWorker) {
        self.workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(output.to_string(), worker);
    }

    /// Forget the transition entry for an output *if it is still ours*.
    fn finish_transition(&self, output: &str, epoch: u64) {
        let mut map = self
            .transitions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if map.get(output).is_some_and(|entry| entry.epoch == epoch) {
            map.remove(output);
        }
    }

    /// Render the new image at `size` and present it, retrying once if the
    /// compositor asks for a different surface size.
    fn present_direct(
        &self,
        gpu: &HeadlessGpu,
        output: &str,
        worker: &OutputWorker,
        decoded: &DecodedImage,
        size: (u32, u32),
        generation: u64,
    ) -> Result<((u32, u32), u32), EngineError> {
        let mut worker = worker.clone();

        // A direct change takes the output over: any animation still running on it
        // must stop rather than fight for the screen.
        self.stop_transition(output);

        let mut current_size = size;
        // At most two attempts: one for the size we assumed, one for the size the
        // compositor actually configured. A third would mean the compositor keeps
        // changing its mind and we would be spinning.
        for attempt in 0..2 {
            let pixels = image::render(
                gpu,
                current_size,
                decoded.size(),
                decoded.pixels(),
                self.fit(),
                PixelFormat::Bgra8,
                BACKGROUND,
            )?;

            let prepared = worker.on(WorkerEvent::Prepared { generation });
            debug_assert!(matches!(prepared, WorkerAction::Present { .. }));

            let frame = Frame {
                width: current_size.0,
                height: current_size.1,
                pixels,
            };

            match self.present(output, frame)? {
                PresentOutcome::Presented { size } => {
                    worker.on(WorkerEvent::Presented { generation, size });
                    self.store_worker(output, worker);
                    return Ok((size, 1));
                }
                PresentOutcome::ResizeRequired { size } => {
                    let reconfigure = worker.on(WorkerEvent::Configure { size });
                    debug_assert!(matches!(reconfigure, WorkerAction::Render { .. }));
                    current_size = size;
                    if attempt == 1 {
                        break;
                    }
                }
            }
        }

        let failure = format!(
            "compositor kept changing the surface size for {output}; gave up after 2 attempts"
        );
        worker.on(WorkerEvent::Failed {
            generation,
            reason: failure.clone(),
        });
        self.store_worker(output, worker);
        Err(EngineError::Present(PresentError::ConfigureTimeout {
            output: output.to_string(),
            timeout: Duration::from_secs(2),
        }))
    }

    /// Hand one frame to the presenter, resolving the presenter lock once.
    fn present(&self, output: &str, frame: Frame) -> Result<PresentOutcome, EngineError> {
        let presenter = self
            .presenter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let presenter = presenter.as_ref().ok_or_else(|| {
            EngineError::NoGpu("no Wayland session: cannot present wallpapers".to_string())
        })?;
        presenter.present(output, frame).map_err(EngineError::from)
    }

    fn present_frame(&self, output: &str, frame: Frame) -> Result<PresentOutcome, EngineError> {
        let presenter = self
            .presenter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let presenter = presenter.as_ref().ok_or_else(|| {
            EngineError::NoGpu("no Wayland session: cannot present wallpapers".to_string())
        })?;
        presenter
            .present_frame(output, frame)
            .map_err(EngineError::from)
    }

    /// Stop any animation on `output`. The caller is about to take the output over.
    ///
    /// Waiting for the renderer lock is the point: a running loop presents its
    /// frame *while holding that lock*, so acquiring it proves the stale frame is
    /// already on screen and everything drawn afterwards wins.
    fn stop_transition(&self, output: &str) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let in_flight = self
            .transitions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(output);
        if let Some(entry) = in_flight {
            // Held until the end of the scope on purpose: this waits for the
            // in-flight frame to finish presenting before the caller draws.
            let _drained = entry.renderer.lock();
        }
    }

    /// Run a transition to completion on one output, frame by frame.
    ///
    /// Interruption: the *newer* apply retargets the shared renderer (baking what
    /// is on screen, so there is no snap) and bumps the epoch; this loop then
    /// notices at its next frame and returns [`AnimatedOutcome::Superseded`]
    /// without presenting again.
    fn animate(
        &self,
        gpu: &HeadlessGpu,
        animation: Animation<'_>,
    ) -> Result<AnimatedOutcome, EngineError> {
        let Animation {
            output,
            size,
            from,
            to,
            transition,
            epoch,
        } = animation;
        let kind = transition.kind;
        let schedule = transition.schedule;

        // Take over the previous animation's renderer when there is one and the
        // surface size has not changed: that is what makes interruption seamless.
        let existing = {
            let map = self
                .transitions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map.get(output)
                .map(|entry| (Arc::clone(&entry.renderer), Arc::clone(&entry.progress)))
        };

        let (renderer, progress) = match existing {
            Some((renderer, progress)) => {
                let at = f32::from_bits(progress.load(Ordering::SeqCst));
                let mut guard = renderer
                    .lock()
                    .map_err(|_| EngineError::NoGpu("transition lock poisoned".to_string()))?;
                if guard.target() == size {
                    guard.retarget(gpu, at, (to.pixels(), to.size()), kind)?;
                    drop(guard);
                    progress.store(0.0_f32.to_bits(), Ordering::SeqCst);
                    (renderer, progress)
                } else {
                    drop(guard);
                    self.stop_transition(output);
                    self.build_transition(gpu, size, from, to, kind)?
                }
            }
            None => self.build_transition(gpu, size, from, to, kind)?,
        };

        self.transitions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                output.to_string(),
                InFlight {
                    epoch,
                    renderer: Arc::clone(&renderer),
                    progress: Arc::clone(&progress),
                },
            );

        let started = Instant::now();
        let mut frames = 0_u32;
        for frame in 0..schedule.frames() {
            // Pace against a deadline so a slow frame does not stretch the rest of
            // the ramp: progress stays a function of the frame index, which is what
            // the golden images freeze.
            let deadline = started + schedule.frame_interval() * frame;
            let now = Instant::now();
            if deadline > now {
                std::thread::sleep(deadline - now);
            }

            let mut guard = match renderer.lock() {
                Ok(guard) => guard,
                Err(_) => return Err(EngineError::NoGpu("transition lock poisoned".to_string())),
            };

            // Held across render *and* present: a retarget can then never land
            // between "computed the blend" and "showed it".
            if self.epoch.load(Ordering::SeqCst) != epoch {
                drop(guard);
                return Ok(AnimatedOutcome::Superseded);
            }

            let progress_now = schedule.progress(frame);
            let pixels = guard.frame(gpu, progress_now)?;
            let frame_to_show = Frame {
                width: size.0,
                height: size.1,
                pixels,
            };
            match self.present(output, frame_to_show)? {
                PresentOutcome::Presented { .. } => {
                    progress.store(progress_now.to_bits(), Ordering::SeqCst);
                    frames += 1;
                }
                PresentOutcome::ResizeRequired { size: resized } => {
                    progress.store(progress_now.to_bits(), Ordering::SeqCst);
                    drop(guard);
                    return Ok(AnimatedOutcome::ResizeRequired { size: resized });
                }
            }
            drop(guard);
        }

        Ok(AnimatedOutcome::Completed { size, frames })
    }

    /// Build the renderer for a fresh animation.
    fn build_transition(
        &self,
        gpu: &HeadlessGpu,
        size: (u32, u32),
        from: &DecodedImage,
        to: &DecodedImage,
        kind: TransitionKind,
    ) -> Result<(Arc<Mutex<TransitionRenderer>>, Arc<AtomicU32>), EngineError> {
        let renderer = TransitionRenderer::new(
            gpu,
            size,
            (from.pixels(), from.size()),
            (to.pixels(), to.size()),
            kind,
            self.fit(),
            PixelFormat::Bgra8,
        )?;
        Ok((
            Arc::new(Mutex::new(renderer)),
            Arc::new(AtomicU32::new(0.0_f32.to_bits())),
        ))
    }

    /// Remove OWE's wallpaper from the resolved outputs.
    pub fn clear(&self, target: &OutputTarget) -> Result<Vec<String>, EngineError> {
        let report = self.start();
        report.backend_failure()?;
        let shell = self.shell_config();
        let routed = report
            .backend
            .as_deref()
            .and_then(|id| self.backend(id))
            .filter(|backend| backend.draw_mode(&shell) == DrawMode::ShellRouted);
        let outputs = self
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let selected = owe_core::output::resolve(&outputs, target)?;
        let shell_output = if self.selects_every_output(&selected) {
            None
        } else {
            selected.first().map(|output| output.name.as_str())
        };

        if let Some(backend) = routed {
            let outcome = backend.clear_wallpaper(&shell, shell_output)?;
            if !matches!(outcome, ApplyOutcome::Routed { .. }) {
                return Err(EngineError::Shell(ShellError::Backend {
                    backend: backend.id().to_string(),
                    detail: "the backend claims shell-routed ownership but declined the \
                              clear request; refusing to fall back to a daemon surface"
                        .to_string(),
                }));
            }
        }

        self.stop_transitions_for_outputs(&selected);
        self.stop_playback_for_outputs(&selected);

        let presenter = self
            .presenter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(presenter) = presenter.as_ref() {
            for output in &selected {
                presenter.clear(&output.name)?;
            }
        }
        drop(presenter);

        let session_path = self.paths.state_dir.join("session.json");
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut next_session = session.clone();
        for output in &selected {
            next_session.remove(&output.name);
        }
        next_session.save(&session_path)?;
        *session = next_session;
        drop(session);

        {
            let mut workers = self
                .workers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for output in &selected {
                if let Some(worker) = workers.get_mut(&output.name) {
                    worker.clear();
                }
            }
        }
        let mut applied = self
            .applied
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for output in &selected {
            applied.remove(&output.name);
        }

        Ok(selected
            .into_iter()
            .map(|output| output.name.clone())
            .collect())
    }

    /// What OWE intends to show on one output, and where that intent comes from.
    ///
    /// Precedence: the **session file** (the last thing the user actually applied)
    /// beats an `[outputs.*]` section, because it is more recent and more
    /// specific. Nothing configured anywhere means `None` — and OWE leaves that
    /// output alone rather than inventing a wallpaper for it.
    pub fn planned_wallpaper(&self, output: &OutputInfo) -> Option<PlannedWallpaper> {
        let session = self
            .session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = session.get(&output.name) {
            return Some(PlannedWallpaper {
                reference: entry.reference.clone(),
                source: PlanSource::Session,
            });
        }
        drop(session);

        let resolved = output_config::resolve(&self.config.outputs, output)?;
        resolved.wallpaper().map(|reference| PlannedWallpaper {
            reference: reference.to_string(),
            source: PlanSource::OutputSection(resolved.section.clone()),
        })
    }

    /// Bring the output list up to date with what the backend reports now.
    ///
    /// Returns the description of the transition that just happened (added and
    /// removed outputs, re-applied wallpapers, released surfaces) so the daemon can
    /// log one honest line per hotplug instead of guessing.
    pub fn reconcile_outputs(&self, current: Vec<OutputInfo>) -> ReconcileReport {
        let current = effective_outputs(current);
        let previous = self
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let events: Vec<HotplugEvent> = owe_core::supervisor::diff(&previous, &current);

        let mut report = ReconcileReport {
            added: events
                .iter()
                .filter(|event| matches!(event, HotplugEvent::Added { .. }))
                .map(|event| event.output().to_string())
                .collect(),
            removed: events
                .iter()
                .filter(|event| matches!(event, HotplugEvent::Removed { .. }))
                .map(|event| event.output().to_string())
                .collect(),
            ..ReconcileReport::default()
        };

        // Worker lifetime is driven by the connected set. Session entries are
        // deliberately *not* pruned against it: an unplugged output keeps its
        // record so a replug restores the same wallpaper (PRD-F-08), which is what
        // the supervisor's `Teardown` does with its own bookkeeping too.
        let connected: Vec<String> = current.iter().map(|output| output.name.clone()).collect();

        // Workers: one per live output, none for the dead ones.
        {
            let mut workers = self
                .workers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            workers.retain(|name, _| connected.contains(name));
            for output in &current {
                workers
                    .entry(output.name.clone())
                    .or_insert_with(|| OutputWorker::new(output.name.clone()));
            }
        }

        let now = Instant::now();
        let actions = {
            let mut supervisor = self
                .supervisor
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let actions = supervisor.observe_snapshot(&previous, &current, now);
            report.tracked = supervisor.present();
            actions
        };

        *self
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = current.clone();

        for action in actions {
            let name = action.output().to_string();
            match action {
                SupervisorAction::Spawn { .. } | SupervisorAction::Reapply { .. } => {
                    let Some(output) = current.iter().find(|output| output.name == name) else {
                        continue;
                    };
                    let Some(planned) = self.planned_wallpaper(output) else {
                        // Nothing configured anywhere: leave the output alone and
                        // say so, rather than inventing a wallpaper for it.
                        continue;
                    };
                    match self.apply_planned(&planned, output) {
                        Ok(reference) => report.applied.push((name, reference)),
                        Err(error) => report.failures.push((name, error.to_string())),
                    }
                }
                SupervisorAction::Resize { size, .. } => {
                    tracing::debug!(output = %name, width = size.0, height = size.1, "output resized");
                    let Some(output) = current.iter().find(|output| output.name == name) else {
                        continue;
                    };
                    let Some(planned) = self.planned_wallpaper(output) else {
                        continue;
                    };
                    // A mode change needs a re-render at the new size; there is
                    // no transition here because nothing about the *content*
                    // changed (a fade on a resolution change would look like a
                    // flicker).
                    match self.apply_planned(&planned, output) {
                        Ok(reference) => report.applied.push((name, reference)),
                        Err(error) => report.failures.push((name, error.to_string())),
                    }
                }
                SupervisorAction::Teardown { .. } => {
                    self.stop_transition(&name);
                    self.stop_playback_output(&name);
                    {
                        let presenter = self
                            .presenter
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if let Some(presenter) = presenter.as_ref()
                            && let Err(error) = presenter.clear(&name)
                        {
                            report.failures.push((name.clone(), error.to_string()));
                        }
                    }
                    self.workers
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&name);
                    self.applied
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&name);
                    report.cleared.push(name);
                }
                SupervisorAction::GiveUp { reason, .. } => {
                    // Out of restarts: report it and leave the daemon running. The
                    // output keeps whatever it last showed, and other outputs are
                    // untouched (NFR-REL-1).
                    report.failures.push((name, reason));
                }
            }
        }

        // Re-publish the caller's snapshot. The pre-loop assignment above is what
        // lets an apply *resolve* an output that only just appeared; this one is
        // what keeps the snapshot from being replaced underneath us. An apply can
        // lazily run `start()` (the very first apply after boot), and `start()`
        // publishes the backend's own output list — so without this, a reconcile
        // would leave `self.outputs` describing whatever `hyprctl` said at that
        // instant instead of the list the caller just gave us, and the *next*
        // reconcile would diff against the wrong baseline (found by the hotplug
        // cycle test: an unplugged output went unnoticed).
        *self
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = current;

        report
    }

    /// Apply a planned wallpaper to exactly one output.
    fn apply_planned(
        &self,
        planned: &PlannedWallpaper,
        output: &OutputInfo,
    ) -> Result<String, EngineError> {
        let applied = self.apply_with(
            &planned.reference,
            &OutputTarget::Named(output.name.clone()),
            None,
        )?;
        Ok(applied.reference)
    }

    /// Re-list the outputs from the backend and reconcile against them.
    ///
    /// A backend that cannot list outputs returns the previous list: a broken
    /// `hyprctl` must not make OWE conclude that every monitor was unplugged, which
    /// would clear the desktop.
    pub fn refresh_outputs(&self) -> Result<(Vec<OutputInfo>, ReconcileReport), EngineError> {
        let report = self.start();
        report.backend_failure()?;
        let backend = report.backend.as_deref().and_then(|id| self.backend(id));
        let listed = match backend.as_ref().map(|backend| backend.list_outputs()) {
            Some(Ok(outputs)) => effective_outputs(outputs),
            Some(Err(error)) => {
                tracing::warn!(%error, "cannot list outputs; keeping the previous list");
                self.outputs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            }
            None => Vec::new(),
        };
        let reconcile = self.reconcile_outputs(listed.clone());
        Ok((listed, reconcile))
    }

    pub fn handle_presenter_event(
        &self,
        event: &PresenterEvent,
    ) -> Result<ReconcileReport, EngineError> {
        match event {
            PresenterEvent::Configured { output, size } => {
                if size.0 == 0 || size.1 == 0 {
                    return Ok(ReconcileReport::default());
                }
                let mut listed = self
                    .outputs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                if !listed.iter().any(|candidate| candidate.name == *output) {
                    let report = self.start();
                    report.backend_failure()?;
                    let backend = report
                        .backend
                        .as_deref()
                        .and_then(|id| self.backend(id))
                        .ok_or_else(|| {
                            EngineError::Shell(ShellError::Backend {
                                backend: "unknown".to_string(),
                                detail: "the presenter has an output the shell backend does not"
                                    .to_string(),
                            })
                        })?;
                    listed = effective_outputs(backend.list_outputs()?);
                }
                let Some(current) = listed
                    .iter_mut()
                    .find(|candidate| candidate.name == *output)
                else {
                    return Ok(ReconcileReport::default());
                };
                current.width = size.0;
                current.height = size.1;
                current.active = true;
                Ok(self.reconcile_outputs(listed))
            }
            PresenterEvent::Closed { output } => {
                let has_applied = self
                    .applied
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains_key(output);
                if !has_applied {
                    return Ok(ReconcileReport::default());
                }
                let listed = self
                    .outputs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                let Some(current) = listed.iter().find(|candidate| candidate.name == *output)
                else {
                    return Ok(ReconcileReport::default());
                };
                let Some(planned) = self.planned_wallpaper(current) else {
                    return Ok(ReconcileReport::default());
                };
                let mut report = ReconcileReport::default();
                match self.apply_planned(&planned, current) {
                    Ok(reference) => report.applied.push((output.clone(), reference)),
                    Err(error) => report.failures.push((output.clone(), error.to_string())),
                }
                Ok(report)
            }
            PresenterEvent::OutputAdded { .. } | PresenterEvent::OutputRemoved { .. } => {
                self.refresh_outputs().map(|(_, report)| report)
            }
        }
    }

    /// Content kinds this build can actually render on this machine.
    ///
    /// Static images go through the still path; animated images and video go through
    /// the frame clock, which paces their real timing and presents each frame
    /// through the same presenter a still uses (FR-LIVE-5/6). Two of the three are
    /// conditional on the *machine*, not on the build: video is advertised only when
    /// a video runtime is on `PATH`, because a kind advertised here is a kind
    /// `wallpaper.set` accepts — and an accepted kind that cannot be decoded is the
    /// promise this list exists to prevent. Shaders wait for P5.
    ///
    /// The machine's *decode* capability is reported separately
    /// (`owe_media::media_backends()`), so the two lists answer two questions
    /// instead of one list being quietly wrong.
    pub fn content_kinds(&self) -> Vec<ContentKind> {
        let mut kinds = vec![ContentKind::StaticImage, ContentKind::AnimatedImage];
        if owe_media::any_video_runtime() {
            kinds.push(ContentKind::Video);
        }
        kinds
    }

    /// Hand the presenter's event stream to the hotplug listener (FR-LIB-4).
    ///
    /// `Some` exactly once per run: there is a single receiver, because two
    /// consumers would split the event stream and lose events.
    pub fn take_presenter_events(
        &self,
    ) -> Option<std::sync::mpsc::Receiver<owe_render::surface::PresenterEvent>> {
        self.presenter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
            .and_then(Presenter::take_events)
    }

    /// Shut the presenter down (used at exit, and by the hotplug driver's `Drop`
    /// so its blocking listener thread is released).
    pub fn stop_presenter(&self) {
        self.stop_playback();
        let outputs = self
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let transition_outputs = self
            .transitions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for output in outputs
            .into_iter()
            .map(|output| output.name)
            .chain(transition_outputs)
        {
            self.stop_transition(&output);
        }
        let presenter = self
            .presenter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(presenter);
    }

    /// Re-apply the recorded wallpaper for every output at daemon start (FR-LIB-5).
    ///
    /// Runs after the presenter is up. Nothing here is fatal: a monitor whose file
    /// was deleted is reported and skipped, because refusing to start over one
    /// missing picture would leave the user with a daemon that does nothing.
    pub fn restore(&self) -> RestoreReport {
        let report = self.start();
        let mut restored = RestoreReport::default();

        if report.backend.is_none() {
            restored.skipped.push((
                "*".to_string(),
                report
                    .backend_error
                    .clone()
                    .unwrap_or_else(|| "no shell backend is available".to_string()),
            ));
            return restored;
        }
        if !report.presenter {
            restored.skipped.push((
                "*".to_string(),
                "no Wayland session: nothing can be drawn, so nothing was restored".to_string(),
            ));
            return restored;
        }

        for output in &report.outputs {
            let Some(planned) = self.planned_wallpaper(output) else {
                restored.skipped.push((
                    output.name.clone(),
                    "no recorded wallpaper and no [outputs.*] section".to_string(),
                ));
                continue;
            };
            match self.apply_inner(
                &planned.reference,
                &OutputTarget::Named(output.name.clone()),
                None,
                true,
            ) {
                Ok(applied) => restored
                    .restored
                    .push((output.name.clone(), applied.reference)),
                Err(error) => restored
                    .failures
                    .push((output.name.clone(), error.to_string())),
            }
        }

        restored
    }

    /// Backend ids this build can actually use (registered, not merely known).
    pub fn backend_ids(&self) -> Vec<String> {
        self.registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ids()
            .iter()
            .map(|id| (*id).to_string())
            .collect()
    }
}

fn effective_outputs(outputs: Vec<OutputInfo>) -> Vec<OutputInfo> {
    outputs.into_iter().filter(|output| output.active).collect()
}

/// Environment lookup for detection, matching `owe_core`'s testable pattern.
fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn engine() -> Engine {
        let paths = XdgPaths::resolve().expect("xdg paths");
        Engine::new(Config::default(), paths)
    }

    /// An engine whose state lives in a throwaway directory, so a test can write
    /// session state and read it back without touching the developer's real one.
    fn engine_in(dir: &Path, config: Config) -> Engine {
        let paths = XdgPaths {
            config_file: dir.join("config.toml"),
            state_dir: dir.join("state"),
            cache_dir: dir.join("cache"),
            data_dir: dir.join("data"),
            runtime_dir: Some(dir.join("run")),
        };
        std::fs::create_dir_all(&paths.state_dir).expect("state dir");
        Engine::new(config, paths)
    }

    /// A synthetic output.
    fn output(name: &str, description: &str) -> OutputInfo {
        OutputInfo {
            name: name.to_string(),
            description: description.to_string(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            focused: true,
            active: true,
        }
    }

    fn transition(name: &str, duration_ms: u64, fps: u32) -> Transition {
        Transition {
            name: name.to_string(),
            duration_ms,
            fps,
        }
    }

    #[test]
    fn planned_wallpaper_prefers_the_recorded_session_over_a_config_section() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.outputs.insert(
            "eDP-1".to_string(),
            owe_core::config::OutputConfig {
                wallpaper: Some("/configured.png".to_string()),
                ..owe_core::config::OutputConfig::default()
            },
        );
        let engine = engine_in(dir.path(), config);
        engine
            .session
            .lock()
            .unwrap()
            .set("eDP-1", "/applied-last.png", "static-image");

        let planned = engine
            .planned_wallpaper(&output("eDP-1", "Samsung"))
            .expect("something is planned");
        assert_eq!(
            planned.reference, "/applied-last.png",
            "the session file is the most recent user intent, so it wins"
        );
        assert_eq!(planned.source, PlanSource::Session);
    }

    #[test]
    fn planned_wallpaper_falls_back_to_the_matching_output_section() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.outputs.insert(
            "U2720Q".to_string(),
            owe_core::config::OutputConfig {
                wallpaper: Some("library:7".to_string()),
                ..owe_core::config::OutputConfig::default()
            },
        );
        config.outputs.insert(
            "any".to_string(),
            owe_core::config::OutputConfig {
                wallpaper: Some("/fallback.png".to_string()),
                ..owe_core::config::OutputConfig::default()
            },
        );
        let engine = engine_in(dir.path(), config);

        let by_description = engine
            .planned_wallpaper(&output("HDMI-A-1", "Dell Inc. DELL U2720Q"))
            .expect("description match");
        assert_eq!(by_description.reference, "library:7");
        assert_eq!(
            by_description.source,
            PlanSource::OutputSection("U2720Q".to_string())
        );

        let by_catch_all = engine
            .planned_wallpaper(&output("DP-1", "Some Panel"))
            .expect("any match");
        assert_eq!(by_catch_all.reference, "/fallback.png");
    }

    #[test]
    fn planned_wallpaper_is_none_when_nothing_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), Config::default());
        assert!(
            engine
                .planned_wallpaper(&output("eDP-1", "Samsung"))
                .is_none(),
            "an output nobody configured must be left alone, not given a surprise wallpaper"
        );
    }

    #[test]
    fn planned_wallpaper_ignores_sections_for_absent_outputs() {
        // Regression shape: a config written for a dock that is not plugged in must
        // not leak onto the internal panel.
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.outputs.insert(
            "DP-9".to_string(),
            owe_core::config::OutputConfig {
                wallpaper: Some("/dock.png".to_string()),
                ..owe_core::config::OutputConfig::default()
            },
        );
        let engine = engine_in(dir.path(), config);
        assert!(
            engine
                .planned_wallpaper(&output("eDP-1", "Samsung Display Corp"))
                .is_none()
        );
    }

    #[test]
    fn transition_resolution_follows_request_then_section_then_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.render.default_transition = transition("fade", 250, 60);
        config.outputs.insert(
            "eDP-1".to_string(),
            owe_core::config::OutputConfig {
                transition: Some(transition("slide", 450, 30)),
                ..owe_core::config::OutputConfig::default()
            },
        );
        let engine = engine_in(dir.path(), config);
        let monitor = output("eDP-1", "Samsung");

        // 1. The request wins.
        let requested = transition("wipe", 100, 30);
        let resolved = engine
            .transition_for(&monitor, Some(&requested))
            .expect("request");
        assert_eq!(resolved.kind, TransitionKind::Wipe);
        assert_eq!(resolved.schedule.frames(), 3, "100 ms at 30 fps");

        // 2. Otherwise the output's own section.
        let resolved = engine.transition_for(&monitor, None).expect("section");
        assert_eq!(resolved.kind, TransitionKind::Slide);
        assert_eq!(resolved.schedule.frames(), 14, "450 ms at 30 fps");

        // 3. Otherwise the global default.
        let other = engine
            .transition_for(&output("DP-1", "Other"), None)
            .expect("default");
        assert_eq!(other.kind, TransitionKind::Fade);
        assert_eq!(other.schedule.frames(), 15, "250 ms at 60 fps");
    }

    #[test]
    fn an_unknown_transition_names_what_this_build_renders() {
        let engine = engine();
        let error = engine
            .validate_transition(&transition("explode", 300, 60))
            .expect_err("unknown transition");
        let text = error.to_string();
        assert!(text.contains("explode"), "{text}");
        for known in ["fade", "wipe", "slide", "grow", "wave", "outer", "none"] {
            assert!(text.contains(known), "{text} must list `{known}`");
        }
    }

    #[test]
    fn a_transition_outside_allow_transitions_is_refused() {
        let mut config = Config::default();
        config.render.allow_transitions = vec!["none".to_string(), "fade".to_string()];
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), config);

        let error = engine
            .validate_transition(&transition("wave", 300, 60))
            .expect_err("wave is disallowed");
        assert!(error.to_string().contains("allow_transitions"), "{error}");
        assert!(
            engine
                .validate_transition(&transition("fade", 300, 60))
                .is_ok(),
            "an allowed transition must still work"
        );
    }

    #[test]
    fn transition_timing_is_range_checked() {
        let engine = engine();
        let too_long = engine
            .validate_transition(&transition("fade", MAX_TRANSITION_MS + 1, 60))
            .expect_err("duration cap");
        assert!(too_long.to_string().contains("maximum"), "{too_long}");

        let bad_fps = engine
            .validate_transition(&transition("fade", 300, 0))
            .expect_err("fps range");
        assert!(bad_fps.to_string().contains("out of range"), "{bad_fps}");
    }

    #[test]
    fn repeated_hotplug_cycles_neither_lose_outputs_nor_grow_state() {
        // The P2 gate item, at the daemon layer: plug and unplug two outputs 20
        // times and prove nothing is lost, nothing leaks, and the record survives
        // so a replug gets the same wallpaper back.
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), Config::default());
        engine
            .session
            .lock()
            .unwrap()
            .set("HDMI-A-1", "/walls/hdmi.png", "static-image");

        let base = [output("eDP-1", "Samsung")];
        let first = engine.reconcile_outputs(base.to_vec());
        assert_eq!(
            first.tracked,
            vec!["eDP-1".to_string()],
            "the first reconcile is what spawns the internal panel"
        );

        let mut tracked_after: Vec<usize> = Vec::new();
        let mut rss_at: Vec<(usize, u64)> = Vec::new();
        for round in 0..20 {
            // RSS is sampled *inside* the loop: a one-time allocation in the first
            // cycle (lazy statics, the first SQLite/state page) is not a leak, so the
            // gate compares cycle 10 with cycle 20. That is what makes the assertion
            // about repeated plug cycles rather than about process startup.
            if (round == 9 || round == 19)
                && let Some(bytes) = resident_bytes()
            {
                rss_at.push((round, bytes));
            }
            let mut with_dock = base.to_vec();
            with_dock.push(output("HDMI-A-1", "Dell U2720Q"));
            let added = engine.reconcile_outputs(with_dock);
            assert_eq!(added.added, vec!["HDMI-A-1".to_string()], "round {round}");
            assert!(
                !added.tracked.is_empty(),
                "round {round}: the supervisor lost every output"
            );
            tracked_after.push(added.tracked.len());

            // Unplug: the surface is released, the record is kept (PRD-F-08).
            let removed = engine.reconcile_outputs(base.to_vec());
            assert_eq!(
                removed.removed,
                vec!["HDMI-A-1".to_string()],
                "round {round}"
            );
            assert_eq!(
                removed.cleared,
                vec!["HDMI-A-1".to_string()],
                "round {round}"
            );
            assert!(
                engine.session.lock().unwrap().get("HDMI-A-1").is_some(),
                "round {round}: the session entry must survive an unplug"
            );
        }

        // No growth: the tracked set is bounded by the distinct connector names,
        // not by how many times they were plugged.
        assert_eq!(
            tracked_after.first(),
            tracked_after.last(),
            "supervisor state grew across cycles"
        );
        let workers = engine.workers.lock().unwrap();
        assert!(
            workers.len() <= 2,
            "worker map grew to {} entries (one per output is the contract)",
            workers.len()
        );
        drop(workers);

        // The other half of the gate: RSS must be stable across the cycles (±5%).
        // Reaching through cycle 10 → 20 also catches the slow leak a single
        // run-to-run comparison would miss. On a machine that cannot report RSS the
        // measurement is skipped *and says so*, rather than passing silently.
        if let (Some(&(_, early)), Some(&(_, late))) = (rss_at.first(), rss_at.last()) {
            let growth = late.saturating_sub(early);
            let budget = early / 20 + 1_048_576;
            eprintln!(
                "hotplug RSS: cycle 10 {early} B -> cycle 20 {late} B \
                 (growth {growth} B, budget {budget} B)"
            );
            assert!(
                growth <= budget,
                "20 plug cycles grew RSS by {growth} B ({early} -> {late}), over the 5%-plus-1MiB \
                 budget of {budget} B"
            );
        } else {
            eprintln!("RSS is not reportable on this platform; the leak check was skipped");
        }
    }

    /// Resident set size in bytes, from `/proc/self/statm`.
    ///
    /// Reads the process's own memory rather than shelling out to `ps`: the gate is
    /// about what the supervisor does across twenty plug cycles, and a subprocess
    /// per sample would be a measurement that perturbs what it measures.
    fn resident_bytes() -> Option<u64> {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let page_size = 4096_u64;
        Some(pages.saturating_mul(page_size))
    }

    #[test]
    fn an_output_that_was_never_seen_is_spawned_with_its_planned_wallpaper() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), Config::default());
        engine
            .session
            .lock()
            .unwrap()
            .set("DP-1", "/walls/dp.png", "static-image");

        let report = engine.reconcile_outputs(vec![output("DP-1", "Dell")]);
        assert_eq!(report.added, vec!["DP-1".to_string()]);
        // Either it was applied (a session is present) or the attempt failed for a
        // reason — but the *attempt* is the contract: every spawned output is
        // offered its planned wallpaper.
        assert_eq!(
            report.applied.len() + report.failures.len(),
            1,
            "{report:?}"
        );
        if let Some((name, reason)) = report.failures.first() {
            assert_eq!(name, "DP-1");
            assert!(!reason.is_empty());
        }
    }

    #[test]
    fn a_resized_output_is_re_applied_at_the_new_size() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), Config::default());
        engine
            .session
            .lock()
            .unwrap()
            .set("eDP-1", "/walls/edp.png", "static-image");

        engine.reconcile_outputs(vec![output("eDP-1", "Samsung")]);
        let mut resized = output("eDP-1", "Samsung");
        resized.width = 1280;
        resized.height = 720;
        let report = engine.reconcile_outputs(vec![resized]);

        assert!(
            report.added.is_empty() && report.removed.is_empty(),
            "a mode change is not a plug event: {report:?}"
        );
        assert_eq!(
            report.applied.len() + report.failures.len(),
            1,
            "a resized output must be re-rendered: {report:?}"
        );
    }

    #[test]
    fn two_outputs_resolve_to_their_own_configured_wallpapers() {
        // The GUI v1 gate's headless half: "assign different wallpapers to 2
        // outputs". Resolution is the part that can be proven without a compositor
        // — a wallpaper that resolves to the wrong monitor is the bug users report
        // as "it put the wrong picture on my second screen". The half that needs
        // real hardware (both pictures actually on screen) is recorded in the
        // phase's hardware checklist, not asserted here.
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.outputs.insert(
            "eDP-1".to_string(),
            owe_core::config::OutputConfig {
                wallpaper: Some("/walls/panel.png".to_string()),
                ..owe_core::config::OutputConfig::default()
            },
        );
        config.outputs.insert(
            "HDMI-A-1".to_string(),
            owe_core::config::OutputConfig {
                wallpaper: Some("/walls/dock.png".to_string()),
                ..owe_core::config::OutputConfig::default()
            },
        );
        let engine = engine_in(dir.path(), config);

        let panel = output("eDP-1", "Samsung Display Corp.");
        let dock = output("HDMI-A-1", "Dell Inc. DELL U2720Q");

        assert_eq!(
            engine.planned_wallpaper(&panel).map(|plan| plan.reference),
            Some("/walls/panel.png".to_string())
        );
        assert_eq!(
            engine.planned_wallpaper(&dock).map(|plan| plan.reference),
            Some("/walls/dock.png".to_string())
        );
    }

    #[test]
    fn a_fresh_engine_is_not_started_and_not_paused() {
        let engine = engine();
        assert!(!engine.is_started());
        assert!(!engine.is_paused());
    }

    #[test]
    fn pause_override_is_remembered() {
        let engine = engine();
        engine.set_paused(true);
        assert!(engine.is_paused());
        engine.set_paused(false);
        assert!(!engine.is_paused());
    }

    #[test]
    fn capabilities_report_only_what_this_build_implements() {
        let engine = engine();
        // P4's playback half put animated images and video on the same path a still
        // takes, so all three are renderable. Video is conditional on the *machine*
        // — a kind is advertised only while a runtime exists to decode it — so the
        // assertion is written against the probe rather than as a fixed list that
        // would fail on a box with neither GStreamer nor FFmpeg installed.
        let kinds = engine.content_kinds();
        assert_eq!(kinds.first(), Some(&ContentKind::StaticImage));
        assert_eq!(kinds.get(1), Some(&ContentKind::AnimatedImage));
        assert_eq!(
            kinds.contains(&ContentKind::Video),
            owe_media::any_video_runtime(),
            "video is advertised exactly when this machine can decode it"
        );
        assert!(
            owe_media::content_kinds().len() >= kinds.len(),
            "the decode layer never knows less than the presenter"
        );

        // The durable invariant, rather than a hardcoded list that goes stale every
        // phase: everything advertised over IPC must be selectable in a config file,
        // and nothing else. A backend that exists in the registry but not in
        // `KNOWN_SHELL_BACKENDS` would be unusable (`shell.backend` would reject
        // it), so advertising it would be a lie.
        for id in engine.backend_ids() {
            assert!(
                owe_core::config::KNOWN_SHELL_BACKENDS.contains(&id.as_str()),
                "`{id}` is advertised but `shell.backend` would refuse it"
            );
        }
        for known in owe_core::config::KNOWN_SHELL_BACKENDS {
            if *known == "auto" {
                continue;
            }
            assert!(
                engine.backend_ids().iter().any(|id| id == known),
                "`{known}` is a config-valid backend but nothing implements it"
            );
        }
    }

    #[test]
    fn the_generic_backend_is_the_last_resort_in_the_detect_chain() {
        // Order matters: a backend that knows less must never win over one that
        // knows more, or `-o focused` silently stops working on Hyprland.
        let order = owe_core::shell::default_detect_order();
        assert_eq!(
            order.last().map(String::as_str),
            Some("generic-layer-shell"),
            "the floor belongs at the bottom of the chain"
        );
        assert_eq!(order.first().map(String::as_str), Some("caelestia"));
    }

    #[test]
    fn starting_is_idempotent_and_reports_warnings_instead_of_failing() {
        // This test runs both headless (CI) and on a live desktop. It must not
        // draw anything, and it must never fail because a session is missing.
        let engine = engine();
        let first = engine.start();
        let second = engine.start();
        assert_eq!(first.backend, second.backend);
        assert!(engine.is_started());
        for warning in &first.warnings {
            eprintln!("startup warning: {warning}");
        }
    }

    #[test]
    fn an_unknown_backend_is_a_config_error_not_an_internal_fault() {
        // Regression: this used to surface as `INTERNAL: auto is not available:
        // unknown shell backend `caelestia``, which blames the wrong thing and
        // gives the user no way to act on it. `caelestia` itself became a real
        // backend in P3, so the example is now an id no build will ever have.
        let config = Config {
            shell: owe_core::config::ShellConfig {
                backend: "kwin".to_string(),
                ..owe_core::config::ShellConfig::default()
            },
            ..Config::default()
        };
        let paths = XdgPaths::resolve().expect("xdg paths");
        let engine = Engine::new(config, paths);

        let error = engine
            .apply("/tmp/whatever.png", &OutputTarget::All)
            .expect_err("kwin is not implemented in this build");
        let text = error.to_string();
        assert!(
            text.contains("unknown shell backend `kwin`"),
            "the error must name the backend the user asked for: {text}"
        );
        for id in engine.backend_ids() {
            assert!(
                text.contains(&id),
                "the error must list what does exist ({id} missing): {text}"
            );
        }
        assert!(
            !text.contains("auto"),
            "`auto` is not what the user asked for and must not be blamed: {text}"
        );
        assert!(
            matches!(error, EngineError::ShellSelection(_)),
            "the structured cause must survive for the IPC error code: {error:?}"
        );
    }

    #[test]
    fn a_recorded_wallpaper_is_not_reported_as_an_applied_one() {
        // Regression, found by restarting the daemon by hand: the session file was
        // written on the first run, and the second run's `owectl monitors` claimed
        // `wallpaper: <path>` for an output it had not drawn on. Restore is P2, so
        // the honest report is "recorded, not applied".
        let state_dir = tempfile::tempdir().expect("temp dir");
        let paths = XdgPaths {
            config_file: state_dir.path().join("config.toml"),
            state_dir: state_dir.path().to_path_buf(),
            cache_dir: state_dir.path().to_path_buf(),
            data_dir: state_dir.path().to_path_buf(),
            runtime_dir: None,
        };
        std::fs::write(
            paths.state_dir.join("session.json"),
            r#"{"version":1,"outputs":{"eDP-1":{"reference":"/tmp/old.png","kind":"static-image"}}}"#,
        )
        .expect("write session state");

        let engine = Engine::new(Config::default(), paths);
        let report = engine.start();
        if report.backend.is_none() {
            eprintln!("no session here; the view cannot be built, which is its own honest answer");
            return;
        }

        let views = engine.output_views().expect("views");
        for view in views {
            assert!(
                view.wallpaper.is_none(),
                "{} must not claim a wallpaper this run never presented",
                view.name
            );
            assert_eq!(
                view.recorded.as_deref(),
                Some("/tmp/old.png"),
                "{} should still tell the user what the session file remembers",
                view.name
            );
            // The state must agree with the claim: idle, not presented.
            assert_eq!(view.state, "idle", "{}", view.name);
        }
    }

    #[test]
    fn a_headless_environment_still_starts_and_reports_why_it_cannot_render() {
        // Simulate CI: no Hyprland signature, no session. Startup must succeed and
        // the reason must be visible in the report rather than a panic.
        let paths = XdgPaths::resolve().expect("xdg paths");
        let engine = Engine::new(Config::default(), paths);
        let report = engine.start();

        if report.backend.is_none() {
            let detail = report
                .backend_error
                .expect("a missing backend explains itself");
            assert!(detail.contains("backend"), "{detail}");
        } else {
            eprintln!("a session is present here; backend = {:?}", report.backend);
        }
    }

    #[test]
    fn the_configured_fit_mode_is_the_transform_the_renderer_is_given() {
        // FR-LIVE-4 is a *config* item, so the value has to survive the journey from
        // `render.fit` to the transform `present_direct` hands the renderer — a
        // setting that silently degrades to `fill` is the failure this pins. The
        // other half (the four ending up as four different pictures) is pixel-level
        // and lives in `owe-render/tests/animated_frames.rs`, where a GPU is needed.
        let dir = tempfile::tempdir().unwrap();
        let mut modes = Vec::new();
        for value in owe_core::config::KNOWN_FIT_MODES {
            let mut config = Config::default();
            config.render.fit = (*value).to_string();
            let engine = engine_in(dir.path(), config);
            modes.push((*value, engine.fit()));
        }

        assert_eq!(modes.len(), 4, "the TRD names four modes (FR-LIVE-4)");
        for (value, scaling) in &modes {
            assert_eq!(
                scaling.as_str(),
                *value,
                "`{value}` must arrive at the renderer as itself, not as {} ",
                Scaling::default().as_str()
            );
        }

        // And a config built in code — the validator never sees it — still cannot
        // smuggle in a fifth mode: parsing is the last gate before the transform.
        assert_eq!(Scaling::parse("diagonal"), None);
    }

    #[test]
    fn applying_an_unsupported_kind_says_so_rather_than_decoding() {
        let engine = engine();
        // A shader is a valid reference and the kind P5 still owes: until then
        // `wallpaper.set` must refuse it *by name* rather than pretend the reference
        // is broken. Animated images and video left this test when the frame clock
        // landed — they are in `content_kinds` now.
        let error = engine
            .apply("shader:aurora", &OutputTarget::All)
            .expect_err("shaders are not supported yet");
        let text = error.to_string();
        assert!(text.contains("shader"), "{text}");
        assert!(
            text.contains("P5") && text.contains("cannot be rendered"),
            "the refusal must name the kind and the phase that lands it: {text}"
        );
    }

    #[test]
    fn applying_an_invalid_reference_reports_the_model_error() {
        // Reference parsing is validated before any session is touched, so this
        // error must be identical headless and on a desktop. The first version
        // called apply() directly, which worked on a machine with a compositor
        // and failed in CI with a backend error instead — wrong layer entirely.
        // The extension check fires at parse time, which is exactly why this
        // error needs no session: it is a pure-model rejection.
        let model_error = WallpaperRef::parse("/tmp/notes.txt")
            .expect_err("a .txt must not parse into a wallpaper reference");
        assert!(model_error.to_string().contains("txt"), "{model_error}");

        // And the engine surfaces it as a Model error, not a shell one: verify
        // the mapping without needing outputs to exist.
        let error = WallpaperRef::parse("shader:nope");
        assert!(
            error.is_ok(),
            "the reference syntax is valid; only the kind is not"
        );
    }

    /// P3's live-session find: `resolved_kind()` ran before the library was
    /// consulted, so a `library:` reference was always refused with the very
    /// error that described the fix — "its content kind is resolved from the
    /// library database". Both orders are wrong for every mode, so both paths
    /// get a test: this one proves the reference *routes* to a shell-routed
    /// backend with the resolved file; the next proves the drawn path decodes it.
    #[test]
    fn a_library_reference_routes_to_a_shell_routed_backend() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.shell.backend = "caelestia".to_string();
        let engine = engine_in(dir.path(), config);
        engine.set_library(Arc::new(FakeLibrary(dir.path().to_path_buf())));

        let shell =
            RoutingShell::shell(DrawMode::ShellRouted, false, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell.clone());

        let applied = engine
            .apply("library:2", &OutputTarget::All)
            .expect("a library reference resolves through the row, not the reference syntax");
        assert!(applied.routed, "shell-routed mode must route, not draw");
        assert_eq!(applied.reference, "library:2");
        assert_eq!(
            shell.calls(),
            vec![None],
            "a whole-session reference reaches the shell as a whole-session request"
        );
    }

    /// The same bug, daemon-drawn half: the resolver is consulted for the path
    /// and the row's kind is what decides renderability — before any GPU or
    /// session work, so this fails on CI exactly as it does here.
    #[test]
    fn a_library_reference_resolves_before_kind_validation_in_drawn_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.shell.backend = "caelestia".to_string();
        let engine = engine_in(dir.path(), config);
        engine.set_library(Arc::new(FakeLibrary(dir.path().to_path_buf())));

        let shell = RoutingShell::shell(DrawMode::DaemonDrawn, false, vec![]);
        engine.register_backend(shell);

        let error = engine
            .apply("library:2", &OutputTarget::All)
            .expect_err("the resolver answered, so the kind question is settled; what fails next\n             depends on the environment (outputs, GPU), which other tests cover");
        let text = error.to_string();
        assert!(
            !text.contains("content kind is resolved"),
            "the kind must come from the row, not block the apply: {text}"
        );
    }

    #[test]
    fn a_library_reference_without_an_open_index_names_the_missing_dependency() {
        let engine = engine();
        let error = engine
            .apply("library:7", &OutputTarget::All)
            .expect_err("no resolver was injected");
        let text = error.to_string();
        assert!(
            text.contains("no library index open"),
            "the error must name the missing resolver, not the kind: {text}"
        );
    }

    #[test]
    fn selecting_an_unknown_output_is_a_clear_error() {
        let engine = engine();
        let error = engine
            .apply(
                "/tmp/does-not-matter.png",
                &OutputTarget::Named("DP-999".to_string()),
            )
            .expect_err("an unknown output (or, headless, no session at all)");
        // What is honest depends on the environment, and the test accepts each
        // environment's honest answer: with a session, the unknown name (or an
        // empty output list); without any session (CI), the backend error — which
        // IS the answer to "apply to DP-999" on a box with no compositor. What no
        // environment may do is complain about the file first.
        let text = error.to_string();
        assert!(
            text.contains("DP-999")
                || text.contains("no outputs")
                || text.contains("no shell backend"),
            "{text}"
        );
        assert!(
            !text.contains("No such file"),
            "target validation must come before decoding: {text}"
        );
    }

    // --- the shell registry (FR-SHELL-1, FR-SHELL-4) -------------------------

    #[test]
    fn every_known_backend_is_registered_in_auto_chain_order() {
        // The registry, not a config file, is what makes the `auto` chain true, and
        // the order of registration is the order `auto` walks when the config does
        // not override it. Asserting equality means a backend added in the wrong
        // place (or forgotten) shows up here rather than as a surprise pick.
        let engine = engine();
        assert_eq!(
            engine.backend_ids(),
            owe_core::shell::default_detect_order(),
            "the registered backends must be exactly the auto chain"
        );
    }

    #[test]
    fn a_backend_registered_at_runtime_is_selectable_by_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.shell.backend = "owe-test-shell".to_string();
        let engine = engine_in(dir.path(), config);

        let shell = Arc::new(RoutingShell {
            id: "owe-test-shell",
            outputs: vec![output("eDP-1", "Panel")],
            mode: DrawMode::ShellRouted,
            theme_refreshed: true,
            refuse_specific_output: false,
            calls: Mutex::new(Vec::new()),
            clear_calls: Mutex::new(Vec::new()),
            list_error: Mutex::new(false),
            clear_error: Mutex::new(false),
            not_applicable: Mutex::new(false),
        });
        engine.register_backend(shell.clone());

        let applied = engine
            .apply("/tmp/wall.png", &OutputTarget::All)
            .expect("the registered backend is selected by id and routes the apply");
        assert!(applied.routed);
        assert_eq!(shell.calls().len(), 1);
    }

    #[test]
    fn an_unknown_backend_is_a_precise_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.shell.backend = "kde".to_string();
        let engine = engine_in(dir.path(), config);

        let error = engine
            .apply("/tmp/wall.png", &OutputTarget::All)
            .expect_err("`kde` is not a backend this build has");
        match &error {
            EngineError::ShellSelection(SelectError::UnknownBackend {
                requested,
                available,
            }) => {
                assert_eq!(requested, "kde");
                // The list is the registry's own ids, so it cannot go stale.
                for id in engine.backend_ids() {
                    assert!(available.contains(&id), "{available} lacks {id}");
                }
            }
            other => panic!("expected an unknown-backend error, got {other:?}"),
        }
    }

    // --- shell-routed mode (FR-SHELL-3, TRD §4) -----------------------------

    /// A backend that owns the pixels and records what it was asked to do.
    ///
    /// Stands in for Caelestia: the daemon's job is to decide *that* the shell owns
    /// them and to hand over a wallpaper, and the shell's job is to run its CLI. The
    /// CLI half is proven in `owe-shell-caelestia`'s stub tests; this proves the
    /// daemon's half without spawning anything.
    #[derive(Debug, Default)]
    struct RoutingShell {
        id: &'static str,
        outputs: Vec<OutputInfo>,
        mode: DrawMode,
        theme_refreshed: bool,
        /// Caelestia refuses a named output because its CLI has no per-monitor
        /// target (OQ-2). Configurable so a future shell that *can* address one
        /// monitor is expressible here too.
        refuse_specific_output: bool,
        calls: Mutex<Vec<Option<String>>>,
        clear_calls: Mutex<Vec<Option<String>>>,
        list_error: Mutex<bool>,
        clear_error: Mutex<bool>,
        not_applicable: Mutex<bool>,
    }

    impl RoutingShell {
        fn shell(mode: DrawMode, theme_refreshed: bool, outputs: Vec<OutputInfo>) -> Arc<Self> {
            Arc::new(Self {
                id: "caelestia",
                outputs,
                mode,
                theme_refreshed,
                refuse_specific_output: true,
                calls: Mutex::new(Vec::new()),
                clear_calls: Mutex::new(Vec::new()),
                list_error: Mutex::new(false),
                clear_error: Mutex::new(false),
                not_applicable: Mutex::new(false),
            })
        }

        fn calls(&self) -> Vec<Option<String>> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn clear_calls(&self) -> Vec<Option<String>> {
            self.clear_calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn set_list_error(&self, value: bool) {
            *self
                .list_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
        }

        fn set_clear_error(&self, value: bool) {
            *self
                .clear_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
        }

        fn set_not_applicable(&self, value: bool) {
            *self
                .not_applicable
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
        }
    }

    /// A library resolver backed by real files on disk, so a `library:` apply
    /// exercises the row → kind → path → routed-apply chain without SQLite.
    struct FakeLibrary(PathBuf);

    impl LibraryResolver for FakeLibrary {
        fn resolve(&self, id: &str) -> Result<PathBuf, String> {
            let id: u32 = id
                .trim()
                .parse()
                .map_err(|_| format!("`{id}` is not a library id"))?;
            match id {
                1 => Ok(self.0.join("one.png")),
                2 => Ok(self.0.join("two.png")),
                other => Err(format!("library item {other} is not in the index")),
            }
        }

        fn kind_of(&self, id: &str) -> Result<ContentKind, String> {
            let _ = self.resolve(id)?;
            Ok(ContentKind::StaticImage)
        }
    }

    impl ShellBackend for RoutingShell {
        fn id(&self) -> &'static str {
            self.id
        }

        fn detect(&self, _env: owe_core::shell::EnvLookup<'_>) -> bool {
            true
        }

        fn list_outputs(&self) -> Result<Vec<OutputInfo>, ShellError> {
            if *self
                .list_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                return Err(ShellError::Backend {
                    backend: self.id.to_string(),
                    detail: "test list failure".to_string(),
                });
            }
            Ok(self.outputs.clone())
        }

        fn draw_mode(&self, _config: &owe_core::config::ShellConfig) -> DrawMode {
            self.mode
        }

        fn apply_wallpaper(
            &self,
            _config: &owe_core::config::ShellConfig,
            output: Option<&str>,
            wallpaper: &str,
        ) -> Result<ApplyOutcome, ShellError> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(output.map(str::to_string));
            if *self
                .not_applicable
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                return Ok(ApplyOutcome::NotApplicable);
            }
            if let Some(name) = output
                && self.refuse_specific_output
            {
                return Err(ShellError::Backend {
                    backend: self.id.to_string(),
                    detail: format!("cannot set `{name}` alone"),
                });
            }
            Ok(ApplyOutcome::Routed {
                detail: format!("{wallpaper} via the shell"),
                theme_refreshed: self.theme_refreshed,
            })
        }

        fn clear_wallpaper(
            &self,
            _config: &owe_core::config::ShellConfig,
            output: Option<&str>,
        ) -> Result<ApplyOutcome, ShellError> {
            self.clear_calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(output.map(str::to_string));
            if *self
                .clear_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                return Err(ShellError::Backend {
                    backend: self.id.to_string(),
                    detail: "test clear failure".to_string(),
                });
            }
            Ok(ApplyOutcome::Routed {
                detail: "cleared via the shell".to_string(),
                theme_refreshed: false,
            })
        }
    }

    fn routed_config(mode: &str) -> Config {
        let mut config = Config::default();
        config.shell.backend = "caelestia".to_string();
        config.shell.caelestia.mode = mode.to_string();
        config
    }

    #[test]
    fn a_failed_shell_patch_keeps_the_previous_override() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("daemon-drawn"));
        let current =
            RoutingShell::shell(DrawMode::DaemonDrawn, false, vec![output("eDP-1", "Panel")]);
        engine.register_backend(current);
        let failing = Arc::new(RoutingShell {
            id: "failing-shell",
            outputs: vec![output("DP-1", "Dock")],
            mode: DrawMode::DaemonDrawn,
            theme_refreshed: false,
            refuse_specific_output: false,
            calls: Mutex::new(Vec::new()),
            clear_calls: Mutex::new(Vec::new()),
            list_error: Mutex::new(false),
            clear_error: Mutex::new(false),
            not_applicable: Mutex::new(false),
        });
        failing.set_list_error(true);
        engine.register_backend(failing);
        let before = engine.shell_config();

        let error = engine
            .patch_shell(&ShellPatch {
                backend: Some("failing-shell".to_string()),
                ..ShellPatch::default()
            })
            .expect_err("the candidate backend cannot be adopted");

        assert!(matches!(error, EngineError::Shell(_)), "{error:?}");
        assert_eq!(engine.shell_config(), before);
        assert_eq!(engine.shell_status().backend.as_deref(), Some("caelestia"));
    }

    #[test]
    fn a_shell_patch_reconciles_the_new_effective_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("daemon-drawn"));
        let old = RoutingShell::shell(DrawMode::DaemonDrawn, false, vec![output("eDP-1", "Panel")]);
        engine.register_backend(old);
        engine.reconcile_outputs(vec![output("eDP-1", "Panel")]);

        let replacement = Arc::new(RoutingShell {
            id: "caelestia",
            outputs: vec![output("DP-1", "Dock")],
            mode: DrawMode::DaemonDrawn,
            theme_refreshed: false,
            refuse_specific_output: false,
            calls: Mutex::new(Vec::new()),
            clear_calls: Mutex::new(Vec::new()),
            list_error: Mutex::new(false),
            clear_error: Mutex::new(false),
            not_applicable: Mutex::new(false),
        });
        engine.register_backend(replacement);

        engine
            .patch_shell(&ShellPatch {
                caelestia_mode: Some("daemon-drawn".to_string()),
                ..ShellPatch::default()
            })
            .expect("the candidate backend lists successfully");

        let outputs = engine
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(
            outputs
                .iter()
                .map(|output| output.name.as_str())
                .collect::<Vec<_>>(),
            ["DP-1"]
        );
        let workers = engine
            .workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(workers.contains_key("DP-1"));
        assert!(!workers.contains_key("eDP-1"));
    }

    #[test]
    fn inactive_outputs_are_filtered_before_reconciliation() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("daemon-drawn"));
        let active = output("eDP-1", "Panel");
        let mut inactive = output("DP-1", "Dock");
        inactive.active = false;
        let shell = RoutingShell::shell(
            DrawMode::DaemonDrawn,
            false,
            vec![active.clone(), inactive.clone()],
        );
        engine.register_backend(shell);

        let (listed, report) = engine.refresh_outputs().expect("effective output refresh");
        assert_eq!(listed, vec![active.clone()]);
        assert!(report.is_empty());
        let outputs = engine
            .outputs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(outputs, vec![active]);
        let workers = engine
            .workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(workers.contains_key("eDP-1"));
        assert!(!workers.contains_key("DP-1"));
    }

    #[test]
    fn shell_routed_media_does_not_fall_back_when_the_shell_declines() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("shell-routed"));
        let shell =
            RoutingShell::shell(DrawMode::ShellRouted, false, vec![output("eDP-1", "Panel")]);
        shell.set_not_applicable(true);
        engine.register_backend(shell);

        let error = engine
            .apply("/missing/animation.gif", &OutputTarget::All)
            .expect_err("a shell-routed backend must not silently hand media to playback");
        assert!(matches!(error, EngineError::Shell(_)), "{error:?}");
        assert!(engine.playback_outputs().is_empty());
    }

    #[test]
    fn clear_uses_shell_semantics_and_releases_local_playback_state() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("shell-routed"));
        let shell =
            RoutingShell::shell(DrawMode::ShellRouted, false, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell.clone());
        engine.reconcile_outputs(vec![output("eDP-1", "Panel")]);
        engine
            .playback_sizes
            .lock()
            .unwrap()
            .insert("eDP-1".to_string(), (1920, 1080));

        let cleared = engine
            .clear(&OutputTarget::All)
            .expect("shell clear succeeds");
        assert_eq!(cleared, vec!["eDP-1".to_string()]);
        assert_eq!(shell.clear_calls(), vec![None]);
        assert!(!engine.playback_sizes.lock().unwrap().contains_key("eDP-1"));
    }

    #[test]
    fn a_refused_shell_clear_keeps_the_recorded_wallpaper() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("shell-routed"));
        let shell =
            RoutingShell::shell(DrawMode::ShellRouted, false, vec![output("eDP-1", "Panel")]);
        shell.set_clear_error(true);
        engine.register_backend(shell);
        engine
            .session
            .lock()
            .unwrap()
            .set("eDP-1", "/walls/keep.png", "static-image");

        let error = engine
            .clear(&OutputTarget::All)
            .expect_err("the shell refused to clear");
        assert!(matches!(error, EngineError::Shell(_)), "{error:?}");
        assert_eq!(
            engine
                .session
                .lock()
                .unwrap()
                .get("eDP-1")
                .map(|entry| entry.reference.as_str()),
            Some("/walls/keep.png")
        );
    }

    #[test]
    fn shell_routed_media_is_owned_by_the_shell_in_both_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("shell-routed"));
        let shell =
            RoutingShell::shell(DrawMode::ShellRouted, false, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell.clone());

        let animated = engine
            .apply("/missing/animation.gif", &OutputTarget::All)
            .expect("the shell owns animated media");
        let video = engine
            .apply("/missing/movie.mp4", &OutputTarget::All)
            .expect("the shell owns video media");

        assert!(animated.routed);
        assert!(video.routed);
        assert_eq!(shell.calls(), vec![None, None]);
        assert!(engine.playback_outputs().is_empty());
    }
    #[test]
    fn clear_releases_playback_state_for_the_selected_output() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("daemon-drawn"));
        let shell =
            RoutingShell::shell(DrawMode::DaemonDrawn, false, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell);
        engine
            .playback_sizes
            .lock()
            .unwrap()
            .insert("eDP-1".to_string(), (1920, 1080));

        let cleared = engine.clear(&OutputTarget::All).expect("clear succeeds");
        assert_eq!(cleared, vec!["eDP-1".to_string()]);
        assert!(!engine.playback_sizes.lock().unwrap().contains_key("eDP-1"));
        assert!(engine.playback_outputs().is_empty());
    }

    #[test]
    fn hotplug_teardown_releases_playback_state() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("daemon-drawn"));
        let shell =
            RoutingShell::shell(DrawMode::DaemonDrawn, false, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell);
        engine.reconcile_outputs(vec![output("eDP-1", "Panel")]);
        engine
            .playback_sizes
            .lock()
            .unwrap()
            .insert("eDP-1".to_string(), (1920, 1080));

        let report = engine.reconcile_outputs(Vec::new());
        assert!(report.cleared.contains(&"eDP-1".to_string()));
        assert!(!engine.playback_sizes.lock().unwrap().contains_key("eDP-1"));
    }

    #[test]
    fn a_failed_still_decode_does_not_release_existing_playback_state() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("daemon-drawn"));
        let shell =
            RoutingShell::shell(DrawMode::DaemonDrawn, false, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell);
        engine.reconcile_outputs(vec![output("eDP-1", "Panel")]);
        engine
            .playback_sizes
            .lock()
            .unwrap()
            .insert("eDP-1".to_string(), (1280, 720));

        let error = engine
            .apply("/missing/still.png", &OutputTarget::All)
            .expect_err("the missing still cannot be decoded");
        assert!(matches!(error, EngineError::Media(_)), "{error:?}");
        assert_eq!(
            engine.playback_sizes.lock().unwrap().get("eDP-1"),
            Some(&(1280, 720))
        );
    }

    #[test]
    fn a_failed_playback_present_drops_its_cached_size() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(engine_in(dir.path(), Config::default()));
        engine.attach_playback();
        engine
            .playback_sizes
            .lock()
            .unwrap()
            .insert("eDP-1".to_string(), (1280, 720));
        let frame = owe_media::DecodedFrame::new(0, 2, 2, Duration::from_millis(100), vec![0; 16])
            .expect("test frame");

        let error = engine
            .playback_sink
            .show("eDP-1", &frame)
            .expect_err("headless presentation cannot succeed");
        assert!(!error.is_empty());
        assert!(!engine.playback_sizes.lock().unwrap().contains_key("eDP-1"));
    }

    #[test]
    fn daemon_drawn_media_uses_the_playback_path_instead_of_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("daemon-drawn"));
        let shell =
            RoutingShell::shell(DrawMode::DaemonDrawn, false, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell.clone());

        let error = engine
            .apply("/missing/animation.gif", &OutputTarget::All)
            .expect_err("the missing file reaches playback, not routing");
        assert!(matches!(error, EngineError::Playback(_)), "{error:?}");
        assert!(shell.calls().is_empty());
    }

    #[test]
    fn a_routed_apply_asks_the_shell_once_for_the_whole_session() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("shell-routed"));
        let shell = RoutingShell::shell(
            DrawMode::ShellRouted,
            true,
            vec![output("eDP-1", "Panel"), output("HDMI-A-1", "Dell")],
        );
        engine.register_backend(shell.clone());

        let applied = engine
            .apply("/walls/a.png", &OutputTarget::All)
            .expect("the shell takes a whole-session change");

        assert!(applied.routed, "the reply says the shell did it");
        assert_eq!(
            applied.outputs,
            vec!["eDP-1".to_string(), "HDMI-A-1".to_string()],
            "a session-wide change is on every output it covers"
        );
        assert!(
            applied.sizes.is_empty() && applied.frames.is_empty(),
            "OWE presented nothing, so it reports no size and no frames"
        );
        assert_eq!(
            shell.calls(),
            vec![None],
            "one call, with no output: the shell sets the session"
        );
    }

    #[test]
    fn the_theme_runs_exactly_once_in_routed_mode() {
        // The double-theme guard (TRD §4). "Exactly once" is two claims: the shell
        // runs its pipeline, and OWE runs none of its own.
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("shell-routed"));
        let shell =
            RoutingShell::shell(DrawMode::ShellRouted, true, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell);

        let applied = engine
            .apply("/walls/a.png", &OutputTarget::All)
            .expect("routed");
        assert_eq!(applied.theme.total(), 1, "one theme refresh per apply");
        assert_eq!(
            applied.theme.by_owe, 0,
            "OWE has no theming pipeline to run"
        );
        assert!(applied.theme.by_shell, "the shell ran its own");
        assert!(
            applied
                .notes
                .iter()
                .any(|note| note.contains("OWE ran none")),
            "the reply says so where a user can read it: {:?}",
            applied.notes
        );
    }

    #[test]
    fn theme_hook_off_means_no_theme_run_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("shell-routed"));
        let shell =
            RoutingShell::shell(DrawMode::ShellRouted, false, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell);

        let applied = engine
            .apply("/walls/a.png", &OutputTarget::All)
            .expect("routed");
        assert_eq!(
            applied.theme.total(),
            0,
            "with theme_hook = false nothing themes, so nothing can theme twice"
        );
        assert!(
            applied.notes.iter().any(|note| note.contains("no-smart")),
            "{:?}",
            applied.notes
        );
    }

    #[test]
    fn a_routed_apply_is_recorded_for_restore() {
        // Restore reads the session file, so a routed apply that did not record
        // itself would come back as nothing after a restart — the same class of bug
        // the P2 exit gate found in `reconcile_outputs`.
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("shell-routed"));
        let shell =
            RoutingShell::shell(DrawMode::ShellRouted, true, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell);

        engine
            .apply("/walls/a.png", &OutputTarget::All)
            .expect("routed");

        let saved = SessionState::load(&dir.path().join("state").join("session.json"));
        assert!(saved.warnings.is_empty(), "{:?}", saved.warnings);
        let entry = saved
            .state
            .get("eDP-1")
            .expect("the output is in the session file");
        assert_eq!(entry.reference, "/walls/a.png");
        assert_eq!(entry.kind, "static-image");
    }

    #[test]
    fn a_per_output_request_is_passed_to_the_shell_and_its_refusal_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("shell-routed"));
        let shell = RoutingShell::shell(
            DrawMode::ShellRouted,
            true,
            vec![output("eDP-1", "Panel"), output("HDMI-A-1", "Dell")],
        );
        engine.register_backend(shell.clone());

        let error = engine
            .apply("/walls/a.png", &OutputTarget::Named("eDP-1".to_string()))
            .expect_err("Caelestia has no per-monitor target (OQ-2)");

        assert_eq!(
            shell.calls(),
            vec![Some("eDP-1".to_string())],
            "the request is passed on, not quietly widened to every output"
        );
        assert!(
            matches!(error, EngineError::Shell(_)),
            "the shell's refusal is the answer, not a fallback draw: {error:?}"
        );
        assert!(error.to_string().contains("eDP-1"), "{error}");

        // And nothing was recorded: the wall is unchanged, so the session must not
        // claim otherwise.
        let saved = SessionState::load(&dir.path().join("state").join("session.json"));
        assert!(saved.state.get("eDP-1").is_none());
    }

    #[test]
    fn daemon_drawn_mode_leaves_the_shell_alone() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine_in(dir.path(), routed_config("daemon-drawn"));
        let shell =
            RoutingShell::shell(DrawMode::DaemonDrawn, true, vec![output("eDP-1", "Panel")]);
        engine.register_backend(shell.clone());

        // The draw path runs and fails on the file, not on routing: in this mode the
        // shell is not consulted at all, which is what the user asked for by
        // choosing `daemon-drawn`.
        let error = engine
            .apply("/walls/does-not-exist.png", &OutputTarget::All)
            .expect_err("there is no such file");
        assert!(
            matches!(error, EngineError::Media(_)),
            "the draw path answered: {error:?}"
        );
        assert!(
            shell.calls().is_empty(),
            "a daemon-drawn shell is never asked to route"
        );
    }
}
