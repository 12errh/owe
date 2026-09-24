//! Configuration schema, defaults, and validation (docs/BACKEND-DESIGN.md §4).
//!
//! Rules that matter:
//!
//! - **Unknown keys are errors.** Every table denies unknown fields, because a
//!   typo'd key silently doing nothing is the worst possible behaviour for a
//!   config file that controls an always-running daemon.
//! - **All problems are reported at once** ([`ConfigError::Invalid`]) so a user
//!   fixes one round of mistakes instead of ten.
//! - **Path syntax is validated, existence is not.** Wallpapers may live on
//!   removable or network mounts; checking existence at load time would make the
//!   daemon fail to start when a drive is unplugged. The P2 scanner warns.
//! - Backend/transition/action ids are validated against fixed lists here; P3
//!   replaces the shell-backend check with the live registry (docs ADR note).

use std::collections::BTreeMap;
use std::path::Path;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;
use crate::model::WallpaperRef;
use crate::path::expand;

/// Config schema version supported by this build.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// Values accepted by `shell.backend`. `auto` resolves via `shell.detect_order`.
pub const KNOWN_SHELL_BACKENDS: &[&str] = &["auto", "caelestia", "hyprland", "generic-layer-shell"];

/// Values accepted inside `shell.detect_order` (`auto` is not a detection target).
pub const KNOWN_DETECT_IDS: &[&str] = &["caelestia", "hyprland", "generic-layer-shell"];

/// Cache compression codecs (P4).
pub const KNOWN_COMPRESSION: &[&str] = &["zstd", "lz4", "none"];

/// Video decode backends (P4).
pub const KNOWN_MEDIA_BACKENDS: &[&str] = &["auto", "gstreamer", "ffmpeg"];

/// Built-in transition names. The gl-transitions catalogue joins this list in P2.
pub const KNOWN_TRANSITIONS: &[&str] = &["none", "fade", "wipe", "slide", "grow", "wave", "outer"];

/// Fit modes the render path implements (TRD FR-LIVE-4 / NFR-UX-2).
///
/// The names are the TRD's words, and every one of them is implemented rather
/// than aliased: `fill` crops to cover, `fit` letterboxes, `center` draws at the
/// source's own size in the middle, `stretch` distorts. A list of spellings that
/// all meant `cover` would be a config surface lying about its own behaviour.
pub const KNOWN_FIT_MODES: &[&str] = &["fill", "fit", "center", "stretch"];

/// Caelestia integration modes (P3).
pub const KNOWN_CAELESTIA_MODES: &[&str] = &["shell-routed", "daemon-drawn"];

/// Policies for an already-running hyprpaper (TRD §4: coexistence, not conquest).
pub const KNOWN_HYPRPAPER_POLICIES: &[&str] = &["warn", "stop", "ignore"];

/// Bounds on `render.buffering.max_in_flight` (TRD NFR-RES-1).
pub const MAX_IN_FLIGHT: std::ops::RangeInclusive<u32> = 1..=8;

/// Bounds on any FPS cap in the config.
pub const FPS_RANGE: std::ops::RangeInclusive<u32> = 1..=240;

/// Upper bound on transition duration (a transition longer than a minute is a typo).
pub const MAX_TRANSITION_MS: u64 = 60_000;

/// The whole configuration file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Schema version; must equal [`SUPPORTED_SCHEMA`].
    pub schema: u32,
    /// Desktop-environment integration (backend selection, detection order).
    pub shell: ShellConfig,
    /// Rendering behaviour (transitions, buffering).
    pub render: RenderConfig,
    /// Media decoding (backend choice, caches).
    pub media: MediaConfig,
    /// Wallpaper library scanning.
    pub library: LibraryConfig,
    /// The resource governor: when to pause, dim, or slow down.
    pub governor: GovernorConfig,
    /// Per-output overrides, keyed by output name, description, or `any`.
    pub outputs: BTreeMap<String, OutputConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: SUPPORTED_SCHEMA,
            shell: ShellConfig::default(),
            render: RenderConfig::default(),
            media: MediaConfig::default(),
            library: LibraryConfig::default(),
            governor: GovernorConfig::default(),
            outputs: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Load and validate a configuration file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let config = Self::from_toml_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    /// Parse and validate configuration from a TOML string.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)
            .map_err(|e| ConfigError::Toml(e.to_string().trim().to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Serialize back to TOML (used by `config.get` and by round-trip tests).
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|e| ConfigError::Toml(e.to_string()))
    }

    /// Validate every rule; all problems are collected before returning.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema != SUPPORTED_SCHEMA {
            return Err(ConfigError::UnsupportedSchema {
                found: u64::from(self.schema),
                supported: SUPPORTED_SCHEMA,
            });
        }

        let mut problems = Vec::new();
        self.shell.validate(&mut problems);
        self.render.validate(&mut problems);
        self.media.validate(&mut problems);
        self.library.validate(&mut problems);
        self.governor.validate(&mut problems);
        for (name, output) in &self.outputs {
            output.validate(name, &mut problems);
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(problems))
        }
    }
}

