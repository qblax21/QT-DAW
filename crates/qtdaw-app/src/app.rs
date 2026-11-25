//! App state and the actions that change it. Drawing lives in `ui.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui;
use qtdaw_engine::clip::ClipAudio;
use qtdaw_engine::devices::{self, DeviceInfo};
use qtdaw_engine::engine::{
    AtomicF32, BusRt, ClipRt, Engine, EngineConfig, Request, SendRt, Shared, TrackRt, db_to_gain,
};
use qtdaw_engine::recorder::{RecorderEvent, TakeSpec};
use qtdaw_engine::session::{self, ClipModel, Session};

use crate::settings::Settings;

/// What a meter shows: the level (jumps up, falls smoothly), a peak-hold line that waits
/// before falling, and a clip light that stays on until clicked.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeterState {
    pub level: f32,
    pub hold: f32,
    hold_since: f64,
    pub clipped: bool,
}

impl MeterState {
    const FALL: f32 = 0.85;
    const HOLD_SECS: f64 = 1.5;

    pub fn update(&mut self, peak: f32, now: f64) {
        self.level = peak.max(self.level * Self::FALL);
        if self.level >= self.hold {
            self.hold = self.level;
            self.hold_since = now;
        } else if now - self.hold_since > Self::HOLD_SECS {
            self.hold = (self.hold * 0.92).max(self.level);
        }
        if peak > 1.0 {
            self.clipped = true;
        }
    }
}

/// Which big view fills the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainView {
    Tracks,
    Mixer,
}

/// UI-only state (zoom, scroll, open dialogs...).
pub struct View {
    pub main: MainView,
    /// Timeline zoom.
    pub px_per_sec: f32,
    /// Seconds of timeline hidden off the left edge.
    pub scroll_sec: f64,
    pub selected_clip: Option<(u64, usize)>,
    pub show_settings: bool,
    pub show_open: bool,
    pub show_new: bool,
    pub new_name: String,
    pub new_rate: u32,
    /// Meter levels for display.
    pub input_levels: Vec<MeterState>,
    pub track_levels: HashMap<u64, MeterState>,
    pub bus_levels: HashMap<u64, [MeterState; 2]>,
    pub master_levels: [MeterState; 2],
    pub devices: Vec<DeviceInfo>,
    pub sessions: Vec<PathBuf>,
    /// Settings being edited in the Audio Settings window.
    pub draft: Settings,
}

pub struct QtDaw {
    pub shared: Arc<Shared>,
    pub engine: Option<Engine>,
    pub engine_error: Option<String>,
    pub settings: Settings,
    pub session: Session,
    /// `None` until the session is first saved (or recorded into).
    pub folder: Option<PathBuf>,
    pub dirty: bool,
    /// Engine-side mirror of each track, same order as `session.tracks`.
    pub rts: Vec<Arc<TrackRt>>,
    /// Engine-side mirror of each bus, same order as `session.buses`.
    pub bus_rts: Vec<Arc<BusRt>>,
    /// Loaded audio, keyed by the clip's file path relative to the session folder.
    pub audio: HashMap<String, Arc<ClipAudio>>,
    pub view: View,
    pub status: Option<(String, Instant)>,
}

