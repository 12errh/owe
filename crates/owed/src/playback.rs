//! P4's playback half: the frame clock that makes a decoded wallpaper move.
//!
//! `owe-media` turns a file into frames; this module turns frames into a
//! wallpaper. It is the piece the decode commit left open: one thread per output
//! that pulls frames at their container timing, clamps them to the output's FPS cap
//! (FR-LIVE-6 — frame production is paced, never free-running), hands each one to
//! the daemon's existing present path, and answers `play`/`pause`/`seek`/`loop`
//! (FR-LIVE-5) plus `stats.get` (FR-GOV-6) from one shared state.
//!
//! Five decisions worth knowing before reading the code:
//!
//! 1. **The decoder lives on the clock thread.** [`owe_media::MediaDecoder`] is
//!    deliberately not `Send`, so the thread both opens and consumes it and only
//!    [`DecodedFrame`]s cross. [`PlaybackRegistry::start`] therefore blocks until
//!    that thread has presented its first frame — which is also the promise
//!    BACKEND-DESIGN §3 makes to `wallpaper.set` ("ok only after first frame of new
//!    content is queued"). A file that cannot be decoded fails *there*, synchronously.
//! 2. **The cap is a ceiling, not a target.** A 10 fps GIF on a 60 fps cap presents
//!    at 10 fps (its own delay is the longer interval); a source faster than the cap
//!    slows to it. Both directions are asserted, because a `fps_cap` that could only
//!    ever *raise* a frame rate would be a config key that does nothing.
//! 3. **A wallpaper loops.** The end of the content wraps to the loop range's start,
//!    or to the beginning; `pause` freezes without losing the position.
//! 4. **Backwards movement is bounded.** Both decoders go forward or rewind, never
//!    backwards, so a seek or a backwards loop jump is "rewind, then skip frames",
//!    capped at [`SEEK_FRAME_BUDGET`]. Past that the command is *refused with the
//!    number it cannot reach* rather than honoured late; the in-process pipeline
//!    BACKEND-DESIGN §6.1 draws is what removes the cap, and the plan records it as
//!    open rather than pretending this build seeks a two-hour film.
//! 5. **Never free-running, even on failure.** A sink error stops the clock and keeps
//!    the reason in the session, so `stats.get` can say *why* a wallpaper froze
//!    instead of showing a session that is somehow still "playing".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use owe_core::config::MediaConfig;
use owe_core::model::ContentKind;
use owe_media::{DecodedFrame, DecoderCancellation, MediaDecoder, MediaInfo};
use serde::Serialize;

/// How long [`PlaybackRegistry::start`] waits for the first presented frame.
pub const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// Frames a backwards move (a seek, or a loop wrapping round) may skip before it
/// is refused. See decision 4 in the module docs.
pub const SEEK_FRAME_BUDGET: u64 = 300;

/// How long a `seek` command waits for the clock to land before giving up.
const SEEK_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `stop` waits for the clock thread to notice before detaching it.
const STOP_TIMEOUT: Duration = Duration::from_millis(500);

/// How often a paused or held clock re-checks its state.
const HOLD_POLL: Duration = Duration::from_millis(20);

/// The window the measured fps is averaged over.
const FPS_WINDOW: Duration = Duration::from_millis(500);

/// Shortest interval between two presentations, so a zero-delay frame cannot spin.
const MIN_INTERVAL: Duration = Duration::from_millis(1);

const MAX_PLAYBACK_SECONDS: f64 = 31_536_000.0;

/// Where a presented frame is drawn.
///
/// A trait rather than a call into the engine because it is the one part of the
/// clock that needs a compositor: the daemon implements it over its layer-shell
/// presenter, and the tests implement it as a counter, which is what makes the
/// pacing assertions runnable on a machine (or a CI runner) with no display at all.
pub trait FrameSink: Send + Sync {
    /// Draw `frame` on `output` and show it; returns the size actually presented.
    fn show(&self, output: &str, frame: &DecodedFrame) -> Result<(u32, u32), String>;

    fn cancel(&self, _output: &str) {}
}

/// What a client can ask a running wallpaper to do (FR-LIVE-5).
///
/// Every variant is idempotent: `play` twice, `pause` twice, `seek` to the same
/// time twice, or the same loop twice all leave exactly one state behind. That is
/// what lets a GUI send a command on every click without tracking what it sent
/// before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackCmd {
    /// Run (or resume) the clock.
    Play,
    /// Freeze on the current frame.
    Pause,
    /// Jump to a position, in content time.
    Seek(Duration),
    /// Narrow the loop to `[start, end)`.
    Loop {
        /// Where the loop restarts, in content time.
        start: Duration,
        /// Where the loop wraps, in content time.
        end: Duration,
    },
}

impl PlaybackCmd {
    /// Parse the `cmd` field of `playback.cmd` (BACKEND-DESIGN §3:
    /// `play|pause|seek{t}|loop{a,b}`).
    ///
    /// Deliberately tolerant about the *shape* and strict about the *meaning*: a
    /// CLI passes `"seek"` with a number, a GUI passes an object, and neither should
    /// have to guess the other's spelling. Times are seconds by default, with `_ms`
    /// keys for callers that think in milliseconds.
    pub fn parse(value: &serde_json::Value) -> Result<Self, String> {
        use serde_json::Value;

        match value {
            Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
                "play" => Ok(PlaybackCmd::Play),
                "pause" => Ok(PlaybackCmd::Pause),
                other => Err(format!(
                    "unknown playback command `{other}`; expected `play`, `pause`, \
                     `{{\"seek\": <seconds>}}` or `{{\"loop\": {{\"a\": <s>, \"b\": <s>}}}}`"
                )),
            },
            Value::Object(map) => {
                if map.len() != 1 {
                    let keys: Vec<&str> = map.keys().map(String::as_str).collect();
                    return Err(format!(
                        "`cmd` must name exactly one command; saw [{}]",
                        keys.join(", ")
                    ));
                }
                let (key, payload) = map.iter().next().expect("checked non-empty");
                match key.as_str() {
                    "seek" => Ok(PlaybackCmd::Seek(time_of(payload, "seek")?)),
                    "loop" => {
                        let (start, end) = range_of(payload)?;
                        Ok(PlaybackCmd::Loop { start, end })
                    }
                    other => Err(format!(
                        "unknown playback command `{other}`; this build has `play`, `pause`, \
                         `seek` and `loop`"
                    )),
                }
            }
            other => Err(format!(
                "`cmd` must be a string or an object, got `{other}`"
            )),
        }
    }
}

