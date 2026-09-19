//! OWE desktop app — the GUI client for the `owed` daemon (ARCHITECTURE §1).
//!
//! P1 scope: daemon status, folder picker → wallpaper list → apply, and an
//! outputs view. Still the "simple UI" contract of UI-DESIGN §3: no styling
//! architecture, no state store, no timers.
//!
//! Every command here is a thin, stateless wrapper around one IPC call. The GUI
//! never caches, never polls, and never renders anything itself — if this window
//! is closed, the daemon keeps working exactly as before.
//!
//! Doing the work in Rust rather than from JavaScript is deliberate: the webview
//! must not be able to reach the socket, and every request goes through the same
//! typed path as `owectl`.
//!
//! The GUI owns no state: everything it shows is answered by the daemon, so
//! closing this window can never lose information (ARCHITECTURE §1.1).
//!
//! It talks to the daemon through `owe-ipc` and nothing else — no daemon
//! internals, no rendering crate (ARCHITECTURE §2).

// Keeps a console window from appearing next to the GUI on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

/// How long the status probe waits before calling the daemon unreachable.
/// Short on purpose: a status bar must never make the window feel hung.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// How long a command the user explicitly triggered may take. Longer than a
/// status probe because decoding and presenting a 4K image is real work, and a
/// premature timeout would report a failure for something that succeeded.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// An IPC failure, shaped for the UI rather than for a log.
///
/// `code` is the protocol's error code when the daemon answered, or one of the
/// two client-side codes (`unreachable`, `protocol`) when it did not — so the UI
/// can distinguish "daemon is not running" from "the daemon said no".
#[derive(Debug, Serialize)]
struct IpcError {
    code: String,
    message: String,
}

impl IpcError {
    fn unreachable(message: impl Into<String>) -> Self {
        Self {
            code: "unreachable".to_string(),
            message: message.into(),
        }
    }

    fn from_client(error: owe_ipc::ClientError) -> Self {
        match error {
            owe_ipc::ClientError::Server(body) => Self {
                // `Display` on ErrorCode is the wire name (`CONFIG_INVALID`, …),
                // which is exactly what the UI shows and what a bug report needs.
                code: body.code.to_string(),
                message: body.msg,
            },
            owe_ipc::ClientError::Io(error) => Self {
                code: "unreachable".to_string(),
                message: error.to_string(),
            },
            owe_ipc::ClientError::Protocol(detail) => Self {
                code: "protocol".to_string(),
                message: detail,
            },
        }
    }
}

/// Where the daemon's socket is.
///
/// `OWE_SOCKET` is a **client-side** override (absolute path). The daemon derives
/// its own socket from `XDG_RUNTIME_DIR` and never reads this variable; it exists
/// so the GUI can be pointed at a second daemon, and so the command layer below
/// can be integration-tested against a stub server on a temporary socket — which
/// is why it is not read from deep inside the request path.
fn daemon_socket() -> Result<PathBuf, IpcError> {
    if let Some(path) = std::env::var_os("OWE_SOCKET").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    owe_ipc::default_socket_path().map_err(|error| IpcError::unreachable(error.to_string()))
}

/// Connect, handshake, run one request, disconnect — against a given socket.
///
/// P1 opens a fresh connection per command on purpose: the GUI makes one request
/// in response to a click, and a held-open socket would be state the UI does not
/// need (and would go stale across a daemon restart). Revisit if a P2 library view
/// makes this measurably slow — not before.
///
/// Taking the socket as an argument (rather than reading it from the environment
/// inside) is what makes the GUI's entire daemon-facing path testable without a
/// compositor or a running daemon.
fn with_daemon<T>(
    socket: &Path,
    request: impl FnOnce(&mut owe_ipc::Client) -> Result<T, owe_ipc::ClientError>,
) -> Result<T, IpcError> {
    let mut client = owe_ipc::Client::connect_with_timeout(socket, COMMAND_TIMEOUT)
        .map_err(IpcError::from_client)?;
    client
        .hello("owe", env!("CARGO_PKG_VERSION"))
        .map_err(IpcError::from_client)?;
    request(&mut client).map_err(IpcError::from_client)
}

