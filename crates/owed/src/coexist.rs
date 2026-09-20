//! hyprpaper / swww coexistence (TRD §4).
//!
//! On a desktop that already runs a wallpaper daemon, two tools draw on the same
//! background layer and the result depends on who painted last. The rule from TRD §4
//! is deliberately narrow:
//!
//! - **hyprpaper** may be warned about or stopped, because it is the tool the user's
//!   wallpaper stack usually means and the config key names it
//!   (`shell.hyprland.hyprpaper = warn | stop | ignore`).
//! - **swww/awww** are detected and reported, never touched. OWE does not manage
//!   another daemon's state, and a `stop` that killed a different tool than the one
//!   the key names would be a serious surprise.
//! - **Nothing is killed silently.** A `stop` decision produces a notice *and* a
//!   pid, the caller logs the notice first, and the message says which config value
//!   caused it — so a process that disappears from the journal is explainable.
//!
//! The decision itself ([`owe_core::shell::decide_coexistence`]) is pure and lives in
//! core. This module is the machine-facing half: scan `/proc`, then act.

use owe_core::config::HyprlandConfig;
use owe_core::shell::{
    CoexistenceDecision, CoexistencePolicy, CompetingProcess, WallpaperTool, decide_coexistence,
};

/// What actually happened, for logs and for `shell.status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoexistenceReport {
    /// One sentence per tool worth telling the user about.
    pub notices: Vec<String>,
    /// Pids OWE asked the kernel to terminate.
    pub stopped: Vec<u32>,
    /// Pids a `stop` decision named but that could not be signalled, with the reason.
    pub failures: Vec<(u32, String)>,
}

impl CoexistenceReport {
    /// Whether there is nothing at all to say.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.notices.is_empty() && self.failures.is_empty()
    }

    /// A one-line summary for the startup log.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} notice(s), {} stopped, {} failure(s)",
            self.notices.len(),
            self.stopped.len(),
            self.failures.len()
        )
    }
}

/// The tools running right now, from a process table.
///
/// Pure, so a recorded table is a test. `WallpaperTool::from_command` decides what
/// counts as a wallpaper daemon, and it is the same function the user-facing messages
/// are built from — one place, so a false positive cannot mean warning about (or
/// stopping) a process nobody thinks of as a wallpaper tool.
pub fn scan(processes: &[String]) -> Vec<CompetingProcess> {
    processes
        .iter()
        .filter_map(|command| {
            let (pid, command_line) = split_pid(command);
            let tool = WallpaperTool::from_command(command_line)?;
            Some(CompetingProcess {
                tool,
                pid,
                command: command_line.trim().to_string(),
            })
        })
        .collect()
}

/// Split a `"<pid> <cmdline>"` entry, or an unpid'd command line.
///
/// The scan needs pids only for the `stop` path; a table that arrives without them
/// still produces detection and warnings, which is the honest degradation.
fn split_pid(command: &str) -> (u32, &str) {
    let (head, rest) = match command.split_once(' ') {
        Some((head, rest)) => (head, rest),
        None => (command, ""),
    };
    match head.trim().parse::<u32>() {
        Ok(pid) => (pid, rest),
        Err(_) => (0, command),
    }
}

/// The process table with pids, as [`scan`] wants it.
pub fn live_table() -> Vec<String> {
    let mut entries = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return entries;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let command = raw
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        if !command.is_empty() {
            entries.push(format!("{name} {command}"));
        }
    }
    entries
}

/// How to terminate a process. Injected so tests never signal a real one.
pub trait Signaller: Send + Sync + std::fmt::Debug {
    /// Ask the process to exit, returning a description of what was done.
    fn terminate(&self, pid: u32) -> Result<String, String>;
}

/// Signal through `/bin/kill`.
///
/// `SIGTERM`, not `SIGKILL`: hyprpaper flushes its state on a normal exit, and a
/// wallpaper daemon that dies without cleanup is the kind of thing a user notices
/// later. A process that ignores it stays running, and the journal says so — which is
/// better than the silence of a `SIGKILL`.
#[derive(Debug, Default)]
pub struct KillCommand;

impl Signaller for KillCommand {
    fn terminate(&self, pid: u32) -> Result<String, String> {
        let output = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .output()
            .map_err(|error| format!("could not run kill: {error}"))?;
        if output.status.success() {
            return Ok(format!("SIGTERM sent to {pid}"));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "`kill -TERM {pid}` failed with {}: {}",
            output.status,
            stderr.trim()
        ))
    }
}

/// The policy named by the config, or the safe default when the value is unparsable.
///
/// `warn` rather than `ignore`: a config file with a typo in this key should still
/// tell the user that another wallpaper tool is running, because the alternative is
/// two tools fighting over the background with no explanation anywhere.
pub fn policy(config: &HyprlandConfig) -> CoexistencePolicy {
    CoexistencePolicy::parse(&config.hyprpaper).unwrap_or(CoexistencePolicy::Warn)
}

