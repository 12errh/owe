//! Stubbed-CLI integration tests (IMPLEMENTATION-PLAN P3: "stubbed-CLI integration
//! tests").
//!
//! These run a real `sh` stub as `caelestia`, on a `PATH` that contains nothing
//! else, and assert what the stub saw. That matters because the pure argv tests in
//! `lib.rs` prove the *shape* of the command while these prove the whole path —
//! resolution, spawning, exit-status handling, and the applied outcome — against a
//! process that is not the developer's own Caelestia install.
//!
//! The stub records its arguments to a file beside itself, so no environment
//! variable is needed to observe it and the test cannot accidentally pass by
//! reading something the real CLI left behind.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use owe_core::config::{CaelestiaConfig, ShellConfig};
use owe_core::shell::{ApplyOutcome, ShellBackend};
use owe_shell_caelestia::{CaelestiaBackend, ID};

type EnvSource = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

static STUB_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// A temporary directory holding a stub `caelestia`, a wallpapers directory with
/// one image in it, and the file the stub records its arguments to.
struct Stub {
    dir: tempfile::TempDir,
    wallpapers: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl Stub {
    /// `body` is the part of the script that runs after the arguments are
    /// recorded, so every variant records what it was called with.
    fn new(body: &str) -> Self {
        let guard = STUB_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let wallpapers = dir.path().join("walls");
        std::fs::create_dir_all(&wallpapers).expect("wallpapers dir");
        std::fs::write(
            wallpapers.join("x.jpg"),
            b"not really a jpeg, the stub does not decode",
        )
        .expect("wallpaper file");

        let script =
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/argv.txt\"\n{body}\n");
        let binary = dir.path().join("caelestia");
        std::fs::write(&binary, script).expect("stub");
        make_executable(&binary);

        Self {
            dir,
            wallpapers,
            _guard: guard,
        }
    }

    fn argv_file(&self) -> PathBuf {
        self.dir.path().join("argv.txt")
    }

