//! Writes the app icons to `assets/branding/` from the drawing in `src/icon.rs`:
//! `dalker.ico` (Windows exe and installer) and `png/dalker-<size>.png` (Linux).
//!
//!     cargo run -p dalker-platform --example make_icons --release

use std::path::Path;

/// Sizes in the .ico: what Explorer, the taskbar and Add/Remove Programs ask for at
/// 100-200% scaling.
const ICO_SIZES: [usize; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
/// Sizes of the hicolor theme's PNG icons on Linux.
const PNG_SIZES: [usize; 7] = [16, 24, 32, 48, 64, 128, 256];

fn png(size: usize) -> Vec<u8> {
    let px = dalker_platform::icon::rgba_sized(dalker_platform::icon::Look::Normal, size);
    let img = image::RgbaImage::from_raw(size as u32, size as u32, px).expect("size");
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .expect("png");
    out.into_inner()
}

fn main() -> std::io::Result<()> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/branding");
    std::fs::create_dir_all(dir.join("png"))?;
    for size in PNG_SIZES {
        std::fs::write(dir.join(format!("png/dalker-{size}.png")), png(size))?;
    }
    // An .ico holding PNG images (Windows Vista and later read those at every size).
    let images: Vec<(usize, Vec<u8>)> = ICO_SIZES.iter().map(|&s| (s, png(s))).collect();
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len();
    for (size, data) in &images {
        let side = if *size >= 256 { 0 } else { *size as u8 }; // 0 means 256
        ico.extend_from_slice(&[side, side, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
        ico.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in &images {
        ico.extend_from_slice(data);
    }
    std::fs::write(dir.join("dalker.ico"), ico)?;
    println!("wrote {}", dir.canonicalize()?.display());
    Ok(())
}
