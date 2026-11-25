//! Rust <-> doomgeneric glue.
//!
//! doomgeneric is a plain C game core that asks the "platform" for a handful of
//! callbacks (`DG_*`). This module implements those callbacks and owns the
//! argument vector the engine keeps pointers into.
//!
//! The engine is compiled for Doom's native 320x200 framebuffer; the cursor
//! layer (see `frame.rs`) downscales that to the user's chosen resolution.

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, CString};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Doom's internal framebuffer, fixed by build.rs (DOOMGENERIC_RESX/RESY).
pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 200;
pub const FRAME_BYTES: usize = WIDTH * HEIGHT * 4;

/// Doom runs its simulation at 35 tics/second; we render one frame per tic.
pub const FPS: u32 = 35;
pub const FRAME_TIME: Duration = Duration::from_nanos(1_000_000_000 / FPS as u64);

extern "C" {
    /// `pixel_t*` owned by the engine; 320*200 BGRA (little endian ARGB) pixels.
    static mut DG_ScreenBuffer: *mut u32;
    fn doomgeneric_Create(argc: c_int, argv: *mut *mut c_char);
    fn doomgeneric_Tick();
}

static KEYS: Mutex<VecDeque<(bool, u8)>> = Mutex::new(VecDeque::new());
static CLOCK: OnceLock<Instant> = OnceLock::new();
/// doomgeneric keeps the `char **argv` we hand it *for the whole run* (in C it
/// would be `main`'s argv on the stack). Both the strings and the pointer array
/// therefore have to outlive this call - a local `Vec` would be freed and the
/// engine would later read the dangling array (which crashed inside M_CheckParm).
static ARGV_STRINGS: Mutex<Vec<CString>> = Mutex::new(Vec::new());
static ARGV_POINTERS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// The engine derives its tics from `DG_GetTicksMs`, so pausing means freezing
/// that clock: doom then resumes exactly where it stopped instead of trying to
/// catch up on the tics it "missed" while the user was typing.
static PAUSED_SINCE: Mutex<Option<Instant>> = Mutex::new(None);
static PAUSED_TOTAL_MS: AtomicU64 = AtomicU64::new(0);

/// Work the session wants to do while the engine waits for the next tic:
/// pump the message queue and re-glue the overlay to the pointer.
static IDLE: Mutex<Option<fn()>> = Mutex::new(None);

/// Diagnostics: how long the last/longest `doomgeneric_Tick` took.
static TICK_LAST_MS: AtomicU64 = AtomicU64::new(0);
static TICK_MAX_MS: AtomicU64 = AtomicU64::new(0);
static TICK_SLOW: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// doomgeneric platform callbacks
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn DG_Init() {}

/// The engine just painted a frame; the session loop reads the framebuffer right
/// after `tick()` returns, so there is nothing to copy here.
#[no_mangle]
pub extern "C" fn DG_DrawFrame() {}

#[no_mangle]
pub extern "C" fn DG_SleepMs(ms: u32) {
    // doomgeneric sleeps 1 ms at a time while it waits for the next tic; use that
    // time to pump our hooks and to keep the overlay under the pointer, so the
    // cursor stays smooth even while the game is busy.
    if let Ok(idle) = IDLE.lock() {
        if let Some(callback) = *idle {
            callback();
        }
    }
    if ms > 0 {
        std::thread::sleep(Duration::from_millis(ms as u64));
    }
}

#[no_mangle]
pub extern "C" fn DG_GetTicksMs() -> u32 {
    let clock = CLOCK.get_or_init(Instant::now);
    let mut ms = clock.elapsed().as_millis() as u64;
    ms = ms.saturating_sub(PAUSED_TOTAL_MS.load(Ordering::Relaxed));
    if let Ok(since) = PAUSED_SINCE.lock() {
        if let Some(started) = *since {
            ms = ms.saturating_sub(started.elapsed().as_millis() as u64);
        }
    }
    ms as u32
}

/// # Safety
/// `pressed` / `key` must be valid, writable pointers (doomgeneric always passes
/// valid stack pointers).
#[no_mangle]
pub unsafe extern "C" fn DG_GetKey(pressed: *mut c_int, key: *mut u8) -> c_int {
    let Some((down, code)) = KEYS.lock().unwrap().pop_front() else {
        return 0;
    };
    if !pressed.is_null() {
        *pressed = i32::from(down);
    }
    if !key.is_null() {
        *key = code;
    }
    1
}

