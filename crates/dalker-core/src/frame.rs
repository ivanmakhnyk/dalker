use std::time::Duration;

use bytes::Bytes;

/// Pixel layout of a [`RawFrame`].
///
/// Only BGRA is produced today: it is the native layout of both DXGI Desktop
/// Duplication and 24/32-bit X11 ZPixmaps on little-endian hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 4 bytes per pixel: B, G, R, A/X (alpha is ignored).
    Bgra8,
}

impl PixelFormat {
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Bgra8 => 4,
        }
    }
}

/// Geometry of the captured display, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
}

/// An uncompressed, top-down frame.
///
/// `data` is reference counted ([`Bytes`]) so one capture can be fanned out to
/// several encoders without copying.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row; may be larger than `width * bpp` because of GPU/X11 padding.
    pub stride: usize,
    pub format: PixelFormat,
    pub data: Bytes,
    /// Monotonic capture time relative to the capturer's start.
    pub timestamp: Duration,
}

impl RawFrame {
    /// Returns row `y` without the stride padding.
    ///
    /// # Panics
    /// Panics if `y >= height`.
    pub fn row(&self, y: u32) -> &[u8] {
        assert!(y < self.height, "row {y} out of bounds ({})", self.height);
        let start = y as usize * self.stride;
        &self.data[start..start + self.width as usize * self.format.bytes_per_pixel()]
    }

    /// Checks that the buffer is large enough for the declared geometry.
    pub fn is_consistent(&self) -> bool {
        let min_stride = self.width as usize * self.format.bytes_per_pixel();
        self.stride >= min_stride
            && self.height > 0
            && self.data.len() >= self.stride * (self.height as usize - 1) + min_stride
    }

    pub fn display_info(&self) -> DisplayInfo {
        DisplayInfo { width: self.width, height: self.height }
    }
}
