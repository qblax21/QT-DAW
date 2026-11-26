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
use crate::fx::{self, InsertRt};
use crate::io;
use crate::meter::Meters;
use crate::recorder::Recorder;
use crate::session::{MASTER, MAX_BUSES};

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

/// A send as the audio thread sees it. The bus and pre/post choice only change by swapping
/// the whole list; the level is atomic so turning a send knob never reallocates.
#[derive(Debug)]
pub struct SendRt {
    pub bus: u64,
    pub pre_fader: bool,
    pub gain: AtomicF32,
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
    /// [`MASTER`] or the id of the bus this track plays into.
    pub output: AtomicU64,
    pub sends: ArcSwap<Vec<SendRt>>,
    /// Effects, run on the track's signal before the fader.
    pub inserts: ArcSwap<Vec<Arc<InsertRt>>>,
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
            output: AtomicU64::new(MASTER),
            sends: ArcSwap::from_pointee(Vec::new()),
            inserts: ArcSwap::from_pointee(Vec::new()),
            clips: ArcSwap::from_pointee(Vec::new()),
            peak: AtomicU32::new(0),
        }
    }

    /// Loudest post-fader sample since the last call.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }
}

/// A stereo aux bus as the audio thread sees it.
pub struct BusRt {
    pub id: u64,
    pub gain: AtomicF32,
    /// Balance, -1..1.
    pub pan: AtomicF32,
    pub mute: AtomicBool,
    pub solo: AtomicBool,
    /// Effects, run before the bus fader.
    pub inserts: ArcSwap<Vec<Arc<InsertRt>>>,
    peak: [AtomicU32; 2],
}

impl BusRt {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            gain: AtomicF32::new(1.0),
            pan: AtomicF32::new(0.0),
            mute: AtomicBool::new(false),
            solo: AtomicBool::new(false),
            inserts: ArcSwap::from_pointee(Vec::new()),
            peak: [AtomicU32::new(0), AtomicU32::new(0)],
        }
    }

    /// Loudest (left, right) output samples since the last call.
    pub fn take_peaks(&self) -> [f32; 2] {
        [0, 1].map(|i| f32::from_bits(self.peak[i].swap(0, Ordering::Relaxed)))
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
    /// Only the first [`MAX_BUSES`] are mixed.
    pub buses: ArcSwap<Vec<Arc<BusRt>>>,
    pub tempo: AtomicF32,
    pub beats_per_bar: AtomicU32,
    pub click_on: AtomicBool,
    pub click_gain: AtomicF32,
    pub count_in_bars: AtomicU32,
    pub master_gain: AtomicF32,
    /// Effects after the master fader.
    pub master_inserts: ArcSwap<Vec<Arc<InsertRt>>>,
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
            buses: ArcSwap::from_pointee(Vec::new()),
            tempo: AtomicF32::new(120.0),
            beats_per_bar: AtomicU32::new(4),
            click_on: AtomicBool::new(true),
            click_gain: AtomicF32::new(0.5),
            count_in_bars: AtomicU32::new(1),
            master_gain: AtomicF32::new(1.0),
            master_inserts: ArcSwap::from_pointee(Vec::new()),
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

        let mut rt = Rt::new(
            shared.clone(),
            meters.clone(),
            (in_ch, out_ch),
            cfg.sample_rate as f64,
            in_cons,
            rec_prod,
            input_started,
            input_latency,
        );
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
    /// One stereo mixing buffer per bus, allocated up front so adding a bus never
    /// allocates on the audio thread.
    bus_l: Vec<Vec<f32>>,
    bus_r: Vec<Vec<f32>>,
    mode: Mode,
    playhead: i64,
    /// Where playback started, so Stop can return there.
    play_start: i64,
    count_pos: i64,
    count_len: i64,
}

const START_BLOCK: usize = 8192;

impl Rt {
    #[allow(clippy::too_many_arguments)]
    fn new(
        shared: Arc<Shared>,
        meters: Arc<Meters>,
        (in_ch, out_ch): (usize, usize),
        sr: f64,
        in_cons: HeapCons<f32>,
        rec_prod: HeapProd<f32>,
        input_started: Arc<AtomicBool>,
        input_latency: Arc<AtomicU64>,
    ) -> Self {
        let playhead = shared.playhead.load(Ordering::Acquire);
        Self {
            shared,
            meters,
            in_ch,
            out_ch,
            sr,
            in_cons,
            rec_prod,
            input_started,
            input_latency,
            out_latency: 0,
            ring_latency: 0,
            in_buf: vec![0.0; START_BLOCK * in_ch],
            track_buf: vec![0.0; START_BLOCK],
            mix_l: vec![0.0; START_BLOCK],
            mix_r: vec![0.0; START_BLOCK],
            bus_l: vec![vec![0.0; START_BLOCK]; MAX_BUSES],
            bus_r: vec![vec![0.0; START_BLOCK]; MAX_BUSES],
            mode: Mode::Stopped,
            playhead,
            play_start: 0,
            count_pos: 0,
            count_len: 0,
        }
    }

