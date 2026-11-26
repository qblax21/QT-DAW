//! Sessions on disk.
//!
//! A session is a plain folder you can copy between computers:
//!
//! ```text
//! My Song/
//!   session.qtdaw     <- readable JSON: tempo, tracks, where each clip sits
//!   audio/            <- one WAV file per recorded take
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::fx::{EffectKind, MAX_PARAMS, Params};

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
    /// Shared by tracks and buses, so every id is unique. Starts at 1: id 0 means "Master".
    pub next_id: u64,
    pub tracks: Vec<TrackModel>,
    /// Aux buses: tracks can be routed into them (e.g. a drum bus) or send a copy to them
    /// (e.g. a reverb bus). Buses always feed the master.
    pub buses: Vec<BusModel>,
    /// Effects on the master, after the master fader (so a limiter's ceiling always holds).
    pub master_inserts: Vec<InsertModel>,
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
            buses: Vec::new(),
            master_inserts: Vec::new(),
        }
    }
}

/// The id that means "the master output" wherever a track's output is stored.
pub const MASTER: u64 = 0;

/// How many buses a session can have (the engine keeps a mixing buffer for each).
pub const MAX_BUSES: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BusModel {
    pub id: u64,
    pub name: String,
    pub volume_db: f32,
    /// Balance: -1.0 = left only, 0.0 = both, 1.0 = right only.
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    /// Effects, before the bus fader.
    pub inserts: Vec<InsertModel>,
}

impl Default for BusModel {
    fn default() -> Self {
        Self { id: 0, name: String::new(), volume_db: 0.0, pan: 0.0, mute: false, solo: false, inserts: Vec::new() }
    }
}

/// The most effects one track, bus or the master can hold.
pub const MAX_INSERTS: usize = 8;

/// One effect in a chain ("insert").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InsertModel {
    /// Unique within the session (same counter as tracks and buses).
    pub id: u64,
    pub kind: EffectKind,
    #[serde(default)]
    pub bypass: bool,
    /// Settings by name. Missing ones use the default, so sessions keep loading when a
    /// later version adds a setting.
    #[serde(default)]
    pub params: BTreeMap<String, f32>,
}

impl InsertModel {
    pub fn new(id: u64, kind: EffectKind, values: &Params) -> Self {
        let params = kind.params().iter().zip(values).map(|(s, v)| (s.key.to_string(), s.clamp(*v))).collect();
        Self { id, kind, bypass: false, params }
    }

    /// All settings, indexed like kind.params().
    pub fn values(&self) -> Params {
        let mut p = [0.0; MAX_PARAMS];
        for (v, s) in p.iter_mut().zip(self.kind.params()) {
            *v = s.clamp(self.params.get(s.key).copied().unwrap_or(s.default));
        }
        p
    }

    pub fn get(&self, index: usize) -> f32 {
        self.values()[index]
    }

    pub fn set(&mut self, index: usize, v: f32) {
        if let Some(s) = self.kind.params().get(index) {
            self.params.insert(s.key.to_string(), s.clamp(v));
        }
    }
}

