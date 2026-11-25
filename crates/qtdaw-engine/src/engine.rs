//! The real-time engine: transport (play / stop / record), count-in and click, live input
//! monitoring, clip playback, track volume / pan / mute / solo, and handing recorded audio
//! to the disk writer.
//!
//! How the pieces talk:
//! - The UI owns the session and mirrors each track into a [`TrackRt`] made of atomics, so
//!   moving a fader never blocks the audio thread.
//! - The UI asks for transport changes with [`Shared::request`]; the audio thread acts on
//!   them at the start of its next block and publishes the result (playhead, playing...).
//! - Everything audible is produced in the *output* callback. The input callback only
//!   pushes into a ring buffer; the output callback pulls one block of input per block of
//!   output, so playback, monitoring and recording all run off one clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use arc_swap::ArcSwap;
use cpal::Stream;
use cpal::traits::StreamTrait;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::devices::{self, device_name};
use crate::io;
use crate::meter::Meters;
use crate::recorder::Recorder;

/// An `f32` that can be shared between threads (stored as its bit pattern).
#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// Convert decibels to a linear gain factor (0 dB = 1.0, -6 dB ~ 0.5).
pub fn db_to_gain(db: f32) -> f32 {
    if db <= -96.0 { 0.0 } else { 10f32.powf(db / 20.0) }
}

/// A clip as the audio thread sees it.
#[derive(Clone)]
pub struct ClipRt {
    pub start: i64,
    pub samples: Arc<Vec<f32>>,
}

/// A track as the audio thread sees it. The UI writes these fields; the engine reads them.
pub struct TrackRt {
    pub id: u64,
    pub input: AtomicUsize,
    pub armed: AtomicBool,
    pub gain: AtomicF32,
    pub pan: AtomicF32,
    pub mute: AtomicBool,
    pub solo: AtomicBool,
    pub clips: ArcSwap<Vec<ClipRt>>,
    peak: AtomicU32,
}

impl TrackRt {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            input: AtomicUsize::new(0),
            armed: AtomicBool::new(false),
            gain: AtomicF32::new(1.0),
            pan: AtomicF32::new(0.0),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            clips: ArcSwap::from_pointee(Vec::new()),
            peak: AtomicU32::new(0),
        }
    }

    /// Loudest post-fader sample since the last call.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }
}

/// Transport requests from the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Request {
    Play = 1,
    Stop = 2,
    Record = 3,
}

/// Recording handshake between UI, audio thread and disk writer.
pub mod rec_state {
    /// Nothing happening.
    pub const IDLE: u8 = 0;
    /// The UI handed the writer a job and asked to record; waiting for count-in.
    pub const PENDING: u8 = 1;
    /// The audio thread is pushing input into the record buffer.
    pub const RECORDING: u8 = 2;
    /// Recording stopped; the writer is finishing the files.
    pub const FINISHED: u8 = 3;
    /// Stopped before recording began (e.g. during count-in); files are discarded.
    pub const CANCELLED: u8 = 4;
}

/// State shared by the UI, the audio thread and the disk writer. Outlives any one
/// [`Engine`], so changing audio device keeps the tracks.
pub struct Shared {
    pub tracks: ArcSwap<Vec<Arc<TrackRt>>>,
    pub tempo: AtomicF32,
    pub beats_per_bar: AtomicU32,
    pub click_on: AtomicBool,
    pub click_gain: AtomicF32,
    pub count_in_bars: AtomicU32,
    pub master_gain: AtomicF32,
    /// Frames to shift recordings by on top of the measured latency (+ = earlier).
    pub record_adjust: AtomicI64,

    // Published by the audio thread.
    pub playhead: AtomicI64,
    pub playing: AtomicBool,
    pub counting_in: AtomicBool,
    pub rec_state: AtomicU8,
    pub rec_start: AtomicI64,
    pub rec_offset: AtomicI64,
    pub rec_overflows: AtomicU64,
    master_peak: [AtomicU32; 2],

    request: AtomicU8,
    locate: AtomicI64,
}

const NO_LOCATE: i64 = i64::MIN;

