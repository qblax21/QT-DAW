//! Built-in effects ("Audio FX" inserts) and the chain presets that combine them.
//!
//! How an effect gets from the UI to the audio thread:
//! - The session stores each insert as an [`InsertModel`](crate::session::InsertModel):
//!   its kind and its settings by name.
//! - The UI mirrors it into an [`InsertRt`]: one atomic per setting (so turning a knob never
//!   blocks audio), plus the effect's own memory (filter states, delay lines...).
//! - Tracks, buses and the master each hold a list of `InsertRt`s; the audio thread runs
//!   them in order with [`run_chain`].
//!
//! Track chains are mono (tracks are mono); bus and master chains are stereo.

pub mod biquad;
mod delay;
mod dynamics;
pub mod eq;
pub mod presets;
mod reverb;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

use crate::engine::AtomicF32;

/// The most settings any one effect has.
pub const MAX_PARAMS: usize = 12;

/// A full set of settings, indexed like the effect's [`ParamSpec`] list.
pub type Params = [f32; MAX_PARAMS];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    Eq,
    Compressor,
    Gate,
    DeEsser,
    Limiter,
    Delay,
    Reverb,
}

impl EffectKind {
    pub const ALL: [EffectKind; 7] = [
        EffectKind::Eq,
        EffectKind::Compressor,
        EffectKind::Gate,
        EffectKind::DeEsser,
        EffectKind::Limiter,
        EffectKind::Delay,
        EffectKind::Reverb,
    ];

