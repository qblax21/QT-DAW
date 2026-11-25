//! Sessions on disk.
//!
//! A session is a plain folder you can copy between computers:
//!
//! ```text
//! My Song/
//!   session.qtdaw     <- readable JSON: tempo, tracks, where each clip sits
//!   audio/            <- one WAV file per recorded take
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const SESSION_FILE: &str = "session.qtdaw";
pub const AUDIO_DIR: &str = "audio";
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub version: u32,
    pub name: String,
    pub sample_rate: u32,
    pub tempo: f32,
    pub beats_per_bar: u32,
    pub master_db: f32,
    pub next_id: u64,
    pub tracks: Vec<TrackModel>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            name: "Untitled".into(),
            sample_rate: 48_000,
            tempo: 120.0,
            beats_per_bar: 4,
            master_db: 0.0,
            next_id: 1,
            tracks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackModel {
    pub id: u64,
    pub name: String,
    /// 0-based interface input this track records from.
    pub input: usize,
    pub volume_db: f32,
    /// -1.0 = hard left, 0.0 = center, 1.0 = hard right.
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    /// Record-arm is not saved: reopening a session should never start recording a track
    /// you forgot about.
    #[serde(skip)]
    pub armed: bool,
    /// Later clips play on top of earlier ones where they overlap (newest take wins).
    pub clips: Vec<ClipModel>,
    /// How many takes have been recorded on this track (for file names).
    pub takes: u32,
}

impl Default for TrackModel {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            input: 0,
            volume_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            armed: false,
            clips: Vec::new(),
            takes: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipModel {
    /// Path relative to the session folder, e.g. `audio/Kick #1.wav`.
    pub file: String,
    /// Timeline position of the clip's first sample, in frames. Can be slightly negative
    /// after latency compensation; the part before 0 is simply never heard.
    pub start: i64,
}

impl Session {
    pub fn new(name: &str, sample_rate: u32) -> Self {
        Self { name: name.to_string(), sample_rate, ..Default::default() }
    }

    pub fn add_track(&mut self, name: Option<String>, input: usize) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let name = name.unwrap_or_else(|| format!("Track {}", self.tracks.len() + 1));
        self.tracks.push(TrackModel { id, name, input, ..Default::default() });
        id
    }

    pub fn track_mut(&mut self, id: u64) -> Option<&mut TrackModel> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Write `session.qtdaw` atomically (write a temp file, then rename), so a crash
    /// mid-save never leaves a half-written session.
    pub fn save(&self, folder: &Path) -> Result<()> {
        fs::create_dir_all(folder.join(AUDIO_DIR))?;
        let json = serde_json::to_string_pretty(self)?;
        let tmp = folder.join(format!("{SESSION_FILE}.tmp"));
        fs::write(&tmp, json)?;
        fs::rename(&tmp, folder.join(SESSION_FILE))
            .with_context(|| format!("saving session in {}", folder.display()))?;
        Ok(())
    }

    pub fn load(folder: &Path) -> Result<Self> {
        let path = folder.join(SESSION_FILE);
        let json = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let session: Session = serde_json::from_str(&json)
            .with_context(|| format!("{} is not a valid session file", path.display()))?;
        Ok(session)
    }

    /// Where a new take for `track` will be written, relative to the session folder.
    pub fn next_take_file(&mut self, track_id: u64) -> Option<String> {
        let track = self.track_mut(track_id)?;
        track.takes += 1;
        Some(format!("{AUDIO_DIR}/{} #{}.wav", safe_file_name(&track.name), track.takes))
    }
}

/// Default folder that holds all sessions: `Documents\QT DAW`.
pub fn sessions_root() -> PathBuf {
    dirs::document_dir().unwrap_or_else(|| PathBuf::from(".")).join("QT DAW")
}

/// Every folder under `root` that contains a session file, newest first.
pub fn list_sessions(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else { return Vec::new() };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter_map(|p| {
            let meta = fs::metadata(p.join(SESSION_FILE)).ok()?;
            Some((meta.modified().ok()?, p))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Turn a track or session name into something Windows and macOS accept as a file name.
pub fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || " -_#()".contains(c) { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim().to_string();
    if trimmed.chars().any(char::is_alphanumeric) { trimmed } else { "Untitled".into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("qtdaw-test-{}", std::process::id()));
        let mut s = Session::new("Test", 48_000);
        let id = s.add_track(Some("Kick".into()), 0);
        let file = s.next_take_file(id).unwrap();
        assert_eq!(file, "audio/Kick #1.wav");
        s.track_mut(id).unwrap().clips.push(ClipModel { file, start: -12 });
        s.save(&dir).unwrap();
        let loaded = Session::load(&dir).unwrap();
        assert_eq!(loaded.tracks[0].clips[0].start, -12);
        assert_eq!(loaded.tracks[0].takes, 1);
        assert!(!loaded.tracks[0].armed);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_names() {
        assert_eq!(safe_file_name("Vox: lead/2"), "Vox_ lead_2");
        assert_eq!(safe_file_name("..."), "Untitled");
    }
}
