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
            Ok(())
        }
        other => {
            let value = dispatch(&mut client, other)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_default()
            );
            Ok(())
        }
    }
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