/// `[shell]` — desktop-environment integration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ShellConfig {
    /// Backend id, or `auto` to use [`ShellConfig::detect_order`].
    pub backend: String,
    /// Backend ids probed in order when `backend = "auto"`.
    pub detect_order: Vec<String>,
    /// Hyprland-specific integration.
    pub hyprland: HyprlandConfig,
    /// Caelestia Shell integration.
    pub caelestia: CaelestiaConfig,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            backend: "auto".to_string(),
            detect_order: KNOWN_DETECT_IDS.iter().map(|s| (*s).to_string()).collect(),
            hyprland: HyprlandConfig::default(),
            caelestia: CaelestiaConfig::default(),
        }
    }
}

impl ShellConfig {
    fn validate(&self, problems: &mut Vec<String>) {
        one_of(
            problems,
            "shell.backend",
            &self.backend,
            KNOWN_SHELL_BACKENDS,
        );

        if self.detect_order.is_empty() {
            problems.push("shell.detect_order: must list at least one backend id".to_string());
        }
        for id in &self.detect_order {
            one_of(problems, "shell.detect_order[]", id, KNOWN_DETECT_IDS);
        }
        for (i, id) in self.detect_order.iter().enumerate() {
            if self.detect_order[i + 1..].contains(id) {
                problems.push(format!("shell.detect_order: `{id}` is listed twice"));
            }
        }

        self.hyprland.validate(problems);
        self.caelestia.validate(problems);
    }
}

/// `[shell.hyprland]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HyprlandConfig {
    /// What to do when a hyprpaper process is detected: `warn`, `stop` or `ignore`.
    pub hyprpaper: String,
    /// Subscribe to the Hyprland event socket (feeds the P6 governor).
    pub event_socket: bool,
}

impl Default for HyprlandConfig {
    fn default() -> Self {
        Self {
            hyprpaper: "warn".to_string(),
            event_socket: true,
        }
    }
}

impl HyprlandConfig {
    fn validate(&self, problems: &mut Vec<String>) {
        one_of(
            problems,
            "shell.hyprland.hyprpaper",
            &self.hyprpaper,
            KNOWN_HYPRPAPER_POLICIES,
        );
    }
}

/// `[shell.caelestia]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CaelestiaConfig {
    /// `shell-routed` (let Caelestia set the wallpaper) or `daemon-drawn`.
    pub mode: String,
    /// Wallpapers directory; defaults to the shell's own variable.
    pub wallpapers_dir: Option<String>,
    /// Let Caelestia run its theming pipeline when the wallpaper changes.
    pub theme_hook: bool,
}

impl Default for CaelestiaConfig {
    fn default() -> Self {
        Self {
            mode: "shell-routed".to_string(),
            wallpapers_dir: Some("$CAELESTIA_WALLPAPERS_DIR".to_string()),
            theme_hook: true,
        }
    }
}

impl CaelestiaConfig {
    fn validate(&self, problems: &mut Vec<String>) {
        one_of(
            problems,
            "shell.caelestia.mode",
            &self.mode,
            KNOWN_CAELESTIA_MODES,
        );
        if let Some(dir) = &self.wallpapers_dir
            && let Err(err) = expand(dir)
        {
            problems.push(format!("shell.caelestia.wallpapers_dir: {err}"));
        }
    }
}

/// `[render]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RenderConfig {
    /// Transition used when a wallpaper changes and nothing else is specified.
    pub default_transition: Transition,
    /// Transition ids a user is allowed to select.
    pub allow_transitions: Vec<String>,
    /// How content is fitted onto an output; see [`KNOWN_FIT_MODES`].
    pub fit: String,
    /// Buffer-pool limits.
    pub buffering: BufferingConfig,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            default_transition: Transition::default(),
            allow_transitions: KNOWN_TRANSITIONS.iter().map(|s| (*s).to_string()).collect(),
            fit: "fill".to_string(),
            buffering: BufferingConfig::default(),
        }
    }
}

