//! Decoding media into frames.
//!
//! One trait, [`MediaDecoder`], covers every content kind this project renders,
//! and one function, [`open`], picks the implementation from the file and the
//! `[media]` config. Nothing here knows about Wayland, wgpu, or surfaces — that
//! separation is what keeps the whole media path testable without a compositor,
//! and it is the reason a decoder can be handed to a renderer as plain pixels.
//!
//! # What each backend is, and what happens when it is missing
//!
//! | Kind | Backend | If the machine cannot run it |
//! |---|---|---|
//! | `static-image` | `image` crate | a decode error naming the file |
//! | `animated-image` | `image` crate's GIF/APNG/WebP animation decoders | a decode error naming the file |
//! | `video` | GStreamer (ADR-006), FFmpeg fallback | [`MediaError::Unavailable`] naming the missing binary — and `capabilities.unavailable` says so before a user asks |
//! | `shader` | P5 | [`MediaError::UnsupportedKind`] |
//!
//! The video row is the docs' own degradation strategy rather than a new one:
//! `media.backend = "auto"` prefers GStreamer and falls back to FFmpeg when its
//! plugins do not load (STRATEGY §3, ADR-006), and if neither runtime is installed
//! the daemon advertises that instead of pretending. Hardware decode is reported
//! only when the real pipeline negotiated it (TRD FR-LIVE-3), which is how the
//! Reference Profile's archived `i965` VA-API driver needs no special case.

mod animated;
mod cache;
mod probe;
mod still;
mod video;

use std::path::Path;
use std::time::Duration;

use thiserror::Error;

pub use animated::{AnimatedImageDecoder, MIN_FRAME_DELAY};
pub use cache::{Compression, FrameCache};
pub use probe::{
    RawVideo, RuntimeAvailability, VideoRuntime, any_video_runtime, runtime_availability,
    select_runtime,
};
pub use still::{
    DEFAULT_MAX_PIXELS, DecodedImage, ImageDecoder, THUMBNAIL_MAX_EDGE, decode_file,
    decode_file_with_budget, thumbnail_png, thumbnail_png_from, thumbnail_size,
};
pub use video::{
    DecoderCancellation, VaapiStatus, VideoDecoder, ffmpeg_frame_argv, gstreamer_frame_argv,
    vaapi_failure_marks, vaapi_status,
};

// Re-exported because it is part of this crate's own public surface: [`open`] and
// [`probe`] take a `MediaConfig`, so a caller should not need a direct `owe-core`
// dependency to name it.
pub use owe_core::config::MediaConfig;

use owe_core::model::ContentKind;

/// Everything that can go wrong decoding.
#[derive(Debug, Error)]
pub enum MediaError {
    /// The file could not be read.
    #[error("cannot read `{path}`: {source}")]
    Io {
        /// File that failed.
        path: String,
        /// Underlying cause.
        source: std::io::Error,
    },

    /// The bytes are not a format this build can decode.
    #[error("cannot decode `{path}`: {detail}")]
    Decode {
        /// File that failed.
        path: String,
        /// Decoder's own message.
        detail: String,
    },

