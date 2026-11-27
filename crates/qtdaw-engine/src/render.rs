//! Offline rendering: play a session without an audio device, as fast as the CPU allows.
//! Used for analysis (the Claude mixing tools); bouncing to a file will reuse it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::clip::ClipAudio;
use crate::engine::{self, AtomicF32, BusRt, ClipRt, SendRt, Shared, TrackRt, db_to_gain};
use crate::fx::{self, InsertRt};
use crate::session::{InsertModel, Session, TrackModel};

/// Everything needed to play a session: its settings and its loaded audio. Cheap to clone
/// (the audio is shared, not copied).
#[derive(Clone)]
pub struct Snapshot {
    pub session: Session,
    /// Loaded audio by clip file (relative to the session folder).
    pub audio: HashMap<String, Arc<ClipAudio>>,
}

impl Snapshot {
    pub fn clips(&self, t: &TrackModel) -> Vec<ClipRt> {
        t.clips
            .iter()
            .filter_map(|c| Some(ClipRt { start: c.start, samples: self.audio.get(&c.file)?.samples.clone() }))
            .collect()
    }

    /// Timeline frame where the last clip ends (0 when nothing is recorded).
    pub fn end_frame(&self) -> i64 {
        self.session
            .tracks
            .iter()
            .flat_map(|t| self.clips(t))
            .map(|c| c.start + c.samples.len() as i64)
            .max()
            .unwrap_or(0)
            .max(0)
    }

    fn ctx(&self) -> fx::Ctx {
        fx::Ctx { sr: self.session.sample_rate as f32, tempo: self.session.tempo }
    }
}

/// Fresh engine effects for a chain (so rendering never disturbs the live ones).
fn chain(models: &[InsertModel]) -> Vec<Arc<InsertRt>> {
    models
        .iter()
        .map(|m| {
            let rt = InsertRt::new(m.id, m.kind);
            rt.set_params(&m.values());
            rt.bypass.store(m.bypass, Ordering::Relaxed);
            Arc::new(rt)
        })
        .collect()
}

/// A private engine state that plays like the session. Solo and record-arm are ignored, so
/// a render always contains the whole mix (mutes are kept).
pub fn shared_from_session(snap: &Snapshot) -> Arc<Shared> {
    let s = &snap.session;
    let shared = Shared::default();
    shared.tempo.set(s.tempo);
    shared.beats_per_bar.store(s.beats_per_bar, Ordering::Relaxed);
    shared.master_gain.set(db_to_gain(s.master_db));
    shared.master_inserts.store(Arc::new(chain(&s.master_inserts)));
    let tracks = s
        .tracks
        .iter()
        .map(|t| {
            let rt = TrackRt::new(t.id);
            rt.gain.set(db_to_gain(t.volume_db));
            rt.pan.set(t.pan);
            rt.mute.store(t.mute, Ordering::Relaxed);
            rt.output.store(t.output, Ordering::Relaxed);
            let sends = t
                .sends
                .iter()
                .map(|x| SendRt { bus: x.bus, pre_fader: x.pre_fader, gain: AtomicF32::new(db_to_gain(x.level_db)) })
                .collect();
            rt.sends.store(Arc::new(sends));
            rt.inserts.store(Arc::new(chain(&t.inserts)));
            rt.clips.store(Arc::new(snap.clips(t)));
            Arc::new(rt)
        })
        .collect();
    shared.tracks.store(Arc::new(tracks));
    let buses = s
        .buses
        .iter()
        .map(|b| {
            let rt = BusRt::new(b.id);
            rt.gain.set(db_to_gain(b.volume_db));
            rt.pan.set(b.pan);
            rt.mute.store(b.mute, Ordering::Relaxed);
            rt.inserts.store(Arc::new(chain(&b.inserts)));
            Arc::new(rt)
        })
        .collect();
    shared.buses.store(Arc::new(buses));
    Arc::new(shared)
}

/// Render the master output: `frames` frames from timeline position `start`. `sink` gets
/// (left, right, timeline position) for each block.
pub fn render_mix(snap: &Snapshot, start: i64, frames: usize, sink: impl FnMut(&[f32], &[f32], i64)) {
    engine::render_offline(shared_from_session(snap), snap.session.sample_rate, start, frames, sink);
}

/// Render one track by itself: its clips through its effects, fader and pan, scaled by the
/// fader of the bus it plays into. That's what the track adds to the mix, apart from bus
/// and master effects. Rendered even when muted.
pub fn render_track(snap: &Snapshot, track: &TrackModel, start: i64, frames: usize, mut sink: impl FnMut(&[f32], &[f32], i64)) {
    const BLOCK: usize = 1024;
    let clips = snap.clips(track);
    let inserts = chain(&track.inserts);
    let ctx = snap.ctx();
    let bus_gain = snap.session.buses.iter().find(|b| b.id == track.output).map_or(1.0, |b| db_to_gain(b.volume_db));
    let gain = db_to_gain(track.volume_db) * bus_gain;
    let (pl, pr) = engine::pan_gains(track.pan);
    let mut buf = vec![0.0f32; BLOCK];
    let (mut l, mut r) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut done = 0;
    while done < frames {
        let n = BLOCK.min(frames - done);
        let pos = start + done as i64;
        let b = &mut buf[..n];
        b.fill(0.0);
        engine::render_clips(&clips, pos, b);
        fx::run_chain(&inserts, &ctx, &mut [&mut *b]);
        for i in 0..n {
            l[i] = b[i] * gain * pl;
            r[i] = b[i] * gain * pr;
        }
        sink(&l[..n], &r[..n], pos);
        done += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ClipModel, MASTER};

    /// A session with one track playing a constant `level` for `frames` frames.
    pub(crate) fn dc_session(level: f32, frames: usize) -> Snapshot {
        let mut s = Session::new("Test", 48_000);
        let t = s.add_track(Some("A".into()), 0);
        s.track_mut(t).unwrap().clips.push(ClipModel { file: "a.wav".into(), start: 0 });
        let mut audio = HashMap::new();
        audio.insert("a.wav".to_string(), Arc::new(ClipAudio::new(vec![level; frames])));
        Snapshot { session: s, audio }
    }

    #[test]
    fn mix_and_track_renders_agree() {
        let mut snap = dc_session(0.5, 4000);
        snap.session.tracks[0].volume_db = -6.0;
        snap.session.tracks[0].solo = true; // ignored by rendering
        assert_eq!(snap.end_frame(), 4000);
        let g = db_to_gain(-6.0) * 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        let mut mix = Vec::new();
        render_mix(&snap, 0, 3000, |l, _, _| mix.extend_from_slice(l));
        let mut solo = Vec::new();
        let track = snap.session.tracks[0].clone();
        render_track(&snap, &track, 0, 3000, |l, _, _| solo.extend_from_slice(l));
        assert_eq!(mix.len(), 3000);
        assert!(mix.iter().zip(&solo).all(|(a, b)| (a - g).abs() < 1e-5 && (b - g).abs() < 1e-5));

        // Routed into a bus at -6 dB: the track render follows the bus fader too.
        let bus = snap.session.add_bus(None).unwrap();
        snap.session.buses[0].volume_db = -6.0;
        snap.session.tracks[0].output = bus;
        let track = snap.session.tracks[0].clone();
        let mut solo = Vec::new();
        render_track(&snap, &track, 0, 100, |l, _, _| solo.extend_from_slice(l));
        assert!((solo[0] - g * db_to_gain(-6.0)).abs() < 1e-5);
        snap.session.tracks[0].output = MASTER;
    }
}
