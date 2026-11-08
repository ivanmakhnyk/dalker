//! Screenshots for the model: geometry (screenshot pixels ↔ normalized points) and encoding.
//!
//! The model works in pixels of the last full screenshot it saw. That image is the screen
//! downscaled so its longest side fits `max_width`; [`Frame`] remembers its size and turns
//! the model's pixel coordinates into the normalized `0..1` points [`InputEvent`] uses.
//!
//! [`InputEvent`]: dalker_core::InputEvent

use std::io::Cursor;

use dalker_core::{NormPoint, PixelFormat, RawFrame};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageEncoder, RgbImage};

/// Default longest side of a screenshot. Vision models do better with images this size.
pub const DEFAULT_MAX_WIDTH: u32 = 1280;
pub const MIN_MAX_WIDTH: u32 = 256;
pub const MAX_MAX_WIDTH: u32 = 4096;
const JPEG_QUALITY: u8 = 80;

/// Size of the image the model's coordinates refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
}

impl Frame {
    /// The size a screen of `native_w`×`native_h` is shown at, longest side ≤ `max_width`
    /// (never upscaled).
    pub fn fit(native_w: u32, native_h: u32, max_width: u32) -> Self {
        let long = native_w.max(native_h).max(1);
        if long <= max_width {
            return Self {
                width: native_w.max(1),
                height: native_h.max(1),
            };
        }
        let scale = max_width as f64 / long as f64;
        Self {
            width: ((native_w as f64 * scale).round() as u32).max(1),
            height: ((native_h as f64 * scale).round() as u32).max(1),
        }
    }

    /// Pixel `(x, y)` of this image as a normalized point (same convention as
    /// [`NormPoint::to_pixels`]: the last pixel maps to `1.0`). Errors when outside the image.
    pub fn to_norm(&self, x: f64, y: f64) -> Result<NormPoint, String> {
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x > (self.width - 1) as f64
            || y > (self.height - 1) as f64
        {
            return Err(format!(
                "({x}, {y}) is outside the screenshot (0..{} x 0..{}); coordinates are pixels of the last screenshot",
                self.width - 1,
                self.height - 1
            ));
        }
        let nx = if self.width > 1 {
            x / (self.width - 1) as f64
        } else {
            0.0
        };
        let ny = if self.height > 1 {
            y / (self.height - 1) as f64
        } else {
            0.0
        };
        Ok(NormPoint::new(nx as f32, ny as f32))
    }
}

/// A rectangle in pixels of a [`Frame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Image format of a screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Png,
}

impl Format {
    pub fn mime(self) -> &'static str {
        match self {
            Format::Jpeg => "image/jpeg",
            Format::Png => "image/png",
        }
    }
}

/// An encoded screenshot.
#[derive(Debug, Clone)]
pub struct Shot {
    pub data: Vec<u8>,
    pub format: Format,
    pub width: u32,
    pub height: u32,
    /// The part of the screen shown, in native pixels.
    pub native: Region,
}