    fn process(&mut self, out: &mut [f32], out_latency: Duration) {
        flush_denormals();
        let n = out.len() / self.out_ch;
        self.out_latency = duration_frames(out_latency, self.sr as usize);
        if self.mix_l.len() < n {
            // Driver gave a bigger block than expected; grow once (rare).
            self.in_buf.resize(n * self.in_ch, 0.0);
            self.track_buf.resize(n, 0.0);
            self.mix_l.resize(n, 0.0);
            self.mix_r.resize(n, 0.0);
            for b in self.bus_l.iter_mut().chain(self.bus_r.iter_mut()) {
                b.resize(n, 0.0);
            }
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

        // 4. Master fader, master effects, meters, and out to the first two outputs.
        let master = self.shared.master_gain.get();
        for (l, r) in self.mix_l[..n].iter_mut().zip(&mut self.mix_r[..n]) {
            *l *= master;
            *r *= master;
        }
        let ctx = self.fx_ctx();
        fx::run_chain(&self.shared.master_inserts.load(), &ctx, &mut [&mut self.mix_l[..n], &mut self.mix_r[..n]]);
        let (mut pk_l, mut pk_r) = (0.0f32, 0.0f32);
        for (i, frame) in out.chunks_exact_mut(self.out_ch).enumerate() {
            let l = self.mix_l[i];
            let r = self.mix_r[i];
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

    fn fx_ctx(&self) -> fx::Ctx {
        fx::Ctx { sr: self.sr as f32, tempo: self.shared.tempo.get() }
    }

    fn beat_len(&self) -> f64 {
        self.sr * 60.0 / self.shared.tempo.get().clamp(20.0, 400.0) as f64
    }

    /// Render `len` frames starting at frame `off` of the current block.
    fn render(&mut self, off: usize, len: usize) {
        let advancing = matches!(self.mode, Mode::Playing | Mode::Recording);
        let seg = off..off + len;
        self.mix_l[seg.clone()].fill(0.0);
        self.mix_r[seg.clone()].fill(0.0);

        let tracks = self.shared.tracks.load();
        let buses_guard = self.shared.buses.load();
        let buses = &buses_guard[..buses_guard.len().min(MAX_BUSES)];
        for i in 0..buses.len() {
            self.bus_l[i][seg.clone()].fill(0.0);
            self.bus_r[i][seg.clone()].fill(0.0);
        }
        let bus_index = |id: u64| buses.iter().position(|b| b.id == id);
        let bus_soloed = |i: Option<usize>| i.is_some_and(|i| buses[i].solo.load(Ordering::Relaxed));
        let ctx = self.fx_ctx();

        // Solo: with anything soloed, you hear soloed tracks, soloed buses (and the tracks
        // feeding them), and the buses a soloed track feeds (so its reverb stays on).
        let mut fed_by_solo = [false; MAX_BUSES];
        let mut any_solo = buses.iter().any(|b| b.solo.load(Ordering::Relaxed));
        for t in tracks.iter().filter(|t| t.solo.load(Ordering::Relaxed)) {
            any_solo = true;
            if let Some(i) = bus_index(t.output.load(Ordering::Relaxed)) {
                fed_by_solo[i] = true;
            }
            for s in t.sends.load().iter() {
                if let Some(i) = bus_index(s.bus) {
                    fed_by_solo[i] = true;
                }
            }
        }

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
            fx::run_chain(&t.inserts.load(), &ctx, &mut [&mut *tb]);

            let muted = t.mute.load(Ordering::Relaxed);
            let soloed = t.solo.load(Ordering::Relaxed);
            let fader = t.gain.get();
            let pan = pan_gains(t.pan.get());

            // Main output.
            let out_idx = bus_index(t.output.load(Ordering::Relaxed));
            let audible = !muted && (!any_solo || soloed || bus_soloed(out_idx));
            let gain = if audible { fader } else { 0.0 };
            let (dl, dr) = match out_idx {
                Some(i) => (&mut self.bus_l[i][seg.clone()], &mut self.bus_r[i][seg.clone()]),
                None => (&mut self.mix_l[seg.clone()], &mut self.mix_r[seg.clone()]),
            };
            let peak = mix_into(tb, gain, pan, dl, dr);
            t.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);

            // Sends.
            for s in t.sends.load().iter() {
                let Some(i) = bus_index(s.bus) else { continue };
                if muted || (any_solo && !soloed && !bus_soloed(Some(i))) {
                    continue;
                }
                let gain = s.gain.get() * if s.pre_fader { 1.0 } else { fader };
                mix_into(tb, gain, pan, &mut self.bus_l[i][seg.clone()], &mut self.bus_r[i][seg.clone()]);
            }
        }

        // Buses (through their effects) into the master.
        for (i, b) in buses.iter().enumerate() {
            fx::run_chain(&b.inserts.load(), &ctx, &mut [&mut self.bus_l[i][seg.clone()], &mut self.bus_r[i][seg.clone()]]);
            let audible = !b.mute.load(Ordering::Relaxed)
                && (!any_solo || b.solo.load(Ordering::Relaxed) || fed_by_solo[i]);
            let gain = if audible { b.gain.get() } else { 0.0 };
            let bal = b.pan.get().clamp(-1.0, 1.0);
            let (gl, gr) = (gain * (1.0 - bal).min(1.0), gain * (1.0 + bal).min(1.0));
            let (mut pl, mut pr) = (0.0f32, 0.0f32);
            for j in seg.clone() {
                let l = self.bus_l[i][j] * gl;
                let r = self.bus_r[i][j] * gr;
                pl = pl.max(l.abs());
                pr = pr.max(r.abs());
                self.mix_l[j] += l;
                self.mix_r[j] += r;
            }
            b.peak[0].fetch_max(pl.to_bits(), Ordering::Relaxed);
            b.peak[1].fetch_max(pr.to_bits(), Ordering::Relaxed);
        }
        drop(buses_guard);
        drop(tracks);

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

/// Treat tiny "denormal" numbers as zero. Reverb and filter tails decay into them, and on
/// x86 CPUs they are up to 100x slower to compute, which can cause dropouts.
#[inline]
fn flush_denormals() {
    #[cfg(target_arch = "x86_64")]
    #[allow(deprecated)]
    // SAFETY: only sets the flush-to-zero / denormals-are-zero bits of this thread's
    // floating-point control register.
    unsafe {
        use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
        _mm_setcsr(_mm_getcsr() | 0x8040);
    }
}

/// Constant-power pan: a centered track is -3 dB in each speaker, so it sounds equally
/// loud wherever you pan it.
fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
}

/// Add mono `src` x `gain` into a stereo pair, panned. Returns the loudest sample added
/// (before panning), for the meter.
fn mix_into(src: &[f32], gain: f32, (pl, pr): (f32, f32), dl: &mut [f32], dr: &mut [f32]) -> f32 {
    let (gl, gr) = (gain * pl, gain * pr);
    let mut peak = 0.0f32;
    for ((s, l), r) in src.iter().zip(dl.iter_mut()).zip(dr.iter_mut()) {
        peak = peak.max((s * gain).abs());
        *l += s * gl;
        *r += s * gr;
    }
    peak
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

    /// An engine core with no device behind it, already playing from frame 0.
    fn test_rt(shared: &Arc<Shared>) -> Rt {
        let (_in_prod, in_cons) = HeapRb::<f32>::new(64).split();
        let (rec_prod, _rec_cons) = HeapRb::<f32>::new(64).split();
        shared.click_on.store(false, Ordering::Relaxed);
        let mut rt = Rt::new(
            shared.clone(),
            Arc::new(Meters::new(1)),
            (1, 2),
            48_000.0,
            in_cons,
            rec_prod,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU64::new(0)),
        );
        rt.set_mode(Mode::Playing);
        rt
    }

    /// A track playing a constant 1.0, routed to `output`.
    fn dc_track(id: u64, output: u64) -> Arc<TrackRt> {
        let t = TrackRt::new(id);
        t.output.store(output, Ordering::Relaxed);
        t.clips.store(Arc::new(vec![ClipRt { start: 0, samples: Arc::new(vec![1.0; 1000]) }]));
        Arc::new(t)
    }

    /// Render one block and return the first (left, right) output frame.
    fn first_frame(rt: &mut Rt) -> (f32, f32) {
        let mut out = vec![0.0; 2 * 16];
        rt.process(&mut out, Duration::ZERO);
        (out[0], out[1])
    }

    const CENTER: f32 = std::f32::consts::FRAC_1_SQRT_2;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn track_into_bus_into_master() {
        let shared = Arc::new(Shared::default());
        let bus = Arc::new(BusRt::new(10));
        bus.gain.set(0.5);
        shared.buses.store(Arc::new(vec![bus.clone()]));
        shared.tracks.store(Arc::new(vec![dc_track(1, 10)]));
        let mut rt = test_rt(&shared);
        let (l, r) = first_frame(&mut rt);
        assert!(close(l, 0.5 * CENTER) && close(r, 0.5 * CENTER), "{l} {r}");
        assert!(close(bus.take_peaks()[0], 0.5 * CENTER));
    }

    #[test]
    fn post_and_pre_fader_sends() {
        let shared = Arc::new(Shared::default());
        shared.buses.store(Arc::new(vec![Arc::new(BusRt::new(10))]));
        let t = dc_track(1, MASTER);
        t.gain.set(0.5);
        t.sends.store(Arc::new(vec![SendRt { bus: 10, pre_fader: false, gain: AtomicF32::new(0.5) }]));
        shared.tracks.store(Arc::new(vec![t.clone()]));
        let mut rt = test_rt(&shared);
        // Dry 0.5 + post-fader send 0.5 x 0.5.
        let (l, _) = first_frame(&mut rt);
        assert!(close(l, (0.5 + 0.25) * CENTER), "{l}");

        t.sends.store(Arc::new(vec![SendRt { bus: 10, pre_fader: true, gain: AtomicF32::new(0.5) }]));
        let (l, _) = first_frame(&mut rt);
        assert!(close(l, (0.5 + 0.5) * CENTER), "{l}");
    }

    #[test]
    fn solo_keeps_the_soloed_tracks_bus() {
        let shared = Arc::new(Shared::default());
        shared.buses.store(Arc::new(vec![Arc::new(BusRt::new(10))]));
        let a = dc_track(1, 10);
        let b = dc_track(2, MASTER);
        a.solo.store(true, Ordering::Relaxed);
        shared.tracks.store(Arc::new(vec![a.clone(), b.clone()]));
        let mut rt = test_rt(&shared);
        // Only A, through its bus.
        let (l, _) = first_frame(&mut rt);
        assert!(close(l, CENTER), "{l}");

        // Soloing the bus instead: A plays (it feeds the bus), B on the master is silent.
        a.solo.store(false, Ordering::Relaxed);
        shared.buses.load()[0].solo.store(true, Ordering::Relaxed);
        let (l, _) = first_frame(&mut rt);
        assert!(close(l, CENTER), "{l}");

        // Muting the bus silences everything.
        shared.buses.load()[0].mute.store(true, Ordering::Relaxed);
        let (l, _) = first_frame(&mut rt);
        assert!(close(l, 0.0), "{l}");
    }

    #[test]
    fn effects_run_on_tracks_buses_and_master() {
        use crate::fx::EffectKind;
        let shared = Arc::new(Shared::default());
        // A limiter with a -6 dB ceiling on the master caps everything.
        let lim = Arc::new(InsertRt::new(100, EffectKind::Limiter));
        let mut p = EffectKind::Limiter.defaults();
        p[1] = -6.0;
        lim.set_params(&p);
        shared.master_inserts.store(Arc::new(vec![lim.clone()]));
        // A compressor on the track.
        let comp = Arc::new(InsertRt::new(101, EffectKind::Compressor));
        let t = dc_track(1, MASTER);
        t.inserts.store(Arc::new(vec![comp.clone()]));
        shared.tracks.store(Arc::new(vec![t]));
        let mut rt = test_rt(&shared);
        let mut out = vec![0.0; 2 * 800];
        rt.process(&mut out, Duration::ZERO);
        let ceiling = 10f32.powf(-6.0 / 20.0);
        assert!(out.iter().all(|s| s.abs() <= ceiling + 1e-6));
        assert!(comp.take_reduction() > 1.0);

        // Bypassed effects do nothing.
        lim.bypass.store(true, Ordering::Relaxed);
        comp.bypass.store(true, Ordering::Relaxed);
        let (l, _) = first_frame(&mut rt);
        assert!(close(l, CENTER), "{l}");
    }

    #[test]
    fn missing_bus_falls_back_to_master() {
        let shared = Arc::new(Shared::default());
        shared.tracks.store(Arc::new(vec![dc_track(1, 99)]));
        let mut rt = test_rt(&shared);
        let (l, _) = first_frame(&mut rt);
        assert!(close(l, CENTER), "{l}");
    }

    #[test]
    fn clip_before_zero_is_trimmed() {
        let a = ClipRt { start: -3, samples: Arc::new((0..6).map(|i| i as f32).collect()) };
        let mut out = vec![0.0; 4];
        render_clips(&[a], 0, &mut out);
        assert_eq!(out, vec![3.0, 4.0, 5.0, 0.0]);
    }
}
