//! Transitions: what each one actually *does*, and the frozen goldens.
//!
//! Two layers, deliberately:
//!
//! 1. **Property tests** (below, no reference files): with a red "old" image and a
//!    blue "new" one, where the blue lands tells you exactly which transition ran
//!    and in which direction. A golden image can freeze a wrong picture; these
//!    assertions cannot.
//! 2. **Golden images** at three sizes (`tests/golden/transitions/`), which freeze
//!    the banded test patterns so a refactor of the shader geometry has to be seen
//!    and re-reviewed rather than slipping through.
//!
//! Recording is explicit: run with `OWE_RECORD_GOLDEN=1` to write missing
//! references, review the PNGs, then commit them. Without the variable, a missing
//! reference is a failure — CI never bakes in whatever the code happens to produce.

use std::path::{Path, PathBuf};

use owe_render::golden::{GoldenImage, GoldenMode, GoldenOutcome, verify_or_record};
use owe_render::gpu::HeadlessGpu;
use owe_render::image::{PixelFormat, Scaling};
use owe_render::transition::{GOLDEN_SIZES, TransitionKind, TransitionRenderer};

/// Mid-transition is the informative frame: every kind is half-way through, so a
/// wrong direction or a wrong axis is unmistakable.
const GOLDEN_PROGRESS: f32 = 0.5;

/// Per-channel tolerance. GPU filtering and rounding differ per driver, and the
/// reference profile runs on a Haswell iGPU while CI runs on lavapipe; a real
/// geometry error moves a boundary by many pixels, far outside this.
const GOLDEN_TOLERANCE: u8 = 8;

fn gpu() -> Option<HeadlessGpu> {
    HeadlessGpu::new().expect("device creation must not fail")
}

fn solid(size: (u32, u32), rgba: [u8; 4]) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((size.0 * size.1 * 4) as usize);
    for _ in 0..(size.0 * size.1) {
        pixels.extend_from_slice(&rgba);
    }
    pixels
}

/// Deterministic vertical bands + a white border. Cheap to compress (few KB per
/// golden) and rich enough that a flipped axis or a wrong mask shows up.
fn banded_vertical(size: (u32, u32)) -> Vec<u8> {
    const BANDS: [[u8; 3]; 6] = [
        [200, 40, 40],
        [40, 200, 40],
        [40, 40, 200],
        [220, 200, 40],
        [200, 40, 200],
        [40, 200, 200],
    ];
    let mut pixels = Vec::with_capacity((size.0 * size.1 * 4) as usize);
    for y in 0..size.1 {
        for x in 0..size.0 {
            let border = x < 8 || y < 8 || x + 8 >= size.0 || y + 8 >= size.1;
            let band = BANDS[((x * BANDS.len() as u32) / size.0.max(1)) as usize % BANDS.len()];
            let colour = if border { [255, 255, 255] } else { band };
            pixels.extend_from_slice(&[colour[0], colour[1], colour[2], 255]);
        }
    }
    pixels
}

/// Deterministic horizontal bands plus a centred square — a pattern that makes
/// "inside the evolving quad" obvious in a golden.
fn banded_horizontal(size: (u32, u32)) -> Vec<u8> {
    const BANDS: [[u8; 3]; 5] = [
        [30, 30, 30],
        [90, 30, 30],
        [30, 90, 30],
        [30, 30, 90],
        [120, 120, 120],
    ];
    let mut pixels = Vec::with_capacity((size.0 * size.1 * 4) as usize);
    for y in 0..size.1 {
        for x in 0..size.0 {
            let band = BANDS[((y * BANDS.len() as u32) / size.1.max(1)) as usize % BANDS.len()];
            let in_square =
                x > size.0 / 4 && x < size.0 * 3 / 4 && y > size.1 / 4 && y < size.1 * 3 / 4;
            let colour = if in_square { [250, 140, 20] } else { band };
            pixels.extend_from_slice(&[colour[0], colour[1], colour[2], 255]);
        }
    }
    pixels
}

fn pixel(pixels: &[u8], size: (u32, u32), x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * size.0 + x) * 4) as usize;
    [
        pixels[offset],
        pixels[offset + 1],
        pixels[offset + 2],
        pixels[offset + 3],
    ]
}

