//! Backup and restore of the user's Windows cursor scheme.
//!
//! `SetSystemCursor` does not only change the cursors of the current session: it
//! overwrites the values of `HKCU\Control Panel\Cursors`, which is what
//! `SystemParametersInfo(SPI_SETCURSORS)` reloads. A blanked set therefore
//! survives the "restore" call and the pointer stays invisible - the exact bug
//! this module exists to prevent.
//!
//! So: read the scheme before blanking anything, keep a copy on disk in
//! `%APPDATA%\CursorDoom\cursor-scheme.txt`, and write it back (followed by
//! `SPI_SETCURSORS`) when the session ends - from the session itself, from the
//! launcher, or from the guardian after a hard kill.

use std::fs;
use std::path::PathBuf;

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    KEY_WRITE, REG_EXPAND_SZ, REG_SZ,
};

const KEY: &str = r"Control Panel\Cursors";

/// The cursor scheme as stored by Windows: named string values such as `Arrow`,
/// `IBeam` or `Hand`.
#[derive(Clone, Debug, Default)]
pub struct CursorScheme {
    pub values: Vec<(String, String)>,
    pub types: Vec<u32>,
}

impl CursorScheme {
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// `name<TAB>type<TAB>value`, one per line. Tab separated so cursor paths
    /// that contain spaces (or `=`) survive.
    pub fn to_text(&self) -> String {
        let mut out = String::from("# Cursor Doom cursor-scheme backup\n");
        for (i, (name, value)) in self.values.iter().enumerate() {
            let ty = self.types.get(i).copied().unwrap_or(1);
            out.push_str(&format!("{name}\t{ty}\t{value}\n"));
        }
        out
    }

    pub fn from_text(text: &str) -> Self {
        let mut scheme = CursorScheme::default();
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let mut parts = line.splitn(3, '\t');
            let (Some(name), Some(ty), Some(value)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            scheme.values.push((name.to_string(), value.to_string()));
            scheme.types.push(ty.parse().unwrap_or(1));
        }
        scheme
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open(write: bool) -> Option<HKEY> {
    let path = wide(KEY);
    let access = if write {
        KEY_READ | KEY_WRITE
    } else {
        KEY_READ
    };
    let mut key = HKEY::default();
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            Some(0),
            access,
            &mut key,
        )
    };
    (rc == ERROR_SUCCESS).then_some(key)
}

/// Reads the scheme Windows currently has stored. Call this *before* blanking.
pub fn read() -> Option<CursorScheme> {
    let key = open(false)?;
    let mut scheme = CursorScheme::default();
    let mut index = 0u32;
    loop {
        let mut name = [0u16; 256];
        let mut name_len = name.len() as u32;
        let mut kind = 0u32;
        let mut data = vec![0u8; 4096];
        let mut data_len = data.len() as u32;
        let rc = unsafe {
            RegEnumValueW(
                key,
                index,
                Some(windows::core::PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut data_len),
            )
        };
        if rc != ERROR_SUCCESS {
            break;
        }
        index += 1;
        let name = String::from_utf16_lossy(&name[..name_len as usize]);
        if name.is_empty() {
            continue; // the scheme name lives in the default value
        }
        if kind != REG_SZ.0 && kind != REG_EXPAND_SZ.0 {
            continue;
        }
        let units: Vec<u16> = (0..data_len as usize / 2)
            .map(|i| u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]))
            .take_while(|unit| *unit != 0)
            .collect();
        let value = String::from_utf16_lossy(&units);
        scheme.values.push((name, value));
        scheme.types.push(kind);
    }
    let _ = unsafe { RegCloseKey(key) };
    (!scheme.is_empty()).then_some(scheme)
}

/// Writes a scheme back into the registry. `SPI_SETCURSORS` must be called
/// afterwards for it to take effect.
pub fn write(scheme: &CursorScheme) -> bool {
    let Some(key) = open(true) else {
        return false;
    };
    let mut written = 0usize;
    for (i, (name, value)) in scheme.values.iter().enumerate() {
        let name_w = wide(name);
        let kind = match scheme.types.get(i).copied() {
            Some(2) => REG_EXPAND_SZ,
            _ => REG_SZ,
        };
        let bytes: Vec<u8> = wide(value)
            .iter()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let rc = unsafe { RegSetValueExW(key, PCWSTR(name_w.as_ptr()), None, kind, Some(&bytes)) };
        if rc == ERROR_SUCCESS {
            written += 1;
        }
    }
    let _ = unsafe { RegCloseKey(key) };
    written > 0
}

pub fn backup_path() -> PathBuf {
    crate::status::app_dir().join("cursor-scheme.txt")
}

/// Reads the live scheme and stores a copy next to the session status file.
pub fn save_backup() -> Option<CursorScheme> {
    let scheme = read()?;
    if let Some(dir) = backup_path().parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(backup_path(), scheme.to_text());
    Some(scheme)
}

pub fn load_backup() -> Option<CursorScheme> {
    let text = fs::read_to_string(backup_path()).ok()?;
    let scheme = CursorScheme::from_text(&text);
    (!scheme.is_empty()).then_some(scheme)
}

/// Puts the backed-up scheme back and reloads the system cursors.
pub fn restore_backup() -> bool {
    let Some(scheme) = load_backup() else {
        return false;
    };
    write(&scheme)
}

#[cfg(test)]
mod tests {
    use super::CursorScheme;

    #[test]
    fn scheme_round_trips_through_the_backup_file() {
        let scheme = CursorScheme {
            values: vec![
                ("Arrow".into(), r"C:\WINDOWS\cursors\aero_arrow.cur".into()),
                ("IBeam".into(), String::new()),
                (
                    "Hand".into(),
                    r"C:\Program Files\Cursors\my hand=1.cur".into(),
                ),
            ],
            types: vec![1, 1, 2],
        };
        let text = scheme.to_text();
        let back = CursorScheme::from_text(&text);
        assert_eq!(back.values, scheme.values);
        assert_eq!(back.types, scheme.types);
    }

    #[test]
    fn blank_backup_parses_to_nothing() {
        assert!(CursorScheme::from_text("# only a comment\n\n").is_empty());
    }
}
