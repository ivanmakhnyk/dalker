//! Times the host's per-frame video pipeline (BGRA → I420, H.264 encode) on synthetic
//! screen content, so a slow stage shows up without a viewer.
//!
//! ```text
//! cargo run --release -p dalker-webrtc --example pipeline_bench -- [WIDTH HEIGHT FRAMES]
//! SCROLL=1 cargo run ...   # the whole screen changes every frame (scrolling text)
//! ```

use std::time::{Duration, Instant};

use bytes::Bytes;
use dalker_core::{PixelFormat, RawFrame};
use dalker_webrtc::encoder::{bgra_to_i420, H264Encoder, I420};
use dalker_webrtc::EncoderSettings;

/// A "desktop": flat background, text-like noise rows, and a block that moves every frame
/// (a dragged window), so the encoder sees both static and changing areas.
fn frame(w: usize, h: usize, n: usize) -> RawFrame {
    let mut data = vec![0u8; w * h * 4];
    let mut seed = 0x1234_5678u32;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let sy = if std::env::var_os("SCROLL").is_some() {
                y + n * 7
            } else {
                y
            };
            let text = (sy / 18) % 2 == 0 && x % 9 < 6 && {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed & 3 == 0
            };
            let (b, g, r) = if text { (30, 30, 30) } else { (240, 236, 230) };
            data[i..i + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    let (bx, by) = ((n * 13) % (w - 400), (n * 7) % (h - 300));
    for y in by..by + 300 {
        for x in bx..bx + 400 {
            let i = (y * w + x) * 4;
            data[i..i + 4].copy_from_slice(&[(x + n) as u8, (y * 2) as u8, 200, 255]);
        }
    }
    RawFrame {
        width: w as u32,
        height: h as u32,
        stride: w * 4,
        format: PixelFormat::Bgra8,
        data: Bytes::from(data),
        timestamp: Duration::ZERO,
    }
}

fn stats(name: &str, v: &mut [Duration]) {
    v.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let mean = v.iter().map(|d| ms(*d)).sum::<f64>() / v.len() as f64;
    println!(
        "{name:<10} mean {mean:6.2} ms   p50 {:6.2}   p95 {:6.2}   max {:6.2}",
        ms(v[v.len() / 2]),
        ms(v[v.len() * 95 / 100]),
        ms(v[v.len() - 1])
    );
}

fn main() -> anyhow::Result<()> {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let (w, h, n) = (
        *args.first().unwrap_or(&1920),
        *args.get(1).unwrap_or(&1080),
        *args.get(2).unwrap_or(&120),
    );
    let frames: Vec<RawFrame> = (0..n).map(|i| frame(w, h, i)).collect();

    let mut yuv = I420 {
        width: 0,
        height: 0,
        y: vec![],
        u: vec![],
        v: vec![],
    };
    let mut convert = Vec::new();
    for f in &frames {
        let t = Instant::now();
        bgra_to_i420(f, 1, &mut yuv);
        convert.push(t.elapsed());
    }

    let mut enc = H264Encoder::new(EncoderSettings::default());
    let (mut total, mut bytes) = (Vec::new(), 0usize);
    for f in &frames {
        let t = Instant::now();
        if let Some((b, _)) = enc.encode(f)? {
            bytes += b.len();
        }
        total.push(t.elapsed());
    }
    println!(
        "{w}x{h}, {n} frames, {:.0} kB/frame",
        bytes as f64 / n as f64 / 1000.0
    );
    stats("convert", &mut convert);
    stats("conv+enc", &mut total);
    let mean = total.iter().sum::<Duration>().as_secs_f64() / n as f64;
    println!(
        "max sustainable fps from the encode thread alone: {:.1}",
        1.0 / mean
    );
    Ok(())
}
