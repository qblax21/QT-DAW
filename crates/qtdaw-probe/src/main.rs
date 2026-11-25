//! `qtdaw-probe`: check that QT DAW can see and use your audio hardware.
//!
//!   qtdaw-probe devices
//!   qtdaw-probe live --host asio --input "AudioBox" --monitor 1
//!   qtdaw-probe live --host asio --input "X-USB" --buffer 128 --monitor 1,2

use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use qtdaw_engine::devices;
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
    }
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
