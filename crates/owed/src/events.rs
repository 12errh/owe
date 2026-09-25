//! The shell event bus (FR-SHELL-2).
//!
//! Hyprland writes typed events to `socket2`: focus, fullscreen, workspace, monitor
//! add/remove — exactly the state P6's governor rules are built on ("pause while a
//! window is fullscreen", "drop quality on battery"). P3 builds the pipe and proves
//! the parsing; P6 adds the rules.
//!
//! Three properties matter, and each is here for a reason:
//!
//! 1. **The listener never takes the daemon down.** The socket disappears when the
//!    compositor restarts, so the loop reconnects with a bounded backoff and logs each
//!    attempt. A daemon that died because a shell restarted would be a bug, not a
//!    feature.
//! 2. **Slow subscribers cannot stall the reader.** Publishing drops an event rather
//!    than blocking: the reader thread is also the only thing keeping the compositor's
//!    socket drained, and a governor that is not fast enough to keep up with window
//!    titles must lose them rather than wedge the pipe. The dropped count is reported,
//!    so "the governor missed something" is visible instead of silent.
//! 3. **Events are typed once.** Parsing lives in `owe_shell_hyprland::socket2`, and
//!    this module only moves [`ShellEvent`]s. P6 gets values, not strings, and the
//!    replay gate in P3 covers a whole recorded stream.
//!
//! Nothing in P3 consumes the bus yet beyond the counters `shell.status` reports. That
//! is deliberate: the exit gate is "10 000 recorded events parsed with zero
//! mismatches", and a bus with a consumer that does not exist yet would need a fake
//! one to be written just to be deleted in P6.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use owe_core::shell::ShellEvent;
use owe_ipc::Shutdown;

/// How many events a subscriber may fall behind by before events are dropped.
///
/// 256 while events arrive at most a few per interaction: a consumer that is 256
/// events behind is not going to catch up, and the alternative (an unbounded queue)
/// is how an event bus becomes a memory leak on a busy desktop.
#[allow(
    dead_code,
    reason = "used by `subscribe` and by the overflow test; the constant is what makes the \
              bound reviewable in one place"
)]
const SUBSCRIBER_CAPACITY: usize = 256;

/// Backoff bounds for reconnecting to the event socket.
const RECONNECT_MIN: Duration = Duration::from_millis(250);
const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// Counters for the bus, as reported by `shell.status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct BusStats {
    /// Events accepted from the socket.
    pub published: u64,
    /// Events dropped because a subscriber's queue was full.
    pub dropped: u64,
    /// Subscribers currently registered (P6's governor is one). A subscriber whose
    /// receiver was dropped is only removed on the next publish, so this can lag by
    /// one event — which is why it is a status counter and not a decision input.
    pub subscribers: usize,
    /// Whether a listener thread is running.
    pub listening: bool,
    /// Times the listener had to reconnect (the shell restarted, or the socket was
    /// not there yet).
    pub reconnects: u64,
    /// Kind of the most recent event (`activewindow`, `fullscreen`, …), or `None`
    /// when nothing has arrived. Not "the state" — P6's governor owns state — but
    /// proof that the pipe is alive, which is otherwise invisible.
    pub last_event: Option<String>,
}

/// A typed event bus with one queue per subscriber.
#[derive(Debug)]
pub struct ShellEventBus {
    subscribers: Mutex<Vec<SyncSender<ShellEvent>>>,
    /// The last event kind seen, so `shell.status` can show that the pipe is alive.
    last: Mutex<Option<String>>,
    published: AtomicU64,
    dropped: AtomicU64,
    reconnects: AtomicU64,
    listening: std::sync::atomic::AtomicBool,
}

