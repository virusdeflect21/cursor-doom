//! Hiding the Windows cursor while Doom *is* the cursor.
//!
//! The overlay window is click-through, so the real pointer keeps working; the
//! only thing we replace is the *image* of the pointer. `SetSystemCursor` swaps
//! every system cursor for a fully transparent one, and
//! `SystemParametersInfo(SPI_SETCURSORS)` restores the user's scheme.
//!
//! Restoring must survive crashes as far as possible: the hider restores on
//! drop, and the launcher always restores on startup.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, CreateCursor, DestroyCursor, GetCursorInfo, GetIconInfo, LoadCursorW,
    SetSystemCursor, SystemParametersInfoW, CURSORINFO, CURSOR_SHOWING, HCURSOR, HICON, ICONINFO,
    OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_NO, OCR_NORMAL, OCR_SIZEALL,
    OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT, SPIF_SENDCHANGE,
    SPI_SETCURSORS, SYSTEM_CURSOR_ID,
};

/// Every cursor role we blank out, so text fields / resize borders do not give
/// the replacement away.
const ROLES: &[SYSTEM_CURSOR_ID] = &[
    OCR_NORMAL,
    OCR_IBEAM,
    OCR_WAIT,
    OCR_CROSS,
    OCR_UP,
    OCR_SIZENWSE,
    OCR_SIZENESW,
    OCR_SIZEWE,
    OCR_SIZENS,
    OCR_SIZEALL,
    OCR_NO,
    OCR_HAND,
    OCR_APPSTARTING,
    OCR_HELP,
];

static HIDDEN: AtomicBool = AtomicBool::new(false);
static BACKED_UP: AtomicBool = AtomicBool::new(false);

use crate::scheme;

fn werr(context: &str, err: windows::core::Error) -> String {
    format!(
        "{context}: {} (0x{:08X})",
        err.message(),
        err.code().0 as u32
    )
}

/// Stock Windows cursors, used as the floor: if the stored scheme itself has
/// been poisoned with blank cursors, a plain standard arrow beats no pointer.
const STOCK: &[(SYSTEM_CURSOR_ID, u16)] = &[
    (OCR_NORMAL, 32512),      // IDC_ARROW
    (OCR_IBEAM, 32513),       // IDC_IBEAM
    (OCR_WAIT, 32514),        // IDC_WAIT
    (OCR_CROSS, 32515),       // IDC_CROSS
    (OCR_UP, 32516),          // IDC_UPARROW
    (OCR_SIZENWSE, 32642),    // IDC_SIZENWSE
    (OCR_SIZENESW, 32643),    // IDC_SIZENESW
    (OCR_SIZEWE, 32644),      // IDC_SIZEWE
    (OCR_SIZENS, 32645),      // IDC_SIZENS
    (OCR_SIZEALL, 32646),     // IDC_SIZEALL
    (OCR_NO, 32648),          // IDC_NO
    (OCR_HAND, 32649),        // IDC_HAND
    (OCR_APPSTARTING, 32650), // IDC_APPSTARTING
    (OCR_HELP, 32651),        // IDC_HELP
];

/// Installs the stock Windows cursors for every role we blank. Returns how many
/// roles were replaced.
fn restore_stock_cursors() -> usize {
    let mut restored = 0usize;
    for (role, id) in STOCK {
        let resource = PCWSTR(*id as usize as *const u16);
        let Ok(stock) = (unsafe { LoadCursorW(None, resource) }) else {
            continue;
        };
        // SetSystemCursor consumes the handle, so hand it a copy.
        let Ok(copy) = (unsafe { CopyIcon(HICON(stock.0)) }) else {
            continue;
        };
        if unsafe { SetSystemCursor(HCURSOR(copy.0), *role) }.is_ok() {
            restored += 1;
        }
    }
    restored
}

/// True when the pointer Windows is about to draw has visible pixels.
fn pointer_is_visible() -> Option<bool> {
    capture_current_cursor().map(|image| image.coverage() > 0.004)
}

/// Reinstalls the user's cursor scheme and proves it worked.
///
/// `SetSystemCursor` overwrites the *stored* scheme, so a bare
/// `SPI_SETCURSORS` can bring the blank cursors right back - the reason this
/// function restores the backed-up scheme first and then checks the result with
/// the same code `--cursor-dump` uses, falling back to the stock cursors if the
/// pointer is still invisible.
pub fn restore_system_cursors() {
    // 1. the user's own values, saved before anything was blanked
    let scheme_restored = scheme::restore_backup();

    // 2. let Windows rebuild the cursors from those values
    unsafe {
        let _ = SystemParametersInfoW(SPI_SETCURSORS, 0, None, SPIF_SENDCHANGE);
    }

    if pointer_is_visible() == Some(true) {
        HIDDEN.store(false, Ordering::SeqCst);
        return;
    }

    // 3. still blank: the stored scheme itself is poisoned, so put the stock
    //    cursors back role by role and check again
    for attempt in 0..6 {
        restore_stock_cursors();
        unsafe {
            let _ = SystemParametersInfoW(SPI_SETCURSORS, 0, None, SPIF_SENDCHANGE);
        }
        if pointer_is_visible() != Some(false) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20 + attempt * 20));
    }
    if !scheme_restored {
        let _ = scheme::save_backup();
    }
    HIDDEN.store(false, Ordering::SeqCst);
}

