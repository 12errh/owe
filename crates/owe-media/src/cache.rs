//! The bounded, compressed frame cache for animated content (TRD FR-LIVE-1).
//!
//! The resource thesis of this project is that a live wallpaper must be *bounded*
//! first and fast second, so animated frames are held compressed and against a
//! hard byte cap. When the cap cannot hold one more frame the cache refuses
//! (`insert` returns `Ok(false)`) and the caller stops pre-caching and switches to
//! streaming decode — the documented trade (BACKEND-DESIGN §6.2: "slower CPU,
//! bounded RAM — documented tradeoff, GUI-visible in stats").
//!
//! Compression is chosen by config (`media.cache.compression`), and the ratio the
//! cache actually achieves is exposed in [`FrameCache::ratio`] so "compression is
//! on" is a measurable statement rather than a setting.

use std::collections::VecDeque;
use std::time::Duration;

use crate::{DecodedFrame, MediaError};

/// How cached frames are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// Store frames raw. Highest CPU headroom, highest memory cost.
    None,
    /// zstd — the config default, and the one that pays off on photographic frames.
    #[default]
    Zstd,
    /// lz4 — cheaper per frame, less compression; better for flat-colour content.
    Lz4,
}

impl Compression {
    /// Stable id used in config and stats.
    pub fn as_str(self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Zstd => "zstd",
            Compression::Lz4 => "lz4",
        }
    }

    /// Parse the config spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Compression::None),
            "zstd" => Some(Compression::Zstd),
            "lz4" => Some(Compression::Lz4),
            _ => None,
        }
    }

    /// Compress one frame's pixels.
    fn encode(self, pixels: &[u8]) -> Vec<u8> {
        match self {
            Compression::None => pixels.to_vec(),
            Compression::Zstd => {
                zstd::bulk::compress(pixels, 3).unwrap_or_else(|_| pixels.to_vec())
            }
            Compression::Lz4 => lz4_flex::compress_prepend_size(pixels),
        }
    }

    /// Expand one frame's pixels back to `raw_len` bytes.
    fn decode(self, data: &[u8], raw_len: usize) -> Result<Vec<u8>, MediaError> {
        match self {
            Compression::None => {
                if data.len() != raw_len {
                    return Err(MediaError::Cache {
                        detail: "uncompressed frame length does not match its image".to_string(),
                    });
                }
                Ok(data.to_vec())
            }
            Compression::Zstd => {
                zstd::bulk::decompress(data, raw_len).map_err(|error| MediaError::Cache {
                    detail: format!("zstd frame is not readable: {error}"),
                })
            }
            Compression::Lz4 => {
                let size = data
                    .get(..4)
                    .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("four-byte slice")))
                    .ok_or_else(|| MediaError::Cache {
                        detail: "lz4 frame has no size prefix".to_string(),
                    })?;
                if usize::try_from(size).ok() != Some(raw_len) {
                    return Err(MediaError::Cache {
                        detail: "lz4 frame size does not match its image".to_string(),
                    });
                }
                let mut pixels = vec![0; raw_len];
                let written =
                    lz4_flex::decompress_into(&data[4..], &mut pixels).map_err(|error| {
                        MediaError::Cache {
                            detail: format!("lz4 frame is not readable: {error}"),
                        }
                    })?;
                if written != raw_len {
                    return Err(MediaError::Cache {
                        detail: "lz4 frame decoded to the wrong length".to_string(),
                    });
                }
                Ok(pixels)
            }
        }
    }
}

/// One frame held in the cache.
#[derive(Debug, Clone)]
struct CachedFrame {
    index: u64,
    width: u32,
    height: u32,
    delay: Duration,
    raw_len: usize,
    data: Vec<u8>,
}

/// A bounded, compressed, in-order frame cache.
#[derive(Debug)]
pub struct FrameCache {
    cap_bytes: usize,
    compression: Compression,
    frames: VecDeque<CachedFrame>,
    stored_bytes: usize,
    raw_bytes: usize,
    refusals: u64,
}

impl FrameCache {
    /// A cache holding at most `cap_bytes` of compressed frames.
    ///
    /// A cap of 0 is legal and means "cache nothing": every insert is refused and
    /// the caller streams. That is a real configuration, not a degenerate one, and
    /// treating it as "cache nothing" keeps the streaming path exercised.
    pub fn new(cap_bytes: usize, compression: Compression) -> Self {
        Self {
            cap_bytes,
            compression,
            frames: VecDeque::new(),
            stored_bytes: 0,
            raw_bytes: 0,
            refusals: 0,
        }
    }

    /// The hard cap, in bytes.
    pub fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    /// How frames are stored.
    pub fn compression(&self) -> Compression {
        self.compression
    }

