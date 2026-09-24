//! A frame the media layer decoded, drawn by the render path that already exists.
//!
//! This is the seam P4 has to prove: `owe-media` produces `RGBA8` buffers and
//! `owe-render::image::render` consumes exactly that layout, so no conversion step
//! and no new renderer are needed for animated content — only a frame clock, which
//! is the remaining P4 task. The test decodes a real GIF through `owe-media`'s
//! public API and draws it, so a shape change on either side fails here rather
//! than at the first wallpaper change on someone's desktop.
//!
//! GPU-less machines skip with a message, the convention the transition goldens
//! already use (`HeadlessGpu::new` returns `Ok(None)` when there is no adapter).

use std::path::Path;

use owe_media::{MediaConfig, open};
use owe_render::gpu::HeadlessGpu;
use owe_render::image::{PixelFormat, Scaling, render};

const GIF: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../owe-media/tests/fixtures/anim-3frame.gif"
);

fn gpu() -> Option<HeadlessGpu> {
    match HeadlessGpu::new() {
        Ok(Some(gpu)) => Some(gpu),
        Ok(None) => {
            eprintln!("skipping: no wgpu adapter on this machine");
            None
        }
        Err(error) => panic!("device creation must not fail: {error}"),
    }
}

fn centre(pixels: &[u8], size: (u32, u32)) -> [u8; 4] {
    let (x, y) = (size.0 / 2, size.1 / 2);
    let offset = ((y * size.0 + x) * 4) as usize;
    [
        pixels[offset],
        pixels[offset + 1],
        pixels[offset + 2],
        pixels[offset + 3],
    ]
}

#[test]
fn a_decoded_animation_frame_renders_as_the_picture_it_contains() {
    let Some(gpu) = gpu() else {
        return;
    };

    let mut decoder =
        open(Path::new(GIF), &MediaConfig::default()).expect("open the committed GIF fixture");
    let frame = decoder.next_frame().expect("frame").expect("one frame");
    assert_eq!(frame.size(), (32, 32));

    let target = (64, 64);
    let pixels = render(
        &gpu,
        target,
        frame.size(),
        frame.pixels(),
        Scaling::Cover,
        PixelFormat::Rgba8,
        [0.0, 0.0, 0.0, 1.0],
    )
    .expect("the decoded frame must satisfy the render path's buffer contract");
    assert_eq!(pixels.len(), 64 * 64 * 4);

    // The fixture's first frame is red. A renderer that dropped the frame, or that
    // read the alpha channel as the colour, would come back black here.
    let centre = centre(&pixels, target);
    assert!(
        centre[0] > 200 && centre[1] < 60 && centre[2] < 60,
        "expected the fixture's red frame, got {centre:?}"
    );
}

#[test]
fn the_frames_of_one_animation_render_as_different_pictures() {
    // "The frame reached the renderer" is worth nothing if every frame renders the
    // same; the fixture is red, green and blue, so the drawn output must differ.
    let Some(gpu) = gpu() else {
        return;
    };

    let mut decoder =
        open(Path::new(GIF), &MediaConfig::default()).expect("open the committed GIF fixture");
    let target = (48, 48);
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame().expect("frame") {
        let pixels = render(
            &gpu,
            target,
            frame.size(),
            frame.pixels(),
            Scaling::Cover,
            PixelFormat::Rgba8,
            [0.0, 0.0, 0.0, 1.0],
        )
        .expect("render");
        frames.push(centre(&pixels, target));
    }

    assert_eq!(frames.len(), 3);
    assert_ne!(
        frames[0], frames[1],
        "frame 1 must render differently to frame 0"
    );
    assert_ne!(
        frames[1], frames[2],
        "frame 2 must render differently to frame 1"
    );
}

#[test]
fn the_configured_fit_modes_draw_a_frame_differently() {
    // FR-LIVE-4's modes have to be *distinguishable* on real pixels, not only in
    // the transform maths: a 32×32 frame on a 64×64 output is bars with `center`,
    // edge-to-edge with `fill`.
    let Some(gpu) = gpu() else {
        return;
    };
    let mut decoder =
        open(Path::new(GIF), &MediaConfig::default()).expect("open the committed GIF fixture");
    let frame = decoder.next_frame().expect("frame").expect("one frame");

    let corner = (2_u32, 2_u32);
    let background = [0.0, 0.0, 1.0, 1.0];
    let centre_of = |mode: Scaling| {
        let pixels = render(
            &gpu,
            (64, 64),
            frame.size(),
            frame.pixels(),
            mode,
            PixelFormat::Rgba8,
            background,
        )
        .expect("render");
        let offset = ((corner.1 * 64 + corner.0) * 4) as usize;
        [
            pixels[offset],
            pixels[offset + 1],
            pixels[offset + 2],
            pixels[offset + 3],
        ]
    };

    // `center` keeps the source at its own size, so the corner is background…
    assert_eq!(
        centre_of(Scaling::Center),
        [0, 0, 255, 255],
        "centre leaves the edge of the output to the background"
    );
    // …while `fill` covers the whole output with the picture.
    let filled = centre_of(Scaling::Cover);
    assert!(
        filled[0] > 200,
        "fill must cover the corner with the image, got {filled:?}"
    );
}