/// Is the cursor currently displayed by the system completely transparent?
///
/// `SetSystemCursor` replaces the *contents* of the shared cursor objects while
/// keeping their handles, so the only reliable check is to look at the cursor's
/// own AND mask (all ones + no colour bitmap = invisible). `None` means the
/// check could not be run in this environment.
#[derive(Debug, Clone, Copy)]
pub struct CursorReport {
    pub handle: isize,
    pub showing: bool,
    pub monochrome: bool,
    pub mask_all_ones: bool,
    pub width: i32,
    pub height: i32,
}

impl CursorReport {
    pub fn is_blank(&self) -> bool {
        self.showing && self.monochrome && self.mask_all_ones
    }
}

pub fn cursor_report() -> Option<CursorReport> {
    unsafe {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        GetCursorInfo(&mut info).ok()?;
        if info.hCursor.is_invalid() {
            return None;
        }
        let showing = (info.flags.0 & CURSOR_SHOWING.0) != 0;

        let mut icon = ICONINFO::default();
        GetIconInfo(HICON(info.hCursor.0), &mut icon).ok()?;
        if icon.hbmMask.is_invalid() {
            return None;
        }

        // Monochrome cursors (ours) have no colour bitmap at all.
        let monochrome = icon.hbmColor.is_invalid();

        let mut bitmap = BITMAP::default();
        let read = GetObjectW(
            HGDIOBJ(icon.hbmMask.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bitmap as *mut _ as *mut core::ffi::c_void),
        );
        if read == 0 || bitmap.bmWidth <= 0 || bitmap.bmHeight <= 0 {
            let _ = DeleteObject(HGDIOBJ(icon.hbmMask.0));
            return None;
        }

        let stride = ((bitmap.bmWidth as usize).div_ceil(8) + 1) & !1;
        let mut bits = vec![0u8; stride * bitmap.bmHeight as usize];
        let mut dib = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: bitmap.bmWidth,
                // negative height = top-down rows, so the AND plane comes first
                biHeight: -bitmap.bmHeight,
                biPlanes: 1,
                biBitCount: 1,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let dc = GetDC(None);
        let lines = GetDIBits(
            dc,
            windows::Win32::Graphics::Gdi::HBITMAP(icon.hbmMask.0),
            0,
            bitmap.bmHeight as u32,
            Some(bits.as_mut_ptr() as *mut core::ffi::c_void),
            &mut dib,
            DIB_RGB_COLORS,
        );
        let _ = ReleaseDC(None, dc);
        let _ = DeleteObject(HGDIOBJ(icon.hbmMask.0));
        if !icon.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon.hbmColor.0));
        }
        if lines == 0 {
            return None;
        }

        // A monochrome cursor bitmap stacks the AND mask on top of the XOR mask,
        // so it is twice as tall as the cursor. Invisible == AND all ones (keep
        // the screen) and XOR all zeros (draw nothing).
        let mask_all_ones = if monochrome && bits.len().is_multiple_of(2) {
            let half = bits.len() / 2;
            let keep = bits[..half].iter().all(|byte| *byte == 0xFF)
                && bits[half..].iter().all(|byte| *byte == 0x00);
            let flipped = bits[..half].iter().all(|byte| *byte == 0x00)
                && bits[half..].iter().all(|byte| *byte == 0xFF);
            keep || flipped
        } else {
            bits.iter().all(|byte| *byte == 0xFF)
        };

        Some(CursorReport {
            handle: info.hCursor.0 as isize,
            showing,
            monochrome,
            mask_all_ones,
            width: bitmap.bmWidth,
            height: bitmap.bmHeight / if monochrome { 2 } else { 1 },
        })
    }
}