    /// The image is larger than the configured decode budget.
    #[error(
        "`{path}` is {pixels} megapixels, above the {limit} megapixel decode budget; \
         refusing rather than risking an out-of-memory kill"
    )]
    TooLarge {
        /// File that failed.
        path: String,
        /// Image size in megapixels (rounded).
        pixels: u64,
        /// Configured limit in megapixels.
        limit: u64,
    },

    /// A decoded buffer did not match its declared size.
    #[error("decoded buffer length {actual} does not match {width}x{height} RGBA8 ({expected})")]
    BufferLength {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
        /// Expected byte count.
        expected: usize,
        /// Actual byte count.
        actual: usize,
    },

    /// The image decoded to nothing.
    #[error("image is empty ({width}x{height})")]
    EmptyImage {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
    },

    /// The content kind is not decodable by this build.
    #[error(
        "`{kind}` wallpapers are not supported by this build yet (see docs/IMPLEMENTATION-PLAN.md)"
    )]
    UnsupportedKind {
        /// Kind id that was requested.
        kind: &'static str,
    },

    /// A thumbnail could not be encoded.
    #[error("cannot encode a thumbnail for `{path}`: {detail}")]
    Encode {
        /// File the thumbnail was for.
        path: String,
        /// Encoder's own message.
        detail: String,
    },

    /// A cached frame could not be expanded.
    #[error("the frame cache is unreadable: {detail}")]
    Cache {
        /// Which codec failed and why.
        detail: String,
    },

    /// A backend this machine cannot run was asked to decode.
    ///
    /// This is the honest form of "not installed": it names the backend and the
    /// concrete thing that is missing, so `owed` can report it rather than
    /// reporting a corrupt file.
    #[error("{backend} cannot decode here: {detail}")]
    Unavailable {
        /// Backend id (`gstreamer`, `ffmpeg`).
        backend: String,
        /// What is missing, in the words of the probe.
        detail: String,
    },

    /// Metadata probing failed.
    #[error("cannot read the metadata of `{path}`: {detail}")]
    Probe {
        /// File that failed.
        path: String,
        /// Prober's own message.
        detail: String,
    },

    /// The decoder process failed while producing frames.
    #[error("{backend} failed while decoding: {detail}")]
    Runtime {
        /// Backend id.
        backend: String,
        /// What the process said (or its exit status).
        detail: String,
    },

    /// Software decode was refused because the config demands hardware decode.
    #[error(
        "`{path}` would decode in software ({decoder}), and media.hw_decode_required = true; \
         install a working VA-API driver or set it to false"
    )]
    HwDecodeRequired {
        /// File that was requested.
        path: String,
        /// The software decoder that would have run.
        decoder: String,
    },
}

impl MediaError {
    /// Add a note about an earlier attempt, without losing the original message.
    ///
    /// Used by the `media.backend = "auto"` fallback: when GStreamer is tried and
    /// FFmpeg answers, the error a user sees must still say what GStreamer said, or
    /// the fallback would hide the very reason it ran.
    pub(crate) fn after(self, note: &str) -> Self {
        match self {
            MediaError::Probe { path, detail } => MediaError::Probe {
                path,
                detail: format!("{detail} (also tried {note})"),
            },
            MediaError::Runtime { backend, detail } => MediaError::Runtime {
                backend,
                detail: format!("{detail} (also tried {note})"),
            },
            MediaError::Unavailable { backend, detail } => MediaError::Unavailable {
                backend,
                detail: format!("{detail} (also tried {note})"),
            },
            other => other,
        }
    }
}

pub(crate) fn pixel_count(width: u32, height: u32) -> Option<u64> {
    u64::from(width).checked_mul(u64::from(height))
}

pub(crate) fn rgba8_buffer_len(width: u32, height: u32) -> Option<usize> {
    pixel_count(width, height)?
        .checked_mul(4)
        .and_then(|bytes| usize::try_from(bytes).ok())
}

pub(crate) fn bounded_frame_len(
    path: &Path,
    width: u32,
    height: u32,
    max_pixels: u64,
) -> Result<usize, MediaError> {
    let pixels = pixel_count(width, height).unwrap_or(u64::MAX);
    if pixels <= max_pixels {
        if let Some(bytes) = rgba8_buffer_len(width, height) {
            return Ok(bytes);
        }
    }
    Err(MediaError::TooLarge {
        path: path.display().to_string(),
        pixels: pixels.div_ceil(1_000_000),
        limit: max_pixels / 1_000_000,
    })
}

/// Static metadata about a piece of content.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    /// Which content kind this is.
    pub kind: ContentKind,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Frames the container holds, when it says so.
    ///
    /// `None` is a real answer — an animated image whose tail is still streaming,
    /// or a container that does not record a count — and is never a zero standing
    /// in for "unknown".
    pub frame_count: Option<u64>,
    /// Nominal frame rate, when the container has one (video).
    ///
    /// Animated images leave this `None` on purpose: their timing is per frame,
    /// and a single "fps" would be a fiction for a GIF that holds some frames
    /// longer than others.
    pub fps: Option<f64>,
    /// Total duration, when known.
    pub duration: Option<Duration>,
    /// Codec or container name, for logs and stats.
    pub codec: Option<String>,
}

