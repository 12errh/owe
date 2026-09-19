//! Protocol v1 message types (docs/BACKEND-DESIGN.md §3).
//!
//! Wire shapes:
//!
//! ```text
//! request : {"v":1,"id":"c1","method":"hello","params":{…}}
//! reply   : {"v":1,"id":"c1","ok":{…}}  |  {"v":1,"id":"c1","err":{"code":"…","msg":"…"}}
//! event   : {"v":1,"event":"outputs_changed","data":{…}}
//! ```

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::frame::FrameError;

/// Schema major version of this build. Breaking change ⇒ bump.
pub const SCHEMA_MAJOR: u32 = 1;

/// Schema minor version of this build. Additive change ⇒ bump.
pub const SCHEMA_MINOR: u32 = 0;

/// A `(major, minor)` schema version pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaVersion {
    /// Breaking-change counter.
    pub major: u32,
    /// Additive-change counter.
    pub minor: u32,
}

impl SchemaVersion {
    /// The version this build speaks.
    pub const CURRENT: SchemaVersion = SchemaVersion {
        major: SCHEMA_MAJOR,
        minor: SCHEMA_MINOR,
    };
}

/// Pick the best schema version shared with a client.
///
/// Rule (TRD NFR-COMPAT-2): same major required; the effective minor is the
/// lower of the two, so a newer client talking to an older daemon (or vice
/// versa) never assumes optional features that are absent.
#[allow(
    clippy::unnecessary_min_or_max,
    reason = "the clamp is a no-op while SCHEMA_MINOR is 0, and becomes load-bearing \
              the moment this build adds its first additive feature"
)]
pub fn negotiate(client_versions: &[SchemaVersion]) -> Option<SchemaVersion> {
    client_versions
        .iter()
        .filter(|version| version.major == SCHEMA_MAJOR)
        .map(|version| SchemaVersion {
            major: SCHEMA_MAJOR,
            minor: version.minor.min(SCHEMA_MINOR),
        })
        .max_by_key(|version| version.minor)
}

/// Protocol method names. Defined once so CLI, GUI, and daemon cannot drift.
pub mod method {
    /// Handshake; must be the first request on a connection.
    pub const HELLO: &str = "hello";
    /// List outputs with their current wallpaper and policy.
    pub const OUTPUTS_LIST: &str = "outputs.list";
    /// Apply a wallpaper to one output or all of them.
    pub const WALLPAPER_SET: &str = "wallpaper.set";
    /// Clear the wallpaper on one output or all of them.
    pub const WALLPAPER_CLEAR: &str = "wallpaper.clear";
    /// Playback control (play/pause/seek/loop).
    pub const PLAYBACK_CMD: &str = "playback.cmd";
    /// Start (or restart) a library scan.
    pub const LIBRARY_SCAN: &str = "library.scan";
    /// Query the library.
    pub const LIBRARY_LIST: &str = "library.list";
    /// Query the governor policy for an output.
    pub const GOVERNOR_POLICY: &str = "governor.policy";
    /// Temporarily override the governor (tray "pause" button).
    pub const GOVERNOR_OVERRIDE: &str = "governor.override";
    /// Per-output runtime statistics.
    pub const STATS_GET: &str = "stats.get";
    /// Read the effective configuration.
    pub const CONFIG_GET: &str = "config.get";
    /// Hot-apply a configuration change.
    pub const CONFIG_PATCH: &str = "config.patch";
    /// Shut the daemon down gracefully.
    pub const DAEMON_KILL: &str = "daemon.kill";

    /// Every method name in protocol v1, in documentation order.
    pub const ALL: &[&str] = &[
        HELLO,
        OUTPUTS_LIST,
        WALLPAPER_SET,
        WALLPAPER_CLEAR,
        PLAYBACK_CMD,
        LIBRARY_SCAN,
        LIBRARY_LIST,
        GOVERNOR_POLICY,
        GOVERNOR_OVERRIDE,
        STATS_GET,
        CONFIG_GET,
        CONFIG_PATCH,
        DAEMON_KILL,
    ];
}

/// A client → daemon request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestFrame {
    /// Schema major the client is speaking.
    pub v: u32,
    /// Client-chosen request id, echoed in the reply.
    pub id: String,
    /// Method name (see [`method`]).
    pub method: String,
    /// Method parameters; `{}` when a method takes none.
    #[serde(default)]
    pub params: Value,
}

impl RequestFrame {
    /// Build a request.
    pub fn new(id: impl Into<String>, method: &str, params: Value) -> Self {
        Self {
            v: SCHEMA_MAJOR,
            id: id.into(),
            method: method.to_string(),
            params,
        }
    }

    /// Decode the parameters, mapping failures to a `BAD_REQUEST` error body.
    pub fn params_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, ErrorBody> {
        serde_json::from_value(self.params.clone()).map_err(|error| {
            ErrorBody::bad_request(format!("invalid params for `{}`: {error}", self.method))
        })
    }
}

