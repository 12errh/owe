//! Binary-level smoke tests for `owed` — these are the P0 gate's "test client
//! can connect, hello, disconnect" requirement, executed against the real
//! binary rather than an in-process server.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use owe_ipc::protocol::method;
use owe_ipc::{Client, ErrorCode};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_owed")
}

fn example_config() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/config.toml")
}

/// A spawned daemon with its own XDG sandbox.
struct Sandbox {
    dir: tempfile::TempDir,
    child: Option<Child>,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("temp dir"),
            child: None,
        }
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("owe/socket")
    }

    fn write_config(&self, contents: &str) -> PathBuf {
        let path = self.dir.path().join("config.toml");
        std::fs::write(&path, contents).expect("write config");
        path
    }

    fn spawn_daemon(&mut self, config: &Path) -> u32 {
        let child = Command::new(bin())
            .arg("--config")
            .arg(config)
            .env("XDG_RUNTIME_DIR", self.dir())
            .env("XDG_CONFIG_HOME", self.dir())
            .env("HOME", self.dir())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn owed");
        self.child = Some(child);
        // Wait for the socket to answer.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if UnixStream::connect(self.socket()).is_ok() {
                return 0;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("daemon never listened on {}", self.socket().display());
    }

    /// Run the binary to completion and return (exit code, stdout, stderr).
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let output = Command::new(bin())
            .args(args)
            .env("XDG_RUNTIME_DIR", self.dir())
            .env("XDG_CONFIG_HOME", self.dir())
            .env("HOME", self.dir())
            .output()
            .expect("run owed");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    fn wait_for_exit(&mut self) -> (i32, String) {
        let child = self.child.take().expect("daemon was spawned");
        let output = child.wait_with_output().expect("wait for owed");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn check_config_accepts_the_shipped_example() {
    let sandbox = Sandbox::new();
    let (code, stdout, stderr) = sandbox.run(&[
        "--check-config",
        "--config",
        example_config().to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("config OK"), "stdout: {stdout}");
}

#[test]
fn check_config_rejects_a_broken_config_with_exit_1() {
    let sandbox = Sandbox::new();
    let config = sandbox.write_config("[shell]\nbackend = \"gnome\"\n");

    let (code, _stdout, stderr) =
        sandbox.run(&["--check-config", "--config", config.to_str().unwrap()]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("shell.backend"), "stderr: {stderr}");
}

#[test]
fn check_config_reports_a_missing_file_with_exit_1() {
    let sandbox = Sandbox::new();
    let missing = sandbox.dir().join("nope.toml");

    let (code, _stdout, stderr) =
        sandbox.run(&["--check-config", "--config", missing.to_str().unwrap()]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(
        stderr.contains("cannot read config file"),
        "stderr: {stderr}"
    );
}

#[test]
fn dump_config_prints_loadable_toml() {
    let sandbox = Sandbox::new();
    let (code, stdout, stderr) = sandbox.run(&["--dump-config"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("schema = 1"), "stdout: {stdout}");

    // The dump must load again: that is the point of dumping it.
    let path = sandbox.write_config(&stdout);
    let (code, _stdout, stderr) =
        sandbox.run(&["--check-config", "--config", path.to_str().unwrap()]);
    assert_eq!(code, 0, "dumped config did not reload: {stderr}");
}

#[test]
fn print_socket_reports_the_runtime_path() {
    let sandbox = Sandbox::new();
    let (code, stdout, stderr) = sandbox.run(&["--print-socket"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout.trim(), sandbox.socket().to_str().unwrap());
}

#[test]
fn daemon_answers_hello_then_kills_cleanly() {
    let sandbox = Sandbox::new();
    let mut sandbox = sandbox;
    let config = sandbox.write_config("");
    sandbox.spawn_daemon(&config);

    let mut client = Client::connect(&sandbox.socket()).expect("connect");
    let hello = client.hello("binary-smoke", "0.0.0").expect("hello");
    assert_eq!(hello.server_version, env!("CARGO_PKG_VERSION"));
    assert!(!hello.capabilities.methods.is_empty());
    assert!(
        hello
            .capabilities
            .methods
            .iter()
            .any(|name| name == method::STATS_GET)
    );

    let stats = client.call(method::STATS_GET, serde_json::json!({}));
    match stats {
        Ok(reply) => {
            assert!(
                reply["stats"].is_array(),
                "stats.get must return an array: {reply}"
            );
            assert_eq!(reply["paused"], serde_json::json!(false));
            assert!(
                reply["rss_bytes"].is_null() || reply["rss_bytes"].is_u64(),
                "rss_bytes must be a number or null: {reply}"
            );
            for row in reply["stats"].as_array().unwrap() {
                assert!(row["output"].is_string(), "stats row has no output: {row}");
                assert!(
                    row["decode"].is_string(),
                    "stats row has no decode path: {row}"
                );
            }
        }
        Err(owe_ipc::ClientError::Server(body))
            if body.code == ErrorCode::ConfigInvalid && body.msg.contains("no shell backend") => {}
        Err(other) => panic!("unexpected stats.get reply: {other:?}"),
    }

    // Implemented methods validate their parameters instead of pretending to
    // work: `wallpaper.set` without a spec is a bad request, not a silent success.
    let error = client
        .call(method::WALLPAPER_SET, serde_json::json!({}))
        .expect_err("wallpaper.set needs a spec");
    match error {
        owe_ipc::ClientError::Server(body) => assert_eq!(body.code, ErrorCode::BadRequest),
        other => panic!("unexpected: {other:?}"),
    }

    // ...and the new P1 read paths answer. The reply *shape* must hold in both
    // environments: on a machine with a session the daemon reports real outputs
    // with a backend; in CI (no Wayland at all) it reports an empty array and a
    // backend error, and both are valid answers. The first version of this
    // assertion called `.expect()` on the call, which passed on the maintainer's
    // desktop and failed in CI — an environment-dependent test is a test that
    // only sometimes tests.
    let outputs = client.call(method::OUTPUTS_LIST, serde_json::json!({}));
    match outputs {
        Ok(reply) => {
            assert!(
                reply["outputs"].is_array(),
                "outputs.list must return an outputs array: {reply}"
            );
            assert_eq!(reply["paused"], serde_json::json!(false));
        }
        Err(owe_ipc::ClientError::Server(body)) => {
            // Allowed only when there is genuinely no session to enumerate.
            assert_eq!(body.code, ErrorCode::ConfigInvalid, "{body:?}");
            assert!(
                body.msg.contains("no shell backend"),
                "a headless failure must explain itself: {}",
                body.msg
            );
        }
        other => panic!("unexpected reply shape: {other:?}"),
    }

    client
        .call(method::DAEMON_KILL, serde_json::json!({}))
        .expect("daemon.kill");

    let (code, stderr) = sandbox.wait_for_exit();
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stderr.contains("listening"), "stderr: {stderr}");
    assert!(stderr.contains("stopped"), "stderr: {stderr}");
    // The startup report must state plainly whether rendering is possible; a
    // silent daemon is how users end up staring at a blank screen with no idea
    // why.
    assert!(
        stderr.contains("shell backend ready")
            || stderr.contains("no shell backend available")
            || stderr.contains("using built-in defaults"),
        "startup must report backend state: {stderr}"
    );
}

#[test]
fn a_second_instance_refuses_to_start_and_exits_3() {
    let mut sandbox = Sandbox::new();
    let config = sandbox.write_config("");
    sandbox.spawn_daemon(&config);

    let (code, _stdout, stderr) = sandbox.run(&["--config", config.to_str().unwrap()]);
    assert_eq!(code, 3, "stderr: {stderr}");
    assert!(stderr.contains("already listening"), "stderr: {stderr}");
}

#[test]
fn daemon_startup_fails_fast_when_the_config_is_invalid() {
    let sandbox = Sandbox::new();
    let config = sandbox.write_config("[render.buffering]\nmax_in_flight = 99\n");

    let (code, _stdout, stderr) = sandbox.run(&["--config", config.to_str().unwrap()]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("max_in_flight"), "stderr: {stderr}");
}

#[test]
fn socket_is_owned_by_the_user_only() {
    use std::os::unix::fs::PermissionsExt;

    let mut sandbox = Sandbox::new();
    let config = sandbox.write_config("");
    sandbox.spawn_daemon(&config);

    let mode = std::fs::metadata(sandbox.socket())
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "socket mode was {mode:o}");

    // Leave a clean state for the Drop impl.
    let mut client = Client::connect(&sandbox.socket()).expect("connect");
    client.hello("binary-smoke", "0.0.0").expect("hello");
    let mut writer = client;
    let _ = writer.call(method::DAEMON_KILL, serde_json::json!({}));
    let _ = sandbox.wait_for_exit();
    let _ = std::io::stdout().flush();
}
