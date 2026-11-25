//! Global low-level keyboard/mouse hooks.
//!
//! The doom cursor never has focus (it is a click-through overlay), so game
//! input has to be observed system-wide:
//!
//! * `WH_KEYBOARD_LL` - feeds Doom and swallows only the arrows and Ctrl, so
//!   every other key keeps working everywhere else.
//! * `WH_MOUSE_LL` - swallows the middle click, which is the "turn the doom
//!   cursor off" bind.
//!
//! The hooks run on a **dedicated thread with its own message pump**. Windows
//! delivers low-level hooks to the thread that installed them and blocks input
//! system-wide if that thread does not answer quickly; keeping them off the
//! render thread means a slow engine tic (loading a level, a big cache rebuild)
//! can never freeze the keyboard and mouse.
//!
//! Windows also *silently drops* low-level hooks in a few situations (a stalled
//! callback, a desktop switch, a secure-desktop prompt). The session therefore
//! asks for a [`InputHooks::rearm`] every few seconds, which re-installs both
//! hooks; `key_events` / `keys_fed` / `hook_ticks` in the session status show
//! what the hooks are seeing.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::keys;

const VK_F8: u32 = 0x77;
const VK_F9: u32 = 0x78;

/// Private message that stops the hook thread.
const WM_HOOKS_STOP: u32 = WM_APP + 0x1d;
/// Private message asking the hook thread to re-install the hooks.
const WM_HOOKS_REARM: u32 = WM_APP + 0x1e;

/// How long one typed character keeps the keyboard away from the game.
const TYPING_SUSPEND_MS: u64 = 1800;

#[derive(Clone, Copy, Debug)]
pub enum HookEvent {
    /// A key the engine should see (doom key code).
    Key { pressed: bool, key: u8 },
    /// Middle mouse button: leave cursor mode.
    MiddleClick,
    /// F9: start/stop the MP4 recording.
    RecordToggle,
    /// F8: freeze/unfreeze the game while keeping the doom cursor on screen.
    PauseToggle,
}

static EVENTS: Mutex<VecDeque<HookEvent>> = Mutex::new(VecDeque::new());
/// `false` while the user is typing or the game is paused: keys travel through
/// untouched and are not fed to the engine.
static CAPTURE: AtomicBool = AtomicBool::new(true);
/// How many Doom key events the hooks have produced (diagnostics: proves that a
/// key really was taken from the desktop and handed to the game).
static KEYS_FED: AtomicU64 = AtomicU64::new(0);
/// How many keyboard events the hook has *seen* at all. Comparing this with
/// `keys_fed` tells whether a key was left to the application on purpose.
static KEY_EVENTS: AtomicU64 = AtomicU64::new(0);
/// Heartbeat of the hook thread's message loop.
static HOOK_TICKS: AtomicU64 = AtomicU64::new(0);
/// Until when a typed character keeps capture suspended (ms since start).
static TYPING_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
static STARTED: Mutex<Option<std::time::Instant>> = Mutex::new(None);

pub fn next_event() -> Option<HookEvent> {
    EVENTS.lock().ok()?.pop_front()
}

/// Lets the game take arrows / Ctrl (or give them back).
pub fn set_capture(on: bool) {
    CAPTURE.store(on, Ordering::Relaxed);
}

pub fn capture() -> bool {
    CAPTURE.load(Ordering::Relaxed)
}

/// Number of Doom key events fed to the engine so far.
pub fn keys_fed() -> u64 {
    KEYS_FED.load(Ordering::Relaxed)
}

/// Number of keyboard events the hook has observed (any key, press or release).
pub fn key_events() -> u64 {
    KEY_EVENTS.load(Ordering::Relaxed)
}

/// Progress of the hook thread's message pump (0 means it never started).
pub fn hook_ticks() -> u64 {
    HOOK_TICKS.load(Ordering::Relaxed)
}

/// `true` while typing keeps the keyboard away from the game.
pub fn typing_burst() -> bool {
    now_ms() < TYPING_UNTIL_MS.load(Ordering::Relaxed)
}

fn now_ms() -> u64 {
    let Ok(mut started) = STARTED.lock() else {
        return 0;
    };
    let start = *started.get_or_insert_with(std::time::Instant::now);
    start.elapsed().as_millis() as u64
}

fn start_typing_burst() {
    TYPING_UNTIL_MS.store(now_ms() + TYPING_SUSPEND_MS, Ordering::Relaxed);
}

/// Keys that mean "a human is typing words" rather than "a player is playing".
fn is_typing_key(vk: u32) -> bool {
    matches!(
        vk,
        // letters, digits, backspace, enter, numpad, punctuation, space
        0x30..=0x39 | 0x41..=0x5a | 0x08 | 0x0d | 0x6a..=0x6f | 0xba..=0xc0 | 0xdb..=0xdf | 0x20
    )
}

