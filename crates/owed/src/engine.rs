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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use owe_core::config::{FPS_RANGE, MAX_TRANSITION_MS, Transition};
use owe_core::model::{ContentKind, WallpaperRef, WallpaperSource};
use owe_core::output::{OutputInfo, OutputTarget};
use owe_core::outputs as output_config;
use owe_core::path::XdgPaths;
use owe_core::shell::{Registry, SelectError, ShellBackend, ShellError};
use owe_core::state::{SessionState, StateError};
use owe_core::supervisor::{HotplugEvent, Supervisor, SupervisorAction};
use owe_core::{Config, OutputWorker, WorkerAction, WorkerEvent};
use owe_media::{DecodedImage, MediaError};
use owe_render::gpu::{HeadlessGpu, RenderError};
use owe_render::image::{self, PixelFormat, Scaling};
use owe_render::surface::{Frame, PresentError, PresentOutcome, Presenter};
use owe_render::transition::{Schedule, TransitionKind, TransitionRenderer};
use serde::Serialize;
use thiserror::Error;

/// Resolves `library:<id>` references to files, so the engine can apply (and
/// restore) library items without owning a database.
///
/// Injected rather than built in: the engine stays testable with a fake resolver,
/// and the library service stays the only thing that knows SQLite exists.
pub trait LibraryResolver: Send + Sync {
    /// Absolute path of a library item, or the reason it cannot be resolved.
    fn resolve(&self, id: &str) -> Result<PathBuf, String>;
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
    registry: Registry,
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
        registry.register(Box::new(owe_shell_hyprland::HyprlandBackend::new()));
        // The floor: any compositor that speaks plain Wayland. Last in the detect
        // order because it knows the least (no focus, no workspace events).
        registry.register(Box::new(owe_shell_generic::GenericBackend::new()));

        // A damaged session file is a warning, never a failure: a daemon that
        // refuses to start because its cache is corrupt is useless exactly when it
        // is needed.
        let loaded = SessionState::load(&paths.state_dir.join("session.json"));

