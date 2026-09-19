//! Generates the app's placeholder icons.
//!
//! Real branding is deliberately deferred with the name check (PRD OQ-1), so the
//! project needs *some* icon that is (a) committed, (b) reproducible, and (c) not
//! a mystery binary blob nobody can regenerate. Hence a committed generator
//! instead of hand-drawn files.
//!
//! Run from the workspace root:
//!
//! ```text
//! cargo run -p owe-render --example gen-app-icons            # writes app/src-tauri/icons
//! cargo run -p owe-render --example gen-app-icons -- <dir>   # writes somewhere else
//! ```
//!
//! On Linux these PNGs are all the bundler needs. Windows `.ico` and macOS
//! `.icns` are produced by `cargo tauri icon` when those platforms are targeted;
//! they are absent on purpose rather than broken.

use std::path::PathBuf;

use image::RgbaImage;

/// Background gradient, top.
const BG_TOP: [u8; 3] = [0x10, 0x10, 0x18];
/// Background gradient, bottom.
const BG_BOTTOM: [u8; 3] = [0x1c, 0x1c, 0x30];
/// Monitor bezel.
const FRAME: [u8; 3] = [0x7a, 0xa2, 0xf7];
/// Screen fill.
const SCREEN: [u8; 3] = [0x2f, 0x38, 0x56];
/// The "sun" on the screen.
const SUN: [u8; 3] = [0xe0, 0xaf, 0x68];
/// Monitor stand.
const STAND: [u8; 3] = [0x56, 0x5f, 0x89];

/// Linear interpolation between two RGB colours.
fn lerp(from: [u8; 3], to: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let channel = |index: usize| {
        let a = f32::from(from[index]);
        let b = f32::from(to[index]);
        (a + (b - a) * t).round() as u8
    };
    [channel(0), channel(1), channel(2)]
}

/// Draw one square icon at `size` pixels: a monitor showing a sun over a sky.
fn render(size: u32) -> RgbaImage {
    let mut image = RgbaImage::new(size, size);
    let scale = size as f32;

    for y in 0..size {
        for x in 0..size {
            // Normalised coordinates keep the drawing size-independent.
            let fx = x as f32 / scale;
            let fy = y as f32 / scale;

            let mut colour = lerp(BG_TOP, BG_BOTTOM, fy);

            let bezel = (0.14..=0.86).contains(&fx) && (0.20..=0.70).contains(&fy);
            let screen = (0.19..=0.81).contains(&fx) && (0.25..=0.65).contains(&fy);
            if bezel {
                colour = FRAME;
            }
            if screen {
                colour = SCREEN;

                let dx = fx - 0.70;
                let dy = fy - 0.36;
                if dx * dx + dy * dy < 0.075 * 0.075 {
                    colour = SUN;
                }
            }

            let neck = (0.46..=0.54).contains(&fx) && (0.70..=0.80).contains(&fy);
            let base = (0.34..=0.66).contains(&fx) && (0.80..=0.86).contains(&fy);
            if neck || base {
                colour = STAND;
            }

            image.put_pixel(x, y, image::Rgba([colour[0], colour[1], colour[2], 255]));
        }
    }

    image
}

fn main() {
    let directory = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("app/src-tauri/icons"), PathBuf::from);

    std::fs::create_dir_all(&directory).expect("create the icon directory");

    // Sizes referenced by app/src-tauri/tauri.conf.json's bundle.icon list.
    let targets: [(u32, &str); 4] = [
        (32, "32x32.png"),
        (128, "128x128.png"),
        (256, "128x128@2x.png"),
        (512, "icon.png"),
    ];

    for (size, name) in targets {
        let path = directory.join(name);
        render(size)
            .save(&path)
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        println!("wrote {} ({size}x{size})", path.display());
    }
}
