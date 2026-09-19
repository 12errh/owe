//! Golden images: render off-screen, compare against a reviewed reference.
//!
//! Pure logic on purpose: every function here is testable without a GPU, and the
//! GPU only ever produces the pixels that get compared. See the crate docs for
//! the freeze process (implement → record → human review → commit → strict).

use std::path::{Path, PathBuf};

use crate::gpu::RenderError;

/// A tightly packed RGBA8 image — the unit of golden comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldenImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl GoldenImage {
    /// Build from raw RGBA8 pixels, validating the buffer length.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, RenderError> {
        let expected = width as usize * height as usize * 4;
        if pixels.len() != expected {
            return Err(RenderError::BufferLength {
                width,
                height,
                expected,
                actual: pixels.len(),
            });
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    /// Load an image from a PNG file.
    pub fn from_png(path: &Path) -> Result<Self, RenderError> {
        let image = image::open(path)
            .map_err(|error| RenderError::Image(format!("{}: {error}", path.display())))?;
        let rgba = image.to_rgba8();
        Ok(Self {
            width: rgba.width(),
            height: rgba.height(),
            pixels: rgba.into_raw(),
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

    /// Raw RGBA8 pixels, row-major from the top-left.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Write this image to a PNG, creating parent directories as needed.
    pub fn save_png(&self, path: &Path) -> Result<(), RenderError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| RenderError::Image(format!("{}: {error}", parent.display())))?;
        }
        let buffer = image::RgbaImage::from_raw(self.width, self.height, self.pixels.clone())
            .ok_or_else(|| RenderError::Image("cannot rebuild image buffer".to_string()))?;
        buffer
            .save(path)
            .map_err(|error| RenderError::Image(format!("{}: {error}", path.display())))
    }

    /// Compare against another image, allowing a per-channel tolerance.
    ///
    /// Tolerance exists because GPU rounding differs by driver; 0 means exact.
    pub fn diff(&self, other: &GoldenImage, tolerance: u8) -> Result<ImageDiff, RenderError> {
        if self.width != other.width || self.height != other.height {
            return Err(RenderError::SizeMismatch {
                left: (self.width, self.height),
                right: (other.width, other.height),
            });
        }

        let mut report = ImageDiff {
            width: self.width,
            height: self.height,
            total_pixels: u64::from(self.width) * u64::from(self.height),
            differing_pixels: 0,
            max_channel_delta: 0,
            tolerance,
            first_mismatch: None,
        };

        for (index, (left, right)) in self
            .pixels
            .chunks_exact(4)
            .zip(other.pixels.chunks_exact(4))
            .enumerate()
        {
            let mut delta = 0_u8;
            for channel in 0..4 {
                delta = delta.max(left[channel].abs_diff(right[channel]));
            }
            report.max_channel_delta = report.max_channel_delta.max(delta);
            if delta > tolerance {
                report.differing_pixels += 1;
                if report.first_mismatch.is_none() {
                    let pixel = index as u32;
                    report.first_mismatch = Some((pixel % self.width, pixel / self.width));
                }
            }
        }

        Ok(report)
    }
}

/// The result of comparing two images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageDiff {
    /// Compared width.
    pub width: u32,
    /// Compared height.
    pub height: u32,
    /// Total pixels compared.
    pub total_pixels: u64,
    /// Pixels differing by more than `tolerance`.
    pub differing_pixels: u64,
    /// Largest per-channel difference seen.
    pub max_channel_delta: u8,
    /// Tolerance this comparison allowed.
    pub tolerance: u8,
    /// First differing pixel as `(x, y)`.
    pub first_mismatch: Option<(u32, u32)>,
}

impl ImageDiff {
    /// Whether the images matched within tolerance.
    pub fn is_match(&self) -> bool {
        self.differing_pixels == 0
    }

    /// One-line summary for test output and CI logs.
    pub fn summary(&self) -> String {
        let percent = if self.total_pixels == 0 {
            0.0
        } else {
            (self.differing_pixels as f64 / self.total_pixels as f64) * 100.0
        };
        match self.first_mismatch {
            Some((x, y)) => format!(
                "{}/{} px differ ({percent:.2}%), max channel delta {}, first at ({x}, {y}), tolerance {}",
                self.differing_pixels, self.total_pixels, self.max_channel_delta, self.tolerance
            ),
            None => format!(
                "identical ({} px, max channel delta {}, tolerance {})",
                self.total_pixels, self.max_channel_delta, self.tolerance
            ),
        }
    }
}

/// How a missing reference is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoldenMode {
    /// Record the actual image when the reference is absent (authoring only).
    RecordIfMissing,
    /// Fail when the reference is absent (CI, and every normal test run).
    Strict,
}

/// What [`verify_or_record`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoldenOutcome {
    /// A new reference was written and must be reviewed and committed by a human.
    Recorded(PathBuf),
    /// The image matched the committed reference.
    Matched(ImageDiff),
}

