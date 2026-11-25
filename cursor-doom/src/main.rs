//! Cursor Doom: doomgeneric running as the mouse cursor.
//!
//! Modes:
//!   (no args)                          the launcher
//!   --session [options]                the worker that owns the overlay + engine
//!   --selftest --wad <wad> [options]   headless frame dump / recording check
//!   --probe <file.mp4>                 decode the first frame of a recording
//!
//! The launcher spawns `--session` as a child process so a crash in the C engine
//! cannot leave the desktop without a pointer.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bmp;
mod crash;
mod cursor;
mod engine;
mod frame;
mod guardian;
mod hooks;
mod keys;
mod launcher;
mod overlay;
mod preview;
mod record;
mod scheme;
mod selftest;
mod session;
mod settings;
mod status;
mod textmode;
mod wad;

use session::log;

use std::path::PathBuf;

fn main() {
    crash::install();

    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");

    match mode {
        "--screenshot" => {
            let Some(path) = value(&args, "--screenshot") else {
                eprintln!("--screenshot needs a path");
                return;
            };
            if let Err(err) = launcher::run_with(Some(PathBuf::from(path))) {
                eprintln!("could not open the launcher: {err}");
            }
        }
        "--session" => run_session(&args),
        "--selftest" => run_selftest(&args),
        "--probe" => run_probe(&args),
        "--cursor-report" => match cursor::cursor_report() {
            Some(report) => {
                println!("cursor handle : 0x{:X}", report.handle);
                println!(
                    "size         : {}x{} (monochrome: {})",
                    report.width, report.height, report.monochrome
                );
                println!("showing      : {}", report.showing);
                println!("invisible    : {}", report.is_blank());
            }
            None => println!("cursor information unavailable in this session"),
        },
        "--cursor-dump" => {
            let path = value(&args, "--cursor-dump").unwrap_or_else(|| "cursor-dump.bmp".into());
            match cursor::capture_current_cursor() {
                Some(image) => {
                    println!("size     : {}x{}", image.width, image.height);
                    println!(
                        "coverage : {:.1}% of the box is drawn",
                        image.coverage() * 100.0
                    );
                    println!("pointer:");
                    for line in image.ink_map(32, 16) {
                        println!("  {line}");
                    }
                    let bgra = image.to_bgra_on_checkerboard();
                    match bmp::write_bgra24(
                        std::path::Path::new(&path),
                        &bgra,
                        image.width,
                        image.height,
                    ) {
                        Ok(()) => println!("wrote {path} (blended over a checkerboard)"),
                        Err(err) => println!("could not write {path}: {err}"),
                    }
                }
                None => println!("no cursor bitmap: Windows reports no pointer in this session"),
            }
        }
        "--guardian" => {
            let pid = number(&args, "--watch-pid", 0u32);
            if let Err(err) = guardian::watch(pid) {
                eprintln!("guardian: {err}");
            }
        }
        "--restore-cursor" => {
            let backup = scheme::load_backup();
            println!(
                "saved scheme : {}",
                match &backup {
                    Some(s) => format!("{} cursor entries", s.values.len()),
                    None => "none yet".to_string(),
                }
            );
            cursor::restore_system_cursors();
            match cursor::capture_current_cursor() {
                Some(image) => println!(
                    "pointer      : {:.1}% of the box is drawn{}",
                    image.coverage() * 100.0,
                    if image.coverage() > 0.004 {
                        " (visible)"
                    } else {
                        " (still blank - try again)"
                    }
                ),
                None => println!("pointer      : no cursor reported in this session"),
            }
        }
        "--help" | "-h" | "/?" => print_help(),
        "" | "--launcher" => {
            if let Err(err) = launcher::run() {
                eprintln!("launcher failed: {err}");
                std::process::exit(1);
            }
        }
        other => {
            eprintln!("unknown argument {other:?}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    print!(
        r#"Cursor Doom - doomgeneric as your mouse cursor

Usage:
  cursor-doom                       open the launcher
  cursor-doom --session --wad <wad> [--width 256] [--height 160] [--scale 100]
                              [--anchor center|topleft] [--reticle 0|1] [--record 0|1]
                              [--autostart 0|1] [--no-hide-cursor] [--no-capture]
                              [--no-typing-pause] [--status-file <path>]
                              [--stop-event <name>] [--parent-pid <pid>] [--max-frames <n>]
  cursor-doom --selftest --wad <wad> [--frames 90] [--out <dir>] [--record] [--real-time]
  cursor-doom --probe <file.mp4> --out <dir>
  cursor-doom --cursor-report
  cursor-doom --cursor-dump <file.bmp>
  cursor-doom --restore-cursor

Resolution:
  --width 16..256, --height 16..160 (even numbers), default 256x160. Doom renders
  320x200; the frame is scaled down to this size, then up by --scale percent.

While the doom cursor is on:
  it replaces the pointer (every system cursor is blanked, so the arrow disappears
  everywhere) and follows it exactly; clicks, drags and scrolls still reach the
  window underneath. Up/Down walk, Left/Right turn, Ctrl shoots, uses doors and
  confirms menu entries, Esc opens doom's menu, middle mouse turns the cursor off.
  Only the arrows and Ctrl are captured; every other key keeps working.

Typing:
  while a text field has focus (caret or I-beam) the game pauses, the normal
  pointer comes back and the keyboard is left alone, so you can always type.
  F8 freezes the game by hand, F9 toggles recording.

Diagnostics:
  --cursor-report       which cursor Windows shows, and whether it is blank
  --cursor-dump <bmp>   the pointer bitmap as a BMP, plus a terminal picture
  --restore-cursor      put the normal Windows cursors back
  --guardian --watch-pid <pid>
                        watchdog that restores the cursor when a session dies
  --no-overlay          run the engine without the cursor overlay
  --no-hooks            run without the global keyboard/mouse hooks
  --no-hide-cursor      keep the normal pointer on screen

Recording:
  every cursor size from 16x16 up records; sizes the H.264 encoder refuses are
  upscaled by a whole number (16x16 -> 48x48). Recordings land in Downloads as
  cursor-doom-<date>.mp4, and the launcher can preview them live, zoomed in.

Logs and status: %APPDATA%\CursorDoom
"#
    );
}

fn value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn number<T: std::str::FromStr>(args: &[String], flag: &str, fallback: T) -> T {
    value(args, flag)
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(fallback)
}

fn flag(args: &[String], name: &str) -> bool {
    match value(args, name) {
        Some(v) => matches!(v.as_str(), "1" | "true" | "yes" | "on"),
        None => false,
    }
}

fn run_session(args: &[String]) {
    let status_path = value(args, "--status-file")
        .map(PathBuf::from)
        .unwrap_or_else(status::status_path);

    let Some(wad) = value(args, "--wad") else {
        eprintln!("--session needs --wad <path>");
        std::process::exit(2);
    };

    let cfg = session::SessionConfig {
        wad: PathBuf::from(wad),
        width: number(args, "--width", 256u32),
        height: number(args, "--height", 160u32),
        scale_percent: number(args, "--scale", 100u32),
        anchor: session::Anchor::parse(&value(args, "--anchor").unwrap_or_default()),
        reticle: flag(args, "--reticle"),
        record: flag(args, "--record"),
        autostart: flag(args, "--autostart"),
        hide_cursor: !args.iter().any(|a| a == "--no-hide-cursor"),
        max_frames: number(args, "--max-frames", 0u64),
        no_overlay: args.iter().any(|a| a == "--no-overlay"),
        no_hooks: args.iter().any(|a| a == "--no-hooks"),
        capture_keys: !args.iter().any(|a| a == "--no-capture"),
        pause_on_typing: !args.iter().any(|a| a == "--no-typing-pause"),
        status_file: Some(status_path.clone()),
        stop_event: value(args, "--stop-event"),
        parent_pid: value(args, "--parent-pid").and_then(|v| v.parse::<u32>().ok()),
    };

    match session::run(cfg) {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("[cursor-doom] session failed: {err}");
            let mut status = status::SessionStatus::load(&status_path).unwrap_or_default();
            status.set("state", "error");
            status.set("error", &err);
            let _ = status.save(&status_path);
            cursor::restore_system_cursors();
            std::process::exit(3);
        }
    }
}

fn run_selftest(args: &[String]) {
    let Some(wad) = value(args, "--wad") else {
        eprintln!("--selftest needs --wad <path>");
        std::process::exit(2);
    };
    let out_dir = value(args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("cursor-doom-selftest"));

    let options = selftest::Options {
        wad: PathBuf::from(wad),
        frames: number(args, "--frames", 90usize),
        out_dir,
        width: number(args, "--width", 256u32),
        height: number(args, "--height", 160u32),
        scale_percent: number(args, "--scale", 100u32),
        record: args.iter().any(|a| a == "--record"),
        real_time: args.iter().any(|a| a == "--real-time"),
    };

    match selftest::run(options) {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("selftest failed: {err}");
            std::process::exit(1);
        }
    }
}

fn run_probe(args: &[String]) {
    let Some(path) = value(args, "--probe").or_else(|| {
        // also accept `--probe file.mp4`
        args.iter()
            .position(|a| a == "--probe")
            .and_then(|i| args.get(i + 1))
            .cloned()
    }) else {
        eprintln!("--probe needs an mp4 path");
        std::process::exit(2);
    };
    let out_dir = value(args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    match record::probe_first_frame(&PathBuf::from(path)) {
        Ok((pixels, width, height)) => {
            let out = out_dir.join("probe.bmp");
            match bmp::write_bgra24(&out, &pixels, width as usize, height as usize) {
                Ok(()) => {
                    println!("decoded {width}x{height} -> {}", out.display());
                    std::process::exit(0);
                }
                Err(err) => {
                    eprintln!("cannot write {}: {err}", out.display());
                    std::process::exit(1);
                }
            }
        }
        Err(err) => {
            eprintln!("probe failed: {err}");
            std::process::exit(1);
        }
    }
}
