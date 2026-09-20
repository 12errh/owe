//! Shell backends: how OWE learns about outputs and (from P3) environment events.
//!
//! Two rules from ARCHITECTURE §1 shape this module:
//!
//! 3. **Registries, not if-chains.** Backends register under a string id, config
//!    selects one, and an unknown id is a configuration error — never a silent
//!    fallback to something the user did not ask for.
//! 4. **Nothing is hardcoded to one compositor.** Hyprland, Caelestia and the
//!    generic layer-shell backend all implement [`ShellBackend`], and adding a
//!    fourth means adding a crate and one `register` call.
//!
//! Detection is env-driven and takes a lookup closure, so it is unit-testable
//! without touching the real environment (same pattern as [`crate::path`]).

use std::sync::Arc;

use thiserror::Error;

use crate::config::ShellConfig;
use crate::output::OutputInfo;

/// Environment lookup, injected so tests are hermetic.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Command lines of every process the current user can see.
///
/// This is the one function in this module that touches the machine rather than a
/// value, and it lives here because *three* callers need the same answer: Caelestia
/// detection (`qs -c caelestia`), and the hyprpaper/swww coexistence scan (TRD §4).
/// Two process readers would eventually disagree.
///
/// Read from `/proc`, which is where a Linux process table actually is: calling
/// `ps` would fork on startup and on every status request, and would make the
/// answer depend on `procps` being installed. Every failure is tolerated — a
/// process that exits mid-scan is skipped, and a machine with no procfs returns an
/// empty list, which degrades detection to "the binary is installed" rather than
/// inventing a shell.
pub fn process_table() -> Vec<String> {
    let mut commands = Vec::new();

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return commands;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let command = raw
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        if !command.is_empty() {
            commands.push(command);
        }
    }
    commands
}

/// Failures a backend can report.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ShellError {
    /// The backend is not usable in this session (no socket, no binary, …).
    #[error("{backend} is not available: {detail}")]
    Unavailable {
        /// Backend id.
        backend: String,
        /// What is missing.
        detail: String,
    },
    /// The backend ran but reported an error.
    #[error("{backend} failed: {detail}")]
    Backend {
        /// Backend id.
        backend: String,
        /// Underlying cause.
        detail: String,
    },
}

/// How confident a backend is that it belongs in this session.
///
/// Three levels and not a bool because "I am running in Hyprland" and "a
/// wallpaper shell is probably installed here" are different claims, and only one
/// of them should win an `auto` chain over a working fallback. The GUI's status
/// card shows this verbatim, so the distinction has a user-visible job too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// This backend does not apply here.
    None,
    /// The pieces are present but nothing is running (an installed shell, an idle
    /// session). Usable when the user names it explicitly; loses to a stronger
    /// claim in an `auto` chain.
    Weak,
    /// The environment is verifiably this one.
    Strong,
}

impl Confidence {
    /// Wire/config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::None => "none",
            Confidence::Weak => "weak",
            Confidence::Strong => "strong",
        }
    }
}

/// A detection answer plus the reason for it.
///
/// The reason is not decoration: `shell.backend = auto` picks a backend, and when
/// the pick is surprising ("why is it using generic-layer-shell?") the only
/// answer that helps is the one each backend gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// How sure the backend is.
    pub confidence: Confidence,
    /// Why, in a sentence the user can act on.
    pub reason: String,
}

impl Detection {
    /// Not applicable here.
    pub fn none(reason: impl Into<String>) -> Self {
        Self {
            confidence: Confidence::None,
            reason: reason.into(),
        }
    }

    /// Pieces present, nothing running.
    pub fn weak(reason: impl Into<String>) -> Self {
        Self {
            confidence: Confidence::Weak,
            reason: reason.into(),
        }
    }

    /// Verifiably this environment.
    pub fn strong(reason: impl Into<String>) -> Self {
        Self {
            confidence: Confidence::Strong,
            reason: reason.into(),
        }
    }

    /// Whether this counts as detected at all.
    pub fn is_detected(&self) -> bool {
        self.confidence != Confidence::None
    }
}

