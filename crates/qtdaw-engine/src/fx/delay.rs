//! Echo delay with tempo sync, filtered feedback and optional ping-pong.

use super::biquad::{Coeffs, State};
use super::{Ctx, Effect, ParamSpec, Params, Unit, coef, dry_wet};

const TIME: usize = 0;
const SYNC: usize = 1;
const FEEDBACK: usize = 2;
const LOW_CUT: usize = 3;
const HIGH_CUT: usize = 4;
const PING_PONG: usize = 5;
const MIX: usize = 6;

const SYNC_NAMES: &[&str] = &["Off (use Time)", "1/2", "1/4", "1/4 dotted", "1/8", "1/8 dotted", "1/8 triplet", "1/16"];
/// Length of each sync choice in beats.
const SYNC_BEATS: &[f32] = &[0.0, 2.0, 1.0, 1.5, 0.5, 0.75, 1.0 / 3.0, 0.25];

pub const PARAMS: &[ParamSpec] = &[
    ParamSpec::new("time", "Time", 1.0, 2000.0, 375.0, Unit::Ms, true),
    ParamSpec::new("sync", "Sync", 0.0, 7.0, 2.0, Unit::Choice(SYNC_NAMES), false),
    ParamSpec::new("feedback", "Feedback", 0.0, 95.0, 35.0, Unit::Percent, false),
    ParamSpec::new("low_cut", "Low Cut", 20.0, 2000.0, 150.0, Unit::Hz, true),
    ParamSpec::new("high_cut", "High Cut", 1000.0, 20_000.0, 6000.0, Unit::Hz, true),
    ParamSpec::new("ping_pong", "Ping-Pong", 0.0, 1.0, 0.0, Unit::Choice(&["Off", "On"]), false),
    ParamSpec::new("mix", "Mix", 0.0, 100.0, 30.0, Unit::Percent, false),
];

/// Longest echo, and the highest sample rate we size the buffers for.
const MAX_SEC: f32 = 2.5;
const MAX_SR: f32 = 192_000.0;

pub struct Delay {
    lines: [Vec<f32>; 2],
    pos: usize,
    /// Current delay in samples; glides toward the setting so changes don't click.
    current: f32,
    made_for: (f32, f32, f32),
    hp: Coeffs,
    lp: Coeffs,
    filt: [[State; 2]; 2],
}

impl Delay {
    pub fn new() -> Self {
        let len = (MAX_SEC * MAX_SR) as usize;
        Self {
            lines: [vec![0.0; len], vec![0.0; len]],
            pos: 0,
            current: 0.0,
            made_for: (0.0, 0.0, 0.0),
            hp: Coeffs::IDENTITY,
            lp: Coeffs::IDENTITY,
            filt: Default::default(),
        }
    }

    /// Read `delay` samples back from the write position, between samples if needed.
    #[inline]
    fn read(line: &[f32], pos: usize, delay: f32) -> f32 {
        let len = line.len();
        let back = delay.floor();
        let frac = delay - back;
        let i0 = (pos + len - back as usize) % len;
        let i1 = (i0 + len - 1) % len;
        line[i0] * (1.0 - frac) + line[i1] * frac
    }
}

impl Effect for Delay {
    fn process(&mut self, p: &Params, ctx: &Ctx, chans: &mut [&mut [f32]]) -> f32 {
        if self.made_for != (p[LOW_CUT], p[HIGH_CUT], ctx.sr) {
            self.hp = Coeffs::highpass(p[LOW_CUT], 0.707, ctx.sr);
            self.lp = Coeffs::lowpass(p[HIGH_CUT], 0.707, ctx.sr);
            self.made_for = (p[LOW_CUT], p[HIGH_CUT], ctx.sr);
        }
        let beats = SYNC_BEATS.get(p[SYNC] as usize).copied().unwrap_or(0.0);
        let secs = if beats > 0.0 { beats * 60.0 / ctx.tempo.clamp(20.0, 400.0) } else { p[TIME] / 1000.0 };
        let len = self.lines[0].len();
        let target = (secs * ctx.sr).clamp(1.0, (len - 2) as f32);
        if self.current < 1.0 {
            self.current = target;
        }
        let glide = coef(60.0, ctx.sr);
        let fb = p[FEEDBACK] / 100.0;
        let (dry, wet) = dry_wet(p[MIX] / 100.0);
        let stereo = chans.len() >= 2;
        let ping_pong = stereo && p[PING_PONG] >= 0.5;

        for i in 0..chans[0].len() {
            self.current = target + (self.current - target) * glide;
            let mut echo = [0.0f32; 2];
            for ch in 0..if stereo { 2 } else { 1 } {
                let e = Self::read(&self.lines[ch], self.pos, self.current);
                let e = self.filt[ch][0].run(&self.hp, e);
                echo[ch] = self.filt[ch][1].run(&self.lp, e);
            }
            if ping_pong {
                let (l, r) = (chans[0][i], chans[1][i]);
                // Input goes into the left line; each echo bounces to the other side.
                self.lines[0][self.pos] = 0.5 * (l + r) + fb * echo[1];
                self.lines[1][self.pos] = fb * echo[0];
                chans[0][i] = l * dry + echo[0] * wet;
                chans[1][i] = r * dry + echo[1] * wet;
            } else {
                for ch in 0..if stereo { 2 } else { 1 } {
                    let x = chans[ch][i];
                    self.lines[ch][self.pos] = x + fb * echo[ch];
                    chans[ch][i] = x * dry + echo[ch] * wet;
                }
            }
            self.pos = (self.pos + 1) % len;
        }
        0.0
    }

    fn reset(&mut self) {
        for l in &mut self.lines {
            l.fill(0.0);
        }
        self.filt = Default::default();
        self.current = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::EffectKind;

    #[test]
    fn echo_arrives_on_the_beat() {
        let ctx = Ctx { sr: 48_000.0, tempo: 120.0 };
        let mut d = Delay::new();
        let mut p = EffectKind::Delay.defaults(); // 1/4 note = 0.5 s at 120 BPM
        p[MIX] = 100.0;
        p[LOW_CUT] = 20.0;
        p[HIGH_CUT] = 20_000.0;
        let mut x = vec![0.0f32; 48_000];
        x[0] = 1.0;
        d.process(&p, &ctx, &mut [&mut x]);
        let loudest = x.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
        assert!((loudest as i64 - 24_000).abs() <= 2, "{loudest}");
    }
}