/// `library.list` → a listing the UI can render.
fn list_wallpapers_from(socket: &Path, dir: &str) -> Result<LibraryListing, IpcError> {
    let listing = with_daemon(socket, |client| {
        client.call("library.list", serde_json::json!({ "dir": dir }))
    })?;

    Ok(LibraryListing {
        dir: field_str(&listing, "dir"),
        entries: listing["entries"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| WallpaperEntry {
                        path: field_str(entry, "path"),
                        name: field_str(entry, "name"),
                        bytes: entry["bytes"].as_u64().unwrap_or(0),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        scope: field_str(&listing, "scope"),
    })
}

/// `outputs.list` → the outputs view.
fn list_outputs_from(socket: &Path) -> Result<OutputsView, IpcError> {
    let reply = with_daemon(socket, |client| {
        client.call("outputs.list", serde_json::json!({}))
    })?;

    Ok(OutputsView {
        outputs: reply["outputs"]
            .as_array()
            .map(|outputs| {
                outputs
                    .iter()
                    .map(|output| OutputView {
                        name: field_str(output, "name"),
                        description: field_str(output, "description"),
                        width: output["width"].as_u64().unwrap_or(0) as u32,
                        height: output["height"].as_u64().unwrap_or(0) as u32,
                        focused: output["focused"].as_bool().unwrap_or(false),
                        wallpaper: output["wallpaper"].as_str().map(str::to_string),
                        kind: output["kind"].as_str().map(str::to_string),
                        state: output["state"].as_str().unwrap_or("unknown").to_string(),
                        recorded: output["recorded"].as_str().map(str::to_string),
                        error: output["error"].as_str().map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        paused: reply["paused"].as_bool().unwrap_or(false),
    })
}

/// `wallpaper.set` → what was applied.
fn apply_wallpaper_to(
    socket: &Path,
    path: &str,
    output: Option<&str>,
) -> Result<ApplyOutcome, IpcError> {
    let reply = with_daemon(socket, |client| {
        let params = match output {
            // The documented object form (BACKEND-DESIGN §3) so the GUI exercises
            // the same shape a script would. `output` is omitted to mean "all",
            // which is the daemon's default rather than a name it must know.
            Some(name) => serde_json::json!({ "output": name, "source": { "path": path } }),
            None => serde_json::json!({ "source": { "path": path } }),
        };
        client.call("wallpaper.set", params)
    })?;

    Ok(ApplyOutcome {
        reference: field_str(&reply, "reference"),
        kind: field_str(&reply, "kind"),
        outputs: string_list(&reply, "outputs"),
        notes: string_list(&reply, "notes"),
    })
}

/// `wallpaper.clear` → the outputs that were cleared.
fn clear_wallpaper_on(socket: &Path, output: Option<&str>) -> Result<Vec<String>, IpcError> {
    let reply = with_daemon(socket, |client| {
        let params = match output {
            Some(name) => serde_json::json!({ "output": name }),
            None => serde_json::json!({ "output": "all" }),
        };
        client.call("wallpaper.clear", params)
    })?;
    Ok(string_list(&reply, "cleared"))
}

/// A JSON string field, or an empty string when the daemon omitted it.
///
/// "Missing" and "empty" are the same thing to a label in the UI, and inventing a
/// placeholder here would only move the decision away from where it is displayed.
fn field_str(value: &serde_json::Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

/// A JSON array of strings.
fn string_list(value: &serde_json::Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// One wallpaper file the library knows about.
#[derive(Debug, Serialize)]
struct WallpaperEntry {
    path: String,
    name: String,
    bytes: u64,
}

/// A directory listing from `library.list`.
#[derive(Debug, Serialize)]
struct LibraryListing {
    dir: String,
    entries: Vec<WallpaperEntry>,
    /// What the daemon actually scanned, so the UI never implies more.
    scope: String,
}

/// One output as the daemon describes it.
#[derive(Debug, Serialize)]
struct OutputView {
    name: String,
    description: String,
    width: u32,
    height: u32,
    focused: bool,
    /// What this daemon run has presented.
    wallpaper: Option<String>,
    kind: Option<String>,
    state: String,
    /// Recorded in the session file but not applied this run (restore is P2).
    recorded: Option<String>,
    error: Option<String>,
}

/// The outputs view plus the governor state.
#[derive(Debug, Serialize)]
struct OutputsView {
    outputs: Vec<OutputView>,
    paused: bool,
}

/// What an apply did (or why it did not).
#[derive(Debug, Serialize)]
struct ApplyOutcome {
    reference: String,
    kind: String,
    outputs: Vec<String>,
    /// Notes the daemon attached, e.g. a transition request it cannot honour yet.
    notes: Vec<String>,
}

/// What the status bar shows (UI-DESIGN §3). Field names are snake_case so they
/// match the TypeScript interface in `app/src/ipc.ts` exactly.
#[derive(Debug, Serialize)]
struct DaemonStatus {
    connected: bool,
    socket_path: String,
    server_version: Option<String>,
    schema: Option<String>,
    shell_backends: Vec<String>,
    content_kinds: Vec<String>,
    media_backends: Vec<String>,
    /// Planned-but-missing features (`"<id>: <why>"`). Shown, not hidden.
    unavailable: Vec<String>,
    error: Option<String>,
}

impl DaemonStatus {
    /// The state where the daemon could not be reached or refused the handshake.
    fn unreachable(socket_path: String, error: String) -> Self {
        Self {
            connected: false,
            socket_path,
            server_version: None,
            schema: None,
            shell_backends: Vec::new(),
            content_kinds: Vec::new(),
            media_backends: Vec::new(),
            unavailable: Vec::new(),
            error: Some(error),
        }
    }
}

/// Blocking IPC work — always called off the UI thread.
fn probe_daemon() -> DaemonStatus {
    let socket = match owe_ipc::default_socket_path() {
        Ok(path) => path,
        Err(error) => return DaemonStatus::unreachable(String::new(), error.to_string()),
    };
    let socket_path = socket.display().to_string();

    let mut client = match owe_ipc::Client::connect_with_timeout(&socket, PROBE_TIMEOUT) {
        Ok(client) => client,
        Err(error) => return DaemonStatus::unreachable(socket_path, error.to_string()),
    };

    match client.hello("owe", env!("CARGO_PKG_VERSION")) {
        Ok(reply) => DaemonStatus {
            connected: true,
            socket_path,
            server_version: Some(reply.server_version),
            schema: Some(format!("{}.{}", reply.schema.major, reply.schema.minor)),
            shell_backends: reply.capabilities.shell_backends,
            content_kinds: reply.capabilities.content_kinds,
            media_backends: reply.capabilities.media_backends,
            unavailable: reply.capabilities.unavailable,
            error: None,
        },
        Err(error) => DaemonStatus::unreachable(socket_path, error.to_string()),
    }
}

/// Ask the daemon for its status. Never fails: "not running" is a normal state
/// the UI displays, not an error dialog.
#[tauri::command]
async fn daemon_status() -> DaemonStatus {
    match tauri::async_runtime::spawn_blocking(probe_daemon).await {
        Ok(status) => status,
        Err(error) => DaemonStatus::unreachable(String::new(), format!("probe failed: {error}")),
    }
}

/// List image files in a directory (the daemon does the scanning, not the GUI).
#[tauri::command]
async fn list_wallpapers(dir: String) -> Result<LibraryListing, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || list_wallpapers_from(&socket, &dir)).await
}

/// Ask the daemon about its outputs (the state it also renders on).
#[tauri::command]
async fn list_outputs() -> Result<OutputsView, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || list_outputs_from(&socket)).await
}

/// Apply a wallpaper to one output (or all of them).
#[tauri::command]
async fn apply_wallpaper(path: String, output: Option<String>) -> Result<ApplyOutcome, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || apply_wallpaper_to(&socket, &path, output.as_deref())).await
}

/// Remove the wallpaper from one output (or all of them).
#[tauri::command]
async fn clear_wallpaper(output: Option<String>) -> Result<Vec<String>, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || clear_wallpaper_on(&socket, output.as_deref())).await
}