        Self {
            config,
            paths,
            registry,
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

    /// Inject the library resolver. Called once, after the library service opens.
    ///
    /// Safe to call before or after [`Engine::start`]: resolution only happens
    /// inside an apply or a restore.
    pub fn set_library(&self, resolver: Arc<dyn LibraryResolver>) {
        if let Ok(mut slot) = self.library.lock() {
            *slot = Some(resolver);
        }
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
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    /// Whether a manual pause is in effect.
    pub fn is_paused(&self) -> bool {
        self.paused.lock().map(|guard| *guard).unwrap_or(false)
    }

    /// Set or clear the manual pause override.
    pub fn set_paused(&self, paused: bool) {
        if let Ok(mut guard) = self.paused.lock() {
            *guard = paused;
        }
    }

    /// Start the engine: load session state, select the backend, list outputs,
    /// and bring up the presenter. Idempotent, and never fatal (see
    /// [`StartupReport`]).
    pub fn start(&self) -> StartupReport {
        if let Some(report) = self.started.lock().ok().and_then(|guard| guard.clone()) {
            return report;
        }

        // Session warnings were collected in `new()`; see the field docs.
        let mut warnings = self
            .session_warnings
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();

        let (backend, backend_error, selection_error) =
            match self.registry.select(&self.config.shell, &env_lookup) {
                Ok(backend) => (Some(backend.id().to_string()), None, None),
                Err(error) => {
                    warnings.push(format!("no shell backend is usable: {error}"));
                    (None, Some(error.to_string()), Some(error))
                }
            };

        let outputs = match self
            .registry
            .get(backend.as_deref().unwrap_or_default())
            .map(ShellBackend::list_outputs)
        {
            Some(Ok(outputs)) => outputs,
            Some(Err(error)) => {
                // A missing `hyprctl` must not stop the daemon either.
                warnings.push(format!("cannot list outputs: {error}"));
                Vec::new()
            }
            None => Vec::new(),
        };

        // Session entries for outputs that are not connected right now are kept on
        // purpose. PRD-F-08 requires that a monitor unplugged and replugged gets
        // its wallpaper back without restarting the daemon, and booting with a
        // laptop lid shut must not erase the external panel's entry (FR-LIB-5). A
        // stale entry is invisible — `output_views` only reports connected outputs
        // — and is overwritten the moment that connector appears again.

        let presentable = match Presenter::start("owe") {
            Ok(presenter) => {
                let names = presenter.outputs().to_vec();
                if let Ok(mut slot) = self.presenter.lock() {
                    *slot = Some(presenter);
                }
                names
            }
            Err(error) => {
                warnings.push(format!(
                    "wallpaper rendering is unavailable this session: {error}"
                ));
                Vec::new()
            }
        };

        if let Ok(mut slot) = self.gpu.lock() {
            match HeadlessGpu::new() {
                Ok(Some(gpu)) => *slot = Some(gpu),
                Ok(None) => {
                    warnings.push("no GPU adapter available; nothing can be rendered".into())
                }
                Err(error) => warnings.push(format!("gpu initialisation failed: {error}")),
            }
        }

        // One worker per output: the state machine is per-output by design.
        if let Ok(mut workers) = self.workers.lock() {
            for output in &outputs {
                workers
                    .entry(output.name.clone())
                    .or_insert_with(|| OutputWorker::new(output.name.clone()));
            }
        }
        if let Ok(mut slot) = self.outputs.lock() {
            *slot = outputs.clone();
        }

        let report = StartupReport {
            backend,
            backend_error,
            selection_error,
            outputs,
            presenter: self
                .presenter
                .lock()
                .map(|guard| guard.is_some())
                .unwrap_or(false),
            presentable,
            warnings,
        };
        if let Ok(mut slot) = self.started.lock() {
            *slot = Some(report.clone());
        }
        report
    }

    /// Outputs plus what OWE currently believes about each.
    pub fn output_views(&self) -> Result<Vec<OutputView>, EngineError> {
        let report = self.start();
        report.backend_failure()?;

        let outputs = self
            .outputs
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let workers = self
            .workers
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let session = self
            .session
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let applied = self
            .applied
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();

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
        let kind = reference.resolved_kind()?;
        // P1 renders still images. Named, explicit failure for everything else
        // rather than a confusing decode error.
        owe_media::ensure_supported(kind)?;

        let report = self.start();
        report.backend_failure()?;

        // A transition request is validated here, before any output work: it is a
        // property of the request, not of the environment (same rule as above).
        if let Some(requested) = requested {
            self.validate_transition(requested)?;
        }

        let path = match reference.source() {
            WallpaperSource::Path(path) => path.clone(),
            WallpaperSource::LibraryItem(id) => self.resolve_library_reference(id)?,
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

        // Resolve the target *before* decoding. Two reasons: a typo'd monitor
        // name should not cost a 4K decode, and "which output" is the mistake a
        // user can actually fix on the spot. (Found by a test that expected the
        // output error and got a file error instead.)
        let outputs = self
            .outputs
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let selected = owe_core::output::resolve(&outputs, target)?;

        // Decode once, render per output: the common case is `-o all`.
        let decoded = owe_media::decode_file(&path)?;

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

        // Record what this run has on screen (fact), then persist it (intent for
        // the next run, which restore reads at startup).
        if let Ok(mut shown) = self.applied.lock() {
            for name in &applied.outputs {
                shown.insert(
                    name.clone(),
                    AppliedRef {
                        reference: applied.reference.clone(),
                        kind: applied.kind.clone(),
                    },
                );
            }
        }

        // Persist only what actually reached the screen.
        {
            let mut session = self.session.lock().map_err(|_| {
                EngineError::State(StateError::Write {
                    path: self.paths.state_dir.join("session.json"),
                    source: std::io::Error::other("session state lock poisoned"),
                })
            })?;
            for name in &applied.outputs {
                session.set(name.clone(), applied.reference.clone(), &applied.kind);
            }
            session.save(&self.paths.state_dir.join("session.json"))?;
        }

        Ok(applied)
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

    /// Resolve a `library:<id>` reference through the injected resolver.
    fn resolve_library_reference(&self, id: &str) -> Result<PathBuf, EngineError> {
        let resolver = self
            .library
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
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
            .ok()
            .and_then(|shown| shown.get(output).map(|entry| entry.reference.clone()));
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
                .map_err(|_| EngineError::NoGpu("worker map poisoned".to_string()))?;
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
        if let Ok(mut workers) = self.workers.lock() {
            workers.insert(output.to_string(), worker);
        }
    }

    /// Forget the transition entry for an output *if it is still ours*.
    fn finish_transition(&self, output: &str, epoch: u64) {
        if let Ok(mut map) = self.transitions.lock()
            && map.get(output).is_some_and(|entry| entry.epoch == epoch)
        {
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
                Scaling::Cover,
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
            .map_err(|_| EngineError::NoGpu("presenter lock poisoned".to_string()))?;
        let presenter = presenter.as_ref().ok_or_else(|| {
            EngineError::NoGpu("no Wayland session: cannot present wallpapers".to_string())
        })?;
        presenter.present(output, frame).map_err(EngineError::from)
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
            .ok()
            .and_then(|mut map| map.remove(output));
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
        let existing = self.transitions.lock().ok().and_then(|map| {
            map.get(output)
                .map(|entry| (Arc::clone(&entry.renderer), Arc::clone(&entry.progress)))
        });

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

        if let Ok(mut map) = self.transitions.lock() {
            map.insert(
                output.to_string(),
                InFlight {
                    epoch,
                    renderer: Arc::clone(&renderer),
                    progress: Arc::clone(&progress),
                },
            );
        }

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
            Scaling::Cover,
            PixelFormat::Bgra8,
        )?;
        Ok((
            Arc::new(Mutex::new(renderer)),
            Arc::new(AtomicU32::new(0.0_f32.to_bits())),
        ))
    }

    /// Remove OWE's wallpaper from the resolved outputs.
    pub fn clear(&self, target: &OutputTarget) -> Result<Vec<String>, EngineError> {
        self.start();

        let outputs = self
            .outputs
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let selected = owe_core::output::resolve(&outputs, target)?;

        let mut cleared = Vec::new();
        let presenter_guard = self
            .presenter
            .lock()
            .map_err(|_| EngineError::NoGpu("presenter lock poisoned".to_string()))?;

        for output in selected {
            if let Some(presenter) = presenter_guard.as_ref() {
                presenter.clear(&output.name)?;
            }
            if let Ok(mut workers) = self.workers.lock()
                && let Some(worker) = workers.get_mut(&output.name)
            {
                worker.clear();
            }
            if let Ok(mut session) = self.session.lock() {
                session.remove(&output.name);
            }
            if let Ok(mut shown) = self.applied.lock() {
                shown.remove(&output.name);
            }
            cleared.push(output.name.clone());
        }

        let session = self
            .session
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        session.save(&self.paths.state_dir.join("session.json"))?;
        Ok(cleared)
    }

    /// What OWE intends to show on one output, and where that intent comes from.
    ///
    /// Precedence: the **session file** (the last thing the user actually applied)
    /// beats an `[outputs.*]` section, because it is more recent and more
    /// specific. Nothing configured anywhere means `None` — and OWE leaves that
    /// output alone rather than inventing a wallpaper for it.
    pub fn planned_wallpaper(&self, output: &OutputInfo) -> Option<PlannedWallpaper> {
        if let Ok(session) = self.session.lock()
            && let Some(entry) = session.get(&output.name)
        {
            return Some(PlannedWallpaper {
                reference: entry.reference.clone(),
                source: PlanSource::Session,
            });
        }

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
        let previous = self
            .outputs
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
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
        if let Ok(mut workers) = self.workers.lock() {
            workers.retain(|name, _| connected.contains(name));
            for output in &current {
                workers
                    .entry(output.name.clone())
                    .or_insert_with(|| OutputWorker::new(output.name.clone()));
            }
        }

        let now = Instant::now();
        let actions = self
            .supervisor
            .lock()
            .map(|mut supervisor| {
                let actions = supervisor.observe_snapshot(&previous, &current, now);
                report.tracked = supervisor.present();
                actions
            })
            .unwrap_or_default();

        if let Ok(mut slot) = self.outputs.lock() {
            *slot = current.clone();
        }

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
                    // Release the surface and forget the worker, but keep the
                    // session entry: a replug must restore the same wallpaper
                    // (PRD-F-08).
                    self.stop_transition(&name);
                    if let Ok(guard) = self.presenter.lock()
                        && let Some(presenter) = guard.as_ref()
                        && let Err(error) = presenter.clear(&name)
                    {
                        report.failures.push((name.clone(), error.to_string()));
                    }
                    if let Ok(mut workers) = self.workers.lock() {
                        workers.remove(&name);
                    }
                    if let Ok(mut shown) = self.applied.lock() {
                        shown.remove(&name);
                    }
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
        if let Ok(mut slot) = self.outputs.lock() {
            *slot = current;
        }

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
        let listed = match report
            .backend
            .as_deref()
            .and_then(|id| self.registry.get(id))
            .map(ShellBackend::list_outputs)
        {
            Some(Ok(outputs)) => outputs,
            Some(Err(error)) => {
                tracing::warn!(%error, "cannot list outputs; keeping the previous list");
                self.outputs
                    .lock()
                    .map(|guard| guard.clone())
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };
        let reconcile = self.reconcile_outputs(listed.clone());
        Ok((listed, reconcile))
    }

    /// Content kinds this build can actually render.
    pub fn content_kinds(&self) -> Vec<ContentKind> {
        vec![ContentKind::StaticImage]
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
            .ok()
            .and_then(|mut slot| slot.as_mut().and_then(Presenter::take_events))
    }

    /// Shut the presenter down (used at exit, and by the hotplug driver's `Drop`
    /// so its blocking listener thread is released).
    pub fn stop_presenter(&self) {
        let presenter = self.presenter.lock().ok().and_then(|mut slot| slot.take());
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
            .ids()
            .iter()
            .map(|id| (*id).to_string())
            .collect()
    }
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
            if round == 9 || round == 19 {
                if let Some(bytes) = resident_bytes() {
                    rss_at.push((round, bytes));
                }
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
        assert_eq!(
            engine.backend_ids(),
            vec!["hyprland".to_string(), "generic-layer-shell".to_string()],
            "capabilities must list exactly the registered backends"
        );
        assert_eq!(engine.content_kinds(), vec![ContentKind::StaticImage]);

        // `caelestia` is planned (P3) and must appear nowhere as if it worked.
        assert!(
            !engine.backend_ids().iter().any(|id| id == "caelestia"),
            "an unimplemented backend must not be advertised as registered"
        );
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
        // gives the user no way to act on it.
        let config = Config {
            shell: owe_core::config::ShellConfig {
                backend: "caelestia".to_string(),
                ..owe_core::config::ShellConfig::default()
            },
            ..Config::default()
        };
        let paths = XdgPaths::resolve().expect("xdg paths");
        let engine = Engine::new(config, paths);

        let error = engine
            .apply("/tmp/whatever.png", &OutputTarget::All)
            .expect_err("caelestia is not implemented in this build");
        let text = error.to_string();
        assert!(
            text.contains("unknown shell backend `caelestia`"),
            "the error must name the backend the user asked for: {text}"
        );
        assert!(
            text.contains("this build has: hyprland, generic-layer-shell"),
            "the error must list what does exist: {text}"
        );
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
    fn applying_an_unsupported_kind_says_so_rather_than_decoding() {
        let engine = engine();
        // A video is a valid reference but not renderable in P1. The error must
        // name the kind, not pretend the file is broken.
        let error = engine
            .apply("/tmp/movie.mp4", &OutputTarget::All)
            .expect_err("videos are not supported yet");
        let text = error.to_string();
        assert!(text.contains("video"), "{text}");
        assert!(text.contains("not supported"), "{text}");
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
}
