// Build script: compiles the doomgeneric engine (plain C) into a static
// library that the Rust side links against.
//
// The engine is built with DOOMGENERIC_RESX/RESY = 320x200 (Doom's native
// resolution). The Rust layer takes that framebuffer and downscales it to the
// user selected resolution (<= 256x160) for the cursor.

use std::path::PathBuf;

/// Same source list as doomgeneric's Makefile.win, minus `doomgeneric_win.c`
/// (that file is the Win32 *frontend*: we implement the DG_* callbacks in Rust
/// instead, so it must not be compiled).
const SOURCES: &[&str] = &[
    "dummy.c",
    "am_map.c",
    "doomdef.c",
    "doomstat.c",
    "dstrings.c",
    "d_event.c",
    "d_items.c",
    "d_iwad.c",
    "d_loop.c",
    "d_main.c",
    "d_mode.c",
    "d_net.c",
    "f_finale.c",
    "f_wipe.c",
    "g_game.c",
    "hu_lib.c",
    "hu_stuff.c",
    "info.c",
    "i_cdmus.c",
    "i_endoom.c",
    "i_joystick.c",
    "i_scale.c",
    "i_sound.c",
    "i_system.c",
    "i_timer.c",
    "memio.c",
    "m_argv.c",
    "m_bbox.c",
    "m_cheat.c",
    "m_config.c",
    "m_controls.c",
    "m_fixed.c",
    "m_menu.c",
    "m_misc.c",
    "m_random.c",
    "p_ceilng.c",
    "p_doors.c",
    "p_enemy.c",
    "p_floor.c",
    "p_inter.c",
    "p_lights.c",
    "p_map.c",
    "p_maputl.c",
    "p_mobj.c",
    "p_plats.c",
    "p_pspr.c",
    "p_saveg.c",
    "p_setup.c",
    "p_sight.c",
    "p_spec.c",
    "p_switch.c",
    "p_telept.c",
    "p_tick.c",
    "p_user.c",
    "r_bsp.c",
    "r_data.c",
    "r_draw.c",
    "r_main.c",
    "r_plane.c",
    "r_segs.c",
    "r_sky.c",
    "r_things.c",
    "sha1.c",
    "sounds.c",
    "statdump.c",
    "st_lib.c",
    "st_stuff.c",
    "s_sound.c",
    "tables.c",
    "v_video.c",
    "wi_stuff.c",
    "w_checksum.c",
    "w_file.c",
    "w_main.c",
    "w_wad.c",
    "z_zone.c",
    "w_file_stdc.c",
    "i_input.c",
    "i_video.c",
    "doomgeneric.c",
];

fn main() {
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let engine_dir = match std::env::var_os("DOOMGENERIC_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => manifest.join("..").join("doomgeneric").join("doomgeneric"),
    };
    // NOTE: deliberately *not* canonicalized - `canonicalize()` returns `\\?\D:\...`
    // paths on Windows, which cl.exe cannot open.

    assert!(
        engine_dir.join("doomgeneric.c").exists(),
        "doomgeneric sources not found at {}. Set DOOMGENERIC_DIR to the engine's source directory.",
        engine_dir.display()
    );

    println!("cargo:rerun-if-env-changed=DOOMGENERIC_DIR");
    println!("cargo:rerun-if-changed={}", engine_dir.display());

    let mut build = cc::Build::new();
    build
        .include(&engine_dir)
        .include(manifest.join("compat"))
        .define("DOOMGENERIC_RESX", "320")
        .define("DOOMGENERIC_RESY", "200")
        .define("_CRT_SECURE_NO_WARNINGS", None)
        .define("_CRT_NONSTDC_NO_DEPRECATE", None)
        .define("_USE_MATH_DEFINES", None)
        // Doom's software renderer is not performance critical here but this
        // keeps the 35 Hz loop comfortably fast.
        .opt_level(2)
        .warnings(false);

    if build.get_compiler().is_like_msvc() {
        build.flag_if_supported("/std:c11");
        build.flag_if_supported("/wd4244");
        build.flag_if_supported("/wd4305");
        build.flag_if_supported("/wd4018");
        build.flag_if_supported("/wd4996");
    }

    for src in SOURCES {
        let path = engine_dir.join(src);
        println!("cargo:rerun-if-changed={}", path.display());
        build.file(&path);
    }

    build.compile("doomgeneric_port");

    // Linker map: lets crash addresses be resolved to function names.
    let map_path = std::env::var("OUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("target"))
        .join("cursor-doom.map");
    println!("cargo:rustc-link-arg=/MAP:{}", map_path.display());

    // Doom's platform code pulls in a couple of Win32 libs on its own.
    println!("cargo:rustc-link-lib=user32");
    println!("cargo:rustc-link-lib=gdi32");
    println!("cargo:rustc-link-lib=winmm");
}