    pub fn name(self) -> &'static str {
        match self {
            EffectKind::Eq => "Channel EQ",
            EffectKind::Compressor => "Compressor",
            EffectKind::Gate => "Noise Gate",
            EffectKind::DeEsser => "De-Esser",
            EffectKind::Limiter => "Limiter",
            EffectKind::Delay => "Delay",
            EffectKind::Reverb => "Reverb",
        }
    }

    /// Short label for a mixer insert slot.
    pub fn short(self) -> &'static str {
        match self {
            EffectKind::Eq => "EQ",
            EffectKind::Compressor => "Comp",
            EffectKind::Gate => "Gate",
            EffectKind::DeEsser => "DeEss",
            EffectKind::Limiter => "Limit",
            EffectKind::Delay => "Delay",
            EffectKind::Reverb => "Verb",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            EffectKind::Eq => "Shape the tone: cut rumble, remove mud, add presence or air.",
            EffectKind::Compressor => "Evens out loud and quiet parts so a track sits steadily in the mix.",
            EffectKind::Gate => "Silences the track when it's quiet (bleed, hiss, hum between notes).",
            EffectKind::DeEsser => "Tames harsh 's' and 't' sounds in vocals.",
            EffectKind::Limiter => "Stops peaks going above the ceiling. Usually last on the master.",
            EffectKind::Delay => "Echoes, in time with the song's tempo.",
            EffectKind::Reverb => "Room sound. Best on a bus that tracks send to, with Mix at 100%.",
        }
    }

    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            EffectKind::Eq => eq::PARAMS,
            EffectKind::Compressor => dynamics::COMP_PARAMS,
            EffectKind::Gate => dynamics::GATE_PARAMS,
            EffectKind::DeEsser => dynamics::DEESS_PARAMS,
            EffectKind::Limiter => dynamics::LIMIT_PARAMS,
            EffectKind::Delay => delay::PARAMS,
            EffectKind::Reverb => reverb::PARAMS,
        }
    }

    /// Effects that turn the level down and can show how much ("gain reduction").
    pub fn shows_reduction(self) -> bool {
        matches!(self, EffectKind::Compressor | EffectKind::Gate | EffectKind::DeEsser | EffectKind::Limiter)
    }

    /// Settings with their default values.
    pub fn defaults(self) -> Params {
        let mut p = [0.0; MAX_PARAMS];
        for (v, spec) in p.iter_mut().zip(self.params()) {
            *v = spec.default;
        }
        p
    }

    /// Index of the setting called `key`.
    pub fn param_index(self, key: &str) -> Option<usize> {
        self.params().iter().position(|p| p.key == key)
    }

    fn make(self) -> Box<dyn Effect> {
        match self {
            EffectKind::Eq => Box::new(eq::ChannelEq::default()),
            EffectKind::Compressor => Box::new(dynamics::Compressor::default()),
            EffectKind::Gate => Box::new(dynamics::Gate::default()),
            EffectKind::DeEsser => Box::new(dynamics::DeEsser::default()),
            EffectKind::Limiter => Box::new(dynamics::Limiter::new()),
            EffectKind::Delay => Box::new(delay::Delay::new()),
            EffectKind::Reverb => Box::new(reverb::Reverb::new()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unit {
    Db,
    Hz,
    Ms,
    /// Compression ratio, shown as "4.0:1".
    Ratio,
    Percent,
    /// Filter width.
    Q,
    /// One of a list of named options (the value is the index).
    Choice(&'static [&'static str]),
}

/// One effect setting: its range, default and how to show it.
#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    /// Stable name used in session files (never rename one that has shipped).
    pub key: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub unit: Unit,
    /// Knob moves in equal steps of ratio (right for frequencies and times).
    pub log: bool,
}

impl ParamSpec {
    pub const fn new(key: &'static str, label: &'static str, min: f32, max: f32, default: f32, unit: Unit, log: bool) -> Self {
        Self { key, label, min, max, default, unit, log }
    }

    pub fn clamp(&self, v: f32) -> f32 {
        let v = if v.is_finite() { v.clamp(self.min, self.max) } else { self.default };
        if matches!(self.unit, Unit::Choice(_)) { v.round() } else { v }
    }

    /// Knob position (0..1) for a value.
    pub fn to_norm(&self, v: f32) -> f32 {
        let v = self.clamp(v);
        if self.log {
            (v / self.min).ln() / (self.max / self.min).ln()
        } else {
            (v - self.min) / (self.max - self.min)
        }
    }

    /// Value for a knob position (0..1).
    pub fn from_norm(&self, n: f32) -> f32 {
        let n = n.clamp(0.0, 1.0);
        let v = if self.log { self.min * (self.max / self.min).powf(n) } else { self.min + n * (self.max - self.min) };
        self.clamp(v)
    }

    pub fn format(&self, v: f32) -> String {
        match self.unit {
            Unit::Db => format!("{v:.1} dB"),
            Unit::Hz if v >= 1000.0 => format!("{:.2} kHz", v / 1000.0),
            Unit::Hz => format!("{v:.0} Hz"),
            Unit::Ms if v < 10.0 => format!("{v:.1} ms"),
            Unit::Ms => format!("{v:.0} ms"),
            Unit::Ratio => format!("{v:.1}:1"),
            Unit::Percent => format!("{v:.0}%"),
            Unit::Q => format!("Q {v:.2}"),
            Unit::Choice(names) => names.get(v.round() as usize).copied().unwrap_or("?").to_string(),
        }
    }
}

/// What every effect gets besides its audio and settings.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    pub sr: f32,
    /// Song tempo in BPM (for tempo-synced delays).
    pub tempo: f32,
}

/// The audio-thread half of an effect.
trait Effect: Send {
    /// Process `chans` (1 or 2 channels of equal length) in place. Returns how many dB the
    /// effect is currently turning the signal down (0 for effects that don't).
    fn process(&mut self, p: &Params, ctx: &Ctx, chans: &mut [&mut [f32]]) -> f32;

    /// Forget all memory (tails, envelopes), as if just created.
    fn reset(&mut self);
}

/// One insert as the audio thread sees it.
pub struct InsertRt {
    pub id: u64,
    pub kind: EffectKind,
    params: [AtomicF32; MAX_PARAMS],
    pub bypass: AtomicBool,
    /// Most gain reduction (dB, as f32 bits) since the UI last looked.
    reduction: AtomicU32,
    /// Only the audio thread locks this, so it never waits; the lock just lets the effect's
    /// memory live inside a shared `Arc`.
    dsp: Mutex<Dsp>,
}

struct Dsp {
    fx: Box<dyn Effect>,
    /// False while bypassed; the effect is reset when it comes back so old tails don't
    /// suddenly replay.
    active: bool,
}

impl InsertRt {
    /// Allocates the effect's buffers, so call this from the UI thread.
    pub fn new(id: u64, kind: EffectKind) -> Self {
        let defaults = kind.defaults();
        Self {
            id,
            kind,
            params: std::array::from_fn(|i| AtomicF32::new(defaults[i])),
            bypass: AtomicBool::new(false),
            reduction: AtomicU32::new(0),
            dsp: Mutex::new(Dsp { fx: kind.make(), active: true }),
        }
    }

