//! Effects that change the level over time: compressor, noise gate, de-esser, limiter.
//!
//! They all follow the signal's level (the "envelope") and turn the audio down when it
//! crosses a threshold. On stereo buses both channels are turned down together, so the
//! stereo image doesn't wander.

use super::biquad::{Coeffs, State};
use super::{Ctx, Effect, ParamSpec, Params, Unit, coef, db_to_lin, dry_wet, lin_to_db};

/// Loudest absolute sample across the channels at frame `i`.
#[inline]
fn frame_peak(chans: &[&mut [f32]], i: usize) -> f32 {
    chans.iter().take(2).fold(0.0f32, |m, c| m.max(c[i].abs()))
}

#[inline]
fn apply_gain(chans: &mut [&mut [f32]], i: usize, g: f32) {
    for c in chans.iter_mut().take(2) {
        c[i] *= g;
    }
}

// ------------------------------------------------------------------------- compressor

const C_THRESH: usize = 0;
const C_RATIO: usize = 1;
const C_ATTACK: usize = 2;
const C_RELEASE: usize = 3;
const C_KNEE: usize = 4;
const C_MAKEUP: usize = 5;
const C_MIX: usize = 6;

pub const COMP_PARAMS: &[ParamSpec] = &[
    ParamSpec::new("threshold", "Threshold", -60.0, 0.0, -18.0, Unit::Db, false),
    ParamSpec::new("ratio", "Ratio", 1.0, 20.0, 4.0, Unit::Ratio, true),
    ParamSpec::new("attack", "Attack", 0.1, 100.0, 10.0, Unit::Ms, true),
    ParamSpec::new("release", "Release", 10.0, 1000.0, 120.0, Unit::Ms, true),
    ParamSpec::new("knee", "Knee", 0.0, 12.0, 6.0, Unit::Db, false),
    ParamSpec::new("makeup", "Make-up", 0.0, 24.0, 0.0, Unit::Db, false),
    ParamSpec::new("mix", "Mix", 0.0, 100.0, 100.0, Unit::Percent, false),
];

/// How many dB a compressor turns down a signal at `level_db` (soft knee).
pub fn comp_reduction(level_db: f32, thresh: f32, ratio: f32, knee: f32) -> f32 {
    let over = level_db - thresh;
    let slope = 1.0 - 1.0 / ratio.max(1.0);
    if knee > 0.0 && over.abs() <= knee / 2.0 {
        slope * (over + knee / 2.0).powi(2) / (2.0 * knee)
    } else if over > 0.0 {
        slope * over
    } else {
        0.0
    }
}

#[derive(Default)]
pub struct Compressor {
    /// Peak level follower: jumps up instantly, falls over ~10 ms, so the detector sees
    /// the wave's peaks rather than every swing through zero.
    env: f32,
    /// Current gain reduction in dB (smoothed by attack/release).
    gr: f32,
}

