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

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

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
fn socket_from_values(
    override_socket: Option<&OsStr>,
    runtime_dir: Option<&OsStr>,
) -> Result<PathBuf, IpcError> {
    if let Some(path) = override_socket.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    owe_ipc::socket_path_in(runtime_dir).ok_or_else(|| {
        IpcError::unreachable("XDG_RUNTIME_DIR is not set, so the daemon socket cannot be located")
    })
}

fn daemon_socket() -> Result<PathBuf, IpcError> {
    socket_from_values(
        std::env::var_os("OWE_SOCKET").as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
    )
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

/// The `library.list` parameters the GUI actually sets.
///
/// All optional: the frontend sends only what the user changed, which leaves the
/// daemon's own defaults (`page = 1`, `per_page = 100`, its configured roots) as
/// the single authority instead of being duplicated here.
#[derive(Debug, Default, Deserialize)]
struct LibraryQuery {
    /// Case-insensitive substring match against the name and path.
    filter: Option<String>,
    /// Restrict to a directory (inclusive of subdirectories).
    dir: Option<String>,
    /// Restrict to one content kind id.
    kind: Option<String>,
    /// 1-based page number.
    page: Option<u32>,
    /// Rows per page.
    per_page: Option<u32>,
    /// Ask the daemon to scan once when the index is empty, so a first launch shows
    /// a library instead of an empty grid on a machine full of wallpapers.
    scan_if_empty: Option<bool>,
}

impl LibraryQuery {
    /// The `params` object for `library.list`, with unset keys omitted.
    fn to_params(&self) -> serde_json::Value {
        let mut params = serde_json::Map::new();
        for (key, value) in [
            ("filter", self.filter.as_deref()),
            ("dir", self.dir.as_deref()),
            ("kind", self.kind.as_deref()),
        ] {
            if let Some(text) = value.filter(|text| !text.is_empty()) {
                params.insert(key.to_string(), serde_json::Value::String(text.to_string()));
            }
        }
        if let Some(page) = self.page {
            params.insert("page".to_string(), serde_json::json!(page));
        }
        if let Some(per_page) = self.per_page {
            params.insert("per_page".to_string(), serde_json::json!(per_page));
        }
        if let Some(scan_if_empty) = self.scan_if_empty {
            params.insert(
                "scan_if_empty".to_string(),
                serde_json::json!(scan_if_empty),
            );
        }
        serde_json::Value::Object(params)
    }
}

/// `library.list` → a page of the index the grid renders.
fn library_index_from(socket: &Path, query: &LibraryQuery) -> Result<LibraryPage, IpcError> {
    let reply = with_daemon(socket, |client| {
        client.call(owe_ipc::protocol::method::LIBRARY_LIST, query.to_params())
    })?;

    Ok(LibraryPage {
        items: reply["items"]
            .as_array()
            .map(|items| items.iter().map(library_item_from).collect())
            .unwrap_or_default(),
        total: reply["total"].as_u64().unwrap_or(0),
        page: reply["page"].as_u64().unwrap_or(1).max(1) as u32,
        pages: reply["pages"].as_u64().unwrap_or(0),
        per_page: reply["per_page"].as_u64().unwrap_or(0) as u32,
        thumbnail_size: reply["thumbnail_size"].as_u64().unwrap_or(0) as u32,
        roots: string_list(&reply, "roots"),
    })
}

/// One row of `library.list`.
fn library_item_from(item: &serde_json::Value) -> LibraryItem {
    LibraryItem {
        id: item["id"].as_i64().unwrap_or(0),
        path: field_str(item, "path"),
        name: field_str(item, "name"),
        kind: field_str(item, "kind"),
        reference: field_str(item, "reference"),
        // `null` is meaningful here (the daemon stat-ed the cache and there is no
        // thumbnail yet), so it must survive as `None` rather than become an empty
        // string the webview would try to load as an image.
        thumb: item["thumb"].as_str().map(str::to_string),
        bytes: item["size_bytes"].as_u64().unwrap_or(0),
    }
}

/// `library.scan` → what the scan did.
fn library_scan_from(socket: &Path, paths: Option<Vec<String>>) -> Result<ScanSummary, IpcError> {
    let reply = with_daemon(socket, |client| {
        let params = match &paths {
            // Omitted means "the configured `library.paths`", which is the daemon's
            // rule to state, not the GUI's to guess.
            Some(paths) if !paths.is_empty() => serde_json::json!({ "paths": paths }),
            _ => serde_json::json!({}),
        };
        client.call(owe_ipc::protocol::method::LIBRARY_SCAN, params)
    })?;

    Ok(ScanSummary {
        summary: field_str(&reply, "summary"),
        roots: string_list(&reply, "roots"),
        added: reply["added"].as_u64().unwrap_or(0),
        updated: reply["updated"].as_u64().unwrap_or(0),
        removed: reply["removed"].as_u64().unwrap_or(0),
        unchanged: reply["unchanged"].as_u64().unwrap_or(0),
        skipped_unsupported: reply["skipped_unsupported"].as_u64().unwrap_or(0),
        rows_touched: reply["rows_touched"].as_u64().unwrap_or(0),
        files_seen: reply["files_seen"].as_u64().unwrap_or(0),
        missing_roots: string_list(&reply, "missing_roots"),
        duration_ms: reply["duration_ms"].as_f64().unwrap_or(0.0),
    })
}

/// `library.thumb` → the cached PNG for one item, as a data URL.
///
/// The bytes are read here and inlined rather than handing the webview a file
/// path. That is a deliberate trade: it costs one file read per grid cell the user
/// actually sees, and it buys a webview with **no filesystem access at all** — no
/// `asset:` protocol to enable, no scope to widen, nothing to get wrong in a CSP.
/// The daemon remains the only component that touches the cache as a path.
fn library_thumb_from(socket: &Path, id: i64) -> Result<Thumbnail, IpcError> {
    let reply = with_daemon(socket, |client| {
        client.call(
            owe_ipc::protocol::method::LIBRARY_THUMB,
            serde_json::json!({ "id": id }),
        )
    })?;
    let path = field_str(&reply, "path");

    let bytes = std::fs::read(&path).map_err(|error| IpcError {
        code: "cache".to_string(),
        message: format!(
            "the daemon reported a thumbnail at `{path}` but it could not be read: {error}"
        ),
    })?;

    Ok(Thumbnail {
        id: reply["id"].as_i64().unwrap_or(id),
        path,
        size: reply["size"].as_u64().unwrap_or(0) as u32,
        cached: reply["cached"].as_bool().unwrap_or(false),
        source: field_str(&reply, "source"),
        data_url: format!("data:image/png;base64,{}", base64(&bytes)),
    })
}

/// Standard-alphabet base64 with `=` padding.
///
/// Hand-rolled rather than a new dependency: it is twenty lines of pure function
/// with an obvious test, and the alternative puts a crate in the GUI's tree for
/// one encode per visible grid cell. Panics are impossible by construction (every
/// index is masked to six bits), which is the property worth having here.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let triple = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(char::from(ALPHABET[((triple >> 18) & 0x3f) as usize]));
        out.push(char::from(ALPHABET[((triple >> 12) & 0x3f) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[((triple >> 6) & 0x3f) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[(triple & 0x3f) as usize])
        } else {
            '='
        });
    }
    out
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

/// The `source` object for a reference, in the documented object form
/// (BACKEND-DESIGN §3).
///
/// `library:<id>` and `shader:<name>` are prefixes on the wire only where a user
/// types the reference; the object form names the *source* key, and the daemon
/// rejects a request that names more than one — so the mapping has to be exact,
/// not a best-effort guess.
fn source_params(reference: &str) -> serde_json::Value {
    if let Some(id) = reference.strip_prefix("library:") {
        serde_json::json!({ "library_id": id })
    } else if let Some(name) = reference.strip_prefix("shader:") {
        serde_json::json!({ "name": name })
    } else {
        serde_json::json!({ "path": reference })
    }
}

/// The `wallpaper.set` parameters for one target.
fn set_params(
    reference: &str,
    output: Option<&str>,
    transition: Option<&TransitionRequest>,
) -> serde_json::Value {
    let mut params = serde_json::Map::new();
    // An omitted `output` means "all outputs" — the daemon's default. Sending a
    // keyword by hand would restate that rule here, where it could drift.
    if let Some(name) = output {
        params.insert("output".to_string(), serde_json::json!(name));
    }
    params.insert("source".to_string(), source_params(reference));
    if let Some(transition) = transition {
        params.insert("transition".to_string(), serde_json::json!(transition));
    }
    serde_json::Value::Object(params)
}

/// `wallpaper.set` → what was applied.
///
/// `outputs` is the assignment the user made: empty means "every output", and one
/// or more names means exactly those. The daemon's target is a single name or a
/// keyword, so a multi-output assignment is one request per output over a *single*
/// connection — which is what makes "put this on two of my three monitors"
/// expressible at all, and keeps it one handshake instead of N.
fn assign_wallpaper_to(
    socket: &Path,
    reference: &str,
    outputs: &[String],
    transition: Option<&TransitionRequest>,
) -> Result<ApplyOutcome, IpcError> {
    let replies = with_daemon(socket, |client| {
        let mut replies = Vec::new();
        if outputs.is_empty() {
            replies.push(client.call(
                owe_ipc::protocol::method::WALLPAPER_SET,
                set_params(reference, None, transition),
            )?);
        } else {
            for output in outputs {
                replies.push(client.call(
                    owe_ipc::protocol::method::WALLPAPER_SET,
                    set_params(reference, Some(output.as_str()), transition),
                )?);
            }
        }
        Ok(replies)
    })?;

    let mut outcome = ApplyOutcome {
        reference: reference.to_string(),
        kind: String::new(),
        outputs: Vec::new(),
        notes: Vec::new(),
    };
    for reply in replies {
        if outcome.kind.is_empty() {
            outcome.kind = field_str(&reply, "kind");
        }
        outcome.outputs.extend(string_list(&reply, "outputs"));
        outcome.notes.extend(string_list(&reply, "notes"));
    }
    Ok(outcome)
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

fn stats_from(socket: &Path, output: Option<&str>) -> Result<StatsView, IpcError> {
    let mut params = serde_json::Map::new();
    if let Some(output) = output {
        params.insert("output".to_string(), serde_json::json!(output));
    }
    let reply = with_daemon(socket, |client| {
        client.call(
            owe_ipc::protocol::method::STATS_GET,
            serde_json::Value::Object(params),
        )
    })?;
    let stats = reply["stats"]
        .as_array()
        .map(|rows| rows.iter().map(stats_row_from).collect())
        .unwrap_or_default();
    Ok(StatsView {
        stats,
        rss_bytes: reply["rss_bytes"].as_u64(),
        paused: reply["paused"].as_bool().unwrap_or(false),
    })
}

fn playback_command_from(
    socket: &Path,
    output: Option<&str>,
    command: &serde_json::Value,
) -> Result<PlaybackResult, IpcError> {
    let mut params = serde_json::Map::new();
    if let Some(output) = output {
        params.insert("output".to_string(), serde_json::json!(output));
    }
    params.insert("cmd".to_string(), command.clone());
    let reply = with_daemon(socket, |client| {
        client.call(
            owe_ipc::protocol::method::PLAYBACK_CMD,
            serde_json::Value::Object(params),
        )
    })?;
    let states = reply["states"]
        .as_array()
        .map(|states| states.iter().map(playback_state_from).collect())
        .or_else(|| {
            reply
                .get("state")
                .filter(|value| value.is_object())
                .map(|state| vec![playback_state_from(state)])
        })
        .unwrap_or_default();
    Ok(PlaybackResult {
        ok: reply["ok"].as_bool().unwrap_or(true),
        states,
    })
}

/// `shell.status` → what the backend card shows.
///
/// A failure is not an error dialog: "cannot reach the daemon" is a state the card
/// displays, the same way the status bar does.
fn shell_status_from(socket: &Path) -> ShellStatus {
    let reply = match with_daemon(socket, |client| {
        client.call(
            owe_ipc::protocol::method::SHELL_STATUS,
            serde_json::json!({}),
        )
    }) {
        Ok(reply) => reply,
        Err(error) => return ShellStatus::unreachable(error.message),
    };

    let shell = &reply["shell"];
    let backends = shell["backends"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| BackendRow {
                    id: field_str(row, "id"),
                    confidence: field_str(row, "confidence"),
                    reason: field_str(row, "reason"),
                    selected: row["selected"].as_bool().unwrap_or(false),
                    mode: field_str(row, "mode"),
                })
                .collect()
        })
        .unwrap_or_default();

    ShellStatus {
        connected: true,
        backend: shell["backend"].as_str().map(str::to_string),
        reason: shell["reason"].as_str().map(str::to_string),
        mode: shell["mode"].as_str().unwrap_or("daemon-drawn").to_string(),
        routed: shell["routed"].as_bool().unwrap_or(false),
        patched: shell["patched"].as_bool().unwrap_or(false),
        detect_order: string_list(shell, "detect_order"),
        backends,
        events: reply
            .get("events")
            .filter(|value| !value.is_null())
            .cloned(),
        competing_tools: reply["competing_tools"]["notices"]
            .as_array()
            .map(|notes| {
                notes
                    .iter()
                    .filter_map(|note| note.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        runtime_note: None,
        error: None,
    }
}

/// `config.patch` → the new shell situation.
///
/// The daemon validates, applies and re-selects; this only shapes the request and
/// reads the answer, so there is exactly one implementation of "what a patch means".
fn patch_shell_from(socket: &Path, patch: &ShellPatch) -> Result<ShellStatus, IpcError> {
    let body = serde_json::to_value(patch)
        .map_err(|error| IpcError::unreachable(format!("cannot encode the patch: {error}")))?;
    let reply = with_daemon(socket, |client| {
        client.call(
            owe_ipc::protocol::method::CONFIG_PATCH,
            serde_json::json!({ "patch": body }),
        )
    })?;

    // The reply carries the patch's own note, and the status call right after it
    // carries the new state — so the card never has to guess what changed.
    let mut status = shell_status_from(socket);
    status.runtime_note = reply["note"].as_str().map(str::to_string);
    Ok(status)
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

/// One registered shell backend and what it says about this session (`shell.status`).
#[derive(Debug, Clone, Serialize)]
struct BackendRow {
    id: String,
    /// `strong`, `weak` or `none`.
    confidence: String,
    /// Why, in the backend's own words — the whole point of the card.
    reason: String,
    selected: bool,
    /// `daemon-drawn` or `shell-routed` for this backend under the live config.
    mode: String,
}

/// The shell situation, as the settings/status card shows it.
#[derive(Debug, Clone, Serialize)]
struct ShellStatus {
    connected: bool,
    backend: Option<String>,
    reason: Option<String>,
    mode: String,
    /// Whether the shell owns the pixels, which decides whether per-output
    /// assignment is possible at all in this mode.
    routed: bool,
    /// Whether the running config was changed by a patch instead of the file.
    patched: bool,
    detect_order: Vec<String>,
    backends: Vec<BackendRow>,
    /// Events from the shell's socket (`null` when the bus is not running).
    events: Option<serde_json::Value>,
    /// Competing wallpaper tools (hyprpaper/swww), one sentence each.
    competing_tools: Vec<String>,
    /// What the daemon said about a just-applied patch (e.g. "the config file is
    /// unchanged, so a restart restores the file's values").
    runtime_note: Option<String>,
    error: Option<String>,
}

impl ShellStatus {
    /// The state where the daemon could not be reached.
    fn unreachable(message: String) -> Self {
        Self {
            connected: false,
            backend: None,
            reason: None,
            mode: "daemon-drawn".to_string(),
            routed: false,
            patched: false,
            detect_order: Vec::new(),
            backends: Vec::new(),
            events: None,
            competing_tools: Vec::new(),
            runtime_note: None,
            error: Some(message),
        }
    }
}

/// One indexed wallpaper, as the grid renders it.
#[derive(Debug, Serialize)]
struct LibraryItem {
    id: i64,
    path: String,
    name: String,
    kind: String,
    /// `library:<id>` — what `wallpaper.set` accepts, so the UI never has to build
    /// a reference itself.
    reference: String,
    /// Cached thumbnail path, or `null` when the UI must call `library.thumbnail`.
    thumb: Option<String>,
    bytes: u64,
}

/// A page of the indexed library.
#[derive(Debug, Serialize)]
struct LibraryPage {
    items: Vec<LibraryItem>,
    total: u64,
    page: u32,
    pages: u64,
    per_page: u32,
    /// Longest edge the daemon generates thumbnails at, for grid sizing.
    thumbnail_size: u32,
    /// Roots the daemon is indexing, so the UI can say where it is looking.
    roots: Vec<String>,
}

/// What a scan did (the daemon's `ScanReport`, flattened for the UI).
#[derive(Debug, Serialize)]
struct ScanSummary {
    summary: String,
    roots: Vec<String>,
    added: u64,
    updated: u64,
    removed: u64,
    unchanged: u64,
    skipped_unsupported: u64,
    rows_touched: u64,
    files_seen: u64,
    /// Configured roots that were unreachable — the reason a library looks empty.
    missing_roots: Vec<String>,
    duration_ms: f64,
}

/// A materialised thumbnail.
#[derive(Debug, Serialize)]
struct Thumbnail {
    id: i64,
    /// Where the daemon cached it (shown in the UI's tooltip, not loaded by the
    /// webview — see [`library_thumb_from`]).
    path: String,
    size: u32,
    /// Whether it was already in the cache (vs generated for this request).
    cached: bool,
    /// The wallpaper it was made from.
    source: String,
    /// The PNG itself, as a `data:` URL.
    data_url: String,
}

/// A transition request: exactly the daemon's `{name, duration_ms, fps}` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransitionRequest {
    name: String,
    duration_ms: u64,
    fps: u32,
}

/// A runtime `[shell]` patch, as `config.patch` takes it: every field optional, and
/// the ones the UI does not set stay as they are.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellPatch {
    backend: Option<String>,
    caelestia_mode: Option<String>,
    theme_hook: Option<bool>,
    detect_order: Option<Vec<String>>,
}

fn playback_state_from(value: &serde_json::Value) -> PlaybackState {
    PlaybackState {
        output: field_str(value, "output"),
        kind: field_str(value, "kind"),
        playing: value["playing"].as_bool().unwrap_or(false),
        held: value["held"].as_bool().unwrap_or(false),
        position_ms: value["position_ms"].as_u64().unwrap_or(0),
        fps: value["fps"].as_f64().unwrap_or(0.0),
        fps_cap: value["fps_cap"].as_u64().map(|value| value as u32),
        decode: field_str(value, "decode"),
        decoder: field_str(value, "decoder"),
        mode: field_str(value, "mode"),
        buffers: value["buffers"].as_u64().unwrap_or(0) as usize,
        buffer_bytes: value["buffer_bytes"].as_u64().unwrap_or(0) as usize,
        buffer_cap_bytes: value["buffer_cap_bytes"].as_u64().unwrap_or(0) as usize,
        total_frames: value["total_frames"].as_u64(),
        failed: value["failed"].as_str().map(str::to_string),
    }
}

fn stats_row_from(value: &serde_json::Value) -> StatsRow {
    StatsRow {
        output: field_str(value, "output"),
        kind: field_str(value, "kind"),
        playing: value["playing"].as_bool().unwrap_or(false),
        held: value["held"].as_bool().unwrap_or(false),
        position_ms: value["position_ms"].as_u64().unwrap_or(0),
        fps: value["fps"].as_f64().unwrap_or(0.0),
        fps_cap: value["fps_cap"].as_u64().map(|value| value as u32),
        decode: field_str(value, "decode"),
        decoder: field_str(value, "decoder"),
        mode: field_str(value, "mode"),
        buffers: value["buffers"].as_u64().unwrap_or(0) as usize,
        buffer_bytes: value["buffer_bytes"].as_u64().unwrap_or(0) as usize,
        buffer_cap_bytes: value["buffer_cap_bytes"].as_u64().unwrap_or(0) as usize,
        total_frames: value["total_frames"].as_u64(),
        failed: value["failed"].as_str().map(str::to_string),
        wallpaper: value["wallpaper"].as_str().map(str::to_string),
        width: value["width"].as_u64().unwrap_or(0) as u32,
        height: value["height"].as_u64().unwrap_or(0) as u32,
        position: value["position"].as_u64(),
        frames_presented: value["frames_presented"].as_u64().unwrap_or(0),
        loop_start_ms: value["loop_start_ms"].as_u64(),
        loop_end_ms: value["loop_end_ms"].as_u64(),
    }
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

#[derive(Debug, Serialize)]
struct PlaybackState {
    output: String,
    kind: String,
    playing: bool,
    held: bool,
    position_ms: u64,
    fps: f64,
    fps_cap: Option<u32>,
    decode: String,
    decoder: String,
    mode: String,
    buffers: usize,
    buffer_bytes: usize,
    buffer_cap_bytes: usize,
    total_frames: Option<u64>,
    failed: Option<String>,
}

#[derive(Debug, Serialize)]
struct PlaybackResult {
    ok: bool,
    states: Vec<PlaybackState>,
}

#[derive(Debug, Serialize)]
struct StatsRow {
    output: String,
    kind: String,
    playing: bool,
    held: bool,
    position_ms: u64,
    fps: f64,
    fps_cap: Option<u32>,
    decode: String,
    decoder: String,
    mode: String,
    buffers: usize,
    buffer_bytes: usize,
    buffer_cap_bytes: usize,
    total_frames: Option<u64>,
    failed: Option<String>,
    wallpaper: Option<String>,
    width: u32,
    height: u32,
    position: Option<u64>,
    frames_presented: u64,
    loop_start_ms: Option<u64>,
    loop_end_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct StatsView {
    stats: Vec<StatsRow>,
    rss_bytes: Option<u64>,
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
    events: Vec<String>,
    /// Transition ids the picker may offer: whatever this build renders **and**
    /// `render.allow_transitions` currently permits.
    transitions: Vec<String>,
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
            events: Vec::new(),
            transitions: Vec::new(),
            unavailable: Vec::new(),
            error: Some(error),
        }
    }
}

/// Blocking IPC work — always called off the UI thread.
fn probe_daemon() -> DaemonStatus {
    probe_resolved_socket(
        std::env::var_os("OWE_SOCKET").as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
    )
}

fn probe_resolved_socket(
    override_socket: Option<&OsStr>,
    runtime_dir: Option<&OsStr>,
) -> DaemonStatus {
    let socket = match socket_from_values(override_socket, runtime_dir) {
        Ok(path) => path,
        Err(error) => return DaemonStatus::unreachable(String::new(), error.message),
    };
    probe_socket(&socket)
}

/// The status probe against a given socket.
///
/// Split out from [`probe_daemon`] for the same reason every other request path
/// takes its socket as an argument: it is what lets the tests read a stub daemon's
/// `hello` through the exact code the status bar uses.
fn probe_socket(socket: &Path) -> DaemonStatus {
    let socket_path = socket.display().to_string();

    let mut client = match owe_ipc::Client::connect_with_timeout(socket, PROBE_TIMEOUT) {
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
            events: reply.capabilities.events,
            transitions: reply.capabilities.transitions,
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

/// Query the indexed library (the daemon owns the index; the GUI owns no state).
#[tauri::command]
async fn library_index(query: LibraryQuery) -> Result<LibraryPage, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || library_index_from(&socket, &query)).await
}

/// Ask the daemon to rescan its configured roots.
#[tauri::command]
async fn library_scan(paths: Option<Vec<String>>) -> Result<ScanSummary, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || library_scan_from(&socket, paths)).await
}

