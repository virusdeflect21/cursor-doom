//! The app launcher (egui): pick the WAD, pick the cursor resolution, start/stop
//! the doom cursor.
//!
//! The session itself runs in a *child process* (`--session`). That keeps a bad
//! IWAD or a crash in the C engine from taking the launcher down with it, and it
//! gives us a guaranteed place to restore the user's cursor afterwards.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use eframe::egui;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

use crate::session::Anchor;
use crate::settings::Settings;
use crate::status::{self, SessionStatus};
use crate::{cursor, wad};

/// Parsed but not yet started configuration, mirrored into the child's argv.
const DEFAULT_SIZES: &[(u32, u32)] = &[
    (256, 160),
    (224, 140),
    (192, 120),
    (160, 100),
    (128, 80),
    (96, 60),
    (64, 40),
    (32, 20),
    (16, 16),
];

/// Uploads an image into an existing texture, creating it the first time.
fn upload(
    texture: &mut Option<egui::TextureHandle>,
    ctx: &egui::Context,
    name: &str,
    image: egui::ColorImage,
) {
    match texture {
        Some(texture) => texture.set(image, egui::TextureOptions::NEAREST),
        None => *texture = Some(ctx.load_texture(name, image, egui::TextureOptions::NEAREST)),
    }
}

pub fn run() -> eframe::Result {
    run_with(None)
}

