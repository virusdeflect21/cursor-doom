//! Turning the engine's 320x200 framebuffer into the cursor image.
//!
//! Doom renders at 320x200; the cursor shows a downscaled copy at the user's
//! resolution (<= 256x160), then scales that up for the screen if asked.

use crate::engine;

/// Nearest-neighbour downscaler 320x200 -> w x h, with pre-computed pixel maps.
pub struct Downscaler {
    dw: usize,
    dh: usize,
    xmap: Vec<usize>,
    ymap: Vec<usize>,
}

impl Downscaler {
    pub fn new(dw: usize, dh: usize) -> Self {
        let dw = dw.clamp(1, engine::WIDTH);
        let dh = dh.clamp(1, engine::HEIGHT);
        let xmap = (0..dw)
            .map(|x| (((x as f64) + 0.5) * engine::WIDTH as f64 / dw as f64) as usize)
            .map(|x| x.min(engine::WIDTH - 1))
            .collect();
        let ymap = (0..dh)
            .map(|y| (((y as f64) + 0.5) * engine::HEIGHT as f64 / dh as f64) as usize)
            .map(|y| y.min(engine::HEIGHT - 1))
            .collect();
        Self { dw, dh, xmap, ymap }
    }

    /// Downscale `src` (BGRA 320x200) into `dst` (w*h*4, alpha byte preserved
    /// from the engine, i.e. 0).
    pub fn run(&self, src: &[u8], dst: &mut [u8]) {
        if src.len() < engine::FRAME_BYTES || dst.len() < self.dw * self.dh * 4 {
            return;
        }
        for dy in 0..self.dh {
            let src_row = self.ymap[dy] * engine::WIDTH;
            let dst_row = dy * self.dw;
            for dx in 0..self.dw {
                let si = (src_row + self.xmap[dx]) * 4;
                let di = (dst_row + dx) * 4;
                dst[di..di + 4].copy_from_slice(&src[si..si + 4]);
            }
        }
    }
}

/// Scales the native cursor frame up to the on-screen size, forces full opacity
/// and optionally stamps a pointer reticle where the real mouse position is.
/// On-screen size of the cursor image for a given native size and scale.
pub fn display_size(nw: usize, nh: usize, scale_percent: u32) -> (usize, usize) {
    (
        ((nw * scale_percent as usize) / 100).max(1),
        ((nh * scale_percent as usize) / 100).max(1),
    )
}

pub fn build_display(
    native: &[u8],
    nw: usize,
    nh: usize,
    scale_percent: u32,
    reticle: Option<(usize, usize)>,
    out: &mut Vec<u8>,
) -> (usize, usize) {
    let (dw, dh) = display_size(nw, nh, scale_percent);
    out.clear();
    out.resize(dw * dh * 4, 0);
    scale_into(native, nw, nh, dw, dh, reticle, out);
    (dw, dh)
}

/// Same as [`build_display`] but writes straight into an existing buffer (the
/// overlay's pixel memory), avoiding a copy per frame.
pub fn scale_into(
    native: &[u8],
    nw: usize,
    nh: usize,
    dw: usize,
    dh: usize,
    reticle: Option<(usize, usize)>,
    out: &mut [u8],
) {
    if out.len() < dw * dh * 4 {
        return;
    }

    // Nearest-neighbour upscale with a forced alpha of 255 (doom's framebuffer
    // has an unused fourth byte).
    let mut xmap = Vec::with_capacity(dw);
    for x in 0..dw {
        xmap.push((x * nw / dw).min(nw.saturating_sub(1)));
    }
    for dy in 0..dh {
        let sy = (dy * nh / dh).min(nh.saturating_sub(1));
        let src_row = sy * nw * 4;
        let dst_row = dy * dw * 4;
        for (dx, sx) in xmap.iter().enumerate() {
            let si = src_row + sx * 4;
            let di = dst_row + dx * 4;
            if si + 4 <= native.len() {
                out[di] = native[si];
                out[di + 1] = native[si + 1];
                out[di + 2] = native[si + 2];
                out[di + 3] = 255;
            }
        }
    }

    if let Some((rx, ry)) = reticle {
        draw_reticle(out, dw, dh, rx, ry);
    }
}

/// A small crosshair so the actual pointer position is visible inside the doom
/// view: black outline + white core (readable on any background).
fn draw_reticle(buf: &mut [u8], w: usize, h: usize, cx: usize, cy: usize) {
    let arm = 5i64;
    let put = |buf: &mut [u8], x: i64, y: i64, r: u8, g: u8, b: u8| {
        if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
            return;
        }
        let i = (y as usize * w + x as usize) * 4;
        buf[i] = b;
        buf[i + 1] = g;
        buf[i + 2] = r;
        buf[i + 3] = 255;
    };

    let (cx, cy) = (cx as i64, cy as i64);
    // outline
    for d in -arm..=arm {
        put(buf, cx + d, cy, 0, 0, 0);
        put(buf, cx, cy + d, 0, 0, 0);
    }
    // core
    for d in -arm + 1..=arm - 1 {
        put(buf, cx + d, cy, 255, 255, 255);
        put(buf, cx, cy + d, 255, 255, 255);
    }
    put(buf, cx, cy, 255, 60, 60);
}
