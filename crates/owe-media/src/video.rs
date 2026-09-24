//! Video decoding through the two pipelines ADR-006 names.
//!
//! The shape is the docs' own: **GStreamer is primary, FFmpeg is the fallback**,
//! `media.backend` selects, and `auto` prefers the first one that can actually run
//! here (BACKEND-DESIGN §6.1). What this module adds is the honest part of that
//! design:
//!
//! - **The runtime is probed, not assumed.** A backend is usable when its tools
//!   are on `PATH`; when they are not, the error names the binary to install
//!   instead of reporting a corrupt file.
//! - **Hardware decode is verified, never declared.** VA-API lives in `libva`,
//!   below both pipelines, so one probe answers for both: a render node must
//!   exist *and* `libva` must actually initialise it. Only then is hardware
//!   decode attempted or reported — which is exactly what handles the Reference
//!   Profile's archived `i965` driver without a per-driver special case (TRD
//!   FR-LIVE-3, P4 gate).
//!
//!   The probe is deliberately *not* "run the pipeline against the file and look
//!   at the exit status": measured on the reference machine, `ffmpeg -hwaccel
//!   vaapi` **exits 0** while its driver fails to initialise (it silently falls
//!   back), and `vaapidecodebin` spins on the same broken driver until killed.
//!   Both would have produced a confident, wrong "hardware" claim, so the probe
//!   reads the one signal that is about the device rather than the exit code.
//! - **`media.hw_decode_required = true` is honoured loudly.** If the probe says
//!   software and the config demands hardware, decoding is refused with the reason
//!   rather than quietly failing the requirement.
//! - **`auto` falls back for real, not only on paper.** GStreamer being *installed*
//!   is not GStreamer *working*: measured here, `gst-discoverer-1.0` can hang for
//!   longer than any sane budget while a wedged plugin registry is rebuilt, and a
//!   decoder element can refuse a file. `auto` therefore runs the whole open against
//!   GStreamer and then against FFmpeg, and an explicit `media.backend` gets exactly
//!   the one pipeline it named.
//!
//! Frames arrive as `RGBA8`, one buffer per frame, which is the layout the render
//! path already consumes — so no colour conversion happens twice and no decoder
//! knows what a texture is.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::probe::{
    VideoRuntime, ffprobe_argv, find_on_path, gst_discoverer_argv, parse_ffprobe,
    parse_gst_discoverer, runtime_availability, select_runtime,
};
use crate::{
    DecodeMode, DecodePath, DecodedFrame, DecoderStats, MediaConfig, MediaDecoder, MediaError,
    MediaInfo, RawVideo,
};
use owe_core::model::ContentKind;

/// How long a metadata probe may run.
///
/// Ten seconds is generous for reading a header, and bounded on purpose: a probe
/// that can hang is a daemon that can hang, and both probes here run against the
/// user's own file.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the VA-API device probe may run. Measured at under a fifth of a
/// second when the driver initialises, so a bound this loose only ever fires on a
/// machine whose driver is wedged.
const DEVICE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The size a decode device probe renders. One 64×64 frame is enough to force
/// device creation and costs nothing.
const DEVICE_PROBE_SIZE: &str = "color=black:s=64x64";

/// The `gst-launch-1.0` argv that turns a file into raw RGBA frames on stdout.
///
/// `hardware` swaps the decoder element: `vaapidecodebin` when VA-API negotiated,
/// `decodebin` otherwise. `videoconvert` is what guarantees the RGBA layout either
/// way, so the reader below never has to negotiate.
pub fn gstreamer_frame_argv(tool: &Path, path: &Path, hardware: bool) -> Vec<String> {
    let decoder = if hardware {
        "vaapidecodebin"
    } else {
        "decodebin"
    };
    [
        tool.display().to_string(),
        "-q".to_string(),
        "filesrc".to_string(),
        format!("location={}", path.display()),
        "!".to_string(),
        decoder.to_string(),
        "!".to_string(),
        "videoconvert".to_string(),
        "!".to_string(),
        "video/x-raw,format=RGBA".to_string(),
        "!".to_string(),
        "fdsink".to_string(),
        "fd=1".to_string(),
    ]
    .to_vec()
}