impl RenderConfig {
    fn validate(&self, problems: &mut Vec<String>) {
        one_of(problems, "render.fit", &self.fit, KNOWN_FIT_MODES);
        if self.allow_transitions.is_empty() {
            problems.push("render.allow_transitions: must allow at least `none`".to_string());
        }
        for name in &self.allow_transitions {
            one_of(
                problems,
                "render.allow_transitions[]",
                name,
                KNOWN_TRANSITIONS,
            );
        }
        self.default_transition
            .validate("render.default_transition", problems);
        if !self
            .allow_transitions
            .contains(&self.default_transition.name)
        {
            problems.push(format!(
                "render.default_transition.name: `{}` is not in render.allow_transitions",
                self.default_transition.name
            ));
        }

        if !MAX_IN_FLIGHT.contains(&self.buffering.max_in_flight) {
            problems.push(format!(
                "render.buffering.max_in_flight: {} is out of range {}..={}",
                self.buffering.max_in_flight,
                MAX_IN_FLIGHT.start(),
                MAX_IN_FLIGHT.end()
            ));
        }
    }
}

/// A wallpaper transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Transition {
    /// Transition id; see [`KNOWN_TRANSITIONS`].
    pub name: String,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Transition frame rate.
    pub fps: u32,
}

impl Default for Transition {
    fn default() -> Self {
        Self {
            name: "fade".to_string(),
            duration_ms: 300,
            fps: 60,
        }
    }
}

impl Transition {
    fn validate(&self, key: &str, problems: &mut Vec<String>) {
        one_of(
            problems,
            &format!("{key}.name"),
            &self.name,
            KNOWN_TRANSITIONS,
        );
        if self.duration_ms > MAX_TRANSITION_MS {
            problems.push(format!(
                "{key}.duration_ms: {} exceeds the {MAX_TRANSITION_MS} ms maximum",
                self.duration_ms
            ));
        }
        if !FPS_RANGE.contains(&self.fps) {
            problems.push(format!(
                "{key}.fps: {} is out of range {}..={}",
                self.fps,
                FPS_RANGE.start(),
                FPS_RANGE.end()
            ));
        }
    }
}

/// `[render.buffering]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BufferingConfig {
    /// Hard cap on frames in flight per output (TRD NFR-RES-1).
    pub max_in_flight: u32,
    /// Allow the shared-memory presentation fallback (always compiled in).
    pub shm_fallback: bool,
}

impl Default for BufferingConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 3,
            shm_fallback: true,
        }
    }
}

/// `[media]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MediaConfig {
    /// `auto`, `gstreamer`, or `ffmpeg`.
    pub backend: String,
    /// Refuse software decode (loudly) when hardware decode is unavailable.
    pub hw_decode_required: bool,
    /// Decode caches.
    pub cache: MediaCacheConfig,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            backend: "auto".to_string(),
            hw_decode_required: false,
            cache: MediaCacheConfig::default(),
        }
    }
}

impl MediaConfig {
    fn validate(&self, problems: &mut Vec<String>) {
        one_of(
            problems,
            "media.backend",
            &self.backend,
            KNOWN_MEDIA_BACKENDS,
        );
        self.cache.validate(problems);
    }
}

/// `[media.cache]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MediaCacheConfig {
    /// Per-wallpaper cap on cached animated frames, in MiB.
    pub animated_frame_cap_mb: u32,
    /// Frame-cache compression: `zstd`, `lz4`, or `none`.
    pub compression: String,
}

impl Default for MediaCacheConfig {
    fn default() -> Self {
        Self {
            animated_frame_cap_mb: 96,
            compression: "zstd".to_string(),
        }
    }
}

impl MediaCacheConfig {
    fn validate(&self, problems: &mut Vec<String>) {
        const RANGE: std::ops::RangeInclusive<u32> = 1..=4096;
        if !RANGE.contains(&self.animated_frame_cap_mb) {
            problems.push(format!(
                "media.cache.animated_frame_cap_mb: {} is out of range {}..={}",
                self.animated_frame_cap_mb,
                RANGE.start(),
                RANGE.end()
            ));
        }
        one_of(
            problems,
            "media.cache.compression",
            &self.compression,
            KNOWN_COMPRESSION,
        );
    }
}

/// `[library]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LibraryConfig {
    /// Directories scanned for wallpapers (`~` and `$VARS` allowed).
    pub paths: Vec<String>,
    /// Thumbnail edge length in pixels (P2).
    pub thumbnail_size: u32,
    /// Seconds between background rescans; 0 means "only on IPC/FS events".
    pub rescan_interval_secs: u64,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            paths: vec!["~/Pictures/Wallpapers".to_string()],
            thumbnail_size: 512,
            rescan_interval_secs: 0,
        }
    }
}

