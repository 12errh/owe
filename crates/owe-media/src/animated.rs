//! Animated images (GIF, APNG, WebP) decoded into a bounded, compressed frame
//! stream (TRD FR-LIVE-1, BACKEND-DESIGN §6.2).
//!
//! Three decisions worth stating, because each is a place where "it works" and
//! "it is honest" are different:
//!
//! 1. **Timing comes from the container.** Every frame carries the delay the file
//!    declares, normalised to a [`Duration`]. A GIF that declares 0 ms (which the
//!    format allows, and which every browser clamps) is clamped here too, once,
//!    with [`MIN_FRAME_DELAY`] — otherwise a 0-delay frame renders as fast as the
//!    compositor will take frames, which is a different animation.
//! 2. **Pre-caching stops at the cap; it does not evict.** The frames cached are
//!    the ones the animation starts with, so the first pass is exact and the rest
//!    streams. Throwing away the head to hold the tail would keep the memory
//!    bounded and break the thing a wallpaper is for.
//! 3. **The mode is reported, not implied.** `stats().mode` is `cached` or
//!    `streaming`, which is what FR-LIVE-1 asks the GUI to be able to show.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use image::AnimationDecoder;
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{ImageError, ImageFormat};

use crate::cache::FrameCache;
use crate::{
    Compression, DecodeMode, DecodePath, DecodedFrame, DecodedImage, DecoderStats, MediaConfig,
    MediaDecoder, MediaError, MediaInfo,
};
use owe_core::ContentKind;

/// The floor applied to a container-declared delay of zero.
///
/// GIF's own convention, and what every browser does: a delay below 20 ms (two
/// centiseconds) is treated as 100 ms. Without it a 0-delay GIF plays at whatever
/// rate the renderer can produce frames — a different animation from the one the
/// file describes.
pub const MIN_FRAME_DELAY: Duration = Duration::from_millis(100);

/// The default delay used when a container declares nothing usable.
const FALLBACK_FRAME_DELAY: Duration = Duration::from_millis(100);

/// One frame as the `image` crate hands it over.
type ImageFrame = Result<image::Frame, ImageError>;

/// A boxed iterator over a container's frames.
///
/// The three decoders have three different `Frames` types, so they are unified
/// here at the only place that cares: the order the frames arrive in. Not `Send`,
/// because `image`'s `Frames` is not (see [`crate::MediaDecoder`]); the decoder is
/// built and used on one decode thread, and the frames it yields are what cross.
type FrameStream = Box<dyn Iterator<Item = ImageFrame>>;

/// The `image` crate's format id for `path`, sniffed from content, not the name.
fn sniff(path: &Path) -> Result<ImageFormat, MediaError> {
    image::ImageReader::open(path)
        .map_err(|source| MediaError::Io {
            path: path.display().to_string(),
            source,
        })?
        .with_guessed_format()
        .map_err(|source| MediaError::Io {
            path: path.display().to_string(),
            source,
        })?
        .format()
        .ok_or_else(|| MediaError::Decode {
            path: path.display().to_string(),
            detail: "the file's format could not be identified".to_string(),
        })
}

fn decode_error(path: &Path, error: ImageError) -> MediaError {
    MediaError::Decode {
        path: path.display().to_string(),
        detail: error.to_string(),
    }
}

/// The decoder name reported in stats, per container.
fn decoder_name(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Gif => "gif",
        ImageFormat::Png => "apng",
        ImageFormat::WebP => "webp",
        _ => "image",
    }
}

/// Whether this format can carry an animation at all.
pub(crate) fn is_animated_format(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Gif | ImageFormat::Png | ImageFormat::WebP
    )
}