/// One presented frame: RGBA8 pixels, its presentation index, and how long the
/// container says it should be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    index: u64,
    width: u32,
    height: u32,
    delay: Duration,
    pixels: Vec<u8>,
}

impl DecodedFrame {
    /// Build a frame, checking the pixel buffer is exactly `width × height × 4`.
    pub fn new(
        index: u64,
        width: u32,
        height: u32,
        delay: Duration,
        pixels: Vec<u8>,
    ) -> Result<Self, MediaError> {
        let expected = rgba8_buffer_len(width, height).unwrap_or(usize::MAX);
        if pixels.len() != expected {
            return Err(MediaError::BufferLength {
                width,
                height,
                expected,
                actual: pixels.len(),
            });
        }
        if width == 0 || height == 0 {
            return Err(MediaError::EmptyImage { width, height });
        }
        Ok(Self {
            index,
            width,
            height,
            delay,
            pixels,
        })
    }

    /// Presentation index (0-based, in container order).
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Frame width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Pixel size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// How long this frame should be shown.
    pub fn delay(&self) -> Duration {
        self.delay
    }

    /// RGBA8 pixels, row-major from the top-left — the layout the render path
    /// takes ([`owe_render`]'s `Rgba8` pixel format).
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Turn the frame into a [`DecodedImage`] without copying the pixels.
    pub fn into_image(self) -> DecodedImage {
        DecodedImage::new(self.width, self.height, self.pixels)
            .expect("a frame was validated when it was built")
    }
}

/// Whether frames come from the cache or from the decoder on demand (FR-LIVE-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeMode {
    /// Every frame fitted in the configured cap; playback never touches the file.
    Cached,
    /// The cap could not hold the content, so frames after the cached head are
    /// decoded on demand. Slower per frame, bounded memory.
    Streaming,
}

impl DecodeMode {
    /// Stable id used in stats and (from P6) `stats.get`.
    pub fn as_str(self) -> &'static str {
        match self {
            DecodeMode::Cached => "cached",
            DecodeMode::Streaming => "streaming",
        }
    }
}

/// Whether pixels came from a hardware decoder, and which one.
///
/// The distinction is a TRD requirement (FR-LIVE-3) and not decoration: "hardware
/// decode" is the claim the project's resource thesis rests on, so it is reported
/// only when a real pipeline negotiated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodePath {
    /// Hardware decode negotiated; the driver name is reported.
    Hardware {
        /// Decoder name, e.g. `vaapi (hardware)`.
        decoder: String,
    },
    /// Software decode.
    Software {
        /// Decoder name, e.g. `ffmpeg (software)`.
        decoder: String,
    },
}

impl DecodePath {
    /// A hardware path with the given decoder name.
    pub fn hardware(decoder: impl Into<String>) -> Self {
        DecodePath::Hardware {
            decoder: decoder.into(),
        }
    }

    /// A software path with the given decoder name.
    pub fn software(decoder: impl Into<String>) -> Self {
        DecodePath::Software {
            decoder: decoder.into(),
        }
    }

    /// The decoder's name.
    pub fn decoder(&self) -> &str {
        match self {
            DecodePath::Hardware { decoder } | DecodePath::Software { decoder } => decoder,
        }
    }

    /// Whether this is the hardware path.
    pub fn is_hardware(&self) -> bool {
        matches!(self, DecodePath::Hardware { .. })
    }

    /// `hardware` or `software` — the word FR-LIVE-3 puts in `get`/GUI status.
    pub fn as_str(&self) -> &'static str {
        if self.is_hardware() {
            "hardware"
        } else {
            "software"
        }
    }
}

/// What a decoder is doing, for `stats` and (from P6) `stats.get`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderStats {
    /// Cached or streaming.
    pub mode: DecodeMode,
    /// Hardware or software, with the decoder's name.
    pub path: DecodePath,
    /// Frames handed out so far.
    pub frames_decoded: u64,
    /// Frames held in the cache.
    pub cached_frames: usize,
    /// Bytes the cache occupies (compressed).
    pub cache_bytes: usize,
    /// The cache's hard cap.
    pub cache_cap_bytes: usize,
}