/// Who owns the pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrawMode {
    /// OWE renders the wallpaper itself on a layer-shell background surface (P1,
    /// and the only mode that can put a different wallpaper on each monitor with
    /// a transition).
    #[default]
    DaemonDrawn,
    /// The shell owns the wallpaper and OWE asks it to switch. Caelestia in
    /// `shell-routed` mode; see TRD §4 for why the shell also runs the theme.
    ShellRouted,
}

impl DrawMode {
    /// Wire/config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            DrawMode::DaemonDrawn => "daemon-drawn",
            DrawMode::ShellRouted => "shell-routed",
        }
    }

    /// Parse the config spelling. `None` for anything else — a typo must not
    /// silently become a mode.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "daemon-drawn" => Some(DrawMode::DaemonDrawn),
            "shell-routed" => Some(DrawMode::ShellRouted),
            _ => None,
        }
    }
}

/// What a backend did with an apply it was asked to handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Nothing to do: this backend does not own the pixels, so the caller should
    /// carry on with the layer-shell path. The default, which is why the P1
    /// backends never had to learn about modes.
    NotApplicable,
    /// The shell took the request.
    Routed {
        /// What was run, for logs and for `wallpaper.set`'s reply.
        detail: String,
        /// Whether the shell's own theming pipeline also ran (Caelestia's smart
        /// scheme). OWE must never add a second theme run on top of this.
        theme_refreshed: bool,
    },
}

/// One environment event, typed.
///
/// The P6 governor consumes these; P3 builds the bus and proves the parsing.
/// `Other` exists on purpose: a parser that drops event kinds it does not know is
/// how a governor later misses the one event that mattered, so unknown events are
/// preserved verbatim instead of discarded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ShellEvent {
    /// `activewindow>>class,title`
    ActiveWindow {
        /// Window class (app id).
        class: String,
        /// Window title.
        title: String,
    },
    /// `activewindowv2>>address`
    ActiveWindowAddress {
        /// Hyprland window address.
        address: String,
    },
    /// `fullscreen>>0|1`
    Fullscreen {
        /// Whether the focused window is fullscreen (the trigger for PRD-F-21).
        on: bool,
    },
    /// `workspace>>id` or `workspacev2>>id,name`
    Workspace {
        /// Workspace id.
        id: i64,
        /// Workspace name, when the shell sends one.
        name: Option<String>,
    },
    /// `focusedmon>>monitor,workspace`
    FocusedMonitor {
        /// Monitor name.
        monitor: String,
        /// Workspace name on that monitor.
        workspace: String,
    },
    /// `monitoradded>>name` or `monitoraddedv2>>id,name,description`
    MonitorAdded {
        /// Hyprland's numeric monitor id, when sent.
        id: Option<i64>,
        /// Connector name.
        monitor: String,
        /// EDID-ish description, when sent.
        description: Option<String>,
    },
    /// `monitorremoved>>name`
    MonitorRemoved {
        /// Connector name.
        monitor: String,
    },
    /// `openwindow>>address,workspace,class,title`
    OpenWindow {
        /// Window address.
        address: String,
        /// Workspace it opened on.
        workspace: String,
        /// Window class.
        class: String,
        /// Window title.
        title: String,
    },
    /// `closewindow>>address`
    CloseWindow {
        /// Window address.
        address: String,
    },
    /// `windowtitle>>address,title`
    WindowTitle {
        /// Window address.
        address: String,
        /// New title.
        title: String,
    },
    /// `submap>>name` — empty name means the default submap.
    Submap {
        /// Active submap name.
        name: String,
    },
    /// `activelayout>>keyboard,layout`
    ActiveLayout {
        /// Keyboard device name.
        keyboard: String,
        /// Layout name.
        layout: String,
    },
    /// `openlayer>>namespace`
    LayerOpened {
        /// Layer namespace.
        namespace: String,
    },
    /// `closelayer>>namespace`
    LayerClosed {
        /// Layer namespace.
        namespace: String,
    },
    /// `createworkspace>>id` / `createworkspacev2>>id,name`
    WorkspaceCreated {
        /// Workspace id.
        id: i64,
        /// Workspace name, when sent.
        name: Option<String>,
    },
    /// `destroyworkspace>>id` / `destroyworkspacev2>>id,name`
    WorkspaceDestroyed {
        /// Workspace id.
        id: i64,
        /// Workspace name, when sent.
        name: Option<String>,
    },
    /// `urgent>>address`
    Urgent {
        /// Window address.
        address: String,
    },
    /// `configreloaded>>`
    ConfigReloaded,
    /// An event this build does not type yet, kept whole.
    Other {
        /// Wire name.
        kind: String,
        /// Raw payload, exactly as received.
        payload: String,
    },
}