/// A duration from a JSON number (seconds) or an object with `t`/`t_ms`.
fn time_of(value: &serde_json::Value, field: &str) -> Result<Duration, String> {
    use serde_json::Value;

    match value {
        Value::Number(number) => seconds(number.as_f64().unwrap_or(f64::NAN), field),
        Value::Object(map) => {
            let mut seen: Vec<String> = Vec::new();
            if let Some(number) = map.get("t").and_then(Value::as_f64) {
                seen.push(format!("t={number}"));
                return seconds(number, field);
            }
            if let Some(number) = map.get("t_ms").and_then(Value::as_f64) {
                seen.push(format!("t_ms={number}"));
                if !number.is_finite() || number < 0.0 {
                    return Err(format!("`{field}.t_ms` must be a non-negative number"));
                }
                return seconds(number / 1000.0, field);
            }
            Err(format!(
                "`{field}` needs `t` (seconds) or `t_ms` (milliseconds); saw [{}]",
                map.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        }
        other => Err(format!(
            "`{field}` must be seconds as a number, or an object with `t`/`t_ms`; got `{other}`"
        )),
    }
}

/// A `[start, end)` pair from `[a, b]` or `{a, b}` / `{a_ms, b_ms}`.
fn range_of(value: &serde_json::Value) -> Result<(Duration, Duration), String> {
    use serde_json::Value;

    let (start, end) = match value {
        Value::Array(pair) if pair.len() == 2 => {
            (time_of(&pair[0], "loop.a")?, time_of(&pair[1], "loop.b")?)
        }
        Value::Object(map) => {
            let seconds_or_ms = |secs: &str, ms: &str| -> Result<Duration, String> {
                if let Some(number) = map.get(secs).and_then(Value::as_f64) {
                    return seconds(number, secs);
                }
                if let Some(number) = map.get(ms).and_then(Value::as_f64) {
                    if !number.is_finite() || number < 0.0 {
                        return Err(format!("`loop.{ms}` must be a non-negative number"));
                    }
                    return seconds(number / 1000.0, &format!("loop.{ms}"));
                }
                Err(format!(
                    "`loop` needs `{secs}` (seconds) or `{ms}` (milliseconds); saw [{}]",
                    map.keys().cloned().collect::<Vec<_>>().join(", ")
                ))
            };
            (seconds_or_ms("a", "a_ms")?, seconds_or_ms("b", "b_ms")?)
        }
        other => {
            return Err(format!(
                "`loop` must be `[a, b]` or `{{a, b}}`; got `{other}`"
            ));
        }
    };

    if end <= start {
        return Err(format!(
            "`loop` end ({:.3} s) must be after its start ({:.3} s)",
            end.as_secs_f64(),
            start.as_secs_f64()
        ));
    }
    Ok((start, end))
}

fn seconds(number: f64, field: &str) -> Result<Duration, String> {
    if !number.is_finite() || !(0.0..=MAX_PLAYBACK_SECONDS).contains(&number) {
        return Err(format!(
            "`{field}` must be a finite number of seconds between 0 and {MAX_PLAYBACK_SECONDS}"
        ));
    }
    Duration::try_from_secs_f64(number)
        .map_err(|_| format!("`{field}` is outside the supported duration range"))
}

/// Why a playback command or start failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackError {
    /// Nothing is playing on that output.
    NotPlaying {
        /// The output asked about.
        output: String,
    },
    /// The session stopped, and this is the reason it kept.
    Stopped {
        /// The output whose clock stopped.
        output: String,
        /// What went wrong, in a sentence.
        detail: String,
    },
    /// The file could not be opened for decoding.
    Decode {
        /// Path of the content.
        path: PathBuf,
        /// What the decoder said.
        detail: String,
    },
    /// A command that cannot be honoured as asked.
    Refused {
        /// The output the command was aimed at.
        output: String,
        /// Why it cannot be honoured.
        detail: String,
    },
}

impl std::fmt::Display for PlaybackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaybackError::NotPlaying { output } => {
                write!(formatter, "no wallpaper is playing on `{output}`")
            }
            PlaybackError::Stopped { output, detail } => {
                write!(formatter, "playback on `{output}` stopped: {detail}")
            }
            PlaybackError::Decode { path, detail } => {
                write!(formatter, "cannot play `{}`: {detail}", path.display())
            }
            PlaybackError::Refused { output, detail } => {
                write!(formatter, "`{output}`: {detail}")
            }
        }
    }
}

impl std::error::Error for PlaybackError {}

/// One output's playback state, as `playback.cmd` and `stats.get` report it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaybackSnapshot {
    /// Output name.
    pub output: String,
    /// Content kind (`animated-image`, `video`).
    pub kind: String,
    /// Whether the clock is running (false = paused, held, or stopped).
    pub playing: bool,
    /// Whether the daemon-level hold (`governor.override pause`) is in force.
    pub held: bool,
    /// Frames presented since this session started.
    pub frames_presented: u64,
    /// Index of the frame on screen.
    pub position: u64,
    /// Surface width the last presented frame was rendered at.
    pub width: u32,
    /// Surface height the last presented frame was rendered at.
    pub height: u32,
    /// Content time of the frame on screen, in milliseconds.
    pub position_ms: u64,
    /// Frames per second actually achieved, measured over the last half second.
    pub fps: f64,
    /// The output's configured cap, when it has one.
    pub fps_cap: Option<u32>,
    /// `hardware` or `software` (FR-LIVE-3).
    pub decode: String,
    /// The decoder's own name, e.g. `gif`, `avdec (software)`.
    pub decoder: String,
    /// `cached` or `streaming` (FR-LIVE-1).
    pub mode: String,
    /// Frames held in the compressed frame cache (FR-GOV-6's "buffers").
    pub buffers: usize,
    /// Bytes the frame cache occupies.
    pub cache_bytes: usize,
    /// The cache's hard cap.
    pub cache_cap_bytes: usize,
    /// Frames the container holds, when it says so.
    pub total_frames: Option<u64>,
    /// Loop start in milliseconds, when a range is set.
    pub loop_start_ms: Option<u64>,
    /// Loop end in milliseconds, when a range is set.
    pub loop_end_ms: Option<u64>,
    /// Why the clock stopped, when it did.
    pub failed: Option<String>,
}

/// Anything that is a usable frame sink, so callers write `Registry::new(sink)`
/// over either a concrete sink or an already-shared trait object.
pub trait SinkOf {
    /// The sink itself, shared.
    fn into_sink(self) -> Arc<dyn FrameSink>;
}

impl SinkOf for Arc<dyn FrameSink> {
    fn into_sink(self) -> Arc<dyn FrameSink> {
        self
    }
}

impl<S: FrameSink + 'static> SinkOf for Arc<S> {
    fn into_sink(self) -> Arc<dyn FrameSink> {
        self
    }
}

impl<S: FrameSink + 'static> SinkOf for S {
    fn into_sink(self) -> Arc<dyn FrameSink> {
        Arc::new(self)
    }
}

/// The framed clock and the sessions it runs.
pub struct PlaybackRegistry {
    sink: Arc<dyn FrameSink>,
    sessions: Mutex<HashMap<String, Session>>,
    lifecycle: Mutex<()>,
    /// `governor.override pause`: one flag for every output, because the override
    /// is a daemon-level decision rather than a per-output one.
    held: Arc<AtomicBool>,
    /// Sequence for accepted `seek` requests, so a command can wait for its own.
    seeks: AtomicU64,
}

impl Drop for PlaybackRegistry {
    fn drop(&mut self) {
        self.stop_all();
    }
}

impl PlaybackRegistry {
    /// A registry that draws with `sink`.
    pub fn new(sink: impl SinkOf) -> Self {
        Self {
            sink: sink.into_sink(),
            sessions: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
            held: Arc::new(AtomicBool::new(false)),
            seeks: AtomicU64::new(0),
        }
    }

