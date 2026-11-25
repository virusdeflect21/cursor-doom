//! Live preview of what the cursor is showing, shared with the launcher.
//!
//! The session owns the pixels; the launcher wants to look at them big enough to
//! actually see. Passing them through a pipe would need an IPC layer, so they go
//! through a small file in the app data folder instead: a header with the three
//! sizes followed by the raw BGRA planes.
//!
//! Two planes travel:
//!
//! * `native` - the engine's own 320x200 framebuffer (what the game really is)
//! * `cursor` - the downscaled frame the cursor is painted with
//!
//! The third view the launcher offers ("on screen") is the cursor plane blown up
//! by the display scale, and it is computed in the launcher with the very same
//! `frame::build_display` the session uses, so it cannot drift from the real
//! cursor image.
//!
//! Writes go to a temporary file that is renamed over the real one, which is
//! atomic on NTFS: the launcher always reads a complete frame, never a half
//! written one.

use std::path::PathBuf;
use std::time::{Duration, Instant};

const MAGIC: [u8; 4] = *b"CDPV";
const VERSION: u32 = 2;
const HEADER: usize = 24;
const MAX_PIXELS: usize = 4096 * 4096;
/// A handful of frames per second is plenty for a viewer.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(100);

fn preview_path() -> PathBuf {
    crate::status::app_dir().join("preview.bin")
}

fn temp_path() -> PathBuf {
    crate::status::app_dir().join("preview.tmp")
}

/// One decoded snapshot: the engine's framebuffer and the cursor's pixels.
pub struct PreviewFrame {
    pub native_width: u32,
    pub native_height: u32,
    pub cursor_width: u32,
    pub cursor_height: u32,
    /// The engine's frame, `native_width * native_height * 4` BGRA bytes.
    pub native: Vec<u8>,
    /// The cursor frame, `cursor_width * cursor_height * 4` BGRA bytes.
    pub cursor: Vec<u8>,
}

/// Writes preview frames for the launcher (throttled, so it is cheap).
pub struct PreviewWriter {
    native_width: u32,
    native_height: u32,
    cursor_width: u32,
    cursor_height: u32,
    last: Instant,
    enabled: bool,
}

impl PreviewWriter {
    pub fn new(
        native_width: u32,
        native_height: u32,
        cursor_width: u32,
        cursor_height: u32,
    ) -> Self {
        let pixels = |w: u32, h: u32| w > 0 && h > 0 && (w as usize * h as usize) <= MAX_PIXELS;
        Self {
            native_width,
            native_height,
            cursor_width,
            cursor_height,
            // Ready immediately: the first frame should show up at once.
            last: Instant::now() - PREVIEW_INTERVAL,
            enabled: pixels(native_width, native_height) && pixels(cursor_width, cursor_height),
        }
    }

    /// Stores one snapshot if the throttle allows it.
    pub fn write(&mut self, native: &[u8], cursor: &[u8]) {
        if !self.enabled || self.last.elapsed() < PREVIEW_INTERVAL {
            return;
        }
        let native_bytes = (self.native_width as usize) * (self.native_height as usize) * 4;
        let cursor_bytes = (self.cursor_width as usize) * (self.cursor_height as usize) * 4;
        if native.len() < native_bytes || cursor.len() < cursor_bytes {
            return;
        }
        self.last = Instant::now();

        let mut bytes = Vec::with_capacity(HEADER + native_bytes + cursor_bytes);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.native_width.to_le_bytes());
        bytes.extend_from_slice(&self.native_height.to_le_bytes());
        bytes.extend_from_slice(&self.cursor_width.to_le_bytes());
        bytes.extend_from_slice(&self.cursor_height.to_le_bytes());
        bytes.extend_from_slice(&native[..native_bytes]);
        bytes.extend_from_slice(&cursor[..cursor_bytes]);

        let tmp = temp_path();
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, preview_path());
        }
    }

    /// Removes the preview so a closed session never looks like a live one.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(preview_path());
        let _ = std::fs::remove_file(temp_path());
    }
}

/// Reads the latest snapshot written by a running session.
pub fn read() -> Option<PreviewFrame> {
    let bytes = std::fs::read(preview_path()).ok()?;
    parse(&bytes)
}

/// Decodes a preview buffer. Kept separate from [`read`] so it can be tested
/// without touching `%APPDATA%`.
pub fn parse(bytes: &[u8]) -> Option<PreviewFrame> {
    if bytes.len() < HEADER || bytes[..4] != MAGIC {
        return None;
    }
    if u32::from_le_bytes(bytes[4..8].try_into().ok()?) != VERSION {
        return None;
    }
    let native_width = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let native_height = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
    let cursor_width = u32::from_le_bytes(bytes[16..20].try_into().ok()?);
    let cursor_height = u32::from_le_bytes(bytes[20..24].try_into().ok()?);

    let native_bytes = (native_width as usize)
        .checked_mul(native_height as usize)?
        .checked_mul(4)?;
    let cursor_bytes = (cursor_width as usize)
        .checked_mul(cursor_height as usize)?
        .checked_mul(4)?;
    if native_bytes == 0
        || cursor_bytes == 0
        || native_bytes > MAX_PIXELS * 4
        || cursor_bytes > MAX_PIXELS * 4
    {
        return None;
    }
    let end = HEADER + native_bytes + cursor_bytes;
    if bytes.len() < end {
        return None;
    }

    Some(PreviewFrame {
        native_width,
        native_height,
        cursor_width,
        cursor_height,
        native: bytes[HEADER..HEADER + native_bytes].to_vec(),
        cursor: bytes[HEADER + native_bytes..end].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(native: (u32, u32), cursor: (u32, u32)) -> Vec<u8> {
        let native_bytes = (native.0 * native.1 * 4) as usize;
        let cursor_bytes = (cursor.0 * cursor.1 * 4) as usize;
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&native.0.to_le_bytes());
        out.extend_from_slice(&native.1.to_le_bytes());
        out.extend_from_slice(&cursor.0.to_le_bytes());
        out.extend_from_slice(&cursor.1.to_le_bytes());
        out.resize(HEADER, 0);
        out.extend(std::iter::repeat_n(0x11u8, native_bytes));
        out.extend(std::iter::repeat_n(0x22u8, cursor_bytes));
        out
    }

    #[test]
    fn parses_both_planes() {
        let bytes = buffer((320, 200), (16, 16));
        let frame = parse(&bytes).expect("valid buffer");
        assert_eq!((frame.native_width, frame.native_height), (320, 200));
        assert_eq!((frame.cursor_width, frame.cursor_height), (16, 16));
        assert_eq!(frame.native.len(), 320 * 200 * 4);
        assert_eq!(frame.cursor.len(), 16 * 16 * 4);
        assert!(frame.native.iter().all(|b| *b == 0x11));
        assert!(frame.cursor.iter().all(|b| *b == 0x22));
    }

    #[test]
    fn rejects_truncated_or_foreign_buffers() {
        let mut bytes = buffer((320, 200), (16, 16));
        assert!(parse(&bytes[..bytes.len() - 4]).is_none());
        bytes[0] = b'X';
        assert!(parse(&bytes).is_none());
        assert!(parse(b"short").is_none());
    }
}
