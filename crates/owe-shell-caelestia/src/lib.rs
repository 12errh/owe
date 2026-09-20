//! Caelestia Shell backend: detection, and shell-routed wallpaper changes via the
//! pinned `caelestia` CLI.
//!
//! # What Caelestia is, and why it gets a backend
//!
//! Caelestia is a Quickshell desktop shell (bar, launcher, notifications, theme)
//! that also owns the wallpaper. On a Caelestia desktop the shell has already
//! drawn a wallpaper, generated a Material You scheme from it, and repainted every
//! widget to match. OWE has two honest ways to live there (TRD §4):
//!
//! - **`shell-routed`** — the shell keeps the pixels and OWE asks it to switch,
//!   through the `caelestia` CLI. The shell runs its own theming pipeline, so OWE
//!   must not run a second one (the double-theme bug class).
//! - **`daemon-drawn`** — OWE draws on its layer surface as everywhere else, and
//!   the shell's own wallpaper is irrelevant (usually because the user turned it
//!   off).
//!
//! The mode is config (`shell.caelestia.mode`), not a compile-time choice, because
//! which one is right depends on whether the user wants the shell's theme to follow
//! OWE's wallpapers.
//!
//! # OQ-2: the CLI surface is pinned, and it does not match the plan
//!
//! FR-SHELL-3 described the routing command as
//! `caelestia wallpaper -f <file> [-m <monitor>]`. The `-m` half is wrong. The
//! recorded help (`tests/fixtures/cli/wallpaper-help.txt`, captured from
//! caelestia-shell 2.5.0 by `scripts/record-caelestia-cli.sh`) lists exactly
//! `-p/--print`, `-r/--random`, `-f/--file`, `-n/--no-filter`, `-t/--threshold`
//! and `-N/--no-smart` — and the shell's IPC surface
//! (`tests/fixtures/cli/shell-ipc.txt`) has `wallpaper.set(path: string)` with no
//! monitor argument at all. There is no per-monitor wallpaper target to call.
//!
//! So this backend sets the wallpaper **for the session**, and a request for one
//! named output is refused with a message that says why and what to use instead.
//! Silently setting every monitor when one was asked for would be exactly the kind
//! of half-truth this project keeps out of the code. The deviation is recorded as
//! ADR-017 in `docs/ARCHITECTURE.md` §9.
//!
//! # The rest of the recorded surface
//!
//! | Fact | Where it comes from |
//! |------|---------------------|
//! | `caelestia wallpaper -f <path>` switches the wallpaper | `wallpaper-help.txt` |
//! | `-N/--no-smart` skips the colour-derived scheme update | `wallpaper-help.txt` |
//! | `caelestia wallpaper` with no flags prints the current wallpaper | `wallpaper-help.txt`, and the shell source it is generated from |
//! | wallpapers live in `$CAELESTIA_WALLPAPERS_DIR`, else `$XDG_PICTURES_DIR/Wallpapers` | `caelestia.utils.paths` (shell source, same version) |
//! | the shell runs as `qs -c caelestia` | process table of the reference session |
//!
//! Every one of those is a *capture*, not a recollection, and the tests read the
//! captures to check the command shapes this crate builds.

use std::path::{Path, PathBuf};
use std::process::Command;

use owe_core::config::{CaelestiaConfig, ShellConfig};
use owe_core::output::OutputInfo;
use owe_core::path::expand_with;
use owe_core::shell::{ApplyOutcome, Detection, DrawMode, EnvLookup, ShellBackend, ShellError};

/// The id this backend registers under (`shell.backend = caelestia`).
pub const ID: &str = "caelestia";

/// The binary every routing call goes through.
pub const BINARY: &str = "caelestia";

/// Flag that names the wallpaper to switch to (recorded: `-f, --file FILE`).
const FLAG_FILE: &str = "-f";

/// Flag that suppresses the colour-derived scheme update (recorded:
/// `-N, --no-smart`).
const FLAG_NO_SMART: &str = "-N";

