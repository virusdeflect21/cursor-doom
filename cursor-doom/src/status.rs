//! Session status shared between the worker process and the launcher.
//!
//! The worker rewrites a small `key=value` file; the launcher polls it to show
//! the live state (fps, recording path, errors).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Default, Clone, Debug)]
pub struct SessionStatus {
    map: BTreeMap<String, String>,
}

impl SessionStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, key: &str, value: impl ToString) {
        self.map.insert(key.to_string(), value.to_string());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    pub fn text(&self, key: &str) -> String {
        self.get(key).unwrap_or_default().to_string()
    }

    pub fn number(&self, key: &str) -> f64 {
        self.get(key)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    }

    pub fn boolean(&self, key: &str) -> bool {
        matches!(self.get(key), Some("1") | Some("true") | Some("yes"))
    }

    pub fn parse(text: &str) -> Self {
        let mut status = Self::new();
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                status.map.insert(key.trim().to_string(), value.to_string());
            }
        }
        status
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut text = String::new();
        for (key, value) in &self.map {
            // values are single line (paths may contain '\' but not '\n')
            text.push_str(key);
            text.push('=');
            text.push_str(&value.replace('\n', " "));
            text.push('\n');
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, text)
    }

    pub fn load(path: &Path) -> Option<Self> {
        std::fs::read_to_string(path)
            .ok()
            .map(|text| Self::parse(&text))
    }
}

/// `%APPDATA%\CursorDoom` (or the temp dir as a fallback).
pub fn app_dir() -> PathBuf {
    if let Some(dir) = dirs::data_dir() {
        dir.join("CursorDoom")
    } else {
        std::env::temp_dir().join("CursorDoom")
    }
}

pub fn status_path() -> PathBuf {
    app_dir().join("session.status")
}

pub fn log_path() -> PathBuf {
    app_dir().join("session.log")
}