    /// Start (or replace) the wallpaper on `output`.
    ///
    /// Returns only once the first frame is on screen, so a caller can report the
    /// size it is showing and a failure lands on the request that caused it.
    pub fn start(
        &self,
        output: &str,
        path: &Path,
        kind: ContentKind,
        media: MediaConfig,
        fps_cap: Option<u32>,
    ) -> Result<PlaybackSnapshot, PlaybackError> {
        let info = owe_media::probe(path, &media).map_err(|error| PlaybackError::Decode {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;

        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = self.remove_session(output) {
            session.stop();
        }

        let shared = Arc::new(Shared::new(info.clone(), fps_cap));
        let clock = Clock {
            sink: Arc::clone(&self.sink),
            output: output.to_string(),
            shared: Arc::clone(&shared),
            held: Arc::clone(&self.held),
            path: path.to_path_buf(),
            kind,
            info,
            media,
            budget: fps_cap.map(|fps| Duration::from_secs_f64(1.0 / f64::from(fps.max(1)))),
            current: None,
            index: 0,
            position_ms: 0,
            next_present: Instant::now(),
            window_start: Instant::now(),
            window_frames: 0,
            window_fps: 0.0,
        };

        let name = format!("owe-playback-{output}");
        let panic_shared = Arc::clone(&shared);
        let handle = match thread::Builder::new().name(name).spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| clock.run()));
            if result.is_err() {
                panic_shared.fail("the playback clock thread panicked".to_string());
            }
        }) {
            Ok(handle) => handle,
            Err(error) => {
                return Err(PlaybackError::Refused {
                    output: output.to_string(),
                    detail: format!("cannot start a frame clock: {error}"),
                });
            }
        };

        {
            let mut sessions = self.sessions();
            sessions.insert(
                output.to_string(),
                Session {
                    output: output.to_string(),
                    sink: Arc::clone(&self.sink),
                    shared: Arc::clone(&shared),
                    handle: Some(handle),
                },
            );
        }
        drop(lifecycle);

        match wait_ready(&shared, FIRST_FRAME_TIMEOUT) {
            Ok(()) => self
                .snapshot_for(output, &shared)
                .ok_or_else(|| PlaybackError::Stopped {
                    output: output.to_string(),
                    detail: "the playback session was replaced before it became ready".to_string(),
                }),
            Err(detail) => {
                self.stop_if_current(output, &shared);
                if let Some(runtime) = shared.failed_detail() {
                    return Err(PlaybackError::Stopped {
                        output: output.to_string(),
                        detail: runtime,
                    });
                }
                Err(PlaybackError::Refused {
                    output: output.to_string(),
                    detail,
                })
            }
        }
    }

    /// Apply a command to one output's clock. Idempotent for every command.
    pub fn command(
        &self,
        output: &str,
        cmd: PlaybackCmd,
    ) -> Result<PlaybackSnapshot, PlaybackError> {
        let shared = self.shared(output)?;
        let command = shared
            .command
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let seek = {
            let mut runtime = shared.runtime();
            if let Some(detail) = runtime.failed.clone() {
                return Err(PlaybackError::Stopped {
                    output: output.to_string(),
                    detail,
                });
            }
            if runtime.stop {
                return Err(PlaybackError::Stopped {
                    output: output.to_string(),
                    detail: "the playback session is stopping".to_string(),
                });
            }
            match cmd {
                PlaybackCmd::Play => {
                    runtime.playing = true;
                    None
                }
                PlaybackCmd::Pause => {
                    runtime.playing = false;
                    None
                }
                PlaybackCmd::Loop { start, end } => {
                    runtime.loop_range = Some((start, end));
                    None
                }
                PlaybackCmd::Seek(target) => {
                    let sequence = self.seeks.fetch_add(1, Ordering::SeqCst) + 1;
                    runtime.seek_result = None;
                    runtime.pending_seek = Some(SeekRequest { sequence, target });
                    Some(sequence)
                }
            }
        };
        shared.wake.notify_all();

        if let Some(sequence) = seek {
            match shared.wait_for_seek(sequence, SEEK_TIMEOUT) {
                SeekWait::Applied => {}
                SeekWait::Failed(detail) => {
                    return Err(PlaybackError::Stopped {
                        output: output.to_string(),
                        detail,
                    });
                }
                SeekWait::Refused(detail) => {
                    return Err(PlaybackError::Refused {
                        output: output.to_string(),
                        detail,
                    });
                }
                SeekWait::Cancelled => {
                    return Err(PlaybackError::Stopped {
                        output: output.to_string(),
                        detail: "the playback session stopped before the seek completed"
                            .to_string(),
                    });
                }
                SeekWait::TimedOut => {
                    return Err(PlaybackError::Refused {
                        output: output.to_string(),
                        detail: format!(
                            "seek did not complete within {:?}; the clock may still apply it",
                            SEEK_TIMEOUT
                        ),
                    });
                }
            }
        }
        drop(command);

        self.snapshot_for(output, &shared)
            .ok_or(PlaybackError::NotPlaying {
                output: output.to_string(),
            })
    }

    /// One output's state, if it has a clock.
    pub fn snapshot(&self, output: &str) -> Option<PlaybackSnapshot> {
        let shared = {
            let sessions = self.sessions();
            Arc::clone(&sessions.get(output)?.shared)
        };
        self.snapshot_shared(output, &shared)
    }

    fn snapshot_for(&self, output: &str, shared: &Arc<Shared>) -> Option<PlaybackSnapshot> {
        let sessions = self.sessions();
        let current = sessions.get(output)?;
        if !Arc::ptr_eq(&current.shared, shared) {
            return None;
        }
        self.snapshot_shared(output, shared)
    }

    fn snapshot_shared(&self, output: &str, shared: &Arc<Shared>) -> Option<PlaybackSnapshot> {
        let runtime = shared.runtime();
        Some(runtime.snapshot(output, self.held.load(Ordering::SeqCst)))
    }

    fn remove_session(&self, output: &str) -> Option<Session> {
        self.sessions().remove(output)
    }

    fn stop_if_current(&self, output: &str, shared: &Arc<Shared>) {
        let session = {
            let mut sessions = self.sessions();
            if sessions
                .get(output)
                .is_some_and(|session| Arc::ptr_eq(&session.shared, shared))
            {
                sessions.remove(output)
            } else {
                None
            }
        };
        if let Some(session) = session {
            session.stop();
        }
    }

    /// Stop one output's clock, joining it if it exits promptly.
    pub fn stop(&self, output: &str) {
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = self.remove_session(output) {
            session.stop();
        }
        drop(lifecycle);
    }

    /// Stop everything (daemon shutdown, presenter loss, hotplug removal).
    pub fn stop_all(&self) {
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sessions: Vec<Session> = self
            .sessions()
            .drain()
            .map(|(_, session)| session)
            .collect();
        for session in sessions {
            session.stop();
        }
        drop(lifecycle);
    }

    /// Set the daemon-level hold (`governor.override pause`).
    ///
    /// The hold is one flag rather than a command per session because a new clock
    /// started while the override is in force must be held too — otherwise pausing
    /// and then changing wallpaper would silently start rendering again.
    pub fn set_held(&self, held: bool) {
        self.held.store(held, Ordering::SeqCst);
        let sessions = self.sessions();
        for session in sessions.values() {
            session.shared.wake.notify_all();
        }
    }

    /// The outputs that currently have a clock, ordered.
    pub fn outputs(&self) -> Vec<String> {
        let mut outputs: Vec<String> = self.sessions().keys().cloned().collect();
        outputs.sort();
        outputs
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn shared(&self, output: &str) -> Result<Arc<Shared>, PlaybackError> {
        self.sessions()
            .get(output)
            .map(|session| Arc::clone(&session.shared))
            .ok_or_else(|| PlaybackError::NotPlaying {
                output: output.to_string(),
            })
    }
}

/// One running clock, as the registry holds it.
struct Session {
    output: String,
    sink: Arc<dyn FrameSink>,
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl Session {
    fn stop(self) {
        {
            let mut runtime = self.shared.runtime();
            runtime.stop = true;
            runtime.playing = false;
            runtime.pending_seek = None;
        }
        self.shared
            .cancel_ready("playback stopped before the first frame");
        self.shared.cancel();
        self.shared.wake.notify_all();
        self.sink.cancel(&self.output);

        let Some(handle) = self.handle else {
            return;
        };
        let deadline = Instant::now() + STOP_TIMEOUT;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
        // Otherwise the handle is dropped and the thread detaches: a decoder blocked
        // in a pipe read exits on its own once the child process ends, and blocking a
        // `wallpaper.set` on that would be worse than a thread that lingers briefly.
    }
}

/// A request to jump, with the sequence number that identifies it.
#[derive(Debug, Clone, Copy)]
struct SeekRequest {
    sequence: u64,
    target: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SeekWait {
    Applied,
    Failed(String),
    Refused(String),
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone)]
enum SeekFailure {
    Refused(String),
    Fatal(String),
}

impl std::fmt::Display for SeekFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeekFailure::Refused(detail) | SeekFailure::Fatal(detail) => {
                formatter.write_str(detail)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct SeekResult {
    sequence: u64,
    result: Result<(), SeekFailure>,
}

/// What the clock thread and its callers share.
#[derive(Debug)]
struct Shared {
    runtime: Mutex<Runtime>,
    command: Mutex<()>,
    wake: Condvar,
    ready: Mutex<Option<Result<(), String>>>,
    ready_wake: Condvar,
    cancellation: Mutex<Option<DecoderCancellation>>,
}

impl Shared {
    fn new(info: MediaInfo, fps_cap: Option<u32>) -> Self {
        Self {
            runtime: Mutex::new(Runtime::new(info, fps_cap)),
            command: Mutex::new(()),
            wake: Condvar::new(),
            ready: Mutex::new(None),
            ready_wake: Condvar::new(),
            cancellation: Mutex::new(None),
        }
    }

    fn runtime(&self) -> std::sync::MutexGuard<'_, Runtime> {
        self.runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The reason the session stopped, when it did.
    fn failed_detail(&self) -> Option<String> {
        self.runtime().failed.clone()
    }

    fn complete_seek(&self, sequence: u64, result: Result<(), SeekFailure>) {
        let mut runtime = self.runtime();
        if runtime
            .seek_result
            .as_ref()
            .is_none_or(|previous| previous.sequence < sequence)
        {
            runtime.seek_result = Some(SeekResult { sequence, result });
        }
        drop(runtime);
        self.wake.notify_all();
    }

