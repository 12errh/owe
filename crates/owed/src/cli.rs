//! Command-line interface of the daemon.

use std::path::PathBuf;

use clap::Parser;

/// owed — the OWE wallpaper daemon.
///
/// Renders wallpapers on layer-shell surfaces and serves the control IPC.
/// Rendering lands in P1; P0 ships configuration handling and the IPC server.
#[derive(Debug, Parser)]
#[command(
    name = "owed",
    version,
    about = "OWE daemon: renders wallpapers and serves the control IPC",
    long_about = None,
)]
pub struct Cli {
    /// Validate the configuration and exit (exit code 1 on any problem).
    #[arg(long)]
    pub check_config: bool,

    /// Print the effective configuration as TOML and exit.
    #[arg(long)]
    pub dump_config: bool,

    /// Print the IPC socket path this daemon uses and exit.
    #[arg(long)]
    pub print_socket: bool,

    /// Configuration file (default: $XDG_CONFIG_HOME/owe/config.toml).
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Verbose logging (debug level).
    #[arg(short, long)]
    pub verbose: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // Catches duplicate/conflicting flags at test time, not at runtime.
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_check_config_with_path() {
        let cli =
            Cli::try_parse_from(["owed", "--check-config", "--config", "/tmp/c.toml"]).unwrap();
        assert!(cli.check_config);
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/c.toml")));
        assert!(!cli.verbose);
    }

    #[test]
    fn parses_bare_invocation_as_daemon_run() {
        let cli = Cli::try_parse_from(["owed"]).unwrap();
        assert!(!cli.check_config && !cli.dump_config && !cli.print_socket);
        assert!(cli.config.is_none());
    }
}