impl Default for Shared {
    fn default() -> Self {
        Self {
            tracks: ArcSwap::from_pointee(Vec::new()),
            tempo: AtomicF32::new(120.0),
            beats_per_bar: AtomicU32::new(4),
            click_on: AtomicBool::new(true),
            click_gain: AtomicF32::new(0.5),
            count_in_bars: AtomicU32::new(1),
            master_gain: AtomicF32::new(1.0),
            record_adjust: AtomicI64::new(0),
            playhead: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            counting_in: AtomicBool::new(false),
            rec_state: AtomicU8::new(rec_state::IDLE),
            rec_start: AtomicI64::new(0),
            rec_offset: AtomicI64::new(0),
            rec_overflows: AtomicU64::new(0),
            master_peak: [AtomicU32::new(0), AtomicU32::new(0)],
            request: AtomicU8::new(0),
            locate: AtomicI64::new(NO_LOCATE),
        }
    }
}

impl Shared {
    pub fn request(&self, r: Request) {
        self.request.store(r as u8, Ordering::Release);
    }

    /// Move the playhead (ignored while recording or counting in).
    pub fn locate(&self, frame: i64) {
        self.locate.store(frame.max(0), Ordering::Release);
    }

    pub fn is_recording(&self) -> bool {
        self.rec_state.load(Ordering::Acquire) == rec_state::RECORDING
    }

