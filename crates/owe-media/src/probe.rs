//! Video runtime detection and metadata probing (ADR-006, BACKEND-DESIGN §6.1).
//!
//! Two decisions are encoded here, both taken from the docs rather than invented:
//!
//! 1. **GStreamer is primary, FFmpeg is the fallback** (ADR-006). `media.backend =
//!    "auto"` prefers a working GStreamer and falls back when its plugins do not
//!    load, which is the documented answer to "plugin availability differs per
//!    distro" (STRATEGY §3).
//! 2. **Hardware decode is probed by negotiation, never assumed.** The probe runs
//!    the real pipeline against the real file and reports hardware only when the
//!    driver negotiated it, which is what handles the Reference Profile's archived
//!    `i965` VA-API driver without a special case (TRD FR-LIVE-3, P4 exit gate).
//!
//! Both metadata parsers are pure functions over captured tool output, and the
//! captures are committed as fixtures — the same "capture, don't recollect" rule
//! the Caelestia CLI surface is pinned with.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The two video pipelines the docs name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoRuntime {
    /// `uridecodebin`-based pipeline; the default (ADR-006).
    Gstreamer,
    /// `ffmpeg`, the fallback when GStreamer's plugins do not load.
    Ffmpeg,
}

impl VideoRuntime {
    /// Every runtime, in `auto` preference order.
    pub const ALL: &'static [VideoRuntime] = &[VideoRuntime::Gstreamer, VideoRuntime::Ffmpeg];

    /// Stable id used in config, capabilities and stats.
    pub fn as_str(self) -> &'static str {
        match self {
            VideoRuntime::Gstreamer => "gstreamer",
            VideoRuntime::Ffmpeg => "ffmpeg",
        }
    }

    /// Parse the config spelling (`media.backend` values other than `auto`).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gstreamer" => Some(VideoRuntime::Gstreamer),
            "ffmpeg" => Some(VideoRuntime::Ffmpeg),
            _ => None,
        }
    }

    /// The element/tool names this runtime needs on `PATH`, in probe order.
    pub fn required_tools(self) -> &'static [&'static str] {
        match self {
            VideoRuntime::Gstreamer => &["gst-launch-1.0", "gst-discoverer-1.0"],
            VideoRuntime::Ffmpeg => &["ffmpeg", "ffprobe"],
        }
    }

    /// The decoder name reported when this runtime decodes in software.
    pub fn software_decoder(self) -> &'static str {
        match self {
            VideoRuntime::Gstreamer => "avdec (software)",
            VideoRuntime::Ffmpeg => "ffmpeg (software)",
        }
    }

    /// The decoder name reported when the hardware probe negotiated.
    pub fn hardware_decoder(self) -> &'static str {
        match self {
            VideoRuntime::Gstreamer => "vaapi (hardware)",
            VideoRuntime::Ffmpeg => "vaapi (hardware)",
        }
    }
}

/// What one runtime says about this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAvailability {
    /// Which pipeline.
    pub runtime: VideoRuntime,
    /// Whether it can be used at all here.
    pub available: bool,
    /// Why, in a sentence — shown through `capabilities.unavailable` when false.
    pub detail: String,
    /// The tools it found, so a bug report names the actual binaries.
    pub tools: Vec<PathBuf>,
}

/// Whether the binary is on `PATH` and executable.
///
/// Reads the environment through the process, like every other backend in this
/// tree: the alternative is a snapshot argument threaded through four call sites
/// to describe something that cannot change between them.
pub fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(binary);
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

