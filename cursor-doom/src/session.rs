//! The worker process: runs Doom *as the mouse cursor* until the user clicks
//! the middle mouse button (or the launcher asks it to stop).
//!
//! Life cycle:
//! 1. boot the engine (`engine::start`) - this blocks while the IWAD loads
//! 2. hide the system cursor, install the global input hooks
//! 3. create the click-through overlay window that follows the pointer
//! 4. loop at 35 fps: tick the engine, downscale the frame, paint the overlay,
//!    optionally feed the MP4 recorder
//! 5. tear everything down and restore the user's cursor

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, POINT, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    OpenEventW, OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetCursorPos, MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage,
    MSG, MWMO_INPUTAVAILABLE, PM_REMOVE, QS_ALLINPUT,
};

use crate::frame;
use crate::hooks::{self, HookEvent};
use crate::overlay::{self, Overlay};
use crate::record;
use crate::status::SessionStatus;
use crate::textmode::{self, Typing};
use crate::{cursor, engine, wad};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// The pointer sits in the middle of the doom view.
    Center,
    /// The pointer sits on the top-left corner of the doom view.
    TopLeft,
}

impl Anchor {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "topleft" | "top-left" | "corner" => Anchor::TopLeft,
            _ => Anchor::Center,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Anchor::Center => "center",
            Anchor::TopLeft => "topleft",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Anchor::Center => "Center of the doom view",
            Anchor::TopLeft => "Top-left corner",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub wad: PathBuf,
    pub width: u32,
    pub height: u32,
    pub scale_percent: u32,
    pub anchor: Anchor,
    pub reticle: bool,
    pub record: bool,
    pub autostart: bool,
    pub hide_cursor: bool,
    pub status_file: Option<PathBuf>,
    pub stop_event: Option<String>,
    pub parent_pid: Option<u32>,
    /// Stop automatically after this many frames (0 = run until asked to stop).
    pub max_frames: u64,
    /// Diagnostics: run without the overlay window / input hooks.
    pub no_overlay: bool,
    pub no_hooks: bool,
    /// Take WASD / arrows / Right Ctrl away from the desktop and hand them to
    /// the game. Off = the doom cursor is display only.
    pub capture_keys: bool,
    /// Pause and give the pointer back while the user is typing somewhere.
    pub pause_on_typing: bool,
}

impl SessionConfig {
    /// Clamps to what the app promises: at most 256x160, even dimensions (H.264).
    pub fn sanitised(mut self) -> Self {
        self.width = (self.width.clamp(16, 256) / 2) * 2;
        self.height = (self.height.clamp(16, 160) / 2) * 2;
        self.scale_percent = self.scale_percent.clamp(25, 400);
        self
    }
}

/// Handles used to stop the worker from outside.
struct Control {
    stop_event: Option<HANDLE>,
    parent: Option<HANDLE>,
}

impl Control {
    fn open(cfg: &SessionConfig) -> Self {
        let stop_event = cfg.stop_event.as_ref().and_then(|name| {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide.as_ptr())).ok() }
        });
        let parent = cfg
            .parent_pid
            .and_then(|pid| unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok() });
        Self { stop_event, parent }
    }

    fn handles(&self) -> Vec<HANDLE> {
        [self.stop_event, self.parent]
            .into_iter()
            .flatten()
            .collect()
    }

    /// `true` when the launcher asked us to stop, or the launcher died.
    fn stop_requested(&self) -> bool {
        self.handles()
            .into_iter()
            .any(|handle| unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0)
    }
}