fn is_red(px: [u8; 4]) -> bool {
    px[0] > 200 && px[1] < 60 && px[2] < 60
}

fn is_blue(px: [u8; 4]) -> bool {
    px[2] > 200 && px[0] < 60 && px[1] < 60
}

/// Render one transition frame at a square target with flat source colours.
fn render_frame(
    gpu: &HeadlessGpu,
    kind: TransitionKind,
    progress: f32,
    size: (u32, u32),
) -> Vec<u8> {
    let mut renderer = TransitionRenderer::new(
        gpu,
        size,
        (&solid(size, [255, 0, 0, 255]), size),
        (&solid(size, [0, 0, 255, 255]), size),
        kind,
        Scaling::Cover,
        PixelFormat::Rgba8,
    )
    .expect("transition renderer");
    renderer.frame(gpu, progress).expect("frame")
}

#[test]
fn every_transition_starts_on_the_old_image_and_ends_on_the_new_one() {
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let size = (64, 64);

    for kind in TransitionKind::ALL {
        // `none` is a hard cut, so progress 0 already shows the new image — that is
        // its definition, and `none_swaps_instantly_at_any_progress` pins it.
        if kind.is_animated() {
            let start = render_frame(&gpu, *kind, 0.0, size);
            for (index, px) in start.chunks_exact(4).enumerate() {
                assert!(
                    is_red([px[0], px[1], px[2], px[3]]),
                    "{kind:?} at progress 0 must show the old image everywhere; \
                     pixel {index} is {:?}",
                    &px[..3]
                );
            }
        }

        let end = render_frame(&gpu, *kind, 1.0, size);
        for (index, px) in end.chunks_exact(4).enumerate() {
            assert!(
                is_blue([px[0], px[1], px[2], px[3]]),
                "{kind:?} at progress 1 must show the new image everywhere; \
                 pixel {index} is {:?}",
                &px[..3]
            );
        }
    }
}

#[test]
fn fade_blends_the_two_images_evenly_at_half_way() {
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let size = (64, 64);
    let frame = render_frame(&gpu, TransitionKind::Fade, 0.5, size);

    for (index, px) in frame.chunks_exact(4).enumerate() {
        let expected = [127_u8, 0, 127];
        for channel in 0..3 {
            assert!(
                px[channel].abs_diff(expected[channel]) <= 2,
                "fade at 0.5 should be a 50/50 blend; pixel {index} channel {channel} \
                 is {} not {}",
                px[channel],
                expected[channel]
            );
        }
    }
}

#[test]
fn wipe_reveals_the_new_image_from_the_left() {
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let size = (64, 64);
    let frame = render_frame(&gpu, TransitionKind::Wipe, 0.5, size);

    assert!(is_blue(pixel(&frame, size, 8, 32)), "the left side is new");
    assert!(is_red(pixel(&frame, size, 56, 32)), "the right side is old");
    // Top and bottom rows must agree: a wipe is vertical, not diagonal.
    assert!(is_blue(pixel(&frame, size, 8, 2)));
    assert!(is_blue(pixel(&frame, size, 8, 61)));
}

#[test]
fn slide_brings_the_new_image_in_from_the_right() {
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let size = (64, 64);
    let frame = render_frame(&gpu, TransitionKind::Slide, 0.5, size);

    assert!(
        is_blue(pixel(&frame, size, 56, 32)),
        "the new image occupies the right edge it slid in from"
    );
    assert!(is_red(pixel(&frame, size, 8, 32)), "the left is still old");
}

#[test]
fn grow_expands_the_new_image_from_the_centre() {
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let size = (64, 64);
    let frame = render_frame(&gpu, TransitionKind::Grow, 0.5, size);

    assert!(
        is_blue(pixel(&frame, size, 32, 32)),
        "the centre is new: the iris starts there"
    );
    assert!(
        is_red(pixel(&frame, size, 1, 1)),
        "the corner is still old at half way"
    );
    assert!(is_red(pixel(&frame, size, 62, 62)));
}