/// Materialise (or fetch) the cached thumbnail for one library item.
///
/// Called per grid cell, not per page: a 500-file page would otherwise decode 500
/// images to display twelve.
#[tauri::command]
async fn library_thumbnail(id: i64) -> Result<Thumbnail, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || library_thumb_from(&socket, id)).await
}

/// Ask the daemon about its outputs (the state it also renders on).
#[tauri::command]
async fn list_outputs() -> Result<OutputsView, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || list_outputs_from(&socket)).await
}

#[tauri::command]
async fn stats(output: Option<String>) -> Result<StatsView, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || stats_from(&socket, output.as_deref())).await
}

#[tauri::command]
async fn playback_command(
    output: Option<String>,
    command: serde_json::Value,
) -> Result<PlaybackResult, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || playback_command_from(&socket, output.as_deref(), &command)).await
}

/// Apply a wallpaper to a set of outputs (empty means all of them), optionally
/// through a chosen transition.
#[tauri::command]
async fn assign_wallpaper(
    reference: String,
    outputs: Vec<String>,
    transition: Option<TransitionRequest>,
) -> Result<ApplyOutcome, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || assign_wallpaper_to(&socket, &reference, &outputs, transition.as_ref()))
        .await
}

/// Remove the wallpaper from one output (or all of them).
#[tauri::command]
async fn clear_wallpaper(output: Option<String>) -> Result<Vec<String>, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || clear_wallpaper_on(&socket, output.as_deref())).await
}