/// The Quickshell config name Caelestia runs under: `qs -c caelestia`.
const SHELL_CONFIG: &str = "caelestia";

/// The process table, re-exported from `owe-core` so this crate's callers do not
/// need to know where it lives.
///
/// The scan itself is shared with the hyprpaper/swww coexistence check: two readers
/// of `/proc` would eventually disagree about what is running.
pub use owe_core::shell::process_table;

/// Whether one command line is the Caelestia shell.
///
/// The shell is Quickshell running Caelestia's config: `qs -c caelestia -n`, or
/// `quickshell --config caelestia`. A `qs -c celestia-memory` (a user's own
/// Quickshell config) is deliberately **not** a match: routing a wallpaper into
/// somebody's unrelated shell would fail confusingly, and claiming the shell is
/// present when it isn't is how `auto` picks the wrong backend.
///
/// Pure, so the recorded process list is a unit test instead of a live dependency.
pub fn is_shell_process(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    let Some(program) = words.first() else {
        return false;
    };
    let program = program.rsplit('/').next().unwrap_or_default();
    if program != "qs" && program != "quickshell" {
        return false;
    }

    // `-c name`, `--config name`, `-cname`, `--config=name`.
    let mut index = 1;
    while index < words.len() {
        let word = words[index];
        if word == "-c" || word == "--config" {
            return words
                .get(index + 1)
                .is_some_and(|name| *name == SHELL_CONFIG);
        }
        if let Some(name) = word.strip_prefix("-c") {
            if !name.is_empty() {
                return name == SHELL_CONFIG;
            }
        }
        if let Some(name) = word.strip_prefix("--config=") {
            return name == SHELL_CONFIG;
        }
        index += 1;
    }
    false
}

/// The Caelestia shell process in a process table, if any.
pub fn find_shell_process(processes: &[String]) -> Option<String> {
    processes
        .iter()
        .find(|command| is_shell_process(command))
        .cloned()
}

/// Find `caelestia` on `PATH`.
///
/// Takes the environment rather than reading it so the search is testable, and
/// checks the executable bit rather than mere existence: a directory or a data
/// file named `caelestia` is not a CLI, and reporting it as one would turn a
/// missing binary into a confusing exec error at apply time.
pub fn find_binary(env: EnvLookup<'_>) -> Option<PathBuf> {
    let path = env("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(BINARY);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Detection from an explicit process table and environment.
///
/// Three levels, because they mean different things to a user (see
/// [`Detection`]):
///
/// - **strong** — the shell is running: a routed change will land on a live bar.
/// - **weak** — the CLI is installed but the shell is not running. Worth using
///   when the user names it, not worth winning `auto` over a backend that works
///   right now.
/// - **none** — neither.
pub fn detect_from(env: EnvLookup<'_>, processes: &[String]) -> Detection {
    if let Some(command) = find_shell_process(processes) {
        return Detection::strong(format!("the Caelestia shell is running ({command})"));
    }

    match find_binary(env) {
        Some(path) => Detection::weak(format!(
            "the {} CLI is installed at {} but no `qs -c {SHELL_CONFIG}` process is running",
            BINARY,
            path.display()
        )),
        None => Detection::none(format!(
            "no `{BINARY}` on PATH and no `qs -c {SHELL_CONFIG}` process"
        )),
    }
}

/// The wallpapers directory this session's shell uses (FR-SHELL-3).
///
/// Precedence, pinned against the shell's own `utils/paths.py`:
///
/// 1. `shell.caelestia.wallpapers_dir` when it resolves to a real path.
/// 2. `$CAELESTIA_WALLPAPERS_DIR`, the shell's own override.
/// 3. `$XDG_PICTURES_DIR/Wallpapers`, else `$HOME/Pictures/Wallpapers` — the
///    shell's documented default.
///
/// The config default is the literal `$CAELESTIA_WALLPAPERS_DIR`, which
/// [`expand_with`] deliberately leaves unexpanded when the variable is unset (a
/// config file must stay valid when the shell is not running). A still-`$`-bearing
/// result is therefore treated as "the shell decides" and falls through to the
/// same default the shell would compute.
pub fn wallpapers_dir(env: EnvLookup<'_>, config: &CaelestiaConfig) -> Option<PathBuf> {
    if let Some(configured) = &config.wallpapers_dir
        && let Ok(expanded) = expand_with(configured, |name| env(name))
        && !expanded.to_string_lossy().contains('$')
    {
        return Some(expanded);
    }

    if let Some(dir) = env("CAELESTIA_WALLPAPERS_DIR")
        && !dir.trim().is_empty()
    {
        return Some(PathBuf::from(dir));
    }

    let pictures = env("XDG_PICTURES_DIR")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|home| PathBuf::from(home).join("Pictures")))?;
    Some(pictures.join("Wallpapers"))
}