fn push(event: HookEvent) {
    if let Ok(mut queue) = EVENTS.lock() {
        if queue.len() < 256 {
            queue.push_back(event);
        }
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wp, lp);
    }

    // NOTE: injected events are deliberately *not* filtered out - remote desktop,
    // virtual machines and key remappers all report input as injected, and we
    // never synthesise input ourselves so there is no feedback loop to guard
    // against.
    let info = &*(lp.0 as *const KBDLLHOOKSTRUCT);

    let message = wp.0 as u32;
    let pressed = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let released = message == WM_KEYUP || message == WM_SYSKEYUP;
    if !pressed && !released {
        return CallNextHookEx(None, code, wp, lp);
    }
    let seen = KEY_EVENTS.fetch_add(1, Ordering::Relaxed) + 1;
    if seen <= 8 {
        eprintln!("[hooks] key event {seen}: vk={:#x}", info.vkCode);
    }

    // Session switches first: they work even while paused or while typing.
    match info.vkCode {
        VK_F8 => {
            if pressed {
                push(HookEvent::PauseToggle);
            }
            return LRESULT(1);
        }
        VK_F9 => {
            if pressed {
                push(HookEvent::RecordToggle);
            }
            return LRESULT(1);
        }
        _ => {}
    }

    // A printable key the game does not use is proof of typing (many apps draw
    // their own cursor and own no Win32 caret). That only pauses the game - the
    // key itself is never touched.
    let mapped = keys::map_vk(info.vkCode);
    if pressed && mapped.is_none() && is_typing_key(info.vkCode) {
        start_typing_burst();
    }

    let capture = CAPTURE.load(Ordering::Relaxed);
    if let Some(mapping) = mapped {
        if capture {
            for key in mapping.keys() {
                KEYS_FED.fetch_add(1, Ordering::Relaxed);
                push(HookEvent::Key { pressed, key: *key });
            }
        }
        if capture && mapping.swallow {
            return LRESULT(1);
        }
    }

    CallNextHookEx(None, code, wp, lp)
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wp, lp);
    }

    let _info = &*(lp.0 as *const MSLLHOOKSTRUCT);

    match wp.0 as u32 {
        WM_MBUTTONDOWN => {
            push(HookEvent::MiddleClick);
            return LRESULT(1);
        }
        WM_MBUTTONUP | WM_MBUTTONDBLCLK => return LRESULT(1),
        _ => {}
    }

    CallNextHookEx(None, code, wp, lp)
}

/// Runs on the hook thread: installs both hooks, reports the thread id to the
/// session, then pumps messages until asked to stop.
unsafe fn hook_thread(sender: mpsc::Sender<Result<u32, String>>) {
    let hinst = match GetModuleHandleW(PCWSTR::null()) {
        Ok(module) => HINSTANCE(module.0),
        Err(err) => {
            let _ = sender.send(Err(format!("GetModuleHandleW failed: {err}")));
            return;
        }
    };

    // Make sure this thread owns a message queue before installing the hooks.
    let mut msg = MSG::default();
    let _ = PeekMessageW(
        &mut msg,
        None,
        0,
        0,
        windows::Win32::UI::WindowsAndMessaging::PM_NOREMOVE,
    );

    let mut keyboard = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(hinst), 0)
    {
        Ok(hook) => hook,
        Err(err) => {
            let _ = sender.send(Err(format!(
                "SetWindowsHookEx(WH_KEYBOARD_LL) failed: {err}"
            )));
            return;
        }
    };
    let mut mouse = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(hinst), 0) {
        Ok(hook) => hook,
        Err(err) => {
            let _ = UnhookWindowsHookEx(keyboard);
            let _ = sender.send(Err(format!("SetWindowsHookEx(WH_MOUSE_LL) failed: {err}")));
            return;
        }
    };

    if let Ok(mut queue) = EVENTS.lock() {
        queue.clear();
    }
    let thread_id = GetCurrentThreadId();
    let _ = sender.send(Ok(thread_id));

    loop {
        let result = GetMessageW(&mut msg, None, 0, 0).0;
        HOOK_TICKS.fetch_add(1, Ordering::Relaxed);
        if result <= 0 {
            break;
        }
        if msg.message == WM_HOOKS_STOP {
            break;
        }
        if msg.message == WM_HOOKS_REARM {
            // Windows can drop a low-level hook; re-installing is the only way
            // back and costs nothing.
            let _ = UnhookWindowsHookEx(keyboard);
            let _ = UnhookWindowsHookEx(mouse);
            match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(hinst), 0) {
                Ok(hook) => keyboard = hook,
                Err(err) => {
                    eprintln!("[hooks] could not re-arm the keyboard hook: {err}");
                    continue;
                }
            }
            match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(hinst), 0) {
                Ok(hook) => mouse = hook,
                Err(err) => eprintln!("[hooks] could not re-arm the mouse hook: {err}"),
            }
            continue;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    let _ = UnhookWindowsHookEx(mouse);
    let _ = UnhookWindowsHookEx(keyboard);
}

/// Installs both hooks on their own thread; stops and joins it on drop.
pub struct InputHooks {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

impl InputHooks {
    pub fn install() -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("cursor-doom-hooks".into())
            .spawn(move || unsafe { hook_thread(sender) })
            .map_err(|e| format!("could not start the hook thread: {e}"))?;

        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(thread_id)) => Ok(Self {
                thread: Some(thread),
                thread_id,
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                Err("the hook thread did not report back".into())
            }
        }
    }

    /// Asks the hook thread to re-install both hooks.
    pub fn rearm(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_HOOKS_REARM, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for InputHooks {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_HOOKS_STOP, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
