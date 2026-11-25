//! Minimal WAD reader: enough to validate a file before handing it to the
//! engine and to tell Doom 1 and Doom 2 style IWADs apart.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct Wad {
    pub path: PathBuf,
    pub kind: String,
    pub lumps: HashSet<String>,
}

impl Wad {
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut file =
            File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let len = file
            .metadata()
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
            .len();

        let mut header = [0u8; 12];
        file.read_exact(&mut header)
            .map_err(|_| format!("{} is too small to be a WAD", path.display()))?;

        let kind = String::from_utf8_lossy(&header[0..4]).to_string();
        if kind != "IWAD" && kind != "PWAD" {
            return Err(format!(
                "{} is not a Doom WAD (header says {kind:?})",
                path.display()
            ));
        }

        let num_lumps = i32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let dir_offset = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);

        if num_lumps <= 0 || num_lumps > 65_536 {
            return Err(format!(
                "{}: implausible lump count {num_lumps}",
                path.display()
            ));
        }
        if u64::from(dir_offset) >= len {
            return Err(format!("{}: directory offset outside file", path.display()));
        }

        let mut dir = vec![0u8; num_lumps as usize * 16];
        file.seek(SeekFrom::Start(u64::from(dir_offset)))
            .map_err(|e| format!("cannot seek: {e}"))?;
        file.read_exact(&mut dir)
            .map_err(|_| format!("{}: truncated WAD directory", path.display()))?;

        let mut lumps = HashSet::with_capacity(num_lumps as usize);
        for entry in dir.as_chunks::<16>().0 {
            let name: Vec<u8> = entry[8..16]
                .iter()
                .copied()
                .take_while(|b| *b != 0)
                .collect();
            let name = String::from_utf8_lossy(&name).to_uppercase();
            if !name.is_empty() {
                lumps.insert(name);
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            kind,
            lumps,
        })
    }

    pub fn has(&self, lump: &str) -> bool {
        self.lumps.contains(lump)
    }

    /// Doom 2 / Final Doom style maps (MAP01...) versus Doom 1 (E1M1...).
    pub fn is_doom2(&self) -> bool {
        self.has("MAP01") && !self.has("E1M1")
    }

    /// Extra argv for "jump straight into a game".
    pub fn warp_args(&self) -> Vec<String> {
        if self.is_doom2() {
            vec!["-warp".into(), "1".into(), "-skill".into(), "3".into()]
        } else if self.has("E1M1") {
            vec![
                "-warp".into(),
                "1".into(),
                "1".into(),
                "-skill".into(),
                "3".into(),
            ]
        } else {
            Vec::new()
        }
    }

    /// Sanity check the lumps the engine dereferences unconditionally.
    pub fn validate(&self) -> Result<(), String> {
        let mut missing = Vec::new();
        for required in ["PLAYPAL", "COLORMAP"] {
            if !self.has(required) {
                missing.push(required);
            }
        }
        if !self.is_doom2() && !self.has("E1M1") {
            missing.push("E1M1/MAP01");
        }
        if !missing.is_empty() {
            return Err(format!(
                "{} does not look like a playable IWAD (missing {})",
                self.path.display(),
                missing.join(", ")
            ));
        }
        if self.kind == "PWAD" {
            // A PWAD is a mod, not a complete game: the engine needs an IWAD.
            return Err(format!(
                "{} is a PWAD (mod). Pick a full IWAD such as doom.wad / doom2.wad.",
                self.path.display()
            ));
        }
        Ok(())
    }

    pub fn describe(&self) -> String {
        let game = if self.is_doom2() {
            "Doom 2 / Final Doom style"
        } else {
            "Doom 1 style"
        };
        format!(
            "{} \u{2022} {} lumps \u{2022} {}",
            self.kind,
            self.lumps.len(),
            game
        )
    }
}

/// Looks for an IWAD next to the executable / in the usual places.
pub fn find_default_wad() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("DOOM.WAD"));
            candidates.push(dir.join("doom.wad"));
            candidates.push(dir.join("DOOM2.WAD"));
            candidates.push(dir.join("doom2.wad"));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join("DOOM.WAD"));
                candidates.push(parent.join("doom.wad"));
                candidates.push(parent.join("DOOM2.WAD"));
                if let Some(grand) = parent.parent() {
                    candidates.push(grand.join("DOOM.WAD"));
                    candidates.push(grand.join("doom.wad"));
                }
            }
        }
    }
    candidates.push(PathBuf::from("DOOM.WAD"));
    candidates.into_iter().find(|p| p.is_file())
}