/// Resolve a wallpaper reference for the shell.
///
/// Relative paths are resolved against [`wallpapers_dir`], because that is what a
/// user types: `owectl set sunset.jpg` is the file the shell's own picker shows.
/// Absolute paths are passed through untouched — the shell accepts any image, not
/// only ones inside its directory.
pub fn resolve_wallpaper(
    env: EnvLookup<'_>,
    config: &CaelestiaConfig,
    wallpaper: &str,
) -> Result<PathBuf, ShellError> {
    let trimmed = wallpaper.trim();
    if trimmed.is_empty() {
        return Err(ShellError::Backend {
            backend: ID.to_string(),
            detail: "empty wallpaper path".to_string(),
        });
    }

    let as_path = PathBuf::from(trimmed);
    if as_path.is_absolute() {
        return Ok(as_path);
    }

    let dir = wallpapers_dir(env, config).ok_or_else(|| ShellError::Unavailable {
        backend: ID.to_string(),
        detail: format!(
            "`{trimmed}` is a relative path and the wallpapers directory cannot be \
             determined: set `shell.caelestia.wallpapers_dir`, or $CAELESTIA_WALLPAPERS_DIR"
        ),
    })?;
    Ok(dir.join(as_path))
}

/// The argv for a routed wallpaper change: what the backend runs, exactly.
///
/// Pure, and the stub-CLI test asserts this against the recorded help so a flag
/// this build does not really have cannot sneak in.
///
/// `theme_hook = false` adds `-N/--no-smart`, which is the shell's own switch for
/// "do not derive the scheme mode from the wallpaper colour". That is what makes
/// the setting mean something rather than being recorded and ignored: with it off,
/// the shell changes the wallpaper and leaves the theme alone.
pub fn wallpaper_argv(binary: &Path, wallpaper: &Path, theme_hook: bool) -> Vec<String> {
    let mut argv = vec![
        binary.display().to_string(),
        "wallpaper".to_string(),
        FLAG_FILE.to_string(),
        wallpaper.display().to_string(),
    ];
    if !theme_hook {
        argv.push(FLAG_NO_SMART.to_string());
    }
    argv
}

/// The argv for reading the shell's current wallpaper.
///
/// `caelestia wallpaper` with no flags prints the current path (recorded help:
/// every option is optional, and the CLI's fallback branch prints the stored
/// path). One process spawn, no state files guessed at.
pub fn current_wallpaper_argv(binary: &Path) -> Vec<String> {
    vec![binary.display().to_string(), "wallpaper".to_string()]
}

/// Interpret `caelestia wallpaper` output.
///
/// The shell prints the path, or the words `No wallpaper set`. Whitespace is
/// trimmed; anything else is the path. `None` means "the shell says there is
/// none", which is different from an error and is reported as such.
pub fn parse_current_wallpaper(stdout: &str) -> Option<String> {
    let text = stdout.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("no wallpaper set") {
        return None;
    }
    Some(text.to_string())
}