impl ShellEvent {
    /// The wire name, which is also its `Display`/config spelling.
    pub fn kind(&self) -> &str {
        match self {
            ShellEvent::ActiveWindow { .. } => "activewindow",
            ShellEvent::ActiveWindowAddress { .. } => "activewindowv2",
            ShellEvent::Fullscreen { .. } => "fullscreen",
            ShellEvent::Workspace { .. } => "workspace",
            ShellEvent::FocusedMonitor { .. } => "focusedmon",
            ShellEvent::MonitorAdded { .. } => "monitoradded",
            ShellEvent::MonitorRemoved { .. } => "monitorremoved",
            ShellEvent::OpenWindow { .. } => "openwindow",
            ShellEvent::CloseWindow { .. } => "closewindow",
            ShellEvent::WindowTitle { .. } => "windowtitle",
            ShellEvent::Submap { .. } => "submap",
            ShellEvent::ActiveLayout { .. } => "activelayout",
            ShellEvent::LayerOpened { .. } => "openlayer",
            ShellEvent::LayerClosed { .. } => "closelayer",
            ShellEvent::WorkspaceCreated { .. } => "createworkspace",
            ShellEvent::WorkspaceDestroyed { .. } => "destroyworkspace",
            ShellEvent::Urgent { .. } => "urgent",
            ShellEvent::ConfigReloaded => "configreloaded",
            ShellEvent::Other { kind, .. } => kind,
        }
    }

    /// Whether the event carries real environment state.
    ///
    /// `fullscreen` and `workspace` move without a window existing, so they are
    /// state. `WindowTitle` is not, and the governor should not wake for it —
    /// P6's rule table uses this to skip noise at the source instead of filtering
    /// it after the fact.
    pub fn is_state(&self) -> bool {
        !matches!(
            self,
            ShellEvent::WindowTitle { .. }
                | ShellEvent::ActiveLayout { .. }
                | ShellEvent::Urgent { .. }
                | ShellEvent::ConfigReloaded
        )
    }
}

/// A source of output information and session events.
///
/// `Send + Sync` is part of the contract, not an accident: the daemon shares one
/// registry between the IPC threads and the event-bus thread, so a backend that
/// cannot cross threads cannot be a backend.
pub trait ShellBackend: Send + Sync {
    /// Stable id: `hyprland`, `caelestia`, `generic-layer-shell`.
    fn id(&self) -> &'static str;

    /// Whether this backend can work in the current environment.
    fn detect(&self, env: EnvLookup<'_>) -> bool;

    /// List connected outputs.
    fn list_outputs(&self) -> Result<Vec<OutputInfo>, ShellError>;

    /// Detect, and say why.
    ///
    /// Defaulted from [`ShellBackend::detect`], so a backend with nothing more to
    /// say (and every fake backend in the tests) needs no new code. Backends that
    /// can distinguish "installed" from "running" override it, because that
    /// distinction decides `auto` chains and is shown in the GUI's status card.
    fn detection(&self, env: EnvLookup<'_>) -> Detection {
        if self.detect(env) {
            Detection::strong(format!("{} is present in this session", self.id()))
        } else {
            Detection::none(format!("{} is not present in this session", self.id()))
        }
    }

    /// Who owns the pixels, given the live config.
    fn draw_mode(&self, _config: &ShellConfig) -> DrawMode {
        DrawMode::DaemonDrawn
    }

    /// Ask the shell to put a wallpaper up, when the shell owns the pixels.
    ///
    /// `NotApplicable` (the default) means "not mine to do": the caller draws on
    /// its own layer surface. `shell_routed` is only ever attempted for a backend
    /// that reports [`DrawMode::ShellRouted`], so a daemon-drawn shell never routes.
    ///
    /// `output: None` means "the whole session". A backend that cannot address one
    /// monitor must refuse `Some(name)` rather than quietly setting every screen —
    /// Caelestia's CLI has no per-monitor target (OQ-2, ADR-017), and a silent
    /// broadening would be a lie about what the user asked for.
    ///
    /// The config comes in because routing semantics are per-backend *configuration*
    /// (Caelestia's `theme_hook`, its wallpapers directory) and the daemon reloads
    /// that config through `config.patch` without rebuilding the registry.
    fn apply_wallpaper(
        &self,
        _config: &ShellConfig,
        _output: Option<&str>,
        _wallpaper: &str,
    ) -> Result<ApplyOutcome, ShellError> {
        Ok(ApplyOutcome::NotApplicable)
    }

    /// Take the wallpaper down. Shells that cannot express "no wallpaper" say so
    /// rather than clearing to something arbitrary.
    fn clear_wallpaper(
        &self,
        _config: &ShellConfig,
        _output: Option<&str>,
    ) -> Result<ApplyOutcome, ShellError> {
        Ok(ApplyOutcome::NotApplicable)
    }
}

/// Another wallpaper tool, found running (TRD §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallpaperTool {
    /// `hyprpaper` — the one tool OWE may be configured to stop.
    Hyprpaper,
    /// `swww` (or its maintained fork `awww`).
    Swww,
    /// `awww` — the fork, named separately so the message names what is running.
    Awww,
}