#[no_mangle]
pub extern "C" fn DG_SetWindowTitle(_title: *const c_char) {}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Boots the engine. Blocks for a second or two while the IWAD is loaded and the
/// first frame is rendered; after it returns, call [`tick`] at ~35 Hz.
pub fn start(wad: &Path, extra_args: &[String]) {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| String::from("cursor-doom.exe"));

    let mut args: Vec<String> = vec![exe, "-iwad".into(), wad.display().to_string()];
    args.extend(extra_args.iter().cloned());

    let mut cstrings = Vec::with_capacity(args.len());
    for arg in &args {
        cstrings.push(CString::new(arg.as_str()).unwrap_or_else(|_| CString::default()));
    }

    let (argc, argv) = {
        let mut strings = ARGV_STRINGS.lock().unwrap();
        strings.clear();
        strings.extend(cstrings);

        let mut pointers = ARGV_POINTERS.lock().unwrap();
        pointers.clear();
        pointers.extend(strings.iter().map(|c| c.as_ptr() as usize));
        // Stable for the lifetime of the process: the Vec is never resized again
        // and lives in a static.
        (
            pointers.len() as c_int,
            pointers.as_ptr() as *mut *mut c_char,
        )
    };

    CLOCK.get_or_init(Instant::now);

    unsafe {
        doomgeneric_Create(argc, argv);
    }
}

/// Runs one game tic and renders one frame (via `DG_DrawFrame`).
///
/// This can block for a whole frame: doomgeneric waits for the next tic
/// internally (`TryRunTics`). The wait is spent in [`DG_SleepMs`], which calls
/// the session's idle callback, so the cursor keeps following the pointer.
pub fn tick() {
    let started = Instant::now();
    unsafe { doomgeneric_Tick() }
    let micros = started.elapsed().as_micros() as u64;
    let ms = micros / 1000;
    TICK_LAST_MS.store(ms, Ordering::Relaxed);
    TICK_MAX_MS.fetch_max(ms, Ordering::Relaxed);
    if ms >= 250 {
        TICK_SLOW.fetch_add(1, Ordering::Relaxed);
    }
}

/// `(last, max, count >= 250 ms)` tick durations, in milliseconds.
pub fn tick_stats() -> (u64, u64, u64) {
    (
        TICK_LAST_MS.load(Ordering::Relaxed),
        TICK_MAX_MS.load(Ordering::Relaxed),
        TICK_SLOW.load(Ordering::Relaxed),
    )
}

/// Freezes (or resumes) the engine clock, which pauses the game itself.
pub fn set_paused(paused: bool) {
    let Ok(mut since) = PAUSED_SINCE.lock() else {
        return;
    };
    match (paused, *since) {
        (true, None) => *since = Some(Instant::now()),
        (false, Some(started)) => {
            PAUSED_TOTAL_MS.fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
            *since = None;
        }
        _ => {}
    }
}

pub fn is_paused() -> bool {
    PAUSED_SINCE
        .lock()
        .map(|since| since.is_some())
        .unwrap_or(false)
}

/// Registers the callback run from [`DG_SleepMs`] (see [`IDLE`]).
pub fn set_idle_callback(callback: Option<fn()>) {
    if let Ok(mut idle) = IDLE.lock() {
        *idle = callback;
    }
}

/// Borrow the engine's BGRA framebuffer (320x200).
pub fn frame() -> &'static [u8] {
    let ptr = unsafe { DG_ScreenBuffer } as *const u8;
    if ptr.is_null() {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(ptr, FRAME_BYTES) }
}

/// Drops queued input and releases every Doom key, so pausing never leaves the
/// player walking into a wall with a key stuck down.
pub fn release_all() {
    let mut queue = KEYS.lock().unwrap();
    queue.clear();
    for key in [
        crate::keys::KEY_UPARROW,
        crate::keys::KEY_DOWNARROW,
        crate::keys::KEY_LEFTARROW,
        crate::keys::KEY_RIGHTARROW,
        crate::keys::KEY_FIRE,
        crate::keys::KEY_ENTER,
        crate::keys::KEY_USE,
        crate::keys::KEY_RSHIFT,
    ] {
        queue.push_back((false, key));
    }
}

/// Queues a key event for the engine (doom key codes, see `keys.rs`).
pub fn push_key(pressed: bool, key: u8) {
    let mut queue = KEYS.lock().unwrap();
    // Keep the backlog bounded; the engine drains this at 35 Hz.
    if queue.len() < 256 {
        queue.push_back((pressed, key));
    }
}
