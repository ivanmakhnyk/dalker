//! Drawing the mouse pointer into captured frames.
//!
//! DXGI Desktop Duplication delivers the desktop image *without* the pointer; the
//! pointer comes separately as a position plus a shape (`GetFramePointerShape`). This
//! module draws such a shape onto a BGRA frame, following the rules of the three DXGI
//! shape types (the same as the Desktop Duplication sample).

/// `DXGI_OUTDUPL_POINTER_SHAPE_TYPE_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    /// 1 bpp AND mask followed by 1 bpp XOR mask (`height` covers both halves).
    Monochrome,
    /// 32 bpp BGRA, alpha-blended onto the desktop.
    Color,
    /// 32 bpp; alpha 0 = replace the desktop pixel, alpha 0xFF = XOR it with the colour.
    MaskedColor,
}

impl ShapeKind {
    pub fn from_dxgi(kind: u32) -> Option<Self> {
        match kind {
            1 => Some(Self::Monochrome),
            2 => Some(Self::Color),
            4 => Some(Self::MaskedColor),
            _ => None,
        }
    }
}

/// A pointer shape as returned by `GetFramePointerShape`.
#[derive(Debug, Clone)]
pub struct PointerShape {
    pub kind: ShapeKind,
    pub width: u32,
    /// For [`ShapeKind::Monochrome`] this is twice the visible height (AND + XOR mask).
    pub height: u32,
    /// Bytes per row of `data`.
    pub pitch: u32,
    pub data: Vec<u8>,
}

impl PointerShape {
    fn visible_height(&self) -> u32 {
        match self.kind {
            ShapeKind::Monochrome => self.height / 2,
            _ => self.height,
        }
    }
}