impl QtDaw {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::ui::apply_theme(&cc.egui_ctx);
        let settings = Settings::load();
        let mut app = Self {
            shared: Arc::new(Shared::default()),
            engine: None,
            engine_error: None,
            session: Session::new("Untitled", 48_000),
            folder: None,
            dirty: false,
            rts: Vec::new(),
            bus_rts: Vec::new(),
            audio: HashMap::new(),
            view: View {
                main: MainView::Tracks,
                px_per_sec: 40.0,
                scroll_sec: 0.0,
                selected_clip: None,
                show_settings: false,
                show_open: false,
                show_new: false,
                new_name: String::new(),
                new_rate: 48_000,
                input_levels: Vec::new(),
                track_levels: HashMap::new(),
                bus_levels: HashMap::new(),
                master_levels: [MeterState::default(); 2],
                devices: Vec::new(),
                sessions: Vec::new(),
                draft: settings.clone(),
            },
            settings,
            status: None,
        };
        let reopened = app
            .settings
            .last_session
            .clone()
            .filter(|p| p.join(session::SESSION_FILE).exists())
            .map(|p| app.open_session(&p).is_ok())
            .unwrap_or(false);
        if !reopened {
            app.session.add_track(Some("Audio 1".into()), 0);
            app.rebuild_tracks();
        }
        app.restart_engine();
        app
    }

    // ---------------------------------------------------------------- messages

    pub fn say(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, t)| t.elapsed() < Duration::from_secs(8))
            .map(|(s, _)| s.as_str())
    }

    // ---------------------------------------------------------------- engine

    pub fn restart_engine(&mut self) {
        // Close the old device first: ASIO drivers only allow one user at a time.
        self.engine = None;
        let cfg = EngineConfig {
            host: self.settings.host.clone(),
            input: self.settings.input.clone(),
            output: self.settings.output.clone(),
            sample_rate: self.session.sample_rate,
            buffer_frames: self.settings.buffer,
        };
        match Engine::start(&cfg, self.shared.clone()) {
            Ok(engine) => {
                self.view.input_levels = vec![MeterState::default(); engine.in_channels];
                self.engine = Some(engine);
                self.engine_error = None;
            }
            Err(e) => self.engine_error = Some(format!("{e:#}")),
        }
    }

    pub fn input_count(&self) -> usize {
        self.engine.as_ref().map_or(2, |e| e.in_channels)
    }

    pub fn refresh_devices(&mut self) {
        match devices::list_devices() {
            Ok(list) => self.view.devices = list,
            Err(e) => self.say(format!("Couldn't list audio devices: {e}")),
        }
    }

    /// Copy session values (faders, arms, tempo...) into the engine. Cheap; runs every frame.
    pub fn sync_engine(&self) {
        let s = &self.shared;
        s.tempo.set(self.session.tempo);
        s.beats_per_bar.store(self.session.beats_per_bar, Ordering::Relaxed);
        s.master_gain.set(db_to_gain(self.session.master_db));
        s.click_gain.set(self.settings.click_volume);
        let adjust = (self.settings.record_adjust_ms / 1000.0 * self.session.sample_rate as f32) as i64;
        s.record_adjust.store(adjust, Ordering::Relaxed);
        for (t, rt) in self.session.tracks.iter().zip(&self.rts) {
            rt.input.store(t.input, Ordering::Relaxed);
            rt.armed.store(t.armed, Ordering::Relaxed);
            rt.gain.set(db_to_gain(t.volume_db));
            rt.pan.set(t.pan);
            rt.mute.store(t.mute, Ordering::Relaxed);
            rt.solo.store(t.solo, Ordering::Relaxed);
            rt.output.store(t.output, Ordering::Relaxed);

            // Sends: swap the list only when a bus or pre/post changed; levels are atomic.
            let current = rt.sends.load();
            let same_layout = current.len() == t.sends.len()
                && current.iter().zip(&t.sends).all(|(a, b)| a.bus == b.bus && a.pre_fader == b.pre_fader);
            if same_layout {
                for (a, b) in current.iter().zip(&t.sends) {
                    a.gain.set(db_to_gain(b.level_db));
                }
            } else {
                let sends = t
                    .sends
                    .iter()
                    .map(|s| SendRt { bus: s.bus, pre_fader: s.pre_fader, gain: AtomicF32::new(db_to_gain(s.level_db)) })
                    .collect();
                rt.sends.store(Arc::new(sends));
            }
        }
        for (b, rt) in self.session.buses.iter().zip(&self.bus_rts) {
            rt.gain.set(db_to_gain(b.volume_db));
            rt.pan.set(b.pan);
            rt.mute.store(b.mute, Ordering::Relaxed);
            rt.solo.store(b.solo, Ordering::Relaxed);
        }
    }

    /// Rebuild the engine's bus list after buses were added or removed.
    pub fn rebuild_buses(&mut self) {
        let mut old: HashMap<u64, Arc<BusRt>> = self.bus_rts.drain(..).map(|r| (r.id, r)).collect();
        self.bus_rts = self
            .session
            .buses
            .iter()
            .map(|b| old.remove(&b.id).unwrap_or_else(|| Arc::new(BusRt::new(b.id))))
            .collect();
        self.shared.buses.store(Arc::new(self.bus_rts.clone()));
        self.sync_engine();
    }

    /// Rebuild the engine's track list after tracks were added, removed or reordered.
    pub fn rebuild_tracks(&mut self) {
        let mut old: HashMap<u64, Arc<TrackRt>> = self.rts.drain(..).map(|r| (r.id, r)).collect();
        self.rts = self
            .session
            .tracks
            .iter()
            .map(|t| old.remove(&t.id).unwrap_or_else(|| Arc::new(TrackRt::new(t.id))))
            .collect();
        self.shared.tracks.store(Arc::new(self.rts.clone()));
        let ids: Vec<u64> = self.session.tracks.iter().map(|t| t.id).collect();
        for id in ids {
            self.refresh_clips(id);
        }
        self.sync_engine();
    }

    /// Hand a track's current clips to the engine.
    pub fn refresh_clips(&mut self, track_id: u64) {
        let Some(idx) = self.session.tracks.iter().position(|t| t.id == track_id) else { return };
        let clips: Vec<ClipRt> = self.session.tracks[idx]
            .clips
            .iter()
            .filter_map(|c| {
                let audio = self.audio.get(&c.file)?;
                Some(ClipRt { start: c.start, samples: audio.samples.clone() })
            })
            .collect();
        if let Some(rt) = self.rts.get(idx) {
            rt.clips.store(Arc::new(clips));
        }
    }

    // ---------------------------------------------------------------- transport

    pub fn busy(&self) -> bool {
        self.shared.playing.load(Ordering::Acquire) || self.shared.counting_in.load(Ordering::Acquire)
    }

    pub fn play_stop(&mut self) {
        if self.busy() {
            self.shared.request(Request::Stop);
        } else {
            self.shared.request(Request::Play);
        }
    }

    pub fn stop(&mut self) {
        self.shared.request(Request::Stop);
    }

    pub fn record(&mut self) {
        let Some(engine) = &self.engine else {
            self.say("No audio device is running. Open Audio Settings.");
            return;
        };
        if !engine.recorder.idle() || self.shared.counting_in.load(Ordering::Acquire) || self.shared.is_recording() {
            return;
        }
        let armed: Vec<(u64, usize)> =
            self.session.tracks.iter().filter(|t| t.armed).map(|t| (t.id, t.input)).collect();
        if armed.is_empty() {
            self.say("Arm a track first (the R button).");
            return;
        }
        let folder = match self.ensure_folder() {
            Ok(f) => f,
            Err(e) => {
                self.say(format!("Can't create the session folder: {e:#}"));
                return;
            }
        };
        let takes: Vec<TakeSpec> = armed
            .into_iter()
            .filter_map(|(id, input)| {
                let file = self.session.next_take_file(id)?;
                Some(TakeSpec { track_id: id, input, path: folder.join(&file), file })
            })
            .collect();
        let engine = self.engine.as_ref().expect("checked above");
        match engine.recorder.begin(takes) {
            Ok(()) => self.shared.request(Request::Record),
            Err(e) => self.say(format!("{e:#}")),
        }
    }

    /// Pick up finished recordings from the disk writer.
    pub fn poll_recorder(&mut self) {
        let Some(engine) = &self.engine else { return };
        let events: Vec<RecorderEvent> = engine.recorder.events.try_iter().collect();
        for event in events {
            match event {
                RecorderEvent::Finished(takes) => {
                    let mut added = 0;
                    for take in takes {
                        match ClipAudio::load_wav(&take.path) {
                            Ok(audio) => {
                                self.audio.insert(take.file.clone(), Arc::new(audio));
                                if let Some(t) = self.session.track_mut(take.track_id) {
                                    t.clips.push(ClipModel { file: take.file, start: take.start });
                                    added += 1;
                                }
                                self.refresh_clips(take.track_id);
                            }
                            Err(e) => self.say(format!("Recorded, but couldn't load the take: {e:#}")),
                        }
                    }
                    self.dirty = true;
                    // Save right away so a crash never loses a take.
                    self.save();
                    let lost = self.shared.rec_overflows.swap(0, Ordering::Relaxed);
                    if lost > 0 {
                        self.say(format!("Saved {added} take(s), but the disk fell behind {lost} time(s): there may be gaps."));
                    } else {
                        self.say(format!("Saved {added} take(s)."));
                    }
                }
                RecorderEvent::Cancelled => {}
                RecorderEvent::Failed(e) => self.say(format!("Recording failed: {e}")),
            }
        }
    }

    // ---------------------------------------------------------------- tracks & clips

    pub fn add_track(&mut self) {
        let n = self.session.tracks.len();
        let input = n % self.input_count().max(1);
        self.session.add_track(Some(format!("Audio {}", n + 1)), input);
        self.dirty = true;
        self.rebuild_tracks();
    }

    pub fn delete_track(&mut self, id: u64) {
        if self.shared.is_recording() {
            return;
        }
        self.session.tracks.retain(|t| t.id != id);
        if self.view.selected_clip.is_some_and(|(t, _)| t == id) {
            self.view.selected_clip = None;
        }
        self.dirty = true;
        self.rebuild_tracks();
    }

    /// Add a bus and return its id (`None` if the session is full).
    pub fn add_bus(&mut self) -> Option<u64> {
        let id = self.session.add_bus(None);
        match id {
            Some(_) => {
                self.dirty = true;
                self.rebuild_buses();
            }
            None => self.say(format!("A session can have up to {} buses.", session::MAX_BUSES)),
        }
        id
    }

    pub fn delete_bus(&mut self, id: u64) {
        self.session.remove_bus(id);
        self.view.bus_levels.remove(&id);
        self.dirty = true;
        self.rebuild_buses();
    }

    /// Remove the selected clip from its track. The WAV file stays on disk.
    pub fn delete_selected_clip(&mut self) {
        let Some((track_id, idx)) = self.view.selected_clip.take() else { return };
        if let Some(t) = self.session.track_mut(track_id) {
            if idx < t.clips.len() {
                t.clips.remove(idx);
                self.dirty = true;
            }
        }
        self.refresh_clips(track_id);
    }

    // ---------------------------------------------------------------- sessions

    fn ensure_folder(&mut self) -> anyhow::Result<PathBuf> {
        if let Some(f) = &self.folder {
            return Ok(f.clone());
        }
        let folder = unique_folder(&session::sessions_root(), &self.session.name);
        self.session.name = folder.file_name().map_or("Untitled".into(), |n| n.to_string_lossy().into_owned());
        self.session.save(&folder)?;
        self.folder = Some(folder.clone());
        self.settings.last_session = Some(folder.clone());
        self.settings.save();
        Ok(folder)
    }

    pub fn save(&mut self) {
        let result = self.ensure_folder().and_then(|f| self.session.save(&f).map(|_| f));
        match result {
            Ok(f) => {
                self.dirty = false;
                if self.status_text().is_none() {
                    self.say(format!("Saved to {}", f.display()));
                }
            }
            Err(e) => self.say(format!("Save failed: {e:#}")),
        }
    }

    pub fn new_session(&mut self, name: &str, sample_rate: u32) {
        if self.busy() {
            self.stop();
        }
        if self.dirty {
            self.save();
        }
        let name = session::safe_file_name(if name.trim().is_empty() { "Untitled" } else { name });
        let mut s = Session::new(&name, sample_rate);
        s.add_track(Some("Audio 1".into()), 0);
        self.session = s;
        self.folder = None;
        self.audio.clear();
        self.view.selected_clip = None;
        self.view.scroll_sec = 0.0;
        self.shared.locate(0);
        self.rebuild_tracks();
        self.rebuild_buses();
        if let Err(e) = self.ensure_folder() {
            self.say(format!("Couldn't create the session folder: {e:#}"));
        }
        self.restart_engine();
    }

    pub fn open_session(&mut self, folder: &Path) -> anyhow::Result<()> {
        if self.busy() {
            self.stop();
        }
        if self.dirty {
            self.save();
        }
        let session = Session::load(folder)?;
        let rate_changed = session.sample_rate != self.session.sample_rate;
        self.audio.clear();
        let mut missing = 0;
        for c in session.tracks.iter().flat_map(|t| &t.clips) {
            match ClipAudio::load_wav(&folder.join(&c.file)) {
                Ok(a) => {
                    self.audio.insert(c.file.clone(), Arc::new(a));
                }
                Err(_) => missing += 1,
            }
        }
        self.session = session;
        self.folder = Some(folder.to_path_buf());
        self.dirty = false;
        self.view.selected_clip = None;
        self.view.scroll_sec = 0.0;
        self.shared.locate(0);
        self.rebuild_tracks();
        self.rebuild_buses();
        self.settings.last_session = Some(folder.to_path_buf());
        self.settings.save();
        if rate_changed && self.engine.is_some() {
            self.restart_engine();
        }
        if missing > 0 {
            self.say(format!("Opened, but {missing} audio file(s) are missing."));
        } else {
            self.say(format!("Opened {}", self.session.name));
        }
        Ok(())
    }

    // ---------------------------------------------------------------- per-frame

    pub fn tick(&mut self, ctx: &egui::Context) {
        self.poll_recorder();
        self.sync_engine();

        let now = ctx.input(|i| i.time);
        if let Some(engine) = &self.engine {
            let peaks = engine.meters.take_peaks();
            self.view.input_levels.resize(peaks.len(), MeterState::default());
            for (m, p) in self.view.input_levels.iter_mut().zip(peaks) {
                m.update(p, now);
            }
        }
        for rt in &self.rts {
            self.view.track_levels.entry(rt.id).or_default().update(rt.take_peak(), now);
        }
        for rt in &self.bus_rts {
            let peaks = rt.take_peaks();
            let m = self.view.bus_levels.entry(rt.id).or_default();
            for i in 0..2 {
                m[i].update(peaks[i], now);
            }
        }
        let peaks = self.shared.take_master_peaks();
        for i in 0..2 {
            self.view.master_levels[i].update(peaks[i], now);
        }

        // Keep ~30 fps for meters and the playhead.
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    pub fn sample_rate(&self) -> f64 {
        self.session.sample_rate as f64
    }
}

fn unique_folder(root: &Path, base: &str) -> PathBuf {
    let base = session::safe_file_name(base);
    let first = root.join(&base);
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|i| root.join(format!("{base} {i}")))
        .find(|p| !p.exists())
        .expect("ran out of folder names")
}