    /// Frames currently cached.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Bytes of compressed frames held.
    pub fn stored_bytes(&self) -> usize {
        self.stored_bytes
    }

    /// Bytes those frames would occupy uncompressed.
    pub fn raw_bytes(&self) -> usize {
        self.raw_bytes
    }

    /// How many inserts were refused because the cap was reached.
    pub fn refusals(&self) -> u64 {
        self.refusals
    }

    /// Stored bytes as a fraction of the raw bytes (1.0 = incompressible).
    ///
    /// Reported rather than asserted-on: a flat-colour GIF compresses to a few
    /// percent, a photographic one barely at all, and a test that demanded a
    /// ratio would be testing the fixture.
    pub fn ratio(&self) -> f64 {
        if self.raw_bytes == 0 {
            return 1.0;
        }
        self.stored_bytes as f64 / self.raw_bytes as f64
    }

    /// Whether the frame at `index` is held.
    pub fn contains(&self, index: u64) -> bool {
        self.frames
            .front()
            .map(|frame| frame.index)
            .unwrap_or(u64::MAX)
            <= index
            && self.frames.back().is_some_and(|frame| frame.index >= index)
    }

    /// Store `frame`, if it fits.
    ///
    /// Returns `Ok(false)` — the overflow signal the cap exists for — instead of
    /// evicting: the frames already cached are the ones the animation starts with,
    /// and throwing the beginning away to hold the end would trade a bounded cache
    /// for a pointless one.
    pub fn insert(&mut self, frame: &DecodedFrame) -> Result<bool, MediaError> {
        let data = self.compression.encode(frame.pixels());
        let Some(stored_bytes) = self.stored_bytes.checked_add(data.len()) else {
            self.refusals = self.refusals.saturating_add(1);
            return Ok(false);
        };
        let Some(raw_bytes) = self.raw_bytes.checked_add(frame.pixels().len()) else {
            self.refusals = self.refusals.saturating_add(1);
            return Ok(false);
        };
        if stored_bytes > self.cap_bytes {
            self.refusals = self.refusals.saturating_add(1);
            return Ok(false);
        }
        self.stored_bytes = stored_bytes;
        self.raw_bytes = raw_bytes;
        self.frames.push_back(CachedFrame {
            index: frame.index(),
            width: frame.width(),
            height: frame.height(),
            delay: frame.delay(),
            raw_len: frame.pixels().len(),
            data,
        });
        Ok(true)
    }

    /// Read the frame at `index` back out.
    ///
    /// `Ok(None)` means "not cached" — which is a normal answer in streaming mode,
    /// not a failure.
    pub fn get(&self, index: u64) -> Result<Option<DecodedFrame>, MediaError> {
        let Some(frame) = self.frames.iter().find(|frame| frame.index == index) else {
            return Ok(None);
        };
        let pixels = self.compression.decode(&frame.data, frame.raw_len)?;
        Ok(Some(DecodedFrame::new(
            frame.index,
            frame.width,
            frame.height,
            frame.delay,
            pixels,
        )?))
    }