/// Draws `shape` with its top-left corner at `(left, top)` onto a BGRA `frame` of
/// `width`×`height` pixels with `stride` bytes per row. Parts outside the frame are
/// clipped. The frame's alpha channel is left opaque.
pub fn draw(frame: &mut [u8], width: u32, height: u32, stride: usize, shape: &PointerShape, left: i32, top: i32) {
    let (w, h) = (shape.width as i64, shape.visible_height() as i64);
    let pitch = shape.pitch as usize;
    for sy in 0..h {
        let fy = top as i64 + sy;
        if fy < 0 || fy >= height as i64 {
            continue;
        }
        for sx in 0..w {
            let fx = left as i64 + sx;
            if fx < 0 || fx >= width as i64 {
                continue;
            }
            let at = fy as usize * stride + fx as usize * 4;
            let Some(dst) = frame.get_mut(at..at + 3) else { continue };
            let (sx, sy) = (sx as usize, sy as usize);
            match shape.kind {
                ShapeKind::Monochrome => {
                    let bit = |row: usize| -> Option<bool> {
                        let byte = *shape.data.get(row * pitch + sx / 8)?;
                        Some(byte & (0x80 >> (sx % 8)) != 0)
                    };
                    let (Some(and), Some(xor)) = (bit(sy), bit(sy + h as usize)) else { continue };
                    for c in dst.iter_mut() {
                        *c = (if and { *c } else { 0 }) ^ (if xor { 0xFF } else { 0 });
                    }
                }
                ShapeKind::Color | ShapeKind::MaskedColor => {
                    let i = sy * pitch + sx * 4;
                    let Some(src) = shape.data.get(i..i + 4) else { continue };
                    let a = src[3] as u32;
                    if shape.kind == ShapeKind::Color {
                        for k in 0..3 {
                            dst[k] = ((src[k] as u32 * a + dst[k] as u32 * (255 - a) + 127) / 255) as u8;
                        }
                    } else if a == 0 {
                        dst.copy_from_slice(&src[..3]);
                    } else {
                        for k in 0..3 {
                            dst[k] ^= src[k];
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, bgr: [u8; 3]) -> Vec<u8> {
        (0..w * h).flat_map(|_| [bgr[0], bgr[1], bgr[2], 255]).collect()
    }

    fn px(f: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [f[i], f[i + 1], f[i + 2], f[i + 3]]
    }

    #[test]
    fn monochrome_and_xor_rules() {
        // 4x1 pointer: transparent, black, white, invert.
        // AND bits: 1 0 0 1 -> 0b1001_0000; XOR bits: 0 0 1 1 -> 0b0011_0000.
        let shape = PointerShape {
            kind: ShapeKind::Monochrome,
            width: 4,
            height: 2,
            pitch: 1,
            data: vec![0b1001_0000, 0b0011_0000],
        };
        let mut f = frame(4, 1, [10, 20, 30]);
        draw(&mut f, 4, 1, 16, &shape, 0, 0);
        assert_eq!(px(&f, 4, 0, 0), [10, 20, 30, 255], "AND=1 XOR=0 keeps the desktop");
        assert_eq!(px(&f, 4, 1, 0), [0, 0, 0, 255], "AND=0 XOR=0 is black");
        assert_eq!(px(&f, 4, 2, 0), [255, 255, 255, 255], "AND=0 XOR=1 is white");
        assert_eq!(px(&f, 4, 3, 0), [245, 235, 225, 255], "AND=1 XOR=1 inverts");
    }

    #[test]
    fn color_pointer_is_alpha_blended() {
        let shape = PointerShape {
            kind: ShapeKind::Color,
            width: 3,
            height: 1,
            pitch: 12,
            data: vec![0, 0, 255, 255, 0, 0, 255, 0, 200, 100, 0, 128],
        };
        let mut f = frame(3, 1, [100, 100, 100]);
        draw(&mut f, 3, 1, 12, &shape, 0, 0);
        assert_eq!(px(&f, 3, 0, 0), [0, 0, 255, 255], "opaque replaces");
        assert_eq!(px(&f, 3, 1, 0), [100, 100, 100, 255], "transparent keeps");
        assert_eq!(px(&f, 3, 2, 0), [150, 100, 50, 255], "half alpha mixes");
    }

    #[test]
    fn masked_color_replaces_or_xors() {
        let shape = PointerShape {
            kind: ShapeKind::MaskedColor,
            width: 2,
            height: 1,
            pitch: 8,
            data: vec![1, 2, 3, 0, 0xFF, 0x0F, 0, 0xFF],
        };
        let mut f = frame(2, 1, [0xF0, 0xF0, 0xF0]);
        draw(&mut f, 2, 1, 8, &shape, 0, 0);
        assert_eq!(px(&f, 2, 0, 0), [1, 2, 3, 255]);
        assert_eq!(px(&f, 2, 1, 0), [0x0F, 0xFF, 0xF0, 255]);
    }

    #[test]
    fn pointer_is_clipped_at_every_edge() {
        let shape = PointerShape {
            kind: ShapeKind::Color,
            width: 2,
            height: 2,
            pitch: 8,
            data: [0u8, 0, 255, 255].repeat(4),
        };
        for (x, y) in [(-1, -1), (2, 2), (-5, 0), (0, 10)] {
            let mut f = frame(3, 3, [0, 0, 0]);
            draw(&mut f, 3, 3, 12, &shape, x, y);
            let red = f.chunks(4).filter(|p| p[2] == 255).count();
            let expected = match (x, y) {
                (-1, -1) => 1,
                (2, 2) => 1,
                _ => 0,
            };
            assert_eq!(red, expected, "at {x},{y}");
        }
    }

    #[test]
    fn truncated_shape_data_does_not_panic() {
        let shape = PointerShape { kind: ShapeKind::Color, width: 4, height: 4, pitch: 16, data: vec![0; 10] };
        let mut f = frame(4, 4, [1, 1, 1]);
        draw(&mut f, 4, 4, 16, &shape, 0, 0);
        assert_eq!(ShapeKind::from_dxgi(3), None);
    }
}
