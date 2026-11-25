//! QT DAW audio engine.
//!
//! Phase 0 contents:
//! - [`devices`]: find audio hosts (WASAPI, ASIO) and the interfaces they expose.
//! - [`io`]: open input/output streams in any sample format and work in `f32` internally.
//! - [`meter`]: lock-free peak meters the audio thread writes and the UI reads.
//! - [`passthrough`]: route live inputs to the outputs (the first real "signal path").
//!
//! Rule for everything that runs on the audio thread: no locks, no file or console I/O,
//! and (almost) no memory allocation. The audio callback has a hard deadline of a few
//! milliseconds; missing it is an audible click ("xrun").

pub mod devices;
pub mod io;
pub mod meter;
pub mod passthrough;

/// Convert a linear amplitude (0.0..=1.0 is full scale) to decibels relative to full scale.
pub fn to_dbfs(linear: f32) -> f32 {
    if linear <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * linear.log10()
    }
}