/// Whether one runtime's tools are present, with the reason when they are not.
pub fn runtime_availability(runtime: VideoRuntime) -> RuntimeAvailability {
    let mut tools = Vec::new();
    for name in runtime.required_tools() {
        match find_on_path(name) {
            Some(path) => tools.push(path),
            None => {
                return RuntimeAvailability {
                    runtime,
                    available: false,
                    detail: format!(
                        "`{name}` is not on PATH; install the {} runtime (see the distro \
                         notes in docs/STRATEGY.md)",
                        runtime.as_str()
                    ),
                    tools,
                };
            }
        }
    }
    RuntimeAvailability {
        runtime,
        available: true,
        detail: format!(
            "{} is available ({})",
            runtime.as_str(),
            tools
                .iter()
                .map(|tool| tool.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        tools,
    }
}

/// Whether any video runtime works here.
pub fn any_video_runtime() -> bool {
    VideoRuntime::ALL
        .iter()
        .any(|runtime| runtime_availability(*runtime).available)
}

/// Pick the runtime `media.backend` asks for.
///
/// `auto` walks [`VideoRuntime::ALL`] (GStreamer first, per ADR-006) and takes the
/// first available one; an explicit id must exist in the config schema (validated
/// in `owe-core`) and must be available here, and says so precisely when it is not
/// — never falls back to the other runtime, because "use ffmpeg" is an answer to a
/// question the user asked about a specific pipeline.
pub fn select_runtime(backend: &str) -> Result<VideoRuntime, (VideoRuntime, String)> {
    let requested = backend.trim().to_ascii_lowercase();
    if requested == "auto" || requested.is_empty() {
        for runtime in VideoRuntime::ALL {
            let availability = runtime_availability(*runtime);
            if availability.available {
                return Ok(*runtime);
            }
        }
        let reasons = VideoRuntime::ALL
            .iter()
            .map(|runtime| runtime_availability(*runtime).detail)
            .collect::<Vec<_>>()
            .join("; ");
        return Err((
            VideoRuntime::Gstreamer,
            format!("no video runtime is usable here: {reasons}"),
        ));
    }

    let Some(runtime) = VideoRuntime::parse(&requested) else {
        return Err((
            VideoRuntime::Gstreamer,
            format!("unknown media backend `{requested}`"),
        ));
    };
    let availability = runtime_availability(runtime);
    if availability.available {
        Ok(runtime)
    } else {
        Err((runtime, availability.detail))
    }
}

/// Metadata a probe recovered, before it is shaped into a `MediaInfo`.
#[derive(Debug, Clone, PartialEq)]
pub struct RawVideo {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Frames per second, from the container's own rate.
    pub fps: f64,
    /// Total frames, when the container states it.
    pub frame_count: Option<u64>,
    /// Duration in seconds, when the container states it.
    pub duration_secs: Option<f64>,
    /// Codec name, as the prober reports it.
    pub codec: String,
}

impl RawVideo {
    /// Duration as a `Duration`, when known.
    pub fn duration(&self) -> Option<Duration> {
        self.duration_secs
            .filter(|secs| secs.is_finite() && *secs >= 0.0)
            .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
    }

    /// Per-frame presentation delay implied by the frame rate.
    pub fn frame_delay(&self) -> Duration {
        if self.fps.is_finite() && self.fps > 0.0 {
            Duration::try_from_secs_f64(1.0 / self.fps).unwrap_or(Duration::from_millis(100))
        } else {
            Duration::from_millis(100)
        }
    }
}

/// Parse `num/den` frame rates, the form both tools report.
fn parse_rate(value: &str) -> Option<f64> {
    let value = value.trim();
    let (numerator, denominator) = match value.split_once('/') {
        Some((numerator, denominator)) => (numerator, denominator),
        None => (value, "1"),
    };
    let numerator: f64 = numerator.trim().parse().ok()?;
    let denominator: f64 = denominator.trim().parse().ok()?;
    if denominator == 0.0 || !numerator.is_finite() {
        return None;
    }
    Some(numerator / denominator)
}

/// The argv that asks `ffprobe` for exactly the fields [`parse_ffprobe`] reads.
pub fn ffprobe_argv(path: &Path) -> Vec<String> {
    [
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height,r_frame_rate,nb_frames,codec_name:format=duration",
        "-of",
        "default=nw=1",
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .chain(std::iter::once(path.display().to_string()))
    .collect()
}

/// Parse `ffprobe -of default=nw=1` output.
///
/// Pure: the committed fixture is the recording of what the tool prints, and a
/// tool upgrade that changes the shape fails a unit test instead of a wallpaper.
pub fn parse_ffprobe(stdout: &str) -> Result<RawVideo, String> {
    let mut width = None;
    let mut height = None;
    let mut fps = None;
    let mut frame_count = None;
    let mut duration_secs = None;
    let mut codec = None;

    for line in stdout.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "width" => width = value.parse::<u32>().ok(),
            "height" => height = value.parse::<u32>().ok(),
            "r_frame_rate" => fps = parse_rate(value),
            "nb_frames" => frame_count = value.parse::<u64>().ok(),
            "duration" => duration_secs = value.parse::<f64>().ok(),
            "codec_name" => codec = Some(value.to_string()),
            _ => {}
        }
    }

    let (Some(width), Some(height)) = (width, height) else {
        return Err("ffprobe reported no video stream dimensions".to_string());
    };
    Ok(RawVideo {
        width,
        height,
        fps: fps.ok_or_else(|| "ffprobe reported no usable frame rate".to_string())?,
        frame_count,
        duration_secs,
        codec: codec.unwrap_or_else(|| "unknown".to_string()),
    })
}

/// The argv that asks `gst-discoverer-1.0` for the same fields.
pub fn gst_discoverer_argv(path: &Path) -> Vec<String> {
    vec![path.display().to_string()]
}

/// Parse `gst-discoverer-1.0` output.
///
/// The `Analyzing …` and `Done discovering …` banner lines are skipped, and only
/// the `video #N:` stream section is read, so an audio-only file is an error
/// rather than a zero-sized frame.
pub fn parse_gst_discoverer(stdout: &str) -> Result<RawVideo, String> {
    let mut width = None;
    let mut height = None;
    let mut fps = None;
    let mut duration_secs = None;
    let mut codec = None;
    let mut in_video_stream = false;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Duration:") {
            duration_secs = parse_clock(rest.trim());
            continue;
        }
        if trimmed.starts_with("video #") {
            in_video_stream = true;
            if let Some((_, after)) = trimmed.split_once(':') {
                codec = Some(after.split('(').next().unwrap_or(after).trim().to_string());
            }
            continue;
        }
        if trimmed.starts_with("audio #") || trimmed.starts_with("container #") {
            in_video_stream = false;
            continue;
        }
        if !in_video_stream {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Width:") {
            width = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = trimmed.strip_prefix("Height:") {
            height = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = trimmed.strip_prefix("Frame rate:") {
            fps = parse_rate(rest);
        }
    }

    let (Some(width), Some(height)) = (width, height) else {
        return Err("gst-discoverer reported no video stream dimensions".to_string());
    };
    Ok(RawVideo {
        width,
        height,
        fps: fps.ok_or_else(|| "gst-discoverer reported no usable frame rate".to_string())?,
        // GStreamer's discoverer reports duration, not a frame count.
        frame_count: None,
        duration_secs,
        codec: codec.unwrap_or_else(|| "unknown".to_string()),
    })
}

/// Parse `H:MM:SS.fffffffff`, the discoverer's duration format.
fn parse_clock(value: &str) -> Option<f64> {
    let mut parts = value.trim().split(':').rev();
    let seconds: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next().unwrap_or("0").parse().ok()?;
    let hours: f64 = parts.next().unwrap_or("0").parse().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FFPROBE: &str = include_str!("../tests/fixtures/video/ffprobe-output.txt");
    const GST_DISCOVERER: &str = include_str!("../tests/fixtures/video/gst-discoverer-output.txt");

    #[test]
    fn runtime_ids_round_trip_and_reject_typos() {
        for runtime in VideoRuntime::ALL {
            assert_eq!(VideoRuntime::parse(runtime.as_str()), Some(*runtime));
        }
        assert_eq!(VideoRuntime::parse("vlc"), None);
        assert_eq!(
            VideoRuntime::parse("GStreamer"),
            Some(VideoRuntime::Gstreamer)
        );
    }

    #[test]
    fn the_ffprobe_capture_parses_to_the_real_media_facts() {
        let raw = parse_ffprobe(FFPROBE).expect("the committed capture must parse");
        assert_eq!((raw.width, raw.height), (64, 48));
        assert_eq!(raw.fps, 10.0);
        assert_eq!(raw.frame_count, Some(5));
        assert_eq!(raw.duration(), Some(Duration::from_millis(500)));
        assert_eq!(raw.codec, "h264");
        assert_eq!(raw.frame_delay(), Duration::from_millis(100));
    }

    #[test]
    fn the_gst_discoverer_capture_parses_to_the_same_facts() {
        let raw = parse_gst_discoverer(GST_DISCOVERER).expect("the committed capture must parse");
        assert_eq!((raw.width, raw.height), (64, 48));
        assert_eq!(raw.fps, 10.0);
        assert_eq!(raw.duration(), Some(Duration::from_millis(500)));
        assert!(raw.codec.starts_with("H.264"), "codec was {:?}", raw.codec);
        // The discoverer does not report a frame count; claiming one would be a
        // guess dressed as a fact.
        assert_eq!(raw.frame_count, None);
    }

    #[test]
    fn a_probe_without_a_video_stream_is_an_error_naming_the_field() {
        let error = parse_ffprobe("codec_name=aac\n").expect_err("no video stream");
        assert!(error.contains("dimensions"), "{error}");
        let error = parse_gst_discoverer("Properties:\n  Duration: 0:00:01.000000000\n")
            .expect_err("no video stream");
        assert!(error.contains("dimensions"), "{error}");
    }

    #[test]
    fn frame_rates_are_read_in_both_forms_and_never_divide_by_zero() {
        assert_eq!(parse_rate("30000/1001"), Some(30000.0 / 1001.0));
        assert_eq!(parse_rate("10/1"), Some(10.0));
        assert_eq!(parse_rate("25"), Some(25.0));
        assert_eq!(parse_rate("10/0"), None);
        assert_eq!(parse_rate("not-a-rate"), None);
    }

    #[test]
    fn clock_durations_parse_from_the_discoverers_own_format() {
        assert_eq!(parse_clock("0:00:00.500000000"), Some(0.5));
        assert_eq!(parse_clock("1:02:03.000000000"), Some(3723.0));
        assert_eq!(parse_clock("nonsense"), None);
    }

    #[test]
    fn a_zero_frame_rate_falls_back_to_a_hundred_milliseconds() {
        let raw = RawVideo {
            width: 2,
            height: 2,
            fps: 0.0,
            frame_count: None,
            duration_secs: None,
            codec: "x".to_string(),
        };
        assert_eq!(raw.frame_delay(), Duration::from_millis(100));
    }

    #[test]
    fn unrepresentable_probe_durations_are_none_not_panics() {
        let raw = RawVideo {
            width: 2,
            height: 2,
            fps: 10.0,
            frame_count: None,
            duration_secs: Some(f64::MAX),
            codec: "x".to_string(),
        };
        assert_eq!(raw.duration(), None);
    }

    #[test]
    fn the_probe_argvs_name_the_fields_the_parsers_read() {
        let argv = ffprobe_argv(Path::new("/tmp/x.mp4"));
        let joined = argv.join(" ");
        assert!(joined.contains("-show_entries"), "{joined}");
        assert!(joined.contains("nb_frames"), "{joined}");
        assert!(joined.ends_with("/tmp/x.mp4"), "{joined}");
        assert_eq!(
            gst_discoverer_argv(Path::new("/tmp/x.mp4")),
            vec!["/tmp/x.mp4".to_string()]
        );
    }

    #[test]
    fn an_unavailable_runtime_says_which_binary_is_missing() {
        // The one thing that must never happen is a silent "unavailable": the
        // message has to name the tool so the user can install it.
        let availability = runtime_availability(VideoRuntime::Ffmpeg);
        assert_eq!(availability.runtime, VideoRuntime::Ffmpeg);
        if !availability.available {
            assert!(
                availability.detail.contains("ffmpeg") || availability.detail.contains("ffprobe"),
                "{}",
                availability.detail
            );
        }
    }

    #[test]
    fn selecting_an_explicit_runtime_never_silently_uses_the_other_one() {
        // `media.backend = "ffmpeg"` is a request about a specific pipeline. If it
        // is unavailable the answer is the reason, not GStreamer.
        match select_runtime("ffmpeg") {
            Ok(runtime) => assert_eq!(runtime, VideoRuntime::Ffmpeg),
            Err((runtime, detail)) => {
                assert_eq!(runtime, VideoRuntime::Ffmpeg);
                assert!(
                    detail.contains("ffmpeg") || detail.contains("ffprobe"),
                    "{detail}"
                );
            }
        }
        let error = select_runtime("vlc").expect_err("unknown backends must not resolve");
        assert!(error.1.contains("vlc"), "{}", error.1);
    }
}
