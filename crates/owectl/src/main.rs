//! `owectl` — the OWE control CLI.
//!
//! Thin by design: it speaks the same IPC client as the GUI, holds no state, and
//! never reimplements daemon logic (ARCHITECTURE §1). Commands that belong to a
//! later phase still exist here and report the daemon's `UNSUPPORTED` answer
//! verbatim, so scripts written today keep working as phases land.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use owe_core::path::XdgPaths;
use owe_ipc::protocol::method;
use owe_ipc::{Client, ClientError};
use serde_json::{Value, json};

/// Exit code for a structured error from the daemon.
const EXIT_SERVER_ERROR: u8 = 1;
/// Exit code for a transport problem (no daemon, timeout, protocol violation).
const EXIT_CONNECTION_ERROR: u8 = 2;

/// Control the OWE daemon over its IPC socket.
#[derive(Debug, Parser)]
#[command(
    name = "owectl",
    version,
    about = "Control the OWE wallpaper daemon",
    long_about = None
)]
struct Cli {
    /// Daemon socket path (default: $XDG_RUNTIME_DIR/owe/socket).
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Emit raw JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Handshake with the daemon and print its version and capabilities.
    Hello,

    /// List outputs, their wallpaper, and their policy.
    Monitors,

    /// Print the current wallpaper of one output (or all).
    Get {
        /// Output name (default: all outputs).
        #[arg(short, long)]
        monitor: Option<String>,
    },

    /// Apply a wallpaper: a path, `library:<id>`, or `shader:<name>`.
    Set {
        /// What to display.
        target: String,

        /// Output name; omit to apply to every output.
        #[arg(short, long)]
        monitor: Option<String>,

        /// Transition id (see the config's `render.allow_transitions`).
        #[arg(long)]
        transition: Option<String>,

        /// Transition duration in milliseconds (with `--transition`).
        #[arg(long, default_value_t = 300)]
        duration_ms: u64,

        /// Transition frame rate (with `--transition`).
        #[arg(long, default_value_t = 60)]
        fps: u32,
    },

    /// Ask the governor to stop drawing (manual override).
    Pause {
        /// Output name; omit for all outputs.
        monitor: Option<String>,
    },

    /// Clear a manual pause.
    Resume {
        /// Output name; omit for all outputs.
        monitor: Option<String>,
    },

    /// Remove the wallpaper from one output (or all of them).
    Clear {
        /// Output name; omit for all outputs.
        monitor: Option<String>,
    },

    /// Browse and rescan the indexed wallpaper library.
    #[command(subcommand)]
    Library(LibraryCommand),

    /// Shell integration: which backend is live, and how wallpapers are drawn.
    #[command(subcommand)]
    Shell(ShellCommand),

    /// Ask the daemon to shut down.
    Kill,
}

/// `owectl library …`: the indexed library (FR-LIB-1/2).
#[derive(Debug, Subcommand)]
enum LibraryCommand {
    /// Rescan the configured folders (or an explicit list of folders).
    Scan {
        /// Folders to scan; omit to use `library.paths` from the config.
        paths: Vec<String>,
    },

    /// List indexed wallpapers, paged and filtered.
    List {
        /// Substring matched against the name and path.
        #[arg(long)]
        filter: Option<String>,

        /// Restrict to a directory.
        #[arg(long)]
        dir: Option<String>,

        /// Restrict to a content kind id (`static-image`, …).
        #[arg(long)]
        kind: Option<String>,

        /// 1-based page number.
        #[arg(long, default_value_t = 1)]
        page: u32,

        /// Rows per page.
        #[arg(long, default_value_t = 50)]
        per_page: u32,
    },

    /// Generate (or find) the cached thumbnail for one item.
    Thumb {
        /// Library id, as printed by `owectl library list`.
        id: i64,
    },
}

