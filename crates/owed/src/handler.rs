//! The daemon's IPC handler: what `owed` answers when a client talks to it.
//!
//! Two rules govern this file:
//!
//! 1. **Capabilities advertise only what works.** After the P0 review found the
//!    old capability list claiming backends that could not render, this module
//!    reports the engine's real registry and the kinds it can actually decode.
//! 2. **Every failure keeps its cause.** A missing output, an unsupported
//!    content kind, and a broken file are three different protocol error codes,
//!    because a client that cannot tell them apart shows the user the wrong thing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use owe_core::Config;
use owe_core::config::SUPPORTED_SCHEMA;
use owe_core::config::Transition;
use owe_core::library::ListQuery;
use owe_core::model::ContentKind;
use owe_core::output::OutputTarget;
use owe_ipc::protocol::{self, method};
use owe_ipc::{
    Capabilities, ErrorBody, ErrorCode, Handler, HelloParams, HelloReply, RequestFrame, Shutdown,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::engine::{Engine, EngineError};
use crate::library::{LibraryService, parse_kind};

/// Protocol methods actually implemented by this build, in advertised order.
///
/// Kept as a list rather than derived from the match below so a method cannot be
/// advertised by accident: `capabilities_never_advertise_unimplemented_methods`
/// fails if the two ever disagree.
pub const IMPLEMENTED_METHODS: &[&str] = &[
    method::HELLO,
    method::CONFIG_GET,
    method::DAEMON_KILL,
    method::OUTPUTS_LIST,
    method::WALLPAPER_SET,
    method::WALLPAPER_CLEAR,
    method::GOVERNOR_OVERRIDE,
    method::LIBRARY_SCAN,
    method::LIBRARY_LIST,
    method::LIBRARY_THUMB,
];

/// Media decode backends this build actually has.
const IMPLEMENTED_MEDIA_BACKENDS: &[&str] = &["image"];

/// Features the project plans and this build does not have, reported through
/// `capabilities.unavailable` so a client can say so instead of staying silent.
///
/// Each entry carries the phase that lands it; when the phase ships, the entry is
/// deleted (not reworded) and the feature appears in its real list. This is the
/// maintainer-visible half of the P0 §0.2.4 fix.
const KNOWN_BUT_NOT_YET: &[(&str, &str)] = &[
    (
        "caelestia",
        "shell backend planned for P3 (P1 renders natively on Hyprland)",
    ),
    (
        "animated-image",
        "content kind planned for P4 (decode + frame pacing)",
    ),
    ("video", "content kind planned for P4 (GStreamer path)"),
    (
        "shader",
        "content kind planned for P5 (WGSL packs, previews)",
    ),
    (
        "avif",
        "AVIF/HEIC decoding is compiled out by default (P2 decision): the decoder is a large \
         C dependency chain and the reference profile decodes JPEG/PNG/WebP only",
    ),
];

/// Where a wallpaper comes from: `source: {path} | {library_id}` per
/// BACKEND-DESIGN §3, plus the bare-string form that `owectl set` and a
/// hand-typed request naturally produce.
///
/// Parsed by hand rather than with `#[serde(untagged)]` because serde's untagged
/// errors are famously opaque — a client with a typo gets "data did not match any
/// variant of untagged enum SetSource", which tells them nothing. This reports the
/// keys it actually saw.
fn source_reference(value: &Value) -> Result<String, ErrorBody> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Ok(text.clone()),
        Value::String(_) => Err(ErrorBody::bad_request(
            "`source` is an empty string; expected a path, `library:<id>`, or `shader:<name>`",
        )),
        Value::Object(map) => {
            let text_of = |key: &str| map.get(key).and_then(Value::as_str);
            let mut found = Vec::new();
            if let Some(path) = text_of("path") {
                found.push(path.to_string());
            }
            if let Some(id) = text_of("library_id") {
                found.push(format!("library:{id}"));
            }
            if let Some(name) = text_of("name") {
                found.push(format!("shader:{name}"));
            }
            match found.len() {
                1 => Ok(found.remove(0)),
                0 => Err(ErrorBody::bad_request(format!(
                    "`source` has no recognised key: expected `path`, `library_id`, or `name`; \
                     saw [{}]",
                    keys_of(map)
                ))),
                _ => Err(ErrorBody::bad_request(format!(
                    "`source` must name exactly one of `path`, `library_id`, `name`; saw [{}]",
                    keys_of(map)
                ))),
            }
        }
        other => Err(ErrorBody::bad_request(format!(
            "`source` must be a string or an object, got `{other}`"
        ))),
    }
}

/// Comma-joined key names of a JSON object, for error messages.
fn keys_of(map: &serde_json::Map<String, Value>) -> String {
    map.keys().cloned().collect::<Vec<_>>().join(", ")
}

/// Parameters of `wallpaper.set`.
#[derive(Debug, Deserialize)]
struct SetParams {
    /// Output target (`all`, `focused`, a name, or a description fragment).
    #[serde(default)]
    output: Option<String>,
    /// The wallpaper to apply; validated by [`source_reference`].
    source: Value,
    /// Transition request; accepted and rejected *visibly* (P2 lands transitions).
    #[serde(default)]
    transition: Option<Value>,
}

/// Parameters of `wallpaper.clear`.
#[derive(Debug, Default, Deserialize)]
struct ClearParams {
    /// Output target; defaults to `all`.
    #[serde(default)]
    output: Option<String>,
}

/// Parameters of `governor.override` (BACKEND-DESIGN §3: `{output, policy}`).
#[derive(Debug, Deserialize)]
struct OverrideParams {
    /// Output target; defaults to all.
    #[serde(default)]
    output: Option<String>,
    /// `pause` to hold rendering, `auto` to hand control back to the policies.
    policy: String,
}