    fn recorded_argv(&self) -> Vec<String> {
        std::fs::read_to_string(self.argv_file())
            .expect("the stub was never run")
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn env(&self) -> EnvSource {
        let pairs: Vec<(String, String)> = vec![
            ("PATH".to_string(), self.dir.path().display().to_string()),
            (
                "CAELESTIA_WALLPAPERS_DIR".to_string(),
                self.wallpapers.display().to_string(),
            ),
        ];
        Arc::new(move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
    }

    fn backend(&self) -> CaelestiaBackend {
        // No shell process: routing must still work for an installed CLI the user
        // asked for explicitly, and detection status must not gate an apply.
        CaelestiaBackend::with_env(Arc::new(Vec::new), self.env())
    }
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn config(theme_hook: bool) -> ShellConfig {
    ShellConfig {
        caelestia: CaelestiaConfig {
            mode: "shell-routed".to_string(),
            theme_hook,
            ..CaelestiaConfig::default()
        },
        ..ShellConfig::default()
    }
}

#[test]
fn a_routed_change_runs_the_recorded_command_shape() {
    let stub = Stub::new("exit 0");
    let backend = stub.backend();

    let outcome = backend
        .apply_wallpaper(&config(true), None, "x.jpg")
        .expect("the stub accepts everything");

    // The stub, not a pure function, is what saw these arguments.
    assert_eq!(
        stub.recorded_argv(),
        vec![
            "wallpaper".to_string(),
            "-f".to_string(),
            stub.wallpapers.join("x.jpg").display().to_string(),
        ],
        "a relative wallpaper must arrive at the CLI as an absolute path"
    );

    match outcome {
        ApplyOutcome::Routed {
            detail,
            theme_refreshed,
        } => {
            assert!(theme_refreshed, "theme_hook = true means the shell themes");
            assert!(detail.contains("caelestia wallpaper -f"), "{detail}");
            assert!(
                detail.ends_with("/walls/x.jpg"),
                "the reply names the image it set: {detail}"
            );
        }
        other => panic!("expected a routed outcome, got {other:?}"),
    }
}

#[test]
fn theme_hook_off_adds_the_no_smart_flag() {
    let stub = Stub::new("exit 0");
    let backend = stub.backend();

    let outcome = backend
        .apply_wallpaper(&config(false), None, "x.jpg")
        .expect("the stub accepts everything");

    assert_eq!(
        stub.recorded_argv(),
        vec![
            "wallpaper".to_string(),
            "-f".to_string(),
            stub.wallpapers.join("x.jpg").display().to_string(),
            "-N".to_string(),
        ],
        "theme_hook = false must reach the CLI, not be recorded and ignored"
    );
    match outcome {
        ApplyOutcome::Routed {
            theme_refreshed, ..
        } => assert!(
            !theme_refreshed,
            "with --no-smart the shell does not refresh the theme"
        ),
        other => panic!("expected a routed outcome, got {other:?}"),
    }
}

#[test]
fn an_absolute_path_is_passed_through_untouched() {
    let stub = Stub::new("exit 0");
    let backend = stub.backend();
    let outside = stub.dir.path().join("elsewhere.jpg");
    std::fs::write(&outside, b"x").unwrap();

    backend
        .apply_wallpaper(&config(true), None, &outside.display().to_string())
        .expect("the stub accepts everything");

    assert_eq!(
        stub.recorded_argv().last().cloned().unwrap(),
        outside.display().to_string(),
        "the shell takes any image, not only ones in its own directory"
    );
}

#[test]
fn a_failing_shell_reports_the_command_and_its_stderr() {
    let stub = Stub::new("echo '\"x.jpg\" is not a valid image' >&2\nexit 1");
    let backend = stub.backend();

    let error = backend
        .apply_wallpaper(&config(true), None, "x.jpg")
        .expect_err("a non-zero exit is a failure");

    let text = error.to_string();
    assert!(text.contains(ID), "the backend is named: {text}");
    assert!(
        text.contains("wallpaper -f"),
        "the command is named: {text}"
    );
    assert!(
        text.contains("not a valid image"),
        "the shell's own words: {text}"
    );
    assert!(
        text.contains("exited with"),
        "a non-zero exit must be distinguishable from a missing binary: {text}"
    );
}

#[test]
fn a_missing_wallpaper_is_refused_before_the_cli_runs() {
    let stub = Stub::new("exit 0");
    let backend = stub.backend();

    let error = backend
        .apply_wallpaper(&config(true), None, "vanished.jpg")
        .expect_err("the file does not exist");

    let text = error.to_string();
    assert!(text.contains("vanished.jpg"), "{text}");
    assert!(
        text.contains(&stub.wallpapers.display().to_string()),
        "the error explains where a relative path resolved: {text}"
    );
    assert!(
        !stub.argv_file().exists(),
        "the CLI must not be spawned for a file that is not there"
    );
}

#[test]
fn current_wallpaper_reads_what_the_shell_prints() {
    let stub = Stub::new("echo /data/walls/current.jpg");
    let backend = stub.backend();
    assert_eq!(
        backend.current_wallpaper(backend.env_lookup()).unwrap(),
        Some("/data/walls/current.jpg".to_string())
    );
    assert_eq!(
        stub.recorded_argv(),
        vec!["wallpaper".to_string()],
        "reading the state is the bare subcommand"
    );

    drop(stub);
    let empty = Stub::new("echo 'No wallpaper set'");
    let backend = empty.backend();
    assert_eq!(
        backend.current_wallpaper(backend.env_lookup()).unwrap(),
        None
    );
}

#[test]
fn daemon_drawn_mode_does_not_spawn_the_shell_cli() {
    let stub = Stub::new("exit 0");
    let backend = stub.backend();
    let config = ShellConfig {
        caelestia: CaelestiaConfig {
            mode: "daemon-drawn".to_string(),
            theme_hook: true,
            ..CaelestiaConfig::default()
        },
        ..ShellConfig::default()
    };

    assert_eq!(
        backend.apply_wallpaper(&config, None, "x.jpg"),
        Ok(ApplyOutcome::NotApplicable)
    );
    assert_eq!(
        backend.clear_wallpaper(&config, None),
        Ok(ApplyOutcome::NotApplicable)
    );
    assert!(!stub.argv_file().exists());
}

#[test]
fn a_missing_cli_is_unavailable_not_a_backend_failure() {
    // A user who names `caelestia` while it is not installed gets "not available",
    // which is what the registry maps to a precise config error.
    let empty_dir = tempfile::tempdir().unwrap();
    let path = Arc::new({
        let path = empty_dir.path().display().to_string();
        move |name: &str| (name == "PATH").then(|| path.clone())
    });
    let backend = CaelestiaBackend::with_env(Arc::new(Vec::new), path);

    let error = backend
        .apply_wallpaper(&config(true), None, "/tmp/x.jpg")
        .expect_err("no binary");
    assert!(
        matches!(error, owe_core::shell::ShellError::Unavailable { .. }),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("no `caelestia` on PATH"),
        "{error}"
    );
}