/// Decide, then act.
///
/// The decision comes from core; this function only carries it out and reports what
/// happened. Notices are returned whether or not anything was stopped, so the caller
/// logs a complete story in one place.
pub fn resolve(
    processes: &[String],
    config: &HyprlandConfig,
    signaller: &dyn Signaller,
) -> CoexistenceReport {
    let decision: CoexistenceDecision = decide_coexistence(&scan(processes), policy(config));
    let mut report = CoexistenceReport {
        notices: decision.notices,
        ..CoexistenceReport::default()
    };

    for pid in decision.stop {
        match signaller.terminate(pid) {
            Ok(detail) => {
                report.notices.push(detail);
                report.stopped.push(pid);
            }
            Err(reason) => report.failures.push((pid, reason)),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(policy: &str) -> HyprlandConfig {
        HyprlandConfig {
            hyprpaper: policy.to_string(),
            event_socket: true,
        }
    }

    /// A signaller that records instead of signalling.
    #[derive(Debug, Default)]
    struct Recorder {
        killed: std::sync::Mutex<Vec<u32>>,
        fail: bool,
    }

    impl Signaller for Recorder {
        fn terminate(&self, pid: u32) -> Result<String, String> {
            self.killed.lock().expect("lock").push(pid);
            if self.fail {
                return Err(format!("no such process {pid}"));
            }
            Ok(format!("SIGTERM sent to {pid}"))
        }
    }

    fn table() -> Vec<String> {
        vec![
            "100 /usr/bin/hyprpaper".to_string(),
            "200 swww-daemon --format xrgb".to_string(),
            "300 /usr/bin/kitty".to_string(),
            "400 qs -c caelestia -n".to_string(),
        ]
    }

    #[test]
    fn the_scan_recognizes_wallpaper_daemons_and_nothing_else() {
        let found = scan(&table());
        let tools: Vec<&str> = found.iter().map(|process| process.tool.as_str()).collect();
        assert_eq!(tools, vec!["hyprpaper", "swww"]);
        assert_eq!(found[0].pid, 100);
        assert_eq!(found[1].pid, 200);
        assert!(
            found
                .iter()
                .all(|process| !process.command.contains("qs -c")),
            "a shell is not a wallpaper daemon"
        );
    }

    #[test]
    fn a_warning_policy_reports_without_touching_anything() {
        let signaller = Recorder::default();
        let report = resolve(&table(), &config("warn"), &signaller);
        assert_eq!(report.stopped, Vec::<u32>::new());
        assert!(signaller.killed.lock().unwrap().is_empty());
        assert_eq!(report.notices.len(), 2, "{:?}", report.notices);
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("hyprpaper is running"))
        );
        assert!(
            report.notices.iter().any(|notice| notice.contains("swww")
                && notice.contains("does not manage another daemon")),
            "the swww notice must say it is not ours to stop: {:?}",
            report.notices
        );
    }

    #[test]
    fn stopping_signals_hyprpaper_and_leaves_swww_alone() {
        let signaller = Recorder::default();
        let report = resolve(&table(), &config("stop"), &signaller);
        assert_eq!(report.stopped, vec![100], "hyprpaper only");
        assert_eq!(signaller.killed.lock().unwrap().as_slice(), &[100]);
        assert_eq!(report.notices.len(), 3, "{:?}", report.notices);
        assert!(
            report
                .notices
                .iter()
                .any(|notice| notice.contains("shell.hyprland.hyprpaper")),
            "the notice names the setting that caused it: {:?}",
            report.notices
        );
    }

    #[test]
    fn ignore_is_silent_about_everything() {
        let signaller = Recorder::default();
        let report = resolve(&table(), &config("ignore"), &signaller);
        assert!(report.is_quiet(), "{report:?}");
        assert!(signaller.killed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_signal_is_reported_rather_than_swallowed() {
        let signaller = Recorder {
            fail: true,
            ..Recorder::default()
        };
        let report = resolve(&table(), &config("stop"), &signaller);
        assert!(report.stopped.is_empty());
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].1.contains("no such process"));
        assert!(
            !report.is_quiet(),
            "a stop that did not happen is something to say"
        );
    }

    #[test]
    fn a_clean_desktop_produces_no_notices() {
        let signaller = Recorder::default();
        let clean: Vec<String> = vec!["500 qs -c caelestia -n".to_string()];
        let report = resolve(&clean, &config("stop"), &signaller);
        assert!(report.is_quiet(), "{report:?}");
        assert!(
            report.summary().contains("0 notice"),
            "{}",
            report.summary()
        );
    }

    #[test]
    fn the_scan_tolerates_entries_without_a_pid() {
        // The pid is only needed for the `stop` path; detection must still work.
        let found = scan(&["hyprpaper".to_string()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, 0);
    }

    #[test]
    fn an_unparsable_policy_is_a_warning_not_a_kill() {
        assert_eq!(policy(&config("kll")), CoexistencePolicy::Warn);
    }
}