/// Drains the thread's message queue. Low level hooks only fire while the
/// installing thread pumps messages, so this runs every frame and while waiting.
fn pump_messages() {
    let mut msg = MSG::default();
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn cursor_position() -> POINT {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    point
}

/// Where the overlay should sit for the current pointer position.
fn pointer_target(width: usize, height: usize, anchor: Anchor) -> (i32, i32) {
    let pointer = cursor_position();
    match anchor {
        Anchor::Center => (pointer.x - width as i32 / 2, pointer.y - height as i32 / 2),
        Anchor::TopLeft => (pointer.x, pointer.y),
    }
}

/// The overlay's window handle while it is on screen (0 = nothing to track).
static IDLE_HWND: AtomicIsize = AtomicIsize::new(0);
/// Last position we moved it to, so a still pointer costs one comparison.
static IDLE_LAST: Mutex<Option<(i32, i32)>> = Mutex::new(None);
static IDLE_SIZE: AtomicUsize = AtomicUsize::new(0);
static IDLE_HEIGHT: AtomicUsize = AtomicUsize::new(0);
static IDLE_TOPLEFT: AtomicBool = AtomicBool::new(false);
/// How often the message queue is pumped while the engine waits for a tic.
static IDLE_PUMPS: AtomicUsize = AtomicUsize::new(0);

/// Re-glues the overlay to the pointer.
///
/// This runs from the frame loop *and* from the engine's idle callback
/// (`DG_SleepMs`), which doomgeneric calls once per millisecond while it waits
/// for the next tic. That is what keeps the cursor glued to the pointer even
/// when a tic takes tens of milliseconds.
pub fn idle_tick() {
    pump_messages();
    IDLE_PUMPS.fetch_add(1, Ordering::Relaxed);

    let hwnd = IDLE_HWND.load(Ordering::Relaxed);
    if hwnd == 0 {
        return;
    }
    let width = IDLE_SIZE.load(Ordering::Relaxed);
    let height = IDLE_HEIGHT.load(Ordering::Relaxed);
    if width == 0 || height == 0 {
        return;
    }
    let anchor = if IDLE_TOPLEFT.load(Ordering::Relaxed) {
        Anchor::TopLeft
    } else {
        Anchor::Center
    };
    let target = pointer_target(width, height, anchor);

    let Ok(mut last) = IDLE_LAST.lock() else {
        return;
    };
    if *last == Some(target) {
        return;
    }
    let _ = crate::overlay::set_position(hwnd, target.0, target.1, width as i32, height as i32);
    *last = Some(target);
}

/// Points the idle ticker at the overlay (or at nothing when it is hidden).
fn track_overlay(overlay: Option<&Overlay>, width: usize, height: usize, anchor: Anchor) {
    match overlay {
        Some(window) => {
            IDLE_HWND.store(window.hwnd(), Ordering::Relaxed);
            IDLE_SIZE.store(width, Ordering::Relaxed);
            IDLE_HEIGHT.store(height, Ordering::Relaxed);
            IDLE_TOPLEFT.store(anchor == Anchor::TopLeft, Ordering::Relaxed);
            let (x, y) = current_target(width, height, anchor);
            let _ = window.move_to(x, y);
            if let Ok(mut last) = IDLE_LAST.lock() {
                *last = None;
            }
        }
        None => {
            IDLE_HWND.store(0, Ordering::Relaxed);
        }
    }
}

/// Where the overlay belongs right now.
fn current_target(width: usize, height: usize, anchor: Anchor) -> (i32, i32) {
    pointer_target(width, height, anchor)
}

fn log(message: &str) {
    println!("[cursor-doom] {message}");
}

/// Owns the optional MP4 recorder while the session runs.
struct Recording {
    recorder: Option<record::Recorder>,
    started: Instant,
    width: u32,
    height: u32,
}

impl Recording {
    fn new(cfg: &SessionConfig, status: &mut SessionStatus) -> Self {
        let mut this = Self {
            recorder: None,
            started: Instant::now(),
            width: cfg.width,
            height: cfg.height,
        };
        if cfg.record {
            this.start(status);
        }
        this
    }

    fn start(&mut self, status: &mut SessionStatus) {
        let path = record::new_recording_path();
        match record::Recorder::start(&path, self.width, self.height) {
            Ok(recorder) => {
                let (ew, eh) = recorder.encoded_size();
                let scale = recorder.scale();
                log(&format!(
                    "recording to {} ({ew}x{eh}, x{scale})",
                    recorder.path().display()
                ));
                status.set("record_size", format!("{ew}x{eh}"));
                status.set("record_scale", scale);
                if scale > 1 {
                    status.set(
                        "record_note",
                        format!(
                            "{0}x{1} is below what Windows' H.264 encoder accepts, so the video is upscaled x{2} to {3}x{4} (pixel exact, no blur)",
                            self.width, self.height, scale, ew, eh
                        ),
                    );
                }
                status.set("recording", "1");
                status.set("record_path", recorder.path().display());
                status.set("record_error", "");
                self.started = Instant::now();
                self.recorder = Some(recorder);
            }
            Err(err) => {
                log(&format!("could not start recording: {err}"));
                status.set("record_error", &err);
            }
        }
    }

    fn toggle(&mut self, status: &mut SessionStatus) {
        if self.recorder.is_some() {
            self.stop(status);
        } else {
            self.start(status);
        }
    }

    fn stop(&mut self, status: &mut SessionStatus) {
        if let Some(recorder) = self.recorder.take() {
            let frames = recorder.frames();
            match recorder.finish() {
                Ok(path) => {
                    log(&format!(
                        "recording saved: {} ({frames} frames)",
                        path.display()
                    ));
                    status.set("recording", "0");
                    status.set("last_record_path", path.display());
                    status.set("last_record_frames", frames);
                }
                Err(err) => {
                    log(&format!("could not finalise recording: {err}"));
                    status.set("record_error", &err);
                }
            }
        }
        status.set("recording", "0");
    }

    fn push(&mut self, native: &[u8], status: &mut SessionStatus) {
        let Some(recorder) = self.recorder.as_mut() else {
            return;
        };
        if let Err(err) = recorder.push(native, self.started.elapsed()) {
            log(&format!("recording stopped: {err}"));
            status.set("record_error", &err);
            self.recorder = None;
        }
    }

    fn frames(&self) -> u64 {
        self.recorder
            .as_ref()
            .map(record::Recorder::frames)
            .unwrap_or(0)
    }
}

/// Why the game is not running right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PauseKind {
    None,
    /// The user is typing in another window: pointer back, keys untouched.
    Typing,
    /// F8: the game is frozen but still drawn on the cursor.
    Manual,
}