/// A copy of a track's signal sent to a bus, at its own level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SendModel {
    pub bus: u64,
    pub level_db: f32,
    /// Pre-fader sends ignore the track's volume fader (useful for headphone mixes).
    #[serde(default)]
    pub pre_fader: bool,
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
    /// Where the track plays to: [`MASTER`] or a bus id.
    pub output: u64,
    pub sends: Vec<SendModel>,
    /// Effects, before the fader (so pre-fader sends hear them too).
    pub inserts: Vec<InsertModel>,
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
            output: MASTER,
            sends: Vec::new(),
            inserts: Vec::new(),
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

    /// Add a bus (`None` = "Bus N"). Returns `None` when the session already has
    /// [`MAX_BUSES`].
    pub fn add_bus(&mut self, name: Option<String>) -> Option<u64> {
        if self.buses.len() >= MAX_BUSES {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        let name = name.unwrap_or_else(|| format!("Bus {}", self.buses.len() + 1));
        self.buses.push(BusModel { id, name, ..Default::default() });
        Some(id)
    }

    /// Remove a bus. Tracks that played into it go back to the master; sends to it are removed.
    pub fn remove_bus(&mut self, id: u64) {
        self.buses.retain(|b| b.id != id);
        for t in &mut self.tracks {
            if t.output == id {
                t.output = MASTER;
            }
            t.sends.retain(|s| s.bus != id);
        }
    }

    /// A new effect with the next free id.
    pub fn new_insert(&mut self, kind: EffectKind, values: &Params) -> InsertModel {
        let id = self.next_id;
        self.next_id += 1;
        InsertModel::new(id, kind, values)
    }

    /// The effect with this id, wherever it is.
    pub fn insert_mut(&mut self, id: u64) -> Option<&mut InsertModel> {
        self.tracks
            .iter_mut()
            .flat_map(|t| t.inserts.iter_mut())
            .chain(self.buses.iter_mut().flat_map(|b| b.inserts.iter_mut()))
            .chain(self.master_inserts.iter_mut())
            .find(|i| i.id == id)
    }

    /// Display name for an output id ("Master" or the bus name).
    pub fn output_name(&self, id: u64) -> &str {
        if id == MASTER {
            return "Master";
        }
        self.buses.iter().find(|b| b.id == id).map_or("Master", |b| b.name.as_str())
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
    fn removing_a_bus_reroutes_tracks() {
        let mut s = Session::new("Test", 48_000);
        let t = s.add_track(None, 0);
        let bus = s.add_bus(Some("Drums".into())).unwrap();
        let verb = s.add_bus(None).unwrap();
        assert_ne!(bus, t);
        let track = s.track_mut(t).unwrap();
        track.output = bus;
        track.sends.push(SendModel { bus, level_db: 0.0, pre_fader: false });
        track.sends.push(SendModel { bus: verb, level_db: -10.0, pre_fader: false });
        assert_eq!(s.output_name(bus), "Drums");
        s.remove_bus(bus);
        let track = s.track_mut(t).unwrap();
        assert_eq!(track.output, MASTER);
        assert_eq!(track.sends.len(), 1);
        assert_eq!(s.output_name(verb), "Bus 2");
    }

    #[test]
    fn old_sessions_still_load() {
        let json = r#"{"name":"Old","tracks":[{"id":1,"name":"Kick","clips":[]}]}"#;
        let s: Session = serde_json::from_str(json).unwrap();
        assert_eq!(s.tracks[0].output, MASTER);
        assert!(s.buses.is_empty());
    }

    #[test]
    fn inserts_save_by_name_and_fill_in_defaults() {
        let mut s = Session::new("Test", 48_000);
        let t = s.add_track(None, 0);
        let mut eq = s.new_insert(EffectKind::Eq, &EffectKind::Eq.defaults());
        eq.set(crate::fx::eq::LOW_CUT, 90.0);
        let eq_id = eq.id;
        s.track_mut(t).unwrap().inserts.push(eq);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"low_cut\":90.0"), "{json}");
        assert!(json.contains("\"kind\":\"eq\""));
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tracks[0].inserts[0].get(crate::fx::eq::LOW_CUT), 90.0);
        assert!(s.insert_mut(eq_id).is_some());

        // A setting missing from the file comes back as its default.
        let json = r#"{"id":5,"kind":"compressor","params":{"ratio":8.0}}"#;
        let ins: InsertModel = serde_json::from_str(json).unwrap();
        assert_eq!(ins.values()[1], 8.0);
        assert_eq!(ins.values()[0], -18.0);
    }

    #[test]
    fn file_names() {
        assert_eq!(safe_file_name("Vox: lead/2"), "Vox_ lead_2");
        assert_eq!(safe_file_name("..."), "Untitled");
    }
}