/// Verify `actual` against the reference at `reference`, or record it.
///
/// `Strict` mode never writes: CI must fail rather than bake in whatever the
/// current code happens to produce.
pub fn verify_or_record(
    reference: &Path,
    actual: &GoldenImage,
    tolerance: u8,
    mode: GoldenMode,
) -> Result<GoldenOutcome, RenderError> {
    if !reference.exists() {
        return match mode {
            GoldenMode::RecordIfMissing => {
                actual.save_png(reference)?;
                Ok(GoldenOutcome::Recorded(reference.to_path_buf()))
            }
            GoldenMode::Strict => Err(RenderError::GoldenMissing(reference.to_path_buf())),
        };
    }

    let expected = GoldenImage::from_png(reference)?;
    let diff = actual.diff(&expected, tolerance)?;
    if diff.is_match() {
        Ok(GoldenOutcome::Matched(diff))
    } else {
        Err(RenderError::GoldenMismatch {
            summary: diff.summary(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> GoldenImage {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            pixels.extend_from_slice(&rgba);
        }
        GoldenImage::new(width, height, pixels).expect("valid buffer")
    }

    #[test]
    fn rejects_buffers_that_do_not_match_their_size() {
        let err = GoldenImage::new(2, 2, vec![0; 15]).unwrap_err();
        match err {
            RenderError::BufferLength {
                width,
                height,
                expected,
                actual,
            } => {
                assert_eq!((width, height), (2, 2));
                assert_eq!(expected, 16);
                assert_eq!(actual, 15);
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn identical_images_match_with_zero_delta() {
        let image = solid(3, 2, [10, 20, 30, 255]);
        let diff = image.diff(&image, 0).unwrap();
        assert!(diff.is_match());
        assert_eq!(diff.max_channel_delta, 0);
        assert_eq!(diff.differing_pixels, 0);
        assert!(diff.summary().contains("identical"), "{}", diff.summary());
    }

    #[test]
    fn tolerance_absorbs_driver_rounding() {
        let expected = solid(2, 2, [100, 100, 100, 255]);
        let actual = solid(2, 2, [103, 100, 100, 255]);

        // Within tolerance: a match.
        assert!(actual.diff(&expected, 3).unwrap().is_match());

        // Beyond tolerance: a mismatch, with the first offending pixel located.
        let diff = actual.diff(&expected, 2).unwrap();
        assert!(!diff.is_match());
        assert_eq!(diff.differing_pixels, 4);
        assert_eq!(diff.max_channel_delta, 3);
        assert_eq!(diff.first_mismatch, Some((0, 0)));
        assert!(
            diff.summary().contains("first at (0, 0)"),
            "{}",
            diff.summary()
        );
    }

    #[test]
    fn reports_the_first_differing_pixel_coordinate() {
        let expected = solid(4, 3, [0, 0, 0, 255]);
        let mut pixels = expected.pixels().to_vec();
        // Pixel index 6 = (x=2, y=1).
        pixels[6 * 4 + 1] = 255;
        let actual = GoldenImage::new(4, 3, pixels).unwrap();

        let diff = actual.diff(&expected, 0).unwrap();
        assert_eq!(diff.first_mismatch, Some((2, 1)));
        assert_eq!(diff.differing_pixels, 1);
    }

    #[test]
    fn size_mismatch_is_an_error_not_a_diff() {
        let left = solid(2, 2, [0, 0, 0, 255]);
        let right = solid(2, 3, [0, 0, 0, 255]);
        let err = left.diff(&right, 0).unwrap_err();
        assert!(matches!(err, RenderError::SizeMismatch { .. }), "{err}");
    }

    #[test]
    fn png_round_trip_preserves_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/reference.png");
        let image = solid(3, 2, [7, 8, 9, 255]);

        image.save_png(&path).expect("save");
        let loaded = GoldenImage::from_png(&path).expect("load");
        assert_eq!(loaded, image);
    }

    #[test]
    fn strict_mode_refuses_to_record_a_missing_reference() {
        let dir = tempfile::tempdir().unwrap();
        let reference = dir.path().join("absent.png");
        let image = solid(2, 2, [1, 2, 3, 255]);

        let err = verify_or_record(&reference, &image, 0, GoldenMode::Strict).unwrap_err();
        assert!(matches!(err, RenderError::GoldenMissing(_)), "{err}");
        assert!(!reference.exists(), "strict mode must not write files");
    }

    #[test]
    fn record_then_verify_is_the_freeze_process() {
        let dir = tempfile::tempdir().unwrap();
        let reference = dir.path().join("frozen.png");
        let image = solid(2, 2, [200, 100, 50, 255]);

        let outcome = verify_or_record(&reference, &image, 0, GoldenMode::RecordIfMissing).unwrap();
        assert!(matches!(outcome, GoldenOutcome::Recorded(_)), "{outcome:?}");
        assert!(reference.exists());

        // Second run: identical content matches strictly.
        let outcome = verify_or_record(&reference, &image, 0, GoldenMode::Strict).unwrap();
        assert!(matches!(outcome, GoldenOutcome::Matched(_)), "{outcome:?}");

        // A drift beyond tolerance fails, and says where.
        let drifted = solid(2, 2, [255, 100, 50, 255]);
        let err = verify_or_record(&reference, &drifted, 0, GoldenMode::Strict).unwrap_err();
        assert!(matches!(err, RenderError::GoldenMismatch { .. }), "{err}");
        assert!(err.to_string().contains("differ"), "{err}");
    }
}