impl WallpaperTool {
    /// Human/tool id.
    pub fn as_str(self) -> &'static str {
        match self {
            WallpaperTool::Hyprpaper => "hyprpaper",
            WallpaperTool::Swww => "swww",
            WallpaperTool::Awww => "awww",
        }
    }

    /// Recognize a tool from a process command line.
    ///
    /// Lives here, once, because both the scan and the tests need the same
    /// answer: `swww-daemon` is swww, `/usr/bin/hyprpaper` is hyprpaper, and a
    /// false positive here would mean warning about (or worse, stopping) a
    /// process the user does not think of as a wallpaper tool.
    pub fn from_command(command: &str) -> Option<Self> {
        let binary = command
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .trim_end_matches("-daemon");
        match binary {
            "hyprpaper" => Some(WallpaperTool::Hyprpaper),
            "swww" => Some(WallpaperTool::Swww),
            "awww" => Some(WallpaperTool::Awww),
            _ => None,
        }
    }
}

/// A competing tool that is actually running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompetingProcess {
    /// Which tool.
    pub tool: WallpaperTool,
    /// Process id.
    pub pid: u32,
    /// The command line, trimmed, for the message.
    pub command: String,
}

/// What to do about a competing tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoexistencePolicy {
    /// Say something and carry on (the default).
    Warn,
    /// Say something, then terminate the process.
    Stop,
    /// Do nothing at all.
    Ignore,
}

impl CoexistencePolicy {
    /// Config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            CoexistencePolicy::Warn => "warn",
            CoexistencePolicy::Stop => "stop",
            CoexistencePolicy::Ignore => "ignore",
        }
    }

    /// Parse the config spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "warn" => Some(CoexistencePolicy::Warn),
            "stop" => Some(CoexistencePolicy::Stop),
            "ignore" => Some(CoexistencePolicy::Ignore),
            _ => None,
        }
    }
}

/// The decision for one scan of the process table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[must_use]
pub struct CoexistenceDecision {
    /// One sentence per tool worth telling the user about.
    pub notices: Vec<String>,
    /// PIDs to terminate. Empty unless the policy is `stop` *and* the tool is
    /// hyprpaper — see [`decide_coexistence`].
    pub stop: Vec<u32>,
}

impl CoexistenceDecision {
    /// Whether anything at all needs saying.
    pub fn is_quiet(&self) -> bool {
        self.notices.is_empty()
    }
}