impl LibraryConfig {
    fn validate(&self, problems: &mut Vec<String>) {
        if self.paths.is_empty() {
            problems.push("library.paths: must list at least one directory".to_string());
        }
        for (i, raw) in self.paths.iter().enumerate() {
            if let Err(err) = expand(raw) {
                problems.push(format!("library.paths[{i}]: {err}"));
            }
        }
        const THUMB: std::ops::RangeInclusive<u32> = 32..=4096;
        if !THUMB.contains(&self.thumbnail_size) {
            problems.push(format!(
                "library.thumbnail_size: {} is out of range {}..={}",
                self.thumbnail_size,
                THUMB.start(),
                THUMB.end()
            ));
        }
        if self.rescan_interval_secs > 86_400 {
            problems.push(format!(
                "library.rescan_interval_secs: {} exceeds one day (86400)",
                self.rescan_interval_secs
            ));
        }
    }
}

/// `[governor]` — the resource governor (P6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GovernorConfig {
    /// What happens on each output when a fullscreen window appears.
    pub fullscreen: FullscreenRules,
    /// Battery-driven policies.
    pub battery: BatteryRules,
    /// Policy when an output's display is off (DPMS).
    pub dpms_off: PolicyAction,
    /// Idle behaviour.
    pub idle: IdleRule,
    /// Opt-in CPU-busy backoff.
    pub cpu_busy: CpuBusyRule,
    /// One switch that applies the bundled conservative preset.
    pub eco_mode: bool,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            fullscreen: FullscreenRules::default(),
            battery: BatteryRules::default(),
            dpms_off: PolicyAction::Pause,
            idle: IdleRule::default(),
            cpu_busy: CpuBusyRule::default(),
            eco_mode: false,
        }
    }
}

impl GovernorConfig {
    fn validate(&self, problems: &mut Vec<String>) {
        validate_policy(
            problems,
            "governor.fullscreen.focused",
            &self.fullscreen.focused,
        );
        validate_policy(
            problems,
            "governor.fullscreen.others",
            &self.fullscreen.others,
        );
        validate_policy(
            problems,
            "governor.battery.on_battery",
            &self.battery.on_battery,
        );
        validate_policy(problems, "governor.dpms_off", &self.dpms_off);

        if let Some(pct) = self.battery.below_pct {
            if !(1..=100).contains(&pct) {
                problems.push(format!(
                    "governor.battery.below_pct: {pct} is out of range 1..=100"
                ));
            }
        }
        if let Some(fps) = self.battery.fps_on_battery
            && !FPS_RANGE.contains(&fps)
        {
            problems.push(format!(
                "governor.battery.fps_on_battery: {fps} is out of range {}..={}",
                FPS_RANGE.start(),
                FPS_RANGE.end()
            ));
        }

        if self.idle.action != IdleAction::None && self.idle.after_secs == 0 {
            problems.push(
                "governor.idle.after_secs: must be at least 1 when governor.idle.action is set"
                    .to_string(),
            );
        }
        if self.cpu_busy.action != IdleAction::None {
            if self.cpu_busy.pct == 0 {
                problems.push(
                    "governor.cpu_busy.pct: must be 1..=100 when governor.cpu_busy.action is set"
                        .to_string(),
                );
            }
            if self.cpu_busy.for_secs == 0 {
                problems.push(
                    "governor.cpu_busy.for_secs: must be at least 1 when governor.cpu_busy.action is set"
                        .to_string(),
                );
            }
        } else if self.cpu_busy.pct > 0 {
            problems.push(
                "governor.cpu_busy.pct: is set but governor.cpu_busy.action is `none`".to_string(),
            );
        }
    }
}

/// `[governor.fullscreen]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FullscreenRules {
    /// Policy for the output that has focus.
    pub focused: PolicyAction,
    /// Policy for outputs without focus.
    pub others: PolicyAction,
}

impl Default for FullscreenRules {
    fn default() -> Self {
        Self {
            focused: PolicyAction::Pause,
            others: PolicyAction::Ignore,
        }
    }
}

/// `[governor.battery]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BatteryRules {
    /// Policy applied while on battery power.
    pub on_battery: PolicyAction,
    /// Also pause while on battery below this charge percentage.
    pub below_pct: Option<u8>,
    /// FPS cap applied while on battery.
    pub fps_on_battery: Option<u32>,
}

impl Default for BatteryRules {
    fn default() -> Self {
        Self {
            on_battery: PolicyAction::Ignore,
            below_pct: None,
            fps_on_battery: None,
        }
    }
}