/// Snapshot of the counters the launcher displays.
fn write_status(
    status: &mut SessionStatus,
    file: &std::path::Path,
    recording: &Recording,
    frames: u64,
    fps: f64,
) {
    status.set("fps", format!("{fps:.1}"));
    status.set("frames", frames);
    status.set("record_frames", recording.frames());
    let (last_tick, max_tick, slow) = engine::tick_stats();
    status.set("tick_ms", last_tick);
    status.set("tick_max_ms", max_tick);
    status.set("slow_ticks", slow);
    status.set("idle_pumps", IDLE_PUMPS.load(Ordering::Relaxed));
    status.set("capture", if hooks::capture() { "1" } else { "0" });
    status.set("keys_fed", hooks::keys_fed());
    status.set("key_events", hooks::key_events());
    status.set("hook_ticks", hooks::hook_ticks());
    status.set("engine_paused", if engine::is_paused() { "1" } else { "0" });
    let _ = status.save(file);
}

/// What the session should do after a hook event.
enum Flow {
    Continue,
    Stop(&'static str),
}

/// Applies one hook event.
fn handle_event(
    event: HookEvent,
    recording: &mut Recording,
    status: &mut SessionStatus,
    manual_pause: &mut bool,
) -> Flow {
    match event {
        HookEvent::Key { pressed, key } => {
            engine::push_key(pressed, key);
            Flow::Continue
        }
        HookEvent::MiddleClick => Flow::Stop("middle click"),
        HookEvent::RecordToggle => {
            recording.toggle(status);
            Flow::Continue
        }
        HookEvent::PauseToggle => {
            *manual_pause = !*manual_pause;
            Flow::Continue
        }
    }
}

pub fn run(cfg: SessionConfig) -> Result<(), String> {
    let cfg = cfg.sanitised();
    if !cfg.wad.is_file() {
        return Err(format!("WAD not found: {}", cfg.wad.display()));
    }

    // Physical pixels everywhere: GetCursorPos and layered windows then agree
    // even on scaled monitors.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // Resolve the IWAD before touching the working directory (Doom chdirs into it
    // by keeping config/savegames in the cwd).
    let wad_path = std::path::absolute(&cfg.wad).unwrap_or_else(|_| cfg.wad.clone());

    // Doom keeps its config/savegames in the working directory; keep them in the
    // app data folder instead of wherever the launcher happened to be started.
    let work_dir = crate::status::app_dir().join("runtime");
    if std::fs::create_dir_all(&work_dir).is_ok() {
        let _ = std::env::set_current_dir(&work_dir);
    }

    let status_file = cfg
        .status_file
        .clone()
        .unwrap_or_else(crate::status::status_path);
    let mut status = SessionStatus::new();
    status.set("state", "starting");
    status.set("wad", wad_path.display());
    status.set("width", cfg.width);
    status.set("height", cfg.height);
    status.set("scale", cfg.scale_percent);
    status.set("anchor", cfg.anchor.as_str());
    status.set("recording", "0");
    let _ = status.save(&status_file);

    let control = Control::open(&cfg);

    // ---- engine boot -----------------------------------------------------
    let mut extra_args: Vec<String> = Vec::new();
    if cfg.autostart {
        match wad::Wad::open(&wad_path) {
            Ok(info) => extra_args.extend(info.warp_args()),
            Err(err) => log(&format!("autostart disabled: {err}")),
        }
    }

    status.set("state", "loading");
    let _ = status.save(&status_file);
    log("booting doomgeneric...");
    engine::start(&wad_path, &extra_args);
    log("engine booted");

    // A hard kill cannot run teardown code, so a separate watchdog process puts
    // the cursor back if this process ever disappears without cleaning up.
    if cfg.hide_cursor {
        match crate::guardian::spawn() {
            Ok(()) => log("cursor guardian started"),
            Err(err) => log(&format!("warning: {err}")),
        }
    }

    // ---- cursor replacement ---------------------------------------------
    // Declared before the overlay so it is restored *after* the overlay is gone.
    let cursor_hider = if cfg.hide_cursor {
        match cursor::CursorHider::hide() {
            Ok(hider) => {
                status.set("cursor_hidden", "1");
                Some(hider)
            }
            Err(err) => {
                log(&format!("could not hide the system cursor: {err}"));
                status.set("cursor_hidden", "0");
                None
            }
        }
    } else {
        status.set("cursor_hidden", "0");
        None
    };

    // One-shot diagnostic: is the pointer actually invisible now?
    if cursor_hider.is_some() {
        match cursor::current_cursor_is_blank() {
            Some(true) => status.set("cursor_blank", "1"),
            Some(false) => status.set("cursor_blank", "0"),
            None => status.set("cursor_blank", "?"),
        }
    }

    let input_hooks = match if cfg.no_hooks {
        Err("disabled by --no-hooks".to_string())
    } else {
        hooks::InputHooks::install()
    } {
        Ok(input_hooks) => Some(input_hooks),
        Err(err) => {
            log(&format!("input hooks unavailable: {err}"));
            status.set("input_error", &err);
            None
        }
    };

    let (dw, dh) = frame::display_size(cfg.width as usize, cfg.height as usize, cfg.scale_percent);
    let mut overlay = Some(Overlay::new(dw as u32, dh as u32)?);
    if let Some(window) = overlay.as_ref() {
        window.show();
        status.set("overlay_hwnd", window.hwnd());
        status.set("overlay_size", format!("{dw}x{dh}"));
    }

    let reticle = if cfg.reticle {
        Some(match cfg.anchor {
            Anchor::Center => (dw / 2, dh / 2),
            Anchor::TopLeft => (3, 3),
        })
    } else {
        None
    };

    // ---- frame buffers ---------------------------------------------------
    let mut native = vec![0u8; cfg.width as usize * cfg.height as usize * 4];
    let downscaler = frame::Downscaler::new(cfg.width as usize, cfg.height as usize);

    if cfg.no_overlay {
        overlay = None;
    }

    let mut recording = Recording::new(&cfg, &mut status);
    let mut preview = crate::preview::PreviewWriter::new(
        engine::WIDTH as u32,
        engine::HEIGHT as u32,
        cfg.width,
        cfg.height,
    );

    status.set("state", "running");
    status.set("fps", "0");
    let _ = status.save(&status_file);

    // ---- main loop -------------------------------------------------------
    #[allow(unused_assignments)]
    let mut stop_reason: &str = "unknown";
    let mut frames: u64 = 0;
    let mut last_status = Instant::now();
    let mut last_frame_time = Instant::now();
    let mut measured_fps = 0.0f64;
    let mut last_cursor_check = Instant::now();
    let mut last_rearm = Instant::now();
    let mut deadline = Instant::now();
    let mut manual_pause = false;
    let mut typing = Typing::No;
    let mut typing_votes: i32 = 0;
    let mut last_typing_check = Instant::now();
    let mut pause: PauseKind = PauseKind::None;
    let mut overlay_visible = overlay.is_some();
    let mut pointer_blanked = cursor_hider.is_some();

    track_overlay(overlay.as_ref(), dw, dh, cfg.anchor);
    engine::set_idle_callback(Some(idle_tick));
    if !cfg.capture_keys {
        hooks::set_capture(false);
    }
    status.set("paused", "0");
    status.set("pause_reason", "");

    'session: loop {
        // ---- hook events -------------------------------------------------
        while let Some(event) = hooks::next_event() {
            match handle_event(event, &mut recording, &mut status, &mut manual_pause) {
                Flow::Continue => {}
                Flow::Stop(reason) => {
                    stop_reason = reason;
                    break 'session;
                }
            }
        }
        if control.stop_requested() {
            stop_reason = "launcher";
            break 'session;
        }

        // ---- typing detection --------------------------------------------
        // A text caret anywhere in the foreground window (or the I-beam cursor
        // over a field) means the user is typing: pause, hand the pointer back
        // and stop eating keys until the text context is gone.
        if cfg.pause_on_typing && last_typing_check.elapsed() >= Duration::from_millis(120) {
            // Two hits in a row before pausing, three misses before resuming:
            // the probes flicker while the pointer crosses a field's edge and we
            // do not want the game to stutter in and out of a pause.
            let mut hit = textmode::probe();
            if hit == Typing::No && hooks::typing_burst() {
                hit = Typing::Burst;
            }
            typing_votes = if hit.active() {
                (typing_votes + 1).min(3)
            } else {
                (typing_votes - 1).max(-3)
            };
            let next = if typing_votes >= 2 {
                hit
            } else if typing_votes <= -3 {
                Typing::No
            } else {
                typing
            };
            if next != typing {
                typing = next;
            }
            last_typing_check = Instant::now();
        }
        let wanted = if typing.active() {
            PauseKind::Typing
        } else if manual_pause {
            PauseKind::Manual
        } else {
            PauseKind::None
        };
        if wanted != pause {
            let was_paused = pause != PauseKind::None;
            pause = wanted;
            let now_paused = pause != PauseKind::None;

            if now_paused != was_paused {
                engine::set_paused(now_paused);
                hooks::set_capture(!now_paused && cfg.capture_keys);
                if now_paused {
                    engine::release_all();
                }
            }

            // Typing needs the real pointer back; a manual pause keeps the frozen
            // doom view on the cursor so you can see what is paused.
            let want_pointer = match pause {
                PauseKind::Typing => false,
                _ => cfg.hide_cursor,
            };
            if want_pointer != pointer_blanked {
                pointer_blanked = want_pointer;
                if want_pointer {
                    if let Some(hider) = cursor_hider.as_ref() {
                        hider.reapply();
                    }
                    status.set("cursor_blank", "1");
                } else {
                    cursor::restore_system_cursors();
                    status.set("cursor_blank", "0");
                }
            }
            let want_overlay = match pause {
                PauseKind::Typing => false,
                _ => overlay.is_some(),
            };
            if want_overlay != overlay_visible {
                overlay_visible = want_overlay;
                match (want_overlay, overlay.as_ref()) {
                    (true, Some(window)) => {
                        window.show();
                        track_overlay(Some(window), dw, dh, cfg.anchor);
                    }
                    (false, _) => {
                        if let Some(window) = overlay.as_ref() {
                            window.hide();
                        }
                        track_overlay(None, dw, dh, cfg.anchor);
                    }
                    _ => {}
                }
            }

            match pause {
                PauseKind::Typing => {
                    log("paused: typing (pointer back, keys untouched)");
                    status.set("pause_reason", "typing");
                }
                PauseKind::Manual => {
                    log("paused: F8");
                    status.set("pause_reason", "manual");
                }
                PauseKind::None => {
                    log("resumed");
                    status.set("pause_reason", "");
                }
            }
            status.set("paused", if now_paused { "1" } else { "0" });
            status.set("typing", typing.as_str());
            let _ = status.save(&status_file);
        }

        // ---- paused: keep the process responsive, draw nothing ------------
        if pause != PauseKind::None {
            idle_tick();
            if last_status.elapsed() >= Duration::from_millis(500) {
                write_status(&mut status, &status_file, &recording, frames, measured_fps);
                last_status = Instant::now();
            }
            if let Some(window) = overlay.as_ref() {
                if overlay_visible {
                    let (ox, oy) = current_target(dw, dh, cfg.anchor);
                    let _ = window.present(ox, oy);
                }
            }
            let handles = control.handles();
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    if handles.is_empty() {
                        None
                    } else {
                        Some(&handles)
                    },
                    15,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
            }
            continue;
        }