/// A daemon → client reply: exactly one of `ok` / `err` is present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyFrame {
    /// Schema major the daemon is speaking.
    pub v: u32,
    /// Echo of the request id.
    pub id: String,
    /// Success payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<Value>,
    /// Failure payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<ErrorBody>,
}

impl ReplyFrame {
    /// Build a successful reply.
    pub fn ok(id: impl Into<String>, value: Value) -> Self {
        Self {
            v: SCHEMA_MAJOR,
            id: id.into(),
            ok: Some(value),
            err: None,
        }
    }

    /// Build a failed reply.
    pub fn err(id: impl Into<String>, error: ErrorBody) -> Self {
        Self {
            v: SCHEMA_MAJOR,
            id: id.into(),
            ok: None,
            err: Some(error),
        }
    }

    /// Collapse into a result, so clients can use `?`.
    pub fn into_result(self) -> Result<Value, ErrorBody> {
        match (self.ok, self.err) {
            (Some(value), None) => Ok(value),
            (None, Some(error)) => Err(error),
            (Some(_), Some(_)) => Err(ErrorBody::internal(
                "protocol violation: reply carried both `ok` and `err`",
            )),
            (None, None) => Err(ErrorBody::internal(
                "protocol violation: reply carried neither `ok` nor `err`",
            )),
        }
    }
}

/// An unsolicited daemon → client event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventFrame {
    /// Schema major.
    pub v: u32,
    /// Event name (see [`event`]).
    pub event: String,
    /// Event payload.
    #[serde(default)]
    pub data: Value,
}

/// Event names in protocol v1.
pub mod event {
    /// Outputs were added/removed/reconfigured.
    pub const OUTPUTS_CHANGED: &str = "outputs_changed";
    /// A wallpaper was applied or cleared.
    pub const WALLPAPER_CHANGED: &str = "wallpaper_changed";
    /// A governor policy changed for an output.
    pub const POLICY_CHANGED: &str = "policy_changed";
    /// Library scan progress.
    pub const SCAN_PROGRESS: &str = "scan_progress";
    /// Non-fatal error worth showing in the GUI.
    pub const DAEMON_ERROR: &str = "error";
}

/// Machine-readable error codes (TRD FR-CORE-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// The configuration is invalid; nothing was applied.
    ConfigInvalid,
    /// The referenced thing does not exist.
    NotFound,
    /// The output name is unknown.
    OutputUnknown,
    /// The method or feature is not implemented yet.
    Unsupported,
    /// The daemon is busy with something that must finish first.
    Busy,
    /// Request understood, parameters wrong (includes malformed frames).
    BadRequest,
    /// Internal fault; details are in the daemon log, not the wire.
    Internal,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            ErrorCode::ConfigInvalid => "CONFIG_INVALID",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::OutputUnknown => "OUTPUT_UNKNOWN",
            ErrorCode::Unsupported => "UNSUPPORTED",
            ErrorCode::Busy => "BUSY",
            ErrorCode::BadRequest => "BAD_REQUEST",
            ErrorCode::Internal => "INTERNAL",
        };
        f.write_str(name)
    }
}

/// The `err` body of a failed reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {msg}")]
pub struct ErrorBody {
    /// Machine-readable code.
    pub code: ErrorCode,
    /// Human-readable message (shown verbatim by `owectl` and the GUI).
    pub msg: String,
    /// Optional structured details.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

impl ErrorBody {
    /// Build an error body.
    pub fn new(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
            details: Value::Null,
        }
    }

    /// `BAD_REQUEST`.
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, msg)
    }

    /// `UNSUPPORTED` — used for methods that exist in the protocol but are not
    /// implemented in this build yet.
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, msg)
    }

    /// `INTERNAL`.
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, msg)
    }
}

/// Parameters of `hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloParams {
    /// Client name, e.g. `owe` (GUI) or `owectl`.
    pub client: String,
    /// Client version string.
    pub client_version: String,
    /// Schema versions the client can speak, newest first.
    pub schema: Vec<SchemaVersion>,
}

/// Reply to `hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloReply {
    /// Daemon version string.
    pub server_version: String,
    /// The schema version chosen for this connection.
    pub schema: SchemaVersion,
    /// What this daemon can do right now.
    pub capabilities: Capabilities,
}

/// Feature advertisement for the GUI's status surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Protocol methods implemented by this build (subset of [`method::ALL`]).
    pub methods: Vec<String>,
    /// Shell backend ids known to this build.
    pub shell_backends: Vec<String>,
    /// Content kinds this build can render.
    pub content_kinds: Vec<String>,
    /// Media decode backends this build can use.
    pub media_backends: Vec<String>,
}

/// Decode a request frame, rejecting unsupported schema majors early.
pub fn decode_request(payload: &[u8]) -> Result<RequestFrame, FrameError> {
    let request: RequestFrame = crate::frame::decode(payload)?;
    if request.v != SCHEMA_MAJOR {
        return Err(FrameError::Malformed(format!(
            "unsupported schema major {} (this daemon speaks {SCHEMA_MAJOR})",
            request.v
        )));
    }
    Ok(request)
}