    pub fn set_params(&self, p: &Params) {
        for (a, v) in self.params.iter().zip(p) {
            a.set(*v);
        }
    }

    /// Gain reduction in dB since the last call (0 = none).
    pub fn take_reduction(&self) -> f32 {
        f32::from_bits(self.reduction.swap(0, Ordering::Relaxed))
    }

    pub fn process(&self, ctx: &Ctx, chans: &mut [&mut [f32]]) {
        let bypassed = self.bypass.load(Ordering::Relaxed);
        let Ok(mut dsp) = self.dsp.try_lock() else { return };
        if bypassed {
            dsp.active = false;
            return;
        }
        if !dsp.active {
            dsp.fx.reset();
            dsp.active = true;
        }
        let mut p = [0.0; MAX_PARAMS];
        for ((v, spec), a) in p.iter_mut().zip(self.kind.params()).zip(&self.params) {
            *v = spec.clamp(a.get());
        }
        let gr = dsp.fx.process(&p, ctx, chans);
        // Positive floats compare the same way as their bit patterns.
        self.reduction.fetch_max(gr.max(0.0).to_bits(), Ordering::Relaxed);
    }
}

/// Run a chain of inserts over `chans`, first to last.
pub fn run_chain(chain: &[std::sync::Arc<InsertRt>], ctx: &Ctx, chans: &mut [&mut [f32]]) {
    for fx in chain {
        fx.process(ctx, chans);
    }
}

// ------------------------------------------------------------------------- shared helpers

/// One-pole smoothing coefficient for a time constant in milliseconds.
fn coef(ms: f32, sr: f32) -> f32 {
    (-1.0 / (ms.max(0.01) * 0.001 * sr)).exp()
}

fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn lin_to_db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

/// Dry/wet gains for a mix knob (0..1): the dry signal stays at full level up to 50%, the
/// wet one reaches full level at 50%, so 100% = wet only (what a send bus wants).
fn dry_wet(mix: f32) -> (f32, f32) {
    ((2.0 * (1.0 - mix)).min(1.0), (2.0 * mix).min(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_specs_are_sane() {
        for kind in EffectKind::ALL {
            let specs = kind.params();
            assert!(specs.len() <= MAX_PARAMS, "{kind:?}");
            for s in specs {
                assert!(s.min < s.max && (s.min..=s.max).contains(&s.default), "{kind:?} {}", s.key);
                assert!(!s.log || s.min > 0.0, "{kind:?} {}", s.key);
                let back = s.from_norm(s.to_norm(s.default));
                assert!((back - s.default).abs() <= (s.max - s.min) * 1e-4, "{kind:?} {}", s.key);
            }
            let mut keys: Vec<_> = specs.iter().map(|s| s.key).collect();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), specs.len(), "duplicate key in {kind:?}");
        }
    }

    /// Every effect, with every setting at its default and at both extremes, must turn a
    /// loud noise burst into finite audio that doesn't blow up.
    #[test]
    fn every_effect_stays_stable() {
        let ctx = Ctx { sr: 48_000.0, tempo: 120.0 };
        for kind in EffectKind::ALL {
            for setting in 0..3 {
                let fx = InsertRt::new(1, kind);
                let mut p = kind.defaults();
                for (v, s) in p.iter_mut().zip(kind.params()) {
                    *v = match setting {
                        0 => s.default,
                        1 => s.min,
                        _ => s.max,
                    };
                }
                fx.set_params(&p);
                let mut seed = 1u32;
                let mut noise = || {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
                };
                let mut loudest = 0.0f32;
                for block in 0..200 {
                    let mut l: Vec<f32> = (0..256).map(|_| if block < 20 { noise() } else { 0.0 }).collect();
                    let mut r = l.clone();
                    fx.process(&ctx, &mut [&mut l, &mut r]);
                    for s in l.iter().chain(&r) {
                        assert!(s.is_finite(), "{kind:?} setting {setting}");
                        loudest = loudest.max(s.abs());
                    }
                }
                assert!(loudest < 40.0, "{kind:?} setting {setting}: {loudest}");
            }
        }
    }
}