/// A stream of frames for one piece of content.
///
/// Decode happens off the UI path (ARCHITECTURE §6), and the way that is arranged
/// here is: a decoder is **created and consumed on the decode thread**, and only
/// [`DecodedFrame`]s — plain `Vec<u8>`s — cross back. That is why the trait does
/// not require `Send`, and it is a documented deviation from the design note in
/// BACKEND-DESIGN §2, which sketched `MediaDecoder: Send`.
///
/// The reason is concrete rather than stylistic: the `image` crate's animation
/// iterator (`Frames`) holds `Box<dyn Iterator<…>>` **without** `Send`, and this
/// crate forbids `unsafe`, so an animated decoder that held a live iterator could
/// never be `Send` — only lies or a reimplementation of GIF frame composition
/// could make it so. Frame hand-off is what actually needs to cross threads, and
/// it does.
pub trait MediaDecoder {
    /// Static metadata, known after opening.
    fn info(&self) -> &MediaInfo;

    /// The next frame in presentation order, or `Ok(None)` at the end.
    ///
    /// A still image yields exactly one frame and then `None`; an animation loops
    /// only if the caller calls [`MediaDecoder::rewind`], so "the end" is never
    /// confused with "start again".
    fn next_frame(&mut self) -> Result<Option<DecodedFrame>, MediaError>;

    /// Go back to the first frame.
    fn rewind(&mut self) -> Result<(), MediaError>;

    /// Cache/streaming mode, decoder path and counters.
    fn stats(&self) -> DecoderStats;

    /// A handle that can interrupt a decoder blocked on external input.
    fn cancellation(&self) -> Option<video::DecoderCancellation> {
        None
    }
}

fn classify_image(path: &Path) -> Result<Option<ContentKind>, MediaError> {
    let Some(format) = animated::image_format(path)? else {
        return Ok(None);
    };
    let animated = animated::is_animated_format(format) && animated::has_animation(path, format)?;
    Ok(Some(if animated {
        ContentKind::AnimatedImage
    } else {
        ContentKind::StaticImage
    }))
}

fn classify_path(path: &Path) -> Result<Option<ContentKind>, MediaError> {
    match ContentKind::from_path(path) {
        Some(ContentKind::Video) if path.exists() => {
            Ok(classify_image(path)?.or(Some(ContentKind::Video)))
        }
        Some(kind @ (ContentKind::Video | ContentKind::Shader | ContentKind::Plugin)) => {
            Ok(Some(kind))
        }
        Some(kind @ (ContentKind::StaticImage | ContentKind::AnimatedImage)) => {
            Ok(classify_image(path)?.or(Some(kind)))
        }
        None => classify_image(path),
    }
}

/// Open `path` as the kind its content implies.
pub fn open(path: &Path, config: &MediaConfig) -> Result<Box<dyn MediaDecoder>, MediaError> {
    let kind = classify_path(path)?.ok_or(MediaError::UnsupportedKind { kind: "unknown" })?;
    open_resolved_kind(path, kind, config)
}

/// Open `path` as `kind`.
pub fn open_kind(
    path: &Path,
    kind: ContentKind,
    config: &MediaConfig,
) -> Result<Box<dyn MediaDecoder>, MediaError> {
    let kind = match kind {
        ContentKind::StaticImage | ContentKind::AnimatedImage => {
            classify_image(path)?.unwrap_or(kind)
        }
        kind => kind,
    };
    open_resolved_kind(path, kind, config)
}

/// Opens a previously probed media file without repeating its initial probe.
pub fn open_kind_with_info(
    path: &Path,
    kind: ContentKind,
    config: &MediaConfig,
    info: MediaInfo,
) -> Result<Box<dyn MediaDecoder>, MediaError> {
    match kind {
        ContentKind::Video => Ok(Box::new(VideoDecoder::open_with_info(path, config, info)?)),
        kind => open_kind(path, kind, config),
    }
}

