//! `qtdaw-probe`: check that QT DAW can see and use your audio hardware.
//!
//!   qtdaw-probe devices
//!   qtdaw-probe live --host asio --input "AudioBox" --monitor 1
//!   qtdaw-probe live --host asio --input "X-USB" --buffer 128 --monitor 1,2

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use qtdaw_engine::devices;
use qtdaw_engine::engine::{Engine, EngineConfig, Request, Shared, TrackRt};
use qtdaw_engine::recorder::{RecorderEvent, TakeSpec};
use qtdaw_engine::passthrough::{Passthrough, PassthroughOptions};
use qtdaw_engine::to_dbfs;

#[derive(Parser)]
#[command(version, about = "QT DAW hardware probe")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List every audio host and device, with channel counts and sample rates.
    Devices,
    /// Open an interface, show live input meters, and optionally hear chosen inputs.
    Live {
        /// Audio host: wasapi or asio (asio needs a build with `--features asio`).
        #[arg(long)]
        host: Option<String>,
        /// Input device (part of its name, or its id). Default: the system default.
        #[arg(long)]
        input: Option<String>,
        /// Output device. Default: same as input for ASIO, system default otherwise.
        #[arg(long)]
        output: Option<String>,
        #[arg(long, default_value_t = 48_000)]
        rate: u32,
        /// Driver buffer size in frames (e.g. 64, 128, 256). Smaller = less delay.
        #[arg(long)]
        buffer: Option<u32>,
        /// Inputs to hear, 1-based: "1" (mono) or "1,2" (stereo). Omit to only meter.
        #[arg(long, value_delimiter = ',')]
        monitor: Vec<usize>,
        /// Stop after this many seconds (0 = run until Ctrl+C).
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    /// Record inputs to WAV files with the real engine (count-in, click, disk writer).
    Record {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        input: Option<String>,
        #[arg(long)]
        output: Option<String>,
        #[arg(long, default_value_t = 48_000)]
        rate: u32,
        #[arg(long)]
        buffer: Option<u32>,
        /// Inputs to record, 1-based, e.g. "1,2".
        #[arg(long, value_delimiter = ',', default_value = "1")]
        inputs: Vec<usize>,
        #[arg(long, default_value_t = 3)]
        seconds: u64,
        /// Folder to write the WAV files into.
        #[arg(long, default_value = "probe-takes")]
        out: std::path::PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Devices => list(),
        Cmd::Live { host, input, output, rate, buffer, monitor, seconds } => live(
            PassthroughOptions {
                host,
                input,
                output,
                sample_rate: rate,
                buffer_frames: buffer,
                monitor: monitor.into_iter().map(|c| c.saturating_sub(1)).collect(),
            },
            seconds,
        ),
        Cmd::Record { host, input, output, rate, buffer, inputs, seconds, out } => record(
            EngineConfig { host, input, output, sample_rate: rate, buffer_frames: buffer },
            inputs,
            seconds,
            out,
        ),
    }
}

fn record(cfg: EngineConfig, inputs: Vec<usize>, seconds: u64, out: std::path::PathBuf) -> Result<()> {
    let shared = Arc::new(Shared::default());
    let tracks: Vec<Arc<TrackRt>> = inputs
        .iter()
        .enumerate()
        .map(|(i, &input)| {
            let t = TrackRt::new(i as u64 + 1);
            t.input.store(input.saturating_sub(1), Ordering::Relaxed);
            t.armed.store(true, Ordering::Relaxed);
            t.gain.set(0.0); // don't monitor through speakers
            Arc::new(t)
        })
        .collect();
    shared.tracks.store(Arc::new(tracks.clone()));
    shared.click_gain.set(0.0);

    let engine = Engine::start(&cfg, shared.clone())?;
    println!("Recording inputs {inputs:?} on {} at {} Hz", engine.input_name, engine.sample_rate);
    let takes = tracks
        .iter()
        .zip(&inputs)
        .map(|(t, &input)| {
            let file = format!("input {input}.wav");
            TakeSpec { track_id: t.id, input: input.saturating_sub(1), path: out.join(&file), file }
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    engine.recorder.begin(takes)?;
    shared.request(Request::Record);

    let started = Instant::now();
    while !shared.is_recording() {
        if started.elapsed() > Duration::from_secs(10) {
            anyhow::bail!("recording never started");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    println!("count-in took {:.2} s; recording for {seconds} s...", started.elapsed().as_secs_f32());
    std::thread::sleep(Duration::from_secs(seconds));
    shared.request(Request::Stop);

    match engine.recorder.events.recv_timeout(Duration::from_secs(10))? {
        RecorderEvent::Finished(done) => {
            for t in done {
                println!(
                    "wrote {} ({} frames = {:.2} s), starts at frame {} (latency offset {} frames = {:.1} ms)",
                    t.path.display(),
                    t.frames,
                    t.frames as f64 / engine.sample_rate as f64,
                    t.start,
                    shared.rec_offset.load(Ordering::Relaxed),
                    shared.rec_offset.load(Ordering::Relaxed) as f64 * 1000.0 / engine.sample_rate as f64,
                );
            }
        }
        other => println!("recorder: {other:?}"),
    }
    println!(
        "playhead after stop: {} (should be 0), dropouts {}, disk overflows {}",
        shared.playhead.load(Ordering::Relaxed),
        engine.meters.xruns.load(Ordering::Relaxed),
        shared.rec_overflows.load(Ordering::Relaxed)
    );
    Ok(())
}

fn list() -> Result<()> {
    let devices = devices::list_devices()?;
    let mut host = String::new();
    for d in devices {
        if d.host != host {
            println!("\n== {} ==", d.host);
            host = d.host.clone();
        }
        let rates: Vec<String> = d.sample_rates.iter().map(|r| format!("{}k", *r as f32 / 1000.0)).collect();
        println!(
            "  {:<45} in {:>2}  out {:>2}  rates [{}]",
            d.name,
            d.max_inputs,
            d.max_outputs,
            rates.join(", ")
        );
    }
    Ok(())
}

fn live(opts: PassthroughOptions, seconds: u64) -> Result<()> {
    if !opts.monitor.is_empty() {
        println!("Monitoring is on: use headphones, or speakers + mics will feed back.");
    }
    let pass = Passthrough::start(&opts)?;
    println!(
        "Input:  {} ({} channels)\nOutput: {} ({} channels)\nRate:   {} Hz\nCtrl+C to stop.\n",
        pass.input_name, pass.input_channels, pass.output_name, pass.output_channels, pass.sample_rate
    );

    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let peaks = pass.meters.take_peaks();
        let mut line = String::new();
        for (i, p) in peaks.iter().enumerate() {
            let db = to_dbfs(*p);
            let shown = if db < -60.0 { "  --".to_string() } else { format!("{db:>4.0}") };
            line.push_str(&format!(" {:>2}:{shown}", i + 1));
        }
        print!(
            "\r{line} | xruns {} errors {}   ",
            pass.meters.xruns.load(Ordering::Relaxed),
            pass.meters.stream_errors.load(Ordering::Relaxed)
        );
        std::io::stdout().flush()?;
        if seconds > 0 && start.elapsed() >= Duration::from_secs(seconds) {
            println!();
            return Ok(());
        }
    }
}
