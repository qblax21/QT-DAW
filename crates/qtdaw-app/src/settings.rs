//! App settings that belong to this computer, not to a session (audio device, last session).
//! Stored as JSON in `%APPDATA%\QT DAW\settings.json`.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// "ASIO" or "WASAPI"; `None` = system default.
    pub host: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    /// Driver buffer size in frames; `None` = driver default.
    pub buffer: Option<u32>,
    /// Extra recording offset in milliseconds (+ moves recordings earlier).
    pub record_adjust_ms: f32,
    pub last_session: Option<PathBuf>,
    pub click_volume: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            host: None,
            input: None,
            output: None,
            buffer: None,
            record_adjust_ms: 0.0,
            last_session: None,
            click_volume: 0.5,
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("QT DAW").join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        path()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(p, json);
        }
    }
}