    /// Drop everything (used when a new wallpaper takes an output over).
    pub fn clear(&mut self) {
        self.frames.clear();
        self.stored_bytes = 0;
        self.raw_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(index: u64, fill: u8) -> DecodedFrame {
        DecodedFrame::new(
            index,
            8,
            8,
            Duration::from_millis(100),
            vec![fill; 8 * 8 * 4],
        )
        .expect("frame")
    }

    /// Pseudo-random pixels, so compression cannot hide an over-cap insert: a flat
    /// 1024-byte frame compresses to a handful of bytes and would fit any cap.
    fn noisy(index: u64) -> DecodedFrame {
        let mut state = 0x2545_f491_4f6c_dd1d_u64 ^ index;
        let pixels = (0..8 * 8 * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xff) as u8
            })
            .collect();
        DecodedFrame::new(index, 8, 8, Duration::from_millis(100), pixels).expect("frame")
    }

    #[test]
    fn compression_ids_round_trip_and_reject_typos() {
        for compression in [Compression::None, Compression::Zstd, Compression::Lz4] {
            assert_eq!(
                Compression::parse(compression.as_str()),
                Some(compression),
                "config spelling must round-trip"
            );
        }
        assert_eq!(Compression::parse("brotli"), None);
        assert_eq!(Compression::parse("ZSTD"), Some(Compression::Zstd));
    }

    #[test]
    fn the_cap_is_hard_and_the_overflow_signal_is_refusal() {
        // Each frame is 1024 bytes of incompressible noise, so a 1500-byte cap can
        // hold one and refuses the rest. The cap must never be exceeded, and the
        // refusal must be counted rather than silent.
        let mut cache = FrameCache::new(1500, Compression::Zstd);
        let mut refused = 0;
        for index in 0..8 {
            if !cache.insert(&noisy(index)).expect("insert") {
                refused += 1;
            }
            assert!(
                cache.stored_bytes() <= cache.cap_bytes(),
                "the cap is a cap: {} > {}",
                cache.stored_bytes(),
                cache.cap_bytes()
            );
        }
        assert_eq!(
            refused,
            cache.refusals(),
            "refusals are counted, not silent"
        );
        assert!(refused > 0, "eight noisy frames cannot fit in 1500 bytes");
        // What did fit is still readable, and byte-identical.
        let first = cache.get(0).expect("get").expect("cached");
        assert_eq!(first.index(), 0);
        assert_eq!(first.pixels().len(), 8 * 8 * 4);
        assert_eq!(
            first,
            noisy(0),
            "a cached frame must survive the round trip"
        );
    }

    #[test]
    fn a_zero_cap_caches_nothing_and_streams_everything() {
        let mut cache = FrameCache::new(0, Compression::Zstd);
        assert!(
            !cache.insert(&frame(0, 1)).expect("insert"),
            "a zero cap must refuse everything"
        );
        assert!(cache.is_empty());
        assert_eq!(cache.get(0).expect("get"), None);
        assert_eq!(cache.ratio(), 1.0);
    }

    #[test]
    fn frames_come_back_exactly_as_they_went_in() {
        // Compression must be lossless for every mode, including the ones that
        // make a byte-level mistake easy (a prepended size, a wrong level).
        for compression in [Compression::None, Compression::Zstd, Compression::Lz4] {
            let mut cache = FrameCache::new(1 << 20, compression);
            let original = DecodedFrame::new(
                3,
                4,
                4,
                Duration::from_millis(40),
                (0..64).collect::<Vec<u8>>(),
            )
            .expect("frame");
            assert!(cache.insert(&original).expect("insert"));
            let read_back = cache.get(3).expect("get").expect("cached");
            assert_eq!(read_back, original, "{compression:?} must be lossless");
        }
    }

    #[test]
    fn flat_colour_frames_actually_compress() {
        // The point of the compression setting, asserted on content that any codec
        // must handle: a cache that stored 4 MB of solid colour as 4 MB would be
        // compression in name only.
        let mut cache = FrameCache::new(1 << 20, Compression::Zstd);
        let flat =
            DecodedFrame::new(0, 512, 512, Duration::ZERO, vec![9; 512 * 512 * 4]).expect("frame");
        assert!(cache.insert(&flat).expect("insert"));
        assert!(
            cache.ratio() < 0.5,
            "flat colour must compress well; ratio was {}",
            cache.ratio()
        );
        assert_eq!(cache.raw_bytes(), 512 * 512 * 4);
    }

    #[test]
    fn a_missing_index_is_none_not_an_error() {
        let cache = FrameCache::new(1 << 20, Compression::Zstd);
        assert_eq!(cache.get(0).expect("get"), None);
        assert!(!cache.contains(0));
    }

    #[test]
    fn corrupt_cached_bytes_are_reported_as_a_cache_error() {
        // A tampered cache entry must be an error naming the codec, not a panic
        // and not silently wrong pixels.
        let mut cache = FrameCache::new(1 << 20, Compression::Zstd);
        assert!(cache.insert(&frame(0, 3)).expect("insert"));
        if let Some(entry) = cache.frames.front_mut() {
            entry.data = vec![0xff; 8];
        }
        let error = cache.get(0).expect_err("corrupt data must not decode");
        assert!(matches!(error, MediaError::Cache { .. }), "{error}");
        assert!(error.to_string().contains("zstd"), "{error}");
    }

    #[test]
    fn lz4_size_prefix_cannot_request_an_unbounded_allocation() {
        let mut cache = FrameCache::new(1 << 20, Compression::Lz4);
        assert!(cache.insert(&frame(0, 3)).expect("insert"));
        if let Some(entry) = cache.frames.front_mut() {
            entry.data[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        }
        let error = cache
            .get(0)
            .expect_err("a corrupt size prefix must not allocate from the cache");
        assert!(matches!(error, MediaError::Cache { .. }), "{error}");
        assert!(error.to_string().contains("size"), "{error}");
    }

    #[test]
    fn clearing_frees_the_bytes_and_keeps_the_cap() {
        let mut cache = FrameCache::new(1 << 20, Compression::Lz4);
        assert!(cache.insert(&frame(0, 1)).expect("insert"));
        assert!(cache.stored_bytes() > 0);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.stored_bytes(), 0);
        assert_eq!(cache.cap_bytes(), 1 << 20);
    }
}