/// The `ffmpeg` argv that writes raw RGBA frames to stdout.
///
/// `-nostdin` matters: without it a mis-detected input can make `ffmpeg` consume
/// the daemon's stdin and sit there. Audio is dropped (`-an`) because a wallpaper
/// is silent by default (BACKEND-DESIGN §6.1).
pub fn ffmpeg_frame_argv(tool: &Path, path: &Path, hardware: bool) -> Vec<String> {
    let mut argv = vec![
        tool.display().to_string(),
        "-v".to_string(),
        "error".to_string(),
    ];
    if hardware {
        argv.extend(["-hwaccel".to_string(), "vaapi".to_string()]);
    }
    argv.extend([
        "-nostdin".to_string(),
        "-i".to_string(),
        path.display().to_string(),
        "-an".to_string(),
        "-f".to_string(),
        "rawvideo".to_string(),
        "-pix_fmt".to_string(),
        "rgba".to_string(),
        "-".to_string(),
    ]);
    argv
}

/// The argv that asks `libva` to initialise a VA-API device.
///
/// `-init_hw_device` is the point: without it `ffmpeg -hwaccel vaapi` **exits 0**
/// on a machine whose driver cannot initialise, because it falls back to software
/// without telling the exit status. This form creates the device explicitly, and
/// the driver's own complaint lands on stderr where [`vaapi_failure_marks`] reads
/// it.
fn device_probe_argv(ffmpeg: &Path, node: &Path) -> Vec<String> {
    vec![
        ffmpeg.display().to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-init_hw_device".to_string(),
        format!("vaapi=owe:{}", node.display()),
        "-f".to_string(),
        "lavfi".to_string(),
        "-i".to_string(),
        DEVICE_PROBE_SIZE.to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ]
}

/// Whether a driver's own stderr says the VA-API device could not be initialised.
///
/// Text matching is a blunt instrument, so it is used the conservative way round:
/// a recognised failure marker means **software**, and a missing marker only
/// supports hardware when the process also exited cleanly. The markers are pinned
/// against a capture of the reference machine's real failure
/// (`tests/fixtures/video/vaapi-failure.txt`) rather than remembered — the same
/// rule the Caelestia CLI surface is pinned with.
pub fn vaapi_failure_marks(stderr: &str) -> bool {
    const MARKERS: &[&str] = &[
        "init failed",
        "Failed to initialise",
        "Failed to initialize",
        "cannot load libva",
        "Cannot load libva",
        "no usable",
        "No usable",
    ];
    let lowered = stderr.to_ascii_lowercase();
    MARKERS
        .iter()
        .any(|marker| lowered.contains(&marker.to_ascii_lowercase()))
}

/// What this machine can say about hardware video decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaapiStatus {
    /// Whether hardware decode was verified.
    pub available: bool,
    /// Why, in a sentence — reported through `stats`/capabilities when false.
    pub detail: String,
    /// The render node the probe used, when there was one.
    pub device: Option<PathBuf>,
}

/// The first DRM render node, if the machine has one.
fn first_render_node() -> Option<PathBuf> {
    let entries = std::fs::read_dir("/dev/dri").ok()?;
    let mut nodes: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().next()
}

/// Verify whether VA-API works here, honestly and cheaply.
///
/// Conservative in both directions that matter: no render node, no `ffmpeg` to
/// run the device probe, or any driver complaint means **software**. Reporting
/// hardware is the claim this project's whole resource thesis rests on, so it is
/// made only when a device initialised under observation.
pub fn vaapi_status() -> VaapiStatus {
    let Some(node) = first_render_node() else {
        return VaapiStatus {
            available: false,
            detail: "no /dev/dri/renderD* node; this machine has no GPU decode device".to_string(),
            device: None,
        };
    };

    let Some(ffmpeg) = find_on_path("ffmpeg") else {
        return VaapiStatus {
            available: false,
            detail: format!(
                "`ffmpeg` is not installed, so VA-API on {} could not be verified; \
                 reporting software decode rather than claiming hardware",
                node.display()
            ),
            device: Some(node),
        };
    };

    let argv = device_probe_argv(&ffmpeg, &node);
    match run_bounded(&argv, DEVICE_PROBE_TIMEOUT) {
        Ok(output) if output.success && !vaapi_failure_marks(&output.stderr) => VaapiStatus {
            available: true,
            detail: format!("VA-API initialised {}", node.display()),
            device: Some(node),
        },
        Ok(output) => {
            let complaint = output.stderr.lines().next().unwrap_or("no detail").trim();
            VaapiStatus {
                available: false,
                detail: format!(
                    "{} could not be initialised ({}); decoding in software",
                    node.display(),
                    complaint
                ),
                device: Some(node),
            }
        }
        Err(detail) => VaapiStatus {
            available: false,
            detail: format!("the VA-API probe did not complete ({detail}); decoding in software"),
            device: Some(node),
        },
    }
}

