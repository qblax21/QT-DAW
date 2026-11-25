//! Opening streams in whatever sample format the driver wants, while the rest of the
//! engine only ever sees interleaved `f32` (-1.0..=1.0).
//!
//! "Interleaved" means samples are stored frame by frame: for 3 channels the buffer is
//! `[ch1, ch2, ch3, ch1, ch2, ch3, ...]`.

use anyhow::{Result, bail};
use cpal::traits::DeviceTrait;
use cpal::{
    BufferSize, Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    SupportedStreamConfigRange,
};

/// Largest block we expect from a driver, in samples, so the audio thread never allocates.
const SCRATCH_SAMPLES: usize = 16_384 * 64;

/// Choose a stream config: the requested rate, the most channels the device offers at
/// that rate, and an optional fixed buffer size in frames.
pub fn pick_config(
    device: &Device,
    input: bool,
    sample_rate: u32,
    buffer_frames: Option<u32>,
) -> Result<(StreamConfig, SampleFormat)> {
    let ranges: Vec<SupportedStreamConfigRange> = if input {
        device.supported_input_configs()?.collect()
    } else {
        device.supported_output_configs()?.collect()
    };
    // Prefer more channels, then f32 (no conversion), then 32-bit, then anything.
    let best = ranges
        .into_iter()
        .filter(|r| r.contains_rate(sample_rate))
        .max_by_key(|r| (r.channels(), format_rank(r.sample_format())));
    let Some(range) = best else {
        bail!(
            "device does not support {sample_rate} Hz for {}",
            if input { "input" } else { "output" }
        );
    };
    let format = range.sample_format();
    let mut config: StreamConfig = range.with_sample_rate(sample_rate).config();
    if let Some(frames) = buffer_frames {
        config.buffer_size = BufferSize::Fixed(frames);
    }
    Ok((config, format))
}

fn format_rank(f: SampleFormat) -> u8 {
    match f {
        SampleFormat::F32 => 4,
        SampleFormat::I32 | SampleFormat::I24 => 3,
        SampleFormat::I16 => 2,
        _ => 1,
    }
}

/// Run `f::<T>(args...)` for the concrete sample type that matches a [`SampleFormat`].
macro_rules! with_sample_type {
    ($format:expr, $f:ident ( $($arg:expr),* )) => {
        match $format {
            SampleFormat::F32 => $f::<f32>($($arg),*),
            SampleFormat::F64 => $f::<f64>($($arg),*),
            SampleFormat::I16 => $f::<i16>($($arg),*),
            SampleFormat::I24 => $f::<cpal::I24>($($arg),*),
            SampleFormat::I32 => $f::<i32>($($arg),*),
            SampleFormat::U16 => $f::<u16>($($arg),*),
            SampleFormat::U8 => $f::<u8>($($arg),*),
            SampleFormat::I8 => $f::<i8>($($arg),*),
            other => bail!("unsupported sample format {other}"),
        }
    };
}

/// Open an input stream. `on_block` gets interleaved `f32` audio on the audio thread.
pub fn build_input<F>(
    device: &Device,
    config: StreamConfig,
    format: SampleFormat,
    on_block: F,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream>
where
    F: FnMut(&[f32]) + Send + 'static,
{
    with_sample_type!(format, input_typed(device, config, on_block, on_error))
}

/// Open an output stream. `render` fills an interleaved `f32` block on the audio thread.
pub fn build_output<F>(
    device: &Device,
    config: StreamConfig,
    format: SampleFormat,
    render: F,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream>
where
    F: FnMut(&mut [f32]) + Send + 'static,
{
    with_sample_type!(format, output_typed(device, config, render, on_error))
}

fn input_typed<T>(
    device: &Device,
    config: StreamConfig,
    mut on_block: impl FnMut(&[f32]) + Send + 'static,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let mut scratch: Vec<f32> = Vec::with_capacity(SCRATCH_SAMPLES);
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _| {
            scratch.clear();
            scratch.extend(data.iter().map(|s| f32::from_sample(*s)));
            on_block(&scratch);
        },
        on_error,
        None,
    )?;
    Ok(stream)
}

fn output_typed<T>(
    device: &Device,
    config: StreamConfig,
    mut render: impl FnMut(&mut [f32]) + Send + 'static,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let mut scratch: Vec<f32> = Vec::with_capacity(SCRATCH_SAMPLES);
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            scratch.clear();
            scratch.resize(data.len(), 0.0);
            render(&mut scratch);
            for (out, s) in data.iter_mut().zip(&scratch) {
                *out = s.clamp(-1.0, 1.0).to_sample::<T>();
            }
        },
        on_error,
        None,
    )?;
    Ok(stream)
}