/// Decide what to do about the competing tools in `found`.
///
/// Two rules from TRD §4 are encoded here rather than left to the caller:
///
/// 1. **swww/awww are never stopped, whatever the policy says.** The config key is
///    `shell.hyprland.hyprpaper`, and letting it kill a different daemon would be
///    a serious surprise. Detection and warning only.
/// 2. **Nothing is killed silently.** `stop` produces a notice *and* a PID, and the
///    caller logs the notice before signalling — so the user can find out why a
///    process disappeared from the journal.
///
/// Pure: takes the process list, returns the decision. The scanning and the
/// signalling are the caller's job, which is what makes both testable.
pub fn decide_coexistence(
    found: &[CompetingProcess],
    policy: CoexistencePolicy,
) -> CoexistenceDecision {
    let mut decision = CoexistenceDecision::default();
    if policy == CoexistencePolicy::Ignore {
        return decision;
    }

    for process in found {
        let tool = process.tool.as_str();
        match (process.tool, policy) {
            (WallpaperTool::Hyprpaper, CoexistencePolicy::Stop) => {
                decision.notices.push(format!(
                    "stopping hyprpaper (pid {}, `{}`) because shell.hyprland.hyprpaper = \"stop\"",
                    process.pid, process.command
                ));
                decision.stop.push(process.pid);
            }
            (WallpaperTool::Hyprpaper, _) => decision.notices.push(format!(
                "hyprpaper is running (pid {}); both tools draw on the background layer, \
                 so set `shell.hyprland.hyprpaper` to \"stop\" or \"ignore\" to silence this",
                process.pid
            )),
            (WallpaperTool::Swww | WallpaperTool::Awww, _) => decision.notices.push(format!(
                "{tool} is running (pid {}); OWE does not manage another daemon's state — \
                 leave it as the wallpaper owner, or stop it yourself",
                process.pid
            )),
        }
    }
    decision
}

/// An unwrapped string field, for parsers that must not guess.
///
///
/// Kept for the P3+ event parsers: `pub` because a backend crate's parser may need
/// the same "split at most once, keep the rest whole" rule that the socket2 type
/// uses for window titles.
#[allow(
    dead_code,
    reason = "used by the socket2 parser's tests today, and by P6's governor rules next"
)]
pub fn split_first_comma(payload: &str) -> (&str, &str) {
    match payload.split_once(',') {
        Some((left, right)) => (left, right),
        None => (payload, ""),
    }
}

/// Why backend selection failed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// `shell.backend` names an id this build does not know.
    #[error("unknown shell backend `{requested}`; this build has: {available}")]
    UnknownBackend {
        /// What the config asked for.
        requested: String,
        /// Ids that exist.
        available: String,
    },

    /// `shell.backend = auto` found nothing that works here.
    #[error(
        "no shell backend is available in this session (tried: {tried}); \
         set `shell.backend` explicitly or make sure a supported shell is running"
    )]
    NoBackendDetected {
        /// Ids that were tried, in order.
        tried: String,
    },
}

/// The set of backends this build knows about.
///
/// Backends are held as [`Arc`]s rather than boxes so a caller can hand out the
/// selected backend without borrowing the registry — and so a backend can be
/// swapped at runtime (the daemon replaces `caelestia` with a recording stub in
/// its routing tests, and a future `shell.backend` override will do the same).
pub struct Registry {
    backends: Vec<Arc<dyn ShellBackend>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("backends", &self.ids())
            .finish()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    /// Add a backend. Ids are unique; registering a duplicate replaces it, which
    /// keeps tests and future overrides simple.
    pub fn register(&mut self, backend: Arc<dyn ShellBackend>) {
        let id = backend.id();
        self.backends.retain(|existing| existing.id() != id);
        self.backends.push(backend);
    }

