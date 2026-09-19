//! present-probe — present one solid colour through the *production* presenter.
//!
//! Debug tool, not a test: it exists to isolate the layer-shell present path from
//! everything else (decode, GPU scaling, the engine's worker threads). When a
//! wallpaper does not appear on screen, this answers "is the presenter at fault?"
//! in one run, instead of guessing.
//!
//!   cargo run -p owe-render --example present-probe -- magenta [seconds]
//!
//! It presents on every output, waits (default 5 seconds) so you can look at the
//! screen or take a screenshot, then clears. The surface is on the background
//! layer with the namespace `owe-probe`.

use std::thread::sleep;
use std::time::Duration;

use owe_render::surface::{Frame, PresentOutcome, Presenter};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let colour_name = args.first().map(String::as_str).unwrap_or("magenta");
    let hold: u64 = args
        .get(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(5);

    // XRGB8888 on little-endian is B, G, R, X — so these are bgr triples.
    let bgr = match colour_name {
        "magenta" => [0xff, 0x00, 0xff],
        "green" => [0x00, 0xff, 0x00],
        "blue" => [0xff, 0x00, 0x00],
        "black" => [0x00, 0x00, 0x00],
        "white" => [0xff, 0xff, 0xff],
        other => {
            eprintln!("unknown colour `{other}`; use magenta|green|blue|black|white");
            std::process::exit(2);
        }
    };

    let presenter = match Presenter::start("owe-probe") {
        Ok(presenter) => presenter,
        Err(error) => {
            eprintln!("cannot start the presenter: {error}");
            std::process::exit(1);
        }
    };

    let outputs = presenter.outputs().to_vec();
    if outputs.is_empty() {
        eprintln!("no outputs");
        std::process::exit(1);
    }
    println!("outputs: {}", outputs.join(", "));

    for output in &outputs {
        // First attempt deliberately uses a placeholder size: the compositor's
        // configure is authoritative, so we ask it and then render for real.
        let mut size = (16, 16);
        for attempt in 1..=3 {
            let frame = Frame::solid(size.0, size.1, bgr);
            match presenter.present(output, frame) {
                Ok(PresentOutcome::Presented { size }) => {
                    println!(
                        "{output}: presented {}x{} (attempt {attempt})",
                        size.0, size.1
                    );
                    break;
                }
                Ok(PresentOutcome::ResizeRequired { size: wanted }) => {
                    println!(
                        "{output}: compositor wants {}x{} (attempt {attempt})",
                        wanted.0, wanted.1
                    );
                    size = wanted;
                }
                Err(error) => {
                    eprintln!("{output}: present failed: {error}");
                    break;
                }
            }
        }
    }

    println!("holding {hold}s — look at the screen (or screenshot) now");
    sleep(Duration::from_secs(hold));

    for output in &outputs {
        if let Err(error) = presenter.clear(output) {
            eprintln!("{output}: clear failed: {error}");
        }
    }
    println!("cleared; exiting");
}