/// The pointer's bitmap, decoded to straight-alpha RGBA (top-down rows).
///
/// Used by `--cursor-dump` so the pointer can be inspected: during a session it
/// comes out empty, after a restore it is the user's real arrow again.
pub struct CursorImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl CursorImage {
    /// A coarse terminal rendering (` .:-=+*#%@`), handy without screenshots.
    pub fn ink_map(&self, cols: usize, rows: usize) -> Vec<String> {
        let ramp = b" .:-=+*#%@";
        let mut out = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut line = String::with_capacity(cols);
            for col in 0..cols {
                let x0 = col * self.width / cols;
                let x1 = ((col + 1) * self.width / cols).max(x0 + 1);
                let y0 = row * self.height / rows;
                let y1 = ((row + 1) * self.height / rows).max(y0 + 1);
                let mut covered = 0usize;
                let mut total = 0usize;
                for y in y0..y1.min(self.height) {
                    for x in x0..x1.min(self.width) {
                        total += 1;
                        if self.rgba[(y * self.width + x) * 4 + 3] > 8 {
                            covered += 1;
                        }
                    }
                }
                let share = if total == 0 {
                    0.0
                } else {
                    covered as f32 / total as f32
                };
                let idx = ((share * (ramp.len() - 1) as f32).round() as usize).min(ramp.len() - 1);
                line.push(ramp[idx] as char);
            }
            out.push(line);
        }
        out
    }

    /// Alpha-composites the pointer over a checkerboard, as BGRA rows for the BMP writer.
    pub fn to_bgra_on_checkerboard(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.width * self.height * 4];
        for y in 0..self.height {
            for x in 0..self.width {
                let i = (y * self.width + x) * 4;
                let a = self.rgba[i + 3] as u32;
                let checker = if ((x / 8) + (y / 8)) % 2 == 0 {
                    0x30
                } else {
                    0x60
                };
                for (c, bg) in [checker, checker + 0x10, checker + 0x28].iter().enumerate() {
                    let src = self.rgba[i + c] as u32;
                    out[i + c] = ((src * a + bg * (255 - a)) / 255) as u8;
                }
                out[i + 3] = 255;
            }
        }
        out
    }

    /// Fraction of pixels that would actually be drawn.
    pub fn coverage(&self) -> f32 {
        let opaque = (0..self.rgba.len() / 4)
            .filter(|i| self.rgba[i * 4 + 3] > 8)
            .count();
        opaque as f32 / (self.width * self.height).max(1) as f32
    }
}

/// Reads the current pointer bitmap. `None` when Windows reports no cursor or the
/// bitmaps cannot be read.
pub fn capture_current_cursor() -> Option<CursorImage> {
    unsafe {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        GetCursorInfo(&mut info).ok()?;
        if info.flags.0 & CURSOR_SHOWING.0 == 0 || info.hCursor.is_invalid() {
            return None;
        }

        let mut icon = ICONINFO::default();
        GetIconInfo(HICON(info.hCursor.0), &mut icon).ok()?;

        let image = read_cursor_bitmaps(&icon);
        if !icon.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon.hbmMask.0));
        }
        if !icon.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon.hbmColor.0));
        }
        image
    }
}

