//! The P4 decode path, exercised through the crate's public API against real
//! files (TRD FR-LIVE-1..3).
//!
//! The unit tests inside the crate can reach private helpers; these cannot, and
//! that is the point: this file is what a *caller* — `owed`, or the frame-pacing
//! work — gets, so a change that keeps the internals consistent but breaks the
//! public shape fails here.
//!
//! Fixtures are committed under `tests/fixtures/` and were generated once with
//! `ffmpeg` (see `tests/fixtures/README.md`), so the assertions are about real
//! container bytes rather than anything this test built itself.

use std::path::{Path, PathBuf};
use std::time::Duration;

use owe_core::ContentKind;
use owe_core::config::{MediaCacheConfig, MediaConfig};
use owe_media::{
    AnimatedImageDecoder, DecodeMode, MediaDecoder, MediaError, VideoRuntime, content_kinds,
    media_backends, open, open_kind, probe, video_runtimes,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn config(cap_mb: u32, compression: &str) -> MediaConfig {
    MediaConfig {
        cache: MediaCacheConfig {
            animated_frame_cap_mb: cap_mb,
            compression: compression.to_string(),
        },
        ..MediaConfig::default()
    }
}

#[test]
fn a_still_image_decodes_to_one_frame_through_the_public_api() {
    let path = fixture("still.png");
    let mut decoder = open(&path, &MediaConfig::default()).expect("open the still fixture");

    assert_eq!(decoder.info().kind, ContentKind::StaticImage);
    assert_eq!(decoder.info().frame_count, Some(1));

    let frame = decoder.next_frame().expect("frame").expect("one frame");
    assert_eq!(frame.index(), 0);
    assert_eq!(
        frame.pixels().len(),
        frame.width() as usize * frame.height() as usize * 4
    );
    assert!(decoder.next_frame().expect("end").is_none());
    assert_eq!(decoder.stats().mode, DecodeMode::Cached);
}

#[test]
fn a_real_gif_decodes_every_frame_with_the_containers_own_timing() {
    let path = fixture("anim-3frame.gif");
    let mut decoder = AnimatedImageDecoder::open(&path, &config(96, "zstd")).expect("open the GIF");

    let info = decoder.info().clone();
    assert_eq!(info.kind, ContentKind::AnimatedImage);
    assert_eq!((info.width, info.height), (32, 32));
    assert_eq!(info.frame_count, Some(3), "the container says three frames");
    assert_eq!(info.codec.as_deref(), Some("gif"));

    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame().expect("frame") {
        frames.push(frame);
    }
    assert_eq!(frames.len(), 3);
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame.index(), index as u64);
        assert_eq!(frame.size(), (32, 32));
        assert_eq!(frame.delay(), Duration::from_millis(100), "10 fps");
    }

    // Real content, not three copies of one frame: the fixture is red, green and
    // blue, and a decoder that repeated frame 0 would pass every shape assertion.
    assert_ne!(frames[0].pixels(), frames[1].pixels());
    assert_ne!(frames[1].pixels(), frames[2].pixels());

    let stats = decoder.stats();
    assert_eq!(stats.mode, DecodeMode::Cached);
    assert_eq!(stats.cached_frames, 3);
    assert!(stats.cache_bytes > 0);
    assert!(stats.cache_cap_bytes > stats.cache_bytes);
    assert!(
        !stats.path.is_hardware(),
        "an image decode is not a GPU path"
    );
    assert_eq!(stats.path.decoder(), "gif");
}

#[test]
fn a_cap_that_cannot_hold_the_animation_falls_back_to_streaming_without_losing_frames() {
    // The FR-LIVE-1 contract: bounded memory is a hard promise, and the frame the
    // cap refused must still be presented — it is the animation's next frame.
    let path = fixture("anim-3frame.gif");
    let mut decoder = AnimatedImageDecoder::open(&path, &config(0, "zstd")).expect("open the GIF");

    assert_eq!(
        decoder.stats().mode,
        DecodeMode::Streaming,
        "a zero-cap cache must stream"
    );
    assert_eq!(decoder.stats().cache_bytes, 0, "and must cache nothing");

    let mut indices = Vec::new();
    while let Some(frame) = decoder.next_frame().expect("frame") {
        indices.push(frame.index());
    }
    assert_eq!(indices, vec![0, 1, 2], "streaming must not skip a frame");
}

#[test]
fn probe_reports_metadata_without_a_frame_stream() {
    let still = probe(&fixture("still.png"), &MediaConfig::default()).expect("probe the PNG");
    assert_eq!(still.kind, ContentKind::StaticImage);
    assert_eq!((still.width, still.height), (2, 2));

    let gif = probe(&fixture("anim-3frame.gif"), &MediaConfig::default()).expect("probe the GIF");
    assert_eq!(gif.kind, ContentKind::AnimatedImage);
    assert_eq!(gif.frame_count, Some(3));
    assert_eq!(gif.codec.as_deref(), Some("gif"));
}