        // ---- one engine tic ----------------------------------------------
        // doomgeneric waits for the next tic inside here (sleeping in
        // `DG_SleepMs`, which pumps our hooks and keeps the cursor on the
        // pointer), so there is no second, external frame clock any more.
        engine::tick();

        let engine_frame = engine::frame();
        if !engine_frame.is_empty() {
            downscaler.run(engine_frame, &mut native);

            if let Some(window) = overlay.as_mut() {
                let pixels = window.pixels();
                frame::scale_into(
                    &native,
                    cfg.width as usize,
                    cfg.height as usize,
                    dw,
                    dh,
                    reticle,
                    pixels,
                );
            }

            if overlay_visible {
                if let Some(window) = overlay.as_ref() {
                    let (ox, oy) = current_target(dw, dh, cfg.anchor);
                    if let Err(err) = window.present(ox, oy) {
                        log(&format!("present failed: {err}"));
                    }
                }
            }

            recording.push(&native, &mut status);
            preview.write(engine_frame, &native);

            frames += 1;
            let now = Instant::now();
            let dt = now.duration_since(last_frame_time).as_secs_f64();
            if dt > 0.0 {
                let instant = 1.0 / dt;
                measured_fps = if measured_fps == 0.0 {
                    instant
                } else {
                    measured_fps * 0.8 + instant * 0.2
                };
            }
            last_frame_time = now;
        }