/// Run blocking IPC work off the UI thread.
///
/// Every command goes through here: a slow decode must never freeze the webview,
/// and Tauri's async runtime is the documented place for the waiting.
///
/// `spawn_blocking` itself only fails if the executor is shutting down, which is
/// indistinguishable from "the window is closing" — still reported rather than
/// swallowed.
async fn run_blocking<T, F>(work: F) -> Result<T, IpcError>
where
    F: FnOnce() -> Result<T, IpcError> + Send + 'static,
    T: Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) => Err(IpcError::unreachable(format!(
            "the request was cancelled before it finished: {error}"
        ))),
    }
}

/// `OWE_SOCKET` (the client-side override) must win over XDG discovery, so the GUI
/// can be aimed at a second daemon — and so these tests can aim it at a stub.
fn main() {
    tauri::Builder::default()
        // P0 plugin set (IMPLEMENTATION-PLAN Phase 0). They are registered now
        // so the capability surface is settled before any screen depends on it.
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            daemon_status,
            list_wallpapers,
            list_outputs,
            apply_wallpaper,
            clear_wallpaper
        ])
        .run(tauri::generate_context!())
        .expect("failed to start the OWE window");
}

#[cfg(test)]
mod tests {
    use super::*;
    use owe_ipc::protocol::{ErrorBody, ErrorCode, method};
    use owe_ipc::{Handler, RequestFrame, Server};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// A daemon that answers from a canned script and records what it was asked.
    ///
    /// These tests exist because "the GUI compiles" is not evidence that its
    /// apply button works: every bug this file can have lives in the parameter
    /// shapes it sends and the reply fields it reads back.
    struct StubDaemon {
        asks: Mutex<Vec<(String, Value)>>,
        hello_count: AtomicUsize,
        fail_with: Option<(ErrorCode, String)>,
    }

