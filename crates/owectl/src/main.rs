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

    /// List image files in a directory.
    ///
    /// P1 scans one level; the indexed library with thumbnails is P2.
    List {
        /// Directory to scan.
        #[arg(default_value = "~/Pictures")]
        dir: String,
    },

    /// Ask the daemon to shut down.
    Kill,
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
        Command::List { .. } => {
            let entries = value["entries"].as_array().cloned().unwrap_or_default();
            let mut lines = vec![format!(
                "{} ({} entries)",
                value["dir"].as_str().unwrap_or("?"),
                entries.len()
            )];
            for entry in entries {
                lines.push(format!(
                    "  {}  {}x{}  {}",
                    entry["name"].as_str().unwrap_or("?"),
                    entry["width"].as_u64().unwrap_or(0),
                    entry["height"].as_u64().unwrap_or(0),
                    entry["path"].as_str().unwrap_or("")
                ));
            }
            if let Some(scope) = value["scope"].as_str() {
                lines.push(format!("  scope: {scope}"));
            }
            lines.join("\n")
        }
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
        // shown as what it is. Printing it as the wallpaper would claim something
        // is on screen when it is not (restore lands in P2).
        if let Some(recorded) = output["recorded"].as_str() {
            lines.push(format!(
                "    recorded: {recorded} (not applied this run; restore lands in P2)"
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
        } => client
            .call(
                method::WALLPAPER_SET,
                set_params(target, monitor.as_deref(), transition.as_deref()),
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
        Command::List { dir } => client
            .call(method::LIBRARY_LIST, json!({ "dir": dir }))
            .map_err(map_client_error),
        Command::Kill => client
            .call(method::DAEMON_KILL, json!({}))
            .map_err(map_client_error),
    }
}

/// Parameters for `wallpaper.set` (protocol v1).
fn set_params(target: &str, monitor: Option<&str>, transition: Option<&str>) -> Value {
    let mut params = json!({
        "output": monitor.unwrap_or("all"),
        "source": target,
    });
    if let Some(transition) = transition {
        params["transition"] = json!({ "name": transition });
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
        let params = set_params("/data/wall.png", None, None);
        assert_eq!(params["output"], json!("all"));
        assert_eq!(params["source"], json!("/data/wall.png"));
        assert!(params.get("transition").is_none());
    }

    #[test]
    fn set_params_carry_monitor_and_transition() {
        let params = set_params("shader:aurora", Some("DP-1"), Some("slide"));
        assert_eq!(params["output"], json!("DP-1"));
        assert_eq!(params["source"], json!("shader:aurora"));
        assert_eq!(params["transition"]["name"], json!("slide"));
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
        ] {
            assert!(protocol::method::ALL.contains(&named), "{named}");
        }
    }
}