unsafe fn read_cursor_bitmaps(icon: &ICONINFO) -> Option<CursorImage> {
    let mut mask_bitmap = BITMAP::default();
    let read = unsafe {
        GetObjectW(
            HGDIOBJ(icon.hbmMask.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut mask_bitmap as *mut _ as *mut core::ffi::c_void),
        )
    };
    if read == 0 || mask_bitmap.bmWidth <= 0 {
        return None;
    }

    let mut colour_bitmap = BITMAP::default();
    let has_colour = !icon.hbmColor.is_invalid()
        && unsafe {
            GetObjectW(
                HGDIOBJ(icon.hbmColor.0),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut colour_bitmap as *mut _ as *mut core::ffi::c_void),
            )
        } != 0
        && colour_bitmap.bmWidth > 0
        && colour_bitmap.bmHeight > 0;

    let width = mask_bitmap.bmWidth as usize;
    let height = if has_colour {
        colour_bitmap.bmHeight as usize
    } else {
        mask_bitmap.bmHeight as usize / 2
    };
    if width == 0 || height == 0 {
        return None;
    }

    // AND mask: 1bpp, DWORD-aligned rows, read top-down.
    let mask_stride = width.div_ceil(32) * 4;
    let mut mask = vec![0u8; mask_stride * mask_bitmap.bmHeight as usize];
    let mut mask_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: mask_bitmap.bmWidth,
            biHeight: -mask_bitmap.bmHeight,
            biPlanes: 1,
            biBitCount: 1,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let dc = unsafe { GetDC(None) };
    let mask_lines = unsafe {
        GetDIBits(
            dc,
            windows::Win32::Graphics::Gdi::HBITMAP(icon.hbmMask.0),
            0,
            mask_bitmap.bmHeight as u32,
            Some(mask.as_mut_ptr() as *mut core::ffi::c_void),
            &mut mask_info,
            DIB_RGB_COLORS,
        )
    };
    if mask_lines == 0 {
        let _ = unsafe { ReleaseDC(None, dc) };
        return None;
    }

    let mut colour = vec![0u8; width * height * 4];
    let mut have_colour = false;
    if has_colour {
        let mut colour_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: colour_bitmap.bmWidth,
                biHeight: -colour_bitmap.bmHeight,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = unsafe {
            GetDIBits(
                dc,
                windows::Win32::Graphics::Gdi::HBITMAP(icon.hbmColor.0),
                0,
                colour_bitmap.bmHeight as u32,
                Some(colour.as_mut_ptr() as *mut core::ffi::c_void),
                &mut colour_info,
                DIB_RGB_COLORS,
            )
        };
        have_colour = lines != 0;
    }
    let _ = unsafe { ReleaseDC(None, dc) };

    let has_alpha = have_colour && (0..colour.len() / 4).any(|i| colour[i * 4 + 3] != 0);
    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 4;
            let mask_byte = mask.get(y * mask_stride + x / 8).copied().unwrap_or(0x00);
            // An AND bit set means "keep the screen": the pixel is not drawn.
            let keep = mask_byte & (0x80 >> (x % 8)) != 0;
            let (r, g, b, a) = if have_colour {
                let px = (colour[i + 2], colour[i + 1], colour[i]);
                if has_alpha {
                    (px.0, px.1, px.2, if keep { 0 } else { colour[i + 3] })
                } else if keep {
                    (0, 0, 0, 0)
                } else {
                    (px.0, px.1, px.2, 255)
                }
            } else if keep {
                (0, 0, 0, 0)
            } else {
                // Monochrome cursor: the XOR plane says white or black.
                let xor_byte = mask
                    .get((height + y) * mask_stride + x / 8)
                    .copied()
                    .unwrap_or(0x00);
                let v = if xor_byte & (0x80 >> (x % 8)) != 0 {
                    255u8
                } else {
                    0u8
                };
                (v, v, v, 255)
            };
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = a;
        }
    }

    Some(CursorImage {
        width,
        height,
        rgba,
    })
}

pub fn current_cursor_is_blank() -> Option<bool> {
    cursor_report().map(|report| report.is_blank())
}

/// RAII guard: hides the system cursor, restores it when dropped.
pub struct CursorHider {
    active: bool,
}

/// Blanks every system cursor. Returns how many roles were replaced.
fn blank_system_cursors() -> Result<usize, String> {
    // `SetSystemCursor` overwrites the stored scheme, so keep a copy of the
    // user's real one before the first blanking of this process.
    if !BACKED_UP.swap(true, Ordering::SeqCst) && pointer_is_visible() != Some(false) {
        let _ = scheme::save_backup();
    }

    // A 32x32 monochrome cursor whose AND mask is all ones and XOR mask all
    // zeros is completely transparent.
    let and_mask = [0xFFu8; 32 * 32 / 8];
    let xor_mask = [0x00u8; 32 * 32 / 8];

    unsafe {
        let module = GetModuleHandleW(PCWSTR::null()).map_err(|e| werr("GetModuleHandleW", e))?;
        let hinst = HINSTANCE(module.0);
        let blank = CreateCursor(
            Some(hinst),
            0,
            0,
            32,
            32,
            and_mask.as_ptr() as *const core::ffi::c_void,
            xor_mask.as_ptr() as *const core::ffi::c_void,
        )
        .map_err(|e| werr("CreateCursor", e))?;

        let mut replaced = 0usize;
        for role in ROLES {
            // SetSystemCursor destroys the handle it is given, so hand it a
            // fresh copy every time.
            if let Ok(copy) = CopyIcon(HICON(blank.0)) {
                if SetSystemCursor(HCURSOR(copy.0), *role).is_ok() {
                    replaced += 1;
                }
            }
        }
        let _ = DestroyCursor(blank);

        if replaced == 0 {
            return Err("SetSystemCursor failed for every cursor role".into());
        }
        HIDDEN.store(true, Ordering::SeqCst);
        Ok(replaced)
    }
}

impl CursorHider {
    pub fn hide() -> Result<Self, String> {
        blank_system_cursors()?;
        Ok(Self { active: true })
    }

    /// Re-blanks the cursors. Windows (or another program calling
    /// `SPI_SETCURSORS`) can restore the scheme at any time, so the session
    /// re-applies this once a second to keep the pointer invisible.
    pub fn reapply(&self) -> bool {
        if !self.active {
            return false;
        }
        blank_system_cursors().is_ok()
    }
}

impl Drop for CursorHider {
    fn drop(&mut self) {
        if self.active {
            restore_system_cursors();
            self.active = false;
        }
    }
}