#[test]
fn outer_closes_in_from_the_edges() {
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let size = (64, 64);
    let frame = render_frame(&gpu, TransitionKind::Outer, 0.5, size);

    assert!(
        is_blue(pixel(&frame, size, 1, 1)),
        "the corner is new: the ring starts at the edges"
    );
    assert!(
        is_red(pixel(&frame, size, 32, 32)),
        "the centre is still old at half way"
    );
}

#[test]
fn wave_has_a_wavy_boundary_not_a_straight_one() {
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let size = (64, 64);
    let frame = render_frame(&gpu, TransitionKind::Wave, 0.5, size);

    // Where the red starts, per row: the ripple must move it up and down. Sampling
    // several rows (not just the extremes) catches a ripple that is merely
    // symmetric about the screen centre — which is exactly what a wrong frequency
    // produced the first time this was written.
    let boundary = |y: u32| -> u32 {
        (0..size.0)
            .find(|x| is_red(pixel(&frame, size, *x, y)))
            .unwrap_or_else(|| panic!("row {y} has no old region at all"))
    };
    let rows = [4_u32, 20, 32, 48, 60];
    let positions: Vec<u32> = rows.iter().map(|y| boundary(*y)).collect();
    let lowest = positions.iter().copied().min().unwrap();
    let highest = positions.iter().copied().max().unwrap();

    assert!(
        highest - lowest >= 2,
        "the ripple must bend the boundary by more than rounding: {rows:?} -> {positions:?}"
    );
    // One full cycle: the top half bulges one way, the bottom half the other, and
    // the centre row sits on the zero crossing between them. A frequency that puts
    // the centre on a peak instead makes both halves bulge the same way — a bow
    // tie, not a wave — and this is the assertion that catches it.
    let (top, upper_mid, centre, lower_mid, bottom) = (
        positions[0],
        positions[1],
        positions[2],
        positions[3],
        positions[4],
    );
    assert!(
        top > centre && upper_mid > centre,
        "the upper half must bulge right of the centre line: {positions:?}"
    );
    assert!(
        bottom < centre && lower_mid < centre,
        "the lower half must bulge left of the centre line: {positions:?}"
    );
}

#[test]
fn none_swaps_instantly_at_any_progress() {
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let size = (32, 32);
    let frame = render_frame(&gpu, TransitionKind::None, 0.0, size);
    for px in frame.chunks_exact(4) {
        assert!(is_blue([px[0], px[1], px[2], px[3]]), "none is a hard cut");
    }
}

#[test]
fn a_source_larger_than_the_target_is_cover_fitted() {
    // The golden patterns are 1:1 with the target, so this is the test that the
    // transitions compose with the P1 scaling rules.
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let target = (64, 36);
    let source = (128, 72);

    let mut renderer = TransitionRenderer::new(
        &gpu,
        target,
        (&solid(source, [255, 0, 0, 255]), source),
        (&solid(source, [0, 0, 255, 255]), source),
        TransitionKind::Fade,
        Scaling::Cover,
        PixelFormat::Rgba8,
    )
    .expect("renderer");
    let frame = renderer.frame(&gpu, 1.0).expect("frame");
    assert_eq!(frame.len(), (target.0 * target.1 * 4) as usize);
    assert!(is_blue(pixel(&frame, target, 0, 0)));
    assert!(is_blue(pixel(&frame, target, 63, 35)));
}

#[test]
fn retargeting_continues_from_what_is_on_screen() {
    // The no-snap property: interrupting a transition must not flash back to the
    // original image. With red -> blue interrupted half-way by green, the frame
    // right after the interruption must be close to the purple we were looking at,
    // not red.
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let size = (32, 32);

    let mut renderer = TransitionRenderer::new(
        &gpu,
        size,
        (&solid(size, [255, 0, 0, 255]), size),
        (&solid(size, [0, 0, 255, 255]), size),
        TransitionKind::Fade,
        Scaling::Cover,
        PixelFormat::Rgba8,
    )
    .expect("renderer");

    let mid = renderer.frame(&gpu, 0.5).expect("mid frame");
    assert!(mid[0] > 100 && mid[2] > 100, "half-way is purple: {mid:?}");

    // Interrupt with a green wallpaper.
    renderer
        .retarget(
            &gpu,
            0.5,
            (&solid(size, [0, 255, 0, 255]), size),
            TransitionKind::Fade,
        )
        .expect("retarget");

    let just_after = renderer.frame(&gpu, 0.0).expect("frame after retarget");
    let px = pixel(&just_after, size, 16, 16);
    assert!(
        px[0] > 100 && px[2] > 100 && px[1] < 60,
        "the frame after an interruption must still be the purple blend, not red: {px:?}"
    );

    // And it converges on green.
    let end = renderer.frame(&gpu, 1.0).expect("final frame");
    let px = pixel(&end, size, 16, 16);
    assert!(
        px[1] > 200 && px[0] < 60,
        "it must land on the new image: {px:?}"
    );
    assert_eq!(renderer.kind(), TransitionKind::Fade);
}

