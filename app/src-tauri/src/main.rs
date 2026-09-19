//! OWE desktop app — the GUI client for the `owed` daemon (ARCHITECTURE §1).
//!
//! P0 scope: the window launches, the six plugins required by the plan are
//! registered, and one command proves the wiring by performing a real `hello`
//! handshake with the daemon over the IPC socket. The Library and Outputs views
//! are P1 (UI-DESIGN §3).
//!
//! The GUI owns no state: everything it shows is answered by the daemon, so
//! closing this window can never lose information (ARCHITECTURE §1.1).
//!
//! It talks to the daemon through `owe-ipc` and nothing else — no daemon
//! internals, no rendering crate (ARCHITECTURE §2).

// Keeps a console window from appearing next to the GUI on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::Duration;

use serde::Serialize;

/// How long the status probe waits before calling the daemon unreachable.
/// Short on purpose: a status bar must never make the window feel hung.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

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
        .invoke_handler(tauri::generate_handler![daemon_status])
        .run(tauri::generate_context!())
        .expect("failed to start the OWE window");
}
