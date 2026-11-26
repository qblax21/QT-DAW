//! Algorithmic reverb.
//!
//! The classic "Freeverb" design (Jezar at Dreampoint, public domain): eight feedback
//! comb filters in parallel make the dense decay, four all-pass filters in series smear
//! it so it sounds smooth. The right channel uses slightly different lengths, which is
//! what makes it wide. Adds pre-delay and a low cut so the reverb doesn't muddy the mix.

use super::biquad::{Coeffs, State};
use super::{Ctx, Effect, ParamSpec, Params, Unit, dry_wet};

const SIZE: usize = 0;
const DAMPING: usize = 1;
const PRE_DELAY: usize = 2;
const LOW_CUT: usize = 3;
const WIDTH: usize = 4;
const MIX: usize = 5;

pub const PARAMS: &[ParamSpec] = &[
    ParamSpec::new("size", "Size", 0.0, 100.0, 70.0, Unit::Percent, false),
    ParamSpec::new("damping", "Damping", 0.0, 100.0, 50.0, Unit::Percent, false),
    ParamSpec::new("pre_delay", "Pre-Delay", 0.0, 200.0, 20.0, Unit::Ms, false),
    ParamSpec::new("low_cut", "Low Cut", 20.0, 1000.0, 200.0, Unit::Hz, true),
    ParamSpec::new("width", "Width", 0.0, 100.0, 100.0, Unit::Percent, false),
    ParamSpec::new("mix", "Mix", 0.0, 100.0, 25.0, Unit::Percent, false),
];

/// Freeverb's tunings, in samples at 44.1 kHz.
const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
const SPREAD: usize = 23;
const MAX_SR: f32 = 192_000.0;
const MAX_PRE_SEC: f32 = 0.25;

/// A delay line whose usable length can change without reallocating.
struct Line {
    buf: Vec<f32>,
    len: usize,
    pos: usize,
}

impl Line {
    fn new(max: usize) -> Self {
        Self { buf: vec![0.0; max.max(1)], len: 1, pos: 0 }
    }

    fn set_len(&mut self, len: usize) {
        self.len = len.clamp(1, self.buf.len());
        self.pos %= self.len;
        self.buf.fill(0.0);
    }

    /// The oldest sample (written `len` samples ago).
    #[inline]
    fn front(&self) -> f32 {
        self.buf[self.pos]
    }

    #[inline]
    fn push(&mut self, x: f32) {
        self.buf[self.pos] = x;
        self.pos += 1;
        if self.pos == self.len {
            self.pos = 0;
        }
    }
}

struct Comb {
    line: Line,
    store: f32,
}

impl Comb {
    #[inline]
    fn run(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let y = self.line.front();
        self.store = y * (1.0 - damp) + self.store * damp;
        self.line.push(x + self.store * feedback);
        y
    }
}

#[inline]
fn allpass(line: &mut Line, x: f32) -> f32 {
    let b = line.front();
    line.push(x + b * 0.5);
    b - x
}

struct Tank {
    combs: Vec<Comb>,
    allpasses: Vec<Line>,
}

impl Tank {
    fn new(spread: usize) -> Self {
        let scale = MAX_SR / 44_100.0;
        Self {
            combs: COMBS
                .iter()
                .map(|&n| Comb { line: Line::new(((n + spread) as f32 * scale) as usize + 1), store: 0.0 })
                .collect(),
            allpasses: ALLPASSES.iter().map(|&n| Line::new(((n + spread) as f32 * scale) as usize + 1)).collect(),
        }
    }

    fn tune(&mut self, sr: f32, spread: usize) {
        let scale = sr / 44_100.0;
        for (c, &n) in self.combs.iter_mut().zip(&COMBS) {
            c.line.set_len(((n + spread) as f32 * scale) as usize);
            c.store = 0.0;
        }
        for (a, &n) in self.allpasses.iter_mut().zip(&ALLPASSES) {
            a.set_len(((n + spread) as f32 * scale) as usize);
        }
    }

    #[inline]
    fn run(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let mut out = 0.0;
        for c in &mut self.combs {
            out += c.run(x, feedback, damp);
        }
        for a in &mut self.allpasses {
            out = allpass(a, out);
        }
        out
    }

