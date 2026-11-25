//! Audio clips loaded into memory, plus a small waveform overview for drawing.
//!
//! Phase 1 keeps whole clips in RAM (a 10-minute mono take at 48 kHz is ~115 MB as f32).
//! Streaming from disk comes later, when sessions get long enough to need it.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

/// Frames summarized by one overview point.
pub const OVERVIEW_FRAMES: usize = 256;

pub struct ClipAudio {
    /// Mono samples.
    pub samples: Arc<Vec<f32>>,
    /// (min, max) of each `OVERVIEW_FRAMES` block, for waveform drawing.
    pub overview: Vec<(f32, f32)>,
}

impl ClipAudio {
    pub fn new(samples: Vec<f32>) -> Self {
        let overview = samples
            .chunks(OVERVIEW_FRAMES)
            .map(|c| {
                c.iter().fold((0.0f32, 0.0f32), |(lo, hi), &s| (lo.min(s), hi.max(s)))
            })
            .collect();
        Self { samples: Arc::new(samples), overview }
    }

    pub fn frames(&self) -> usize {
        self.samples.len()
    }

    /// Load a WAV file as mono (multi-channel files are averaged).
    pub fn load_wav(path: &Path) -> Result<Self> {
        let mut reader =
            hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
        let spec = reader.spec();
        let channels = spec.channels.max(1) as usize;
        let interleaved: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
            hound::SampleFormat::Int => {
                let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|v| v as f32 * scale))
                    .collect::<Result<_, _>>()?
            }
        };
        let mono = if channels == 1 {
            interleaved
        } else {
            interleaved
                .chunks_exact(channels)
                .map(|f| f.iter().sum::<f32>() / channels as f32)
                .collect()
        };
        Ok(Self::new(mono))
    }
}