/// Opens the launcher. With `screenshot` it renders a few frames, writes the
/// window contents to that PNG and quits (used to document the app).
pub fn run_with(screenshot: Option<PathBuf>) -> eframe::Result {
    let tall = screenshot.is_some();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(if tall {
                [600.0, 1000.0]
            } else {
                [560.0, 660.0]
            })
            .with_min_inner_size([500.0, 560.0])
            .with_title("Cursor Doom"),
        ..Default::default()
    };
    eframe::run_native(
        "Cursor Doom",
        options,
        Box::new(move |_cc| Ok(Box::new(Launcher::with_options(screenshot.clone())))),
    )
}
/// Builds and starts the worker process. Kept separate from the GUI so the
/// launcher -> worker handshake can be tested headlessly.
#[allow(clippy::too_many_arguments)] // one field per worker flag, kept explicit
fn spawn_session(
    exe: &std::path::Path,
    settings: &Settings,
    event_name: &str,
    status_path: &std::path::Path,
    stdout: std::fs::File,
    stderr: std::fs::File,
    max_frames: Option<u64>,
    hide_cursor: bool,
) -> std::io::Result<Child> {
    let mut command = Command::new(exe);
    command
        .args(session_args(
            settings,
            event_name,
            status_path,
            max_frames,
            hide_cursor,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command.spawn()
}
/// The worker's command line. Split out so it can be unit tested.
fn session_args(
    settings: &Settings,
    event_name: &str,
    status_path: &std::path::Path,
    max_frames: Option<u64>,
    hide_cursor: bool,
) -> Vec<String> {
    let wad_path = std::path::absolute(&settings.wad)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| settings.wad.clone());
    let mut args: Vec<String> = vec![
        "--session".into(),
        "--wad".into(),
        wad_path,
        "--width".into(),
        settings.width.to_string(),
        "--height".into(),
        settings.height.to_string(),
        "--scale".into(),
        settings.scale_percent.to_string(),
        "--anchor".into(),
        settings.anchor.as_str().into(),
        "--reticle".into(),
        if settings.reticle { "1" } else { "0" }.into(),
        "--record".into(),
        if settings.record { "1" } else { "0" }.into(),
        "--autostart".into(),
        if settings.autostart { "1" } else { "0" }.into(),
        "--status-file".into(),
        status_path.display().to_string(),
        "--stop-event".into(),
        event_name.into(),
        "--parent-pid".into(),
        std::process::id().to_string(),
    ];
    if let Some(frames) = max_frames {
        args.push("--max-frames".into());
        args.push(frames.to_string());
    }
    if !hide_cursor {
        args.push("--no-hide-cursor".into());
    }
    if !settings.capture_keys {
        args.push("--no-capture".into());
    }
    if !settings.pause_on_typing {
        args.push("--no-typing-pause".into());
    }
    args
}
struct Running {
    child: Child,
    stop_event: HANDLE,
    status: SessionStatus,
    last_poll: Instant,
    poll_error: Option<String>,
}
pub struct Launcher {
    settings: Settings,
    running: Option<Running>,
    message: String,
    is_error: bool,
    counter: u32,
    wad_info: String,
    /// The three views of the same frame, kept as textures.
    preview_native: Option<egui::TextureHandle>,
    preview_cursor: Option<egui::TextureHandle>,
    preview_screen: Option<egui::TextureHandle>,
    preview_native_size: (u32, u32),
    preview_cursor_size: (u32, u32),
    preview_screen_size: (u32, u32),
    preview_last: std::time::Instant,
    preview_live: bool,
    /// `--screenshot <png>`: save one frame of the window and exit, so the
    /// README can show the real launcher instead of a mock-up.
    screenshot_target: Option<PathBuf>,
    screenshot_frames: u32,
}
impl Launcher {
    fn with_options(screenshot_target: Option<PathBuf>) -> Self {
        // A previous session may have died while the cursor was blanked.
        cursor::restore_system_cursors();
        let settings = Settings::load();
        let mut launcher = Self {
            settings,
            running: None,
            message: String::new(),
            is_error: false,
            counter: 0,
            wad_info: String::new(),
            preview_native: None,
            preview_cursor: None,
            preview_screen: None,
            preview_native_size: (0, 0),
            preview_cursor_size: (0, 0),
            preview_screen_size: (0, 0),
            preview_last: std::time::Instant::now() - std::time::Duration::from_secs(1),
            preview_live: false,
            screenshot_target,
            screenshot_frames: 0,
        };
        launcher.refresh_wad_info();
        launcher
    }
    fn refresh_wad_info(&mut self) {
        let path = PathBuf::from(&self.settings.wad);
        self.wad_info = match wad::Wad::open(&path) {
            Ok(info) => match info.validate() {
                Ok(()) => info.describe(),
                Err(err) => format!("!! {err}"),
            },
            Err(err) => format!("!! {err}"),
        };
    }
    /// Pulls the session's frames so the launcher can show what the cursor shows.
    fn poll_preview(&mut self, ctx: &egui::Context) {
        if !self.settings.show_preview {
            self.preview_live = false;
            return;
        }
        if self.preview_last.elapsed() < std::time::Duration::from_millis(90) {
            return;
        }
        self.preview_last = std::time::Instant::now();

        let Some(frame) = crate::preview::read() else {
            self.preview_live = false;
            return;
        };
        self.preview_live = true;

        // 1. the engine's own framebuffer (320x200)
        self.preview_native_size = (frame.native_width, frame.native_height);
        let native = Self::bgra_to_color_image(
            &frame.native,
            frame.native_width as usize,
            frame.native_height as usize,
        );
        upload(&mut self.preview_native, ctx, "preview-native", native);

        // 2. the pixels the cursor is painted with (the chosen cursor resolution)
        self.preview_cursor_size = (frame.cursor_width, frame.cursor_height);
        let cursor = Self::bgra_to_color_image(
            &frame.cursor,
            frame.cursor_width as usize,
            frame.cursor_height as usize,
        );
        upload(&mut self.preview_cursor, ctx, "preview-cursor", cursor);

        // 3. what actually appears on screen: the cursor pixels blown up by the
        //    display scale. Same helper the session uses, reticle included.
        let reticle = if self.settings.reticle {
            let (w, h) = (frame.cursor_width as usize, frame.cursor_height as usize);
            Some(match self.settings.anchor {
                Anchor::Center => (w / 2, h / 2),
                Anchor::TopLeft => (3, 3),
            })
        } else {
            None
        };
        let mut screen = Vec::new();
        let (dw, dh) = crate::frame::build_display(
            &frame.cursor,
            frame.cursor_width as usize,
            frame.cursor_height as usize,
            self.settings.scale_percent,
            reticle,
            &mut screen,
        );
        self.preview_screen_size = (dw as u32, dh as u32);
        let screen = Self::bgra_to_color_image(&screen, dw, dh);
        upload(&mut self.preview_screen, ctx, "preview-screen", screen);
    }

    /// BGRA (engine order) to the RGBA image egui wants.
    fn bgra_to_color_image(pixels: &[u8], width: usize, height: usize) -> egui::ColorImage {
        let mut rgba = Vec::with_capacity(width * height * 4);
        for pixel in pixels.as_chunks::<4>().0.iter().take(width * height) {
            rgba.push(pixel[2]);
            rgba.push(pixel[1]);
            rgba.push(pixel[0]);
            rgba.push(255);
        }
        egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba)
    }

    fn start(&mut self) {
        self.message.clear();
        self.is_error = false;
        let path = PathBuf::from(&self.settings.wad);
        let info = match wad::Wad::open(&path) {
            Ok(info) => info,
            Err(err) => {
                self.is_error = true;
                self.message = err;
                return;
            }
        };
        if let Err(err) = info.validate() {
            self.is_error = true;
            self.message = err;
            return;
        }
        let status_path = status::status_path();
        let log_path = status::log_path();
        if let Some(dir) = log_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let log = match std::fs::File::create(&log_path) {
            Ok(file) => file,
            Err(err) => {
                self.is_error = true;
                self.message = format!("cannot write {}: {err}", log_path.display());
                return;
            }
        };
        let log_clone = match log.try_clone() {
            Ok(file) => file,
            Err(err) => {
                self.is_error = true;
                self.message = format!("cannot duplicate log handle: {err}");
                return;
            }
        };
        self.counter += 1;
        let event_name = format!(
            "Local\\CursorDoomStop-{}-{}",
            std::process::id(),
            self.counter
        );
        let wide: Vec<u16> = event_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let stop_event = unsafe { CreateEventW(None, true, false, PCWSTR(wide.as_ptr())) };
        let stop_event = match stop_event {
            Ok(handle) => handle,
            Err(err) => {
                self.is_error = true;
                self.message = format!("CreateEventW failed: {err}");
                return;
            }
        };
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                self.is_error = true;
                self.message = format!("cannot locate the executable: {err}");
                return;
            }
        };
        let _ = std::fs::remove_file(&status_path);
        self.settings.save();
        match spawn_session(
            &exe,
            &self.settings,
            &event_name,
            &status_path,
            log,
            log_clone,
            None,
            true,
        ) {
            Ok(child) => {
                self.message =
                    "Doom is on your cursor. Press the middle mouse button to stop.".to_string();
                self.running = Some(Running {
                    child,
                    stop_event,
                    status: SessionStatus::new(),
                    last_poll: Instant::now() - Duration::from_secs(1),
                    poll_error: None,
                });
            }
            Err(err) => {
                self.is_error = true;
                self.message = format!("could not start the session: {err}");
                unsafe {
                    let _ = CloseHandle(stop_event);
                }
            }
        }
    }
    fn stop(&mut self) {
        let Some(mut running) = self.running.take() else {
            return;
        };
        unsafe {
            let _ = SetEvent(running.stop_event);
        }
        // Give it a moment to restore the cursor cleanly, then make sure.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match running.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = running.child.kill();
                        let _ = running.child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => break,
            }
        }
        unsafe {
            let _ = CloseHandle(running.stop_event);
        }
        cursor::restore_system_cursors();
        if self.message.is_empty() {
            self.message = "Stopped. Your cursor is back.".into();
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        self.poll_preview(ctx);
        let path = status::status_path();
        if let Some(running) = self.running.as_mut() {
            match running.child.try_wait() {
                Ok(Some(code)) => {
                    let final_status = SessionStatus::load(&path).unwrap_or_default();
                    let reason = final_status.text("stop_reason");
                    let error = final_status.text("error");
                    cursor::restore_system_cursors();
                    unsafe {
                        let _ = CloseHandle(running.stop_event);
                    }
                    self.running = None;
                    if !error.is_empty() {
                        self.is_error = true;
                        self.message = error;
                    } else if code.success() || reason == "middle click" || reason == "launcher" {
                        self.message = if reason.is_empty() {
                            "Session ended.".into()
                        } else {
                            format!("Session ended ({reason}). Your cursor is back.")
                        };
                    } else {
                        self.is_error = true;
                        self.message = format!(
                            "Session process exited with code {code}. See {}",
                            status::log_path().display()
                        );
                    }
                    return;
                }
                Ok(None) => {}
                Err(err) => {
                    running.poll_error = Some(err.to_string());
                }
            }
            if running.last_poll.elapsed() >= Duration::from_millis(250) {
                if let Some(status) = SessionStatus::load(&path) {
                    running.status = status;
                }
                running.last_poll = Instant::now();
            }
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
    fn busy(&self) -> bool {
        self.running.is_some()
    }
}
impl Drop for Launcher {
    fn drop(&mut self) {
        self.stop();
    }
}
impl eframe::App for Launcher {
    /// Runs even while the launcher window is hidden/minimised, so a finished
    /// session is always noticed (and the cursor restored) immediately.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
    }
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.screenshot_target.is_some() {
            self.finish_screenshot(root);
        }
        egui::CentralPanel::default().show(root, |ui| {
                     egui::ScrollArea::vertical().show(ui, |ui| {
                     ui.heading("Cursor Doom");
                     ui.label(
                     egui::RichText::new("doomgeneric running as your mouse cursor").weak(),
                     );
                     ui.add_space(8.0);

            // ---------------- preview: native / cursor / on screen ----------
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(
                        &mut self.settings.show_preview,
                        egui::RichText::new("Gameplay preview").strong(),
                    );
                    ui.label(
                        egui::RichText::new("- the game's own frame, the cursor's pixels and what lands on screen")
                            .small()
                            .weak(),
                    );
                });
                if self.settings.show_preview {
                    let native = self.preview_native_size;
                    let cursor = self.preview_cursor_size;
                    let screen = self.preview_screen_size;
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(
                            &mut self.settings.preview_view,
                            0,
                            format!("Native {}x{}", native.0.max(320), native.1.max(200)),
                        );
                        ui.selectable_value(
                            &mut self.settings.preview_view,
                            1,
                            format!("Cursor {}x{}", cursor.0, cursor.1),
                        );
                        let on_screen = if screen.0 > 0 {
                            screen
                        } else {
                            let (w, h) = crate::frame::display_size(
                                self.settings.width as usize,
                                self.settings.height as usize,
                                self.settings.scale_percent,
                            );
                            (w as u32, h as u32)
                        };
                        ui.selectable_value(
                            &mut self.settings.preview_view,
                            2,
                            format!("On screen {}x{}", on_screen.0, on_screen.1),
                        );
                    });
                    ui.add_enabled_ui(!self.busy(), |ui| {
                        ui.add(
                            egui::Slider::new(&mut self.settings.preview_zoom, 1..=10)
                                .text("Zoom"),
                        );
                    });

                    let (texture, (w, h), what) = match self.settings.preview_view {
                        0 => (
                            self.preview_native.as_ref(),
                            self.preview_native_size,
                            "the engine's own 320x200 framebuffer - the game at its native resolution",
                        ),
                        2 => (
                            self.preview_screen.as_ref(),
                            self.preview_screen_size,
                            "what the overlay actually paints, display scale and reticle included",
                        ),
                        _ => (
                            self.preview_cursor.as_ref(),
                            self.preview_cursor_size,
                            "the frame downscaled to your cursor resolution",
                        ),
                    };

                    match texture {
                        Some(texture) if w > 0 && h > 0 => {
                            // Never draw wider than the window: shrink the zoom
                            // instead of clipping the frame.
                            let room = (ui.available_width() - 16.0).max(32.0);
                            let wanted = self.settings.preview_zoom.clamp(1, 10) as f32;
                            let fits = (room / w as f32).max(1.0).floor();
                            let zoom = wanted.min(fits);
                            let size = egui::vec2(w as f32 * zoom, h as f32 * zoom);
                            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                                texture.id(),
                                size,
                            )));
                            ui.label(
                                egui::RichText::new(format!(
                                    "{w}x{h} at x{zoom:.0} - {what}{}",
                                    if self.preview_live {
                                        ""
                                    } else {
                                        " (last frame: paused or stopped)"
                                    }
                                ))
                                .small()
                                .weak(),
                            );
                        }
                        _ => {
                            ui.label(
                                egui::RichText::new(
                                    "Turn the doom cursor on and the gameplay shows up here, big enough to watch.",
                                )
                                .small()
                                .weak(),
                            );
                        }
                    }
                }
            });
            ui.add_space(6.0);
                     // ---------------- IWAD ----------------
                     ui.group(|ui| {
                     ui.label(egui::RichText::new("IWAD (game data)").strong());
                ui.horizontal(|ui| {
                    ui.add_enabled(
                        !self.busy(),
                        egui::TextEdit::singleline(&mut self.settings.wad)
                            .desired_width(360.0)
                            .hint_text("path to DOOM.WAD"),
                     );
                     if ui.add_enabled(!self.busy(), egui::Button::new("Browse…")).clicked() {
                     if let Some(path) = rfd::FileDialog::new()
                     .add_filter("Doom WAD", &["wad", "WAD"])
                     .set_title("Pick an IWAD")
                     .pick_file()
                     {
                     self.settings.wad = path.display().to_string();
                     self.refresh_wad_info();
                     }
                     }
                     });
                     let color = if self.wad_info.starts_with("!!") {
                     egui::Color32::from_rgb(220, 120, 120)
                     } else {
                     egui::Color32::from_rgb(140, 200, 140)
                     };
                     ui.colored_label(color, egui::RichText::new(&self.wad_info).small());
                     });
                     ui.add_space(6.0);
                     // ---------------- resolution / scale ----------------
                     ui.group(|ui| {
                     ui.label(egui::RichText::new("Cursor resolution").strong());
                ui.label(
                    egui::RichText::new(
                        "Doom renders 320×200 and the cursor shows it scaled down. Pick any size from 16×16 up to 256×160 (even numbers, to keep H.264 recording happy).",
                     )
                     .small()
                     .weak(),
                     );
                     ui.horizontal_wrapped(|ui| {
                     for (w, h) in DEFAULT_SIZES {
                     let selected = self.settings.width == *w && self.settings.height == *h;
                     let label = format!("{w}×{h}");
                     if ui
                     .add_enabled(!self.busy(), egui::Button::new(label).selected(selected))
                     .clicked()
                     {
                     self.settings.width = *w;
                     self.settings.height = *h;
                     }
                     }
                     });
                     ui.horizontal(|ui| {
                     ui.add_enabled_ui(!self.busy(), |ui| {
                     ui.label("Custom:");
                     ui.add(
                     egui::DragValue::new(&mut self.settings.width)
                     .range(16..=256)
                     .speed(2.0)
                     .suffix(" px"),
                     );
                     ui.label("×");
                     ui.add(
                     egui::DragValue::new(&mut self.settings.height)
                     .range(16..=160)
                     .speed(2.0)
                     .suffix(" px"),
                     );
                     });
                     });
                     // Keep the cursor inside the promised range and on even numbers
                     // (H.264 needs even frames). Only touch a value when the user
                     // actually changes it: clamping every frame used to snap a
                     // half-typed number back to the previous size.
                     let width = (self.settings.width.clamp(16, 256) / 2) * 2;
                     let height = (self.settings.height.clamp(16, 160) / 2) * 2;
                     if width != self.settings.width {
                         self.settings.width = width;
                     }
                     if height != self.settings.height {
                         self.settings.height = height;
                     }
                     ui.add_space(4.0);
                     ui.add_enabled_ui(!self.busy(), |ui| {
                     ui.add(
                     egui::Slider::new(&mut self.settings.scale_percent, 25..=400)
                     .suffix(" %")
                     .text("On-screen scale"),
                     );
                     });
                     let (dw, dh) = crate::frame::display_size(
                     self.settings.width as usize,
                     self.settings.height as usize,
                     self.settings.scale_percent,
                     );
                     ui.label(
                     egui::RichText::new(format!(
                     "On screen: {dw}×{dh} px once the cursor is on, 35 fps"
                     ))
                     .small()
                     .weak(),
                     );

                     });
                     ui.add_space(6.0);
                     // ---------------- behaviour ----------------
                     ui.group(|ui| {
                     ui.label(egui::RichText::new("Behaviour").strong());
                ui.add_enabled_ui(!self.busy(), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Pointer sits at:");
                     egui::ComboBox::from_id_salt("anchor")
                     .selected_text(self.settings.anchor.label())
                     .show_ui(ui, |ui| {
                     ui.selectable_value(
                     &mut self.settings.anchor,
                     Anchor::Center,
                     Anchor::Center.label(),
                     );
                     ui.selectable_value(
                     &mut self.settings.anchor,
                     Anchor::TopLeft,
                     Anchor::TopLeft.label(),
                     );
                     });
                     });
                     ui.checkbox(&mut self.settings.reticle, "Draw a small crosshair at the pointer");
                    ui.checkbox(
                        &mut self.settings.capture_keys,
                        "Let the game take the arrows and Ctrl while it is on",
                    );
                    ui.checkbox(
                        &mut self.settings.pause_on_typing,
                        "Pause and give me the pointer back while I am typing somewhere",
                    );
                     ui.checkbox(
                     &mut self.settings.autostart,
                     "Start a new game immediately (E1M1 / MAP01, skill 3)",
                     );
                     ui.checkbox(
                     &mut self.settings.record,
                     "Record the cursor frames to MP4 (Downloads, the cursor's resolution)",
                     );
                     if self.settings.record {
                        let scale = crate::record::encoder_scale(
                            self.settings.width.min(self.settings.height),
                            self.settings.height,
                        );
                        if scale > 1 {
                            let ew = self.settings.width * scale;
                            let eh = self.settings.height * scale;
                            ui.colored_label(
                                egui::Color32::from_rgb(150, 190, 220),
                                egui::RichText::new(format!(
                                    "Windows' encoder cannot take less than {0}x{0}, so a {1}x{2} recording is stored as {ew}x{eh} (upscaled x{scale}, still pixel exact).",
                                    crate::record::MIN_DIMENSION,
                                    self.settings.width,
                                    self.settings.height
                                ))
                                .small(),
                            );
                        }
                    }
                    if self.settings.record {
                        ui.label(
                            egui::RichText::new(
                                "Only the cursor frames are recorded - no desktop, no other windows - at the cursor's own resolution. The file lands in your Downloads folder. F9 toggles recording while the cursor is on.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                     });
                     });
                     ui.add_space(10.0);
                     // ---------------- start / stop ----------------
                     ui.horizontal(|ui| {
                     if !self.busy() {
                     if ui
                     .add(egui::Button::new(
                     egui::RichText::new("▶  Turn on the doom cursor").strong(),
                        ))
                        .clicked()
                    {
                        self.start();
                    }
                } else if ui
                    .add(egui::Button::new(
                        egui::RichText::new("■  Turn off (or middle-click)").strong(),
                    ))
                    .clicked()
                {
                    self.stop();
                }

                if ui.button("Restore cursor").clicked() {
                     cursor::restore_system_cursors();
                     self.message = "System cursors restored.".into();
                    self.is_error = false;
                }
            });

            if !self.message.is_empty() {
                let color = if self.is_error {
                    egui::Color32::from_rgb(230, 130, 130)
                } else {
                    egui::Color32::from_rgb(150, 200, 150)
                };
                ui.add_space(4.0);
                ui.colored_label(color, &self.message);
            }

            // ---------------- live status ----------------
            if let Some(running) = self.running.as_ref() {
                let st = &running.status;
                ui.add_space(6.0);
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Session").strong());
                    ui.monospace(format!(
                        "state              {}",
                     st.text("state")
                     ));
                     ui.monospace(format!(
                     "fps                {:.1}",
                     st.number("fps")
                     ));
                     ui.monospace(format!(
                     "frames drawn       {}",
                     st.text("frames")
                     ));
                     let blank = match st.get("cursor_blank") {
                     Some("1") => "yes (system cursor blank)",
                     Some("0") => "replaced, but the system cursor is still visible",
                     _ => "unknown",
                     };
                     ui.monospace(format!("pointer invisible  {blank}"));
                     ui.monospace(format!(
                     "recording          {}",
                     if st.boolean("recording") { "yes" } else { "no" }
                     ));
                     let record_path = st.text("record_path");
                     if !record_path.is_empty() {
                     ui.monospace(format!("  file             {record_path}"));
                     }
                     let last = st.text("last_record_path");
                     if !last.is_empty() {
                     ui.monospace(format!("last recording     {last}"));
                     ui.monospace(format!(
                     "                   {} frames",
                     st.text("last_record_frames")
                     ));
                     if ui.button("Open Downloads").clicked() {
                     if let Some(dir) = std::path::Path::new(&last).parent() {
                     let _ = Command::new("explorer").arg(dir).spawn();
                            }
                        }
                    }
                    let error = st.text("record_error");
                     if !error.is_empty() {
                     ui.colored_label(egui::Color32::from_rgb(230, 160, 120), error);
                     }
                     let input_error = st.text("input_error");
                     if !input_error.is_empty() {
                     ui.colored_label(egui::Color32::from_rgb(230, 160, 120), input_error);
                     }
                     if let Some(err) = &running.poll_error {
                     ui.colored_label(egui::Color32::from_rgb(230, 160, 120), err);
                     }
                     });
                     }
                     ui.add_space(10.0);
                     ui.separator();
                     ui.label(egui::RichText::new("Controls").strong());
            ui.label(
                egui::RichText::new(
                    "Arrows: walk (up/down) and turn (left/right)  •  Ctrl: shoot, use doors, confirm
                     F8: pause / resume  •  F9: record  •  Esc: doom menu  •  middle mouse: off",
                     )
                     .small()
                     .weak(),
                     );
                     ui.add_space(4.0);
                     ui.label(
                     egui::RichText::new(
                     "The doom view is click-through: click, drag, scroll and everything else you normally do with the pointer still works. While it is on, only the arrows and Ctrl belong to Doom - WASD, Space, Enter and every other key keep going to the app you are using.",
                     )
                     .small()
                     .weak(),
                     );
                     ui.add_space(4.0);
                     ui.label(
                     egui::RichText::new(
                     "It keeps up with the pointer: it is repositioned several times per frame, so it sits under the pointer instead of trailing behind it.",
                     )
                     .small()
                     .weak(),
                     );
                     ui.add_space(4.0);
                     ui.label(
                     egui::RichText::new(
                     "Typing always works: as soon as a text field has focus (caret or I-beam under the pointer) the game pauses, the normal pointer comes back and every key belongs to that field again. F8 freezes the game and frees the keyboard by hand. Only the arrows and Ctrl are ever captured.

Your normal arrow pointer disappears while the doom cursor is on — the app asks Windows to blank every system cursor, so it is gone everywhere, not only over this window. (A few programs draw their own pointer bitmap and will still show theirs.) Your own cursor scheme is saved before the first blanking and written back when you stop, when the app closes, when it crashes, and by a watchdog process that survives a hard kill; the restore is then verified against the real pointer bitmap. “Restore cursor” does the same by hand.",
                )
                .small()
                .weak(),
            );
            });
        });
    }
}

