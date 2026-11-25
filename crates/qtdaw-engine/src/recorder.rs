//! The disk writer: a normal (non-real-time) thread that takes recorded audio out of the
//! record ring buffer and writes one 24-bit WAV per armed track.
//!
//! Handshake (see [`rec_state`]):
//! 1. UI calls [`Recorder::begin`] (state -> PENDING) and then requests Record.
//! 2. Audio thread starts recording after the count-in (-> RECORDING) and pushes every
//!    input channel into the ring.
//! 3. On Stop the audio thread sets FINISHED (or CANCELLED during count-in).
//! 4. The writer drains what's left, closes the files, reports a [`RecorderEvent`], and
//!    sets the state back to IDLE.

use std::fs;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use anyhow::{Result, bail};
use ringbuf::HeapCons;
use ringbuf::traits::Consumer;

use crate::engine::{Shared, rec_state};

/// One track to record.
#[derive(Debug, Clone)]
pub struct TakeSpec {
    pub track_id: u64,
    /// 0-based interface input.
    pub input: usize,
    /// Absolute path of the WAV to write.
    pub path: PathBuf,
    /// Same path, relative to the session folder (what gets saved in the session).
    pub file: String,
}

#[derive(Debug, Clone)]
pub struct FinishedTake {
    pub track_id: u64,
    pub file: String,
    pub path: PathBuf,
    /// Timeline position of the first sample, latency-compensated.
    pub start: i64,
    pub frames: u64,
}

#[derive(Debug)]
pub enum RecorderEvent {
    Finished(Vec<FinishedTake>),
    Cancelled,
    Failed(String),
}

pub struct Recorder {
    jobs: Sender<Vec<TakeSpec>>,
    pub events: Receiver<RecorderEvent>,
    shared: Arc<Shared>,
    alive: Arc<AtomicBool>,
}

type Writer = hound::WavWriter<BufWriter<fs::File>>;

impl Recorder {
    pub(crate) fn spawn(mut ring: HeapCons<f32>, in_ch: usize, sample_rate: u32, shared: Arc<Shared>) -> Self {
        let (jobs, job_rx) = mpsc::channel::<Vec<TakeSpec>>();
        let (event_tx, events) = mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_shared = shared.clone();
        let thread_alive = alive.clone();
        std::thread::Builder::new()
            .name("qtdaw-disk-writer".into())
            .spawn(move || {
                for takes in job_rx {
                    let event = run_job(&takes, &mut ring, in_ch, sample_rate, &thread_shared, &thread_alive);
                    thread_shared.rec_state.store(rec_state::IDLE, Ordering::Release);
                    if event_tx.send(event).is_err() {
                        break;
                    }
                }
            })
            .expect("failed to start disk writer thread");
        Self { jobs, events, shared, alive }
    }

    /// True when a new recording can start.
    pub fn idle(&self) -> bool {
        self.shared.rec_state.load(Ordering::Acquire) == rec_state::IDLE
    }

    /// Prepare to record these takes. Call this, then `Shared::request(Request::Record)`.
    pub fn begin(&self, takes: Vec<TakeSpec>) -> Result<()> {
        if takes.is_empty() {
            bail!("no tracks are armed for recording");
        }
        if self.shared
            .rec_state
            .compare_exchange(rec_state::IDLE, rec_state::PENDING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!("still saving the previous recording");
        }
        self.jobs.send(takes)?;
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Device is closing: let the writer finish whatever it has.
        self.alive.store(false, Ordering::Release);
    }
}

fn run_job(
    takes: &[TakeSpec],
    ring: &mut HeapCons<f32>,
    in_ch: usize,
    sample_rate: u32,
    shared: &Shared,
    alive: &AtomicBool,
) -> RecorderEvent {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    // If a file can't be created we still drain the ring (so the next take starts clean)
    // and report the error at the end.
    let mut error: Option<String> = None;
    let mut writers: Vec<Option<Writer>> = takes
        .iter()
        .map(|t| {
            if let Some(dir) = t.path.parent() {
                let _ = fs::create_dir_all(dir);
            }
            hound::WavWriter::create(&t.path, spec)
                .map_err(|e| error = Some(format!("can't create {}: {e}", t.path.display())))
                .ok()
        })
        .collect();

    const MAX_24: f32 = 8_388_607.0;
    let mut buf = vec![0.0f32; 4096 * in_ch];
    let mut frames: u64 = 0;
    let finished = loop {
        // Read the state *before* draining: if it said FINISHED and the ring is then empty,
        // everything recorded has been written.
        let state = shared.rec_state.load(Ordering::Acquire);
        let got = ring.pop_slice(&mut buf);
        if got > 0 {
            for frame in buf[..got].chunks_exact(in_ch) {
                for (take, w) in takes.iter().zip(writers.iter_mut()) {
                    if let Some(writer) = w {
                        let s = frame[take.input.min(in_ch - 1)].clamp(-1.0, 1.0);
                        if let Err(e) = writer.write_sample((s * MAX_24) as i32) {
                            error = Some(format!("writing {}: {e}", take.path.display()));
                            *w = None;
                        }
                    }
                }
                frames += 1;
            }
            continue;
        }
        match state {
            rec_state::FINISHED => break true,
            rec_state::CANCELLED => break false,
            _ if !alive.load(Ordering::Acquire) => break state == rec_state::RECORDING,
            _ => std::thread::sleep(Duration::from_millis(5)),
        }
    };

    for w in writers.into_iter().flatten() {
        if let Err(e) = w.finalize() {
            error = Some(format!("closing recording: {e}"));
        }
    }
    if !finished || frames == 0 {
        for t in takes {
            let _ = fs::remove_file(&t.path);
        }
        return error.map_or(RecorderEvent::Cancelled, RecorderEvent::Failed);
    }
    if let Some(e) = error {
        return RecorderEvent::Failed(e);
    }
    let start = shared.rec_start.load(Ordering::Relaxed) - shared.rec_offset.load(Ordering::Relaxed);
    RecorderEvent::Finished(
        takes
            .iter()
            .map(|t| FinishedTake {
                track_id: t.track_id,
                file: t.file.clone(),
                path: t.path.clone(),
                start,
                frames,
            })
            .collect(),
    )
}