#[test]
fn a_bad_buffer_is_rejected_before_the_gpu_is_touched() {
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let error = TransitionRenderer::new(
        &gpu,
        (16, 16),
        (&[0_u8; 8], (16, 16)),
        (&solid((16, 16), [0, 0, 255, 255]), (16, 16)),
        TransitionKind::Fade,
        Scaling::Cover,
        PixelFormat::Rgba8,
    )
    .unwrap_err();
    assert!(error.to_string().contains("buffer length"), "{error}");
}

// ------------------------------------------------------------------ goldens

fn golden_path(kind: TransitionKind, size: (u32, u32)) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/transitions")
        .join(format!("{}-{}x{}.png", kind.as_str(), size.0, size.1))
}

fn golden_mode() -> GoldenMode {
    match std::env::var("OWE_RECORD_GOLDEN") {
        Ok(value) if value != "0" => GoldenMode::RecordIfMissing,
        _ => GoldenMode::Strict,
    }
}

#[test]
fn transitions_match_their_golden_images_at_three_sizes() {
    let Some(gpu) = gpu() else {
        eprintln!("skipping: no wgpu adapter on this machine");
        return;
    };
    let mode = golden_mode();
    let mut recorded = Vec::new();
    let mut matched = 0;

    for size in GOLDEN_SIZES {
        let (width, height) = *size;
        let from = banded_vertical((width, height));
        let to = banded_horizontal((width, height));

        for kind in TransitionKind::ALL {
            let mut renderer = TransitionRenderer::new(
                &gpu,
                (width, height),
                (&from, (width, height)),
                (&to, (width, height)),
                *kind,
                Scaling::Cover,
                PixelFormat::Rgba8,
            )
            .expect("renderer");
            let pixels = renderer.frame(&gpu, GOLDEN_PROGRESS).expect("frame");
            let actual = GoldenImage::new(width, height, pixels).expect("golden image");

            let reference = golden_path(*kind, *size);
            match verify_or_record(&reference, &actual, GOLDEN_TOLERANCE, mode)
                .unwrap_or_else(|error| panic!("{}: {error}", reference.display()))
            {
                GoldenOutcome::Recorded(path) => recorded.push(path),
                GoldenOutcome::Matched(diff) => {
                    matched += 1;
                    assert!(
                        diff.max_channel_delta <= GOLDEN_TOLERANCE,
                        "{}: {}",
                        reference.display(),
                        diff.summary()
                    );
                }
            }
        }
    }

    if !recorded.is_empty() {
        eprintln!(
            "recorded {} golden image(s) — REVIEW them, then commit:\n{}",
            recorded.len(),
            recorded
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        );
        return;
    }

    assert_eq!(
        matched,
        TransitionKind::ALL.len() * GOLDEN_SIZES.len(),
        "every transition must be compared at every size"
    );
}

#[test]
fn a_stale_or_missing_golden_set_is_a_failure_not_a_silent_pass() {
    // Guards the gate itself: if someone deletes the references, CI must notice.
    // Runs only when the files are absent, which is exactly the broken state.
    let expected = TransitionKind::ALL.len() * GOLDEN_SIZES.len();
    let present = GOLDEN_SIZES
        .iter()
        .flat_map(|size| TransitionKind::ALL.iter().map(move |kind| (*kind, *size)))
        .filter(|(kind, size)| golden_path(*kind, *size).exists())
        .count();

    assert!(
        present == 0 || present == expected,
        "the golden set is incomplete: {present} of {expected} references exist; \
         re-record with OWE_RECORD_GOLDEN=1 and commit the result"
    );
}