fn open_resolved_kind(
    path: &Path,
    kind: ContentKind,
    config: &MediaConfig,
) -> Result<Box<dyn MediaDecoder>, MediaError> {
    match kind {
        ContentKind::StaticImage => Ok(Box::new(ImageDecoder::open(path)?)),
        ContentKind::AnimatedImage => Ok(Box::new(AnimatedImageDecoder::open(path, config)?)),
        ContentKind::Video => Ok(Box::new(VideoDecoder::open(path, config)?)),
        other => Err(MediaError::UnsupportedKind {
            kind: other.as_str(),
        }),
    }
}

/// Static metadata for `path`, without opening a frame stream.
pub fn probe(path: &Path, config: &MediaConfig) -> Result<MediaInfo, MediaError> {
    let kind = classify_path(path)?.ok_or(MediaError::UnsupportedKind { kind: "unknown" })?;
    match kind {
        ContentKind::StaticImage | ContentKind::AnimatedImage => {
            let mut decoder = open_kind(path, kind, config)?;
            let mut info = decoder.info().clone();
            // An animated image knows its frame count only after the head has been
            // pre-cached; that already happened during `open`.
            if let Some(frame) = decoder.next_frame()? {
                info.width = frame.width();
                info.height = frame.height();
            }
            Ok(info)
        }
        ContentKind::Video => video::probe_metadata(path, config),
        other => Err(MediaError::UnsupportedKind {
            kind: other.as_str(),
        }),
    }
}

/// Check that a content kind is something this build can decode.
pub fn ensure_supported(kind: ContentKind) -> Result<(), MediaError> {
    match kind {
        ContentKind::StaticImage | ContentKind::AnimatedImage => Ok(()),
        ContentKind::Video if any_video_runtime() => Ok(()),
        other => Err(MediaError::UnsupportedKind {
            kind: other.as_str(),
        }),
    }
}

/// Content kinds the decode layer can produce frames for on this machine.
///
/// This is deliberately about decoding. The daemon separately reports the
/// presentation capabilities it has wired for those kinds.
pub fn content_kinds() -> Vec<&'static str> {
    let mut kinds = vec!["static-image", "animated-image"];
    if any_video_runtime() {
        kinds.push("video");
    }
    kinds
}

/// Decode backends usable on this machine, in `media.backend` spelling.
pub fn media_backends() -> Vec<&'static str> {
    let mut backends = vec!["image"];
    for runtime in VideoRuntime::ALL {
        if runtime_availability(*runtime).available {
            backends.push(runtime.as_str());
        }
    }
    backends
}

