//! Peak meters shared between the audio thread and the UI, without locks.
//!
//! Trick: for non-negative floats, the raw bit patterns sort in the same order as the
//! numbers, so `AtomicU32::fetch_max` on the bits is an atomic "keep the loudest peak".

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub struct Meters {
    peaks: Vec<AtomicU32>,
    /// Number of times audio had to be filled with silence because data arrived late.
    pub xruns: AtomicU64,
    /// Errors reported by the audio driver.
    pub stream_errors: AtomicU64,
}

impl Meters {
    pub fn new(channels: usize) -> Self {
        Self {
            peaks: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            xruns: AtomicU64::new(0),
            stream_errors: AtomicU64::new(0),
        }
    }

    pub fn channels(&self) -> usize {
        self.peaks.len()
    }

    /// Audio thread: record the peaks of an interleaved block.
    pub fn record_interleaved(&self, block: &[f32], channels: usize) {
        let n = channels.min(self.peaks.len());
        for frame in block.chunks_exact(channels) {
            for (slot, s) in self.peaks[..n].iter().zip(frame) {
                slot.fetch_max(s.abs().to_bits(), Ordering::Relaxed);
            }
        }
    }

    /// UI thread: read every channel's peak since the last call, and reset it.
    pub fn take_peaks(&self) -> Vec<f32> {
        self.peaks
            .iter()
            .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
            .collect()
    }
}