/// `[governor.idle]`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IdleRule {
    /// Seconds of user idle before [`IdleRule::action`] applies; 0 disables.
    pub after_secs: u64,
    /// What to do once idle.
    pub action: IdleAction,
}

impl Default for IdleRule {
    fn default() -> Self {
        Self {
            after_secs: 0,
            action: IdleAction::None,
        }
    }
}

/// Action taken when a governor rule fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum IdleAction {
    /// Do nothing (rule disabled).
    #[default]
    None,
    /// Freeze the current frame (cheapest option for animated content).
    Static,
    /// Stop producing frames entirely.
    Pause,
}

/// `[governor.cpu_busy]`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CpuBusyRule {
    /// CPU utilisation percentage that counts as "busy".
    pub pct: u8,
    /// How long it must stay busy before the action applies.
    pub for_secs: u64,
    /// What to do while busy.
    pub action: IdleAction,
}

impl Default for CpuBusyRule {
    fn default() -> Self {
        Self {
            pct: 0,
            for_secs: 0,
            action: IdleAction::None,
        }
    }
}

/// A per-output section such as `[outputs."DP-1"]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OutputConfig {
    /// Wallpaper reference (path, `library:<id>`, or `shader:<name>`).
    pub wallpaper: Option<String>,
    /// FPS cap for this output.
    pub fps_cap: Option<u32>,
    /// Transition override for this output.
    pub transition: Option<Transition>,
}

impl OutputConfig {
    fn validate(&self, section: &str, problems: &mut Vec<String>) {
        if let Some(wallpaper) = &self.wallpaper {
            if let Err(err) = WallpaperRef::parse(wallpaper) {
                problems.push(format!("outputs.\"{section}\".wallpaper: {err}"));
            }
        }
        if let Some(fps) = self.fps_cap
            && !FPS_RANGE.contains(&fps)
        {
            problems.push(format!(
                "outputs.\"{section}\".fps_cap: {fps} is out of range {}..={}",
                FPS_RANGE.start(),
                FPS_RANGE.end()
            ));
        }
        if let Some(transition) = &self.transition {
            transition.validate(&format!("outputs.\"{section}\".transition"), problems);
        }
    }
}

/// A governor action: pause, dim, ignore, or re-cap the frame rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PolicyAction {
    /// Stop producing frames.
    Pause,
    /// Keep drawing but fade the surface down (implemented in P6).
    Dim,
    /// Do nothing. The default: governor rules are opt-in.
    #[default]
    Ignore,
    /// Keep drawing at (at most) this frame rate.
    Fps(u32),
}

impl PolicyAction {
    /// Short id, for logs and the GUI.
    pub fn as_str(&self) -> String {
        match self {
            PolicyAction::Pause => "pause".to_string(),
            PolicyAction::Dim => "dim".to_string(),
            PolicyAction::Ignore => "ignore".to_string(),
            PolicyAction::Fps(n) => format!("fps({n})"),
        }
    }
}

impl Serialize for PolicyAction {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            PolicyAction::Pause => serializer.serialize_str("pause"),
            PolicyAction::Dim => serializer.serialize_str("dim"),
            PolicyAction::Ignore => serializer.serialize_str("ignore"),
            PolicyAction::Fps(n) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("fps", n)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PolicyAction {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = toml::Value::deserialize(deserializer)?;
        match value {
            toml::Value::String(name) => match name.as_str() {
                "pause" => Ok(PolicyAction::Pause),
                "dim" => Ok(PolicyAction::Dim),
                "ignore" => Ok(PolicyAction::Ignore),
                other => Err(D::Error::custom(format!(
                    "unknown policy action `{other}` (expected `pause`, `dim`, `ignore`, \
                     or a table like `{{ fps = 30 }}`)"
                ))),
            },
            toml::Value::Table(table) => {
                if let Some(extra) = table.keys().find(|key| key.as_str() != "fps") {
                    return Err(D::Error::custom(format!(
                        "unknown key `{extra}` in policy action (only `fps` is allowed)"
                    )));
                }
                match table.get("fps") {
                    // Range checking lives in `validate_policy`, not here, so that
                    // every range problem is reported together with the rest of the
                    // config (consistent with `Transition`).
                    Some(toml::Value::Integer(n)) if *n >= 0 => Ok(PolicyAction::Fps(*n as u32)),
                    Some(toml::Value::Integer(n)) => Err(D::Error::custom(format!(
                        "policy `fps` {n} must not be negative"
                    ))),
                    Some(_) => Err(D::Error::custom("policy `fps` must be an integer")),
                    None => Err(D::Error::custom(
                        "policy action table must contain `fps` (e.g. `{ fps = 30 }`)",
                    )),
                }
            }
            other => Err(D::Error::custom(format!(
                "policy action must be a string or a `{{ fps = N }}` table, got {}",
                other.type_str()
            ))),
        }
    }
}