/// The tool a runtime runs frames through (first required tool), if present.
fn frame_tool(runtime: VideoRuntime) -> Result<PathBuf, MediaError> {
    let availability = runtime_availability(runtime);
    availability
        .tools
        .first()
        .cloned()
        .ok_or_else(|| MediaError::Unavailable {
            backend: runtime.as_str().to_string(),
            detail: availability.detail,
        })
}

/// The tool a runtime reads metadata with (second required tool), if present.
fn probe_tool(runtime: VideoRuntime) -> Result<PathBuf, MediaError> {
    let availability = runtime_availability(runtime);
    availability
        .tools
        .get(1)
        .cloned()
        .ok_or_else(|| MediaError::Unavailable {
            backend: runtime.as_str().to_string(),
            detail: availability.detail,
        })
}

/// The outcome of one bounded child process.
#[derive(Debug)]
struct RunOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run a command with a deadline, capturing its output.
///
/// Implemented by polling `try_wait` rather than blocking in `wait_with_output`:
/// every probe here runs against a user-supplied file, and a prober that never
/// returns must cost a bounded amount of the daemon's time. When the deadline
/// passes the child is killed, so a hung probe cannot outlive the call.
fn run_bounded(argv: &[String], timeout: Duration) -> Result<RunOutput, String> {
    let (program, args) = argv.split_first().ok_or("empty command")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run `{}`: {error}", program))?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(format!("`{program}` could not be waited on: {error}")),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "`{program}` did not finish within {}s",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    Ok(RunOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

/// Metadata for a video file, through whichever runtime `media.backend` allows.
pub fn probe_metadata(path: &Path, config: &MediaConfig) -> Result<MediaInfo, MediaError> {
    with_runtime(&config.backend, |runtime| probe_with(runtime, path))
}

/// Metadata through one named runtime — the part with no fallback inside it.
///
/// Kept separate so that a fallback cannot nest: `probe_metadata` walking the
/// runtimes on top of `open_with` walking them again would pair one runtime's
/// metadata with another runtime's frame tool.
fn probe_with(runtime: VideoRuntime, path: &Path) -> Result<MediaInfo, MediaError> {
    let tool = probe_tool(runtime)?;
    let argv = match runtime {
        VideoRuntime::Gstreamer => gst_discoverer_argv(path),
        VideoRuntime::Ffmpeg => ffprobe_argv(path),
    };

    let output = run_bounded(&argv_for(&tool, &argv), PROBE_TIMEOUT).map_err(|detail| {
        MediaError::Probe {
            path: path.display().to_string(),
            detail,
        }
    })?;
    if !output.success {
        return Err(MediaError::Probe {
            path: path.display().to_string(),
            detail: format!(
                "{} exited with an error: {}",
                tool.display(),
                output.stderr.trim()
            ),
        });
    }

    let raw = match runtime {
        VideoRuntime::Gstreamer => parse_gst_discoverer(&output.stdout),
        VideoRuntime::Ffmpeg => parse_ffprobe(&output.stdout),
    }
    .map_err(|detail| MediaError::Probe {
        path: path.display().to_string(),
        detail,
    })?;
    Ok(info_from(&raw))
}

/// Shape probed metadata into [`MediaInfo`].
fn info_from(raw: &RawVideo) -> MediaInfo {
    MediaInfo {
        kind: ContentKind::Video,
        width: raw.width,
        height: raw.height,
        frame_count: raw.frame_count,
        fps: Some(raw.fps),
        duration: raw.duration(),
        codec: Some(raw.codec.clone()),
    }
}

/// Prepend a tool path to an argv built for "the prober".
fn argv_for(tool: &Path, argv: &[String]) -> Vec<String> {
    std::iter::once(tool.display().to_string())
        .chain(argv.iter().cloned())
        .collect()
}

/// Every runtime `media.backend` allows, in the order they are preferred.
///
/// `auto` is the whole list minus the ones whose binaries are missing; an explicit
/// id is a request about one pipeline, so the list has exactly one entry and an
/// unavailable one is an error rather than a silent substitution.
fn runtime_candidates(backend: &str) -> Result<Vec<VideoRuntime>, MediaError> {
    let requested = backend.trim().to_ascii_lowercase();
    if requested == "auto" || requested.is_empty() {
        let available: Vec<VideoRuntime> = VideoRuntime::ALL
            .iter()
            .copied()
            .filter(|runtime| runtime_availability(*runtime).available)
            .collect();
        if available.is_empty() {
            // The selector already knows how to say this well — every runtime and
            // the binary each one is missing.
            let (_, detail) = select_runtime("auto").expect_err("nothing is available here");
            return Err(MediaError::Unavailable {
                backend: "auto".to_string(),
                detail,
            });
        }
        Ok(available)
    } else {
        Ok(vec![resolve_runtime(backend)?])
    }
}

/// Run `attempt` against the first runtime that works, in preference order.
///
/// This is the degradation rule the docs already prescribe — `media.backend =
/// "auto"` prefers GStreamer and falls back when its plugins do not load (STRATEGY
/// §3, ADR-006) — applied to the failures that actually happen on a machine: a
/// discoverer that hangs on a wedged plugin registry, a decoder element that
/// refuses the file. "GStreamer is installed" is not the same claim as "GStreamer
/// works", and discovering the difference by failing the whole wallpaper would be
/// the wrong trade when the docs name a fallback.
///
/// An explicit `media.backend = "gstreamer"` still gets exactly one attempt: that
/// is a request about one pipeline, and the answer is the reason it could not run.
///
/// The fallback covers opening and probing. A pipeline that dies *mid-stream* is
/// reported through `MediaError::Runtime` rather than swapped underneath a running
/// wallpaper; replacing a live decoder is frame-pacing work, not decode work.
fn with_runtime<T>(
    backend: &str,
    mut attempt: impl FnMut(VideoRuntime) -> Result<T, MediaError>,
) -> Result<T, MediaError> {
    let candidates = runtime_candidates(backend)?;
    let mut failures: Vec<(VideoRuntime, MediaError)> = Vec::new();
    for runtime in candidates {
        match attempt(runtime) {
            Ok(value) => return Ok(value),
            Err(error) => failures.push((runtime, error)),
        }
    }

    // Everything failed. Report the last attempt's error — the pipeline the user
    // ends up with — and name what was tried before it, so a bug report carries the
    // whole story instead of only the tail of it.
    let (_, last) = failures.pop().expect("at least one candidate ran");
    if failures.is_empty() {
        return Err(last);
    }
    let earlier = failures
        .iter()
        .map(|(runtime, error)| format!("{}: {}", runtime.as_str(), one_line(error)))
        .collect::<Vec<_>>()
        .join("; ");
    Err(last.after(&earlier))
}

/// An error on a single line, for use inside another error's message.
fn one_line(error: &MediaError) -> String {
    error.to_string().replace('\n', " ")
}

/// Resolve `media.backend` to a runtime that is actually usable.
fn resolve_runtime(backend: &str) -> Result<VideoRuntime, MediaError> {
    select_runtime(backend).map_err(|(runtime, detail)| MediaError::Unavailable {
        backend: backend.to_string(),
        detail: if backend.trim().is_empty() || backend.trim() == "auto" {
            detail
        } else {
            format!("{detail} (selected backend: {})", runtime.as_str())
        },
    })
}

/// Whether the hardware pipeline may be used: the one device-level answer, shared
/// because VA-API sits below both runtimes.
fn hardware_negotiates() -> bool {
    vaapi_status().available
}

/// A video as a [`MediaDecoder`].
pub struct VideoDecoder {
    runtime: VideoRuntime,
    tool: PathBuf,
    path: PathBuf,
    info: MediaInfo,
    frame_bytes: usize,
    delay: Duration,
    child: Option<Child>,
    hardware: bool,
    next_index: u64,
    frames_decoded: u64,
    finished: bool,
}

impl std::fmt::Debug for VideoDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoDecoder")
            .field("runtime", &self.runtime)
            .field("path", &self.path)
            .field("hardware", &self.hardware)
            .field("size", &(self.info.width, self.info.height))
            .finish_non_exhaustive()
    }
}