    impl StubDaemon {
        fn new() -> Self {
            Self {
                asks: Mutex::new(Vec::new()),
                hello_count: AtomicUsize::new(0),
                fail_with: None,
            }
        }

        fn failing(code: ErrorCode, message: &str) -> Self {
            Self {
                fail_with: Some((code, message.to_string())),
                ..Self::new()
            }
        }

        fn ask(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
            self.asks
                .lock()
                .expect("stub lock")
                .push((request.method.clone(), request.params.clone()));
            if let Some((code, message)) = &self.fail_with {
                return Err(ErrorBody::new(*code, message.clone()));
            }
            match request.method.as_str() {
                method::HELLO => {
                    self.hello_count.fetch_add(1, Ordering::Relaxed);
                    Ok(json!({
                        "server_version": "0.1.0",
                        "schema": { "major": 1, "minor": 0 },
                        "capabilities": {
                            "methods": [method::HELLO],
                            "shell_backends": ["hyprland"],
                            "content_kinds": ["static-image"],
                            "media_backends": ["image"],
                            "unavailable": ["caelestia: planned for P3"],
                        },
                    }))
                }
                "library.list" => Ok(json!({
                    "dir": "/tmp/walls",
                    "entries": [
                        { "name": "a.png", "path": "/tmp/walls/a.png", "bytes": 10 },
                        { "name": "b.jpg", "path": "/tmp/walls/b.jpg", "bytes": 20 },
                    ],
                    "scope": "one level, images only",
                })),
                method::OUTPUTS_LIST => Ok(json!({
                    "outputs": [{
                        "name": "eDP-1",
                        "description": "Samsung",
                        "width": 1366,
                        "height": 768,
                        "focused": true,
                        "wallpaper": "/tmp/walls/a.png",
                        "kind": "static-image",
                        "state": "presented",
                        "recorded": null,
                        "error": null,
                    }],
                    "paused": false,
                })),
                "wallpaper.set" => Ok(json!({
                    "reference": request.params["source"]["path"],
                    "kind": "static-image",
                    "outputs": ["eDP-1"],
                    "notes": ["transition requests are not applied yet"],
                })),
                "wallpaper.clear" => Ok(json!({ "cleared": ["eDP-1"] })),
                other => Err(ErrorBody::unsupported(format!("`{other}` is not stubbed"))),
            }
        }
    }

    impl Handler for StubDaemon {
        fn handle(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
            self.ask(request)
        }
    }

    /// A stub daemon running on its own thread, stopped when this is dropped.
    ///
    /// The first version of these tests spawned the server and called `join()` at
    /// the end — which hung forever, because `serve_blocking` loops until someone
    /// requests shutdown. A guard makes the stop part of the type instead of a
    /// line each test has to remember (and that a failing assertion would skip).
    struct Running {
        shutdown: owe_ipc::Shutdown,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.shutdown.request();
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// Host a stub daemon on a temporary socket.
    ///
    /// The temp dir is returned so it outlives the socket's users: dropping it
    /// early would unlink the socket while a test is still talking to it.
    fn start_stub(daemon: Arc<StubDaemon>) -> (PathBuf, tempfile::TempDir, Running) {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("socket");
        let server = Server::bind(&socket, daemon).expect("bind stub daemon");
        let shutdown = server.shutdown_signal();
        let handle = std::thread::spawn(move || {
            let _ = server.serve_blocking();
        });
        (
            socket,
            dir,
            Running {
                shutdown,
                handle: Some(handle),
            },
        )
    }

