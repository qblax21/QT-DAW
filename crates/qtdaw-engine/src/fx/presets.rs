//! Chain presets: ready-made effect chains (a vocal chain, a reverb bus, a master chain...)
//! as starting points. Each entry lists effects in order with the settings that differ
//! from the effect's defaults.

use super::{EffectKind, Params};

/// Where a preset makes sense; the mixer lists track presets on tracks, and so on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetFor {
    Track,
    Bus,
    Master,
}

pub struct ChainPreset {
    pub name: &'static str,
    pub hint: &'static str,
    pub target: PresetFor,
    pub chain: &'static [(EffectKind, &'static [(&'static str, f32)])],
}

impl ChainPreset {
    /// The preset's effects with full settings.
    pub fn build(&self) -> Vec<(EffectKind, Params)> {
        self.chain
            .iter()
            .map(|(kind, overrides)| {
                let mut p = kind.defaults();
                for (key, v) in *overrides {
                    if let Some(i) = kind.param_index(key) {
                        p[i] = *v;
                    }
                }
                (*kind, p)
            })
            .collect()
    }
}

use EffectKind::*;
use PresetFor::*;

pub const CHAIN_PRESETS: &[ChainPreset] = &[
    ChainPreset {
        name: "Lead Vocal",
        hint: "Cuts rumble and boxiness, adds presence and air, evens out the level, tames esses.",
        target: Track,
        chain: &[
            (Eq, &[("low_cut", 90.0), ("mid1_freq", 300.0), ("mid1_gain", -2.5), ("mid1_q", 1.2), ("mid2_freq", 3000.0), ("mid2_gain", 2.5), ("mid2_q", 0.8), ("high_freq", 10_000.0), ("high_gain", 3.0)]),
            (Compressor, &[("threshold", -20.0), ("ratio", 3.0), ("attack", 5.0), ("release", 100.0), ("makeup", 4.0)]),
            (DeEsser, &[("freq", 6500.0), ("threshold", -30.0), ("range", 8.0)]),
        ],
    },
    ChainPreset {
        name: "Backing Vocals",
        hint: "Thinner and more controlled than a lead, so it sits behind it.",
        target: Track,
        chain: &[
            (Eq, &[("low_cut", 150.0), ("mid1_freq", 400.0), ("mid1_gain", -3.0), ("high_freq", 10_000.0), ("high_gain", 2.0)]),
            (Compressor, &[("threshold", -24.0), ("ratio", 4.0), ("attack", 10.0), ("release", 150.0), ("makeup", 5.0)]),
            (DeEsser, &[("threshold", -34.0), ("range", 10.0)]),
        ],
    },
    ChainPreset {
        name: "Spoken Voice",
        hint: "Podcast/voice-over: gate out room noise, clear and steady.",
        target: Track,
        chain: &[
            (Gate, &[("threshold", -50.0), ("range", 20.0), ("release", 250.0)]),
            (Eq, &[("low_cut", 80.0), ("mid1_freq", 250.0), ("mid1_gain", -2.0), ("mid2_freq", 3500.0), ("mid2_gain", 2.0)]),
            (Compressor, &[("threshold", -22.0), ("ratio", 3.5), ("attack", 8.0), ("release", 150.0), ("makeup", 6.0)]),
            (DeEsser, &[("threshold", -32.0)]),
        ],
    },
    ChainPreset {
        name: "Acoustic Guitar",
        hint: "Less boom, more sparkle, gently controlled.",
        target: Track,
        chain: &[
            (Eq, &[("low_cut", 80.0), ("mid1_freq", 250.0), ("mid1_gain", -3.0), ("mid2_freq", 5000.0), ("mid2_gain", 2.0), ("mid2_q", 0.7)]),
            (Compressor, &[("threshold", -18.0), ("ratio", 2.5), ("attack", 15.0), ("release", 150.0), ("makeup", 2.0)]),
        ],
    },
    ChainPreset {
        name: "Electric Guitar",
        hint: "Clears the low end for the bass and trims fizz.",
        target: Track,
        chain: &[(Eq, &[("low_cut", 100.0), ("mid1_freq", 500.0), ("mid1_gain", -2.0), ("mid2_freq", 2500.0), ("mid2_gain", 2.0), ("high_cut", 9000.0)])],
    },
    ChainPreset {
        name: "Bass",
        hint: "Solid low end, less mud, more definition, steady level.",
        target: Track,
        chain: &[
            (Eq, &[("low_cut", 35.0), ("low_freq", 80.0), ("low_gain", 2.0), ("mid1_freq", 250.0), ("mid1_gain", -3.0), ("mid2_freq", 900.0), ("mid2_gain", 2.0)]),
            (Compressor, &[("threshold", -20.0), ("ratio", 4.0), ("attack", 20.0), ("release", 200.0), ("makeup", 4.0)]),
        ],
    },
    ChainPreset {
        name: "Kick",
        hint: "Gate out the bleed, add thump and click, scoop the cardboard.",
        target: Track,
        chain: &[
            (Gate, &[("threshold", -40.0), ("hold", 60.0), ("release", 100.0)]),
            (Eq, &[("low_cut", 30.0), ("low_freq", 60.0), ("low_gain", 3.0), ("mid1_freq", 350.0), ("mid1_gain", -5.0), ("mid1_q", 1.5), ("mid2_freq", 4000.0), ("mid2_gain", 3.0), ("mid2_q", 1.5)]),
        ],
    },
    ChainPreset {
        name: "Snare",
        hint: "Gate out the bleed, add body and crack.",
        target: Track,
        chain: &[
            (Gate, &[("threshold", -35.0), ("hold", 80.0), ("release", 120.0)]),
            (Eq, &[("low_cut", 80.0), ("mid1_freq", 200.0), ("mid1_gain", 2.0), ("mid2_freq", 5000.0), ("mid2_gain", 2.0), ("high_gain", 2.0)]),
            (Compressor, &[("threshold", -18.0), ("ratio", 4.0), ("attack", 5.0), ("release", 80.0), ("makeup", 2.0)]),
        ],
    },
    ChainPreset {
        name: "Reverb Bus",
        hint: "A medium hall at 100% wet. Send tracks to this bus.",
        target: Bus,
        chain: &[(Reverb, &[("size", 75.0), ("damping", 50.0), ("pre_delay", 25.0), ("low_cut", 250.0), ("mix", 100.0)])],
    },
    ChainPreset {
        name: "Room Bus",
        hint: "A short room at 100% wet, for drums and guitars.",
        target: Bus,
        chain: &[(Reverb, &[("size", 35.0), ("damping", 60.0), ("pre_delay", 5.0), ("low_cut", 200.0), ("mix", 100.0)])],
    },
    ChainPreset {
        name: "Delay Bus",
        hint: "Quarter-note echoes at 100% wet, filtered so they stay behind the vocal.",
        target: Bus,
        chain: &[(Delay, &[("sync", 2.0), ("feedback", 30.0), ("low_cut", 200.0), ("high_cut", 5000.0), ("mix", 100.0)])],
    },
    ChainPreset {
        name: "Drum Bus",
        hint: "Parallel 'glue' compression plus a little low and top.",
        target: Bus,
        chain: &[
            (Compressor, &[("threshold", -16.0), ("ratio", 4.0), ("attack", 20.0), ("release", 120.0), ("makeup", 3.0), ("mix", 60.0)]),
            (Eq, &[("low_freq", 60.0), ("low_gain", 1.5), ("high_freq", 10_000.0), ("high_gain", 1.5)]),
        ],
    },
    ChainPreset {
        name: "Master",
        hint: "Light glue compression and a limiter so the mix never clips.",
        target: Master,
        chain: &[
            (Eq, &[("low_cut", 25.0)]),
            (Compressor, &[("threshold", -12.0), ("ratio", 2.0), ("attack", 30.0), ("release", 200.0), ("knee", 6.0), ("makeup", 1.0)]),
            (Limiter, &[("gain", 3.0), ("ceiling", -1.0), ("release", 80.0)]),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_settings_exist_and_are_in_range() {
        for preset in CHAIN_PRESETS {
            for (kind, overrides) in preset.chain {
                for (key, v) in *overrides {
                    let i = kind.param_index(key).unwrap_or_else(|| panic!("{}: {kind:?} has no {key}", preset.name));
                    let spec = &kind.params()[i];
                    assert!((spec.min..=spec.max).contains(v), "{}: {key} = {v}", preset.name);
                }
            }
        }
    }
}