impl VideoDecoder {
    /// Open `path` through the runtime `media.backend` selects, probing hardware
    /// decode first.
    ///
    /// Under `auto` this walks the runtimes in the docs' preference order and keeps
    /// the first that can actually open the file; an explicit backend gets one
    /// attempt and its own error (see [`with_runtime`]).
    pub fn open(path: &Path, config: &MediaConfig) -> Result<Self, MediaError> {
        with_runtime(&config.backend, |runtime| {
            Self::open_with(runtime, path, config)
        })
    }

    /// Open through one specific runtime — the part with no fallback inside it.
    fn open_with(
        runtime: VideoRuntime,
        path: &Path,
        config: &MediaConfig,
    ) -> Result<Self, MediaError> {
        let tool = frame_tool(runtime)?;
        let info = probe_with(runtime, path)?;
        let hardware = hardware_negotiates();

        if !hardware && config.hw_decode_required {
            return Err(MediaError::HwDecodeRequired {
                path: path.display().to_string(),
                decoder: runtime.software_decoder().to_string(),
            });
        }

        let frame_bytes = info.width as usize * info.height as usize * 4;
        let delay = Duration::from_secs_f64(
            1.0 / info
                .fps
                .filter(|fps| fps.is_finite() && *fps > 0.0)
                .unwrap_or(10.0),
        );

        let mut decoder = Self {
            runtime,
            tool,
            path: path.to_path_buf(),
            info,
            frame_bytes,
            delay,
            child: None,
            hardware,
            next_index: 0,
            frames_decoded: 0,
            finished: false,
        };
        decoder.spawn()?;
        Ok(decoder)
    }

