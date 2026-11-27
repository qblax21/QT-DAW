//! The mix report for Claude: loudness, peaks and tone of the master and of every track,
//! plus which tracks fight over the same frequencies at the same time ("masking").
//!
//! Each track and the master is rendered offline on its own CPU core, measured block by
//! block (nothing is kept in memory but the measurements), then compared.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::analysis::{self, BANDS, Loudness, LoudnessResult, NB, REGIONS, SILENT, Spectrum, band_label, power_db};
use crate::render::{self, Snapshot};

/// What to analyze.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub start_sec: Option<f64>,
    pub end_sec: Option<f64>,
    /// Add the full 1/3-octave spectrum of everything.
    pub detail: bool,
}

#[derive(Debug, Serialize)]
pub struct Report {
    /// The part of the song measured, in seconds.
    pub range_sec: [f64; 2],
    /// Names of the `tone_db` entries, low to high.
    pub tone_regions: Vec<&'static str>,
    /// Centre frequencies of the `third_octave_db` entries (only with `detail`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub third_octave_hz: Option<Vec<f32>>,
    pub master: Stats,
    pub tracks: Vec<TrackReport>,
    /// Track pairs that cover each other up, worst first.
    pub masking: Vec<Conflict>,
    pub notes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrated_lufs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_term_max_lufs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loudness_range_lu: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub true_peak_dbtp: Option<f64>,
    pub sample_peak_dbfs: f64,
    pub rms_dbfs: f64,
    /// Share of the range where it is playing.
    pub active_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stereo_correlation: Option<f64>,
    #[serde(skip_serializing_if = "is_zero")]
    pub clipped_samples: u64,
    /// Average level (dB) in each of `tone_regions`, while playing.
    pub tone_db: Vec<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub third_octave_db: Option<Vec<f64>>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Serialize)]
pub struct TrackReport {
    pub id: u64,
    pub name: String,
    pub output: String,
    pub volume_db: f64,
    pub pan: f64,
    pub muted: bool,
    /// `None` when the track is silent in the range.
    #[serde(flatten)]
    pub stats: Option<Stats>,
}

#[derive(Debug, Serialize)]
pub struct Conflict {
    pub tracks: [String; 2],
    pub pans: [f64; 2],
    pub ranges: Vec<Overlap>,
}

#[derive(Debug, Serialize)]
pub struct Overlap {
    /// Frequency range, e.g. "200 Hz-315 Hz".
    pub freq: String,
    /// Of the time both tracks play, how much they are both strong and close in level here.
    pub overlap_pct: f64,
    /// Their average levels (dB) in this range while overlapping.
    pub levels_db: [f64; 2],
}

/// Round to 0.1 (as f64, so the JSON shows -14.2 rather than -14.199999809).
fn r1(x: f32) -> f64 {
    (x as f64 * 10.0).round() / 10.0
}

struct Measured {
    loudness: LoudnessResult,
    frames: Vec<[f32; NB]>,
}

fn stats(m: &Measured, detail: bool, stereo: bool) -> Option<Stats> {
    let l = &m.loudness;
    let avg = analysis::average(&m.frames)?;
    l.integrated_lufs?;
    let tone = REGIONS.iter().map(|(_, r)| r1(power_db(avg[r.clone()].iter().map(|&p| p as f64).sum()))).collect();
    Some(Stats {
        integrated_lufs: l.integrated_lufs.map(r1),
        short_term_max_lufs: l.short_term_max_lufs.map(r1),
        loudness_range_lu: l.range_lu.map(r1),
        true_peak_dbtp: l.true_peak_dbtp.map(r1),
        sample_peak_dbfs: r1(l.sample_peak_dbfs),
        rms_dbfs: r1(l.rms_dbfs),
        active_pct: l.active_pct.round() as f64,
        stereo_correlation: l.correlation.filter(|_| stereo).map(|c| (c as f64 * 100.0).round() / 100.0),
        clipped_samples: l.clipped_samples,
        tone_db: tone,
        third_octave_db: detail.then(|| avg.iter().map(|&p| r1(power_db(p as f64))).collect()),
    })
}

