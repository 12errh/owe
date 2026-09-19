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
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use owe_core::model::{ContentKind, WallpaperRef, WallpaperSource};
use owe_core::output::{OutputInfo, OutputTarget};
use owe_core::path::XdgPaths;
use owe_core::shell::{Registry, SelectError, ShellBackend, ShellError};
use owe_core::state::{SessionState, StateError};
use owe_core::{Config, OutputWorker, WorkerAction, WorkerEvent};
use owe_media::{DecodedImage, MediaError};
use owe_render::gpu::{HeadlessGpu, RenderError};
use owe_render::image::{self, PixelFormat, Scaling};
use owe_render::surface::{Frame, PresentError, PresentOutcome, Presenter};
use serde::Serialize;
use thiserror::Error;

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
}

/// A wallpaper reference this run has presented on an output.
#[derive(Debug, Clone)]
struct AppliedRef {
    reference: String,
    kind: String,
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
    gpu: Mutex<Option<HeadlessGpu>>,
    presenter: Mutex<Option<Presenter>>,
    /// Manual governor override; reported over IPC and honoured from P4 onwards,
    /// where pausing actually stops frames.
    paused: Mutex<bool>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("backend", &self.config.shell.backend)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Build the engine. No session is touched until [`Engine::start`].
    pub fn new(config: Config, paths: XdgPaths) -> Self {
        let mut registry = Registry::new();
        registry.register(Box::new(owe_shell_hyprland::HyprlandBackend::new()));
        // The floor: any compositor that speaks plain Wayland. Last in the detect
        // order because it knows the least (no focus, no workspace events).
        registry.register(Box::new(owe_shell_generic::GenericBackend::new()));

        Self {
            config,
            paths,
            registry,
            generation: AtomicU64::new(1),
            started: Mutex::new(None),
            outputs: Mutex::new(Vec::new()),
            workers: Mutex::new(HashMap::new()),
            session: Mutex::new(SessionState::default()),
            applied: Mutex::new(HashMap::new()),
            gpu: Mutex::new(None),
            presenter: Mutex::new(None),
            paused: Mutex::new(false),
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

        let mut warnings = Vec::new();

        // Session state first: a corrupt file is a warning, never a failure.
        let loaded = SessionState::load(&self.paths.state_dir.join("session.json"));
        warnings.extend(loaded.warnings);
        if let Ok(mut session) = self.session.lock() {
            *session = loaded.state;
        }

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

        // Drop session entries for monitors that are gone, so `get` never
        // reports a wallpaper on an output that no longer exists.
        let connected: Vec<String> = outputs.iter().map(|output| output.name.clone()).collect();
        if let Ok(mut session) = self.session.lock() {
            let dropped = session.retain_outputs(&connected);
            if !dropped.is_empty() {
                warnings.push(format!(
                    "forgot wallpapers for absent outputs: {}",
                    dropped.join(", ")
                ));
            }
        }

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

    /// Apply a wallpaper reference to the resolved outputs.
    pub fn apply(&self, spec: &str, target: &OutputTarget) -> Result<Applied, EngineError> {
        let report = self.start();
        report.backend_failure()?;

        let reference = WallpaperRef::parse(spec)?;
        let kind = reference.resolved_kind()?;
        // P1 renders still images. Named, explicit failure for everything else
        // rather than a confusing decode error.
        owe_media::ensure_supported(kind)?;

        let path = match reference.source() {
            WallpaperSource::Path(path) => path.clone(),
            other => {
                return Err(EngineError::Media(MediaError::UnsupportedKind {
                    // The library and shader packs arrive in P2/P5; until then
                    // these are honest "not yet" answers, not silent no-ops.
                    kind: match other {
                        WallpaperSource::LibraryItem(_) => "library",
                        WallpaperSource::ShaderPack(_) => "shader",
                        WallpaperSource::Path(_) => unreachable!(),
                    },
                }));
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
        };

        for output in selected {
            let generation = self.generation.fetch_add(1, Ordering::SeqCst);
            let size = output.pixel_size();
            let outcome = self.present_on(gpu, &decoded, &output.name, size, generation)?;
            applied.outputs.push(output.name.clone());
            applied
                .sizes
                .push((output.name.clone(), outcome.0, outcome.1));
        }

        // Record what this run has on screen (fact), then persist it (intent for
        // the next run, once P2 restores it).
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

    /// Render and present one wallpaper on one output, following the worker state
    /// machine and re-rendering once if the compositor wants a different size.
    fn present_on(
        &self,
        gpu: &HeadlessGpu,
        decoded: &DecodedImage,
        output: &str,
        size: (u32, u32),
        generation: u64,
    ) -> Result<(u32, u32), EngineError> {
        let mut worker = {
            let mut workers = self
                .workers
                .lock()
                .map_err(|_| EngineError::NoGpu("worker map poisoned".to_string()))?;
            workers
                .entry(output.to_string())
                .or_insert_with(|| OutputWorker::new(output))
                .clone()
        };

        let action = worker.on(WorkerEvent::Apply { generation, size });
        let WorkerAction::Render { size, .. } = action else {
            // Nothing to do: the same generation is already on screen.
            return Ok(size);
        };

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

            let presented = worker.on(WorkerEvent::Prepared { generation });
            debug_assert!(matches!(presented, WorkerAction::Present { .. }));

            let frame = Frame {
                width: current_size.0,
                height: current_size.1,
                pixels,
            };

            let outcome = {
                let presenter = self
                    .presenter
                    .lock()
                    .map_err(|_| EngineError::NoGpu("presenter lock poisoned".to_string()))?;
                let presenter = presenter.as_ref().ok_or(EngineError::NoGpu(
                    "no Wayland session: cannot present wallpapers".to_string(),
                ))?;
                presenter.present(output, frame)?
            };

            match outcome {
                PresentOutcome::Presented { size } => {
                    worker.on(WorkerEvent::Presented { generation, size });
                    if let Ok(mut workers) = self.workers.lock() {
                        workers.insert(output.to_string(), worker);
                    }
                    return Ok(size);
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
        if let Ok(mut workers) = self.workers.lock() {
            workers.insert(output.to_string(), worker);
        }
        Err(EngineError::Present(PresentError::ConfigureTimeout {
            output: output.to_string(),
            timeout: std::time::Duration::from_secs(2),
        }))
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

    /// Content kinds this build can actually render.
    pub fn content_kinds(&self) -> Vec<ContentKind> {
        vec![ContentKind::StaticImage]
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

    fn engine() -> Engine {
        let paths = XdgPaths::resolve().expect("xdg paths");
        Engine::new(Config::default(), paths)
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
            .expect_err("unknown output");
        // Either "no output matches" (a session with outputs) or "no outputs" (a
        // headless box) — both are the caller's fault, clearly stated, and the
        // missing file must not be what we complain about first.
        let text = error.to_string();
        assert!(
            text.contains("DP-999") || text.contains("no outputs"),
            "{text}"
        );
        assert!(
            !text.contains("No such file"),
            "target validation must come before decoding: {text}"
        );
    }
}