impl Launcher {
    /// Grabs the window once it has settled, then asks egui to close.
    fn finish_screenshot(&mut self, root: &mut egui::Ui) {
        self.screenshot_frames += 1;
        // Fonts, sliders and the layout only settle after a few frames.
        if self.screenshot_frames < 8 {
            root.ctx().request_repaint();
            return;
        }
        let Some(path) = self.screenshot_target.clone() else {
            return;
        };
        let ctx = root.ctx().clone();
        ctx.request_screenshot(move |image: std::sync::Arc<egui::ColorImage>| {
            let (width, height) = (image.size[0], image.size[1]);
            let mut rgba = Vec::with_capacity(width * height * 4);
            for pixel in &image.pixels {
                rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), 255]);
            }
            match crate::bmp::write_rgba24(&path, &rgba, width, height) {
                Ok(()) => crate::log(&format!(
                    "screenshot: {} ({width}x{height})",
                    path.display()
                )),
                Err(err) => crate::log(&format!("screenshot failed: {err}")),
            }
        });
        root.ctx().request_repaint();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            wad: r"D:\games\DOOM.WAD".into(),
            width: 128,
            height: 80,
            scale_percent: 200,
            anchor: Anchor::TopLeft,
            reticle: true,
            record: false,
            autostart: true,
            capture_keys: true,
            pause_on_typing: true,
            show_preview: true,
            preview_zoom: 4,
            preview_view: 1,
        }
    }

    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    }

    #[test]
    fn worker_command_line_matches_the_worker_parser() {
        let args = session_args(
            &settings(),
            r"Local\CursorDoomStop-1-1",
            std::path::Path::new(r"C:	mp\status.txt"),
            Some(12),
            false,
        );
        assert_eq!(args[0], "--session");
        assert_eq!(
            value_of(&args, "--wad").unwrap(),
            std::path::absolute(r"D:\games\DOOM.WAD")
                .unwrap()
                .display()
                .to_string()
        );
        assert_eq!(value_of(&args, "--width"), Some("128"));
        assert_eq!(value_of(&args, "--height"), Some("80"));
        assert_eq!(value_of(&args, "--scale"), Some("200"));
        assert_eq!(value_of(&args, "--anchor"), Some("topleft"));
        assert_eq!(value_of(&args, "--reticle"), Some("1"));
        assert_eq!(value_of(&args, "--record"), Some("0"));
        assert_eq!(value_of(&args, "--autostart"), Some("1"));
        assert_eq!(value_of(&args, "--max-frames"), Some("12"));
        assert_eq!(
            value_of(&args, "--stop-event"),
            Some(r"Local\CursorDoomStop-1-1")
        );
        assert!(args.iter().any(|a| a == "--no-hide-cursor"));
        assert_eq!(value_of(&args, "--status-file"), Some(r"C:	mp\status.txt"));
    }
    #[test]
    fn hiding_the_cursor_is_the_default() {
        let args = session_args(
            &settings(),
            "event",
            std::path::Path::new("status.txt"),
            None,
            true,
        );
        assert!(!args.iter().any(|a| a == "--no-hide-cursor"));
        assert!(!args.iter().any(|a| a == "--max-frames"));
    }
}