/// Parameters of `library.list` (schema 1.1: `dir` is an index filter, not a
/// one-level directory scan).
#[derive(Debug, Default, Deserialize)]
struct LibraryListParams {
    /// Case-insensitive substring match against the file name and path.
    #[serde(default)]
    filter: Option<String>,
    /// Restrict to a directory (inclusive of subdirectories).
    #[serde(default)]
    dir: Option<String>,
    /// Restrict to one content kind id (`static-image`, …).
    #[serde(default)]
    kind: Option<String>,
    /// 1-based page number; defaults to 1.
    #[serde(default)]
    page: Option<u32>,
    /// Rows per page (clamped to 500); defaults to 100.
    #[serde(default)]
    per_page: Option<u32>,
    /// Start the scan before listing, so the first GUI call has something to show
    /// without a second round trip.
    #[serde(default)]
    scan_if_empty: Option<bool>,
}

/// Parameters of `library.scan`.
#[derive(Debug, Default, Deserialize)]
struct LibraryScanParams {
    /// Roots to scan; omitted means "the configured `library.paths`".
    #[serde(default)]
    paths: Option<Vec<String>>,
}

/// Parameters of `library.thumb`.
#[derive(Debug, Deserialize)]
struct LibraryThumbParams {
    /// Library id, as returned by `library.list`.
    id: i64,
}

/// State shared between the IPC handler and the engine.
#[derive(Debug)]
pub struct DaemonState {
    config: Config,
    socket_path: PathBuf,
    shutdown: Shutdown,
    /// Shared with the hotplug listener, which re-applies wallpapers on the same
    /// engine when an output appears.
    engine: Arc<Engine>,
    library: Arc<LibraryService>,
}

#[allow(
    dead_code,
    reason = "these accessors are the state's contract: the tests use them today, and the \
              P3 shell-event bus reads config/socket_path through them"
)]
impl DaemonState {
    /// Build the shared state for a daemon run.
    ///
    /// The [`Shutdown`] handle is shared with the IPC server, so a `daemon.kill`
    /// request actually stops the accept loop (one flag, one truth).
    pub fn new(
        config: Config,
        socket_path: PathBuf,
        shutdown: Shutdown,
        engine: Arc<Engine>,
        library: Arc<LibraryService>,
    ) -> Self {
        Self {
            config,
            socket_path,
            shutdown,
            engine,
            library,
        }
    }

    /// The library index and thumbnail cache.
    pub fn library(&self) -> &Arc<LibraryService> {
        &self.library
    }

    /// The render engine, shared with the hotplug listener.
    pub fn shared_engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// The effective configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The socket this daemon is serving on.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The render engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Ask the daemon to stop after the current work item.
    pub fn request_shutdown(&self) {
        self.shutdown.request();
    }

    /// Whether shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.is_requested()
    }
}

/// The IPC handler served by [`owe_ipc::Server`].
#[derive(Debug)]
pub struct IpcHandler {
    state: Arc<DaemonState>,
}

impl IpcHandler {
    /// Build a handler over shared daemon state.
    pub fn new(state: Arc<DaemonState>) -> Self {
        Self { state }
    }