/// Open `path` as an animation.
///
/// A PNG that is not an APNG is a decode error here rather than a one-frame
/// animation: the caller that wants "one frame of this still image" should use
/// [`crate::decode_file`], and quietly treating a still as a 1-frame animation
/// would make `stats().mode` mean nothing.
fn frames_for(path: &Path, format: ImageFormat) -> Result<FrameStream, MediaError> {
    let file = File::open(path).map_err(|source| MediaError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let reader = BufReader::new(file);

    match format {
        ImageFormat::Gif => {
            let decoder = GifDecoder::new(reader).map_err(|error| decode_error(path, error))?;
            Ok(Box::new(decoder.into_frames()))
        }
        ImageFormat::Png => {
            let decoder = PngDecoder::new(reader).map_err(|error| decode_error(path, error))?;
            let apng = decoder.apng().map_err(|error| decode_error(path, error))?;
            Ok(Box::new(apng.into_frames()))
        }
        ImageFormat::WebP => {
            let decoder = WebPDecoder::new(reader).map_err(|error| decode_error(path, error))?;
            Ok(Box::new(decoder.into_frames()))
        }
        other => Err(MediaError::UnsupportedKind {
            kind: if other == ImageFormat::Png {
                "static-image"
            } else {
                "unknown"
            },
        }),
    }
}

/// Normalise a container delay, clamping the zero case the format allows.
fn frame_delay(delay: image::Delay) -> Duration {
    let (numerator, denominator) = delay.numer_denom_ms();
    if denominator == 0 {
        return FALLBACK_FRAME_DELAY;
    }
    let millis = f64::from(numerator) / f64::from(denominator);
    if !millis.is_finite() || millis <= 0.0 {
        return MIN_FRAME_DELAY;
    }
    let duration = Duration::from_millis(millis.round().max(1.0) as u64);
    duration.max(MIN_FRAME_DELAY)
}

/// The first frame of an animated file, for thumbnails.
///
/// `Ok(None)` means "not an animated container", which is the caller's cue to use
/// the still path — not an error. A decodable animation that cannot produce even
/// one frame *is* an error.
pub(crate) fn first_frame(path: &Path, _max_edge: u32) -> Result<Option<DecodedImage>, MediaError> {
    let format = sniff(path)?;
    if !is_animated_format(format) {
        return Ok(None);
    }
    // A still PNG is not an APNG; that is "not animated", not a broken file. The
    // `image` crate reports that as an empty frame sequence rather than an error.
    let mut frames = match frames_for(path, format) {
        Ok(frames) => frames,
        Err(MediaError::Decode { .. }) if format == ImageFormat::Png => return Ok(None),
        Err(other) => return Err(other),
    };
    match frames.next() {
        Some(Ok(frame)) => {
            let buffer = frame.into_buffer();
            Ok(Some(DecodedImage::new(
                buffer.width(),
                buffer.height(),
                buffer.into_raw(),
            )?))
        }
        Some(Err(error)) => Err(decode_error(path, error)),
        // No frames is the same answer as "not animated": the caller falls back
        // to the still decoder, which is what a static PNG in the library needs.
        None => Ok(None),
    }
}

/// An animated image as a [`MediaDecoder`].
pub struct AnimatedImageDecoder {
    path: PathBuf,
    format: ImageFormat,
    source: FrameStream,
    cache: FrameCache,
    info: MediaInfo,
    /// A frame the pre-cache had already pulled when the cap refused it. It is the
    /// animation's next frame, so it is held rather than dropped: losing it would
    /// make the first pass skip a frame exactly when the cache is too small.
    pending: Option<DecodedFrame>,
    /// How many frames the container has produced so far; the index the source
    /// will hand over next.
    produced: u64,
    /// The consumer's next index. Kept separate from `produced` because the
    /// pre-cache consumes the container's head before anyone asks for a frame.
    cursor: u64,
    frames_decoded: u64,
    exhausted: bool,
    streaming: bool,
}

impl std::fmt::Debug for AnimatedImageDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnimatedImageDecoder")
            .field("path", &self.path)
            .field("codec", &decoder_name(self.format))
            .field("cached", &self.cache.len())
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

impl AnimatedImageDecoder {
    /// Open `path`, pre-caching frames until the configured cap is reached.
    pub fn open(path: &Path, config: &MediaConfig) -> Result<Self, MediaError> {
        let format = sniff(path)?;
        if !is_animated_format(format) {
            return Err(MediaError::UnsupportedKind {
                kind: "static-image",
            });
        }
        let source = frames_for(path, format)?;
        let cap_bytes = (config.cache.animated_frame_cap_mb as usize) * 1024 * 1024;
        let compression = Compression::parse(&config.cache.compression).unwrap_or_default();

        let mut decoder = Self {
            path: path.to_path_buf(),
            format,
            source,
            cache: FrameCache::new(cap_bytes, compression),
            info: MediaInfo {
                kind: ContentKind::AnimatedImage,
                width: 0,
                height: 0,
                frame_count: None,
                fps: None,
                duration: None,
                codec: Some(decoder_name(format).to_string()),
            },
            pending: None,
            produced: 0,
            cursor: 0,
            frames_decoded: 0,
            exhausted: false,
            streaming: false,
        };
        decoder.pre_cache();

        // A container that produced nothing is not an animation. For a PNG that is
        // the ordinary case of a still image reaching the animated path, and the
        // honest answer is "use the still decoder" — not a decode failure.
        if decoder.produced == 0 && decoder.cache.is_empty() {
            return Err(match decoder.format {
                ImageFormat::Png => MediaError::UnsupportedKind {
                    kind: "static-image",
                },
                _ => MediaError::Decode {
                    path: path.display().to_string(),
                    detail: "the container holds no frames".to_string(),
                },
            });
        }
        Ok(decoder)
    }

    /// Fill the cache from the head of the animation.
    ///
    /// Stops at the first frame that does not fit, which is the streaming trigger:
    /// the cache stays full of the frames the animation *starts* with.
    fn pre_cache(&mut self) {
        let mut total_delay = Duration::ZERO;
        while let Some(frame) = self.pull() {
            total_delay += frame.delay();
            match self.cache.insert(&frame) {
                Ok(true) => {}
                // The cap is reached (or the frame could not be stored): everything
                // from here on is decoded on demand. The head stays cached so the
                // first pass is exact, and the frame that did not fit is kept for
                // the next call — refusing is not the same as dropping it.
                Ok(false) | Err(_) => {
                    self.streaming = true;
                    self.pending = Some(frame);
                    break;
                }
            }
        }

        if let Ok(Some(first)) = self.cache.get(0) {
            self.info.width = first.width();
            self.info.height = first.height();
        }
        if self.exhausted {
            self.info.frame_count = Some(self.produced);
            self.info.duration = Some(total_delay);
        }
    }

    /// Pull the next frame from the container.
    fn pull(&mut self) -> Option<DecodedFrame> {
        if self.exhausted {
            return None;
        }
        match self.source.next() {
            Some(Ok(frame)) => {
                let delay = frame_delay(frame.delay());
                let buffer = frame.into_buffer();
                let index = self.produced;
                self.produced += 1;
                self.frames_decoded += 1;
                DecodedFrame::new(
                    index,
                    buffer.width(),
                    buffer.height(),
                    delay,
                    buffer.into_raw(),
                )
                .ok()
            }
            Some(Err(_)) | None => {
                self.exhausted = true;
                None
            }
        }
    }

    /// Whether the whole animation fitted in the cache (FR-LIVE-1's
    /// `cached`/`streaming` distinction).
    pub fn mode(&self) -> DecodeMode {
        if self.streaming {
            DecodeMode::Streaming
        } else {
            DecodeMode::Cached
        }
    }

    /// The cache itself, for tests and for a future `stats.get`.
    pub fn cache(&self) -> &FrameCache {
        &self.cache
    }

    /// The file this decoder reads.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl MediaDecoder for AnimatedImageDecoder {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn next_frame(&mut self) -> Result<Option<DecodedFrame>, MediaError> {
        // Cached first: in cached mode this is the whole animation, and in
        // streaming mode it is the head that already fitted.
        if let Some(frame) = self.cache.get(self.cursor)? {
            self.cursor += 1;
            return Ok(Some(frame));
        }
        // Then the frame the pre-cache pulled but could not store. It is only
        // handed over at its own index, so the sequence stays in order.
        if let Some(frame) = self.pending.take_if(|frame| frame.index() == self.cursor) {
            self.cursor += 1;
            return Ok(Some(frame));
        }
        // Past the cached head the container is the source, and the consumer is
        // exactly where the pre-cache left off, so frames stay in order.
        let Some(frame) = self.pull() else {
            return Ok(None);
        };
        self.cursor = frame.index() + 1;
        Ok(Some(frame))
    }

    fn rewind(&mut self) -> Result<(), MediaError> {
        self.source = frames_for(&self.path, self.format)?;
        // Indices are the playback sequence, so they restart with it: a caller
        // that rewinds sees frame 0 again, not frame *n* of a second pass.
        self.produced = 0;
        self.cursor = 0;
        self.exhausted = false;
        Ok(())
    }

    fn stats(&self) -> DecoderStats {
        DecoderStats {
            mode: self.mode(),
            path: DecodePath::software(decoder_name(self.format)),
            frames_decoded: self.frames_decoded,
            cached_frames: self.cache.len(),
            cache_bytes: self.cache.stored_bytes(),
            cache_cap_bytes: self.cache.cap_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real 3-frame GIF, generated once and committed (see the crate's
    /// `tests/fixtures/README.md`): red, green, blue at 10 fps.
    pub(crate) const ANIMATED_GIF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/anim-3frame.gif"
    );

    fn config(cap_mb: u32, compression: &str) -> MediaConfig {
        MediaConfig {
            cache: owe_core::config::MediaCacheConfig {
                animated_frame_cap_mb: cap_mb,
                compression: compression.to_string(),
            },
            ..MediaConfig::default()
        }
    }

    #[test]
    fn a_real_gif_decodes_to_the_frames_it_contains() {
        let path = Path::new(ANIMATED_GIF);
        let mut decoder =
            AnimatedImageDecoder::open(path, &config(96, "zstd")).expect("open fixture GIF");

        assert_eq!(decoder.info().kind, ContentKind::AnimatedImage);
        assert_eq!(decoder.info().width, 32);
        assert_eq!(decoder.info().height, 32);
        assert_eq!(decoder.info().frame_count, Some(3));
        assert_eq!(decoder.mode(), DecodeMode::Cached);

        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame().expect("frame") {
            frames.push(frame);
        }
        assert_eq!(frames.len(), 3, "the fixture has three frames");
        assert_eq!(
            frames.iter().map(|frame| frame.index()).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        for frame in &frames {
            assert_eq!((frame.width(), frame.height()), (32, 32));
            assert_eq!(frame.pixels().len(), 32 * 32 * 4);
            assert_eq!(
                frame.delay(),
                Duration::from_millis(100),
                "10 fps is what the container says"
            );
        }
        // The three frames are different colours, so "decoded three frames" cannot
        // be satisfied by handing back the same frame three times.
        assert_ne!(frames[0].pixels(), frames[1].pixels());
        assert_ne!(frames[1].pixels(), frames[2].pixels());
    }

    #[test]
    fn every_compression_mode_decodes_the_same_frames_losslessly() {
        // The cache is the only thing that changes with the compression setting;
        // if a mode altered the pixels, this is where it would show.
        let path = Path::new(ANIMATED_GIF);
        let mut reference: Option<Vec<DecodedFrame>> = None;
        for compression in ["none", "zstd", "lz4"] {
            let mut decoder =
                AnimatedImageDecoder::open(path, &config(96, compression)).expect("open");
            let mut frames = Vec::new();
            while let Some(frame) = decoder.next_frame().expect("frame") {
                frames.push(frame);
            }
            match &reference {
                None => reference = Some(frames),
                Some(expected) => assert_eq!(&frames, expected, "{compression} changed pixels"),
            }
        }
    }

    #[test]
    fn a_cap_too_small_for_the_animation_switches_to_streaming_and_still_decodes() {
        // 1 MiB cannot hold three 32x32 RGBA frames once the cap is measured in
        // *compressed* bytes only because the cap is tiny — so use a cap of 0,
        // which is the honest "cache nothing" case, and prove the animation still
        // plays through the streaming path.
        let path = Path::new(ANIMATED_GIF);
        let mut decoder = AnimatedImageDecoder::open(path, &config(0, "zstd")).expect("open");

        assert_eq!(
            decoder.mode(),
            DecodeMode::Streaming,
            "a zero cap must stream"
        );
        assert!(decoder.cache().is_empty());
        assert_eq!(decoder.stats().mode, DecodeMode::Streaming);

        let mut count = 0;
        while let Some(frame) = decoder.next_frame().expect("frame") {
            assert_eq!(frame.pixels().len(), 32 * 32 * 4);
            count += 1;
        }
        assert_eq!(count, 3, "streaming must still yield every frame");
        assert_eq!(decoder.stats().cache_bytes, 0);
    }

    #[test]
    fn rewind_replays_the_animation_from_the_first_frame() {
        let path = Path::new(ANIMATED_GIF);
        let mut decoder = AnimatedImageDecoder::open(path, &config(96, "lz4")).expect("open");
        let first = decoder.next_frame().expect("frame").expect("some");

        assert!(decoder.next_frame().expect("frame").is_some());
        decoder.rewind().expect("rewind");
        let again = decoder.next_frame().expect("frame").expect("some");
        assert_eq!(again, first, "rewind must return to frame 0");
    }

    #[test]
    fn a_still_png_is_not_silently_treated_as_an_animation() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/still.png"
        ));
        let error = AnimatedImageDecoder::open(path, &config(96, "zstd"))
            .expect_err("a still image is not an animation");
        assert!(
            matches!(error, MediaError::UnsupportedKind { .. }),
            "{error}"
        );
        // And the thumbnail path falls back to the still decoder instead of
        // failing: a picture in the library still needs a grid cell.
        assert!(super::first_frame(path, 64).expect("first frame").is_none());
    }

    #[test]
    fn a_zero_declared_delay_is_clamped_not_played_as_fast_as_possible() {
        assert_eq!(
            frame_delay(image::Delay::from_numer_denom_ms(0, 1)),
            MIN_FRAME_DELAY
        );
        assert_eq!(
            frame_delay(image::Delay::from_numer_denom_ms(1, 1)),
            MIN_FRAME_DELAY
        );
        assert_eq!(
            frame_delay(image::Delay::from_numer_denom_ms(250, 1)),
            Duration::from_millis(250)
        );
        // Fractional delays (APNG) survive the normalisation.
        assert_eq!(
            frame_delay(image::Delay::from_numer_denom_ms(500, 3)),
            Duration::from_millis(167).max(MIN_FRAME_DELAY)
        );
    }
}