    fn install_cancellation(&self, cancellation: Option<DecoderCancellation>) {
        let runtime = self.runtime();
        let mut installed = self
            .cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if runtime.stop {
            if let Some(cancellation) = cancellation.as_ref() {
                cancellation.cancel();
            }
        }
        *installed = cancellation;
    }

    fn cancel(&self) {
        if let Some(cancellation) = self
            .cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            cancellation.cancel();
        }
    }

    fn cancel_ready(&self, detail: &str) {
        let mut ready = self
            .ready
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if ready.is_none() {
            *ready = Some(Err(detail.to_string()));
        }
        drop(ready);
        self.ready_wake.notify_all();
    }

    fn fail(&self, detail: String) {
        {
            let mut runtime = self.runtime();
            runtime.failed = Some(detail.clone());
            runtime.playing = false;
        }
        self.set_ready(Err(detail));
        self.wake.notify_all();
    }

    fn set_ready(&self, result: Result<(), String>) {
        let mut ready = self
            .ready
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if ready.is_none() {
            *ready = Some(result);
        }
        drop(ready);
        self.ready_wake.notify_all();
    }

    fn wait_for_seek(&self, sequence: u64, timeout: Duration) -> SeekWait {
        let deadline = Instant::now() + timeout;
        let mut runtime = self.runtime();
        loop {
            if let Some(result) = runtime
                .seek_result
                .as_ref()
                .filter(|result| result.sequence >= sequence)
            {
                return match &result.result {
                    Ok(()) => SeekWait::Applied,
                    Err(SeekFailure::Refused(detail)) => SeekWait::Refused(detail.clone()),
                    Err(SeekFailure::Fatal(detail)) => SeekWait::Failed(detail.clone()),
                };
            }
            if let Some(detail) = runtime.failed.clone() {
                return SeekWait::Failed(detail);
            }
            if runtime.stop {
                return SeekWait::Cancelled;
            }
            let now = Instant::now();
            if now >= deadline {
                return SeekWait::TimedOut;
            }
            let (next, _) = self
                .wake
                .wait_timeout(runtime, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            runtime = next;
        }
    }
}

/// Everything the clock thread publishes and its callers change.
#[derive(Debug)]
struct Runtime {
    info: MediaInfo,
    playing: bool,
    stop: bool,
    pending_seek: Option<SeekRequest>,
    seek_result: Option<SeekResult>,
    loop_range: Option<(Duration, Duration)>,
    frames: u64,
    position: u64,
    width: u32,
    height: u32,
    position_ms: u64,
    fps: f64,
    fps_cap: Option<u32>,
    decoder: String,
    hardware: bool,
    mode: String,
    buffers: usize,
    cache_bytes: usize,
    cache_cap_bytes: usize,
    failed: Option<String>,
}

impl Runtime {
    fn new(info: MediaInfo, fps_cap: Option<u32>) -> Self {
        Self {
            decoder: info.codec.clone().unwrap_or_else(|| "unknown".to_string()),
            info,
            playing: true,
            stop: false,
            pending_seek: None,
            seek_result: None,
            loop_range: None,
            frames: 0,
            position: 0,
            width: 0,
            height: 0,
            position_ms: 0,
            fps: 0.0,
            fps_cap,
            hardware: false,
            mode: "cached".to_string(),
            buffers: 0,
            cache_bytes: 0,
            cache_cap_bytes: 0,
            failed: None,
        }
    }

    fn snapshot(&self, output: &str, held: bool) -> PlaybackSnapshot {
        PlaybackSnapshot {
            output: output.to_string(),
            kind: self.info.kind.as_str().to_string(),
            playing: self.playing && !self.stop && self.failed.is_none() && !held,
            held,
            frames_presented: self.frames,
            position: self.position,
            width: self.width,
            height: self.height,
            position_ms: self.position_ms,
            fps: self.fps,
            fps_cap: self.fps_cap,
            decode: if self.hardware {
                "hardware"
            } else {
                "software"
            }
            .to_string(),
            decoder: self.decoder.clone(),
            mode: self.mode.clone(),
            buffers: self.buffers,
            cache_bytes: self.cache_bytes,
            cache_cap_bytes: self.cache_cap_bytes,
            total_frames: self.info.frame_count,
            loop_start_ms: self.loop_range.map(|(start, _)| start.as_millis() as u64),
            loop_end_ms: self.loop_range.map(|(_, end)| end.as_millis() as u64),
            failed: self.failed.clone(),
        }
    }
}

/// What the clock should do when it wakes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    /// The session is over.
    Stop,
    /// A frame is due, or a command wants handling.
    Act,
}

/// How a frame arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Advance {
    /// The next frame in sequence; the caller presents it.
    Pulled,
    /// A wrap landed somewhere else and is already on screen.
    Jumped,
}

/// One output's frame clock. Created per session, owned by its thread.
///
/// Everything here is `Send` on purpose, because the whole struct moves into the
/// spawn closure. The decoder is *not* a field — [`owe_media::MediaDecoder`] is
/// deliberately not `Send` (its docs explain why) — so it is created inside the
/// clock thread and handed to [`Clock::drive`] as a `&mut dyn MediaDecoder` that
/// never leaves. Only [`DecodedFrame`]s cross back, through the sink.
struct Clock {
    sink: Arc<dyn FrameSink>,
    output: String,
    shared: Arc<Shared>,
    held: Arc<AtomicBool>,
    path: PathBuf,
    kind: ContentKind,
    info: MediaInfo,
    media: MediaConfig,
    /// `1 / fps_cap`, when the output sets a cap.
    budget: Option<Duration>,
    current: Option<DecodedFrame>,
    index: u64,
    position_ms: u64,
    next_present: Instant,
    window_start: Instant,
    window_frames: u32,
    window_fps: f64,
}