/// Measure the session. Takes a few seconds for a full song.
pub fn analyze(snap: &Snapshot, opts: Options) -> Result<Report> {
    let sr = snap.session.sample_rate as f64;
    let end_total = snap.end_frame();
    if end_total <= 0 {
        bail!("nothing is recorded yet, so there's nothing to analyze");
    }
    let to_frame = |s: f64| (s.max(0.0) * sr).round() as i64;
    let start = opts.start_sec.map_or(0, to_frame).min(end_total);
    let end = opts.end_sec.map_or(end_total, to_frame).min(end_total);
    if end - start < (sr / 2.0) as i64 {
        bail!("the range is shorter than half a second (the song is {:.1} s long)", end_total as f64 / sr);
    }
    // Start a few seconds early so reverbs, delays and compressors are "warmed up".
    let pre = start.min((3.0 * sr) as i64);
    let frames = (end - start + pre) as usize;
    let from = start - pre;

    // Job 0 = master, job i = track i - 1.
    let jobs = snap.session.tracks.len() + 1;
    let results: Vec<Mutex<Option<Measured>>> = (0..jobs).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(jobs);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let job = next.fetch_add(1, Ordering::Relaxed);
                    if job >= jobs {
                        break;
                    }
                    let mut loud = Loudness::new(sr as f32, job == 0);
                    let mut spec = Spectrum::new(sr as f32);
                    let sink = |l: &[f32], r: &[f32], pos: i64| {
                        let skip = (start - pos).clamp(0, l.len() as i64) as usize;
                        let (l, r) = (&l[skip..], &r[skip..]);
                        loud.push(l, r);
                        spec.push(l.iter().zip(r).map(|(a, b)| 0.5 * (a + b)));
                    };
                    if job == 0 {
                        render::render_mix(snap, from, frames, sink);
                    } else {
                        render::render_track(snap, &snap.session.tracks[job - 1], from, frames, sink);
                    }
                    *results[job].lock().unwrap() = Some(Measured { loudness: loud.finish(), frames: spec.frames });
                }
            });
        }
    });
    let measured: Vec<Measured> = results.into_iter().map(|m| m.into_inner().unwrap().expect("every job ran")).collect();

    let master = stats(&measured[0], opts.detail, true).unwrap_or_else(|| Stats {
        integrated_lufs: None,
        short_term_max_lufs: None,
        loudness_range_lu: None,
        true_peak_dbtp: None,
        sample_peak_dbfs: r1(measured[0].loudness.sample_peak_dbfs),
        rms_dbfs: r1(measured[0].loudness.rms_dbfs),
        active_pct: 0.0,
        stereo_correlation: None,
        clipped_samples: 0,
        tone_db: Vec::new(),
        third_octave_db: None,
    });

    let s = &snap.session;
    let bus_muted = |out: u64| s.buses.iter().any(|b| b.id == out && b.mute);
    let mut notes = Vec::new();
    let mut tracks = Vec::new();
    for (t, m) in s.tracks.iter().zip(&measured[1..]) {
        let st = stats(m, opts.detail, false);
        if st.is_none() && !t.mute {
            notes.push(format!("\"{}\" is silent in this range.", t.name));
        }
        tracks.push(TrackReport {
            id: t.id,
            name: t.name.clone(),
            output: s.output_name(t.output).to_string(),
            volume_db: r1(t.volume_db),
            pan: (t.pan as f64 * 100.0).round() / 100.0,
            muted: t.mute || bus_muted(t.output),
            stats: st,
        });
    }

    // Masking, among the tracks you can hear.
    let audible: Vec<usize> = (0..s.tracks.len()).filter(|&i| !tracks[i].muted && tracks[i].stats.is_some()).collect();
    let rows = measured[0].frames.len();
    let mut mix = vec![[0.0f32; NB]; rows];
    for &i in &audible {
        for (m, row) in mix.iter_mut().zip(&measured[i + 1].frames) {
            for k in 0..NB {
                m[k] += row[k];
            }
        }
    }
    let mut masking = Vec::new();
    for (x, &a) in audible.iter().enumerate() {
        for &b in &audible[x + 1..] {
            if let Some(c) = conflict(&measured[a + 1].frames, &measured[b + 1].frames, &mix) {
                masking.push((c.0, Conflict {
                    tracks: [s.tracks[a].name.clone(), s.tracks[b].name.clone()],
                    pans: [tracks[a].pan, tracks[b].pan],
                    ranges: c.1,
                }));
            }
        }
    }
    masking.sort_by(|a, b| b.0.total_cmp(&a.0));
    let masking: Vec<Conflict> = masking.into_iter().take(12).map(|(_, c)| c).collect();

    if master.clipped_samples > 0 {
        notes.push(format!(
            "The master goes over 0 dBFS on {} samples: it will clip. Lower the master or add a limiter.",
            master.clipped_samples
        ));
    }
    if let Some(tp) = master.true_peak_dbtp.filter(|&tp| tp > -1.0) {
        notes.push(format!("Master true peak is {tp} dBTP; streaming services ask for -1 dBTP or lower."));
    }

    Ok(Report {
        range_sec: [start as f64 / sr, end as f64 / sr].map(|x| (x * 10.0).round() / 10.0),
        tone_regions: REGIONS.iter().map(|(n, _)| *n).collect(),
        third_octave_hz: opts.detail.then(|| BANDS.to_vec()),
        master,
        tracks,
        masking,
        notes,
    })
}

