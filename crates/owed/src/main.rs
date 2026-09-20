//! `owed` — the OWE daemon.
//!
//! P1 scope: configuration handling (`--check-config`, `--dump-config`), IPC
//! hosting with an honest capability set, the render engine (static images on
//! layer-shell background surfaces), session state, and clean exit codes.
//!
//! Exit codes (docs/BACKEND-DESIGN.md §8):
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | clean shutdown |
//! | 1 | configuration invalid |
//! | 2 | session/Wayland environment unavailable |
//! | 3 | startup blocked (e.g. another instance owns the socket) |

mod cli;
mod engine;
mod handler;
mod hotplug;
mod library;

use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use owe_core::path::XdgPaths;
use owe_ipc::{Server, ServerError, Shutdown};
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::engine::Engine;
use crate::handler::{DaemonState, IpcHandler};
use crate::hotplug::HotplugDriver;
use crate::library::LibraryService;

/// Process exit codes (see the table in the module docs).
pub mod exit_code {
    /// Clean shutdown.
    pub const OK: u8 = 0;
    /// Configuration invalid.
    pub const CONFIG_INVALID: u8 = 1;
    /// Session/Wayland environment unavailable.
    pub const SESSION_UNAVAILABLE: u8 = 2;
    /// Startup blocked by a precondition (another instance, missing backend).
    pub const STARTUP_BLOCKED: u8 = 3;
}