        // Windows may hand the real cursor back (another app calling
        // SPI_SETCURSORS, a UAC prompt, a resolution change, ...). Keep it blank.
        // Windows occasionally drops a low-level hook (desktop switch, a stalled
        // callback, a secure-desktop prompt). Re-install them every few seconds;
        // it is a single posted message and costs nothing.
        if let Some(hooks) = input_hooks.as_ref() {
            if last_rearm.elapsed() >= Duration::from_secs(3) {
                hooks.rearm();
                last_rearm = Instant::now();
            }
        }

        if let Some(hider) = cursor_hider.as_ref() {
            if last_cursor_check.elapsed() >= Duration::from_millis(1000) {
                if !hider.reapply() {
                    log("could not re-apply the blank cursor");
                }
                last_cursor_check = Instant::now();
            }
        }

        if last_status.elapsed() >= Duration::from_millis(500) {
            write_status(&mut status, &status_file, &recording, frames, measured_fps);
            last_status = Instant::now();
        }

        if cfg.max_frames > 0 && frames >= cfg.max_frames {
            stop_reason = "frame limit";
            break;
        }

        if overlay::close_requested() {
            stop_reason = "window closed";
            break;
        }

        // ---- pace the loop at 35 fps -------------------------------------
        // Absolute deadlines, not "sleep 28.57 ms per frame": the Windows timer
        // wakes a little late every time, and accumulating that error was what
        // made the game run at ~32 fps instead of 35.
        deadline += engine::FRAME_TIME;
        let now = Instant::now();
        if deadline < now {
            // We fell behind (a slow tic, a stalled present): resync instead of
            // sprinting through a burst of frames.
            deadline = now;
        }
        while Instant::now() < deadline {
            idle_tick();
            while let Some(event) = hooks::next_event() {
                match handle_event(event, &mut recording, &mut status, &mut manual_pause) {
                    Flow::Continue => {}
                    Flow::Stop(reason) => {
                        stop_reason = reason;
                        break 'session;
                    }
                }
            }
            if control.stop_requested() {
                stop_reason = "launcher";
                break 'session;
            }
            if overlay::close_requested() {
                stop_reason = "window closed";
                break 'session;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait_ms = (deadline - now).as_micros().div_ceil(1000).clamp(1, 4) as u32;
            let handles = control.handles();
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    if handles.is_empty() {
                        None
                    } else {
                        Some(&handles)
                    },
                    wait_ms,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
            }
        }
    }

    // ---- teardown --------------------------------------------------------
    preview.clear();
    log(&format!("stopping ({stop_reason})"));
    recording.stop(&mut status);

    status.set("state", "stopped");
    status.set("stop_reason", stop_reason);
    status.set("frames", frames);
    status.set("fps", format!("{measured_fps:.1}"));
    let _ = status.save(&status_file);

    // Drop order matters: overlay first, then the hooks, then the cursor, so the
    // real pointer comes back exactly when Doom stops being drawn.
    drop(overlay);
    drop(input_hooks);
    drop(cursor_hider);

    // The pointer is normal again: refresh the snapshot so the launcher stops
    // reporting "pointer invisible" once the session is over.
    status.set(
        "cursor_blank",
        if cursor::current_cursor_is_blank().unwrap_or(false) {
            "1"
        } else {
            "0"
        },
    );
    let _ = status.save(&status_file);

    Ok(())
}