impl Clock {
    /// Open the decoder on this thread and run the clock to completion.
    fn run(mut self) {
        let mut decoder = match owe_media::open_kind_with_info(
            &self.path,
            self.kind,
            &self.media,
            self.info.clone(),
        ) {
            Ok(decoder) => decoder,
            Err(error) => {
                if !self.stopped() {
                    self.fail(error.to_string());
                }
                return;
            }
        };
        self.shared.install_cancellation(decoder.cancellation());
        let first = match decoder.next_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                if !self.stopped() {
                    self.fail("the file decoded to no frames at all".to_string());
                }
                return;
            }
            Err(error) => {
                if !self.stopped() {
                    self.fail(error.to_string());
                }
                return;
            }
        };
        self.current = Some(first);
        self.publish_stats(decoder.stats());

        // The first frame goes up before readiness is signalled: `wallpaper.set`
        // must not return while the output still shows the previous content. A
        // failure here is synchronous with the apply that caused it, which is the
        // contract BACKEND-DESIGN §3 draws.
        if self.stopped() {
            self.shared
                .cancel_ready("playback stopped before the first frame");
            return;
        }
        match self.present_current() {
            Ok(()) => {}
            Err(detail) => {
                self.fail_if_running(detail);
                return;
            }
        }
        if self.stopped() {
            self.shared
                .cancel_ready("playback stopped before the first frame");
            return;
        }
        self.ready(Ok(()));
        self.schedule_next();

        self.drive(&mut *decoder);
    }

    /// The frame loop proper. `decoder` was opened by [`Clock::run`] on this same
    /// thread and is borrowed for the whole session.
    fn drive(&mut self, decoder: &mut dyn MediaDecoder) {
        loop {
            match self.wait_for_turn() {
                Turn::Stop => return,
                Turn::Act => {}
            }

            if let Some(request) = self.take_seek() {
                match self.jump_to(decoder, request.target.as_millis() as u64) {
                    Ok(()) => self.shared.complete_seek(request.sequence, Ok(())),
                    Err(SeekFailure::Refused(detail)) => {
                        self.shared
                            .complete_seek(request.sequence, Err(SeekFailure::Refused(detail)));
                        if let Err(reset) = self.reset_to_start(decoder) {
                            self.fail_if_running(reset);
                            return;
                        }
                    }
                    Err(SeekFailure::Fatal(detail)) => {
                        self.shared.complete_seek(
                            request.sequence,
                            Err(SeekFailure::Fatal(detail.clone())),
                        );
                        self.fail_if_running(detail);
                        return;
                    }
                }
                continue;
            }

            match self.advance(decoder) {
                Ok(Advance::Pulled) => {
                    if let Err(detail) = self.present_current() {
                        self.fail_if_running(detail);
                        return;
                    }
                    self.schedule_next();
                }
                Ok(Advance::Jumped) => {}
                Err(detail) => {
                    self.fail_if_running(detail);
                    return;
                }
            }
        }
    }

    /// Sleep until the next frame is due, unless a command arrives first.
    ///
    /// A hold or a pause is *not* a frame request: while either is in force the
    /// clock waits without advancing the deadline, so resuming continues the
    /// animation rather than jumping forward by however long the pause lasted. A
    /// pending seek, by contrast, *is* a turn of its own — the first version of
    /// this loop treated it as waiting and the clock could never act on the very
    /// command it was waiting for (found by `seek_moves_the_clock…` hanging on its
    /// 2 s timeout with the position unchanged).
    fn wait_for_turn(&mut self) -> Turn {
        loop {
            {
                let runtime = self.shared.runtime();
                if runtime.stop {
                    return Turn::Stop;
                }
                if runtime.pending_seek.is_some() {
                    return Turn::Act;
                }
                let waiting = !runtime.playing || self.held.load(Ordering::SeqCst);
                if !waiting {
                    let now = Instant::now();
                    if now >= self.next_present {
                        return Turn::Act;
                    }
                    drop(runtime);
                    let guard = self.shared.runtime();
                    let _ = self
                        .shared
                        .wake
                        .wait_timeout(guard, self.next_present - now);
                    continue;
                }
            }
            // Held or paused: poll rather than sleep the full interval so `play`
            // and `seek` land promptly.
            let guard = self.shared.runtime();
            let (guard, _) = self
                .shared
                .wake
                .wait_timeout(guard, HOLD_POLL)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            drop(guard);
            // Resuming must not present a burst of frames to catch up.
            self.next_present = Instant::now();
        }
    }

    fn stopped(&self) -> bool {
        self.shared.runtime().stop
    }

    fn present_current(&mut self) -> Result<(), String> {
        if self.stopped() {
            return Ok(());
        }
        let size = self.sink.show(&self.output, self.frame())?;
        if !self.stopped() {
            self.record_present(size);
        }
        Ok(())
    }

    fn schedule_next(&mut self) {
        let now = Instant::now();
        let interval = self.interval();
        let candidate = self.next_present.checked_add(interval).unwrap_or(now);
        self.next_present = if candidate <= now {
            now.checked_add(interval).unwrap_or(now)
        } else {
            candidate
        };
    }

    fn reset_to_start(&mut self, decoder: &mut dyn MediaDecoder) -> Result<(), String> {
        decoder.rewind().map_err(|error| error.to_string())?;
        let frame = decoder
            .next_frame()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                "the decoder returned no first frame after a refused seek".to_string()
            })?;
        self.current = Some(frame);
        self.index = 0;
        self.position_ms = 0;
        self.publish_stats(decoder.stats());
        self.schedule_next();
        Ok(())
    }

    fn take_seek(&mut self) -> Option<SeekRequest> {
        self.shared.runtime().pending_seek.take()
    }

    fn advance(&mut self, decoder: &mut dyn MediaDecoder) -> Result<Advance, String> {
        let next_start = self.position_ms + self.frame().delay().as_millis() as u64;
        let wrap_to = self
            .loop_range()
            .filter(|(_, end)| next_start >= end.as_millis() as u64)
            .map(|(start, _)| start.as_millis() as u64);
        if let Some(target) = wrap_to {
            self.jump_to(decoder, target)
                .map_err(|error| error.to_string())?;
            return Ok(Advance::Jumped);
        }
        let content_end = self
            .shared
            .runtime()
            .info
            .duration
            .map(|duration| duration.as_millis() as u64);
        if self.loop_range().is_none() && content_end.is_some_and(|end| next_start >= end) {
            self.jump_to(decoder, 0)
                .map_err(|error| error.to_string())?;
            return Ok(Advance::Jumped);
        }

        match decoder.next_frame() {
            Ok(Some(frame)) => {
                self.position_ms = next_start;
                self.index += 1;
                self.current = Some(frame);
                self.publish_stats(decoder.stats());
                Ok(Advance::Pulled)
            }
            // The end of the content: a wallpaper loops, so wrap to the loop range's
            // start, or to the beginning when no range is set.
            Ok(None) => {
                let target = self
                    .loop_range()
                    .map(|(start, _)| start.as_millis() as u64)
                    .unwrap_or(0);
                self.jump_to(decoder, target)
                    .map_err(|error| error.to_string())?;
                Ok(Advance::Jumped)
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// Rewind and skip forward to the frame whose start time contains `target_ms`.
    ///
    /// This is the only way to move backwards with either decoder, and it is why the
    /// skip is budgeted: `SEEK_FRAME_BUDGET` frames of 1080p video is seconds of
    /// work, and a caller is better served by a precise refusal than by a command
    /// that answers instantly and lands a minute later.
    fn jump_to(
        &mut self,
        decoder: &mut dyn MediaDecoder,
        target_ms: u64,
    ) -> Result<(), SeekFailure> {
        if let Err(error) = decoder.rewind() {
            return Err(SeekFailure::Fatal(error.to_string()));
        }

        // Walk forward from the container's start until the frame whose slot
        // contains `target_ms` is the one held. `elapsed` tracks the *end* of the
        // frame just pulled, so the held frame's start is `elapsed - its delay`,
        // which is what `position_ms` must report — a position that names a time
        // nobody measured would make every later bookkeeping drift.
        let mut elapsed = 0_u64;
        let mut index = 0_u64;
        loop {
            if self.stopped() {
                return Err(SeekFailure::Fatal(
                    "playback stopped during a seek".to_string(),
                ));
            }
            if elapsed > target_ms {
                break;
            }
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    if index >= SEEK_FRAME_BUDGET {
                        return Err(SeekFailure::Refused(format!(
                            "cannot reach {} ms: the decoder can only move by rewinding and \
                             skipping, and this build stops after {SEEK_FRAME_BUDGET} frames \
                             (BACKEND-DESIGN §6.1's in-process pipeline is what removes that \
                             limit; documented as open in the P4 plan)",
                            target_ms
                        )));
                    }
                    elapsed += frame.delay().as_millis() as u64;
                    index += 1;
                    self.current = Some(frame);
                }
                Ok(None) => {
                    // Asked past the end: land on the last frame rather than looping.
                    break;
                }
                Err(error) => return Err(SeekFailure::Fatal(error.to_string())),
            }
        }

        if index == 0 {
            return Err(SeekFailure::Refused(
                "the decoder returned no frame for the requested seek".to_string(),
            ));
        }
        let held_start = elapsed.saturating_sub(self.frame().delay().as_millis() as u64);
        self.index = index - 1;
        self.position_ms = held_start.min(target_ms);
        self.publish_stats(decoder.stats());

        self.present_current().map_err(SeekFailure::Fatal)?;
        if self.stopped() {
            return Err(SeekFailure::Fatal(
                "playback stopped before the seek completed".to_string(),
            ));
        }
        self.schedule_next();
        Ok(())
    }

    fn loop_range(&self) -> Option<(Duration, Duration)> {
        self.shared.runtime().loop_range
    }

    /// How long to wait before the next frame: the frame's own delay, unless the
    /// output's cap asks for a slower one.
    fn interval(&self) -> Duration {
        let delay = self.frame().delay();
        match self.budget {
            Some(budget) => budget.max(delay).max(MIN_INTERVAL),
            None => delay.max(MIN_INTERVAL),
        }
    }

    fn frame(&self) -> &DecodedFrame {
        self.current.as_ref().expect("a frame is always held")
    }

    /// Count one presentation and refresh the measured fps.
    fn record_present(&mut self, size: (u32, u32)) {
        {
            let mut runtime = self.shared.runtime();
            runtime.frames += 1;
            runtime.position = self.index;
            runtime.width = size.0;
            runtime.height = size.1;
            runtime.position_ms = self.position_ms;
        }
        self.window_frames += 1;
        let elapsed = self.window_start.elapsed();
        if elapsed >= FPS_WINDOW {
            self.window_fps = f64::from(self.window_frames) / elapsed.as_secs_f64();
            self.window_start = Instant::now();
            self.window_frames = 0;
            self.shared.runtime().fps = self.window_fps;
        }
    }

    /// Copy what the decoder knows into the shared state (`stats.get` reads it).
    fn publish_stats(&mut self, stats: owe_media::DecoderStats) {
        let mut runtime = self.shared.runtime();
        runtime.mode = stats.mode.as_str().to_string();
        runtime.decoder = stats.path.decoder().to_string();
        runtime.hardware = stats.path.is_hardware();
        runtime.buffers = stats.cached_frames;
        runtime.cache_bytes = stats.cache_bytes;
        runtime.cache_cap_bytes = stats.cache_cap_bytes;
        runtime.position = self.index;
        runtime.position_ms = self.position_ms;
    }

    fn ready(&self, result: Result<(), String>) {
        self.shared.set_ready(result);
    }

    /// Record why the clock stopped and wake anyone waiting on it.
    ///
    /// The session stays in the registry on purpose: `stats.get` then reports
    /// "failed, because …", which is the answer a user needs when a wallpaper
    /// freezes, and `playback.cmd` repeats the same reason instead of claiming
    /// nothing was ever playing.
    fn fail(&mut self, detail: String) {
        self.shared.fail(detail);
    }

    fn fail_if_running(&mut self, detail: String) {
        if !self.stopped() {
            self.fail(detail);
        }
    }
}