    /// Every registered id, in registration order.
    pub fn ids(&self) -> Vec<&'static str> {
        self.backends.iter().map(|backend| backend.id()).collect()
    }

    /// Whether an id is registered.
    pub fn contains(&self, id: &str) -> bool {
        self.backends.iter().any(|backend| backend.id() == id)
    }

    /// Look up a backend by id.
    pub fn get(&self, id: &str) -> Option<Arc<dyn ShellBackend>> {
        self.backends
            .iter()
            .find(|backend| backend.id() == id)
            .map(Arc::clone)
    }

    /// Choose the backend to use for this session.
    ///
    /// `auto` walks `shell.detect_order` and takes the first registered backend
    /// that detects itself; an explicit id must exist, and if it does not detect
    /// itself the daemon still uses it (the user asked for it, so the honest
    /// failure belongs at first use, not at startup).
    pub fn select(
        &self,
        config: &ShellConfig,
        env: EnvLookup<'_>,
    ) -> Result<Arc<dyn ShellBackend>, SelectError> {
        self.select_with_reason(config, env)
            .map(|(backend, _)| backend)
    }

    /// [`Registry::select`], plus why the backend was chosen.
    ///
    /// The reason is what makes a surprising `auto` pick explainable after the
    /// fact — and it is what the GUI's status card and `shell.status` show.
    pub fn select_with_reason(
        &self,
        config: &ShellConfig,
        env: EnvLookup<'_>,
    ) -> Result<(Arc<dyn ShellBackend>, Detection), SelectError> {
        let requested = config.backend.trim();
        if requested.is_empty() || requested == "auto" {
            for (backend, detection) in self.detections(config, env) {
                if detection.is_detected() {
                    return Ok((backend, detection));
                }
            }
            return Err(SelectError::NoBackendDetected {
                tried: self.detect_order(config).join(", "),
            });
        }

        let backend = self
            .get(requested)
            .ok_or_else(|| SelectError::UnknownBackend {
                requested: requested.to_string(),
                available: self.ids().join(", "),
            })?;
        // Explicitly asked for: the answer is `Strong` by instruction, not by
        // evidence, and the reason says exactly that. An explicit backend that
        // does not detect itself is still used — see [`Registry::select`].
        let described = backend.detection(env);
        let reason = if described.is_detected() {
            format!("selected by config: {}", described.reason)
        } else {
            format!(
                "selected by config even though it does not detect itself here ({})",
                described.reason
            )
        };
        Ok((
            backend,
            Detection {
                confidence: described.confidence,
                reason,
            },
        ))
    }

    /// The `auto` chain for this config, in order.
    pub fn detect_order(&self, config: &ShellConfig) -> Vec<String> {
        if config.detect_order.is_empty() {
            default_detect_order()
        } else {
            config.detect_order.clone()
        }
    }

    /// Ask every registered backend what it thinks of this session, in the order
    /// `auto` would consider them.
    ///
    /// This is the whole answer to "why did it pick that?": the chain, each
    /// backend's confidence, and the sentence behind it. Backends outside the
    /// configured chain are still reported (a user debugging an explicit
    /// `shell.backend` needs to see the one they named), appended in registration
    /// order.
    pub fn detections(
        &self,
        config: &ShellConfig,
        env: EnvLookup<'_>,
    ) -> Vec<(Arc<dyn ShellBackend>, Detection)> {
        let mut seen: Vec<&'static str> = Vec::new();
        let mut rows: Vec<(Arc<dyn ShellBackend>, Detection)> = Vec::new();

        for id in self.detect_order(config) {
            if let Some(backend) = self.get(&id) {
                seen.push(backend.id());
                let detection = backend.detection(env);
                rows.push((backend, detection));
            }
        }
        for backend in self
            .backends
            .iter()
            .filter(|backend| !seen.contains(&backend.id()))
        {
            let detection = backend.detection(env);
            rows.push((Arc::clone(backend), detection));
        }
        rows
    }
}