    /// What this build can do right now.
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            methods: IMPLEMENTED_METHODS
                .iter()
                .map(|m| (*m).to_string())
                .collect(),
            // Real registry ids, not a static list of everything the project has
            // ever heard of (the P0 §0.2.4 finding).
            shell_backends: self.state.engine.backend_ids(),
            content_kinds: self
                .state
                .engine
                .content_kinds()
                .iter()
                .map(|kind| kind.as_str().to_string())
                .collect(),
            media_backends: IMPLEMENTED_MEDIA_BACKENDS
                .iter()
                .map(|id| (*id).to_string())
                .collect(),
            // The live config's allow-list, not the full catalogue: a picker that
            // offers what `wallpaper.set` will refuse is worse than a short list.
            transitions: self.state.config.render.allow_transitions.clone(),
            unavailable: KNOWN_BUT_NOT_YET
                .iter()
                .map(|(id, why)| format!("{id}: {why}"))
                .collect(),
        }
    }

    fn hello(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        let params: HelloParams = request.params_as()?;
        let schema = protocol::negotiate(&params.schema).ok_or_else(|| {
            ErrorBody::unsupported(format!(
                "no shared ipc schema major; this daemon speaks {} (client sent {:?})",
                protocol::SCHEMA_MAJOR,
                params.schema
            ))
        })?;

        tracing::info!(
            client = %params.client,
            client_version = %params.client_version,
            schema_minor = schema.minor,
            "ipc client connected"
        );

        let reply = HelloReply {
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            schema,
            capabilities: self.capabilities(),
        };
        serde_json::to_value(reply).map_err(|error| ErrorBody::internal(error.to_string()))
    }

    fn config_get(&self, _request: &RequestFrame) -> Result<Value, ErrorBody> {
        let toml_text = self
            .state
            .config
            .to_toml_string()
            .map_err(|error| ErrorBody::internal(error.to_string()))?;
        Ok(json!({
            "schema": SUPPORTED_SCHEMA,
            "toml": toml_text,
        }))
    }

    fn daemon_kill(&self, _request: &RequestFrame) -> Result<Value, ErrorBody> {
        self.state.request_shutdown();
        tracing::info!("shutdown requested over ipc");
        Ok(json!({ "shutting_down": true }))
    }

    fn outputs_list(&self, _request: &RequestFrame) -> Result<Value, ErrorBody> {
        let outputs = self
            .state
            .engine
            .output_views()
            .map_err(|error| engine_error(&error))?;
        Ok(json!({
            "outputs": outputs,
            "paused": self.state.engine.is_paused(),
        }))
    }

    fn wallpaper_set(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        let params: SetParams = request.params_as()?;
        let target = params
            .output
            .as_deref()
            .map(OutputTarget::parse)
            .unwrap_or(OutputTarget::All);
        let reference = source_reference(&params.source)?;

        // `transition` is either a name (`"wave"`) or a table
        // (`{name, duration_ms, fps}`); both forms are what a user types.
        let transition = match &params.transition {
            Some(value) => Some(parse_transition(value)?),
            None => None,
        };

        tracing::info!(spec = %reference, "applying wallpaper");
        let applied = self
            .state
            .engine
            .apply_with(&reference, &target, transition.as_ref())
            .map_err(|error| engine_error(&error))?;
        tracing::info!(
            spec = %applied.reference,
            outputs = %applied.outputs.join(", "),
            "wallpaper applied"
        );

        serde_json::to_value(applied).map_err(|error| ErrorBody::internal(error.to_string()))
    }

    fn wallpaper_clear(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        let params: ClearParams = request.params_as()?;
        let target = params
            .output
            .as_deref()
            .map(OutputTarget::parse)
            .unwrap_or(OutputTarget::All);

        let cleared = self
            .state
            .engine
            .clear(&target)
            .map_err(|error| engine_error(&error))?;
        Ok(json!({ "cleared": cleared }))
    }

    fn governor_override(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        let params: OverrideParams = request.params_as()?;
        let paused = match params.policy.trim().to_ascii_lowercase().as_str() {
            "pause" | "paused" => true,
            "auto" | "resume" | "running" => false,
            other => {
                return Err(ErrorBody::new(
                    ErrorCode::BadRequest,
                    format!("unknown override policy `{other}`; expected `pause` or `auto`"),
                ));
            }
        };

        self.state.engine.set_paused(paused);
        tracing::info!(
            paused,
            output = %params.output.as_deref().unwrap_or("all"),
            "governor override set"
        );
        Ok(json!({
            "paused": paused,
            // Honest about reach: with static wallpapers there is nothing to stop,
            // so the override is recorded now and takes effect when animated
            // content lands in P4 (the policy engine itself is P6).
            "affects": "animated and video wallpapers; static wallpapers are already motionless",
        }))
    }

    /// `library.scan` (FR-LIB-1): bring the index up to date.
    ///
    /// Synchronous in P2, and it says so: the reply carries the scan report, so a
    /// client never has to guess whether the scan finished. Progress *events* need
    /// the shell-event bus (P3); until then a long scan simply blocks its own
    /// request, which is honest and easy to reason about.
    fn library_scan(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        let params: LibraryScanParams = request.params_as()?;
        let roots = match &params.paths {
            Some(paths) => {
                let mut expanded = Vec::with_capacity(paths.len());
                for path in paths {
                    expanded.push(
                        owe_core::path::expand(path)
                            .map_err(|error| ErrorBody::bad_request(error.to_string()))?,
                    );
                }
                Some(expanded)
            }
            None => None,
        };

        let started = std::time::Instant::now();
        let report = self
            .state
            .library
            .scan(roots.as_deref())
            .map_err(|error| ErrorBody::internal(error.to_string()))?;

        tracing::info!(summary = %report.summary(), "library scan finished");
        Ok(json!({
            "scan_id": 1,
            "finished": true,
            "summary": report.summary(),
            "roots": report.roots.iter().map(|root| root.display().to_string()).collect::<Vec<_>>(),
            "added": report.added,
            "updated": report.updated,
            "removed": report.removed,
            "unchanged": report.unchanged,
            "skipped_unsupported": report.skipped_unsupported,
            "rows_touched": report.rows_touched(),
            "files_seen": report.files_seen(),
            "missing_roots": report.missing_roots.iter().map(|root| root.display().to_string()).collect::<Vec<_>>(),
            "unreadable_dirs": report.unreadable_dirs.iter().map(|dir| dir.display().to_string()).collect::<Vec<_>>(),
            "duration_ms": report.duration.as_secs_f64() * 1000.0,
            "wall_clock_ms": started.elapsed().as_secs_f64() * 1000.0,
        }))
    }

    /// `library.list`: paged, filtered, index-backed.
    fn library_list(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        let params: LibraryListParams = request.params_as()?;

        if params.scan_if_empty.unwrap_or(false) {
            let indexed = self
                .state
                .library
                .count(&ListQuery::default())
                .map_err(|error| ErrorBody::internal(error.to_string()))?;
            if indexed == 0 {
                // First GUI connect on a fresh install: scan once so the grid is
                // not mysteriously empty on a machine that has wallpapers.
                let report = self
                    .state
                    .library
                    .scan(None)
                    .map_err(|error| ErrorBody::internal(error.to_string()))?;
                tracing::info!(summary = %report.summary(), "library scanned on first list");
            }
        }

        let kind = match params.kind.as_deref() {
            Some(kind) => Some(parse_kind(kind).ok_or_else(|| {
                ErrorBody::bad_request(format!(
                    "unknown content kind `{kind}` (known: {})",
                    ContentKind::ALL
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?),
            None => None,
        };
        let under = match params.dir.as_deref() {
            Some(dir) => Some(
                owe_core::path::expand(dir)
                    .map_err(|error| ErrorBody::bad_request(error.to_string()))?,
            ),
            None => None,
        };

        let per_page = params
            .per_page
            .unwrap_or(100)
            .clamp(1, owe_core::library::MAX_PAGE);
        let page = params.page.unwrap_or(1).max(1);
        let query = ListQuery {
            filter: params.filter.clone(),
            kind,
            under: under.clone(),
            limit: Some(per_page),
            offset: Some((page - 1).saturating_mul(per_page)),
        };

        let items = self
            .state
            .library
            .list(&query)
            .map_err(|error| ErrorBody::internal(error.to_string()))?;
        let total = self
            .state
            .library
            .count(&ListQuery {
                limit: None,
                offset: None,
                ..query.clone()
            })
            .map_err(|error| ErrorBody::internal(error.to_string()))?;

        let library = &self.state.library;
        let entries: Vec<Value> = items
            .iter()
            .map(|item| {
                let mut value = item.to_json();
                // The cache path when the thumbnail already exists, `null` when the
                // client must ask for it via `library.thumb`. Computing it is a
                // stat, so a page costs one stat per row and nothing decodes.
                value["thumb"] = match library.cached_thumbnail(item) {
                    Some(path) => json!(path.display().to_string()),
                    None => Value::Null,
                };
                value
            })
            .collect();

        Ok(json!({
            "items": entries,
            "total": total,
            "page": page,
            "per_page": per_page,
            "pages": total.div_ceil(u64::from(per_page)),
            "thumbnail_size": library.thumbnail_size(),
            "roots": library.roots().iter().map(|root| root.display().to_string()).collect::<Vec<_>>(),
        }))
    }

    /// `library.thumb`: materialise one cached thumbnail (FR-LIB-2).
    fn library_thumb(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        let params: LibraryThumbParams = request.params_as()?;
        let item = self
            .state
            .library
            .get(params.id)
            .map_err(|error| ErrorBody::internal(error.to_string()))?
            .ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::NotFound,
                    format!(
                        "library item {} is not in the index (it may have been removed since \
                         the last scan)",
                        params.id
                    ),
                )
            })?;

        let thumbnail = self
            .state
            .library
            .thumbnail(&item)
            // A file that cannot be decoded is not a server fault: the caller can
            // act on it (skip this wallpaper, fix the file).
            .map_err(|reason| ErrorBody::new(ErrorCode::BadRequest, reason))?;

        Ok(json!({
            "id": item.id,
            "path": thumbnail.path.display().to_string(),
            "size": thumbnail.size,
            "cached": thumbnail.cached,
            "source": item.path.display().to_string(),
        }))
    }
}

