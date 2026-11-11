//! Verifies the red border really ends up on the captured desktop (Windows/X11):
//! shows it, captures the screen and checks the edge pixels are red and the
//! centre is untouched, then hides it and checks the edge is no longer red.
//!
//! ```text
//! cargo run -p dalker-platform --example border_probe
//! ```
//! Note: this briefly draws a red frame around your screen.

use std::time::{Duration, Instant};

fn is_red(f: &dalker_core::RawFrame, x: u32, y: u32) -> bool {
    let i = y as usize * f.stride + x as usize * 4;
    let (b, g, r) = (f.data[i], f.data[i + 1], f.data[i + 2]);
    r > 200 && g < 60 && b < 60
}

/// Waits until `pred` holds for a freshly captured frame (or times out).
fn wait_for(
    cap: &mut dyn dalker_core::ScreenCapturer,
    what: &str,
    pred: impl Fn(&dalker_core::RawFrame) -> bool,
) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(Some(f)) = cap.next_frame(Duration::from_millis(300)) {
            if pred(&f) {
                println!("ok: {what} ({:?})", start.elapsed());
                return true;
            }
        }
    }
    println!("FAIL: {what}");
    false
}

fn main() {
    dalker_platform::init_process();
    let indicator = dalker_platform::create_indicator(dalker_platform::Desktop::detect())
        .expect("create indicator");
    let mut cap = dalker_platform::create_capturer(dalker_platform::Desktop::detect())
        .expect("create capturer");
    let (w, h) = (cap.display().width, cap.display().height);
    println!(
        "indicator: {}  capture: {} {}x{}",
        indicator.name(),
        cap.name(),
        w,
        h
    );

    let edges = |f: &dalker_core::RawFrame| {
        [
            (0, 0),
            (w - 1, 0),
            (0, h - 1),
            (w - 1, h - 1),
            (w / 2, 1),
            (w / 2, h - 2),
            (1, h / 2),
            (w - 2, h / 2),
        ]
        .iter()
        .all(|&(x, y)| is_red(f, x, y))
    };
    let mut ok = true;
    // Let capture settle on the plain desktop first.
    let _ = cap.next_frame(Duration::from_millis(500));

    indicator.set_active(true);
    ok &= wait_for(cap.as_mut(), "border visible on all 4 edges", &edges);
    ok &= wait_for(cap.as_mut(), "screen centre not red", |f| {
        !is_red(f, w / 2, h / 2)
    });
    ok &= wait_for(cap.as_mut(), "pixel just inside the frame not red", |f| {
        !is_red(f, 8, 8)
    });
    indicator.set_active(false);
    ok &= wait_for(cap.as_mut(), "border gone after set_active(false)", |f| {
        !is_red(f, 0, 0)
    });
    std::process::exit(if ok { 0 } else { 1 });
}