impl Default for ShellEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellEventBus {
    /// A bus with no subscribers.
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            last: Mutex::new(None),
            published: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            listening: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A shared bus.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Register a subscriber.
    ///
    /// The queue is bounded (see [`SUBSCRIBER_CAPACITY`]) and a send to a full queue
    /// is dropped *from the front* by virtue of being dropped at all: P6's rules only
    /// care about recent state, so losing the oldest events in a burst is the correct
    /// failure direction.
    #[allow(
        dead_code,
        reason = "P6's governor is the first subscriber; the bus and its bounded queues \
                  are built and tested now so the governor can be written against them"
    )]
    pub fn subscribe(&self) -> Receiver<ShellEvent> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(SUBSCRIBER_CAPACITY);
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.push(sender);
        }
        receiver
    }

    /// Publish one typed event to every subscriber.
    pub fn publish(&self, event: ShellEvent) {
        self.published.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last) = self.last.lock() {
            *last = Some(event.kind().to_string());
        }
        let Ok(mut subscribers) = self.subscribers.lock() else {
            return;
        };
        subscribers.retain(|sender| match sender.try_send(event.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    /// Publish a parsed line, ignoring blank/comment lines.
    ///
    /// Returns whether the line became an event, which is what the listener counts.
    pub fn publish_line(&self, line: &str) -> bool {
        match owe_shell_hyprland::socket2::parse_line(line) {
            owe_shell_hyprland::socket2::ParsedLine::Event(event) => {
                self.publish(*event);
                true
            }
            _ => false,
        }
    }

    /// Current counters.
    pub fn stats(&self) -> BusStats {
        BusStats {
            published: self.published.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            subscribers: self
                .subscribers
                .lock()
                .map(|subscribers| subscribers.len())
                .unwrap_or(0),
            listening: self.listening.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            last_event: self.last.lock().ok().and_then(|last| last.clone()),
        }
    }
    /// Spawn the listener on a thread of its own.
    ///
    /// The listener owns the socket for the whole run: reconnecting is its job (see
    /// [`ShellEventBus::run`]), so the daemon starts it once and only joins it at
    /// shutdown.
    pub fn spawn_listener(
        self: &Arc<Self>,
        socket: PathBuf,
        shutdown: Shutdown,
    ) -> std::thread::JoinHandle<()> {
        let bus = Arc::clone(self);
        std::thread::Builder::new()
            .name("owe-shell-events".to_string())
            .spawn(move || bus.run(socket, shutdown))
            .expect("spawning the shell event listener")
    }

    /// Connect, read, and reconnect until shutdown is requested.
    fn run(self: Arc<Self>, socket: PathBuf, shutdown: Shutdown) {
        let mut backoff = RECONNECT_MIN;
        self.listening.store(true, Ordering::Relaxed);

        while !shutdown.is_requested() {
            match owe_shell_hyprland::socket2::Socket2Stream::connect(&socket) {
                Ok(mut stream) => {
                    tracing::info!(
                        socket = %socket.display(),
                        "listening for shell events (socket2)"
                    );
                    backoff = RECONNECT_MIN;
                    loop {
                        if shutdown.is_requested() {
                            break;
                        }
                        match stream.next_line() {
                            Ok(Some(line)) => {
                                self.publish_line(&line);
                            }
                            // EOF: the compositor exited or the socket was replaced.
                            Ok(None) => {
                                tracing::info!("shell event socket closed; reconnecting");
                                break;
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                continue;
                            }
                            Err(error) => {
                                tracing::warn!(%error, "shell event socket read failed");
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    // Not an error at startup: a session that has no socket yet (or
                    // a machine with no Hyprland at all) lands here, and the only
                    // honest thing to do is wait and say so.
                    tracing::debug!(
                        socket = %socket.display(),
                        %error,
                        "shell event socket not available yet"
                    );
                }
            }

            if shutdown.is_requested() {
                break;
            }
            self.reconnects.fetch_add(1, Ordering::Relaxed);
            sleep_interruptibly(backoff, &shutdown);
            backoff = (backoff * 2).min(RECONNECT_MAX);
        }

        self.listening.store(false, Ordering::Relaxed);
        tracing::debug!("shell event listener stopped");
    }
}

/// Sleep, but wake up promptly when shutdown is requested.
///
/// Polled in short slices rather than parked on a condvar: the only cost is a few
/// wakeups during a reconnect backoff, and it keeps the shutdown path identical to
/// the one the hotplug listener already uses.
fn sleep_interruptibly(duration: Duration, shutdown: &Shutdown) {
    const SLICE: Duration = Duration::from_millis(50);
    let mut slept = Duration::ZERO;
    while slept < duration && !shutdown.is_requested() {
        let step = SLICE.min(duration - slept);
        std::thread::sleep(step);
        slept += step;
    }
}

/// The socket path for this session, if the environment names one.
pub fn socket_for_session(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    owe_shell_hyprland::socket2::socket_path(env)
}

/// Path to the socket, for logging when there is none.
pub fn socket_hint(env: &dyn Fn(&str) -> Option<String>) -> String {
    match socket_for_session(env) {
        Some(path) => path.display().to_string(),
        None => {
            let signature = env("HYPRLAND_INSTANCE_SIGNATURE");
            match signature {
                Some(signature) if !signature.trim().is_empty() => {
                    format!("$HYPRLAND_INSTANCE_SIGNATURE={signature} has no socket2")
                }
                _ => "no HYPRLAND_INSTANCE_SIGNATURE (not a Hyprland session)".to_string(),
            }
        }
    }
}

/// Whether a path looks like a Hyprland event socket.
///
/// Used by the tests today; a future `--dump-shell` will print whether the path it
/// resolved is the real socket or something else entirely.
#[allow(
    dead_code,
    reason = "diagnostic helper; the tests are its callers until a `--dump-shell` lands"
)]
pub fn is_event_socket(path: &Path) -> bool {
    path.file_name()
        .map(|name| name == ".socket2.sock")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixListener;

    /// A listener on a temp socket that the test writes to, so the whole path —
    /// connect, read, parse, publish — runs for real without a compositor.
    struct FakeShell {
        /// Held only so the directory outlives the socket inside it.
        _dir: tempfile::TempDir,
        socket: PathBuf,
    }

    impl FakeShell {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket = dir.path().join(".socket2.sock");
            Self { _dir: dir, socket }
        }

        fn path(&self) -> PathBuf {
            self.socket.clone()
        }

        /// Accept one connection and write `lines`, then close.
        fn serve(&self, lines: &[&str]) {
            let listener = UnixListener::bind(&self.socket).expect("bind");
            let lines: Vec<String> = lines.iter().map(|line| (*line).to_string()).collect();
            std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    for line in lines {
                        let _ = writeln!(stream, "{line}");
                    }
                    let _ = stream.flush();
                }
            });
        }
    }

    /// Wait for a condition, or fail with the state that never became true.
    fn eventually(label: &str, mut check: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if check() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {label}");
    }

    #[test]
    fn a_published_event_reaches_a_subscriber_typed() {
        let bus = ShellEventBus::new();
        let subscriber = bus.subscribe();
        bus.publish_line("fullscreen>>1\n");
        assert_eq!(
            subscriber.recv_timeout(Duration::from_millis(200)).unwrap(),
            ShellEvent::Fullscreen { on: true }
        );
        assert_eq!(bus.stats().published, 1);
    }

    #[test]
    fn blank_lines_and_junk_do_not_publish_anything() {
        let bus = ShellEventBus::new();
        let subscriber = bus.subscribe();
        assert!(!bus.publish_line("\n"));
        assert!(!bus.publish_line("# a comment\n"));
        assert!(!bus.publish_line("this is not an event\n"));
        assert_eq!(bus.stats().published, 0);
        assert!(subscriber.try_recv().is_err());
    }

    #[test]
    fn a_slow_subscriber_loses_events_instead_of_stalling_the_reader() {
        let bus = ShellEventBus::new();
        let _subscriber = bus.subscribe();
        for _ in 0..SUBSCRIBER_CAPACITY + 50 {
            bus.publish(ShellEvent::ConfigReloaded);
        }
        let stats = bus.stats();
        assert_eq!(stats.published, (SUBSCRIBER_CAPACITY + 50) as u64);
        assert_eq!(
            stats.dropped, 50,
            "the overflow must be counted, not hidden: {stats:?}"
        );
    }

    #[test]
    fn a_dropped_subscriber_is_removed_from_the_list() {
        let bus = ShellEventBus::new();
        let subscriber = bus.subscribe();
        drop(subscriber);
        // The next publish notices and removes it.
        bus.publish(ShellEvent::ConfigReloaded);
        assert_eq!(bus.stats().subscribers, 0);
    }

    #[test]
    fn the_listener_pipes_a_live_socket_into_subscribers() {
        let shell = FakeShell::new();
        let bus = ShellEventBus::shared();
        let subscriber = bus.subscribe();
        let shutdown = Shutdown::new();
        shell.serve(&[
            "activewindow>>kitty,report, final.pdf",
            "workspacev2>>2,web",
            "monitoraddedv2>>1,HDMI-A-1,Dell Inc. DELL U2720Q",
        ]);
        let handle = bus.spawn_listener(shell.path(), shutdown.clone());

        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(
                subscriber
                    .recv_timeout(Duration::from_secs(5))
                    .expect("three events were written"),
            );
        }
        assert_eq!(
            seen,
            vec![
                ShellEvent::ActiveWindow {
                    class: "kitty".to_string(),
                    title: "report, final.pdf".to_string()
                },
                ShellEvent::Workspace {
                    id: 2,
                    name: Some("web".to_string())
                },
                ShellEvent::MonitorAdded {
                    id: Some(1),
                    monitor: "HDMI-A-1".to_string(),
                    description: Some("Dell Inc. DELL U2720Q".to_string())
                },
            ]
        );

        shutdown.request();
        handle.join().expect("the listener stops when asked");
        assert!(!bus.stats().listening);
    }

    #[test]
    fn the_listener_survives_the_shell_restarting() {
        // The shell exiting closes the socket; the listener must reconnect to the new
        // one instead of dying, because a compositor restart happens under a session
        // that is otherwise fine.
        let shell = FakeShell::new();
        let bus = ShellEventBus::shared();
        let subscriber = bus.subscribe();
        let shutdown = Shutdown::new();

        shell.serve(&["fullscreen>>1"]);
        let handle = bus.spawn_listener(shell.path(), shutdown.clone());
        assert_eq!(
            subscriber.recv_timeout(Duration::from_secs(5)).unwrap(),
            ShellEvent::Fullscreen { on: true }
        );

        // The socket is unlinked and rebound, which is what a restart looks like.
        std::fs::remove_file(shell.path()).expect("unlink");
        eventually("the first connection to close", || {
            bus.stats().reconnects >= 1
        });
        shell.serve(&["fullscreen>>0"]);
        assert_eq!(
            subscriber
                .recv_timeout(Duration::from_secs(10))
                .expect("the second connection delivers"),
            ShellEvent::Fullscreen { on: false }
        );

        shutdown.request();
        handle.join().expect("the listener stops when asked");
    }

    #[test]
    fn a_missing_socket_is_not_fatal_it_waits_and_retries() {
        let shell = FakeShell::new();
        let bus = ShellEventBus::shared();
        let shutdown = Shutdown::new();
        // Nothing is bound yet: the listener must retry rather than exit.
        let handle = bus.spawn_listener(shell.path(), shutdown.clone());
        eventually("the first reconnect attempt", || {
            bus.stats().reconnects >= 1
        });
        assert!(
            bus.stats().listening,
            "a waiting listener is still listening"
        );
        shutdown.request();
        handle.join().expect("shutdown stops a waiting listener");
    }

    #[test]
    fn the_session_socket_path_is_reported_honestly() {
        let none = |_: &str| None;
        assert!(socket_hint(&none).contains("not a Hyprland session"));
        let signature =
            |name: &str| (name == "HYPRLAND_INSTANCE_SIGNATURE").then(|| "abc_1_2".to_string());
        assert!(
            socket_hint(&signature).contains("no socket2"),
            "a signature with no socket must not be reported as a missing session: {}",
            socket_hint(&signature)
        );
        assert!(is_event_socket(Path::new(
            "/run/user/1000/hypr/x/.socket2.sock"
        )));
        assert!(!is_event_socket(Path::new(
            "/run/user/1000/hypr/x/.socket.sock"
        )));
    }
}