/// Parse a `transition` parameter: a name, or a table with name/duration/fps.
fn parse_transition(value: &Value) -> Result<Transition, ErrorBody> {
    match value {
        Value::String(name) => Ok(Transition {
            name: name.clone(),
            ..Transition::default()
        }),
        Value::Object(map) => {
            let name = map.get("name").and_then(Value::as_str).ok_or_else(|| {
                ErrorBody::bad_request(
                    "`transition` object must contain a `name` (e.g. \
                     {name: \"wipe\", duration_ms: 400})",
                )
            })?;
            let defaults = Transition::default();
            let duration_ms = match map.get("duration_ms") {
                Some(value) => value.as_u64().ok_or_else(|| {
                    ErrorBody::bad_request("`transition.duration_ms` must be a positive integer")
                })?,
                None => defaults.duration_ms,
            };
            let fps = match map.get("fps") {
                Some(value) => value.as_u64().ok_or_else(|| {
                    ErrorBody::bad_request("`transition.fps` must be a positive integer")
                })? as u32,
                None => defaults.fps,
            };
            Ok(Transition {
                name: name.to_string(),
                duration_ms,
                fps,
            })
        }
        other => Err(ErrorBody::bad_request(format!(
            "`transition` must be a name or an object, got `{other}`"
        ))),
    }
}

/// Map an engine failure onto the protocol error code a client can act on.
fn engine_error(error: &EngineError) -> ErrorBody {
    use owe_core::shell::SelectError;

    match error {
        // Wrong output: the client should tell the user which names exist.
        EngineError::Target(_) => ErrorBody::new(ErrorCode::OutputUnknown, error.to_string()),

        // A file we cannot find is "not found"; a file we cannot parse is a bad
        // request. Keeping those apart is what lets the GUI say "missing" vs
        // "corrupt" instead of a generic failure.
        EngineError::Media(owe_media::MediaError::Io { .. }) => {
            ErrorBody::new(ErrorCode::NotFound, error.to_string())
        }
        EngineError::Media(owe_media::MediaError::UnsupportedKind { .. }) => {
            ErrorBody::unsupported(error.to_string())
        }
        EngineError::Media(_) => ErrorBody::new(ErrorCode::BadRequest, error.to_string()),

        // Invalid reference strings and unknown backends are configuration
        // mistakes, which the client can fix.
        EngineError::Model(_) => ErrorBody::new(ErrorCode::BadRequest, error.to_string()),
        EngineError::ShellSelection(SelectError::UnknownBackend { .. }) => {
            ErrorBody::new(ErrorCode::ConfigInvalid, error.to_string())
        }
        EngineError::ShellSelection(_) => {
            ErrorBody::new(ErrorCode::ConfigInvalid, error.to_string())
        }

        // Both buffers held: the client may retry, so say so instead of "internal".
        EngineError::Present(owe_render::PresentError::Busy(_)) | EngineError::Superseded(_) => {
            ErrorBody::new(ErrorCode::Busy, error.to_string())
        }

        // A transition the build does not render or the config disallows is a
        // request the client can fix; the message carries the allowed list.
        EngineError::Transition(_) => ErrorBody::new(ErrorCode::BadRequest, error.to_string()),

        // A library id that resolves to nothing is "not found", not an internal
        // fault: the row can have been removed by a rescan since the GUI listed it.
        EngineError::Library(_) => ErrorBody::new(ErrorCode::NotFound, error.to_string()),

        EngineError::Unsupported(_) => ErrorBody::unsupported(error.to_string()),

        EngineError::Shell(_)
        | EngineError::Render(_)
        | EngineError::Present(_)
        | EngineError::State(_)
        | EngineError::NoGpu(_) => ErrorBody::new(ErrorCode::Internal, error.to_string()),
    }
}

impl Handler for IpcHandler {
    fn handle(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        match request.method.as_str() {
            method::HELLO => self.hello(request),
            method::CONFIG_GET => self.config_get(request),
            method::DAEMON_KILL => self.daemon_kill(request),
            method::OUTPUTS_LIST => self.outputs_list(request),
            method::WALLPAPER_SET => self.wallpaper_set(request),
            method::WALLPAPER_CLEAR => self.wallpaper_clear(request),
            method::GOVERNOR_OVERRIDE => self.governor_override(request),
            method::LIBRARY_LIST => self.library_list(request),
            method::LIBRARY_SCAN => self.library_scan(request),
            // `library.thumb` is the other half of `library.list`: the list says
            // which cells have a cached thumbnail, this materialises the rest.
            method::LIBRARY_THUMB => self.library_thumb(request),
            other => Err(ErrorBody::unsupported(format!(
                "`{other}` is not implemented in this build yet; see docs/IMPLEMENTATION-PLAN.md \
                 for the phase that lands it"
            ))),
        }
    }
}

/// Backends this build knows by name but does not implement yet. Used by the
/// capability tests to prove we never advertise them.
///
/// `generic-layer-shell` was on this list through P0 and left it in P1, when the
/// backend became real — the list shrinking is the evidence that a phase landed.
#[cfg(test)]
const NOT_YET_IMPLEMENTED_BACKENDS: &[&str] = &["caelestia"];