/// A fatal startup error and the exit code it maps to.
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    match run(&cli) {
        Ok(code) => ExitCode::from(code),
        Err(failure) => {
            // Diagnostics go to stderr; stdout stays machine-readable.
            eprintln!("owed: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

fn init_logging(verbose: bool) {
    let default_level = if verbose { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!("owed={default_level},owe_ipc={default_level}"))
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// Load the configuration, honouring two rules:
///
/// - A **missing default** config file is not an error: a fresh install runs on
///   built-in defaults (an always-running daemon that refuses to start because a
///   file nobody created is absent would be hostile).
/// - A missing file the user **explicitly** pointed at with `--config` is an
///   error, because they meant that file.
fn load_config(cli: &Cli, path: &std::path::Path) -> Result<owe_core::Config, Failure> {
    if !path.exists() {
        if cli.config.is_some() {
            return Err(Failure::new(
                exit_code::CONFIG_INVALID,
                format!("cannot read config file `{}`: no such file", path.display()),
            ));
        }
        tracing::info!(
            path = %path.display(),
            "no config file found; using built-in defaults"
        );
        return Ok(owe_core::Config::default());
    }

    owe_core::Config::load(path)
        .map_err(|error| Failure::new(exit_code::CONFIG_INVALID, error.to_string()))
}

fn run(cli: &Cli) -> Result<u8, Failure> {
    let paths = XdgPaths::resolve().map_err(|error| {
        Failure::new(
            exit_code::SESSION_UNAVAILABLE,
            format!("cannot resolve XDG paths: {error}"),
        )
    })?;

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| paths.config_file.clone());

    // Inspection modes: no socket, no session required.
    if cli.check_config {
        let config = load_config(cli, &config_path)?;
        println!(
            "config OK: {} (schema {})",
            config_path.display(),
            config.schema
        );
        return Ok(exit_code::OK);
    }

    if cli.dump_config {
        let config = load_config(cli, &config_path)?;
        let text = config
            .to_toml_string()
            .map_err(|error| Failure::new(exit_code::CONFIG_INVALID, error.to_string()))?;
        println!("{text}");
        return Ok(exit_code::OK);
    }

    let socket_path = paths.socket_path().map_err(|error| {
        Failure::new(
            exit_code::SESSION_UNAVAILABLE,
            format!("{error} (owed needs a running session to serve IPC)"),
        )
    })?;

    if cli.print_socket {
        println!("{}", socket_path.display());
        return Ok(exit_code::OK);
    }

    // Daemon run.
    let config = load_config(cli, &config_path)?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_path.display(),
        backend = %config.shell.backend,
        "owed starting"
    );

    // Start the engine before serving: its report is the honest summary of what
    // this session can actually do, and it never fails (see Engine::start).
    let engine = Arc::new(Engine::new(config.clone(), paths.clone()));
    let report = engine.start();

    match (&report.backend, &report.backend_error) {
        (Some(id), _) => {
            let names: Vec<&str> = report
                .outputs
                .iter()
                .map(|output| output.name.as_str())
                .collect();
            tracing::info!(
                backend = %id,
                outputs = %if names.is_empty() { "none".to_string() } else { names.join(", ") },
                "shell backend ready"
            );
        }
        (None, Some(error)) => {
            tracing::warn!(%error, "no shell backend available; serving ipc only");
        }
        (None, None) => {}
    }

    if report.presenter {
        tracing::info!(
            outputs = %report.presentable.join(", "),
            "wallpaper rendering available"
        );
    } else {
        tracing::warn!(
            "wallpaper rendering is unavailable in this session (no Wayland session): \
             `wallpaper.set` will fail with a precise reason"
        );
    }

    for warning in &report.warnings {
        tracing::warn!("{warning}");
    }

    // The library: indexed folders plus the thumbnail worker. Opening it never
    // fails fatally (a broken database degrades to an in-memory index with a
    // warning), and the engine needs the resolver before any `library:<id>` apply.
    let library = Arc::new(LibraryService::open(&config, &paths));
    for warning in library.warnings() {
        tracing::warn!("{warning}");
    }
    engine.set_library(Arc::clone(&library) as Arc<dyn crate::engine::LibraryResolver>);

    // Initial scan: the GUI's library page must have something to show, and a
    // scan of an unchanged library is one stat per file. Failures are logged and
    // swallowed — a daemon that refuses to serve IPC because a wallpaper folder is
    // unreadable would be useless exactly when the user is fixing that folder.
    match library.scan(None) {
        Ok(report) => tracing::info!(summary = %report.summary(), "library indexed"),
        Err(error) => tracing::warn!(%error, "library scan failed; the index may be stale"),
    }

    // Session restore (FR-LIB-5): put back what was on screen before this run.
    let restored = engine.restore();
    for (output, reference) in &restored.restored {
        tracing::info!(%output, %reference, "restored wallpaper");
    }
    for (output, reason) in &restored.skipped {
        tracing::debug!(%output, %reason, "nothing to restore");
    }
    for (output, reason) in &restored.failures {
        tracing::warn!(%output, %reason, "restore failed");
    }

    // Hotplug: the presenter's output events drive re-apply and teardown
    // (FR-LIB-4). The listener blocks on the channel, so it costs nothing until
    // a monitor is actually plugged or unplugged.
    let hotplug = engine
        .take_presenter_events()
        .map(|events| HotplugDriver::spawn(events, Arc::clone(&engine)));

    // One shutdown signal shared by the IPC handler and the accept loop.
    let shutdown = Shutdown::new();
    let state = Arc::new(DaemonState::new(
        config,
        socket_path.clone(),
        shutdown.clone(),
        Arc::clone(&engine),
        Arc::clone(&library),
    ));
    let server = Server::bind_with_shutdown(
        &socket_path,
        Arc::new(IpcHandler::new(Arc::clone(&state))),
        shutdown,
    )
    .map_err(|error| match error {
        ServerError::AlreadyRunning(path) => Failure::new(
            exit_code::STARTUP_BLOCKED,
            format!(
                "another owed instance is already listening on {}",
                path.display()
            ),
        ),
        other => Failure::new(
            exit_code::SESSION_UNAVAILABLE,
            format!("cannot serve the ipc socket: {other}"),
        ),
    })?;

    tracing::info!(
        socket = %server.socket_path().display(),
        "owed listening (ipc protocol v{}.{})",
        owe_ipc::protocol::SCHEMA_MAJOR,
        owe_ipc::protocol::SCHEMA_MINOR
    );

    // P0: accept loop on a dedicated thread; P1 registers the listener as a
    // calloop source together with the Wayland and shell-event sources.
    server.serve_blocking().map_err(|error| {
        Failure::new(
            exit_code::SESSION_UNAVAILABLE,
            format!("ipc server stopped unexpectedly: {error}"),
        )
    })?;

    // Shutdown order: the presenter first (this is what releases the hotplug
    // listener's blocking recv), then the listener, then the leftovers.
    engine.stop_presenter();
    drop(hotplug);

    let stats = library.thumbnail_stats();
    tracing::info!(
        thumbnails = stats.generated,
        served_from_cache = stats.served_from_cache,
        failures = stats.failures,
        "owed stopped"
    );
    Ok(exit_code::OK)
}
