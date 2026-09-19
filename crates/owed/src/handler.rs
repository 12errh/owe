//! The daemon's IPC handler: what `owed` answers when a client talks to it.
//!
//! Capabilities advertise **only what this build implements** (TRD: no
//! advertising stub methods), and every not-yet-implemented protocol method
//! answers `UNSUPPORTED` with the phase that will land it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use owe_core::Config;
use owe_core::config::{KNOWN_MEDIA_BACKENDS, KNOWN_SHELL_BACKENDS, SUPPORTED_SCHEMA};
use owe_core::model::ContentKind;
use owe_ipc::protocol::{self, method};
use owe_ipc::{Capabilities, ErrorBody, Handler, HelloParams, HelloReply, RequestFrame, Shutdown};
use serde_json::{Value, json};

/// Protocol methods actually implemented by this build, in advertised order.
pub const IMPLEMENTED_METHODS: &[&str] = &[method::HELLO, method::CONFIG_GET, method::DAEMON_KILL];

/// State shared between the IPC handler and (from P1) the render engine.
#[derive(Debug)]
pub struct DaemonState {
    config: Config,
    socket_path: PathBuf,
    shutdown: Shutdown,
}

// Accessors below are part of the state's contract: P1's output workers and the
// P6 governor read the config and socket path through them, and the tests use
// them today. Marked allowed until their first non-test caller lands.
#[allow(dead_code)]
impl DaemonState {
    /// Build the shared state for a daemon run.
    ///
    /// The [`Shutdown`] handle is shared with the IPC server, so a `daemon.kill`
    /// request actually stops the accept loop (one flag, one truth).
    pub fn new(config: Config, socket_path: PathBuf, shutdown: Shutdown) -> Self {
        Self {
            config,
            socket_path,
            shutdown,
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
            shell_backends: KNOWN_SHELL_BACKENDS
                .iter()
                .filter(|id| **id != "auto")
                .map(|id| (*id).to_string())
                .collect(),
            content_kinds: ContentKind::ALL
                .iter()
                .map(|kind| kind.as_str().to_string())
                .collect(),
            media_backends: KNOWN_MEDIA_BACKENDS
                .iter()
                .map(|id| (*id).to_string())
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
}

impl Handler for IpcHandler {
    fn handle(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        match request.method.as_str() {
            method::HELLO => self.hello(request),
            method::CONFIG_GET => self.config_get(request),
            method::DAEMON_KILL => self.daemon_kill(request),
            other => Err(ErrorBody::unsupported(format!(
                "`{other}` is not implemented in this build yet; see docs/IMPLEMENTATION-PLAN.md \
                 for the phase that lands it"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use owe_ipc::protocol::SchemaVersion;
    use owe_ipc::{ErrorCode, ReplyFrame};

    fn handler() -> IpcHandler {
        let state = Arc::new(DaemonState::new(
            Config::default(),
            PathBuf::from("/run/user/1000/owe/socket"),
            Shutdown::new(),
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
            method::WALLPAPER_SET,
            method::OUTPUTS_LIST,
            method::STATS_GET,
        ] {
            assert!(
                !capabilities.methods.iter().any(|m| m == unimplemented),
                "`{unimplemented}` must not be advertised before it works"
            );
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
        let request = RequestFrame::new("c1", method::WALLPAPER_SET, json!({}));

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
        let request = RequestFrame::new("c1", method::STATS_GET, json!({}));
        let error = handler.handle(&request).unwrap_err();
        let frame = ReplyFrame::err("c1", error);
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["err"]["code"], json!("UNSUPPORTED"));
    }
}
