//! Headless verification mode: boots the engine, runs N frames, and writes the
//! frames out as BMPs (plus an MP4 when asked) so the pipeline can be checked
//! without touching the user's cursor or desktop.
//!
//! `cursor-doom.exe --selftest --wad DOOM.WAD --frames 120 --record`

use std::path::PathBuf;
use std::time::Instant;

use crate::{bmp, engine, frame, record};

pub struct Options {
    pub wad: PathBuf,
    pub frames: usize,
    pub out_dir: PathBuf,
    pub width: u32,
    pub height: u32,
    pub scale_percent: u32,
    pub record: bool,
    /// Sleep between ticks so the engine runs at a realistic 35 fps.
    pub real_time: bool,
}

pub fn run(opts: Options) -> Result<(), String> {
    let width = (opts.width.clamp(16, 256) / 2) * 2;
    let height = (opts.height.clamp(16, 160) / 2) * 2;
    let frames = opts.frames.max(1);
    std::fs::create_dir_all(&opts.out_dir)
        .map_err(|e| format!("cannot create {}: {e}", opts.out_dir.display()))?;

    println!(
        "selftest: wad={} frames={} native={}x{} scale={}% out={}",
        opts.wad.display(),
        frames,
        width,
        height,
        opts.scale_percent,
        opts.out_dir.display()
    );

    // Absolute path first: Doom writes config/savegames into the working
    // directory, which we are about to change.
    let wad_path = std::path::absolute(&opts.wad).unwrap_or_else(|_| opts.wad.clone());
    let _ = std::env::set_current_dir(&opts.out_dir);

    let boot = Instant::now();
    engine::start(&wad_path, &[]);
    println!(
        "selftest: engine booted in {:.2}s",
        boot.elapsed().as_secs_f64()
    );

    let downscaler = frame::Downscaler::new(width as usize, height as usize);
    let mut native = vec![0u8; width as usize * height as usize * 4];
    let mut display: Vec<u8> = Vec::new();
    let (dw, dh) = frame::display_size(width as usize, height as usize, opts.scale_percent);

    let mut recorder = if opts.record {
        let path = opts.out_dir.join("selftest.mp4");
        let _ = std::fs::remove_file(&path);
        match record::Recorder::start(&path, width, height) {
            Ok(rec) => {
                println!("selftest: recording to {}", path.display());
                Some(rec)
            }
            Err(err) => {
                println!("selftest: recording unavailable: {err}");
                None
            }
        }
    } else {
        None
    };

    let started = Instant::now();
    let mut dumped = false;
    for index in 0..frames {
        engine::tick();
        let engine_frame = engine::frame();
        if engine_frame.is_empty() {
            return Err("engine produced no framebuffer".into());
        }
        downscaler.run(engine_frame, &mut native);
        frame::build_display(
            &native,
            width as usize,
            height as usize,
            opts.scale_percent,
            Some((dw / 2, dh / 2)),
            &mut display,
        );

        if let Some(rec) = recorder.as_mut() {
            rec.push(&native, started.elapsed())?;
        }

        if opts.real_time {
            std::thread::sleep(engine::FRAME_TIME);
        }

        // Dump the first frame after the engine has settled and the last one.
        if index + 1 == frames / 2 || index + 1 == frames {
            let tag = index + 1;
            bmp::write_bgra24(
                &opts.out_dir.join(format!("engine-{tag}.bmp")),
                engine_frame,
                engine::WIDTH,
                engine::HEIGHT,
            )
            .map_err(|e| format!("bmp: {e}"))?;
            bmp::write_bgra24(
                &opts.out_dir.join(format!("cursor-{tag}.bmp")),
                &native,
                width as usize,
                height as usize,
            )
            .map_err(|e| format!("bmp: {e}"))?;
            bmp::write_bgra24(
                &opts.out_dir.join(format!("display-{tag}.bmp")),
                &display,
                dw,
                dh,
            )
            .map_err(|e| format!("bmp: {e}"))?;
            dumped = true;
            println!("selftest: frame {tag} written");
        }
    }

    let elapsed = started.elapsed();
    println!(
        "selftest: {frames} frames in {:.2}s ({:.1} fps), dumped={dumped}",
        elapsed.as_secs_f64(),
        frames as f64 / elapsed.as_secs_f64().max(1e-6)
    );

    if let Some(rec) = recorder {
        let encoded_frames = rec.frames();
        let path = rec.finish()?;
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        println!(
            "selftest: wrote {} ({:.1} KiB, {} frames)",
            path.display(),
            size as f64 / 1024.0,
            encoded_frames
        );
        // Round-trip: decode the first frame back and dump it for inspection.
        match record::probe_first_frame(&path) {
            Ok((pixels, w, h)) => {
                let probe = opts.out_dir.join("probe.bmp");
                bmp::write_bgra24(&probe, &pixels, w as usize, h as usize)
                    .map_err(|e| format!("bmp: {e}"))?;
                println!(
                    "selftest: decoded first frame {w}x{h} -> {}",
                    probe.display()
                );
            }
            Err(err) => println!("selftest: could not decode recording: {err}"),
        }
    }

    Ok(())
}