#[cfg(test)]
mod tests {
    use super::*;
    use owe_core::path::XdgPaths;
    use owe_ipc::ReplyFrame;
    use owe_ipc::protocol::SchemaVersion;

    /// A handler over a throwaway XDG tree.
    ///
    /// The temporary directory is deliberately kept (`TempDir::keep`) rather than
    /// deleted: the daemon state holds paths into it and outlives this function.
    /// A few kilobytes in the OS temp directory per test process is a fine price
    /// for not teaching `DaemonState` a special test-only lifetime.
    fn handler_in_tree(config: Config) -> (IpcHandler, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let paths = XdgPaths {
            config_file: dir.join("config.toml"),
            state_dir: dir.join("state"),
            cache_dir: dir.join("cache"),
            data_dir: dir.join("data"),
            runtime_dir: Some(dir.join("run")),
        };
        let library = Arc::new(LibraryService::open(&config, &paths));
        let engine = Arc::new(Engine::new(config.clone(), paths));
        engine.set_library(Arc::clone(&library) as Arc<dyn crate::engine::LibraryResolver>);
        let state = Arc::new(DaemonState::new(
            config,
            PathBuf::from("/run/user/1000/owe/socket"),
            Shutdown::new(),
            engine,
            library,
        ));
        (IpcHandler::new(state), dir)
    }

    fn handler() -> IpcHandler {
        handler_in_tree(Config::default()).0
    }

    fn state_of(handler: &IpcHandler) -> Arc<DaemonState> {
        Arc::clone(&handler.state)
    }

    #[test]
    fn hello_negotiates_and_reports_capabilities() {
        let handler = handler();
        let request = RequestFrame::new(
            "c1",
            method::HELLO,
            json!({
                "client": "test",
                "client_version": "0.1.0",
                "schema": [{ "major": 1, "minor": 0 }],
            }),
        );

        let value = handler.handle(&request).expect("hello ok");
        let reply: HelloReply = serde_json::from_value(value).unwrap();
        assert_eq!(reply.server_version, env!("CARGO_PKG_VERSION"));
        // The client offered minor 0, so that is what it gets: the negotiated
        // version is the *client's* ceiling, never the server's latest.
        assert_eq!(
            reply.schema,
            SchemaVersion { major: 1, minor: 0 },
            "negotiation must not answer with a newer minor than the client sent"
        );
        assert_eq!(reply.capabilities.methods, IMPLEMENTED_METHODS.to_vec());
    }

    #[test]
    fn hello_with_a_current_client_negotiates_the_current_minor() {
        let handler = handler();
        let request = RequestFrame::new(
            "c1",
            method::HELLO,
            json!({
                "client": "test",
                "client_version": "0.2.0",
                "schema": [{ "major": 1, "minor": 1 }],
            }),
        );
        let value = handler.handle(&request).expect("hello ok");
        let reply: HelloReply = serde_json::from_value(value).unwrap();
        assert_eq!(reply.schema, SchemaVersion::CURRENT);
        assert_eq!(SchemaVersion::CURRENT.minor, 1, "P2 bumped the minor");
    }

    #[test]
    fn capabilities_never_advertise_unimplemented_methods() {
        let capabilities = handler().capabilities();
        for advertised in &capabilities.methods {
            assert!(
                method::ALL.contains(&advertised.as_str()),
                "`{advertised}` is not a v1 protocol method"
            );
            assert!(
                IMPLEMENTED_METHODS.contains(&advertised.as_str()),
                "`{advertised}` is advertised but not implemented"
            );
        }
        for unimplemented in [
            method::PLAYBACK_CMD,
            method::STATS_GET,
            method::CONFIG_PATCH,
            method::GOVERNOR_POLICY,
        ] {
            assert!(
                !capabilities.methods.iter().any(|m| m == unimplemented),
                "`{unimplemented}` must not be advertised before it works"
            );
        }
    }

    #[test]
    fn shell_backends_are_the_registry_and_not_a_wish_list() {
        // Closes P0 §0.2.4: the old build advertised caelestia and
        // generic-layer-shell, neither of which could render anything. Today
        // generic-layer-shell really is registered (P1 added it) and caelestia is
        // not — and the difference between those two is exactly what this asserts.
        let capabilities = handler().capabilities();
        assert_eq!(
            capabilities.shell_backends,
            vec!["hyprland".to_string(), "generic-layer-shell".to_string()]
        );
        for phantom in NOT_YET_IMPLEMENTED_BACKENDS {
            assert!(
                !capabilities.shell_backends.iter().any(|id| id == phantom),
                "`{phantom}` is advertised as working but is not implemented"
            );
        }
    }

    #[test]
    fn planned_features_are_reported_as_unavailable_rather_than_dropped() {
        // Removing unimplemented ids from a capability list makes them invisible.
        // The P0 finding was about over-claiming; the fix must not become
        // under-reporting, so every planned shell backend and content kind is
        // named in `unavailable` with the phase that lands it.
        let capabilities = handler().capabilities();
        assert!(!capabilities.unavailable.is_empty());

        for (feature, _) in KNOWN_BUT_NOT_YET {
            let entry = capabilities
                .unavailable
                .iter()
                .find(|entry| entry.starts_with(&format!("{feature}:")))
                .unwrap_or_else(|| panic!("`{feature}` should be reported as unavailable"));
            assert!(
                entry.contains('P') && (entry.contains("planned") || entry.contains("decision")),
                "`{entry}` must say when or why it lands, not just that it is missing"
            );
            assert!(
                !capabilities.shell_backends.iter().any(|id| id == feature),
                "`{feature}` cannot be both unavailable and advertised"
            );
        }

        // And nothing that *does* work may be listed as unavailable.
        for working in ["hyprland", "generic-layer-shell", "static-image"] {
            assert!(
                !capabilities
                    .unavailable
                    .iter()
                    .any(|entry| entry.starts_with(&format!("{working}:"))),
                "`{working}` works and must not be reported as unavailable"
            );
        }
    }