    /// Master (left, right) peaks since the last call.
    pub fn take_master_peaks(&self) -> [f32; 2] {
        [0, 1].map(|i| f32::from_bits(self.master_peak[i].swap(0, Ordering::Relaxed)))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineConfig {
    /// "asio" or "wasapi"; `None` = platform default.
    pub host: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub sample_rate: u32,
    pub buffer_frames: Option<u32>,
}

/// A running audio engine. Dropping it closes the device.
pub struct Engine {
    _input: Stream,
    _output: Stream,
    pub recorder: Recorder,
    /// Input-channel meters, plus dropout and driver-error counters.
    pub meters: Arc<Meters>,
    pub input_name: String,
    pub output_name: String,
    pub in_channels: usize,
    pub out_channels: usize,
    pub sample_rate: u32,
}

impl Engine {
    pub fn start(cfg: &EngineConfig, shared: Arc<Shared>) -> Result<Self> {
        let host = devices::host(cfg.host.as_deref())?;
        let is_asio = cfg.host.as_deref().is_some_and(|h| h.eq_ignore_ascii_case("asio"));
        let in_dev = devices::find_device(&host, cfg.input.as_deref(), true)?;
        let out_query = if is_asio && cfg.output.is_none() { cfg.input.as_deref() } else { cfg.output.as_deref() };
        let out_dev = devices::find_device(&host, out_query, false)?;

        let (in_cfg, in_fmt) = io::pick_config(&in_dev, true, cfg.sample_rate, cfg.buffer_frames)?;
        let (out_cfg, out_fmt) = io::pick_config(&out_dev, false, cfg.sample_rate, cfg.buffer_frames)?;
        let in_ch = in_cfg.channels as usize;
        let out_ch = out_cfg.channels as usize;
        if in_ch == 0 || out_ch == 0 {
            bail!("the device needs at least one input and one output channel");
        }
        let sr = cfg.sample_rate as usize;

        // A fresh device starts stopped.
        shared.playing.store(false, Ordering::Release);
        shared.counting_in.store(false, Ordering::Release);
        shared.rec_state.store(rec_state::IDLE, Ordering::Release);
        shared.request.store(0, Ordering::Release);

        let meters = Arc::new(Meters::new(in_ch));

        // Live input: half a second of room, plus a little silence up front so the output
        // never starts on an empty buffer. This cushion is part of what you hear as
        // monitoring delay, so keep it to one driver buffer when we know the size.
        let (mut in_prod, in_cons) = HeapRb::<f32>::new(sr / 2 * in_ch).split();
        let prefill = cfg.buffer_frames.map_or(sr / 100, |b| b as usize) * in_ch;
        for _ in 0..prefill {
            let _ = in_prod.try_push(0.0);
        }
        // Recording: 4 seconds of room for the disk writer to fall behind (it shouldn't).
        let (rec_prod, rec_cons) = HeapRb::<f32>::new(sr * 4 * in_ch).split();

        let input_started = Arc::new(AtomicBool::new(false));
        let input_latency = Arc::new(AtomicU64::new(0));

        let input = {
            let in_meters = meters.clone();
            let started = input_started.clone();
            let latency = input_latency.clone();
            io::build_input(
                &in_dev,
                in_cfg,
                in_fmt,
                move |block, lat| {
                    started.store(true, Ordering::Release);
                    latency.store(duration_frames(lat, sr), Ordering::Relaxed);
                    // Only push whole blocks so channels never get out of step.
                    if in_prod.vacant_len() >= block.len() {
                        in_prod.push_slice(block);
                    } else {
                        in_meters.xruns.fetch_add(1, Ordering::Relaxed);
                    }
                },
                error_counter(meters.clone()),
            )?
        };

        let mut rt = Rt {
            shared: shared.clone(),
            meters: meters.clone(),
            in_ch,
            out_ch,
            sr: cfg.sample_rate as f64,
            in_cons,
            rec_prod,
            input_started,
            input_latency,
            out_latency: 0,
            ring_latency: 0,
            in_buf: vec![0.0; 8192 * in_ch],
            track_buf: vec![0.0; 8192],
            mix_l: vec![0.0; 8192],
            mix_r: vec![0.0; 8192],
            mode: Mode::Stopped,
            playhead: shared.playhead.load(Ordering::Acquire),
            play_start: 0,
            count_pos: 0,
            count_len: 0,
        };
        let output = io::build_output(
            &out_dev,
            out_cfg,
            out_fmt,
            move |block, lat| rt.process(block, lat),
            error_counter(meters.clone()),
        )?;

        let recorder = Recorder::spawn(rec_cons, in_ch, cfg.sample_rate, shared.clone());

        input.play()?;
        output.play()?;

        Ok(Self {
            _input: input,
            _output: output,
            recorder,
            meters,
            input_name: device_name(&in_dev),
            output_name: device_name(&out_dev),
            in_channels: in_ch,
            out_channels: out_ch,
            sample_rate: cfg.sample_rate,
        })
    }
}

fn duration_frames(d: Duration, sr: usize) -> u64 {
    (d.as_secs_f64() * sr as f64).round() as u64
}

fn error_counter(meters: Arc<Meters>) -> impl FnMut(cpal::Error) + Send + 'static {
    move |_err| {
        meters.stream_errors.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Stopped,
    CountIn,
    Playing,
    Recording,
}

/// Everything the output callback owns. Lives on the audio thread.
struct Rt {
    shared: Arc<Shared>,
    meters: Arc<Meters>,
    in_ch: usize,
    out_ch: usize,
    sr: f64,
    in_cons: HeapCons<f32>,
    rec_prod: HeapProd<f32>,
    input_started: Arc<AtomicBool>,
    input_latency: Arc<AtomicU64>,
    out_latency: u64,
    ring_latency: u64,
    in_buf: Vec<f32>,
    track_buf: Vec<f32>,
    mix_l: Vec<f32>,
    mix_r: Vec<f32>,
    mode: Mode,
    playhead: i64,
    /// Where playback started, so Stop can return there.
    play_start: i64,
    count_pos: i64,
    count_len: i64,
}

impl Rt {
    fn process(&mut self, out: &mut [f32], out_latency: Duration) {
        let n = out.len() / self.out_ch;
        self.out_latency = duration_frames(out_latency, self.sr as usize);
        if self.mix_l.len() < n {
            // Driver gave a bigger block than expected; grow once (rare).
            self.in_buf.resize(n * self.in_ch, 0.0);
            self.track_buf.resize(n, 0.0);
            self.mix_l.resize(n, 0.0);
            self.mix_r.resize(n, 0.0);
        }

        // 1. Pull one block of live input.
        let need = n * self.in_ch;
        let got = self.in_cons.pop_slice(&mut self.in_buf[..need]);
        if got < need {
            self.in_buf[got..need].fill(0.0);
            if self.input_started.load(Ordering::Acquire) {
                self.meters.xruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.ring_latency = (self.in_cons.occupied_len() / self.in_ch) as u64;
        self.meters.record_interleaved(&self.in_buf[..need], self.in_ch);

        // 2. Transport requests from the UI.
        self.handle_requests();

        // 3. Render, splitting the block where a count-in ends so the first recorded
        //    sample lands exactly on the downbeat.
        let mut done = 0;
        while done < n {
            let seg = match self.mode {
                Mode::CountIn => ((self.count_len - self.count_pos) as usize).clamp(1, n - done),
                _ => n - done,
            };
            self.render(done, seg);
            done += seg;
            if self.mode == Mode::CountIn && self.count_pos >= self.count_len {
                self.start_recording();
            }
        }
        self.shared.playhead.store(self.playhead, Ordering::Release);

        // 4. Master fader, meters, and out to the first two outputs.
        let master = self.shared.master_gain.get();
        let (mut pk_l, mut pk_r) = (0.0f32, 0.0f32);
        for (i, frame) in out.chunks_exact_mut(self.out_ch).enumerate() {
            let l = self.mix_l[i] * master;
            let r = self.mix_r[i] * master;
            pk_l = pk_l.max(l.abs());
            pk_r = pk_r.max(r.abs());
            frame.fill(0.0);
            if self.out_ch == 1 {
                frame[0] = 0.5 * (l + r);
            } else {
                frame[0] = l;
                frame[1] = r;
            }
        }
        self.shared.master_peak[0].fetch_max(pk_l.to_bits(), Ordering::Relaxed);
        self.shared.master_peak[1].fetch_max(pk_r.to_bits(), Ordering::Relaxed);
    }

    fn handle_requests(&mut self) {
        let loc = self.shared.locate.swap(NO_LOCATE, Ordering::AcqRel);
        if loc != NO_LOCATE && matches!(self.mode, Mode::Stopped | Mode::Playing) {
            self.playhead = loc;
            if self.mode == Mode::Stopped {
                self.play_start = loc;
            }
        }

        let req = self.shared.request.swap(0, Ordering::AcqRel);
        if req == Request::Play as u8 && self.mode == Mode::Stopped {
            self.play_start = self.playhead;
            self.set_mode(Mode::Playing);
        } else if req == Request::Record as u8 {
            self.begin_record();
        } else if req == Request::Stop as u8 {
            self.stop();
        }
    }

    fn begin_record(&mut self) {
        let shared = &self.shared;
        if shared.rec_state.load(Ordering::Acquire) != rec_state::PENDING {
            return;
        }
        let any_armed = shared.tracks.load().iter().any(|t| t.armed.load(Ordering::Relaxed));
        if !any_armed {
            shared.rec_state.store(rec_state::CANCELLED, Ordering::Release);
            return;
        }
        match self.mode {
            Mode::Playing => self.start_recording(),
            Mode::Stopped => {
                self.play_start = self.playhead;
                let bars = shared.count_in_bars.load(Ordering::Relaxed) as i64;
                if bars > 0 {
                    let beats = bars * shared.beats_per_bar.load(Ordering::Relaxed).max(1) as i64;
                    self.count_len = (beats as f64 * self.beat_len()).round() as i64;
                    self.count_pos = 0;
                    self.set_mode(Mode::CountIn);
                } else {
                    self.start_recording();
                }
            }
            Mode::CountIn | Mode::Recording => {}
        }
    }

    fn start_recording(&mut self) {
        // Audio played at timeline position p is heard out_latency later; the performer
        // plays along, and that sound needs in_latency to be captured plus ring_latency to
        // reach this callback. So a recorded sample belongs this many frames earlier:
        let offset = self.out_latency as i64
            + self.input_latency.load(Ordering::Relaxed) as i64
            + self.ring_latency as i64
            + self.shared.record_adjust.load(Ordering::Relaxed);
        self.shared.rec_start.store(self.playhead, Ordering::Relaxed);
        self.shared.rec_offset.store(offset, Ordering::Relaxed);
        self.shared.rec_state.store(rec_state::RECORDING, Ordering::Release);
        self.set_mode(Mode::Recording);
    }

    fn stop(&mut self) {
        match self.mode {
            Mode::Recording => self.shared.rec_state.store(rec_state::FINISHED, Ordering::Release),
            Mode::CountIn => self.shared.rec_state.store(rec_state::CANCELLED, Ordering::Release),
            _ => {}
        }
        // Stop returns to where playback started; Stop again goes to the beginning.
        self.playhead = if self.mode == Mode::Stopped { 0 } else { self.play_start };
        self.play_start = self.playhead;
        self.set_mode(Mode::Stopped);
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.shared.playing.store(matches!(mode, Mode::Playing | Mode::Recording), Ordering::Release);
        self.shared.counting_in.store(mode == Mode::CountIn, Ordering::Release);
    }

    fn beat_len(&self) -> f64 {
        self.sr * 60.0 / self.shared.tempo.get().clamp(20.0, 400.0) as f64
    }

    /// Render `len` frames starting at frame `off` of the current block.
    fn render(&mut self, off: usize, len: usize) {
        let advancing = matches!(self.mode, Mode::Playing | Mode::Recording);
        let mix_l = &mut self.mix_l[off..off + len];
        let mix_r = &mut self.mix_r[off..off + len];
        mix_l.fill(0.0);
        mix_r.fill(0.0);

        let tracks = self.shared.tracks.load();
        let any_solo = tracks.iter().any(|t| t.solo.load(Ordering::Relaxed));
        for t in tracks.iter() {
            let tb = &mut self.track_buf[..len];
            tb.fill(0.0);
            if t.armed.load(Ordering::Relaxed) {
                // Armed tracks let you hear what you're playing (input monitoring).
                let ch = t.input.load(Ordering::Relaxed).min(self.in_ch - 1);
                for (i, s) in tb.iter_mut().enumerate() {
                    *s = self.in_buf[(off + i) * self.in_ch + ch];
                }
            } else if advancing {
                render_clips(&t.clips.load(), self.playhead, tb);
            }

            let audible = !t.mute.load(Ordering::Relaxed) && (!any_solo || t.solo.load(Ordering::Relaxed));
            let gain = if audible { t.gain.get() } else { 0.0 };
            // Constant-power pan: a centered track is -3 dB in each speaker.
            let angle = (t.pan.get().clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
            let (gl, gr) = (gain * angle.cos(), gain * angle.sin());
            let mut peak = 0.0f32;
            for i in 0..len {
                let s = tb[i];
                peak = peak.max((s * gain).abs());
                mix_l[i] += s * gl;
                mix_r[i] += s * gr;
            }
            t.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
        }

        if self.mode == Mode::Recording {
            let chunk = &self.in_buf[off * self.in_ch..(off + len) * self.in_ch];
            if self.rec_prod.vacant_len() >= chunk.len() {
                self.rec_prod.push_slice(chunk);
            } else {
                self.shared.rec_overflows.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Click: always during count-in, otherwise when enabled and rolling.
        if self.mode == Mode::CountIn {
            self.click(off, len, self.count_pos);
            self.count_pos += len as i64;
        } else if advancing {
            if self.shared.click_on.load(Ordering::Relaxed) {
                self.click(off, len, self.playhead);
            }
            self.playhead += len as i64;
        }
    }

    fn click(&mut self, off: usize, len: usize, pos: i64) {
        let beat_len = self.beat_len();
        let bpb = self.shared.beats_per_bar.load(Ordering::Relaxed).max(1) as i64;
        let gain = self.shared.click_gain.get();
        let click_len = 0.03 * self.sr;
        let decay = 0.006 * self.sr;
        for i in 0..len {
            let p = (pos + i as i64) as f64;
            let beat = (p / beat_len).floor();
            let phase = p - beat * beat_len;
            if phase < click_len {
                let accent = (beat as i64).rem_euclid(bpb) == 0;
                let (freq, amp) = if accent { (1760.0, 1.0) } else { (1320.0, 0.6) };
                let s = (std::f64::consts::TAU * freq * phase / self.sr).sin() * (-phase / decay).exp() * amp;
                let s = s as f32 * gain;
                self.mix_l[off + i] += s;
                self.mix_r[off + i] += s;
            }
        }
    }
}

/// Copy the clips overlapping `[pos, pos + out.len())` into `out`. Later clips overwrite
/// earlier ones, so the newest take is the one you hear.
fn render_clips(clips: &[ClipRt], pos: i64, out: &mut [f32]) {
    let end = pos + out.len() as i64;
    for clip in clips {
        let c_start = clip.start;
        let c_end = clip.start + clip.samples.len() as i64;
        let from = pos.max(c_start);
        let to = end.min(c_end);
        if from >= to {
            continue;
        }
        let src = &clip.samples[(from - c_start) as usize..(to - c_start) as usize];
        out[(from - pos) as usize..(to - pos) as usize].copy_from_slice(src);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_clip_wins_and_edges_are_exact() {
        let a = ClipRt { start: 0, samples: Arc::new(vec![1.0; 10]) };
        let b = ClipRt { start: 4, samples: Arc::new(vec![2.0; 3]) };
        let mut out = vec![0.0; 6];
        render_clips(&[a, b], 2, &mut out);
        assert_eq!(out, vec![1.0, 1.0, 2.0, 2.0, 2.0, 1.0]);
    }

    #[test]
    fn clip_before_zero_is_trimmed() {
        let a = ClipRt { start: -3, samples: Arc::new((0..6).map(|i| i as f32).collect()) };
        let mut out = vec![0.0; 4];
        render_clips(&[a], 0, &mut out);
        assert_eq!(out, vec![3.0, 4.0, 5.0, 0.0]);
    }
}