/// Detection order used when the config does not set one.
///
/// The generic layer-shell backend is last because it always "detects" itself —
/// it is the floor, not a preference.
pub fn default_detect_order() -> Vec<String> {
    vec![
        "caelestia".to_string(),
        "hyprland".to_string(),
        "generic-layer-shell".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeBackend {
        id: &'static str,
        detects: bool,
        outputs: Vec<OutputInfo>,
    }

    impl FakeBackend {
        fn new(id: &'static str, detects: bool) -> Self {
            Self {
                id,
                detects,
                outputs: Vec::new(),
            }
        }
    }

    impl ShellBackend for FakeBackend {
        fn id(&self) -> &'static str {
            self.id
        }
        fn detect(&self, _env: EnvLookup<'_>) -> bool {
            self.detects
        }
        fn list_outputs(&self) -> Result<Vec<OutputInfo>, ShellError> {
            Ok(self.outputs.clone())
        }
    }

    fn env_with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    fn config(backend: &str, order: &[&str]) -> ShellConfig {
        let mut shell = ShellConfig {
            backend: backend.to_string(),
            ..ShellConfig::default()
        };
        if !order.is_empty() {
            shell.detect_order = order.iter().map(|id| (*id).to_string()).collect();
        }
        shell
    }

    fn registry(backends: &[(&'static str, bool)]) -> Registry {
        let mut registry = Registry::new();
        for (id, detects) in backends {
            registry.register(std::sync::Arc::new(FakeBackend::new(id, *detects)));
        }
        registry
    }

    #[test]
    fn ids_are_reported_in_registration_order() {
        let registry = registry(&[("hyprland", true), ("generic-layer-shell", true)]);
        assert_eq!(registry.ids(), vec!["hyprland", "generic-layer-shell"]);
        assert!(registry.contains("hyprland"));
        assert!(!registry.contains("caelestia"));
    }

    #[test]
    fn registering_the_same_id_twice_keeps_one_backend() {
        let mut registry = Registry::new();
        registry.register(std::sync::Arc::new(FakeBackend::new("hyprland", true)));
        registry.register(std::sync::Arc::new(FakeBackend::new("hyprland", false)));
        assert_eq!(registry.ids(), vec!["hyprland"]);
        assert!(!registry.get("hyprland").unwrap().detect(&|_| None));
    }

    #[test]
    fn explicit_backend_is_selected_by_id() {
        let registry = registry(&[("hyprland", false), ("generic-layer-shell", true)]);
        let selected = registry
            .select(&config("generic-layer-shell", &[]), &env_with(&[]))
            .unwrap();
        assert_eq!(selected.id(), "generic-layer-shell");
    }

    #[test]
    fn explicit_unknown_backend_is_an_error_not_a_fallback() {
        let registry = registry(&[("hyprland", true)]);
        let error = registry
            .select(&config("kde", &[]), &env_with(&[]))
            .err()
            .expect("an unknown backend must not be selected");
        match &error {
            SelectError::UnknownBackend {
                requested,
                available,
            } => {
                assert_eq!(requested, "kde");
                assert_eq!(available, "hyprland");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn auto_walks_the_configured_detect_order() {
        // caelestia is not registered (that is P3), so hyprland must win.
        let registry = registry(&[("hyprland", true), ("generic-layer-shell", true)]);
        let shell = config("auto", &["caelestia", "hyprland", "generic-layer-shell"]);
        let selected = registry.select(&shell, &env_with(&[])).unwrap();
        assert_eq!(selected.id(), "hyprland");
    }

    #[test]
    fn auto_prefers_an_earlier_backend_that_detects_itself() {
        let registry = registry(&[("hyprland", false), ("generic-layer-shell", true)]);
        let shell = config("auto", &["hyprland", "generic-layer-shell"]);
        let selected = registry.select(&shell, &env_with(&[])).unwrap();
        assert_eq!(selected.id(), "generic-layer-shell");
    }

    #[test]
    fn auto_with_nothing_detecting_reports_what_was_tried() {
        let registry = registry(&[("hyprland", false)]);
        let shell = config("auto", &["hyprland", "generic-layer-shell"]);
        let error = registry
            .select(&shell, &env_with(&[]))
            .err()
            .expect("nothing detects itself, so selection must fail");
        match &error {
            SelectError::NoBackendDetected { tried } => {
                assert!(tried.contains("hyprland"), "{tried}");
                assert!(tried.contains("generic-layer-shell"), "{tried}");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(error.to_string().contains("shell.backend"), "{error}");
    }

    #[test]
    fn auto_uses_the_builtin_order_when_the_config_leaves_it_empty() {
        assert_eq!(
            default_detect_order(),
            vec!["caelestia", "hyprland", "generic-layer-shell"]
        );
        let registry = registry(&[("generic-layer-shell", true), ("hyprland", false)]);
        let shell = config("auto", &[]);
        assert_eq!(
            registry.select(&shell, &env_with(&[])).unwrap().id(),
            "generic-layer-shell"
        );
    }

    #[test]
    fn an_explicit_backend_is_used_even_when_it_does_not_detect_itself() {
        // The user asked for it; failing at first use with a real error beats
        // silently switching their compositor.
        let registry = registry(&[("hyprland", false)]);
        let selected = registry
            .select(&config("hyprland", &[]), &env_with(&[]))
            .unwrap();
        assert_eq!(selected.id(), "hyprland");
    }

    #[test]
    fn empty_backend_string_behaves_like_auto() {
        let registry = registry(&[("hyprland", true)]);
        assert_eq!(
            registry
                .select(&config("", &[]), &env_with(&[]))
                .unwrap()
                .id(),
            "hyprland"
        );
    }
}