/// Maps `region` (pixels of `frame`) to native pixels of a `native_w`×`native_h` screen,
/// clipped to the screen. Errors if nothing is left.
pub fn region_to_native(
    frame: Frame,
    region: Region,
    native_w: u32,
    native_h: u32,
) -> Result<Region, String> {
    if region.w == 0 || region.h == 0 {
        return Err("region width and height must be positive".into());
    }
    if region.x >= frame.width || region.y >= frame.height {
        return Err(format!(
            "region starts outside the screenshot ({}x{})",
            frame.width, frame.height
        ));
    }
    let sx = native_w as f64 / frame.width as f64;
    let sy = native_h as f64 / frame.height as f64;
    let x0 = ((region.x as f64 * sx).floor() as u32).min(native_w - 1);
    let y0 = ((region.y as f64 * sy).floor() as u32).min(native_h - 1);
    let x1 =
        (((region.x as u64 + region.w as u64) as f64 * sx).ceil() as u32).clamp(x0 + 1, native_w);
    let y1 =
        (((region.y as u64 + region.h as u64) as f64 * sy).ceil() as u32).clamp(y0 + 1, native_h);
    Ok(Region {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Crops `native` out of `frame`, scales it so the longest side is at most `max_width`, and
/// encodes it. CPU-heavy: run it off the async workers.
pub fn encode(
    frame: &RawFrame,
    native: Region,
    max_width: u32,
    format: Format,
) -> Result<Shot, String> {
    if !frame.is_consistent() || frame.format != PixelFormat::Bgra8 {
        return Err("captured frame is malformed".into());
    }
    let native = Region {
        x: native.x.min(frame.width - 1),
        y: native.y.min(frame.height - 1),
        w: native
            .w
            .min(frame.width - native.x.min(frame.width - 1))
            .max(1),
        h: native
            .h
            .min(frame.height - native.y.min(frame.height - 1))
            .max(1),
    };
    let mut rgb = Vec::with_capacity(native.w as usize * native.h as usize * 3);
    for y in native.y..native.y + native.h {
        let row = frame.row(y);
        for px in row[native.x as usize * 4..(native.x + native.w) as usize * 4].chunks_exact(4) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    let mut img = RgbImage::from_raw(native.w, native.h, rgb).ok_or("bad image buffer")?;
    let target = Frame::fit(native.w, native.h, max_width);
    if (target.width, target.height) != (native.w, native.h) {
        img = image::imageops::resize(&img, target.width, target.height, FilterType::Triangle);
    }
    let mut data = Vec::new();
    let (w, h) = img.dimensions();
    let result = match format {
        Format::Jpeg => JpegEncoder::new_with_quality(Cursor::new(&mut data), JPEG_QUALITY)
            .write_image(img.as_raw(), w, h, ExtendedColorType::Rgb8),
        Format::Png => PngEncoder::new(Cursor::new(&mut data)).write_image(
            img.as_raw(),
            w,
            h,
            ExtendedColorType::Rgb8,
        ),
    };
    result.map_err(|e| format!("encode screenshot: {e}"))?;
    Ok(Shot {
        data,
        format,
        width: w,
        height: h,
        native,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes_frame::frame;

    mod bytes_frame {
        use dalker_core::{PixelFormat, RawFrame};
        use std::time::Duration;

        /// A frame whose pixel (x, y) has B = x, G = y (mod 256), R = 7, with stride padding.
        pub fn frame(w: u32, h: u32) -> RawFrame {
            let stride = w as usize * 4 + 16;
            let mut buf = vec![0u8; stride * h as usize];
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let i = y * stride + x * 4;
                    buf[i..i + 4].copy_from_slice(&[x as u8, y as u8, 7, 255]);
                }
            }
            RawFrame {
                width: w,
                height: h,
                stride,
                format: PixelFormat::Bgra8,
                data: buf.into(),
                timestamp: Duration::ZERO,
            }
        }
    }

    #[test]
    fn fit_keeps_aspect_and_never_upscales() {
        assert_eq!(
            Frame::fit(1920, 1080, 1280),
            Frame {
                width: 1280,
                height: 720
            }
        );
        assert_eq!(
            Frame::fit(2560, 1600, 1280),
            Frame {
                width: 1280,
                height: 800
            }
        );
        assert_eq!(
            Frame::fit(1080, 1920, 1280),
            Frame {
                width: 720,
                height: 1280
            },
            "portrait: longest side"
        );
        assert_eq!(
            Frame::fit(1024, 768, 1280),
            Frame {
                width: 1024,
                height: 768
            }
        );
        assert_eq!(
            Frame::fit(3440, 1440, 1280),
            Frame {
                width: 1280,
                height: 536
            }
        );
    }

    #[test]
    fn screenshot_pixels_map_to_normalized_points() {
        let f = Frame {
            width: 1280,
            height: 720,
        };
        assert_eq!(f.to_norm(0.0, 0.0).unwrap(), NormPoint { x: 0.0, y: 0.0 });
        assert_eq!(
            f.to_norm(1279.0, 719.0).unwrap(),
            NormPoint { x: 1.0, y: 1.0 }
        );
        let p = f.to_norm(639.5, 359.5).unwrap();
        assert!(
            (p.x - 0.5).abs() < 1e-6 && (p.y - 0.5).abs() < 1e-6,
            "{p:?}"
        );
        // The centre of the screenshot lands on the centre of a 1920x1080 screen.
        assert_eq!(
            f.to_norm(639.5, 359.5).unwrap().to_pixels(1920, 1080),
            (960, 540)
        );
        // A pixel maps back onto the same spot of the native screen (1.5x).
        assert_eq!(
            f.to_norm(100.0, 200.0).unwrap().to_pixels(1920, 1080),
            (150, 300)
        );
        for (x, y) in [
            (-1.0, 0.0),
            (0.0, -0.5),
            (1280.0, 0.0),
            (0.0, 720.0),
            (f64::NAN, 1.0),
            (f64::INFINITY, 1.0),
        ] {
            assert!(f.to_norm(x, y).is_err(), "({x}, {y}) should be rejected");
        }
    }

    #[test]
    fn regions_map_to_native_pixels() {
        let f = Frame {
            width: 1280,
            height: 720,
        };
        let r = region_to_native(
            f,
            Region {
                x: 100,
                y: 50,
                w: 200,
                h: 100,
            },
            1920,
            1080,
        )
        .unwrap();
        assert_eq!(
            r,
            Region {
                x: 150,
                y: 75,
                w: 300,
                h: 150
            }
        );
        // Clipped at the screen edge.
        let r = region_to_native(
            f,
            Region {
                x: 1200,
                y: 700,
                w: 500,
                h: 500,
            },
            1920,
            1080,
        )
        .unwrap();
        assert_eq!(
            r,
            Region {
                x: 1800,
                y: 1050,
                w: 120,
                h: 30
            }
        );
        assert!(region_to_native(
            f,
            Region {
                x: 0,
                y: 0,
                w: 0,
                h: 10
            },
            1920,
            1080
        )
        .is_err());
        assert!(region_to_native(
            f,
            Region {
                x: 1280,
                y: 0,
                w: 10,
                h: 10
            },
            1920,
            1080
        )
        .is_err());
    }

    #[test]
    fn encodes_downscaled_jpeg_and_exact_png_crop() {
        let raw = frame(640, 360);
        let full = Region {
            x: 0,
            y: 0,
            w: 640,
            h: 360,
        };
        let shot = encode(&raw, full, 320, Format::Jpeg).unwrap();
        assert_eq!((shot.width, shot.height), (320, 180));
        assert_eq!(&shot.data[..2], &[0xFF, 0xD8], "JPEG magic");

        let crop = Region {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        };
        let shot = encode(&raw, crop, 1280, Format::Png).unwrap();
        assert_eq!(
            (shot.width, shot.height),
            (30, 40),
            "crops are not scaled up"
        );
        let img = image::load_from_memory(&shot.data).unwrap().to_rgb8();
        // Top-left of the crop is native pixel (10, 20): R = 7, G = 20, B = 10.
        assert_eq!(img.get_pixel(0, 0).0, [7, 20, 10]);
        assert_eq!(img.get_pixel(29, 39).0, [7, 59, 39]);
    }
}