#[test]
fn a_video_decodes_to_rgba_frames_or_says_exactly_which_runtime_is_missing() {
    // This machine has both `gst-launch-1.0` and `ffmpeg`, so the decode runs for
    // real here. On a machine without them the *only* acceptable answer is an
    // `Unavailable` naming the tool — never a decode error, which would send the
    // user looking for a broken file.
    let path = fixture("video-5frame.mp4");
    let mut decoder = match open(&path, &MediaConfig::default()) {
        Ok(decoder) => decoder,
        Err(error) => {
            assert!(
                matches!(error, MediaError::Unavailable { .. }),
                "a missing runtime must not look like a broken file: {error}"
            );
            assert!(!video_runtimes().iter().any(|runtime| runtime.available));
            eprintln!("skipping the decode: {error}");
            return;
        }
    };

    assert_eq!(decoder.info().kind, ContentKind::Video);
    assert_eq!((decoder.info().width, decoder.info().height), (64, 48));
    assert_eq!(decoder.info().fps, Some(10.0));
    assert_eq!(decoder.info().duration, Some(Duration::from_millis(500)));
    // The frame count is a runtime-specific fact and is asserted as such: the
    // ffmpeg prober reads it from the container, the GStreamer discoverer does not
    // report one, and claiming a number it did not measure would be the exact
    // over-claim this project keeps out of its capability surface.
    assert!(
        decoder.info().frame_count.is_none() || decoder.info().frame_count == Some(5),
        "frame_count was {:?}",
        decoder.info().frame_count
    );

    let mut frames = 0;
    while let Some(frame) = decoder.next_frame().expect("frame") {
        assert_eq!(
            frame.pixels().len(),
            64 * 48 * 4,
            "every frame is a complete RGBA buffer"
        );
        assert_eq!(frame.delay(), Duration::from_millis(100));
        frames += 1;
    }
    assert_eq!(frames, 5, "the fixture is five frames");

    // FR-LIVE-3: the decode path is reported from the negotiation, and software is
    // a legitimate answer (this machine has no usable VA-API driver).
    let stats = decoder.stats();
    assert_eq!(stats.mode, DecodeMode::Streaming);
    assert!(stats.path.decoder().contains("software") || stats.path.is_hardware());
}

#[test]
fn each_installed_runtime_decodes_the_fixture_on_its_own() {
    // ADR-006 makes GStreamer primary and FFmpeg the fallback, and BACKEND-DESIGN
    // §6.1 calls the second one the degradation path. That ordering is only real if
    // each pipeline can decode the file *by itself*, and if the daemon never serves
    // an explicit choice from the other runtime — otherwise `media.backend` is
    // decorative and a distro without GStreamer plugins has no fallback at all.
    let path = fixture("video-5frame.mp4");
    let available: Vec<&'static str> = video_runtimes()
        .iter()
        .filter(|runtime| runtime.available)
        .map(|runtime| runtime.runtime.as_str())
        .collect();
    if available.is_empty() {
        eprintln!("no video runtime on this machine; selection is covered by unit tests");
        return;
    }

    // The explicit runtimes first, then `auto` — which must land on one of their
    // answers rather than on a third result only `auto` can produce.
    let mut results = Vec::new();
    for backend in available.iter().copied().chain(std::iter::once("auto")) {
        let config = MediaConfig {
            backend: backend.to_string(),
            ..MediaConfig::default()
        };
        let mut decoder = open(&path, &config)
            .unwrap_or_else(|error| panic!("`{backend}` failed on the committed fixture: {error}"));

        let mut frames = 0;
        while let Some(frame) = decoder.next_frame().expect("frame") {
            assert_eq!(
                frame.pixels().len(),
                64 * 48 * 4,
                "`{backend}` handed over a partial frame"
            );
            frames += 1;
        }
        assert_eq!(
            frames, 5,
            "`{backend}` must produce the fixture's five frames"
        );
        results.push((backend, decoder.stats()));
    }

    for (backend, stats) in &results {
        if *backend == "auto" {
            continue;
        }
        let runtime = VideoRuntime::parse(backend).expect("a probed runtime id parses");
        let named = stats.path.decoder() == runtime.software_decoder()
            || stats.path.decoder() == runtime.hardware_decoder();
        assert!(
            named,
            "`{backend}` was asked for, but the stats named `{}` — an explicit \
             `media.backend` must never be served by the other runtime",
            stats.path.decoder()
        );
    }

    // And `auto` is not a third decoder: it resolves to one the probes called
    // usable, which is the whole point of preferring GStreamer over FFmpeg.
    let auto = &results.last().expect("auto was requested").1;
    assert!(
        available
            .iter()
            .filter_map(|id| VideoRuntime::parse(id))
            .any(|runtime| auto.path.decoder() == runtime.software_decoder()
                || auto.path.decoder() == runtime.hardware_decoder()),
        "`auto` reported `{}`, which is neither installed runtime",
        auto.path.decoder()
    );
}

#[test]
fn capabilities_are_derived_from_the_probes_not_from_a_wish_list() {
    let kinds = content_kinds();
    assert!(kinds.contains(&"static-image"));
    assert!(kinds.contains(&"animated-image"));
    assert_eq!(
        kinds.contains(&"video"),
        video_runtimes().iter().any(|runtime| runtime.available)
    );

    let backends = media_backends();
    assert!(backends.contains(&"image"));
    for runtime in video_runtimes() {
        assert_eq!(
            backends.contains(&runtime.runtime.as_str()),
            runtime.available,
            "`{}` must be advertised exactly when it is usable: {}",
            runtime.runtime.as_str(),
            runtime.detail
        );
        if !runtime.available {
            assert!(
                runtime.detail.contains("PATH"),
                "the reason must name the missing binary: {}",
                runtime.detail
            );
        }
    }
}

#[test]
fn a_kind_this_build_cannot_decode_yet_says_so() {
    let error = open_kind(
        &fixture("still.png"),
        ContentKind::Shader,
        &MediaConfig::default(),
    )
    .err()
    .expect("shaders are P5");
    assert!(
        matches!(error, MediaError::UnsupportedKind { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("P5") || error.to_string().contains("shader"));
}
