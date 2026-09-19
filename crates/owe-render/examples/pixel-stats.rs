//! Inspect a PNG's pixels: this is what the end-to-end test uses to prove that
//! what the compositor shows is actually the wallpaper we sent it.
//!
//! ```text
//! cargo run -p owe-render --example pixel-stats -- shot.png
//! cargo run -p owe-render --example pixel-stats -- shot.png --expect ff00ff --min-fraction 0.3
//! cargo run -p owe-render --example pixel-stats -- shot.png --diff reference.png
//! ```
//!
//! Prints JSON (machine-readable — the shell script parses it) and exits non-zero
//! when an expectation is not met, so it can gate a test directly.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use owe_render::golden::GoldenImage;

struct Options {
    image: PathBuf,
    expect: Option<[u8; 3]>,
    tolerance: u8,
    min_fraction: f64,
    diff: Option<PathBuf>,
}

fn parse_hex(value: &str) -> Result<[u8; 3], String> {
    let value = value.trim_start_matches('#');
    if value.len() != 6 {
        return Err(format!("expected 6 hex digits, got `{value}`"));
    }
    let byte = |index: usize| {
        u8::from_str_radix(&value[index..index + 2], 16)
            .map_err(|error| format!("bad hex in `{value}`: {error}"))
    };
    Ok([byte(0)?, byte(2)?, byte(4)?])
}

fn parse_args() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let image = args
        .next()
        .ok_or("usage: pixel-stats <image.png> [options]")?;
    let mut options = Options {
        image: PathBuf::from(image),
        expect: None,
        tolerance: 0,
        min_fraction: 0.0,
        diff: None,
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--expect" => {
                options.expect = Some(parse_hex(
                    &args.next().ok_or("--expect needs a hex colour")?,
                )?);
            }
            "--tolerance" => {
                options.tolerance = args
                    .next()
                    .ok_or("--tolerance needs a number")?
                    .parse()
                    .map_err(|error| format!("bad tolerance: {error}"))?;
            }
            "--min-fraction" => {
                options.min_fraction = args
                    .next()
                    .ok_or("--min-fraction needs a number")?
                    .parse()
                    .map_err(|error| format!("bad fraction: {error}"))?;
            }
            "--diff" => {
                options.diff = Some(PathBuf::from(args.next().ok_or("--diff needs a path")?));
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(options)
}

/// Percentage of pixels within `tolerance` of `target`, plus the first hit.
fn match_fraction(
    image: &GoldenImage,
    target: [u8; 3],
    tolerance: u8,
) -> (f64, u64, Option<(u32, u32)>) {
    let mut hits = 0_u64;
    let mut first = None;
    let total = u64::from(image.width()) * u64::from(image.height());
    for (index, pixel) in image.pixels().chunks_exact(4).enumerate() {
        let close = (0..3).all(|channel| pixel[channel].abs_diff(target[channel]) <= tolerance);
        if close {
            hits += 1;
            if first.is_none() {
                let position = index as u32;
                first = Some((position % image.width(), position / image.width()));
            }
        }
    }
    let fraction = if total == 0 {
        0.0
    } else {
        hits as f64 / total as f64
    };
    (fraction, hits, first)
}

fn mean_colour(image: &GoldenImage) -> (u8, u8, u8) {
    let mut sums = [0_u64; 3];
    let mut count = 0_u64;
    for pixel in image.pixels().chunks_exact(4) {
        for channel in 0..3 {
            sums[channel] += u64::from(pixel[channel]);
        }
        count += 1;
    }
    if count == 0 {
        return (0, 0, 0);
    }
    (
        (sums[0] / count) as u8,
        (sums[1] / count) as u8,
        (sums[2] / count) as u8,
    )
}

/// The most common exact colour and how much of the image it covers.
fn dominant_colour(image: &GoldenImage) -> ([u8; 3], f64) {
    let mut counts: HashMap<[u8; 3], u64> = HashMap::new();
    for pixel in image.pixels().chunks_exact(4) {
        *counts.entry([pixel[0], pixel[1], pixel[2]]).or_insert(0) += 1;
    }
    let total = u64::from(image.width()) * u64::from(image.height());
    match counts.iter().max_by_key(|(_, count)| **count) {
        Some((colour, count)) if total > 0 => (*colour, *count as f64 / total as f64),
        _ => ([0, 0, 0], 0.0),
    }
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("pixel-stats: {error}");
            return ExitCode::from(2);
        }
    };

    let image = match GoldenImage::from_png(&options.image) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("pixel-stats: {error}");
            return ExitCode::from(2);
        }
    };

    let (mean_r, mean_g, mean_b) = mean_colour(&image);
    let (dominant, dominant_fraction) = dominant_colour(&image);

    let mut fields = vec![
        format!(
            "\"image\":{}",
            json_string(&options.image.display().to_string())
        ),
        format!("\"width\":{}", image.width()),
        format!("\"height\":{}", image.height()),
        format!("\"mean_rgb\":\"{mean_r:02x}{mean_g:02x}{mean_b:02x}\""),
        format!(
            "\"dominant_rgb\":\"{:02x}{:02x}{:02x}\"",
            dominant[0], dominant[1], dominant[2]
        ),
        format!("\"dominant_fraction\":{dominant_fraction:.4}"),
    ];

    let mut failed = false;

    if let Some(target) = options.expect {
        let (fraction, hits, first) = match_fraction(&image, target, options.tolerance);
        fields.push(format!(
            "\"expect_rgb\":\"{:02x}{:02x}{:02x}\"",
            target[0], target[1], target[2]
        ));
        fields.push(format!("\"expect_tolerance\":{}", options.tolerance));
        fields.push(format!("\"expect_fraction\":{fraction:.4}"));
        fields.push(format!("\"expect_pixels\":{hits}"));
        match first {
            Some((x, y)) => fields.push(format!("\"expect_first\":[{x},{y}]")),
            None => fields.push("\"expect_first\":null".to_string()),
        }
        fields.push(format!("\"min_fraction\":{:.4}", options.min_fraction));
        if fraction < options.min_fraction {
            fields.push("\"expect_ok\":false".to_string());
            failed = true;
        } else {
            fields.push("\"expect_ok\":true".to_string());
        }
    }

    if let Some(reference_path) = &options.diff {
        match GoldenImage::from_png(reference_path) {
            Ok(reference) => match image.diff(&reference, options.tolerance) {
                Ok(diff) => {
                    fields.push(format!("\"diff_is_match\":{}", diff.is_match()));
                    fields.push(format!(
                        "\"diff_differing_pixels\":{}",
                        diff.differing_pixels
                    ));
                    fields.push(format!(
                        "\"diff_max_channel_delta\":{}",
                        diff.max_channel_delta
                    ));
                    fields.push(format!("\"diff_summary\":{}", json_string(&diff.summary())));
                    if !diff.is_match() {
                        failed = true;
                    }
                }
                Err(error) => {
                    fields.push(format!(
                        "\"diff_error\":{}",
                        json_string(&error.to_string())
                    ));
                    failed = true;
                }
            },
            Err(error) => {
                fields.push(format!(
                    "\"diff_error\":{}",
                    json_string(&error.to_string())
                ));
                failed = true;
            }
        }
    }

    println!("{{{}}}", fields.join(","));
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// Minimal JSON string escaping — these are paths and messages, not user content.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