// ---------------------------------------------------------------- validation helpers

fn one_of(problems: &mut Vec<String>, key: &str, value: &str, allowed: &[&str]) {
    if !allowed.contains(&value) {
        problems.push(format!(
            "{key}: unknown value `{value}` (expected one of: {})",
            allowed.join(", ")
        ));
    }
}

fn validate_policy(problems: &mut Vec<String>, key: &str, action: &PolicyAction) {
    match action {
        PolicyAction::Fps(fps) if !FPS_RANGE.contains(fps) => problems.push(format!(
            "{key}: fps {fps} is out of range {}..={}",
            FPS_RANGE.start(),
            FPS_RANGE.end()
        )),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Problems for a config that must FAIL validation.
    fn errors(toml_text: &str) -> Vec<String> {
        match Config::from_toml_str(toml_text) {
            Ok(config) => panic!("expected an error, got {config:?}"),
            Err(ConfigError::Invalid(problems)) => problems,
            Err(other) => panic!("expected validation problems, got {other:?}"),
        }
    }

    /// Problems for a config that may be valid; empty means "accepted".
    fn problems(toml_text: &str) -> Vec<String> {
        match Config::from_toml_str(toml_text) {
            Ok(_) => Vec::new(),
            Err(ConfigError::Invalid(problems)) => problems,
            Err(other) => panic!("unexpected parse error: {other:?}"),
        }
    }

    fn assert_has(problems: &[String], needle: &str) {
        assert!(
            problems.iter().any(|p| p.contains(needle)),
            "no problem contained {needle:?}; got {problems:#?}"
        );
    }

    #[test]
    fn empty_config_gets_safe_defaults() {
        let config = Config::from_toml_str("").unwrap();
        assert_eq!(config.schema, SUPPORTED_SCHEMA);
        assert_eq!(config.shell.backend, "auto");
        assert_eq!(config.shell.detect_order, KNOWN_DETECT_IDS.to_vec());
        assert_eq!(config.library.thumbnail_size, 512);
        assert_eq!(config.governor.fullscreen.focused, PolicyAction::Pause);
        assert_eq!(config.governor.fullscreen.others, PolicyAction::Ignore);
        assert_eq!(config.governor.battery.on_battery, PolicyAction::Ignore);
        assert_eq!(config.governor.dpms_off, PolicyAction::Pause);
        assert_eq!(config.render.buffering.max_in_flight, 3);
        assert_eq!(config.media.backend, "auto");
        assert_eq!(config.media.cache.compression, "zstd");
        assert!(config.outputs.is_empty());
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let config = Config::default();
        let text = config.to_toml_string().unwrap();
        let parsed = Config::from_toml_str(&text).unwrap();
        assert_eq!(config, parsed);
        // The defaults must actually survive the trip, not just re-default.
        assert!(text.contains("default_transition") || text.contains("[render]"));
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        // `library` misspelled: the classic silent-failure typo (serde message).
        let err = Config::from_toml_str("[librarry]\npaths = []\n").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("unknown field"), "{text}");
        assert!(text.contains("librarry"), "{text}");
    }

    #[test]
    fn unknown_nested_key_is_rejected() {
        let err = Config::from_toml_str("[shell.hyprland]\nhyprpaperr = \"stop\"\n").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("unknown field"), "{text}");
        assert!(text.contains("hyprpaperr"), "{text}");
    }

    #[test]
    fn wrong_type_mentions_expected_type() {
        let err = Config::from_toml_str("[library]\nthumbnail_size = \"big\"\n").unwrap_err();
        assert!(err.to_string().contains("invalid type"), "{err}");
    }

    #[test]
    fn schema_mismatch_is_its_own_error() {
        let err = Config::from_toml_str("schema = 7\n").unwrap_err();
        match err {
            ConfigError::UnsupportedSchema { found, supported } => {
                assert_eq!(found, 7);
                assert_eq!(supported, SUPPORTED_SCHEMA);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_backend_lists_alternatives() {
        let problems = errors("[shell]\nbackend = \"gnome\"\n");
        assert_has(&problems, "shell.backend");
        assert_has(&problems, "auto, caelestia, hyprland, generic-layer-shell");
    }

    #[test]
    fn detect_order_rejects_auto_and_duplicates() {
        let problems = errors("[shell]\ndetect_order = [\"auto\", \"hyprland\", \"hyprland\"]\n");
        assert_has(&problems, "shell.detect_order[]");
        assert_has(&problems, "listed twice");
    }

    #[test]
    fn empty_detect_order_is_rejected() {
        let problems = errors("[shell]\ndetect_order = []\n");
        assert_has(&problems, "shell.detect_order");
    }

    #[test]
    fn hyprpaper_policy_is_validated() {
        let problems = errors("[shell.hyprland]\nhyprpaper = \"kill\"\n");
        assert_has(&problems, "shell.hyprland.hyprpaper");
        assert_has(&problems, "warn, stop, ignore");
    }

    #[test]
    fn caelestia_mode_is_validated() {
        let problems = errors("[shell.caelestia]\nmode = \"wine\"\n");
        assert_has(&problems, "shell.caelestia.mode");
    }

    #[test]
    fn caelestia_default_wallpapers_dir_survives_validation() {
        // The shell owns this variable; an unset value must not fail config load.
        let problems = problems(
            "[shell.caelestia]\nmode = \"shell-routed\"\nwallpapers_dir = \"$CAELESTIA_WALLPAPERS_DIR\"\n",
        );
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn unknown_transition_is_rejected() {
        let problems = errors("[render]\nallow_transitions = [\"fade\", \"explode\"]\n");
        assert_has(&problems, "render.allow_transitions[]");
        assert_has(&problems, "explode");
    }

    #[test]
    fn default_transition_must_be_allowed() {
        let problems = errors(
            "[render]\nallow_transitions = [\"none\"]\n[render.default_transition]\nname = \"fade\"\n",
        );
        assert_has(&problems, "is not in render.allow_transitions");
    }

    #[test]
    fn transition_ranges_are_enforced() {
        let problems =
            errors("[render.default_transition]\nname = \"fade\"\nduration_ms = 999999\nfps = 0\n");
        assert_has(&problems, "duration_ms");
        assert_has(&problems, "fps");
    }

    #[test]
    fn buffer_cap_is_enforced() {
        let problems = errors("[render.buffering]\nmax_in_flight = 99\n");
        assert_has(&problems, "max_in_flight");
        assert_has(&problems, "1..=8");
    }

    #[test]
    fn the_fit_mode_defaults_to_fill_and_rejects_a_typo() {
        // FR-LIVE-4 gives the render path four fit modes; the config key has to
        // accept exactly those and nothing else, or a typo would silently pick
        // whichever mode the renderer happened to default to.
        assert_eq!(Config::default().render.fit, "fill");
        assert_eq!(KNOWN_FIT_MODES, &["fill", "fit", "center", "stretch"]);

        let typo = errors("[render]\nfit = \"squish\"\n");
        assert_has(&typo, "render.fit");
        assert_has(&typo, "squish");

        for mode in KNOWN_FIT_MODES {
            let found = problems(&format!("[render]\nfit = \"{mode}\"\n"));
            assert!(
                found.is_empty(),
                "`{mode}` is documented and must validate: {found:?}"
            );
        }
    }

    #[test]
    fn media_backend_and_cache_are_validated() {
        let problems = errors(
            "[media]\nbackend = \"vlc\"\n[media.cache]\nanimated_frame_cap_mb = 0\ncompression = \"brotli\"\n",
        );
        assert_has(&problems, "media.backend");
        assert_has(&problems, "animated_frame_cap_mb");
        assert_has(&problems, "media.cache.compression");
    }

    #[test]
    fn library_path_must_be_absolute() {
        let problems = errors("[library]\npaths = [\"Pictures/Wallpapers\"]\n");
        assert_has(&problems, "library.paths[0]");
        assert_has(&problems, "must be absolute");
    }

    #[test]
    fn library_path_reports_unset_variable_by_name() {
        let problems = errors("[library]\npaths = [\"$DEFINITELY_NOT_SET_OWE/x\"]\n");
        assert_has(&problems, "DEFINITELY_NOT_SET_OWE");
    }

    #[test]
    fn empty_library_paths_is_rejected() {
        let problems = errors("[library]\npaths = []\n");
        assert_has(&problems, "library.paths");
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let problems = errors(
            "[shell]\nbackend = \"gnome\"\n[library]\nthumbnail_size = 1\n[render.buffering]\nmax_in_flight = 0\n",
        );
        assert!(
            problems.len() >= 3,
            "expected 3+ problems, got {problems:#?}"
        );
    }

    #[test]
    fn policy_actions_accept_strings_and_fps_tables() {
        let config = Config::from_toml_str(
            "[governor]\ndpms_off = \"dim\"\n[governor.fullscreen]\nfocused = { fps = 30 }\nothers = \"ignore\"\n",
        )
        .unwrap();
        assert_eq!(config.governor.dpms_off, PolicyAction::Dim);
        assert_eq!(config.governor.fullscreen.focused, PolicyAction::Fps(30));
        assert_eq!(config.governor.fullscreen.others, PolicyAction::Ignore);
    }

    #[test]
    fn policy_action_rejects_unknown_names_and_keys() {
        let err = Config::from_toml_str("[governor]\ndpms_off = \"sleep\"\n").unwrap_err();
        assert!(err.to_string().contains("unknown policy action"), "{err}");

        let err = Config::from_toml_str("[governor]\ndpms_off = { fsp = 30 }\n").unwrap_err();
        assert!(err.to_string().contains("unknown key"), "{err}");

        let err = Config::from_toml_str("[governor]\ndpms_off = { fps = 9999 }\n").unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn policy_action_round_trips() {
        for action in [
            PolicyAction::Pause,
            PolicyAction::Dim,
            PolicyAction::Ignore,
            PolicyAction::Fps(45),
        ] {
            let mut config = Config::default();
            config.governor.dpms_off = action;
            let text = config.to_toml_string().unwrap();
            let back = Config::from_toml_str(&text).unwrap();
            assert_eq!(back.governor.dpms_off, action, "text was:\n{text}");
        }
        assert_eq!(PolicyAction::Fps(45).as_str(), "fps(45)");
    }

    #[test]
    fn idle_rule_requires_a_duration_when_enabled() {
        let problems = errors("[governor.idle]\naction = \"static\"\n");
        assert_has(&problems, "governor.idle.after_secs");
    }

    #[test]
    fn cpu_busy_rule_cross_checks_its_fields() {
        let found = errors("[governor.cpu_busy]\npct = 80\nfor_secs = 0\n");
        assert_has(&found, "governor.cpu_busy.pct: is set but");

        let accepted =
            problems("[governor.cpu_busy]\npct = 80\nfor_secs = 5\naction = \"pause\"\n");
        assert!(accepted.is_empty(), "{accepted:#?}");
    }

    #[test]
    fn battery_rules_are_range_checked() {
        let problems = errors(
            "[governor.battery]\nbelow_pct = 0\nfps_on_battery = 999\non_battery = { fps = 0 }\n",
        );
        assert_has(&problems, "below_pct");
        assert_has(&problems, "fps_on_battery");
        assert_has(&problems, "governor.battery.on_battery");
    }

    #[test]
    fn output_sections_are_validated() {
        let problems = errors(
            "[outputs.\"DP-1\"]\nwallpaper = \"library:\"\nfps_cap = 400\n[outputs.\"DP-1\".transition]\nname = \"explode\"\n",
        );
        assert_has(&problems, "library:");
        assert_has(&problems, "fps_cap");
        assert_has(&problems, "transition.name");
    }

    #[test]
    fn output_sections_accept_every_reference_form() {
        let problems = problems(
            "[outputs.\"DP-1\"]\nwallpaper = \"/data/wall.png\"\n[outputs.\"any\"]\nwallpaper = \"shader:aurora\"\n[outputs.\"eDP-1\"]\nwallpaper = \"library:abc123\"\nfps_cap = 30\n",
        );
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn valid_full_config_produces_no_errors() {
        let config = Config::from_toml_str(
            r#"
schema = 1

[shell]
backend = "auto"
detect_order = ["caelestia", "hyprland", "generic-layer-shell"]

[shell.hyprland]
hyprpaper = "warn"
event_socket = true

[render]
allow_transitions = ["none", "fade", "wipe"]
[render.default_transition]
name = "fade"
duration_ms = 250
fps = 60
[render.buffering]
max_in_flight = 2

[media]
backend = "auto"
[media.cache]
animated_frame_cap_mb = 64
compression = "lz4"

[library]
paths = ["~/Pictures/Wallpapers"]
thumbnail_size = 256

[governor]
eco_mode = true
dpms_off = "pause"
[governor.idle]
after_secs = 600
action = "static"

[outputs."DP-1"]
wallpaper = "/data/wall.png"
fps_cap = 30
"#,
        )
        .unwrap();
        assert!(config.governor.eco_mode);
        assert_eq!(config.outputs.len(), 1);
    }

    #[test]
    fn shipped_example_config_is_valid() {
        // Docs drift is a bug: the example in docs/ must always load.
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/config.toml");
        let config = Config::load(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        assert_eq!(config.schema, SUPPORTED_SCHEMA);
    }
}
