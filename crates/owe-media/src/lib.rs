//! Decoding media into pixels.
//!
//! P1 decodes still images. Animated images and video arrive in P4 behind the
//! same shape (a decoder that hands pixels to the renderer and knows nothing
//! about Wayland), so nothing here mentions surfaces, buffers, or GPUs — that
//! separation is what keeps this crate testable without a compositor.

use std::path::Path;

use thiserror::Error;

use owe_core::ContentKind;

/// Decoded RGBA8 pixels plus the size they belong to.
///
/// Validation on construction is deliberate: a mismatched buffer is the kind of
/// bug that shows up as a garbled screen rather than an error, so it is refused
/// at the boundary instead.
#[derive(Clone, PartialEq, Eq)]
pub struct DecodedImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl std::fmt::Debug for DecodedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.pixels.len())
            .finish()
    }
}

impl DecodedImage {
    /// Build from raw RGBA8 pixels, checking the length matches the size.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, MediaError> {
        let expected = width as usize * height as usize * 4;
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
            width,
            height,
            pixels,
        })
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// RGBA8 pixels, row-major from the top-left.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Pixel size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Total pixel count.
    pub fn pixel_count(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Approximate resident size of the pixel buffer.
    pub fn byte_len(&self) -> usize {
        self.pixels.len()
    }
}

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

    /// The content kind is not decodable by this build yet.
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
}

/// Default decode budget: ~80 megapixels, which covers 8K comfortably while
/// keeping a hostile or accidental huge file from eating all the RAM on an
/// 8 GB reference machine.
pub const DEFAULT_MAX_PIXELS: u64 = 80_000_000;

/// Decode a still image from disk with the default budget.
pub fn decode_file(path: &Path) -> Result<DecodedImage, MediaError> {
    decode_file_with_budget(path, DEFAULT_MAX_PIXELS)
}