/// Render an argv as one shell-ish line, for logs and IPC replies.
pub fn describe(argv: &[String]) -> String {
    argv.iter()
        .map(|word| {
            if word.contains(char::is_whitespace) {
                format!("'{word}'")
            } else {
                word.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run a command, mapping every failure mode onto an honest [`ShellError`].
fn run(argv: &[String]) -> Result<String, ShellError> {
    let (program, args) = argv.split_first().ok_or_else(|| ShellError::Backend {
        backend: ID.to_string(),
        detail: "empty command".to_string(),
    })?;

    let output =
        Command::new(program)
            .args(args)
            .output()
            .map_err(|error| ShellError::Unavailable {
                backend: ID.to_string(),
                detail: format!("could not run `{}`: {error}", describe(argv)),
            })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(ShellError::Backend {
            backend: ID.to_string(),
            detail: format!(
                "`{}` exited with {}{}",
                describe(argv),
                output.status,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            ),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A process table and an environment, injectable so tests are hermetic.
///
/// The environment is a closure rather than a snapshot because it is read at every
/// call: `PATH` can gain a CLI mid-run, and `$CAELESTIA_WALLPAPERS_DIR` is part of
/// the routing decision.
type EnvSource = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The Caelestia [`ShellBackend`].
///
/// Holds the process table and the environment as closures so tests can hand it a
/// recorded process list, a stub `PATH` and a stub wallpapers directory. The binary
/// path is resolved per call, not stored: a user who installs the CLI while the
/// daemon runs should not need to restart it.
#[derive(Clone)]
pub struct CaelestiaBackend {
    processes: std::sync::Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    env: EnvSource,
}

impl std::fmt::Debug for CaelestiaBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaelestiaBackend")
            .field("id", &ID)
            .finish_non_exhaustive()
    }
}

impl Default for CaelestiaBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CaelestiaBackend {
    /// The backend, reading the live process table and the live environment.
    pub fn new() -> Self {
        Self {
            processes: std::sync::Arc::new(process_table),
            env: std::sync::Arc::new(|name| std::env::var(name).ok()),
        }
    }

    /// The backend with an injected process table (tests, and a future
    /// `--dump-detection` that must not depend on a running shell).
    pub fn with_processes(
        processes: std::sync::Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    ) -> Self {
        Self {
            processes,
            env: std::sync::Arc::new(|name| std::env::var(name).ok()),
        }
    }

    /// The backend with both injected. Used by the stub-CLI integration test,
    /// which supplies a `PATH` holding a recording stub and nothing else.
    pub fn with_env(
        processes: std::sync::Arc<dyn Fn() -> Vec<String> + Send + Sync>,
        env: EnvSource,
    ) -> Self {
        Self { processes, env }
    }

    /// The environment this backend was built with.
    pub fn env_lookup(&self) -> EnvLookup<'_> {
        &*self.env
    }

    /// The path to the CLI, or the reason there is none.
    fn binary(&self, env: EnvLookup<'_>) -> Result<PathBuf, ShellError> {
        find_binary(env).ok_or_else(|| ShellError::Unavailable {
            backend: ID.to_string(),
            detail: format!("no `{BINARY}` on PATH"),
        })
    }

    /// The wallpaper the shell reports as current.
    ///
    /// Not part of [`ShellBackend`]: only a shell that owns the pixels has a
    /// current wallpaper to report, and a trait method every backend returned
    /// `Ok(None)` from would be a method no caller could trust.
    pub fn current_wallpaper(&self, env: EnvLookup<'_>) -> Result<Option<String>, ShellError> {
        let binary = self.binary(env)?;
        let argv = current_wallpaper_argv(&binary);
        let stdout = run(&argv)?;
        Ok(parse_current_wallpaper(&stdout))
    }
}

impl ShellBackend for CaelestiaBackend {
    fn id(&self) -> &'static str {
        ID
    }

    fn detect(&self, env: EnvLookup<'_>) -> bool {
        detect_from(env, &(self.processes)()).is_detected()
    }

    fn detection(&self, env: EnvLookup<'_>) -> Detection {
        detect_from(env, &(self.processes)())
    }

    /// Caelestia's outputs are Hyprland's: the shell runs on Hyprland and its own
    /// monitor handling goes through `hyprctl` (`caelestia.utils.hypr.message`).
    ///
    /// Delegated rather than reimplemented so there is one parser for
    /// `hyprctl monitors -j` in the tree. A session with no Hyprland signature is
    /// not an error state this backend claims to support, and the error says so.
    fn list_outputs(&self) -> Result<Vec<OutputInfo>, ShellError> {
        if (self.env)("HYPRLAND_INSTANCE_SIGNATURE")
            .map(|value| value.trim().is_empty())
            .unwrap_or(true)
        {
            return Err(ShellError::Unavailable {
                backend: ID.to_string(),
                detail: "Caelestia runs on Hyprland and this session has no \
                         HYPRLAND_INSTANCE_SIGNATURE; outputs cannot be listed"
                    .to_string(),
            });
        }
        let json = owe_shell_hyprland::run_hyprctl(&["monitors", "-j"])?;
        owe_shell_hyprland::parse_monitors(&json)
    }

    fn draw_mode(&self, config: &ShellConfig) -> DrawMode {
        DrawMode::parse(&config.caelestia.mode).unwrap_or(DrawMode::DaemonDrawn)
    }

    /// Ask the shell to switch wallpaper (FR-SHELL-3).
    ///
    /// `output: None` means "the whole session", which is the only thing the
    /// recorded CLI can express. A named output is refused with the reason and the
    /// way out, never silently broadened to every monitor.
    fn apply_wallpaper(
        &self,
        config: &ShellConfig,
        output: Option<&str>,
        wallpaper: &str,
    ) -> Result<ApplyOutcome, ShellError> {
        if let Some(name) = output {
            return Err(ShellError::Backend {
                backend: ID.to_string(),
                detail: format!(
                    "Caelestia cannot put a wallpaper on `{name}` alone: the recorded CLI \
                     (tests/fixtures/cli/wallpaper-help.txt) and the shell IPC \
                     (`wallpaper.set(path)`) have no per-monitor target. Set \
                     `shell.caelestia.mode = \"daemon-drawn\"` to address outputs \
                     individually, or drop `-o {name}` to change the whole session"
                ),
            });
        }

        let env = self.env_lookup();
        let binary = self.binary(env)?;
        let path = resolve_wallpaper(env, &config.caelestia, wallpaper)?;

        // The CLI validates the image itself and fails with `is not a valid
        // image`. Checking first turns that into a message that names the path
        // OWE resolved, which is the part that surprises people (a relative path
        // resolved against the shell's wallpapers directory, not the cwd).
        if !path.is_file() {
            return Err(ShellError::Backend {
                backend: ID.to_string(),
                detail: format!(
                    "`{}` does not exist; a relative path is resolved against the Caelestia \
                     wallpapers directory ({})",
                    path.display(),
                    wallpapers_dir(env, &config.caelestia)
                        .map(|dir| dir.display().to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                ),
            });
        }

        let argv = wallpaper_argv(&binary, &path, config.caelestia.theme_hook);
        run(&argv)?;

        Ok(ApplyOutcome::Routed {
            detail: describe(&argv),
            theme_refreshed: config.caelestia.theme_hook,
        })
    }

    /// Caelestia always has a wallpaper: its state files, its thumbnail symlink and
    /// its scheme all point at one, and the CLI's only mode is "set to this image".
    ///
    /// A shell-routed clear is therefore refused rather than approximated, and the
    /// message says what to do instead. (`daemon-drawn` mode clears normally: OWE
    /// owns that surface.)
    fn clear_wallpaper(
        &self,
        config: &ShellConfig,
        output: Option<&str>,
    ) -> Result<ApplyOutcome, ShellError> {
        let _ = output;
        let _ = config;
        Err(ShellError::Backend {
            backend: ID.to_string(),
            detail: "Caelestia cannot express `no wallpaper`: it always keeps one and its \
                     theme is derived from it. Switch to another image, or set \
                     `shell.caelestia.mode = \"daemon-drawn\"` to have OWE own the surface"
                .to_string(),
        })
    }
}

/// The `release` the shell reports, from `caelestia -v` output.
///
/// Presented in `shell.status` so a bug report says which shell version produced
/// it. The recorded capture is multi-line and verbose; the `Shell:` section line
/// is the part that names a version, and the parse returns `None` rather than
/// guessing when it is absent (a distro package may print something else entirely).
pub fn parse_shell_version(version_output: &str) -> Option<String> {
    version_output.lines().map(str::trim).find_map(|line| {
        line.strip_prefix("caelestia-shell ").map(|rest| {
            rest.split_whitespace()
                .next()
                .unwrap_or(rest)
                .trim_end_matches(',')
                .to_string()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WALLPAPER_HELP: &str = include_str!("../tests/fixtures/cli/wallpaper-help.txt");
    const SHELL_IPC: &str = include_str!("../tests/fixtures/cli/shell-ipc.txt");
    const VERSION: &str = include_str!("../tests/fixtures/cli/version.txt");

    /// The reference session's process table, as recorded while writing this
    /// backend: Quickshell running Caelestia, a second Quickshell config, and the
    /// usual noise. Kept inline because it is short and its *shape* is the point.
    fn reference_processes() -> Vec<String> {
        vec![
            "/bin/sh -c PATH=... qs -c celestia-memory".to_string(),
            "qs -c celestia-memory".to_string(),
            "/usr/local/bin/qs -c caelestia -n".to_string(),
            "/usr/bin/kitty".to_string(),
        ]
    }

    fn env_with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    fn config(mode: &str, theme_hook: bool) -> ShellConfig {
        ShellConfig {
            caelestia: CaelestiaConfig {
                mode: mode.to_string(),
                theme_hook,
                ..CaelestiaConfig::default()
            },
            ..ShellConfig::default()
        }
    }

    // --- detection ----------------------------------------------------------

    #[test]
    fn the_shell_is_recognized_by_its_quickshell_config() {
        assert!(is_shell_process("/usr/local/bin/qs -c caelestia -n"));
        assert!(is_shell_process(
            "quickshell -ccaerestia"
                .replace("-ccaerestia", "-ccaelestia")
                .as_str()
        ));
        assert!(is_shell_process("quickshell --config caelestia"));
        assert!(is_shell_process("quickshell --config=caelestia"));
    }

    #[test]
    fn another_quickshell_config_is_not_caelestia() {
        // Recorded from the reference machine: the maintainer also runs a personal
        // Quickshell config. Matching it would route wallpapers into the wrong shell.
        assert!(!is_shell_process("qs -c celestia-memory"));
        assert!(!is_shell_process("qs"));
        assert!(!is_shell_process("qs -c"));
        assert!(!is_shell_process(""));
        assert!(!is_shell_process("something-else -c caelestia"));
    }

    #[test]
    fn a_running_shell_is_strong_detection() {
        let detection = detect_from(&env_with(&[]), &reference_processes());
        assert_eq!(detection.confidence, owe_core::shell::Confidence::Strong);
        assert!(
            detection.reason.contains("qs -c caelestia"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn an_installed_but_idle_cli_is_only_weak() {
        let detection = detect_from(&env_with(&[("PATH", "/nonexistent-owe-test-dir")]), &[]);
        assert!(!detection.is_detected(), "{}", detection.reason);
        assert_eq!(detection.confidence, owe_core::shell::Confidence::None);
        assert!(
            detection.reason.contains("no `caelestia` on PATH"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn a_present_binary_without_a_shell_is_weak_not_strong() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join(BINARY);
        std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = dir.path().display().to_string();
        let detection = detect_from(&env_with(&[("PATH", &path)]), &[]);
        assert_eq!(detection.confidence, owe_core::shell::Confidence::Weak);
        assert!(
            detection.reason.contains("no `qs -c caelestia`"),
            "{}",
            detection.reason
        );
    }

    #[test]
    fn a_directory_named_caelestia_is_not_a_binary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(BINARY)).unwrap();
        let path = dir.path().display().to_string();
        assert!(find_binary(&env_with(&[("PATH", &path)])).is_none());
    }

    #[test]
    fn the_backend_uses_its_injected_process_table() {
        let processes = reference_processes();
        let backend =
            CaelestiaBackend::with_processes(std::sync::Arc::new(move || processes.clone()));
        assert!(backend.detect(&env_with(&[])));
        assert_eq!(backend.id(), ID);
    }

    // --- the wallpapers directory (FR-SHELL-3) ------------------------------

    #[test]
    fn the_shells_own_variable_wins_over_the_pictures_default() {
        let dir = wallpapers_dir(
            &env_with(&[
                ("CAELESTIA_WALLPAPERS_DIR", "/data/walls"),
                ("XDG_PICTURES_DIR", "/home/u/Pictures"),
            ]),
            &CaelestiaConfig::default(),
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/data/walls"));
    }

    #[test]
    fn an_unexpanded_config_default_falls_through_to_the_shells_default() {
        // This is the default config value, and it is *unset* in this environment:
        // expand_with leaves it literal, so the backend must compute what the shell
        // would (`$XDG_PICTURES_DIR/Wallpapers`).
        let dir = wallpapers_dir(
            &env_with(&[("XDG_PICTURES_DIR", "/home/u/Pictures")]),
            &CaelestiaConfig::default(),
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/Pictures/Wallpapers"));
    }

    #[test]
    fn a_configured_directory_is_honoured() {
        let config = CaelestiaConfig {
            wallpapers_dir: Some("~/walls".to_string()),
            ..CaelestiaConfig::default()
        };
        let dir = wallpapers_dir(&env_with(&[("HOME", "/home/u")]), &config).unwrap();
        assert_eq!(dir, PathBuf::from("/home/u/walls"));
    }

    #[test]
    fn a_relative_wallpaper_resolves_inside_the_wallpapers_directory() {
        let config = CaelestiaConfig {
            wallpapers_dir: Some("/data/walls".to_string()),
            ..CaelestiaConfig::default()
        };
        let resolved = resolve_wallpaper(&env_with(&[]), &config, "sunset.jpg").unwrap();
        assert_eq!(resolved, PathBuf::from("/data/walls/sunset.jpg"));

        // Absolute stays absolute: the shell takes any image, not only its own.
        let absolute = resolve_wallpaper(&env_with(&[]), &config, "/tmp/other.png").unwrap();
        assert_eq!(absolute, PathBuf::from("/tmp/other.png"));
    }

    // --- the pinned CLI surface (OQ-2) -------------------------------------

    #[test]
    fn the_flags_this_backend_sends_are_flags_the_recorded_cli_has() {
        let argv = wallpaper_argv(
            Path::new("/usr/local/bin/caelestia"),
            Path::new("/data/walls/sunset.jpg"),
            false,
        );
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/caelestia",
                "wallpaper",
                "-f",
                "/data/walls/sunset.jpg",
                "-N",
            ]
        );
        // Not a literal-string test: the flags are checked against the capture.
        for flag in [FLAG_FILE, FLAG_NO_SMART] {
            assert!(
                WALLPAPER_HELP.contains(&format!("{flag},")) || WALLPAPER_HELP.contains(flag),
                "`{flag}` is not in the recorded help:\n{WALLPAPER_HELP}"
            );
        }
    }

    #[test]
    fn the_plan_documented_a_monitor_flag_that_does_not_exist() {
        // The deviation behind ADR-017, asserted so a future release that *does*
        // add per-monitor support turns this test red and forces a re-pin rather
        // than leaving the refusal above in place silently.
        assert!(
            !WALLPAPER_HELP.contains("-m") && !WALLPAPER_HELP.contains("--monitor"),
            "the recorded CLI now has a monitor flag; re-pin OQ-2 and revisit the refusal"
        );
        assert!(
            SHELL_IPC.contains("function set(path: string): void"),
            "the recorded IPC no longer matches `wallpaper.set(path)`:\n{SHELL_IPC}"
        );
        assert!(
            !SHELL_IPC.contains("set(path: string, monitor"),
            "the recorded IPC gained a monitor argument; revisit ADR-017"
        );
    }

    #[test]
    fn the_theme_hook_switch_is_the_recorded_no_smart_flag() {
        let with_hook = wallpaper_argv(Path::new("caelestia"), Path::new("/w.jpg"), true);
        assert!(!with_hook.contains(&FLAG_NO_SMART.to_string()));
        let without_hook = wallpaper_argv(Path::new("caelestia"), Path::new("/w.jpg"), false);
        assert!(without_hook.contains(&FLAG_NO_SMART.to_string()));
        assert!(WALLPAPER_HELP.contains("--no-smart"));
    }

    #[test]
    fn reading_the_current_wallpaper_is_a_bare_subcommand() {
        assert_eq!(
            current_wallpaper_argv(Path::new("caelestia")),
            vec!["caelestia", "wallpaper"]
        );
        assert_eq!(
            parse_current_wallpaper("/home/u/Pictures/Wallpapers/x.jpg\n"),
            Some("/home/u/Pictures/Wallpapers/x.jpg".to_string())
        );
        assert_eq!(parse_current_wallpaper("No wallpaper set\n"), None);
        assert_eq!(parse_current_wallpaper("   \n"), None);
    }

    #[test]
    fn the_recorded_version_names_a_shell_release() {
        assert_eq!(
            parse_shell_version(VERSION).as_deref(),
            Some("2.5.0"),
            "the recorded `caelestia -v` must yield a version:\n{VERSION}"
        );
        assert_eq!(parse_shell_version("nothing useful here"), None);
    }

    // --- mode and routing ---------------------------------------------------

    #[test]
    fn the_configured_mode_decides_who_owns_the_pixels() {
        let backend = CaelestiaBackend::new();
        assert_eq!(
            backend.draw_mode(&config("shell-routed", true)),
            DrawMode::ShellRouted
        );
        assert_eq!(
            backend.draw_mode(&config("daemon-drawn", true)),
            DrawMode::DaemonDrawn
        );
        // An unparsable value is a config error the validator catches first; the
        // backend must not invent a mode from it.
        assert_eq!(
            backend.draw_mode(&config("nonsense", true)),
            DrawMode::DaemonDrawn
        );
    }

    #[test]
    fn a_named_output_is_refused_with_the_way_out() {
        let backend = CaelestiaBackend::new();
        let error = backend
            .apply_wallpaper(&config("shell-routed", true), Some("eDP-1"), "/w.jpg")
            .expect_err("Caelestia has no per-monitor target");
        let text = error.to_string();
        assert!(text.contains("eDP-1"), "{text}");
        assert!(text.contains("daemon-drawn"), "{text}");
        assert!(text.contains("wallpaper.set(path)"), "{text}");
    }

    #[test]
    fn a_whole_session_change_needs_the_binary_to_exist() {
        // Nothing on PATH in this environment unless the developer has Caelestia
        // installed, and either way the failure must name the reason.
        let backend = CaelestiaBackend::new();
        match backend.apply_wallpaper(&config("shell-routed", true), None, "/w.jpg") {
            Ok(outcome) => {
                assert!(matches!(outcome, ApplyOutcome::Routed { .. }));
            }
            Err(error) => {
                let text = error.to_string();
                assert!(
                    text.contains("caelestia"),
                    "the error must name the backend: {text}"
                );
            }
        }
    }

    #[test]
    fn clearing_is_refused_with_the_reason() {
        let backend = CaelestiaBackend::new();
        let error = backend
            .clear_wallpaper(&config("shell-routed", true), None)
            .expect_err("the shell cannot express no wallpaper");
        assert!(error.to_string().contains("always keeps one"), "{error}");
        assert!(error.to_string().contains("daemon-drawn"), "{error}");
    }

    #[test]
    fn describing_a_command_quotes_paths_with_spaces() {
        assert_eq!(
            describe(&[
                "caelestia".to_string(),
                "wallpaper".to_string(),
                "-f".to_string(),
                "/home/u/My Walls/x.jpg".to_string(),
            ]),
            "caelestia wallpaper -f '/home/u/My Walls/x.jpg'"
        );
    }
}