/// `owectl shell …`: shell-backend inspection and switching (FR-SHELL-3/4).
#[derive(Debug, Subcommand)]
enum ShellCommand {
    /// Which shell backend is live, why it was chosen, and how wallpapers are drawn.
    Status,

    /// Switch backend, draw mode or theme hook at runtime (`config.patch`).
    Patch {
        /// Backend id to select (`auto`, `hyprland`, `caelestia`, `generic`).
        #[arg(long)]
        backend: Option<String>,

        /// Draw mode: `daemon-drawn` or `shell-routed`.
        #[arg(long)]
        mode: Option<String>,

        /// Enable or disable the shell's own theme switching for OWE wallpapers.
        #[arg(long)]
        theme_hook: Option<bool>,

        /// Auto-detection order (backend ids, most preferred first).
        #[arg(long, value_delimiter = ',')]
        detect_order: Option<Vec<String>>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("owectl: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn resolve_socket(cli: &Cli) -> Result<PathBuf, Failure> {
    if let Some(path) = &cli.socket {
        return Ok(path.clone());
    }
    let paths = XdgPaths::resolve().map_err(|error| {
        Failure::new(
            EXIT_CONNECTION_ERROR,
            format!("cannot resolve XDG paths: {error}"),
        )
    })?;
    paths.socket_path().map_err(|error| {
        Failure::new(
            EXIT_CONNECTION_ERROR,
            format!("cannot determine the daemon socket: {error}"),
        )
    })
}

fn run(cli: &Cli) -> Result<(), Failure> {
    let socket = resolve_socket(cli)?;
    let mut client = Client::connect(&socket).map_err(|error| {
        Failure::new(
            EXIT_CONNECTION_ERROR,
            format!(
                "cannot connect to {}: {error} (is owed running?)",
                socket.display()
            ),
        )
    })?;

    // Mandatory handshake: it also tells us the schema we are speaking.
    let hello = client
        .hello("owectl", env!("CARGO_PKG_VERSION"))
        .map_err(map_client_error)?;

    if cli.json {
        let value = match &cli.command {
            Command::Hello => json!({
                "server_version": hello.server_version,
                "schema": hello.schema,
                "capabilities": hello.capabilities,
            }),
            other => dispatch(&mut client, other)?,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        return Ok(());
    }

    match &cli.command {
        Command::Hello => {
            println!(
                "daemon {} (ipc schema {}.{})",
                hello.server_version, hello.schema.major, hello.schema.minor
            );
            println!(
                "implemented methods: {}",
                hello.capabilities.methods.join(", ")
            );
            println!(
                "shell backends: {}",
                hello.capabilities.shell_backends.join(", ")
            );
            println!(
                "content kinds: {}",
                hello.capabilities.content_kinds.join(", ")
            );
            println!(
                "media backends: {}",
                hello.capabilities.media_backends.join(", ")
            );
            // Planned-but-missing features are shown, not hidden: a user
            // comparing the roadmap to the build should not have to infer them.
            if !hello.capabilities.unavailable.is_empty() {
                println!("not in this build:");
                for entry in &hello.capabilities.unavailable {
                    println!("  {entry}");
                }
            }
            Ok(())
        }
        other => {
            let value = dispatch(&mut client, other)?;
            println!("{}", render(other, &value));
            Ok(())
        }
    }
}

/// Human-readable rendering of a reply.
///
/// The CLI is the surface most users touch first, so a raw JSON dump is a bug,
/// not a default: `owectl monitors` exists to answer "what is on my screens?".
fn render(command: &Command, value: &Value) -> String {
    match command {
        Command::Hello => unreachable!("hello is rendered before dispatch"),
        Command::Monitors | Command::Get { .. } => render_outputs(command, value),
        Command::Set { .. } => {
            let outputs = value["outputs"].as_array().cloned().unwrap_or_default();
            let mut lines = vec![format!(
                "applied {} ({}) to {}",
                value["reference"].as_str().unwrap_or("?"),
                value["kind"].as_str().unwrap_or("?"),
                join_strings(&outputs)
            )];
            if let Some(sizes) = value["sizes"].as_array() {
                for size in sizes {
                    lines.push(format!(
                        "  {} presented at {}x{}",
                        size[0].as_str().unwrap_or("?"),
                        size[1].as_u64().unwrap_or(0),
                        size[2].as_u64().unwrap_or(0)
                    ));
                }
            }
            // Which transition ran, and over how many frames, is the difference
            // between "it faded" and "it snapped" — so it is printed, not implied.
            if let Some(transitions) = value["transitions"].as_array() {
                for entry in transitions {
                    lines.push(format!(
                        "  {}: transition {}",
                        entry[0].as_str().unwrap_or("?"),
                        entry[1].as_str().unwrap_or("?")
                    ));
                }
            }
            if let Some(frames) = value["frames"].as_array() {
                for entry in frames {
                    let count = entry[1].as_u64().unwrap_or(0);
                    if count > 1 {
                        lines.push(format!(
                            "  {}: {count} frames rendered",
                            entry[0].as_str().unwrap_or("?")
                        ));
                    }
                }
            }
            append_notes(&mut lines, value);
            lines.join("\n")
        }
        Command::Pause { .. } | Command::Resume { .. } => {
            let mut lines = vec![if value["paused"] == json!(true) {
                "paused".to_string()
            } else {
                "resumed".to_string()
            }];
            if let Some(affects) = value["affects"].as_str() {
                lines.push(format!("  affects: {affects}"));
            }
            lines.join("\n")
        }
        Command::Clear { .. } => {
            let cleared = value["cleared"].as_array().cloned().unwrap_or_default();
            if cleared.is_empty() {
                "nothing was set on the matching output(s)".to_string()
            } else {
                format!("cleared {}", join_strings(&cleared))
            }
        }
        Command::Kill => "daemon is shutting down".to_string(),
        Command::Library(command) => render_library(command, value),
        Command::Shell(command) => render_shell(command, value),
    }
}

/// Human-readable rendering of a `shell …` reply.
///
/// `shell.status` is the answer to "why does my wallpaper look the way it does?",
/// so every backend gets its own line with its own reason, and the competing-tool
/// notices are printed verbatim — they are the daemon speaking, not us paraphrasing.
fn render_shell(command: &ShellCommand, value: &Value) -> String {
    match command {
        ShellCommand::Status | ShellCommand::Patch { .. } => {
            let shell = &value["shell"];
            let mut lines = Vec::new();
            if let Some(note) = value["note"].as_str() {
                lines.push(note.to_string());
            }
            let backend = shell["backend"].as_str().unwrap_or("none");
            let mode = shell["mode"].as_str().unwrap_or("daemon-drawn");
            let reason = shell["reason"].as_str().unwrap_or("");
            lines.push(format!(
                "backend {backend} ({mode}){}",
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(": {reason}")
                }
            ));
            if let Some(order) = shell["detect_order"].as_array() {
                let order: Vec<&str> = order.iter().filter_map(|entry| entry.as_str()).collect();
                if !order.is_empty() {
                    lines.push(format!("detect order: {}", order.join(" → ")));
                }
            }
            for row in shell["backends"].as_array().cloned().unwrap_or_default() {
                let selected = if row["selected"].as_bool().unwrap_or(false) {
                    "*"
                } else {
                    " "
                };
                lines.push(format!(
                    " {} {} [{}] {}",
                    selected,
                    row["id"].as_str().unwrap_or("?"),
                    row["confidence"].as_str().unwrap_or("?"),
                    row["reason"].as_str().unwrap_or("")
                ));
            }
            for notice in value["competing_tools"]["notices"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                if let Some(text) = notice.as_str() {
                    lines.push(format!("! {text}"));
                }
            }
            lines.join("\n")
        }
    }
}

/// Human-readable rendering of a `library.*` reply.
fn render_library(command: &LibraryCommand, value: &Value) -> String {
    match command {
        LibraryCommand::Scan { .. } => {
            let mut lines = vec![format!(
                "scanned {} ({})",
                join_strings(&value["roots"].as_array().cloned().unwrap_or_default()),
                value["summary"].as_str().unwrap_or("?")
            )];
            // Missing roots are the reason a library looks empty; say so loudly
            // rather than reporting a successful scan of nothing.
            for root in value["missing_roots"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                lines.push(format!(
                    "warning: `{}` does not exist (nothing indexed from it)",
                    root.as_str().unwrap_or("?")
                ));
            }
            lines.push(format!(
                "  rows touched: {} (files seen: {})",
                value["rows_touched"].as_u64().unwrap_or(0),
                value["files_seen"].as_u64().unwrap_or(0)
            ));
            lines.join("\n")
        }
        LibraryCommand::List { .. } => {
            let items = value["items"].as_array().cloned().unwrap_or_default();
            let mut lines = vec![format!(
                "{} items (page {} of {}, {} total)",
                items.len(),
                value["page"].as_u64().unwrap_or(1),
                value["pages"].as_u64().unwrap_or(0),
                value["total"].as_u64().unwrap_or(0)
            )];
            for item in items {
                lines.push(format!(
                    "  {}  {}  {}  {}",
                    item["id"].as_i64().unwrap_or(0),
                    item["kind"].as_str().unwrap_or("?"),
                    item["name"].as_str().unwrap_or("?"),
                    match item["thumb"].as_str() {
                        Some(path) => format!("thumb: {path}"),
                        None => "thumb: (not generated)".to_string(),
                    }
                ));
            }
            lines.join("\n")
        }
        LibraryCommand::Thumb { id } => format!(
            "thumbnail for {} ({}, {} px): {}",
            id,
            if value["cached"] == json!(true) {
                "already cached"
            } else {
                "generated"
            },
            value["size"].as_u64().unwrap_or(0),
            value["path"].as_str().unwrap_or("?")
        ),
    }
}

/// Render `outputs.list` for `monitors` (a table) or `get` (the wallpaper).
fn render_outputs(command: &Command, value: &Value) -> String {
    let wanted = match command {
        Command::Get { monitor } => monitor.as_deref(),
        _ => None,
    };
    let empty = Vec::new();
    let outputs = value["outputs"].as_array().unwrap_or(&empty);
    let matching: Vec<&Value> = outputs
        .iter()
        .filter(|output| match wanted {
            None => true,
            Some(name) => output["name"].as_str() == Some(name),
        })
        .collect();

    if matching.is_empty() {
        return match wanted {
            Some(name) => format!("no output named {name}"),
            None => "no outputs".to_string(),
        };
    }

    if wanted.is_some() {
        return matching
            .iter()
            .map(|output| output["wallpaper"].as_str().unwrap_or("none").to_string())
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut lines = Vec::new();
    for output in &matching {
        lines.push(format!(
            "{}  {}x{}  {}{}",
            output["name"].as_str().unwrap_or("?"),
            output["width"].as_u64().unwrap_or(0),
            output["height"].as_u64().unwrap_or(0),
            if output["focused"] == json!(true) {
                "focused  "
            } else {
                ""
            },
            output["state"].as_str().unwrap_or("?")
        ));
        lines.push(format!(
            "    {}",
            output["description"].as_str().unwrap_or("")
        ));
        lines.push(format!(
            "    wallpaper: {}",
            output["wallpaper"].as_str().unwrap_or("none")
        ));
        // A reference the session file remembers but this run has not applied is
        // shown as what it is. Restore (P2) normally closes this gap at startup, so
        // seeing it means the restore itself failed — which is worth saying.
        if let Some(recorded) = output["recorded"].as_str() {
            lines.push(format!(
                "    recorded: {recorded} (not on screen; the startup restore did not apply it)"
            ));
        }
        if let Some(error) = output["error"].as_str() {
            lines.push(format!("    error: {error}"));
        }
    }
    if value["paused"] == json!(true) {
        lines.push(String::from("governor: paused"));
    }
    append_notes(&mut lines, value);
    lines.join("\n")
}

/// Append any `notes` array — the daemon's way of saying "this worked, but not
/// the way you may think".
fn append_notes(lines: &mut Vec<String>, value: &Value) {
    if let Some(notes) = value["notes"].as_array() {
        for note in notes {
            if let Some(text) = note.as_str() {
                lines.push(format!("note: {text}"));
            }
        }
    }
}

/// Join a JSON string array for display.
fn join_strings(values: &[Value]) -> String {
    if values.is_empty() {
        return "no outputs".to_string();
    }
    values
        .iter()
        .map(|value| value.as_str().unwrap_or("?"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn dispatch(client: &mut Client, command: &Command) -> Result<Value, Failure> {
    match command {
        Command::Hello => Err(Failure::new(
            EXIT_SERVER_ERROR,
            "internal: hello is handled before dispatch",
        )),
        Command::Monitors | Command::Get { .. } => {
            client.call(method::OUTPUTS_LIST, json!({})).map_err(map_client_error)
        }
        Command::Set {
            target,
            monitor,
            transition,
            duration_ms,
            fps,
        } => client
            .call(
                method::WALLPAPER_SET,
                set_params(
                    target,
                    monitor.as_deref(),
                    transition.as_deref(),
                    *duration_ms,
                    *fps,
                ),
            )
            .map_err(map_client_error),
        Command::Pause { monitor } => client
            .call(
                method::GOVERNOR_OVERRIDE,
                json!({ "output": monitor.clone().unwrap_or_else(|| "all".into()), "policy": "pause" }),
            )
            .map_err(map_client_error),
        Command::Resume { monitor } => client
            .call(
                method::GOVERNOR_OVERRIDE,
                json!({ "output": monitor.clone().unwrap_or_else(|| "all".into()), "policy": "auto" }),
            )
            .map_err(map_client_error),
        Command::Clear { monitor } => client
            .call(
                method::WALLPAPER_CLEAR,
                json!({ "output": monitor.clone().unwrap_or_else(|| "all".into()) }),
            )
            .map_err(map_client_error),
        Command::Library(command) => dispatch_library(client, command),
        Command::Shell(command) => dispatch_shell(client, command),
        Command::Kill => client
            .call(method::DAEMON_KILL, json!({}))
            .map_err(map_client_error),
    }
}

/// Wire a `shell …` command to its method (FR-SHELL-3/4).
fn dispatch_shell(client: &mut Client, command: &ShellCommand) -> Result<Value, Failure> {
    match command {
        ShellCommand::Status => client
            .call(method::SHELL_STATUS, json!({}))
            .map_err(map_client_error),
        ShellCommand::Patch {
            backend,
            mode,
            theme_hook,
            detect_order,
        } => {
            let mut patch = serde_json::Map::new();
            if let Some(backend) = backend {
                patch.insert("backend".into(), json!(backend));
            }
            if let Some(mode) = mode {
                patch.insert("caelestia_mode".into(), json!(mode));
            }
            if let Some(theme_hook) = theme_hook {
                patch.insert("theme_hook".into(), json!(theme_hook));
            }
            if let Some(order) = detect_order {
                patch.insert("detect_order".into(), json!(order));
            }
            if patch.is_empty() {
                return Err(Failure::new(
                    EXIT_CONNECTION_ERROR,
                    "a patch needs at least one of --backend, --mode, --theme-hook, --detect-order",
                ));
            }
            client
                .call(method::CONFIG_PATCH, json!({ "patch": patch }))
                .map_err(map_client_error)
        }
    }
}

fn dispatch_library(client: &mut Client, command: &LibraryCommand) -> Result<Value, Failure> {
    match command {
        LibraryCommand::Scan { paths } => {
            let params = if paths.is_empty() {
                json!({})
            } else {
                json!({ "paths": paths })
            };
            client
                .call(method::LIBRARY_SCAN, params)
                .map_err(map_client_error)
        }
        LibraryCommand::List {
            filter,
            dir,
            kind,
            page,
            per_page,
        } => {
            let mut params = json!({ "page": page, "per_page": per_page });
            if let Some(filter) = filter {
                params["filter"] = json!(filter);
            }
            if let Some(dir) = dir {
                params["dir"] = json!(dir);
            }
            if let Some(kind) = kind {
                params["kind"] = json!(kind);
            }
            client
                .call(method::LIBRARY_LIST, params)
                .map_err(map_client_error)
        }
        LibraryCommand::Thumb { id } => client
            .call(method::LIBRARY_THUMB, json!({ "id": id }))
            .map_err(map_client_error),
    }
}

/// Parameters for `wallpaper.set` (protocol v1.1).
fn set_params(
    target: &str,
    monitor: Option<&str>,
    transition: Option<&str>,
    duration_ms: u64,
    fps: u32,
) -> Value {
    let mut params = json!({
        "output": monitor.unwrap_or("all"),
        "source": target,
    });
    if let Some(transition) = transition {
        // Always the table form: it carries the timing the user asked for, and the
        // daemon accepts a bare name only as a convenience.
        params["transition"] = json!({
            "name": transition,
            "duration_ms": duration_ms,
            "fps": fps,
        });
    }
    params
}

fn map_client_error(error: ClientError) -> Failure {
    match error {
        ClientError::Server(body) => Failure::new(EXIT_SERVER_ERROR, format!("{body}")),
        other => Failure::new(EXIT_CONNECTION_ERROR, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use owe_ipc::protocol;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn set_params_default_to_all_outputs() {
        let params = set_params("/data/wall.png", None, None, 300, 60);
        assert_eq!(params["output"], json!("all"));
        assert_eq!(params["source"], json!("/data/wall.png"));
        assert!(params.get("transition").is_none());
    }

    #[test]
    fn set_params_carry_monitor_and_transition_timing() {
        let params = set_params("shader:aurora", Some("DP-1"), Some("slide"), 450, 30);
        assert_eq!(params["output"], json!("DP-1"));
        assert_eq!(params["source"], json!("shader:aurora"));
        assert_eq!(params["transition"]["name"], json!("slide"));
        assert_eq!(params["transition"]["duration_ms"], json!(450));
        assert_eq!(params["transition"]["fps"], json!(30));
    }

    #[test]
    fn subcommands_parse_as_documented() {
        let cli = Cli::try_parse_from(["owectl", "set", "~/wall.png", "-m", "DP-1"]).unwrap();
        match cli.command {
            Command::Set {
                target, monitor, ..
            } => {
                assert_eq!(target, "~/wall.png");
                assert_eq!(monitor.as_deref(), Some("DP-1"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        assert!(matches!(
            Cli::try_parse_from(["owectl", "kill"]).unwrap().command,
            Command::Kill
        ));
        assert!(matches!(
            Cli::try_parse_from(["owectl", "resume", "DP-2"])
                .unwrap()
                .command,
            Command::Resume { .. }
        ));
    }

    #[test]
    fn json_flag_is_global() {
        let cli = Cli::try_parse_from(["owectl", "hello", "--json"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn protocol_methods_used_here_exist_in_v1() {
        // Guards against typos that would only surface against a live daemon.
        for named in [
            method::OUTPUTS_LIST,
            method::WALLPAPER_SET,
            method::GOVERNOR_OVERRIDE,
            method::DAEMON_KILL,
            method::LIBRARY_SCAN,
            method::LIBRARY_LIST,
            method::LIBRARY_THUMB,
        ] {
            assert!(protocol::method::ALL.contains(&named), "{named}");
        }
    }
}
