//! Live input monitoring: every input channel is metered, and the chosen ones are sent
//! to the first two output channels (one channel = centered mono, two = left/right).
//!
//! The input and output callbacks run on separate driver threads, so audio crosses
//! between them through a lock-free ring buffer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use cpal::Stream;
use cpal::traits::StreamTrait;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Producer, Split};

use crate::devices::{self, device_name};
use crate::io;
use crate::meter::Meters;

#[derive(Debug, Clone)]
pub struct PassthroughOptions {
    /// Audio host name ("wasapi", "asio"); `None` = platform default.
    pub host: Option<String>,
    /// Input device name fragment or id; `None` = default.
    pub input: Option<String>,
    /// Output device name fragment or id; `None` = default (for ASIO, use the same device).
    pub output: Option<String>,
    pub sample_rate: u32,
    /// Driver buffer size in frames; `None` = driver default.
    pub buffer_frames: Option<u32>,
    /// 0-based input channels to hear. Empty = meter only, play nothing.
    pub monitor: Vec<usize>,
}

pub struct Passthrough {
    _input: Stream,
    _output: Stream,
    pub meters: Arc<Meters>,
    pub input_name: String,
    pub output_name: String,
    pub input_channels: usize,
    pub output_channels: usize,
    pub sample_rate: u32,
}

impl Passthrough {
    pub fn start(opts: &PassthroughOptions) -> Result<Self> {
        let host = devices::host(opts.host.as_deref())?;
        let in_dev = devices::find_device(&host, opts.input.as_deref(), true)?;
        let out_dev = match (&opts.output, &opts.input, opts.host.as_deref()) {
            // ASIO drivers are one device for both directions; default to the same one.
            (None, Some(_), Some(h)) if h.eq_ignore_ascii_case("asio") => {
                devices::find_device(&host, opts.input.as_deref(), false)?
            }
            _ => devices::find_device(&host, opts.output.as_deref(), false)?,
        };

        let (in_cfg, in_fmt) = io::pick_config(&in_dev, true, opts.sample_rate, opts.buffer_frames)?;
        let (out_cfg, out_fmt) =
            io::pick_config(&out_dev, false, opts.sample_rate, opts.buffer_frames)?;
        let in_ch = in_cfg.channels as usize;
        let out_ch = out_cfg.channels as usize;
        if let Some(&bad) = opts.monitor.iter().find(|&&c| c >= in_ch) {
            bail!("input {} does not exist; this device has {in_ch} inputs", bad + 1);
        }

        let meters = Arc::new(Meters::new(in_ch));

        // Half a second of headroom between the two callbacks, pre-filled with ~10 ms of
        // silence so the output never starts by reading an empty buffer.
        let rate = opts.sample_rate as usize;
        let ring = HeapRb::<f32>::new(rate / 2 * in_ch);
        let (mut producer, mut consumer) = ring.split();
        let prefill = (rate / 100).max(opts.buffer_frames.unwrap_or(0) as usize * 2) * in_ch;
        for _ in 0..prefill {
            let _ = producer.try_push(0.0);
        }

        // The output can start a moment before the first input block arrives; underruns
        // before then are startup, not dropouts, so they are not counted.
        let input_started = Arc::new(AtomicBool::new(false));

        let in_meters = meters.clone();
        let in_started = input_started.clone();
        let input = io::build_input(
            &in_dev,
            in_cfg,
            in_fmt,
            move |block, _latency| {
                in_started.store(true, Ordering::Release);
                in_meters.record_interleaved(block, in_ch);
                if producer.push_slice(block) < block.len() {
                    in_meters.xruns.fetch_add(1, Ordering::Relaxed);
                }
            },
            error_counter(meters.clone()),
        )?;

        let out_meters = meters.clone();
        let monitor = opts.monitor.clone();
        let mut frame = vec![0.0f32; in_ch];
        let output = io::build_output(
            &out_dev,
            out_cfg,
            out_fmt,
            move |block, _latency| {
                let mut starved = false;
                for out_frame in block.chunks_exact_mut(out_ch) {
                    if consumer.pop_slice(&mut frame) < in_ch {
                        frame.fill(0.0);
                        starved = true;
                    }
                    out_frame.fill(0.0);
                    match monitor.as_slice() {
                        [] => {}
                        [mono] => {
                            // -3 dB on each side keeps a centered mono source at equal loudness.
                            let s = frame[*mono] * std::f32::consts::FRAC_1_SQRT_2;
                            out_frame[0] = s;
                            if out_ch > 1 {
                                out_frame[1] = s;
                            }
                        }
                        [left, right, ..] => {
                            out_frame[0] = frame[*left];
                            if out_ch > 1 {
                                out_frame[1] = frame[*right];
                            }
                        }
                    }
                }
                // One dropout per late block, not per missing sample.
                if starved && input_started.load(Ordering::Acquire) {
                    out_meters.xruns.fetch_add(1, Ordering::Relaxed);
                }
            },
            error_counter(meters.clone()),
        )?;

        input.play()?;
        output.play()?;

        Ok(Self {
            _input: input,
            _output: output,
            meters,
            input_name: device_name(&in_dev),
            output_name: device_name(&out_dev),
            input_channels: in_ch,
            output_channels: out_ch,
            sample_rate: opts.sample_rate,
        })
    }
}

fn error_counter(meters: Arc<Meters>) -> impl FnMut(cpal::Error) + Send + 'static {
    move |_err| {
        meters.stream_errors.fetch_add(1, Ordering::Relaxed);
    }
}
