//! Channel EQ: low cut, low shelf, two bells, high shelf, high cut.

use super::biquad::{Coeffs, State};
use super::{Ctx, Effect, MAX_PARAMS, ParamSpec, Params, Unit};

pub const LOW_CUT: usize = 0;
pub const LOW_FREQ: usize = 1;
pub const LOW_GAIN: usize = 2;
pub const MID1_FREQ: usize = 3;
pub const MID1_GAIN: usize = 4;
pub const MID1_Q: usize = 5;
pub const MID2_FREQ: usize = 6;
pub const MID2_GAIN: usize = 7;
pub const MID2_Q: usize = 8;
pub const HIGH_FREQ: usize = 9;
pub const HIGH_GAIN: usize = 10;
pub const HIGH_CUT: usize = 11;

/// At these settings the cut filters are off.
pub const LOW_CUT_OFF: f32 = 20.0;
pub const HIGH_CUT_OFF: f32 = 20_000.0;

pub const PARAMS: &[ParamSpec] = &[
    ParamSpec::new("low_cut", "Low Cut", LOW_CUT_OFF, 1000.0, LOW_CUT_OFF, Unit::Hz, true),
    ParamSpec::new("low_freq", "Low Freq", 30.0, 500.0, 100.0, Unit::Hz, true),
    ParamSpec::new("low_gain", "Low Gain", -18.0, 18.0, 0.0, Unit::Db, false),
    ParamSpec::new("mid1_freq", "Freq", 80.0, 8000.0, 400.0, Unit::Hz, true),
    ParamSpec::new("mid1_gain", "Gain", -18.0, 18.0, 0.0, Unit::Db, false),
    ParamSpec::new("mid1_q", "Q", 0.3, 8.0, 1.0, Unit::Q, true),
    ParamSpec::new("mid2_freq", "Freq", 300.0, 16_000.0, 2500.0, Unit::Hz, true),
    ParamSpec::new("mid2_gain", "Gain", -18.0, 18.0, 0.0, Unit::Db, false),
    ParamSpec::new("mid2_q", "Q", 0.3, 8.0, 1.0, Unit::Q, true),
    ParamSpec::new("high_freq", "High Freq", 1000.0, 16_000.0, 8000.0, Unit::Hz, true),
    ParamSpec::new("high_gain", "High Gain", -18.0, 18.0, 0.0, Unit::Db, false),
    ParamSpec::new("high_cut", "High Cut", 2000.0, HIGH_CUT_OFF, HIGH_CUT_OFF, Unit::Hz, true),
];

/// Columns for the editor: which settings belong to which band.
pub const BANDS: &[(&str, &[usize])] = &[
    ("Low Cut", &[LOW_CUT]),
    ("Low", &[LOW_FREQ, LOW_GAIN]),
    ("Mid 1", &[MID1_FREQ, MID1_GAIN, MID1_Q]),
    ("Mid 2", &[MID2_FREQ, MID2_GAIN, MID2_Q]),
    ("High", &[HIGH_FREQ, HIGH_GAIN]),
    ("High Cut", &[HIGH_CUT]),
];

const STAGES: usize = 7;

/// The filters for a set of settings. `None` = that stage is switched off (flat).
/// The low cut is two stages (a 24 dB/octave Butterworth), the high cut is 12 dB/octave.
pub fn stages(p: &Params, sr: f32) -> [Option<Coeffs>; STAGES] {
    let on = |gain: f32| gain.abs() > 0.05;
    let low_cut = p[LOW_CUT] > LOW_CUT_OFF + 0.5;
    let high_cut = p[HIGH_CUT] < HIGH_CUT_OFF - 1.0 && p[HIGH_CUT] < sr * 0.45;
    [
        low_cut.then(|| Coeffs::highpass(p[LOW_CUT], 0.541_196, sr)),
        low_cut.then(|| Coeffs::highpass(p[LOW_CUT], 1.306_563, sr)),
        on(p[LOW_GAIN]).then(|| Coeffs::low_shelf(p[LOW_FREQ], p[LOW_GAIN], sr)),
        on(p[MID1_GAIN]).then(|| Coeffs::peak(p[MID1_FREQ], p[MID1_Q], p[MID1_GAIN], sr)),
        on(p[MID2_GAIN]).then(|| Coeffs::peak(p[MID2_FREQ], p[MID2_Q], p[MID2_GAIN], sr)),
        on(p[HIGH_GAIN]).then(|| Coeffs::high_shelf(p[HIGH_FREQ], p[HIGH_GAIN], sr)),
        high_cut.then(|| Coeffs::lowpass(p[HIGH_CUT], std::f32::consts::FRAC_1_SQRT_2, sr)),
    ]
}

/// The whole EQ's boost/cut at `freq`, in dB (for drawing the curve).
pub fn response_db(stages: &[Option<Coeffs>], freq: f32, sr: f32) -> f32 {
    stages.iter().flatten().map(|c| c.magnitude_db(freq, sr)).sum()
}

#[derive(Default)]
pub struct ChannelEq {
    /// Settings the current filters were made for.
    made_for: Option<(Params, f32)>,
    stages: [Option<Coeffs>; STAGES],
    state: [[State; 2]; STAGES],
}

impl Effect for ChannelEq {
    fn process(&mut self, p: &Params, ctx: &Ctx, chans: &mut [&mut [f32]]) -> f32 {
        if self.made_for != Some((*p, ctx.sr)) {
            let new = stages(p, ctx.sr);
            for (i, (old, new)) in self.stages.iter().zip(&new).enumerate() {
                if old.is_some() != new.is_some() {
                    // A stage switching on starts from silence, not from stale memory.
                    self.state[i] = Default::default();
                }
            }
            self.stages = new;
            self.made_for = Some((*p, ctx.sr));
        }
        for (ch, buf) in chans.iter_mut().enumerate().take(2) {
            for (c, st) in self.stages.iter().zip(self.state.iter_mut()) {
                let Some(c) = c else { continue };
                let st = &mut st[ch];
                for s in buf.iter_mut() {
                    *s = st.run(c, *s);
                }
            }
        }
        0.0
    }

    fn reset(&mut self) {
        self.state = Default::default();
    }
}

const _: () = assert!(PARAMS.len() == MAX_PARAMS);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::EffectKind;

    #[test]
    fn flat_by_default_and_bands_add_up() {
        let sr = 48_000.0;
        let mut p = EffectKind::Eq.defaults();
        assert!(stages(&p, sr).iter().all(Option::is_none));
        p[MID1_GAIN] = 6.0;
        p[MID1_FREQ] = 1000.0;
        p[LOW_CUT] = 100.0;
        let s = stages(&p, sr);
        assert!((response_db(&s, 1000.0, sr) - 6.0).abs() < 0.1);
        // 24 dB/octave: two octaves below the cutoff is about -48 dB.
        assert!(response_db(&s, 25.0, sr) < -44.0);
    }
}
