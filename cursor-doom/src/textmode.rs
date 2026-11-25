//! Detects when the user is typing somewhere.
//!
//! The doom cursor steals WASD/arrows from the whole desktop, which is exactly
//! what you want while playing and exactly what you do *not* want while typing in
//! a browser field, a terminal or an editor. Two cheap signals tell us the user
//! is in a text context:
//!
//! * the foreground window's thread owns a blinking caret (`GetGUIThreadInfo`)
//! * the pointer shows the I-beam cursor (`IDC_IBEAM`)
//!
//! Either one pauses the session: the game stops ticking, the overlay hides, the
//! normal pointer comes back and the keyboard is no longer captured. Both
//! disappear -> the doom cursor returns where it left off.

use std::mem::size_of;

use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetWindowRect,
    GetWindowThreadProcessId, LoadCursorW, CURSORINFO, GUITHREADINFO, IDC_IBEAM,
};

/// Why the session should behave as if the user is typing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Typing {
    /// No text context: the doom cursor can swallow the arrows and Ctrl again.
    No,
    /// The focused control in the foreground window has a blinking caret.
    Caret,
    /// The pointer is over something that shows the text cursor.
    IBeam,
    /// A burst of typed characters: the user is writing somewhere.
    Burst,
}

impl Typing {
    pub fn active(self) -> bool {
        self != Typing::No
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Typing::No => "",
            Typing::Caret => "caret",
            Typing::IBeam => "i-beam",
            Typing::Burst => "keystrokes",
        }
    }
}

/// Looks at the pointer shape first, then at the caret of the focused control.
///
/// Cheap enough to call a few times per second: it is one `GetCursorInfo`, plus
/// a window query when the pointer is over the foreground window.
pub fn probe() -> Typing {
    unsafe {
        let mut cursor = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut cursor).is_ok() && cursor.flags.0 != 0 {
            // The pointer itself shows a text cursor: this is the case the user
            // asked for - you are hovering a field you can type into.
            if let Ok(ibeam) = LoadCursorW(None, IDC_IBEAM) {
                if cursor.hCursor == ibeam {
                    return Typing::IBeam;
                }
            }
        }

        // Some editors draw their own text cursor. Fall back to the caret, but
        // only while the pointer is over the window that owns it, so a focused
        // editor in the background does not pause the game.
        let foreground = GetForegroundWindow();
        if foreground.0.is_null() {
            return Typing::No;
        }
        let mut pid = 0u32;
        let thread = GetWindowThreadProcessId(foreground, Some(&mut pid));
        if thread == 0 || pid == std::process::id() {
            return Typing::No;
        }
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        // `hwndCaret` is only set while a text control has keyboard focus, and
        // unlike `GUI_CARETBLINKING` it does not flicker with the caret blink.
        if GetGUIThreadInfo(thread, &mut info).is_err() || info.hwndCaret.0.is_null() {
            return Typing::No;
        }

        let mut pointer = windows::Win32::Foundation::POINT::default();
        if GetCursorPos(&mut pointer).is_err() {
            return Typing::No;
        }
        let mut rect = RECT::default();
        if GetWindowRect(info.hwndCaret, &mut rect).is_err() {
            return Typing::No;
        }
        let inside = pointer.x >= rect.left
            && pointer.x < rect.right
            && pointer.y >= rect.top
            && pointer.y < rect.bottom;
        if inside {
            Typing::Caret
        } else {
            Typing::No
        }
    }
}