impl Effect for Compressor {
    fn process(&mut self, p: &Params, ctx: &Ctx, chans: &mut [&mut [f32]]) -> f32 {
        let att = coef(p[C_ATTACK], ctx.sr);
        let rel = coef(p[C_RELEASE], ctx.sr);
        let makeup = p[C_MAKEUP];
        let (dry, wet) = dry_wet(p[C_MIX] / 100.0);
        let env_rel = coef(10.0, ctx.sr);
        let mut most = 0.0f32;
        for i in 0..chans[0].len() {
            let x = frame_peak(chans, i);
            self.env = if x > self.env { x } else { x + (self.env - x) * env_rel };
            let target = comp_reduction(lin_to_db(self.env), p[C_THRESH], p[C_RATIO], p[C_KNEE]);
            let k = if target > self.gr { att } else { rel };
            self.gr = target + (self.gr - target) * k;
            most = most.max(self.gr);
            let g = db_to_lin(makeup - self.gr);
            for c in chans.iter_mut().take(2) {
                c[i] = c[i] * dry + c[i] * g * wet;
            }
        }
        most
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

// ------------------------------------------------------------------------- noise gate

const G_THRESH: usize = 0;
const G_RANGE: usize = 1;
const G_ATTACK: usize = 2;
const G_HOLD: usize = 3;
const G_RELEASE: usize = 4;

pub const GATE_PARAMS: &[ParamSpec] = &[
    ParamSpec::new("threshold", "Threshold", -80.0, 0.0, -50.0, Unit::Db, false),
    ParamSpec::new("range", "Reduction", 0.0, 80.0, 60.0, Unit::Db, false),
    ParamSpec::new("attack", "Attack", 0.05, 50.0, 0.5, Unit::Ms, true),
    ParamSpec::new("hold", "Hold", 1.0, 500.0, 50.0, Unit::Ms, true),
    ParamSpec::new("release", "Release", 5.0, 2000.0, 150.0, Unit::Ms, true),
];

#[derive(Default)]
pub struct Gate {
    env: f32,
    gain: f32,
    hold_left: u32,
}

impl Effect for Gate {
    fn process(&mut self, p: &Params, ctx: &Ctx, chans: &mut [&mut [f32]]) -> f32 {
        let open_at = db_to_lin(p[G_THRESH]);
        // Close a little below the threshold so a note hovering there doesn't chatter.
        let close_at = open_at * db_to_lin(-3.0);
        let floor = db_to_lin(-p[G_RANGE]);
        let att = coef(p[G_ATTACK], ctx.sr);
        let rel = coef(p[G_RELEASE], ctx.sr);
        let env_rel = coef(10.0, ctx.sr);
        let hold = (p[G_HOLD] * 0.001 * ctx.sr) as u32;
        let mut most = 0.0f32;
        for i in 0..chans[0].len() {
            let x = frame_peak(chans, i);
            self.env = if x > self.env { x } else { x + (self.env - x) * env_rel };
            if self.env > open_at || (self.hold_left > 0 && self.env > close_at) {
                self.hold_left = hold;
            } else {
                self.hold_left = self.hold_left.saturating_sub(1);
            }
            let target = if self.hold_left > 0 { 1.0 } else { floor };
            let k = if target > self.gain { att } else { rel };
            self.gain = target + (self.gain - target) * k;
            most = most.max(-lin_to_db(self.gain));
            apply_gain(chans, i, self.gain);
        }
        most
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

// ------------------------------------------------------------------------- de-esser

const D_FREQ: usize = 0;
const D_THRESH: usize = 1;
const D_RANGE: usize = 2;
const D_LISTEN: usize = 3;

pub const DEESS_PARAMS: &[ParamSpec] = &[
    ParamSpec::new("freq", "Frequency", 2000.0, 12_000.0, 6000.0, Unit::Hz, true),
    ParamSpec::new("threshold", "Threshold", -60.0, 0.0, -30.0, Unit::Db, false),
    ParamSpec::new("range", "Max Cut", 0.0, 24.0, 10.0, Unit::Db, false),
    ParamSpec::new("listen", "Listen", 0.0, 1.0, 0.0, Unit::Choice(&["Off", "Esses only"]), false),
];

/// Splits the audio at the chosen frequency and turns down only the top part when it gets
/// loud, so the rest of the voice is untouched.
#[derive(Default)]
pub struct DeEsser {
    made_for: (f32, f32),
    hp: Coeffs,
    state: [State; 2],
    env: f32,
    gr: f32,
}

impl Effect for DeEsser {
    fn process(&mut self, p: &Params, ctx: &Ctx, chans: &mut [&mut [f32]]) -> f32 {
        if self.made_for != (p[D_FREQ], ctx.sr) {
            self.hp = Coeffs::highpass(p[D_FREQ], std::f32::consts::FRAC_1_SQRT_2, ctx.sr);
            self.made_for = (p[D_FREQ], ctx.sr);
        }
        let listen = p[D_LISTEN] >= 0.5;
        let env_att = coef(0.5, ctx.sr);
        let env_rel = coef(60.0, ctx.sr);
        let gr_rel = coef(40.0, ctx.sr);
        let n_ch = chans.len().min(2);
        let mut most = 0.0f32;
        for i in 0..chans[0].len() {
            let mut hi = [0.0f32; 2];
            let mut level = 0.0f32;
            for ch in 0..n_ch {
                hi[ch] = self.state[ch].run(&self.hp, chans[ch][i]);
                level = level.max(hi[ch].abs());
            }
            let k = if level > self.env { env_att } else { env_rel };
            self.env = level + (self.env - level) * k;
            // 3:1 above the threshold, capped at the range.
            let target = ((lin_to_db(self.env) - p[D_THRESH]) * (2.0 / 3.0)).clamp(0.0, p[D_RANGE]);
            self.gr = if target > self.gr { target } else { target + (self.gr - target) * gr_rel };
            most = most.max(self.gr);
            let g = db_to_lin(-self.gr);
            for ch in 0..n_ch {
                let x = chans[ch][i];
                // low + high = the original signal, so with no reduction nothing changes.
                chans[ch][i] = if listen { hi[ch] } else { (x - hi[ch]) + hi[ch] * g };
            }
        }
        most
    }

    fn reset(&mut self) {
        self.state = Default::default();
        self.env = 0.0;
        self.gr = 0.0;
    }
}

// ------------------------------------------------------------------------- limiter

const L_GAIN: usize = 0;
const L_CEILING: usize = 1;
const L_RELEASE: usize = 2;

pub const LIMIT_PARAMS: &[ParamSpec] = &[
    ParamSpec::new("gain", "Gain", 0.0, 24.0, 0.0, Unit::Db, false),
    ParamSpec::new("ceiling", "Ceiling", -12.0, 0.0, -1.0, Unit::Db, false),
    ParamSpec::new("release", "Release", 1.0, 1000.0, 50.0, Unit::Ms, true),
];

/// How far ahead the limiter looks, so it can turn down *before* a peak arrives.
const LOOKAHEAD_SEC: f32 = 0.0015;
/// Enough lookahead room for 192 kHz.
const LA_CAP: usize = 512;

/// A look-ahead peak limiter. It delays the audio by 1.5 ms, sees each peak coming, and
/// lowers the gain smoothly in time. Anything that still gets through is clamped at the
/// ceiling, so the output never goes above it.
pub struct Limiter {
    window: usize,
    delay: [Vec<f32>; 2],
    pos: usize,
    /// Running maximum of the last `window + 1` input peaks (a "monotonic queue").
    q_val: Vec<f32>,
    q_idx: Vec<u64>,
    q_head: usize,
    q_len: usize,
    n: u64,
    gain: f32,
    sr: f32,
}

impl Limiter {
    pub fn new() -> Self {
        Self {
            window: 1,
            delay: [vec![0.0; LA_CAP], vec![0.0; LA_CAP]],
            pos: 0,
            q_val: vec![0.0; LA_CAP + 1],
            q_idx: vec![0; LA_CAP + 1],
            q_head: 0,
            q_len: 0,
            n: 0,
            gain: 1.0,
            sr: 0.0,
        }
    }

    /// Add a peak and return the largest one within the window.
    fn window_max(&mut self, v: f32) -> f32 {
        let cap = self.q_val.len();
        // Drop smaller values from the back: they can never be the maximum again.
        while self.q_len > 0 {
            let back = (self.q_head + self.q_len - 1) % cap;
            if self.q_val[back] > v {
                break;
            }
            self.q_len -= 1;
        }
        let at = (self.q_head + self.q_len) % cap;
        self.q_val[at] = v;
        self.q_idx[at] = self.n;
        self.q_len += 1;
        // Drop values that have left the window from the front.
        while self.q_idx[self.q_head] + (self.window as u64) < self.n {
            self.q_head = (self.q_head + 1) % cap;
            self.q_len -= 1;
        }
        self.n += 1;
        self.q_val[self.q_head]
    }
}

impl Effect for Limiter {
    fn process(&mut self, p: &Params, ctx: &Ctx, chans: &mut [&mut [f32]]) -> f32 {
        if self.sr != ctx.sr {
            self.sr = ctx.sr;
            self.window = ((LOOKAHEAD_SEC * ctx.sr).round() as usize).clamp(1, LA_CAP - 1);
            self.reset();
        }
        let input = db_to_lin(p[L_GAIN]);
        let ceiling = db_to_lin(p[L_CEILING]);
        // Reach (nearly) the needed gain within the lookahead time.
        let att = (-3.0 / self.window as f32).exp();
        let rel = coef(p[L_RELEASE], ctx.sr);
        let n_ch = chans.len().min(2);
        let mut most = 0.0f32;
        for i in 0..chans[0].len() {
            let mut peak = 0.0f32;
            for ch in 0..n_ch {
                let x = chans[ch][i] * input;
                peak = peak.max(x.abs());
                chans[ch][i] = x;
            }
            let loudest = self.window_max(peak);
            let need = if loudest > ceiling { ceiling / loudest } else { 1.0 };
            let k = if need < self.gain { att } else { rel };
            self.gain = need + (self.gain - need) * k;
            most = most.max(-lin_to_db(self.gain));

            let read = (self.pos + LA_CAP - self.window) % LA_CAP;
            for ch in 0..n_ch {
                self.delay[ch][self.pos] = chans[ch][i];
                let y = self.delay[ch][read] * self.gain;
                chans[ch][i] = y.clamp(-ceiling, ceiling);
            }
            self.pos = (self.pos + 1) % LA_CAP;
        }
        most
    }

    fn reset(&mut self) {
        for d in &mut self.delay {
            d.fill(0.0);
        }
        self.q_head = 0;
        self.q_len = 0;
        self.n = 0;
        self.gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::EffectKind;

    const CTX: Ctx = Ctx { sr: 48_000.0, tempo: 120.0 };

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (std::f32::consts::TAU * freq * i as f32 / CTX.sr).sin()).collect()
    }

    fn peak_db(x: &[f32]) -> f32 {
        lin_to_db(x.iter().fold(0.0f32, |m, s| m.max(s.abs())))
    }

    #[test]
    fn compressor_curve() {
        assert_eq!(comp_reduction(-30.0, -20.0, 4.0, 0.0), 0.0);
        assert!((comp_reduction(-8.0, -20.0, 4.0, 0.0) - 9.0).abs() < 1e-4);
        // Soft knee: halfway in at the threshold.
        assert!(comp_reduction(-20.0, -20.0, 4.0, 6.0) > 0.0);
    }

    #[test]
    fn compressor_turns_a_loud_tone_down() {
        let mut c = Compressor::default();
        let mut p = EffectKind::Compressor.defaults();
        p[C_THRESH] = -20.0;
        p[C_RATIO] = 4.0;
        p[C_KNEE] = 0.0;
        let mut x = sine(1000.0, 1.0, 48_000); // 0 dBFS peaks: 20 dB over
        let gr = c.process(&p, &CTX, &mut [&mut x]);
        assert!((13.5..=15.2).contains(&gr), "{gr}");
        let out = peak_db(&x[40_000..]);
        assert!((-16.0..=-13.5).contains(&out), "{out}");
    }

    #[test]
    fn gate_silences_quiet_noise() {
        let mut g = Gate::default();
        let p = EffectKind::Gate.defaults(); // -50 dB threshold, 60 dB range
        let mut x = sine(200.0, 0.001, 48_000); // -60 dB
        g.process(&p, &CTX, &mut [&mut x]);
        assert!(peak_db(&x[24_000..]) < -110.0);
        let mut loud = sine(200.0, 0.5, 4800);
        g.process(&p, &CTX, &mut [&mut loud]);
        assert!(peak_db(&loud[2400..]) > -6.5);
    }

    #[test]
    fn de_esser_only_touches_the_highs() {
        let p = EffectKind::DeEsser.defaults();
        let mut low = sine(300.0, 0.9, 24_000);
        let orig = low.clone();
        DeEsser::default().process(&p, &CTX, &mut [&mut low]);
        let diff = low.iter().zip(&orig).fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(diff < 0.05, "{diff}");
        let mut ess = sine(8000.0, 0.5, 24_000);
        let gr = DeEsser::default().process(&p, &CTX, &mut [&mut ess]);
        assert!(gr > 5.0, "{gr}");
    }

    #[test]
    fn limiter_holds_the_ceiling() {
        let mut l = Limiter::new();
        let mut p = EffectKind::Limiter.defaults();
        p[L_GAIN] = 12.0;
        p[L_CEILING] = -1.0;
        let mut a = sine(100.0, 0.9, 48_000);
        let mut b = sine(3000.0, 0.9, 48_000);
        l.process(&p, &CTX, &mut [&mut a, &mut b]);
        assert!(peak_db(&a) <= -1.0 + 1e-3 && peak_db(&b) <= -1.0 + 1e-3);
        // Not just clipped: the level stays close to the ceiling.
        assert!(peak_db(&a[24_000..]) > -2.0);
    }
}