    #[test]
    fn the_library_listing_reaches_the_shape_the_ui_renders() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let listing = list_wallpapers_from(&socket, "/tmp/walls").expect("listing");
        assert_eq!(listing.dir, "/tmp/walls");
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(listing.entries[0].name, "a.png");
        assert_eq!(listing.entries[1].bytes, 20);
        assert_eq!(listing.scope, "one level, images only");

        let asks = daemon.asks.lock().unwrap();
        let (_, params) = asks.last().expect("one request recorded");
        assert_eq!(params["dir"], json!("/tmp/walls"));
        assert_eq!(
            daemon.hello_count.load(Ordering::Relaxed),
            1,
            "every command must handshake first"
        );
    }

    #[test]
    fn applying_uses_the_documented_source_object_and_reads_back_every_field() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        // No output given: the parameter must be *absent*, not `"all"` by hand,
        // so the daemon's own default stays authoritative.
        let outcome = apply_wallpaper_to(&socket, "/tmp/walls/a.png", None).expect("apply");
        assert_eq!(outcome.reference, "/tmp/walls/a.png");
        assert_eq!(outcome.kind, "static-image");
        assert_eq!(outcome.outputs, vec!["eDP-1".to_string()]);
        assert_eq!(
            outcome.notes.len(),
            1,
            "the daemon's note must reach the UI"
        );

        {
            let asks = daemon.asks.lock().unwrap();
            let (_, params) = asks.last().expect("recorded");
            assert_eq!(params["source"]["path"], json!("/tmp/walls/a.png"));
            assert!(params.get("output").is_none(), "omitted means all outputs");
        }

        // Named output: it must be passed through untouched.
        apply_wallpaper_to(&socket, "/tmp/walls/b.jpg", Some("HDMI-A-1")).expect("apply");
        {
            let asks = daemon.asks.lock().unwrap();
            let (_, params) = asks.last().expect("recorded");
            assert_eq!(params["output"], json!("HDMI-A-1"));
        }
    }

    #[test]
    fn outputs_and_clear_map_onto_the_views_the_screens_use() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let view = list_outputs_from(&socket).expect("outputs");
        assert!(!view.paused);
        assert_eq!(view.outputs.len(), 1);
        let output = &view.outputs[0];
        assert_eq!(output.name, "eDP-1");
        assert_eq!((output.width, output.height), (1366, 768));
        assert!(output.focused);
        assert_eq!(output.state, "presented");
        assert_eq!(output.wallpaper.as_deref(), Some("/tmp/walls/a.png"));
        assert!(output.error.is_none());

        let cleared = clear_wallpaper_on(&socket, None).expect("clear");
        assert_eq!(cleared, vec!["eDP-1".to_string()]);
        let cleared = clear_wallpaper_on(&socket, Some("eDP-1")).expect("clear one");
        assert_eq!(cleared, vec!["eDP-1".to_string()]);
    }

    #[test]
    fn a_daemon_refusal_keeps_its_protocol_code() {
        // The UI shows `code` next to the message; losing it would turn "the file
        // is missing" and "the daemon exploded" into the same dialogue.
        let daemon = Arc::new(StubDaemon::failing(ErrorCode::NotFound, "no such file"));
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let failure = apply_wallpaper_to(&socket, "/nope.png", None).expect_err("must fail");
        assert_eq!(failure.code, "NOT_FOUND");
        assert_eq!(failure.message, "no such file");
    }

    #[test]
    fn an_absent_daemon_is_reported_as_unreachable_not_as_a_crash() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("nothing-here");
        let _ = &dir;
        for failure in [
            list_outputs_from(&missing).expect_err("no daemon"),
            list_wallpapers_from(&missing, "/tmp").expect_err("no daemon"),
            apply_wallpaper_to(&missing, "/tmp/a.png", None).expect_err("no daemon"),
            clear_wallpaper_on(&missing, None).expect_err("no daemon"),
        ] {
            assert_eq!(failure.code, "unreachable", "{}", failure.message);
            assert!(!failure.message.is_empty());
        }
    }
}