/// Decode a still image, enforcing a pixel budget.
///
/// The budget is checked **before** decoding where possible: a 20000×20000 PNG
/// that would allocate 1.6 GB should be refused, not attempted and then killed by
/// the OOM killer.
pub fn decode_file_with_budget(path: &Path, max_pixels: u64) -> Result<DecodedImage, MediaError> {
    let reader = image::ImageReader::open(path)
        .map_err(|source| MediaError::Io {
            path: path.display().to_string(),
            source,
        })?
        .with_guessed_format()
        .map_err(|source| MediaError::Io {
            path: path.display().to_string(),
            source,
        })?;

    // Decide from the header when the format allows it, so oversized files are
    // rejected before the big allocation. A header we cannot read is not an error
    // here — the decode below reports the real problem with a better message.
    if let Ok((header_width, header_height)) = reader.into_dimensions() {
        let pixels = u64::from(header_width) * u64::from(header_height);
        if pixels > max_pixels {
            return Err(MediaError::TooLarge {
                path: path.display().to_string(),
                pixels: pixels.div_ceil(1_000_000),
                limit: max_pixels / 1_000_000,
            });
        }
    }

    // The reader was consumed by `into_dimensions`; reopen for the decode itself.
    let decoded = image::ImageReader::open(path)
        .map_err(|source| MediaError::Io {
            path: path.display().to_string(),
            source,
        })?
        .with_guessed_format()
        .map_err(|source| MediaError::Io {
            path: path.display().to_string(),
            source,
        })?
        .decode()
        .map_err(|error| MediaError::Decode {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;

    let rgba = decoded.to_rgba8();
    DecodedImage::new(rgba.width(), rgba.height(), rgba.into_raw())
}

/// Longest edge of a generated thumbnail, in pixels.
///
/// 512 is the default in `library.thumbnail_size`: big enough to look sharp in a
/// GUI grid cell on a HiDPI panel, small enough that a PNG encode stays in the
/// tens of milliseconds and the cache stays a few tens of kilobytes per file.
pub const THUMBNAIL_MAX_EDGE: u32 = 512;

/// Decode `path` and produce a PNG thumbnail whose longest edge is `max_edge`.
///
/// Kept here (rather than in the library scanner) because it is a media concern:
/// the scanner knows about paths and stamps, this knows about pixels. The scale
/// is computed from the source size so the aspect ratio is preserved, and the
/// image is never **up**scaled — a 64×64 icon should not be blown up to 512.
pub fn thumbnail_png(path: &Path, max_edge: u32) -> Result<Vec<u8>, MediaError> {
    let decoded = decode_file(path)?;
    thumbnail_png_from(&decoded, max_edge, path)
}

/// Encode a PNG thumbnail from already-decoded pixels.
///
/// Split out so a caller that has already paid for the decode (the daemon
/// generating a thumbnail right after rendering a wallpaper) does not pay twice.
pub fn thumbnail_png_from(
    decoded: &DecodedImage,
    max_edge: u32,
    path: &Path,
) -> Result<Vec<u8>, MediaError> {
    if max_edge == 0 {
        return Err(MediaError::Encode {
            path: path.display().to_string(),
            detail: "thumbnail size 0".to_string(),
        });
    }

    let (width, height) = thumbnail_size(decoded.size(), max_edge);
    let buffer =
        image::RgbaImage::from_raw(decoded.width(), decoded.height(), decoded.pixels().to_vec())
            .ok_or_else(|| MediaError::Encode {
                path: path.display().to_string(),
                detail: format!(
                    "decoded buffer is not {}x{} RGBA8",
                    decoded.width(),
                    decoded.height()
                ),
            })?;

    // Triangle (bilinear) is the right trade here: it is what makes a 4K photo
    // readable at 512 px, and unlike Lanczos it does not cost tens of
    // milliseconds per file on the reference laptop.
    let scaled = if (width, height) == decoded.size() {
        buffer
    } else {
        image::imageops::resize(
            &buffer,
            width,
            height,
            image::imageops::FilterType::Triangle,
        )
    };

    let mut png = Vec::new();
    scaled
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| MediaError::Encode {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
    Ok(png)
}

/// The size a thumbnail of `source` will have, capped to `max_edge` on the
/// longest side and never magnified.
///
/// Pure and public because it is the part worth asserting on: a 4000×2000 photo
/// becomes 512×256, and a 100×50 image stays 100×50.
pub fn thumbnail_size(source: (u32, u32), max_edge: u32) -> (u32, u32) {
    let (width, height) = source;
    if width == 0 || height == 0 || max_edge == 0 {
        return (width.max(1), height.max(1));
    }
    let longest = width.max(height);
    if longest <= max_edge {
        return (width, height);
    }
    let scale = f64::from(max_edge) / f64::from(longest);
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

/// Check that a content kind is something this build can actually render, with a
/// clear error naming the phase that will land it.
pub fn ensure_supported(kind: ContentKind) -> Result<(), MediaError> {
    match kind {
        ContentKind::StaticImage => Ok(()),
        other => Err(MediaError::UnsupportedKind {
            kind: other.as_str(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a small test image so decode tests do not depend on fixtures on disk.
    fn write_png(
        dir: &Path,
        name: &str,
        width: u32,
        height: u32,
        rgba: [u8; 4],
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut buffer = image::RgbaImage::new(width, height);
        for pixel in buffer.pixels_mut() {
            *pixel = image::Rgba(rgba);
        }
        buffer.save(&path).expect("write test image");
        path
    }

    fn write_jpeg(dir: &Path, name: &str, width: u32, height: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut buffer = image::RgbImage::new(width, height);
        for pixel in buffer.pixels_mut() {
            *pixel = image::Rgb([200, 30, 90]);
        }
        buffer.save(&path).expect("write test jpeg");
        path
    }

    #[test]
    fn decodes_a_png_with_exact_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(dir.path(), "solid.png", 4, 3, [10, 20, 30, 255]);

        let image = decode_file(&path).expect("decode");
        assert_eq!(image.size(), (4, 3));
        assert_eq!(image.pixel_count(), 12);
        assert_eq!(image.byte_len(), 48);
        for pixel in image.pixels().chunks_exact(4) {
            assert_eq!(pixel, &[10, 20, 30, 255]);
        }
    }

    #[test]
    fn decodes_a_jpeg_and_converts_to_rgba() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_jpeg(dir.path(), "photo.jpg", 8, 8);

        let image = decode_file(&path).expect("decode");
        assert_eq!(image.size(), (8, 8));
        assert_eq!(image.byte_len(), 8 * 8 * 4, "always RGBA8 out");
    }

    #[test]
    fn format_is_sniffed_not_trusted_from_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        // A PNG that claims to be a jpg: content sniffing must win.
        let real = write_png(dir.path(), "real.png", 2, 2, [1, 2, 3, 255]);
        let lying = dir.path().join("actually-a-png.jpg");
        std::fs::copy(&real, &lying).unwrap();

        let image = decode_file(&lying).expect("content sniffing should decode this");
        assert_eq!(image.size(), (2, 2));
    }

    #[test]
    fn missing_file_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let error = decode_file(&dir.path().join("absent.png")).unwrap_err();
        assert!(matches!(error, MediaError::Io { .. }), "{error}");
        assert!(error.to_string().contains("absent.png"), "{error}");
    }

    #[test]
    fn non_image_bytes_report_a_decode_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-an-image.png");
        std::fs::write(&path, b"this is plain text, not a png").unwrap();

        let error = decode_file(&path).unwrap_err();
        assert!(matches!(error, MediaError::Decode { .. }), "{error}");
    }

    #[test]
    fn oversized_images_are_refused_before_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(dir.path(), "big.png", 100, 100, [0, 0, 0, 255]);

        // Budget of 5000 pixels against a 10000-pixel image.
        let error = decode_file_with_budget(&path, 5_000).unwrap_err();
        match error {
            MediaError::TooLarge { pixels, limit, .. } => {
                assert_eq!(
                    pixels, 1,
                    "10000 px is reported as 1 megapixel (rounded up)"
                );
                assert_eq!(limit, 0, "a 5000 px budget is 0 whole megapixels");
            }
            other => panic!("unexpected: {other}"),
        }
        assert!(error.to_string().contains("decode budget"), "{error}");
    }

    #[test]
    fn an_image_at_the_budget_boundary_still_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(dir.path(), "exact.png", 10, 10, [9, 9, 9, 255]);
        assert!(decode_file_with_budget(&path, 100).is_ok());
    }

    #[test]
    fn buffer_length_mismatch_is_rejected_at_construction() {
        let error = DecodedImage::new(2, 2, vec![0; 15]).unwrap_err();
        match error {
            MediaError::BufferLength {
                width,
                height,
                expected,
                actual,
            } => {
                assert_eq!((width, height), (2, 2));
                assert_eq!((expected, actual), (16, 15));
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn zero_sized_images_are_rejected() {
        assert!(matches!(
            DecodedImage::new(0, 4, vec![]).unwrap_err(),
            MediaError::EmptyImage { .. }
        ));
    }

    #[test]
    fn thumbnail_sizes_preserve_aspect_and_never_magnify() {
        // The pure part, so the rules are asserted without any pixels.
        assert_eq!(thumbnail_size((4000, 2000), 512), (512, 256));
        assert_eq!(thumbnail_size((2000, 4000), 512), (256, 512));
        assert_eq!(thumbnail_size((512, 512), 512), (512, 512));
        assert_eq!(
            thumbnail_size((100, 50), 512),
            (100, 50),
            "a small image must not be blown up"
        );
        // Rounding must never produce a zero dimension.
        let (width, height) = thumbnail_size((10000, 3), 512);
        assert_eq!((width, height), (512, 1));
        assert_eq!(thumbnail_size((0, 0), 512), (1, 1));
    }

    #[test]
    fn a_thumbnail_is_a_png_matching_the_source_colour() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(dir.path(), "wall.png", 64, 32, [7, 200, 90, 255]);

        let png = thumbnail_png(&path, 16).expect("thumbnail");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "must be a real PNG");

        let decoded = image::load_from_memory(&png)
            .expect("decode thumbnail")
            .to_rgba8();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (16, 8),
            "the aspect ratio survives the downscale"
        );
        // Triangle filtering of a solid image is exact, so the colour must match.
        assert_eq!(decoded.get_pixel(8, 4).0, [7, 200, 90, 255]);
    }

    #[test]
    fn a_thumbnail_of_a_small_image_keeps_its_own_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(dir.path(), "icon.png", 24, 24, [1, 2, 3, 255]);

        let png = thumbnail_png(&path, 512).expect("thumbnail");
        let decoded = image::load_from_memory(&png).expect("decode").to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (24, 24));
    }

    #[test]
    fn thumbnailing_a_broken_file_reports_the_decode_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.png");
        std::fs::write(&path, b"not an image at all").unwrap();

        let error = thumbnail_png(&path, 256).unwrap_err();
        assert!(matches!(error, MediaError::Decode { .. }), "{error}");
    }

    #[test]
    fn a_zero_thumbnail_size_is_an_encode_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_png(dir.path(), "wall.png", 8, 8, [0, 0, 0, 255]);
        let decoded = decode_file(&path).unwrap();

        let error = thumbnail_png_from(&decoded, 0, &path).unwrap_err();
        assert!(matches!(error, MediaError::Encode { .. }), "{error}");
    }

    #[test]
    fn only_static_images_are_supported_in_this_build() {
        assert!(ensure_supported(ContentKind::StaticImage).is_ok());

        for kind in [
            ContentKind::AnimatedImage,
            ContentKind::Video,
            ContentKind::Shader,
        ] {
            let error = ensure_supported(kind).unwrap_err();
            assert!(
                matches!(error, MediaError::UnsupportedKind { .. }),
                "{error}"
            );
            assert!(
                error.to_string().contains(kind.as_str()),
                "the error must name the kind: {error}"
            );
        }
    }
}