    fn clear(&mut self) {
        for c in &mut self.combs {
            c.line.buf.fill(0.0);
            c.store = 0.0;
        }
        for a in &mut self.allpasses {
            a.buf.fill(0.0);
        }
    }
}

pub struct Reverb {
    tanks: [Tank; 2],
    pre: Vec<f32>,
    pre_pos: usize,
    sr: f32,
    low_cut: (f32, Coeffs),
    low_cut_state: State,
}

impl Reverb {
    pub fn new() -> Self {
        Self {
            tanks: [Tank::new(0), Tank::new(SPREAD)],
            pre: vec![0.0; (MAX_PRE_SEC * MAX_SR) as usize],
            pre_pos: 0,
            sr: 0.0,
            low_cut: (0.0, Coeffs::IDENTITY),
            low_cut_state: State::default(),
        }
    }
}

impl Effect for Reverb {
    fn process(&mut self, p: &Params, ctx: &Ctx, chans: &mut [&mut [f32]]) -> f32 {
        if self.sr != ctx.sr {
            self.sr = ctx.sr;
            self.tanks[0].tune(ctx.sr, 0);
            self.tanks[1].tune(ctx.sr, SPREAD);
            self.low_cut.0 = 0.0;
        }
        if self.low_cut.0 != p[LOW_CUT] {
            self.low_cut = (p[LOW_CUT], Coeffs::highpass(p[LOW_CUT], 0.707, ctx.sr));
        }
        let feedback = p[SIZE] / 100.0 * 0.28 + 0.7;
        let damp = p[DAMPING] / 100.0 * 0.4;
        let width = p[WIDTH] / 100.0;
        let (dry, wet) = dry_wet(p[MIX] / 100.0);
        // Freeverb's gains: quiet input (many combs add up), loud output.
        let (in_gain, out_gain) = (0.015, 3.0);
        let (w1, w2) = (out_gain * (width / 2.0 + 0.5), out_gain * ((1.0 - width) / 2.0));
        let pre_len = self.pre.len();
        let pre = ((p[PRE_DELAY] / 1000.0 * ctx.sr) as usize).min(pre_len - 1);
        let stereo = chans.len() >= 2;

        for i in 0..chans[0].len() {
            let input = if stereo { 0.5 * (chans[0][i] + chans[1][i]) } else { chans[0][i] };
            self.pre[self.pre_pos] = input;
            let delayed = self.pre[(self.pre_pos + pre_len - pre) % pre_len];
            self.pre_pos = (self.pre_pos + 1) % pre_len;
            let x = self.low_cut_state.run(&self.low_cut.1, delayed) * in_gain;

            let l = self.tanks[0].run(x, feedback, damp);
            if stereo {
                let r = self.tanks[1].run(x, feedback, damp);
                chans[0][i] = chans[0][i] * dry + (l * w1 + r * w2) * wet;
                chans[1][i] = chans[1][i] * dry + (r * w1 + l * w2) * wet;
            } else {
                chans[0][i] = chans[0][i] * dry + l * out_gain * wet;
            }
        }
        0.0
    }

    fn reset(&mut self) {
        for t in &mut self.tanks {
            t.clear();
        }
        self.pre.fill(0.0);
        self.low_cut_state.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::EffectKind;

    #[test]
    fn a_click_leaves_a_decaying_tail() {
        let ctx = Ctx { sr: 48_000.0, tempo: 120.0 };
        let mut v = Reverb::new();
        let mut p = EffectKind::Reverb.defaults();
        p[MIX] = 100.0;
        let mut l = vec![0.0f32; 96_000];
        let mut r = vec![0.0f32; 96_000];
        l[0] = 1.0;
        r[0] = 1.0;
        v.process(&p, &ctx, &mut [&mut l, &mut r]);
        let energy = |x: &[f32]| x.iter().map(|s| s * s).sum::<f32>();
        let early = energy(&l[2_000..12_000]);
        let late = energy(&l[80_000..90_000]);
        assert!(early > 1e-4, "{early}");
        assert!(late < early / 10.0, "{early} {late}");
        assert!(l[2_000..12_000] != r[2_000..12_000], "should be stereo");
    }
}
