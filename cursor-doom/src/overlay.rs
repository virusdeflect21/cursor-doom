//! The layered, click-through window that follows the mouse pointer: this is
//! what makes Doom look like the cursor.
//!
//! * `WS_EX_LAYERED` + `UpdateLayeredWindow` for per-pixel drawing without a
//!   normal paint cycle.
//! * `WS_EX_TRANSPARENT` so every click, drag, wheel event and hover goes to
//!   whatever is underneath - the overlay never steals input.
//! * `WS_EX_TOOLWINDOW` + `WS_POPUP` keeps it out of the taskbar / alt-tab.
//! * `WS_EX_NOACTIVATE` + `WS_EX_TOPMOST` so it never takes focus and stays on
//!   top of fullscreen-ish windows.

use std::sync::Once;

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetWindowPos, ShowWindow,
    UpdateLayeredWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SW_HIDE, SW_SHOWNA,
    ULW_ALPHA, WM_CLOSE, WM_DESTROY, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const CLASS_NAME: &str = "CursorDoomOverlay";

fn werr(context: &str, err: windows::core::Error) -> String {
    format!(
        "{context}: {} (0x{:08X})",
        err.message(),
        err.code().0 as u32
    )
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Set when someone closes the overlay (for example `taskkill /PID` without
/// `/F`, which sends `WM_CLOSE`), so the session can stop cleanly and restore
/// the pointer instead of being killed.
static CLOSE_REQUESTED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE | WM_DESTROY => CLOSE_REQUESTED.store(true, Ordering::SeqCst),
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// True once the overlay has been closed from the outside.
pub fn close_requested() -> bool {
    CLOSE_REQUESTED.load(Ordering::SeqCst)
}

pub struct Overlay {
    hwnd: HWND,
    mem_dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
}

// The raw DIB pointer is only touched from the session thread.
unsafe impl Send for Overlay {}

impl Overlay {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        static REGISTER: Once = Once::new();
        let mut class_error: Option<String> = None;

        unsafe {
            let module =
                GetModuleHandleW(PCWSTR::null()).map_err(|e| werr("GetModuleHandleW", e))?;
            let hinst = HINSTANCE(module.0);

            REGISTER.call_once(|| {
                let name = wide(CLASS_NAME);
                let class = WNDCLASSW {
                    style: Default::default(),
                    lpfnWndProc: Some(wnd_proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: hinst,
                    hIcon: Default::default(),
                    hCursor: Default::default(),
                    hbrBackground: Default::default(),
                    lpszMenuName: PCWSTR::null(),
                    lpszClassName: PCWSTR(name.as_ptr()),
                };
                let atom = RegisterClassW(&class);
                if atom == 0 {
                    class_error = Some(werr(
                        "RegisterClassW failed",
                        windows::core::Error::from_thread(),
                    ));
                }
            });

            if let Some(err) = class_error {
                return Err(err);
            }

            let name = wide(CLASS_NAME);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                PCWSTR(name.as_ptr()),
                PCWSTR(wide("Cursor Doom").as_ptr()),
                WS_POPUP,
                0,
                0,
                width as i32,
                height as i32,
                None,
                None,
                Some(hinst),
                None,
            )
            .map_err(|e| werr("CreateWindowExW failed", e))?;

            let mem_dc = CreateCompatibleDC(None);
            if mem_dc.is_invalid() {
                return Err(werr(
                    "CreateCompatibleDC failed",
                    windows::core::Error::from_thread(),
                ));
            }

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32), // top-down rows
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(mem_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                .map_err(|e| werr("CreateDIBSection failed", e))?;
            if bits.is_null() {
                return Err("CreateDIBSection returned no pixel memory".into());
            }
            let previous = SelectObject(mem_dc, HGDIOBJ(bitmap.0));

            Ok(Self {
                hwnd,
                mem_dc,
                bitmap,
                previous,
                bits: bits as *mut u8,
                width,
                height,
            })
        }
    }

    /// Pixel memory for `width * height` BGRA pixels with straight alpha.
    pub fn pixels(&mut self) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(self.bits, (self.width * self.height * 4) as usize)
        }
    }

    /// Raw window handle (reported in the session status for diagnostics).
    pub fn hwnd(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// Repositions the window without touching its pixels. Cheap enough to call
    /// on every mouse move, which is what keeps the doom view glued to the
    /// pointer instead of stepping at the render frame rate.
    pub fn move_to(&self, x: i32, y: i32) -> Result<(), String> {
        unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                self.width as i32,
                self.height as i32,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            )
            .map_err(|e| werr("SetWindowPos failed", e))
        }
    }

    /// Uploads the current pixels and places the window at `(x, y)`.
    pub fn present(&self, x: i32, y: i32) -> Result<(), String> {
        let dst = POINT { x, y };
        let size = SIZE {
            cx: self.width as i32,
            cy: self.height as i32,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };

        unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&dst),
                Some(&size),
                Some(self.mem_dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
            .map_err(|e| werr("UpdateLayeredWindow failed", e))
        }
    }

    /// Hides the overlay (used while the game is paused or the user is typing).
    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// Makes the overlay visible without activating it.
    pub fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
            // Keep it above everything; SWP_NOACTIVATE is implied by the style.
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
            );
        }
    }
}

/// Moves a bare window handle; used by the engine's idle callback, which must
/// not borrow the [`Overlay`] while the session loop owns it.
pub fn set_position(hwnd: isize, x: i32, y: i32, width: i32, height: i32) -> Result<(), String> {
    unsafe {
        SetWindowPos(
            HWND(hwnd as *mut _),
            Some(HWND_TOPMOST),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
        .map_err(|e| werr("SetWindowPos failed", e))
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = SelectObject(self.mem_dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.mem_dc);
        }
    }
}