/// Ask the daemon which shell backend it is using, and why.
#[tauri::command]
async fn shell_status() -> Result<ShellStatus, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || Ok(shell_status_from(&socket))).await
}

/// Change the shell backend, draw mode or theme switch for the running daemon.
#[tauri::command]
async fn patch_shell(patch: ShellPatch) -> Result<ShellStatus, IpcError> {
    let socket = daemon_socket()?;
    run_blocking(move || patch_shell_from(&socket, &patch)).await
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
            library_index,
            library_scan,
            library_thumbnail,
            list_outputs,
            stats,
            playback_command,
            assign_wallpaper,
            clear_wallpaper,
            shell_status,
            patch_shell
        ])
        .run(tauri::generate_context!())
        .expect("failed to start the OWE window");
}

/// The record/replay harness (IMPLEMENTATION-PLAN Phase 2, "GUI mock-daemon test
/// harness"). Test-only: it exists to serve a recording to the tests, and nothing
/// in the shipped binary may depend on a fixture.
#[cfg(test)]
mod replay;

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
        /// Where the stub claims the cached thumbnail lives. Points nowhere until
        /// a test aims it at a real file, so "no thumbnail" is the default state.
        thumbnail_path: Mutex<String>,
    }

    impl StubDaemon {
        fn new() -> Self {
            Self {
                asks: Mutex::new(Vec::new()),
                hello_count: AtomicUsize::new(0),
                fail_with: None,
                thumbnail_path: Mutex::new("/nonexistent/thumb.png".to_string()),
            }
        }

        fn failing(code: ErrorCode, message: &str) -> Self {
            Self {
                fail_with: Some((code, message.to_string())),
                ..Self::new()
            }
        }

        /// Aim the stub's `library.thumb` reply at a real file.
        fn serve_thumbnail(&self, path: &Path) {
            *self.thumbnail_path.lock().expect("stub lock") = path.display().to_string();
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
                             "events": [],
                             "transitions": ["none", "fade", "wipe"],
                            "unavailable": ["caelestia: planned for P3"],
                        },
                    }))
                }
                method::LIBRARY_LIST => Ok(json!({
                    "items": [
                        {
                            "id" : 1,
                            "path": "/walls/a.png",
                            "name": "a.png",
                            "kind": "static-image",
                            "reference": "library:1",
                            "size_bytes": 10,
                            "thumb": "/cache/thumbs/256/aaa.png",
                        },
                        {
                            "id": 2,
                            "path": "/walls/b.jpg",
                            "name": "b.jpg",
                            "kind": "static-image",
                            "reference": "library:2",
                            "size_bytes": 20,
                            // Deliberately null: the UI must be able to tell "no
                            // thumbnail yet" from "thumbnail at the empty path".
                            "thumb": null,
                        },
                    ],
                    "total": 2,
                    "page": 1,
                    "per_page": 100,
                    "pages": 1,
                    "thumbnail_size": 256,
                    "roots": ["/walls"],
                })),
                method::LIBRARY_SCAN => Ok(json!({
                    "scan_id": 1,
                    "finished": true,
                    "summary": "scanned 1 root(s): 3 file(s) seen, +2 ~0 -0 (0 unchanged, 1 skipped)",
                    "roots": ["/walls"],
                    "added": 2,
                    "updated": 0,
                    "removed": 0,
                    "unchanged": 0,
                    "skipped_unsupported": 1,
                    "rows_touched": 2,
                    "files_seen": 3,
                    "missing_roots": ["/mnt/usb"],
                    "unreadable_dirs": [],
                    "duration_ms": 4.5,
                    "wall_clock_ms": 4.6,
                })),
                method::LIBRARY_THUMB => Ok(json!({
                    "id": request.params["id"],
                    "path": self.thumbnail_path.lock().expect("stub lock").clone(),
                    "size": 256,
                    "cached": false,
                    "source": "/walls/a.png",
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
                method::STATS_GET => Ok(json!({
                    "stats": [{
                        "output": "eDP-1",
                        "kind": "animated-image",
                        "wallpaper": "/tmp/walls/a.gif",
                        "playing": true,
                        "held": false,
                        "fps": 9.5,
                        "fps_cap": 60,
                        "decode": "software",
                        "decoder": "gif",
                        "mode": "cached",
                        "buffers": 3,
                        "buffer_bytes": 4096,
                        "buffer_cap_bytes": 8192,
                        "width": 1366,
                        "height": 768,
                        "position": 2,
                        "position_ms": 200,
                        "total_frames": 3,
                        "frames_presented": 12,
                        "loop_start_ms": null,
                        "loop_end_ms": null,
                        "failed": null
                    }],
                    "rss_bytes": 123456,
                    "paused": false
                })),
                method::PLAYBACK_CMD => Ok(json!({
                    "ok": true,
                    "state": {
                        "output": request.params["output"].clone(),
                        "kind": "animated-image",
                        "playing": false,
                        "held": false,
                        "position_ms": 200,
                        "fps": 9.5,
                        "fps_cap": 60,
                        "decode": "software",
                        "decoder": "gif",
                        "mode": "cached",
                        "buffers": 3,
                        "buffer_bytes": 4096,
                        "buffer_cap_bytes": 8192,
                        "total_frames": 3,
                        "failed": null
                    }
                })),
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
    fn the_library_page_reaches_the_shape_the_grid_renders() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let page = library_index_from(
            &socket,
            &LibraryQuery {
                filter: Some("a".to_string()),
                page: Some(2),
                scan_if_empty: Some(true),
                ..LibraryQuery::default()
            },
        )
        .expect("page");

        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].name, "a.png");
        assert_eq!(page.items[0].reference, "library:1");
        assert_eq!(page.items[1].bytes, 20);
        assert!(page.items[1].thumb.is_none(), "null must stay null");
        assert_eq!(page.total, 2);
        assert_eq!(page.pages, 1);
        assert_eq!(page.thumbnail_size, 256);
        assert_eq!(page.roots, vec!["/walls".to_string()]);

        let asks = daemon.asks.lock().unwrap();
        let (_, params) = asks.last().expect("one request recorded");
        assert_eq!(params["filter"], json!("a"));
        assert_eq!(params["page"], json!(2));
        assert_eq!(params["scan_if_empty"], json!(true));
        assert!(
            params.get("dir").is_none(),
            "an unset directory must be omitted so the daemon's roots stay authoritative"
        );
        assert_eq!(
            daemon.hello_count.load(Ordering::Relaxed),
            1,
            "every command must handshake first"
        );
    }

    #[test]
    fn a_scan_summary_reports_what_the_scan_did_and_what_it_could_not_reach() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let summary = library_scan_from(&socket, None).expect("scan");
        assert_eq!(summary.added, 2);
        assert_eq!(summary.skipped_unsupported, 1);
        assert_eq!(summary.files_seen, 3);
        assert_eq!(summary.rows_touched, 2);
        assert!((summary.duration_ms - 4.5).abs() < 1e-9);
        assert_eq!(
            summary.missing_roots,
            vec!["/mnt/usb".to_string()],
            "an unreachable root is the reason a library looks empty and must be shown"
        );

        {
            let asks = daemon.asks.lock().unwrap();
            let (_, params) = asks.last().expect("recorded");
            assert!(
                params.get("paths").is_none(),
                "no paths means the daemon's configured roots, so the key must be absent"
            );
        }

        // With explicit paths, they must be sent as given.
        library_scan_from(&socket, Some(vec!["/tmp/custom".to_string()])).expect("scan");
        {
            let asks = daemon.asks.lock().unwrap();
            let (_, params) = asks.last().expect("recorded");
            assert_eq!(params["paths"], json!(["/tmp/custom"]));
        }
    }

    #[test]
    fn a_thumbnail_is_inlined_as_a_data_url_and_its_cache_state_survives() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        // A real file: the command reads the bytes the daemon wrote, so the test has
        // to put bytes there. A stubbed path with nothing behind it is asserted in
        // its own test below.
        let store = tempfile::tempdir().expect("temp dir");
        let png = store.path().join("thumb.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G', 1, 2, 3]).expect("write png");
        daemon.serve_thumbnail(&png);

        let thumbnail = library_thumb_from(&socket, 1).expect("thumb");
        assert_eq!(thumbnail.id, 1);
        assert_eq!(thumbnail.path, png.display().to_string());
        assert_eq!(thumbnail.size, 256);
        assert!(
            !thumbnail.cached,
            "a freshly generated thumbnail is not cached"
        );
        assert_eq!(thumbnail.source, "/walls/a.png");
        assert_eq!(
            thumbnail.data_url, "data:image/png;base64,iVBORwECAw==",
            "the webview receives the bytes, never a path it would have to open"
        );

        {
            let asks = daemon.asks.lock().unwrap();
            let (_, params) = asks.last().expect("recorded");
            assert_eq!(params["id"], json!(1));
        }
    }

    #[test]
    fn a_thumbnail_that_cannot_be_read_is_reported_rather_than_shown_as_broken() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let failure = library_thumb_from(&socket, 3).expect_err("nothing at that path");
        assert_eq!(failure.code, "cache");
        assert!(
            failure.message.contains("/nonexistent/thumb.png"),
            "{failure:?}"
        );
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        // Padding is where a hand-rolled encoder goes wrong, so every remainder
        // class is pinned.
        for (input, expected) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg=="),
            (&b"fo"[..], "Zm8="),
            (&b"foo"[..], "Zm9v"),
            (&b"foob"[..], "Zm9vYg=="),
            (&b"fooba"[..], "Zm9vYmE="),
            (&b"foobar"[..], "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input), expected, "{input:?}");
        }
    }

    #[test]
    fn assigning_to_two_outputs_sends_both_over_one_connection() {
        // The GUI's "assign these two, not those" case. It must not collapse into
        // "all outputs", and it must not pay for two handshakes either.
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let outputs = vec!["eDP-1".to_string(), "HDMI-A-1".to_string()];
        let outcome = assign_wallpaper_to(&socket, "/walls/a.png", &outputs, None).expect("apply");
        assert_eq!(outcome.reference, "/walls/a.png");
        assert_eq!(outcome.kind, "static-image");
        assert_eq!(
            outcome.outputs,
            vec!["eDP-1".to_string(), "eDP-1".to_string()]
        );

        {
            let asks = daemon.asks.lock().unwrap();
            let sets: Vec<&Value> = asks
                .iter()
                .filter(|(method, _)| method == method::WALLPAPER_SET)
                .map(|(_, params)| params)
                .collect();
            assert_eq!(sets.len(), 2, "one request per assigned output");
            assert_eq!(sets[0]["output"], json!("eDP-1"));
            assert_eq!(sets[1]["output"], json!("HDMI-A-1"));
            assert_eq!(sets[0]["source"]["path"], json!("/walls/a.png"));
        }
        assert_eq!(
            daemon.hello_count.load(Ordering::Relaxed),
            1,
            "two outputs must not mean two handshakes"
        );
    }

    #[test]
    fn assigning_nothing_means_every_output() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        assign_wallpaper_to(&socket, "/walls/a.png", &[], None).expect("apply all");

        let asks = daemon.asks.lock().unwrap();
        let (_, params) = asks.last().expect("recorded");
        assert!(
            params.get("output").is_none(),
            "an omitted output means all, which is the daemon's default to state"
        );
    }

    #[test]
    fn a_library_reference_becomes_a_library_id_source() {
        // `library:<id>` is what the grid applies; sending it as a `path` would
        // make the daemon look for a file literally named `library:1`.
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        assign_wallpaper_to(&socket, "library:7", &["eDP-1".to_string()], None).expect("apply");
        {
            let asks = daemon.asks.lock().unwrap();
            let (_, params) = asks.last().expect("recorded");
            assert_eq!(params["source"]["library_id"], json!("7"));
            assert!(params["source"].get("path").is_none());
        }

        assign_wallpaper_to(&socket, "/walls/a.png", &[], None).expect("apply");
        {
            let asks = daemon.asks.lock().unwrap();
            let (_, params) = asks.last().expect("recorded");
            assert_eq!(params["source"]["path"], json!("/walls/a.png"));
        }
    }

    #[test]
    fn a_transition_request_uses_the_documented_table_form() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let transition = TransitionRequest {
            name: "wave".to_string(),
            duration_ms: 450,
            fps: 60,
        };
        assign_wallpaper_to(&socket, "/walls/a.png", &[], Some(&transition)).expect("apply");

        // Each lock is its own scope: holding the stub's mutex across the *next*
        // IPC call deadlocks the server on its own bookkeeping, which surfaces as a
        // 20-second client timeout rather than as a lock error.
        {
            let asks = daemon.asks.lock().unwrap();
            let (_, params) = asks.last().expect("recorded");
            assert_eq!(
                params["transition"],
                json!({ "name": "wave", "duration_ms": 450, "fps": 60 })
            );
        }

        // No transition chosen: the key must be absent, so the daemon's configured
        // default applies rather than a transition the GUI invented.
        assign_wallpaper_to(&socket, "/walls/a.png", &[], None).expect("apply");
        {
            let asks = daemon.asks.lock().unwrap();
            let (_, params) = asks.last().expect("recorded");
            assert!(params.get("transition").is_none());
        }
    }

    #[test]
    fn the_status_probe_honours_the_socket_override() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));
        let status = probe_resolved_socket(Some(socket.as_os_str()), None);
        assert!(status.connected);
        assert_eq!(status.socket_path, socket.display().to_string());
    }

    #[test]
    fn the_status_view_carries_the_transitions_the_picker_may_offer() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let status = probe_socket(&socket);
        assert!(status.connected);
        assert!(status.events.is_empty());
        assert_eq!(
            status.transitions,
            vec!["none".to_string(), "fade".to_string(), "wipe".to_string()],
            "the picker's options come from the daemon, not from a hardcoded list"
        );
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
    fn stats_and_playback_round_trip_through_the_command_layer() {
        let daemon = Arc::new(StubDaemon::new());
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let stats = stats_from(&socket, Some("eDP-1")).expect("stats");
        assert_eq!(stats.stats.len(), 1);
        assert_eq!(stats.stats[0].decode, "software");
        assert_eq!(stats.rss_bytes, Some(123456));

        let playback =
            playback_command_from(&socket, Some("eDP-1"), &serde_json::json!({ "seek": 0.2 }))
                .expect("playback");
        assert!(playback.ok);
        assert_eq!(playback.states.len(), 1);
        assert!(!playback.states[0].playing);

        let asks = daemon.asks.lock().unwrap();
        let stats_request = asks
            .iter()
            .find(|(method, _)| method == method::STATS_GET)
            .expect("stats request");
        assert_eq!(stats_request.1["output"], json!("eDP-1"));
        let playback_request = asks
            .iter()
            .find(|(method, _)| method == method::PLAYBACK_CMD)
            .expect("playback request");
        assert_eq!(playback_request.1["cmd"]["seek"], json!(0.2));
    }

    #[test]
    fn a_daemon_refusal_keeps_its_protocol_code() {
        // The UI shows `code` next to the message; losing it would turn "the file
        // is missing" and "the daemon exploded" into the same dialogue.
        let daemon = Arc::new(StubDaemon::failing(ErrorCode::NotFound, "no such file"));
        let (socket, _dir, _running) = start_stub(Arc::clone(&daemon));

        let failure = assign_wallpaper_to(&socket, "/nope.png", &[], None).expect_err("must fail");
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
            library_index_from(&missing, &LibraryQuery::default()).expect_err("no daemon"),
            library_scan_from(&missing, None).expect_err("no daemon"),
            library_thumb_from(&missing, 1).expect_err("no daemon"),
            stats_from(&missing, None).expect_err("no daemon"),
            playback_command_from(&missing, None, &serde_json::json!("pause"))
                .expect_err("no daemon"),
            assign_wallpaper_to(&missing, "/tmp/a.png", &[], None).expect_err("no daemon"),
            clear_wallpaper_on(&missing, None).expect_err("no daemon"),
        ] {
            assert_eq!(failure.code, "unreachable", "{}", failure.message);
            assert!(!failure.message.is_empty());
        }
    }
}