/// Every video runtime and whether it is usable here.
pub fn video_runtimes() -> Vec<RuntimeAvailability> {
    VideoRuntime::ALL
        .iter()
        .map(|runtime| runtime_availability(*runtime))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/anim-3frame.gif"
    );
    const PNG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/still.png");
    const MP4: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/video-5frame.mp4"
    );

    #[test]
    fn the_extension_chooses_the_decoder() {
        let config = MediaConfig::default();
        assert_eq!(
            open(Path::new(PNG), &config).expect("png").info().kind,
            ContentKind::StaticImage
        );
        assert_eq!(
            open(Path::new(GIF), &config).expect("gif").info().kind,
            ContentKind::AnimatedImage
        );
        // Video needs a runtime; on a machine without one this is a precise
        // "unavailable", never a decode failure.
        match open(Path::new(MP4), &config) {
            Ok(decoder) => assert_eq!(decoder.info().kind, ContentKind::Video),
            Err(error) => {
                assert!(matches!(error, MediaError::Unavailable { .. }), "{error}");
                assert!(!any_video_runtime());
            }
        }
    }

    #[test]
    fn shaders_and_plugins_are_named_not_guessed() {
        for kind in [ContentKind::Shader, ContentKind::Plugin] {
            let error = open_kind(Path::new(PNG), kind, &MediaConfig::default())
                .err()
                .expect("modules are not decodable media");
            assert!(
                matches!(error, MediaError::UnsupportedKind { .. }),
                "{error}"
            );
            assert!(error.to_string().contains(kind.as_str()), "{error}");
        }
    }

    #[test]
    fn ensure_supported_follows_what_this_machine_can_actually_decode() {
        assert!(ensure_supported(ContentKind::StaticImage).is_ok());
        assert!(ensure_supported(ContentKind::AnimatedImage).is_ok());
        assert_eq!(
            ensure_supported(ContentKind::Video).is_ok(),
            any_video_runtime(),
            "video is supported exactly when a runtime is"
        );
        assert!(ensure_supported(ContentKind::Shader).is_err());
    }

    #[test]
    fn capability_lists_never_advertise_what_is_not_there() {
        let kinds = content_kinds();
        assert!(kinds.contains(&"static-image"));
        assert!(kinds.contains(&"animated-image"));
        assert_eq!(
            kinds.contains(&"video"),
            any_video_runtime(),
            "video must be advertised exactly when a runtime can decode it"
        );
        for runtime in video_runtimes() {
            assert_eq!(
                media_backends().contains(&runtime.runtime.as_str()),
                runtime.available,
                "{} availability must match its probe: {}",
                runtime.runtime.as_str(),
                runtime.detail
            );
        }
    }

    #[test]
    fn probing_a_frame_shape_matches_the_render_path_expectation() {
        // The render path takes `width, height, RGBA8 len`; a decoder that produced
        // anything else would fail at the GPU boundary instead of here.
        let frame = DecodedFrame::new(0, 4, 2, Duration::from_millis(50), vec![0; 32])
            .expect("valid frame");
        assert_eq!(frame.size(), (4, 2));
        assert_eq!(frame.pixels().len(), 4 * 2 * 4);
        let image = frame.into_image();
        assert_eq!(image.size(), (4, 2));
        assert_eq!(image.byte_len(), 32);
    }

    #[test]
    fn a_frame_with_the_wrong_buffer_size_is_refused_at_construction() {
        let error = DecodedFrame::new(0, 2, 2, Duration::ZERO, vec![0; 15]).unwrap_err();
        assert!(matches!(error, MediaError::BufferLength { .. }), "{error}");
        let error = DecodedFrame::new(0, 0, 2, Duration::ZERO, vec![]).unwrap_err();
        assert!(matches!(error, MediaError::EmptyImage { .. }), "{error}");
    }

    #[test]
    fn image_content_refines_ambiguous_extensions_without_losing_stills() {
        let dir = tempfile::tempdir().unwrap();
        let renamed_gif = dir.path().join("animation.png");
        let renamed_png = dir.path().join("still.gif");
        let video_named_png = dir.path().join("still.mp4");
        std::fs::copy(GIF, &renamed_gif).unwrap();
        std::fs::copy(PNG, &renamed_png).unwrap();
        std::fs::copy(PNG, &video_named_png).unwrap();

        let mut animated = open(&renamed_gif, &MediaConfig::default()).expect("animated content");
        assert_eq!(animated.info().kind, ContentKind::AnimatedImage);
        assert_eq!(animated.next_frame().unwrap().unwrap().index(), 0);

        let mut still = open(&renamed_png, &MediaConfig::default()).expect("still content");
        assert_eq!(still.info().kind, ContentKind::StaticImage);
        assert_eq!(still.next_frame().unwrap().unwrap().index(), 0);
        assert!(still.next_frame().unwrap().is_none());

        let mut video_named_png =
            open(&video_named_png, &MediaConfig::default()).expect("image content");
        assert_eq!(video_named_png.info().kind, ContentKind::StaticImage);
        assert!(video_named_png.next_frame().unwrap().is_some());
    }

    #[test]
    fn overflowing_frame_dimensions_are_rejected_without_arithmetic_wraparound() {
        let error =
            DecodedFrame::new(0, u32::MAX, u32::MAX, Duration::ZERO, Vec::new()).unwrap_err();
        assert!(matches!(error, MediaError::BufferLength { .. }), "{error}");
        assert!(rgba8_buffer_len(u32::MAX, u32::MAX).is_none());
    }
}
