//! Capture-only smoke test of the native backend (no input is injected):
//!
//! ```text
//! cargo run -p dalker-platform --example probe
//! ```
//!
//! Grabs a few frames and prints geometry, timing and how much of the image is non-black.

use std::time::{Duration, Instant};

fn main() {
    dalker_platform::init_process();
    let mut cap = match dalker_platform::create_capturer(dalker_platform::Desktop::detect()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("capture unavailable: {e}");
            std::process::exit(1);
        }
    };
    println!("backend: {}  display: {:?}", cap.name(), cap.display());

    let start = Instant::now();
    let mut got = 0;
    while got < 5 && start.elapsed() < Duration::from_secs(5) {
        match cap.next_frame(Duration::from_millis(500)) {
            Ok(Some(f)) => {
                got += 1;
                let lit = f
                    .data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|p| p[0] | p[1] | p[2] != 0)
                    .count();
                let total = (f.width * f.height) as usize;
                println!(
                    "frame {got}: {}x{} stride {} t={:?} consistent={} non-black={:.1}%",
                    f.width,
                    f.height,
                    f.stride,
                    f.timestamp,
                    f.is_consistent(),
                    100.0 * lit as f64 / total as f64
                );
            }
            Ok(None) => println!("(no change)"),
            Err(e) => {
                eprintln!("capture error: {e}");
                std::process::exit(2);
            }
        }
    }
    println!("{got} frames in {:?}", start.elapsed());
}