    /// Start (or restart) the frame pipeline.
    fn spawn(&mut self) -> Result<(), MediaError> {
        let argv = match self.runtime {
            VideoRuntime::Gstreamer => gstreamer_frame_argv(&self.tool, &self.path, self.hardware),
            VideoRuntime::Ffmpeg => ffmpeg_frame_argv(&self.tool, &self.path, self.hardware),
        };
        let (program, args) = argv.split_first().ok_or_else(|| MediaError::Runtime {
            backend: self.runtime.as_str().to_string(),
            detail: "empty pipeline".to_string(),
        })?;
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| MediaError::Runtime {
                backend: self.runtime.as_str().to_string(),
                detail: format!("could not start `{program}`: {error}"),
            })?;
        self.child = Some(child);
        self.finished = false;
        Ok(())
    }

    /// Which pipeline is running.
    pub fn runtime(&self) -> VideoRuntime {
        self.runtime
    }

    /// The file being decoded.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        // A decoder that leaves `ffmpeg`/`gst-launch` behind would outlive the
        // daemon that started it; the child is ours to end.
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl MediaDecoder for VideoDecoder {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_frame(&mut self) -> Result<Option<DecodedFrame>, MediaError> {
        if self.finished {
            return Ok(None);
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let Some(stdout) = child.stdout.as_mut() else {
            self.finished = true;
            return Ok(None);
        };

        let mut pixels = vec![0_u8; self.frame_bytes];
        let mut filled = 0;
        while filled < self.frame_bytes {
            match stdout.read(&mut pixels[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(MediaError::Runtime {
                        backend: self.runtime.as_str().to_string(),
                        detail: error.to_string(),
                    });
                }
            }
        }

        if filled == 0 {
            self.finished = true;
            return Ok(None);
        }
        if filled < self.frame_bytes {
            // A partial frame is a broken pipeline, not an end of stream: handing
            // it to the renderer would paint garbage from a short buffer.
            self.finished = true;
            return Err(MediaError::Runtime {
                backend: self.runtime.as_str().to_string(),
                detail: format!(
                    "the pipeline ended mid-frame ({filled} of {} bytes)",
                    self.frame_bytes
                ),
            });
        }

        let index = self.next_index;
        self.next_index += 1;
        self.frames_decoded += 1;
        Ok(Some(DecodedFrame::new(
            index,
            self.info.width,
            self.info.height,
            self.delay,
            pixels,
        )?))
    }

    fn rewind(&mut self) -> Result<(), MediaError> {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
        self.next_index = 0;
        self.spawn()
    }

    fn stats(&self) -> DecoderStats {
        DecoderStats {
            // Video is never frame-cached: it is decoded on demand at the frame
            // rate, which is what keeps a 10-minute clip inside the memory budget.
            mode: DecodeMode::Streaming,
            path: if self.hardware {
                DecodePath::hardware(self.runtime.hardware_decoder())
            } else {
                DecodePath::software(self.runtime.software_decoder())
            },
            frames_decoded: self.frames_decoded,
            cached_frames: 0,
            cache_bytes: 0,
            cache_cap_bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::any_video_runtime;

    const MP4: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/video-5frame.mp4"
    );

    #[test]
    fn the_gstreamer_pipeline_names_the_elements_the_docs_specify() {
        let argv = gstreamer_frame_argv(
            Path::new("/usr/bin/gst-launch-1.0"),
            Path::new("/walls/clip.mp4"),
            false,
        );
        let joined = argv.join(" ");
        assert!(
            joined.contains("filesrc location=/walls/clip.mp4"),
            "{joined}"
        );
        assert!(joined.contains("decodebin"), "{joined}");
        assert!(joined.contains("video/x-raw,format=RGBA"), "{joined}");
        assert!(joined.contains("fdsink fd=1"), "{joined}");

        // The hardware element is a real substitution, not a comment. Compared
        // element-by-element because `vaapidecodebin` contains the `decodebin`
        // substring — a `contains` check would pass on the software pipeline.
        let hw_argv = gstreamer_frame_argv(
            Path::new("/usr/bin/gst-launch-1.0"),
            Path::new("/walls/clip.mp4"),
            true,
        );
        let elements: Vec<&str> = hw_argv.iter().map(String::as_str).collect();
        assert!(elements.contains(&"vaapidecodebin"), "{elements:?}");
        assert!(!elements.contains(&"decodebin"), "{elements:?}");
        let sw_elements: Vec<&str> = argv.iter().map(String::as_str).collect();
        assert!(sw_elements.contains(&"decodebin"), "{sw_elements:?}");
    }

    #[test]
    fn the_ffmpeg_pipeline_drops_audio_and_never_reads_our_stdin() {
        let argv = ffmpeg_frame_argv(
            Path::new("/usr/bin/ffmpeg"),
            Path::new("/walls/a.mp4"),
            false,
        );
        let joined = argv.join(" ");
        assert!(joined.contains("-nostdin"), "{joined}");
        assert!(joined.contains("-an"), "audio must be dropped: {joined}");
        assert!(joined.contains("-pix_fmt rgba"), "{joined}");
        assert!(joined.contains("-f rawvideo"), "{joined}");
        assert!(!joined.contains("-hwaccel"), "software pipeline: {joined}");

        let hw = ffmpeg_frame_argv(
            Path::new("/usr/bin/ffmpeg"),
            Path::new("/walls/a.mp4"),
            true,
        )
        .join(" ");
        assert!(hw.contains("-hwaccel vaapi"), "{hw}");
    }

    #[test]
    fn the_device_probe_asks_libva_to_initialise_the_node() {
        // The probe is about the *device*, because the exit status of a pipeline is
        // not: `ffmpeg -hwaccel vaapi` exits 0 on a machine whose driver failed.
        let argv = device_probe_argv(
            Path::new("/usr/bin/ffmpeg"),
            Path::new("/dev/dri/renderD128"),
        )
        .join(" ");
        assert!(
            argv.contains("-init_hw_device vaapi=owe:/dev/dri/renderD128"),
            "{argv}"
        );
        assert!(argv.contains("-f null"), "{argv}");
    }

    #[test]
    fn a_driver_that_cannot_initialise_is_read_as_software() {
        // The marker list is pinned against the reference machine's real failure
        // capture, so this test is about what the driver actually says.
        const CAPTURE: &str = include_str!("../tests/fixtures/video/vaapi-failure.txt");
        assert!(
            vaapi_failure_marks(CAPTURE),
            "the captured driver failure must be recognised: {CAPTURE}"
        );

        // And a clean run is not mistaken for a failure.
        assert!(!vaapi_failure_marks(""));
        assert!(!vaapi_failure_marks(
            "frame=    1 fps=0.0 q=-0.0 Lsize=N/A time=00:00:00.00"
        ));
    }

    #[test]
    fn hardware_is_claimed_only_when_a_device_initialised() {
        let status = vaapi_status();
        assert!(
            !status.detail.is_empty(),
            "the status must always say why, in both directions"
        );
        if status.available {
            assert!(
                status.device.is_some(),
                "hardware needs a device it initialised"
            );
        }
        assert_eq!(
            hardware_negotiates(),
            status.available,
            "the pipeline choice must follow the verified status"
        );
    }

    /// Whether both backends are usable here, so the fallback chain has two links.
    fn both_runtimes_available() -> bool {
        if VideoRuntime::ALL
            .iter()
            .all(|runtime| runtime_availability(*runtime).available)
        {
            true
        } else {
            eprintln!("skipping: this machine does not have both video runtimes");
            false
        }
    }

    #[test]
    fn auto_uses_the_next_runtime_when_the_first_one_fails() {
        // The docs' degradation rule (STRATEGY §3 / ADR-006), proven without needing
        // a broken GStreamer install: the attempt is injected, so what is verified is
        // that a failure is followed by the next runtime and not by an error.
        if !both_runtimes_available() {
            return;
        }
        let mut tried = Vec::new();
        let value = with_runtime("auto", |runtime| {
            tried.push(runtime);
            match runtime {
                VideoRuntime::Gstreamer => Err(MediaError::Probe {
                    path: "/tmp/x.mp4".to_string(),
                    detail: "`gst-discoverer-1.0` did not finish within 10s".to_string(),
                }),
                VideoRuntime::Ffmpeg => Ok("frames"),
            }
        })
        .expect("auto must use the runtime that works");
        assert_eq!(value, "frames");
        assert_eq!(tried, vec![VideoRuntime::Gstreamer, VideoRuntime::Ffmpeg]);
    }

    #[test]
    fn an_explicit_backend_gets_exactly_one_attempt() {
        // `media.backend = "ffmpeg"` is a request about one pipeline; answering it
        // with the other runtime would be the silent substitution the capability
        // surface exists to prevent.
        let Some(runtime) = VideoRuntime::ALL
            .iter()
            .copied()
            .find(|runtime| runtime_availability(*runtime).available)
        else {
            eprintln!("skipping: no video runtime installed");
            return;
        };
        let mut tried = Vec::new();
        let error = with_runtime::<()>(runtime.as_str(), |attempted| {
            tried.push(attempted);
            Err(MediaError::Runtime {
                backend: attempted.as_str().to_string(),
                detail: "the pipeline refused the file".to_string(),
            })
        })
        .expect_err("an explicit pipeline reports its own failure");
        assert_eq!(tried, vec![runtime], "no silent substitution");
        assert!(!error.to_string().contains("also tried"), "{error}");
    }

    #[test]
    fn a_failure_from_every_runtime_keeps_the_earlier_one_in_the_message() {
        if !both_runtimes_available() {
            return;
        }
        let error = with_runtime::<()>("auto", |runtime| {
            Err(MediaError::Probe {
                path: "/tmp/x.mp4".to_string(),
                detail: format!("{} refused it", runtime.as_str()),
            })
        })
        .expect_err("both runtimes failed");
        assert!(error.to_string().contains("ffmpeg refused it"), "{error}");
        assert!(
            error.to_string().contains("gstreamer refused it"),
            "the earlier attempt must survive in the message, or the fallback hides why it ran: {error}"
        );
    }

    #[test]
    fn a_runtime_without_its_tools_is_reported_as_unavailable_rather_than_broken() {
        // Whatever this machine has, the report has to be one of the two honest
        // answers — never a decode error for a missing binary.
        match VideoDecoder::open(Path::new(MP4), &MediaConfig::default()) {
            Ok(decoder) => {
                assert_eq!(decoder.info().kind, ContentKind::Video);
                assert_eq!((decoder.info().width, decoder.info().height), (64, 48));
            }
            Err(error) => {
                assert!(
                    matches!(error, MediaError::Unavailable { .. }),
                    "a missing runtime must not look like a broken file: {error}"
                );
                assert!(!any_video_runtime());
            }
        }
    }

    #[test]
    fn a_real_video_decodes_to_rgba_frames_when_a_runtime_is_present() {
        // The real decode path against a real fixture. When no runtime is
        // installed this prints why and returns instead of passing silently, the
        // same convention the GPU tests in `owe-render` use.
        let Some(decoder) = open_fixture() else {
            return;
        };
        let mut decoder = decoder;
        let info = decoder.info().clone();

        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame().expect("frame") {
            frames.push(frame);
        }
        assert_eq!(frames.len(), 5, "the fixture has five frames");
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame.index(), index as u64);
            assert_eq!(frame.size(), (info.width, info.height));
            assert_eq!(
                frame.pixels().len(),
                info.width as usize * info.height as usize * 4
            );
        }

        let stats = decoder.stats();
        // Decode path is reported from the negotiation that actually happened; no
        // VA-API driver on this machine legitimately means "software".
        assert_eq!(stats.mode, DecodeMode::Streaming);
        assert!(
            !stats.path.decoder().is_empty(),
            "the decoder name is part of the report"
        );
    }

    #[test]
    fn rewinding_a_video_restarts_the_pipeline() {
        let Some(mut decoder) = open_fixture() else {
            return;
        };
        let first = decoder.next_frame().expect("frame").expect("frame 0");
        assert!(decoder.next_frame().expect("frame").is_some());
        decoder.rewind().expect("rewind");
        let again = decoder.next_frame().expect("frame").expect("frame 0 again");
        assert_eq!(again.index(), 0);
        assert_eq!(again.pixels().len(), first.pixels().len());
    }

    #[test]
    fn a_probe_of_a_missing_file_is_not_a_missing_runtime() {
        // Order matters in the error the user sees: "this machine cannot decode
        // video" and "that file is not a video" are different problems.
        let config = MediaConfig::default();
        if !any_video_runtime() {
            let error = probe_metadata(Path::new("/nonexistent.mp4"), &config)
                .expect_err("no runtime means no probe");
            assert!(matches!(error, MediaError::Unavailable { .. }), "{error}");
            return;
        }
        let error = probe_metadata(Path::new("/nonexistent.mp4"), &config)
            .expect_err("a missing file must not probe");
        assert!(matches!(error, MediaError::Probe { .. }), "{error}");
    }

    #[test]
    fn hw_decode_required_refuses_a_software_only_pipeline_loudly() {
        if !any_video_runtime() {
            eprintln!("skipping: no video runtime installed");
            return;
        }
        let config = MediaConfig {
            hw_decode_required: true,
            ..MediaConfig::default()
        };
        match VideoDecoder::open(Path::new(MP4), &config) {
            // A machine whose VA-API driver negotiates satisfies the requirement.
            Ok(decoder) => assert!(decoder.stats().path.is_hardware()),
            Err(error) => {
                assert!(
                    matches!(error, MediaError::HwDecodeRequired { .. }),
                    "{error}"
                );
                assert!(error.to_string().contains("hw_decode_required"), "{error}");
            }
        }
    }

    /// Open the committed fixture, or print why not and return `None`.
    fn open_fixture() -> Option<VideoDecoder> {
        match VideoDecoder::open(Path::new(MP4), &MediaConfig::default()) {
            Ok(decoder) => Some(decoder),
            Err(error) => {
                eprintln!("skipping: video decode unavailable here ({error})");
                None
            }
        }
    }

    #[test]
    fn the_bounded_runner_reports_a_nonzero_exit_instead_of_hanging() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo out; echo err >&2; exit 3".to_string(),
        ];
        let output = run_bounded(&argv, Duration::from_secs(5)).expect("run");
        assert!(!output.success);
        assert_eq!(output.stdout.trim(), "out");
        assert_eq!(output.stderr.trim(), "err");

        let hanging = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
        let error = run_bounded(&hanging, Duration::from_millis(200))
            .expect_err("a hung probe must be killed, not waited on");
        assert!(error.contains("did not finish"), "{error}");
    }

    #[test]
    fn the_tool_lookup_returns_an_executable_from_the_real_path() {
        // `sh` is on every Unix `PATH`, so this pins the lookup against the real
        // environment rather than a hardcoded list of distro locations.
        let shell = find_on_path("sh").expect("sh must be found on PATH");
        assert!(shell.is_absolute(), "{} is not absolute", shell.display());
        assert!(shell.is_file());
        assert!(find_on_path("definitely-not-a-real-binary-owe").is_none());
    }
}