/// How much tracks `a` and `b` mask each other: per frequency band, the share of the time
/// (while both play) that both are a big part of the mix there (each at least a tenth of the
/// band's energy) and within 6 dB of each other. Returns the worst score and the ranges
/// that overlap at least 25% of the time.
fn conflict(a: &[[f32; NB]], b: &[[f32; NB]], mix: &[[f32; NB]]) -> Option<(f64, Vec<Overlap>)> {
    const FIRST: usize = 3; // 50 Hz; below that it's rumble, not something to fix by masking
    let mut both = 0u32;
    let mut hits = [0u32; NB];
    let mut sum = [[0.0f64; 2]; NB];
    for ((ra, rb), rm) in a.iter().zip(b).zip(mix) {
        if analysis::row_total(ra) < SILENT || analysis::row_total(rb) < SILENT {
            continue;
        }
        both += 1;
        // A band only matters if it's within 40 dB of the whole mix at that moment.
        let floor = 1e-4 * analysis::row_total(rm);
        for k in FIRST..NB {
            let (pa, pb, pm) = (ra[k], rb[k], rm[k]);
            if pm > floor && pa >= 0.1 * pm && pb >= 0.1 * pm && (0.25..=4.0).contains(&(pa / pb)) {
                hits[k] += 1;
                sum[k][0] += pa as f64;
                sum[k][1] += pb as f64;
            }
        }
    }
    if both < 12 {
        return None;
    }
    let pct = |k: usize| 100.0 * hits[k] as f32 / both as f32;
    // Merge neighbouring bands into ranges.
    let mut ranges = Vec::new();
    let mut k = FIRST;
    while k < NB {
        if pct(k) < 25.0 {
            k += 1;
            continue;
        }
        let lo = k;
        while k + 1 < NB && pct(k + 1) >= 25.0 {
            k += 1;
        }
        // The band that overlaps most often (the louder one on a tie).
        let score = |x: usize| (pct(x), sum[x][0] + sum[x][1]);
        let best = (lo..=k).max_by(|&x, &y| score(x).partial_cmp(&score(y)).unwrap_or(std::cmp::Ordering::Equal)).unwrap();
        let freq = if lo == k { band_label(lo) } else { format!("{}-{}", band_label(lo), band_label(k)) };
        let n = hits[best].max(1) as f64;
        ranges.push(Overlap {
            freq,
            overlap_pct: pct(best).round() as f64,
            levels_db: [r1(power_db(sum[best][0] / n)), r1(power_db(sum[best][1] / n))],
        });
        k += 1;
    }
    if ranges.is_empty() {
        return None;
    }
    ranges.sort_by(|x, y| y.overlap_pct.total_cmp(&x.overlap_pct));
    ranges.truncate(3);
    Some((ranges[0].overlap_pct, ranges))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::clip::ClipAudio;
    use crate::session::{ClipModel, Session};

    fn tone(freq: f32, amp: f32, secs: f32) -> Vec<f32> {
        (0..(48_000.0 * secs) as usize).map(|i| amp * (std::f32::consts::TAU * freq * i as f32 / 48_000.0).sin()).collect()
    }

    fn snap(tracks: &[(&str, Vec<f32>)]) -> Snapshot {
        let mut s = Session::new("Test", 48_000);
        let mut audio = HashMap::new();
        for (name, samples) in tracks {
            let id = s.add_track(Some(name.to_string()), 0);
            let file = format!("{name}.wav");
            s.track_mut(id).unwrap().clips.push(ClipModel { file: file.clone(), start: 0 });
            audio.insert(file, Arc::new(ClipAudio::new(samples.clone())));
        }
        Snapshot { session: s, audio }
    }

    #[test]
    fn report_measures_tracks_and_finds_masking() {
        // Bass and keys both sit on 250 Hz at the same level; the hi-hat is up at 8 kHz.
        let s = snap(&[
            ("Bass", tone(250.0, 0.3, 5.0)),
            ("Keys", tone(250.0, 0.3, 5.0)),
            ("Hat", tone(8000.0, 0.1, 5.0)),
        ]);
        let r = analyze(&s, Options::default()).unwrap();
        assert_eq!(r.range_sec, [0.0, 5.0]);
        assert_eq!(r.tracks.len(), 3);
        let bass = r.tracks[0].stats.as_ref().unwrap();
        // 0.3 amplitude, centred (-3 dB per side): about -13.5 LUFS.
        let i = bass.integrated_lufs.unwrap();
        assert!((i + 13.5).abs() < 1.0, "{i}");
        // The bass's energy is in the low mids region (250-500 Hz), the hat's in "air".
        let loudest = |s: &Stats| (0..s.tone_db.len()).max_by(|&a, &b| s.tone_db[a].total_cmp(&s.tone_db[b])).unwrap();
        assert_eq!(loudest(bass), 2);
        assert_eq!(loudest(r.tracks[2].stats.as_ref().unwrap()), 6);
        assert!(r.master.true_peak_dbtp.is_some());

        assert_eq!(r.masking.len(), 1, "{:?}", r.masking);
        let c = &r.masking[0];
        assert_eq!(c.tracks, ["Bass".to_string(), "Keys".to_string()]);
        assert!(c.ranges[0].freq.contains("250 Hz"), "{:?}", c.ranges);
        assert!(c.ranges[0].overlap_pct > 90.0);
    }

    #[test]
    fn a_range_and_muted_tracks() {
        let mut s = snap(&[("A", tone(250.0, 0.3, 6.0)), ("B", tone(250.0, 0.3, 6.0))]);
        s.session.tracks[1].mute = true;
        let r = analyze(&s, Options { start_sec: Some(2.0), end_sec: Some(4.0), detail: true }).unwrap();
        assert_eq!(r.range_sec, [2.0, 4.0]);
        assert!(r.tracks[1].muted);
        assert!(r.masking.is_empty());
        assert_eq!(r.master.third_octave_db.as_ref().unwrap().len(), NB);
        assert!(analyze(&s, Options { start_sec: Some(5.9), ..Default::default() }).is_err());
        let empty = Snapshot { session: Session::new("Empty", 48_000), audio: HashMap::new() };
        assert!(analyze(&empty, Options::default()).is_err());
    }
}