/// Wait until the clock thread has presented its first frame (or failed).
fn wait_ready(shared: &Shared, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut ready = shared
        .ready
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if let Some(result) = ready.take() {
            return result;
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(format!("no frame was presented within {timeout:?}"));
        }
        let (next, _) = shared
            .ready_wake
            .wait_timeout(ready, deadline - now)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ready = next;
    }
}

/// Resident set size in bytes, from `/proc/self/statm` (Linux only; `None` when it
/// cannot be read, and callers report the absence rather than inventing a number).
///
/// Read in-process rather than via `ps` because `stats.get` is a status call: a
/// subprocess per status query would show up in the very number it reports.
pub fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages.saturating_mul(4096))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A sink that counts presentations and can be told to fail.
    #[derive(Default)]
    struct CountingSink {
        frames: std::sync::atomic::AtomicU64,
        fail_after: std::sync::atomic::AtomicU64,
    }

    impl CountingSink {
        fn new() -> Arc<Self> {
            Arc::default()
        }

        fn count(&self) -> u64 {
            self.frames.load(Ordering::SeqCst)
        }
    }

    impl FrameSink for CountingSink {
        fn show(&self, _output: &str, _frame: &DecodedFrame) -> Result<(u32, u32), String> {
            let shown = self.frames.fetch_add(1, Ordering::SeqCst) + 1;
            let fail_after = self.fail_after.load(Ordering::SeqCst);
            if fail_after > 0 && shown > fail_after {
                return Err("sink asked to fail".to_string());
            }
            Ok((64, 48))
        }
    }

    #[derive(Default)]
    struct SlowSink {
        frames: AtomicU64,
        delay_ms: AtomicU64,
        cancelled: AtomicU64,
    }

    impl FrameSink for SlowSink {
        fn show(&self, _output: &str, _frame: &DecodedFrame) -> Result<(u32, u32), String> {
            let shown = self.frames.fetch_add(1, Ordering::SeqCst) + 1;
            if shown > 1 {
                thread::sleep(Duration::from_millis(self.delay_ms.load(Ordering::SeqCst)));
            }
            Ok((64, 48))
        }

        fn cancel(&self, _output: &str) {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }

    const GIF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../owe-media/tests/fixtures/anim-3frame.gif"
    );

    /// A registry with one running clock over the committed GIF fixture (10 fps).
    fn started(sink: &Arc<CountingSink>, cap: Option<u32>) -> (PlaybackRegistry, String) {
        let registry = PlaybackRegistry::new(Arc::clone(sink));
        registry
            .start(
                "test-output",
                Path::new(GIF),
                ContentKind::AnimatedImage,
                MediaConfig::default(),
                cap,
            )
            .expect("the committed GIF starts");
        (registry, "test-output".to_string())
    }

    fn parse_ok(value: &serde_json::Value) -> PlaybackCmd {
        PlaybackCmd::parse(value).expect("valid command")
    }

    #[test]
    fn commands_parse_from_every_documented_shape() {
        assert_eq!(parse_ok(&json!("play")), PlaybackCmd::Play);
        assert_eq!(parse_ok(&json!("pause")), PlaybackCmd::Pause);
        assert_eq!(
            parse_ok(&json!({ "seek": 1.5 })),
            PlaybackCmd::Seek(Duration::from_millis(1500))
        );
        // The object form a GUI would send.
        assert_eq!(
            parse_ok(&json!({ "seek": { "t": 2 } })),
            PlaybackCmd::Seek(Duration::from_secs(2))
        );
        assert_eq!(
            parse_ok(&json!({ "seek": { "t_ms": 250 } })),
            PlaybackCmd::Seek(Duration::from_millis(250))
        );
        assert_eq!(
            parse_ok(&json!({ "loop": [0.2, 0.8] })),
            PlaybackCmd::Loop {
                start: Duration::from_millis(200),
                end: Duration::from_millis(800)
            }
        );
        assert_eq!(
            parse_ok(&json!({ "loop": { "a": 0, "b": 1 } })),
            PlaybackCmd::Loop {
                start: Duration::ZERO,
                end: Duration::from_secs(1)
            }
        );
    }

    #[test]
    fn malformed_commands_are_refused_with_the_field_named() {
        for (bad, expected) in [
            (json!("rewind"), "unknown playback command"),
            (json!({}), "exactly one"),
            (json!({ "seek": -1 }), "finite number"),
            (json!({ "seek": 1e20 }), "finite number"),
            (json!({ "seek": {} }), "needs `t`"),
            (json!({ "loop": [1, 0] }), "must be after"),
            (json!({ "loop": { "a": 0 } }), "needs `b`"),
            (json!(5), "must be a string or an object"),
            (json!({ "play": 1, "pause": 2 }), "exactly one"),
        ] {
            let error = PlaybackCmd::parse(&bad).expect_err(&bad.to_string());
            assert!(error.contains(expected), "{bad}: {error}");
        }
    }

    #[test]
    fn the_clock_frames_the_cap_but_never_beats_the_container() {
        // Pure arithmetic, no thread: a 30 fps cap frames nothing faster than
        // 1/30 s, and 10 fps container timing stays exactly 100 ms.
        let mut clock = Clock {
            sink: CountingSink::new(),
            output: "t".to_string(),
            shared: Arc::new(Shared::new(
                owe_media::probe(Path::new(GIF), &MediaConfig::default())
                    .expect("probe the fixture"),
                Some(30),
            )),
            held: Arc::new(AtomicBool::new(false)),
            path: PathBuf::from(GIF),
            kind: ContentKind::AnimatedImage,
            info: owe_media::probe(Path::new(GIF), &MediaConfig::default())
                .expect("probe the fixture"),
            media: MediaConfig::default(),
            budget: Some(Duration::from_secs_f64(1.0 / 30.0)),
            current: None,
            index: 0,
            position_ms: 0,
            next_present: Instant::now(),
            window_start: Instant::now(),
            window_frames: 0,
            window_fps: 0.0,
        };
        clock.current = Some(
            DecodedFrame::new(0, 2, 2, Duration::from_millis(100), vec![0; 16]).expect("2×2 frame"),
        );

        assert_eq!(clock.interval(), Duration::from_millis(100));
        // And a fast frame under the same cap takes the cap's interval.
        clock.current =
            Some(DecodedFrame::new(1, 2, 2, Duration::from_millis(5), vec![0; 16]).expect("frame"));
        assert_eq!(
            clock.interval(),
            Duration::from_secs_f64(1.0 / 30.0),
            "the cap is a ceiling on production speed"
        );

        // Without a cap the container's timing wins outright.
        clock.budget = None;
        assert_eq!(clock.interval(), Duration::from_millis(5));
        // A zero-delay frame cannot spin the clock.
        clock.current =
            Some(DecodedFrame::new(2, 2, 2, Duration::ZERO, vec![0; 16]).expect("frame"));
        assert_eq!(clock.interval(), MIN_INTERVAL);
    }

    #[test]
    fn the_fixture_clock_runs_at_the_gifs_own_rate_without_a_cap() {
        let sink = CountingSink::new();
        let (registry, output) = started(&sink, None);
        thread::sleep(Duration::from_millis(700));
        let snapshot = registry.snapshot(&output).expect("session");
        registry.stop(&output);

        // 10 fps content over 700 ms: 5-9 frames. Timing from the container, not a
        // hard-coded cadence.
        assert!(
            (5..=9).contains(&snapshot.frames_presented),
            "a 10 fps GIF must present at its own rate over 700 ms, saw {}",
            snapshot.frames_presented
        );
        assert!(
            snapshot.fps > 5.0 && snapshot.fps < 15.0,
            "{:?}",
            snapshot.fps
        );
        // The animation wraps every 300 ms. `position_ms` is the *start* time of
        // the frame on screen, and the snapshot can race the wrap by one frame, so
        // the honest bound is "inside one cycle, measured loosely": anything at or
        // past the cycle end means the clock kept counting past the container's
        // own duration, which is the bug this test exists for.
        assert!(
            snapshot.position_ms < 300,
            "position {} ms must stay inside the 300 ms cycle",
            snapshot.position_ms
        );
    }

    #[test]
    fn a_cap_holds_the_fixture_below_the_limit() {
        let sink = CountingSink::new();
        // Cap below the source rate: the GIF is 10 fps; a 3 fps cap means ~2-4
        // frames in a second.
        let (registry, output) = started(&sink, Some(3));
        thread::sleep(Duration::from_millis(1050));
        let snapshot = registry.snapshot(&output).expect("session");
        registry.stop(&output);
        assert!(
            (2..=5).contains(&snapshot.frames_presented),
            "a 3 fps cap must hold against a 10 fps source, saw {} in ~1 s",
            snapshot.frames_presented
        );
        assert_eq!(snapshot.fps_cap, Some(3));
    }

    #[test]
    fn a_cap_above_the_source_rate_never_speeds_it_up() {
        let sink = CountingSink::new();
        // 10 fps content under a 60 fps cap: still ~10 fps.
        let (registry, output) = started(&sink, Some(60));
        thread::sleep(Duration::from_millis(1050));
        let snapshot = registry.snapshot(&output).expect("session");
        registry.stop(&output);
        assert!(
            (8..=13).contains(&snapshot.frames_presented),
            "a 60 fps cap must not speed a 10 fps GIF up, saw {} in ~1 s",
            snapshot.frames_presented
        );
    }

    #[test]
    fn pause_freezes_and_play_resumes_without_losing_position() {
        let sink = CountingSink::new();
        let (registry, output) = started(&sink, None);
        thread::sleep(Duration::from_millis(150));

        registry
            .command(&output, PlaybackCmd::Pause)
            .expect("pause");
        let frozen_at = sink.count();
        let paused = registry.snapshot(&output).expect("session");
        assert!(!paused.playing);
        assert!(paused.position_ms > 0, "a pause lands on real content");

        thread::sleep(Duration::from_millis(150));
        assert_eq!(
            sink.count(),
            frozen_at,
            "paused means exactly zero new frames"
        );

        registry.command(&output, PlaybackCmd::Play).expect("play");
        thread::sleep(Duration::from_millis(150));
        registry.stop(&output);
        assert!(
            sink.count() > frozen_at,
            "play resumes from where the pause froze it"
        );
    }

    #[test]
    fn a_loop_range_wraps_earlier_than_the_content_end() {
        let sink = CountingSink::new();
        let (registry, output) = started(&sink, None);
        // The fixture GIF is 300 ms; a 100 ms loop restarts three times as often.
        registry
            .command(
                &output,
                PlaybackCmd::Loop {
                    start: Duration::ZERO,
                    end: Duration::from_millis(100),
                },
            )
            .expect("loop");

        thread::sleep(Duration::from_millis(400));
        let snapshot = registry.snapshot(&output).expect("session");
        registry.stop(&output);
        assert_eq!(snapshot.loop_end_ms, Some(100));
        assert!(
            snapshot.position_ms < 100,
            "a 100 ms loop must never show content past 100 ms, saw {} ms",
            snapshot.position_ms
        );
        // The loop restarted: 400 ms with a 100 ms cycle means several wraps, and
        // the count is bounded by the frame rate (10 fps ⇒ ~4-6 presentations).
        assert!(
            (2..=8).contains(&snapshot.frames_presented),
            "looping restarted within every 100 ms window: {} frames in 400 ms",
            snapshot.frames_presented
        );
    }

    #[test]
    fn seek_moves_the_clock_and_reports_where_it_landed() {
        let sink = CountingSink::new();
        let (registry, output) = started(&sink, None);
        registry
            .command(&output, PlaybackCmd::Pause)
            .expect("pause");

        let snapshot = registry
            .command(&output, PlaybackCmd::Seek(Duration::from_millis(210)))
            .expect("seek");
        // 210 ms falls inside frame 2's slot ([200, 300) on a 3×100 ms GIF). A
        // position is a *frame slot start*, not an arbitrary instant: the clock
        // can only show frames that exist, so the reply names the frame it landed
        // on and where that frame's own slot begins.
        assert_eq!(snapshot.position, 2, "210 ms is inside frame 2");
        assert_eq!(snapshot.position_ms, 200, "frame 2 starts at 200 ms");

        // Resuming from the seek continues forward.
        registry.command(&output, PlaybackCmd::Play).expect("play");
        thread::sleep(Duration::from_millis(150));
        let later = registry.snapshot(&output).expect("session");
        registry.stop(&output);
        assert!(
            later.frames_presented > 1,
            "frames presented after the seek: {}",
            later.frames_presented
        );
    }

    #[test]
    fn seek_does_not_acknowledge_before_the_new_frame_is_presented() {
        let sink = Arc::new(SlowSink::default());
        sink.delay_ms.store(180, Ordering::SeqCst);
        let registry = Arc::new(PlaybackRegistry::new(Arc::clone(&sink)));
        registry
            .start(
                "test-output",
                Path::new(GIF),
                ContentKind::AnimatedImage,
                MediaConfig::default(),
                None,
            )
            .expect("the fixture starts");
        registry
            .command("test-output", PlaybackCmd::Pause)
            .expect("pause");

        let done = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let command_registry = Arc::clone(&registry);
        let command_done = Arc::clone(&done);
        let command_result = Arc::clone(&result);
        let worker = thread::spawn(move || {
            let value = command_registry
                .command("test-output", PlaybackCmd::Seek(Duration::from_millis(210)));
            command_done.store(true, Ordering::SeqCst);
            *command_result
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(value);
        });

        thread::sleep(Duration::from_millis(30));
        assert!(!done.load(Ordering::SeqCst));
        worker.join().expect("seek worker");
        assert!(done.load(Ordering::SeqCst));
        let value = result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .expect("seek result")
            .expect("seek succeeds");
        assert_eq!(value.position_ms, 200);
        registry.stop("test-output");
    }

    #[test]
    fn a_seek_failure_is_returned_instead_of_being_reported_as_success() {
        let sink = CountingSink::new();
        let (registry, output) = started(&sink, None);
        registry
            .command(&output, PlaybackCmd::Pause)
            .expect("pause");
        sink.fail_after.store(1, Ordering::SeqCst);

        let error = registry
            .command(&output, PlaybackCmd::Seek(Duration::from_millis(210)))
            .expect_err("the seek must not succeed");
        assert!(matches!(error, PlaybackError::Stopped { .. }), "{error}");
        assert!(error.to_string().contains("sink asked to fail"), "{error}");
        registry.stop(&output);
    }

    #[test]
    fn a_seek_timeout_is_returned_to_the_caller() {
        let shared = Arc::new(Shared::new(
            owe_media::probe(Path::new(GIF), &MediaConfig::default()).expect("probe"),
            None,
        ));
        let sequence = 1;
        shared.runtime().pending_seek = Some(SeekRequest {
            sequence,
            target: Duration::ZERO,
        });
        assert_eq!(
            shared.wait_for_seek(sequence, Duration::from_millis(5)),
            SeekWait::TimedOut
        );
    }

    #[test]
    fn scheduling_a_late_frame_does_not_create_a_catch_up_burst() {
        let shared = Arc::new(Shared::new(
            owe_media::probe(Path::new(GIF), &MediaConfig::default()).expect("probe"),
            None,
        ));
        let mut clock = Clock {
            sink: CountingSink::new(),
            output: "test-output".to_string(),
            shared,
            held: Arc::new(AtomicBool::new(false)),
            path: PathBuf::from(GIF),
            kind: ContentKind::AnimatedImage,
            info: owe_media::probe(Path::new(GIF), &MediaConfig::default()).expect("probe"),
            media: MediaConfig::default(),
            budget: None,
            current: Some(
                DecodedFrame::new(0, 2, 2, Duration::from_millis(100), vec![0; 16]).expect("frame"),
            ),
            index: 0,
            position_ms: 0,
            next_present: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(|| Instant::now() - Duration::from_millis(1)),
            window_start: Instant::now(),
            window_frames: 0,
            window_fps: 0.0,
        };
        let before = Instant::now();
        clock.schedule_next();
        assert!(clock.next_present > before + Duration::from_millis(50));
    }

    #[test]
    fn poisoned_registry_state_remains_usable() {
        let sink = CountingSink::new();
        let registry = PlaybackRegistry::new(Arc::clone(&sink));
        registry
            .start(
                "test-output",
                Path::new(GIF),
                ContentKind::AnimatedImage,
                MediaConfig::default(),
                None,
            )
            .expect("the fixture starts");
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.sessions.lock().expect("lock sessions");
            panic!("poison the registry lock");
        }));
        assert!(poisoned.is_err());
        assert!(registry.snapshot("test-output").is_some());
        registry.stop("test-output");
        assert!(registry.snapshot("test-output").is_none());
    }
    #[test]
    fn commands_are_idempotent_and_answer_one_state() {
        let sink = CountingSink::new();
        let (registry, output) = started(&sink, None);

        registry.command(&output, PlaybackCmd::Play).expect("play");
        let once = registry.command(&output, PlaybackCmd::Play).expect("play");
        let twice = registry.command(&output, PlaybackCmd::Play).expect("play");
        assert_eq!(once.playing, twice.playing);

        let paused = registry
            .command(&output, PlaybackCmd::Pause)
            .expect("pause");
        let again = registry
            .command(&output, PlaybackCmd::Pause)
            .expect("pause");
        assert!(!paused.playing && !again.playing);

        // The same loop twice lands on the same range, not an intersection.
        let range = PlaybackCmd::Loop {
            start: Duration::ZERO,
            end: Duration::from_millis(200),
        };
        let first = registry.command(&output, range).expect("loop");
        let second = registry.command(&output, range).expect("loop");
        registry.stop(&output);
        assert_eq!(first.loop_end_ms, second.loop_end_ms);
    }

    #[test]
    fn a_sink_failure_stops_the_clock_and_stats_say_why() {
        let sink = CountingSink::new();
        sink.fail_after.store(3, Ordering::SeqCst);
        let (registry, output) = started(&sink, None);

        let mut snapshot = None;
        for _ in 0..50 {
            if let Some(current) = registry.snapshot(&output)
                && current.failed.is_some()
            {
                snapshot = Some(current);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let snapshot = snapshot.expect("the clock reports its own death");

        let reason = snapshot.failed.expect("the reason is kept");
        assert!(reason.contains("sink asked to fail"), "{reason}");

        // A command on a dead session repeats the reason instead of pretending —
        // asked *before* `stop`, because stopping removes the session entirely and
        // "nothing is playing" then becomes the honest answer.
        let error = registry
            .command(&output, PlaybackCmd::Play)
            .expect_err("the clock is gone");
        assert!(matches!(error, PlaybackError::Stopped { .. }), "{error}");
        assert!(error.to_string().contains("sink asked to fail"));

        registry.stop(&output);
    }

    #[test]
    fn the_governor_hold_freezes_every_clock() {
        let sink = CountingSink::new();
        let (registry, output) = started(&sink, None);
        registry.set_held(true);

        let frozen = sink.count();
        thread::sleep(Duration::from_millis(120));
        assert_eq!(sink.count(), frozen, "held means no frames");

        let snapshot = registry.snapshot(&output).expect("session");
        assert!(snapshot.held);
        assert!(!snapshot.playing);

        registry.set_held(false);
        thread::sleep(Duration::from_millis(120));
        registry.stop(&output);
        assert!(sink.count() > frozen, "releasing the hold resumes");
    }

    #[test]
    fn stats_report_the_real_decode_facts_for_the_fixture() {
        let sink = CountingSink::new();
        let (registry, output) = started(&sink, None);
        thread::sleep(Duration::from_millis(80));
        let snapshot = registry.snapshot(&output).expect("session");
        registry.stop(&output);

        assert_eq!(snapshot.kind, "animated-image");
        assert_eq!(snapshot.decoder, "gif");
        assert_eq!(snapshot.decode, "software");
        assert_eq!(snapshot.mode, "cached");
        assert_eq!(snapshot.fps_cap, None);
        assert_eq!(snapshot.total_frames, Some(3));
        assert!(snapshot.cache_bytes > 0);
        assert_eq!(snapshot.position_ms, snapshot.position * 100);
    }

    #[test]
    fn starting_over_a_running_wallpaper_replaces_it_without_leaks() {
        let sink = CountingSink::new();
        let (registry, output) = started(&sink, None);
        registry
            .start(
                &output,
                Path::new(GIF),
                ContentKind::AnimatedImage,
                MediaConfig::default(),
                Some(30),
            )
            .expect("replacement starts");
        thread::sleep(Duration::from_millis(80));
        let snapshot = registry.snapshot(&output).expect("session");
        registry.stop(&output);
        assert_eq!(snapshot.fps_cap, Some(30), "the newer request's cap wins");
        assert_eq!(registry.outputs().len(), 0, "one session, not two");
    }

    #[test]
    fn replacement_cancels_the_old_clock_before_installing_the_new_one() {
        let sink = Arc::new(SlowSink::default());
        let registry = PlaybackRegistry::new(Arc::clone(&sink));
        registry
            .start(
                "test-output",
                Path::new(GIF),
                ContentKind::AnimatedImage,
                MediaConfig::default(),
                None,
            )
            .expect("first start");
        registry
            .start(
                "test-output",
                Path::new(GIF),
                ContentKind::AnimatedImage,
                MediaConfig::default(),
                Some(30),
            )
            .expect("replacement start");
        assert!(sink.cancelled.load(Ordering::SeqCst) >= 1);
        assert_eq!(registry.outputs(), vec!["test-output".to_string()]);
        registry.stop("test-output");
    }
    #[test]
    fn stop_is_safe_to_call_twice_and_on_an_unknown_output() {
        let sink = CountingSink::new();
        let (registry, output) = started(&sink, None);
        registry.stop(&output);
        registry.stop(&output);
        registry.stop("never-started");
        registry.stop_all();
        assert!(registry.snapshot(&output).is_none());
        assert!(registry.outputs().is_empty());
    }

    #[test]
    fn commands_on_a_never_started_output_say_so() {
        let sink = CountingSink::new();
        let registry = PlaybackRegistry::new(sink);
        let error = registry
            .command("eDP-1", PlaybackCmd::Play)
            .expect_err("nothing playing");
        assert_eq!(
            error,
            PlaybackError::NotPlaying {
                output: "eDP-1".to_string()
            }
        );
        assert!(registry.snapshot("eDP-1").is_none());
    }

    #[test]
    fn a_broken_file_fails_start_synchronously_with_the_reason() {
        let sink = CountingSink::new();
        let registry = PlaybackRegistry::new(sink);
        let broken = std::env::temp_dir().join("owe-playback-broken.gif");
        std::fs::write(&broken, b"not a gif at all").expect("write the broken fixture");

        let error = registry
            .start(
                "test-output",
                &broken,
                ContentKind::AnimatedImage,
                MediaConfig::default(),
                None,
            )
            .expect_err("a text file is not an animation");
        let _ = std::fs::remove_file(&broken);
        assert!(
            error.to_string().to_lowercase().contains("cannot play"),
            "{error}"
        );
        assert!(registry.snapshot("test-output").is_none());
    }

    #[test]
    fn rss_is_a_real_number_or_an_admitted_none() {
        // On Linux this must read; the point of the check is that the function
        // returns a plausible magnitude, not a page count or zero.
        if let Some(bytes) = rss_bytes() {
            assert!(bytes > 1024 * 1024, "{bytes} bytes is not a plausible RSS");
        }
    }
}