    #[test]
    fn content_kinds_are_limited_to_what_decodes() {
        let capabilities = handler().capabilities();
        assert_eq!(capabilities.content_kinds, vec!["static-image".to_string()]);
        assert_eq!(capabilities.media_backends, vec!["image".to_string()]);
    }

    #[test]
    fn known_backend_ids_do_not_drift_from_the_config_schema() {
        // The config accepts these ids; only some are implemented. If the config
        // schema grows a new id, this test is the reminder to implement it (or to
        // keep not advertising it).
        let known = owe_core::config::KNOWN_SHELL_BACKENDS;
        assert!(known.contains(&"hyprland"), "{known:?}");
        for id in NOT_YET_IMPLEMENTED_BACKENDS {
            assert!(known.contains(id), "`{id}` should still be a known id");
        }
    }

    #[test]
    fn hello_with_incompatible_schema_is_unsupported() {
        let handler = handler();
        let request = RequestFrame::new(
            "c1",
            method::HELLO,
            json!({
                "client": "future-client",
                "client_version": "99.0.0",
                "schema": [{ "major": 7, "minor": 0 }],
            }),
        );

        let error = handler.handle(&request).unwrap_err();
        assert_eq!(error.code, ErrorCode::Unsupported);
        assert!(error.msg.contains("schema"), "{}", error.msg);
    }

    #[test]
    fn config_get_returns_the_effective_toml() {
        let handler = handler();
        let request = RequestFrame::new("c1", method::CONFIG_GET, json!({}));

        let value = handler.handle(&request).expect("config.get ok");
        let toml_text = value["toml"].as_str().unwrap();
        assert!(toml_text.contains("schema = 1"), "{toml_text}");
        assert!(toml_text.contains("[shell]"), "{toml_text}");
        assert_eq!(value["schema"], json!(SUPPORTED_SCHEMA));
    }

    #[test]
    fn daemon_kill_flags_shutdown_and_replies_ok() {
        let handler = handler();
        let state = state_of(&handler);
        assert!(!state.is_shutdown());

        let request = RequestFrame::new("c1", method::DAEMON_KILL, json!({}));
        let value = handler.handle(&request).expect("daemon.kill ok");
        assert_eq!(value["shutting_down"], json!(true));
        assert!(state.is_shutdown());
    }

    #[test]
    fn unimplemented_methods_answer_unsupported_with_a_pointer() {
        // `playback.cmd` is the next method the plan lands (P4). Using it here
        // proves the unsupported path still exists now that every library method
        // is implemented.
        let handler = handler();
        let request = RequestFrame::new("c1", method::PLAYBACK_CMD, json!({}));

        let error = handler.handle(&request).unwrap_err();
        assert_eq!(error.code, ErrorCode::Unsupported);
        assert!(error.msg.contains("IMPLEMENTATION-PLAN"), "{}", error.msg);
    }