/// Best-effort extraction of a request id from a malformed payload, so an error
/// reply can still be correlated by the client.
pub fn extract_id(payload: &[u8]) -> String {
    serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn negotiation_accepts_same_major() {
        let picked = negotiate(&[SchemaVersion { major: 1, minor: 5 }]).unwrap();
        assert_eq!(picked, SchemaVersion::CURRENT, "clamped to our minor");
    }

    #[test]
    fn negotiation_prefers_the_highest_shared_minor() {
        let picked = negotiate(&[
            SchemaVersion { major: 1, minor: 0 },
            SchemaVersion { major: 1, minor: 3 },
        ])
        .unwrap();
        assert_eq!(picked.minor, SCHEMA_MINOR);
        assert_eq!(negotiate(&[]), None);
        assert_eq!(negotiate(&[SchemaVersion { major: 2, minor: 0 }]), None);
    }

    #[test]
    fn request_round_trips_through_json() {
        let request = RequestFrame::new("c7", method::DAEMON_KILL, json!({}));
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("\"method\":\"daemon.kill\""));
        let back: RequestFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn reply_ok_and_err_are_exclusive_on_the_wire() {
        let ok = serde_json::to_value(ReplyFrame::ok("c1", json!({"a": 1}))).unwrap();
        assert_eq!(ok["ok"], json!({"a": 1}));
        assert!(ok.get("err").is_none());

        let err =
            serde_json::to_value(ReplyFrame::err("c1", ErrorBody::unsupported("nope"))).unwrap();
        assert!(err.get("ok").is_none());
        assert_eq!(err["err"]["code"], "UNSUPPORTED");
    }

    #[test]
    fn error_codes_match_the_documented_strings() {
        let cases = [
            (ErrorCode::ConfigInvalid, "CONFIG_INVALID"),
            (ErrorCode::NotFound, "NOT_FOUND"),
            (ErrorCode::OutputUnknown, "OUTPUT_UNKNOWN"),
            (ErrorCode::Unsupported, "UNSUPPORTED"),
            (ErrorCode::Busy, "BUSY"),
            (ErrorCode::BadRequest, "BAD_REQUEST"),
            (ErrorCode::Internal, "INTERNAL"),
        ];
        for (code, expected) in cases {
            assert_eq!(serde_json::to_value(code).unwrap(), json!(expected));
            assert_eq!(code.to_string(), expected);
        }
    }

    #[test]
    fn into_result_surfaces_protocol_violations() {
        let bad = ReplyFrame {
            v: SCHEMA_MAJOR,
            id: "c1".into(),
            ok: None,
            err: None,
        };
        let error = bad.into_result().unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
    }

    #[test]
    fn params_as_reports_the_offending_method() {
        let request = RequestFrame::new("c1", method::WALLPAPER_SET, json!({ "output": 42 }));
        let error = request.params_as::<HelloParams>().unwrap_err();
        assert_eq!(error.code, ErrorCode::BadRequest);
        assert!(error.msg.contains("wallpaper.set"), "{}", error.msg);
    }

    #[test]
    fn decode_request_rejects_other_majors() {
        let payload = serde_json::to_vec(&json!({
            "v": 2, "id": "c1", "method": "hello", "params": {}
        }))
        .unwrap();
        let err = decode_request(&payload).unwrap_err();
        assert!(
            err.to_string().contains("unsupported schema major 2"),
            "{err}"
        );
    }

    #[test]
    fn extract_id_finds_the_id_in_garbage() {
        assert_eq!(
            extract_id(br#"{"v":1,"id":"c9","method":42}"#),
            "c9".to_string()
        );
        assert_eq!(extract_id(b"total garbage"), "");
    }

    #[test]
    fn hello_types_round_trip() {
        let params = HelloParams {
            client: "owectl".into(),
            client_version: "0.1.0".into(),
            schema: vec![SchemaVersion::CURRENT],
        };
        let value = serde_json::to_value(&params).unwrap();
        let back: HelloParams = serde_json::from_value(value).unwrap();
        assert_eq!(back, params);

        let reply = HelloReply {
            server_version: "0.1.0".into(),
            schema: SchemaVersion::CURRENT,
            capabilities: Capabilities {
                methods: vec![method::HELLO.to_string()],
                shell_backends: vec!["hyprland".into()],
                content_kinds: vec!["static-image".into()],
                media_backends: vec!["auto".into()],
            },
        };
        let value = serde_json::to_value(&reply).unwrap();
        let back: HelloReply = serde_json::from_value(value).unwrap();
        assert_eq!(back, reply);
    }

    #[test]
    fn method_names_are_unique() {
        let mut names = method::ALL.to_vec();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate method name in method::ALL");
    }

    #[test]
    fn event_frames_round_trip() {
        let event = EventFrame {
            v: SCHEMA_MAJOR,
            event: event::OUTPUTS_CHANGED.to_string(),
            data: json!({ "count": 2 }),
        };
        let text = serde_json::to_string(&event).unwrap();
        let back: EventFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(back, event);
    }
}
