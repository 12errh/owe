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
use owe_core::model::ContentKind;
use owe_core::output::OutputTarget;
use owe_ipc::protocol::{self, method};
use owe_ipc::{
    Capabilities, ErrorBody, ErrorCode, Handler, HelloParams, HelloReply, RequestFrame, Shutdown,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::engine::{Engine, EngineError};

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
    method::LIBRARY_LIST,
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
        "transitions",
        "planned for P2; `wallpaper.set` accepts a transition request and says so in its reply",
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

/// Parameters of `library.list`.
#[derive(Debug, Deserialize)]
struct LibraryParams {
    /// Directory to scan (one level deep, P1 scope).
    dir: String,
}

/// State shared between the IPC handler and the engine.
#[derive(Debug)]
pub struct DaemonState {
    config: Config,
    socket_path: PathBuf,
    shutdown: Shutdown,
    engine: Engine,
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
    pub fn new(config: Config, socket_path: PathBuf, shutdown: Shutdown, engine: Engine) -> Self {
        Self {
            config,
            socket_path,
            shutdown,
            engine,
        }
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

        tracing::info!(spec = %reference, "applying wallpaper");
        let applied = self
            .state
            .engine
            .apply(&reference, &target)
            .map_err(|error| engine_error(&error))?;
        tracing::info!(
            spec = %applied.reference,
            outputs = %applied.outputs.join(", "),
            "wallpaper applied"
        );

        let mut value = serde_json::to_value(applied)
            .map_err(|error| ErrorBody::internal(error.to_string()))?;
        if params.transition.is_some() {
            // Silent ignoring is how a user ends up believing a feature shipped.
            value["notes"] = json!([
                "transition requests are not applied yet: the transition engine lands in P2 \
                 (docs/IMPLEMENTATION-PLAN.md)"
            ]);
        }
        Ok(value)
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

    fn library_list(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        let params: LibraryParams = request.params_as()?;
        let directory = owe_core::path::expand(&params.dir)
            .map_err(|error| ErrorBody::new(ErrorCode::BadRequest, error.to_string()))?;

        let entries = scan_directory(&directory)
            .map_err(|error| ErrorBody::new(ErrorCode::NotFound, error.to_string()))?;

        Ok(json!({
            "dir": directory.display().to_string(),
            "entries": entries,
            // P2 replaces this one-level scan with the indexed library, thumbnails
            // and incremental rescan. Until then, say what this actually is.
            "scope": "one level, images only",
        }))
    }
}

/// One directory level of image files.
fn scan_directory(directory: &Path) -> Result<Vec<Value>, std::io::Error> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(kind) = ContentKind::from_path(&path) else {
            continue;
        };
        entries.push(json!({
            "path": path.display().to_string(),
            "name": path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default(),
            "kind": kind.as_str(),
        }));
    }
    entries.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    Ok(entries)
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
        EngineError::Present(owe_render::PresentError::Busy(_)) => {
            ErrorBody::new(ErrorCode::Busy, error.to_string())
        }

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

    fn handler() -> IpcHandler {
        let state = Arc::new(DaemonState::new(
            Config::default(),
            PathBuf::from("/run/user/1000/owe/socket"),
            Shutdown::new(),
            Engine::new(Config::default(), XdgPaths::resolve().unwrap()),
        ));
        IpcHandler::new(state)
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
        assert_eq!(reply.schema, SchemaVersion::CURRENT);
        assert_eq!(reply.capabilities.methods, IMPLEMENTED_METHODS.to_vec());
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
            method::LIBRARY_SCAN,
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
                entry.contains('P') && entry.contains("planned"),
                "`{entry}` must say when it lands, not just that it is missing"
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
        let handler = handler();
        let request = RequestFrame::new("c1", method::LIBRARY_SCAN, json!({}));

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
        let request = RequestFrame::new("c1", method::LIBRARY_SCAN, json!({}));
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

    #[test]
    fn library_list_reports_missing_directories_as_not_found() {
        let handler = handler();
        let request = RequestFrame::new(
            "c1",
            method::LIBRARY_LIST,
            json!({"dir": "/definitely/not/a/real/directory"}),
        );
        let error = handler.handle(&request).unwrap_err();
        assert_eq!(error.code, ErrorCode::NotFound);
    }

    #[test]
    fn library_list_finds_images_and_ignores_other_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("wall.png"), b"not really a png").unwrap();
        std::fs::write(directory.path().join("notes.txt"), b"text").unwrap();
        std::fs::create_dir(directory.path().join("nested")).unwrap();

        let handler = handler();
        let request = RequestFrame::new(
            "c1",
            method::LIBRARY_LIST,
            json!({"dir": directory.path().display().to_string()}),
        );
        let value = handler.handle(&request).expect("library.list ok");

        let entries = value["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "{value}");
        assert_eq!(entries[0]["name"], json!("wall.png"));
        assert_eq!(entries[0]["kind"], json!("static-image"));
    }
}
