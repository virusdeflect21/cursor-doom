//! Launcher settings, persisted as a tiny `key=value` file.

use std::path::PathBuf;

use crate::session::Anchor;
use crate::status::{app_dir, SessionStatus};
use crate::wad;

#[derive(Clone, Debug)]
pub struct Settings {
    pub wad: String,
    pub width: u32,
    pub height: u32,
    pub scale_percent: u32,
    pub anchor: Anchor,
    pub reticle: bool,
    pub record: bool,
    pub autostart: bool,
    /// Take WASD / arrows / Right Ctrl away from the desktop while playing.
    pub capture_keys: bool,
    /// Pause and hand the pointer back while the user is typing somewhere.
    pub pause_on_typing: bool,
    /// Show the cursor frames magnified in the launcher.
    pub show_preview: bool,
    /// Magnification used by that view.
    pub preview_zoom: u32,
    /// Which view the preview shows: 0 native, 1 cursor pixels, 2 on screen.
    pub preview_view: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            wad: wad::find_default_wad()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            // 256x160 is Doom's 16:10 image scaled down from 320x200.
            width: 256,
            height: 160,
            scale_percent: 100,
            anchor: Anchor::Center,
            reticle: true,
            record: false,
            autostart: false,
            capture_keys: true,
            pause_on_typing: true,
            show_preview: true,
            preview_zoom: 4,
            preview_view: 1,
        }
    }
}

impl Settings {
    fn path() -> PathBuf {
        app_dir().join("settings.txt")
    }

    pub fn load() -> Self {
        let mut settings = Self::default();
        let Some(status) = SessionStatus::load(&Self::path()) else {
            return settings;
        };
        if let Some(wad) = status.get("wad") {
            if !wad.is_empty() {
                settings.wad = wad.to_string();
            }
        }
        if let Ok(value) = status.text("width").parse::<u32>() {
            settings.width = value.clamp(16, 256) & !1;
        }
        if let Ok(value) = status.text("height").parse::<u32>() {
            settings.height = value.clamp(16, 160) & !1;
        }
        if let Ok(value) = status.text("scale").parse::<u32>() {
            settings.scale_percent = value.clamp(25, 400);
        }
        settings.anchor = Anchor::parse(&status.text("anchor"));
        if status.get("reticle").is_some() {
            settings.reticle = status.boolean("reticle");
        }
        if status.get("record").is_some() {
            settings.record = status.boolean("record");
        }
        if status.get("autostart").is_some() {
            settings.autostart = status.boolean("autostart");
        }
        if status.get("capture_keys").is_some() {
            settings.capture_keys = status.boolean("capture_keys");
        }
        if status.get("pause_on_typing").is_some() {
            settings.pause_on_typing = status.boolean("pause_on_typing");
        }
        if status.get("show_preview").is_some() {
            settings.show_preview = status.boolean("show_preview");
        }
        if let Ok(value) = status.text("preview_zoom").parse::<u32>() {
            settings.preview_zoom = value.clamp(1, 16);
        }
        if let Ok(value) = status.text("preview_view").parse::<u32>() {
            settings.preview_view = value.min(2);
        }
        settings
    }

    pub fn save(&self) {
        let mut status = SessionStatus::new();
        status.set("wad", &self.wad);
        status.set("width", self.width);
        status.set("height", self.height);
        status.set("scale", self.scale_percent);
        status.set("anchor", self.anchor.as_str());
        status.set("reticle", if self.reticle { "1" } else { "0" });
        status.set("record", if self.record { "1" } else { "0" });
        status.set("autostart", if self.autostart { "1" } else { "0" });
        status.set("capture_keys", if self.capture_keys { "1" } else { "0" });
        status.set(
            "pause_on_typing",
            if self.pause_on_typing { "1" } else { "0" },
        );
        status.set("show_preview", if self.show_preview { "1" } else { "0" });
        status.set("preview_zoom", self.preview_zoom.clamp(1, 16));
        status.set("preview_view", self.preview_view.min(2));
        let _ = status.save(&Self::path());
    }
}
