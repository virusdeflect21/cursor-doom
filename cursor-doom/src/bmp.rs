//! Tiny BMP writer used by `--selftest` / `--probe` to dump frames for
//! inspection (no image crate needed).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// Writes a 24-bit BMP from a BGRA buffer (alpha ignored), top-down input.
pub fn write_bgra24(
    path: &Path,
    pixels: &[u8],
    width: usize,
    height: usize,
) -> std::io::Result<()> {
    let row_raw = width * 3;
    let row_padded = (row_raw + 3) & !3;
    let image_size = row_padded * height;
    let file_size = 14 + 40 + image_size;

    let file = File::create(path)?;
    let mut out = BufWriter::new(file);

    // BITMAPFILEHEADER
    out.write_all(b"BM")?;
    out.write_all(&(file_size as u32).to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?; // reserved
    out.write_all(&54u32.to_le_bytes())?; // data offset
                                          // BITMAPINFOHEADER
    out.write_all(&40u32.to_le_bytes())?;
    out.write_all(&(width as i32).to_le_bytes())?;
    out.write_all(&(height as i32).to_le_bytes())?;
    out.write_all(&1u16.to_le_bytes())?; // planes
    out.write_all(&24u16.to_le_bytes())?; // bpp
    out.write_all(&0u32.to_le_bytes())?; // BI_RGB
    out.write_all(&(image_size as u32).to_le_bytes())?;
    out.write_all(&2835i32.to_le_bytes())?;
    out.write_all(&2835i32.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?;

    let mut row = vec![0u8; row_padded];
    for y in (0..height).rev() {
        for x in 0..width {
            let src = (y * width + x) * 4;
            if src + 3 < pixels.len() {
                row[x * 3] = pixels[src];
                row[x * 3 + 1] = pixels[src + 1];
                row[x * 3 + 2] = pixels[src + 2];
            }
        }
        out.write_all(&row)?;
    }
    out.flush()
}

/// Writes a 24-bit BMP from an RGBA buffer (alpha ignored), top-down input.
/// The launcher's screenshots are saved as BMP: lossless, dependency free, and
/// the release script converts them to PNG.
pub fn write_rgba24(
    path: &Path,
    pixels: &[u8],
    width: usize,
    height: usize,
) -> std::io::Result<()> {
    let mut bgra = Vec::with_capacity(width * height * 4);
    for i in (0..pixels.len() / 4).map(|i| i * 4) {
        bgra.extend_from_slice(&[pixels[i + 2], pixels[i + 1], pixels[i], 255]);
    }
    write_bgra24(path, &bgra, width, height)
}
