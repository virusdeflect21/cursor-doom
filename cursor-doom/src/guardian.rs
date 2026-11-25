//! Cursor guardian.
//!
//! The session blanks every system cursor with `SetSystemCursor`, which is a
//! *system wide* change: it stays in effect even after the process that made it
//! is gone. A normal exit, a crash detected by our handler and a launcher kill
//! all restore the cursors - but a hard kill (`taskkill /F`, a wedged driver, a
//! blue screen survivor, ...) cannot run any teardown code.
//!
//! So the session spawns this tiny watchdog as a *separate process*: it waits on
//! the session's process handle and puts the cursor back the moment the session
//! disappears, however that happens.

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

use crate::cursor;

/// Waits for `pid` to exit, then restores the system cursors.
///
/// Returns an error only when the process could not be watched at all (already
/// gone, access denied); in that case the cursor is restored immediately so we
/// never leave a blank pointer behind.
pub fn watch(pid: u32) -> Result<(), String> {
    if pid == 0 {
        cursor::restore_system_cursors();
        return Ok(());
    }

    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
        .map_err(|e| format!("cannot watch process {pid}: {e}"))?;

    unsafe {
        let result = WaitForSingleObject(HANDLE(handle.0), u32::MAX);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if result == WAIT_OBJECT_0 {
            cursor::restore_system_cursors();
            return Ok(());
        }
    }

    // Could not wait (the process is already gone): restore right away.
    cursor::restore_system_cursors();
    Ok(())
}

/// Path of the guardian executable: a copy of this program under its own name.
///
/// Keeping it as `cursor-guard.exe` matters: if something kills every
/// `cursor-doom.exe` (`taskkill /IM cursor-doom.exe`, a stuck launcher, a
/// crash), the guardian survives under its own name and still puts the cursor
/// back. A guardian that shares the name would die with the session.
fn guardian_exe() -> Result<std::path::PathBuf, String> {
    let current = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    if current
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.eq_ignore_ascii_case("cursor-guard.exe"))
        .unwrap_or(false)
    {
        return Ok(current);
    }

    let dir = crate::status::app_dir();
    let copy = dir.join("cursor-guard.exe");
    let stale = match (std::fs::metadata(&copy), std::fs::metadata(&current)) {
        (Ok(a), Ok(b)) => a.len() != b.len() || a.modified().ok() != b.modified().ok(),
        _ => true,
    };
    if stale {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        std::fs::copy(&current, &copy).map_err(|e| format!("cannot copy the guardian: {e}"))?;
    }
    Ok(copy)
}

/// Starts a guardian for this process. The child is deliberately detached: it
/// must outlive us, so we never wait for it and never kill it.
pub fn spawn() -> Result<(), String> {
    let exe = guardian_exe()?;
    std::process::Command::new(exe)
        .arg("--guardian")
        .arg("--watch-pid")
        .arg(std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not start the cursor guardian: {e}"))
}