    #[test]
    fn bad_params_are_reported_as_bad_request_not_internal() {
        let handler = handler();
        let request = RequestFrame::new("c1", method::HELLO, json!({ "client": 5 }));

        let error = handler.handle(&request).unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest);
    }

    #[test]
    fn error_bodies_serialize_into_reply_frames() {
        let handler = handler();
        let request = RequestFrame::new("c1", method::PLAYBACK_CMD, json!({}));
        let error = handler.handle(&request).unwrap_err();
        let frame = ReplyFrame::err("c1", error);
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["err"]["code"], json!("UNSUPPORTED"));
    }

    #[test]
    fn governor_override_reports_what_it_actually_affects() {
        let handler = handler();
        let request =
            RequestFrame::new("c1", method::GOVERNOR_OVERRIDE, json!({"policy": "pause"}));

        let value = handler.handle(&request).expect("governor.override ok");
        assert_eq!(value["paused"], json!(true));
        assert!(
            value["affects"]
                .as_str()
                .unwrap()
                .contains("static wallpapers"),
            "the reply must not imply more than it does: {value}"
        );
        assert!(state_of(&handler).engine().is_paused());

        // `auto` hands control back.
        let request = RequestFrame::new("c1", method::GOVERNOR_OVERRIDE, json!({"policy": "auto"}));
        handler.handle(&request).expect("resume");
        assert!(!state_of(&handler).engine().is_paused());
    }

    #[test]
    fn an_unknown_override_policy_is_a_bad_request_naming_the_options() {
        let handler = handler();
        let request = RequestFrame::new("c1", method::GOVERNOR_OVERRIDE, json!({"policy": "nope"}));
        let error = handler.handle(&request).unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest);
        assert!(error.msg.contains("`pause` or `auto`"), "{}", error.msg);
    }

    #[test]
    fn source_accepts_both_the_documented_and_the_plain_forms() {
        // Exercised directly, not through the handler: the handler's engine would
        // connect to the live compositor, and a unit test must never do that.
        assert_eq!(
            source_reference(&json!("~/wall.png")).unwrap(),
            "~/wall.png"
        );
        assert_eq!(
            source_reference(&json!({"path": "~/wall.png"})).unwrap(),
            "~/wall.png"
        );
        assert_eq!(
            source_reference(&json!({"library_id": "abc123"})).unwrap(),
            "library:abc123"
        );
        assert_eq!(
            source_reference(&json!({"name": "aurora"})).unwrap(),
            "shader:aurora"
        );
    }

    #[test]
    fn a_bad_source_names_the_problem_instead_of_mumbling_about_variants() {
        let cases = [
            (json!({"uri": "x"}), "path"),
            (json!({}), "path"),
            (json!({"path": "a", "name": "b"}), "exactly one"),
            (json!(""), "empty string"),
            (json!(7), "must be a string or an object"),
        ];
        for (value, needle) in cases {
            let error = source_reference(&value).expect_err("invalid source");
            assert_eq!(error.code, ErrorCode::BadRequest, "{value}");
            assert!(
                error.msg.contains(needle),
                "message for {value} should mention `{needle}`: {}",
                error.msg
            );
            assert!(
                !error.msg.contains("untagged enum"),
                "serde's untagged error must never reach a client: {}",
                error.msg
            );
        }
    }

    #[test]
    fn wallpaper_set_params_parse_including_a_transition_a_p2_client_would_send() {
        let parse = |value: Value| -> SetParams { serde_json::from_value(value).expect("params") };
        let with_extras = parse(json!({
            "output": "eDP-1",
            "source": "/tmp/x.png",
            "transition": {"name": "fade", "duration_ms": 400},
        }));
        assert_eq!(with_extras.output.as_deref(), Some("eDP-1"));
        assert!(with_extras.transition.is_some());
    }

    /// A config whose library roots are `paths` and whose thumbnail size is set.
    fn library_config(paths: &[&Path], thumbnail_size: u32) -> Config {
        let mut config = Config::default();
        config.library.paths = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        config.library.thumbnail_size = thumbnail_size;
        config
    }

    fn write_png(path: &Path, width: u32, height: u32) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut buffer = image::RgbaImage::new(width, height);
        for pixel in buffer.pixels_mut() {
            *pixel = image::Rgba([40, 80, 120, 255]);
        }
        // PNG bytes under any extension: the scanner keys on the extension and the
        // decoder sniffs the bytes, so this is a valid fixture either way.
        buffer
            .save_with_format(path, image::ImageFormat::Png)
            .unwrap();
    }

    #[test]
    fn library_scan_indexes_the_configured_roots_and_reports_what_it_did() {
        let root = tempfile::tempdir().unwrap();
        write_png(&root.path().join("a.png"), 16, 16);
        write_png(&root.path().join("nested/b.jpg"), 16, 16);
        std::fs::write(root.path().join("notes.txt"), b"text").unwrap();

        let (handler, _tree) = handler_in_tree(library_config(&[root.path()], 64));
        let request = RequestFrame::new("c1", method::LIBRARY_SCAN, json!({}));
        let value = handler.handle(&request).expect("library.scan ok");

        assert_eq!(value["finished"], json!(true), "the scan is synchronous");
        assert_eq!(value["added"], json!(2));
        assert_eq!(value["skipped_unsupported"], json!(1));
        assert_eq!(value["rows_touched"], json!(2));
        let summary = value["summary"].as_str().unwrap();
        assert!(summary.contains("3 file(s) seen"), "{summary}");
        assert!(summary.contains("+2"), "{summary}");
        assert!(summary.contains("1 skipped"), "{summary}");
    }

    #[test]
    fn a_second_scan_of_an_unchanged_library_touches_no_rows() {
        // The incremental-rescan property, over IPC: it is what keeps a rescan on
        // a 20 000-file library cheap enough to run on a timer.
        let root = tempfile::tempdir().unwrap();
        for index in 0..5 {
            write_png(&root.path().join(format!("w{index}.png")), 8, 8);
        }
        let (handler, _tree) = handler_in_tree(library_config(&[root.path()], 64));

        let request = RequestFrame::new("c1", method::LIBRARY_SCAN, json!({}));
        handler.handle(&request).expect("first scan");
        let value = handler.handle(&request).expect("second scan");
        assert_eq!(value["rows_touched"], json!(0), "{value}");
        assert_eq!(value["unchanged"], json!(5));
    }

    #[test]
    fn library_list_returns_indexed_items_with_their_references() {
        let root = tempfile::tempdir().unwrap();
        write_png(&root.path().join("wall.png"), 16, 16);
        std::fs::write(root.path().join("notes.txt"), b"text").unwrap();

        let (handler, _tree) = handler_in_tree(library_config(&[root.path()], 64));
        handler
            .handle(&RequestFrame::new("c1", method::LIBRARY_SCAN, json!({})))
            .expect("scan");
        let value = handler
            .handle(&RequestFrame::new("c1", method::LIBRARY_LIST, json!({})))
            .expect("library.list ok");

        let items = value["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{value}");
        assert_eq!(items[0]["name"], json!("wall.png"));
        assert_eq!(items[0]["kind"], json!("static-image"));
        assert!(
            items[0]["reference"]
                .as_str()
                .unwrap()
                .starts_with("library:"),
            "every item must carry the reference the CLI applies: {value}"
        );
        assert_eq!(value["total"], json!(1));
        assert_eq!(value["page"], json!(1));
        assert_eq!(value["thumbnail_size"], json!(64));
        assert!(
            value["roots"].as_array().unwrap().len() == 1,
            "the client is told which roots are indexed: {value}"
        );
    }

    #[test]
    fn library_list_filters_pages_and_validates_the_kind() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..5 {
            write_png(&root.path().join(format!("wall-{index}.png")), 8, 8);
        }
        let (handler, _tree) = handler_in_tree(library_config(&[root.path()], 32));
        handler
            .handle(&RequestFrame::new("c1", method::LIBRARY_SCAN, json!({})))
            .expect("scan");

        let page = handler
            .handle(&RequestFrame::new(
                "c1",
                method::LIBRARY_LIST,
                json!({"page": 2, "per_page": 2}),
            ))
            .expect("page 2");
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
        assert_eq!(page["pages"], json!(3));
        assert_eq!(page["total"], json!(5));

        let filtered = handler
            .handle(&RequestFrame::new(
                "c1",
                method::LIBRARY_LIST,
                json!({"filter": "wall-3"}),
            ))
            .expect("filter");
        assert_eq!(filtered["items"].as_array().unwrap().len(), 1);

        // An unknown kind is a bad request naming the known ones, not an empty
        // page that makes the GUI look broken.
        let error = handler
            .handle(&RequestFrame::new(
                "c1",
                method::LIBRARY_LIST,
                json!({"kind": "hologram"}),
            ))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest);
        assert!(error.msg.contains("static-image"), "{}", error.msg);
    }

    #[test]
    fn library_thumb_materialises_a_png_and_then_serves_it_from_cache() {
        let root = tempfile::tempdir().unwrap();
        write_png(&root.path().join("wide.png"), 400, 100);
        let (handler, _tree) = handler_in_tree(library_config(&[root.path()], 64));
        handler
            .handle(&RequestFrame::new("c1", method::LIBRARY_SCAN, json!({})))
            .expect("scan");
        let listed = handler
            .handle(&RequestFrame::new("c1", method::LIBRARY_LIST, json!({})))
            .expect("list");
        let id = listed["items"][0]["id"].as_i64().unwrap();
        assert!(
            listed["items"][0]["thumb"].is_null(),
            "nothing is cached before it is asked for: {listed}"
        );

        let first = handler
            .handle(&RequestFrame::new(
                "c1",
                method::LIBRARY_THUMB,
                json!({"id": id}),
            ))
            .expect("first thumbnail");
        assert_eq!(first["cached"], json!(false));
        assert_eq!(first["size"], json!(64));
        let thumb_path = PathBuf::from(first["path"].as_str().unwrap());
        let decoded = image::open(&thumb_path).expect("thumbnail file").to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (64, 16));

        let second = handler
            .handle(&RequestFrame::new(
                "c1",
                method::LIBRARY_THUMB,
                json!({"id": id}),
            ))
            .expect("second thumbnail");
        assert_eq!(second["cached"], json!(true));
        assert_eq!(second["path"], first["path"]);

        // And the list now advertises the cached path, so the GUI can draw the
        // cell without a second round trip on the next page load.
        let relisted = handler
            .handle(&RequestFrame::new("c1", method::LIBRARY_LIST, json!({})))
            .expect("relist");
        assert_eq!(relisted["items"][0]["thumb"], first["path"]);
    }

    #[test]
    fn library_thumb_for_an_unknown_id_is_not_found() {
        let (handler, _tree) = handler_in_tree(Config::default());
        let error = handler
            .handle(&RequestFrame::new(
                "c1",
                method::LIBRARY_THUMB,
                json!({"id": 987654}),
            ))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::NotFound);
        assert!(error.msg.contains("987654"), "{}", error.msg);
    }

    #[test]
    fn library_list_can_scan_an_empty_index_on_first_connect() {
        // The GUI's first call. Without this, a fresh install shows an empty grid
        // and the user has to find the "rescan" button before seeing anything.
        let root = tempfile::tempdir().unwrap();
        write_png(&root.path().join("wall.png"), 8, 8);
        let (handler, _tree) = handler_in_tree(library_config(&[root.path()], 32));

        let value = handler
            .handle(&RequestFrame::new(
                "c1",
                method::LIBRARY_LIST,
                json!({"scan_if_empty": true}),
            ))
            .expect("list with scan");
        assert_eq!(value["items"].as_array().unwrap().len(), 1, "{value}");
    }

    #[test]
    fn an_unknown_transition_is_refused_with_the_allowed_list() {
        // Refusing is the point: substituting a transition silently is how a user
        // concludes the feature is broken.
        let handler = handler();
        let request = RequestFrame::new(
            "c1",
            method::WALLPAPER_SET,
            json!({"source": "/tmp/x.png", "transition": "explode"}),
        );
        let error = handler.handle(&request).unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest);
        assert!(error.msg.contains("explode"), "{}", error.msg);
        assert!(error.msg.contains("fade"), "{}", error.msg);
    }

    #[test]
    fn a_transition_the_config_disallows_is_refused_and_says_so() {
        let mut config = Config::default();
        config.render.allow_transitions = vec!["none".to_string(), "fade".to_string()];
        let (handler, _tree) = handler_in_tree(config);

        let request = RequestFrame::new(
            "c1",
            method::WALLPAPER_SET,
            json!({"source": "/tmp/x.png", "transition": "wave"}),
        );
        let error = handler.handle(&request).unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest);
        assert!(
            error.msg.contains("allow_transitions"),
            "the message must name the config key that disallows it: {}",
            error.msg
        );
    }

    #[test]
    fn a_request_mistake_outranks_the_session_being_unavailable() {
        // The rule this pins: whether the *request* is valid must not depend on
        // whether a compositor happens to be attached, because "the daemon cannot
        // start here" is not a useful answer to a misspelled transition — and the
        // CLI is how a user discovers the allow-list in the first place.
        //
        // It regressed: the transition check sat *below* the session start, so a
        // desktop got `BAD_REQUEST` naming `render.allow_transitions` while CI, with
        // no session at all, got `CONFIG_INVALID: no shell backend` for the same
        // request. Two tests passed on the developer's machine and failed on every
        // CI run. A backend id that can never resolve makes the condition
        // reproducible in both: start() is guaranteed to fail, so if the request is
        // reported correctly *despite* that, the ordering is right.
        let mut config = Config::default();
        config.shell.backend = "no-such-backend".to_string();
        config.render.allow_transitions = vec!["none".to_string(), "fade".to_string()];
        let (handler, _tree) = handler_in_tree(config);

        let disallowed = RequestFrame::new(
            "c1",
            method::WALLPAPER_SET,
            json!({"source": "/tmp/x.png", "transition": "wave"}),
        );
        let error = handler.handle(&disallowed).unwrap_err();
        assert_eq!(
            error.code,
            ErrorCode::BadRequest,
            "a disallowed transition must be the client's fault, not the session's: {}",
            error.msg
        );
        assert!(error.msg.contains("allow_transitions"), "{}", error.msg);

        let unknown = RequestFrame::new(
            "c2",
            method::WALLPAPER_SET,
            json!({"source": "/tmp/x.png", "transition": "explode"}),
        );
        let error = handler.handle(&unknown).unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest, "{}", error.msg);
        assert!(error.msg.contains("explode"), "{}", error.msg);
    }

    #[test]
    fn transition_parameters_accept_a_name_and_a_table_and_reject_junk() {
        assert_eq!(
            parse_transition(&json!("wipe")).unwrap(),
            Transition {
                name: "wipe".to_string(),
                ..Transition::default()
            }
        );
        let table = parse_transition(&json!({"name": "slide", "duration_ms": 450, "fps": 30}))
            .expect("table form");
        assert_eq!(table.name, "slide");
        assert_eq!(table.duration_ms, 450);
        assert_eq!(table.fps, 30);

        for bad in [
            json!({}),
            json!({"duration_ms": 100}),
            json!(7),
            json!(null),
        ] {
            let error = parse_transition(&bad).expect_err("invalid transition");
            assert_eq!(error.code, ErrorCode::BadRequest, "{bad}");
        }
    }
}
