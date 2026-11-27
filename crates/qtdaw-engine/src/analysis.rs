//! Measuring audio: the numbers Claude (and later the app's meters) uses to judge a mix.
//!
//! - [`Loudness`]: loudness in LUFS the way streaming services measure it (ITU-R BS.1770 /
//!   EBU R128), loudness range, sample peak, true peak and stereo correlation.
//! - [`Spectrum`]: how the energy spreads over frequency, in 1/3-octave bands, both moment by
//!   moment (to find tracks fighting over the same frequencies) and on average.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Centre frequencies of the standard 1/3-octave bands.
pub const BANDS: [f32; 30] = [
    25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0,
    1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0, 10_000.0, 12_500.0,
    16_000.0, 20_000.0,
];
pub const NB: usize = BANDS.len();

/// Plain-language frequency regions, as ranges of [`BANDS`] indexes.
pub const REGIONS: [(&str, std::ops::Range<usize>); 7] = [
    ("sub (20-60 Hz)", 0..4),
    ("bass (60-250 Hz)", 4..10),
    ("low mids (250-500 Hz)", 10..13),
    ("mids (500 Hz-2 kHz)", 13..19),
    ("upper mids (2-4 kHz)", 19..22),
    ("presence (4-6 kHz)", 22..24),
    ("air (6-20 kHz)", 24..30),
];

/// "1 kHz" / "250 Hz" style label for a band.
pub fn band_label(i: usize) -> String {
    let f = BANDS[i];
    if f >= 1000.0 {
        let k = f / 1000.0;
        if k.fract() == 0.0 { format!("{k:.0} kHz") } else { format!("{k} kHz") }
    } else {
        format!("{f} Hz")
    }
}

/// Power (mean square) to decibels; silence is -120 dB rather than minus infinity.
pub fn power_db(p: f64) -> f32 {
    (10.0 * p.max(1e-12).log10()) as f32
}

pub fn amp_db(a: f32) -> f32 {
    20.0 * a.max(1e-6).log10()
}

// ------------------------------------------------------------------------------- loudness

/// A biquad in double precision (the loudness filters sit very low relative to the sample
/// rate, where single precision gets noisy).
#[derive(Clone, Copy)]
struct Biquad64 {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad64 {
    #[inline]
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// The BS.1770 "K-weighting": a high shelf (the head boosts highs) and a high-pass (we
/// barely hear deep bass as loud). Same formulas as libebur128, valid at any sample rate.
fn k_weighting(sr: f64) -> [Biquad64; 2] {
    let (f0, g, q) = (1681.974_450_955_533, 3.999_843_853_973_347, 0.707_175_236_955_419_6);
    let k = (std::f64::consts::PI * f0 / sr).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad64 {
        b: [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        z: [0.0; 2],
    };
    let (f0, q) = (38.135_470_876_024_44, 0.500_327_037_323_877_3);
    let k = (std::f64::consts::PI * f0 / sr).tan();
    let a0 = 1.0 + k / q + k * k;
    let hp = Biquad64 { b: [1.0, -2.0, 1.0], a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0], z: [0.0; 2] };
    [shelf, hp]
}

/// 4x oversampling peak detector: catches peaks *between* samples, which is what clips a
/// converter or an MP3/AAC encoder even when no sample reaches 0 dBFS.
struct TruePeak {
    /// 4 phases x 12 taps.
    taps: [[f32; 12]; 4],
    hist: [f32; 12],
    pos: usize,
    peak: f32,
}

impl TruePeak {
    fn new() -> Self {
        let mut taps = [[0.0f32; 12]; 4];
        let len = 48.0;
        for (p, phase) in taps.iter_mut().enumerate() {
            for (k, t) in phase.iter_mut().enumerate() {
                let n = (k * 4 + p) as f64;
                let x = (n - (len - 1.0) / 2.0) / 4.0;
                let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
                let w = 0.5 - 0.5 * (std::f64::consts::TAU * (n + 0.5) / len).cos();
                *t = (sinc * w) as f32;
            }
            let sum: f32 = phase.iter().sum();
            for t in phase.iter_mut() {
                *t /= sum;
            }
        }
        Self { taps, hist: [0.0; 12], pos: 0, peak: 0.0 }
    }

    #[inline]
    fn push(&mut self, x: f32) {
        self.hist[self.pos] = x;
        self.pos = (self.pos + 1) % 12;
        for phase in &self.taps {
            let mut y = 0.0;
            for (k, t) in phase.iter().enumerate() {
                // hist[pos - 1 - k] = the sample k steps ago.
                y += t * self.hist[(self.pos + 11 - k) % 12];
            }
            self.peak = self.peak.max(y.abs());
        }
    }
}

/// Loudness and level measurements of a stereo signal fed block by block.
pub struct Loudness {
    filters: [[Biquad64; 2]; 2],
    /// 100 ms of samples.
    step: usize,
    step_acc: f64,
    step_count: usize,
    /// K-weighted energy (summed over both channels) of each 100 ms step.
    steps: Vec<f64>,
    sample_peak: f32,
    true_peak: Option<[TruePeak; 2]>,
    sum_sq: f64,
    sum_lr: f64,
    sum_l2: f64,
    sum_r2: f64,
    frames: u64,
    clipped: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LoudnessResult {
    /// Loudness of the whole range, ignoring silence (`None` if it's all silence).
    pub integrated_lufs: Option<f32>,
    /// Loudest 3-second stretch.
    pub short_term_max_lufs: Option<f32>,
    /// How much the loudness moves between quiet and loud parts (EBU R128 LRA), in LU.
    pub range_lu: Option<f32>,
    pub sample_peak_dbfs: f32,
    pub true_peak_dbtp: Option<f32>,
    /// Plain average level over the range, silence included.
    pub rms_dbfs: f32,
    /// Share of the range where it's actually playing (louder than -50 LUFS).
    pub active_pct: f32,
    /// +1 = mono, 0 = unrelated left/right, below 0 = phase problems (cancels in mono).
    pub correlation: Option<f32>,
    /// Samples beyond full scale (they will clip when they leave the computer).
    pub clipped_samples: u64,
}

impl Loudness {
    pub fn new(sample_rate: f32, true_peak: bool) -> Self {
        let k = k_weighting(sample_rate as f64);
        Self {
            filters: [k, k],
            step: (sample_rate as usize / 10).max(1),
            step_acc: 0.0,
            step_count: 0,
            steps: Vec::new(),
            sample_peak: 0.0,
            true_peak: true_peak.then(|| [TruePeak::new(), TruePeak::new()]),
            sum_sq: 0.0,
            sum_lr: 0.0,
            sum_l2: 0.0,
            sum_r2: 0.0,
            frames: 0,
            clipped: 0,
        }
    }

    pub fn push(&mut self, l: &[f32], r: &[f32]) {
        for (&a, &b) in l.iter().zip(r) {
            let [[sa, ha], [sb, hb]] = &mut self.filters;
            let ka = ha.run(sa.run(a as f64));
            let kb = hb.run(sb.run(b as f64));
            self.step_acc += ka * ka + kb * kb;
            self.step_count += 1;
            if self.step_count == self.step {
                self.steps.push(self.step_acc / self.step as f64);
                self.step_acc = 0.0;
                self.step_count = 0;
            }
            let (a64, b64) = (a as f64, b as f64);
            self.sum_l2 += a64 * a64;
            self.sum_r2 += b64 * b64;
            self.sum_lr += a64 * b64;
            self.sum_sq += 0.5 * (a64 * a64 + b64 * b64);
            let m = a.abs().max(b.abs());
            self.sample_peak = self.sample_peak.max(m);
            if m > 1.0 {
                self.clipped += 1;
            }
            if let Some(tp) = &mut self.true_peak {
                tp[0].push(a);
                tp[1].push(b);
            }
        }
        self.frames += l.len().min(r.len()) as u64;
    }

    /// Energy of each window of `len` 100 ms steps, advancing one step at a time.
    fn windows(&self, len: usize) -> Vec<f64> {
        if self.steps.len() < len {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.steps.len() - len + 1);
        let mut acc: f64 = self.steps[..len].iter().sum();
        out.push(acc / len as f64);
        for i in len..self.steps.len() {
            acc += self.steps[i] - self.steps[i - len];
            out.push(acc.max(0.0) / len as f64);
        }
        out
    }

    pub fn finish(&self) -> LoudnessResult {
        let lufs = |z: f64| -0.691 + power_db(z);
        // Integrated: 400 ms blocks, drop silence (-70 LUFS) and then anything 10 LU below
        // the average of what's left, so quiet intros and pauses don't drag the number down.
        let blocks = self.windows(4);
        let loud: Vec<f64> = blocks.iter().copied().filter(|&z| lufs(z) > -70.0).collect();
        let integrated = (!loud.is_empty()).then(|| {
            let rel = lufs(loud.iter().sum::<f64>() / loud.len() as f64) - 10.0;
            let kept: Vec<f64> = loud.iter().copied().filter(|&z| lufs(z) > rel).collect();
            lufs(kept.iter().sum::<f64>() / kept.len().max(1) as f64)
        });
        let active = blocks.iter().filter(|&&z| lufs(z) > -50.0).count();

        // Short-term: 3 s windows. Loudness range = spread between the 10th and 95th
        // percentile of those (after dropping silence and anything 20 LU below average).
        let short = self.windows(30);
        let short_max = short.iter().copied().fold(None, |m: Option<f64>, z| Some(m.map_or(z, |m| m.max(z))));
        let mut s: Vec<f32> = short.iter().map(|&z| lufs(z)).filter(|&l| l > -70.0).collect();
        let range = (!s.is_empty()).then(|| {
            let e: Vec<f64> = short.iter().copied().filter(|&z| lufs(z) > -70.0).collect();
            let rel = lufs(e.iter().sum::<f64>() / e.len() as f64) - 20.0;
            s.retain(|&l| l > rel);
            s.sort_by(f32::total_cmp);
            let pick = |q: f32| s[((s.len() - 1) as f32 * q).round() as usize];
            if s.is_empty() { 0.0 } else { pick(0.95) - pick(0.10) }
        });

        let denom = (self.sum_l2 * self.sum_r2).sqrt();
        LoudnessResult {
            integrated_lufs: integrated,
            short_term_max_lufs: short_max.map(lufs),
            range_lu: range,
            sample_peak_dbfs: amp_db(self.sample_peak),
            true_peak_dbtp: self.true_peak.as_ref().map(|tp| amp_db(tp[0].peak.max(tp[1].peak).max(self.sample_peak))),
            rms_dbfs: power_db(self.sum_sq / self.frames.max(1) as f64),
            active_pct: if blocks.is_empty() { 0.0 } else { 100.0 * active as f32 / blocks.len() as f32 },
            correlation: (denom > 1e-9).then(|| (self.sum_lr / denom) as f32),
            clipped_samples: self.clipped,
        }
    }
}

// ------------------------------------------------------------------------------- spectrum

/// FFT size: 8192 samples (~170 ms at 48 kHz) resolves the low bands; frames overlap by half.
const FFT_LEN: usize = 8192;
const HOP: usize = FFT_LEN / 2;

/// Band levels of a mono signal over time.
pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Bin range of each band.
    bins: Vec<(usize, usize)>,
    /// Scales summed |X|^2 to mean-square level.
    norm: f64,
    pending: Vec<f32>,
    scratch: Vec<Complex<f32>>,
    /// Power (mean square) per band, one row per hop (~85 ms at 48 kHz).
    pub frames: Vec<[f32; NB]>,
}

impl Spectrum {
    pub fn new(sample_rate: f32) -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_LEN);
        let window: Vec<f32> = (0..FFT_LEN)
            .map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / FFT_LEN as f64).cos()) as f32)
            .collect();
        let w2: f64 = window.iter().map(|w| (*w as f64).powi(2)).sum();
        let bin_of = |f: f32| ((f / sample_rate * FFT_LEN as f32).ceil() as usize).clamp(1, FFT_LEN / 2);
        let edge = 2f32.powf(1.0 / 6.0);
        let bins = BANDS.iter().map(|&c| (bin_of(c / edge), bin_of(c * edge))).collect();
        Self {
            fft,
            window,
            bins,
            norm: 2.0 / (FFT_LEN as f64 * w2),
            pending: Vec::with_capacity(FFT_LEN),
            scratch: vec![Complex::default(); FFT_LEN],
            frames: Vec::new(),
        }
    }

    pub fn push(&mut self, mono: impl IntoIterator<Item = f32>) {
        for x in mono {
            self.pending.push(x);
            if self.pending.len() == FFT_LEN {
                self.frame();
                self.pending.drain(..HOP);
            }
        }
    }

    fn frame(&mut self) {
        for ((c, x), w) in self.scratch.iter_mut().zip(&self.pending).zip(&self.window) {
            *c = Complex::new(x * w, 0.0);
        }
        self.fft.process(&mut self.scratch);
        let mut row = [0.0f32; NB];
        for (v, &(a, b)) in row.iter_mut().zip(&self.bins) {
            let sum: f64 = self.scratch[a..b].iter().map(|c| c.norm_sqr() as f64).sum();
            *v = (sum * self.norm) as f32;
        }
        self.frames.push(row);
    }
}

/// Total power of a spectrum row.
pub fn row_total(row: &[f32; NB]) -> f32 {
    row.iter().sum()
}

/// Frames quieter than this (-60 dB) count as "not playing".
pub const SILENT: f32 = 1e-6;

/// Average band powers over the frames where something is playing.
pub fn average(frames: &[[f32; NB]]) -> Option<[f32; NB]> {
    let mut sum = [0.0f64; NB];
    let mut n = 0;
    for row in frames.iter().filter(|r| row_total(r) > SILENT) {
        for (s, v) in sum.iter_mut().zip(row) {
            *s += *v as f64;
        }
        n += 1;
    }
    (n > 0).then(|| sum.map(|s| (s / n as f64) as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, amp: f32, sr: f32, secs: f32) -> Vec<f32> {
        (0..(sr * secs) as usize).map(|i| amp * (std::f32::consts::TAU * freq * i as f32 / sr).sin()).collect()
    }

    #[test]
    fn a_1k_sine_at_minus_20_reads_minus_20_lufs() {
        // BS.1770: a 1 kHz sine in both channels reads its level in dBFS as LUFS.
        for sr in [44_100.0, 48_000.0, 96_000.0] {
            let s = sine(997.0, 0.1, sr, 5.0);
            let mut m = Loudness::new(sr, true);
            m.push(&s, &s);
            let r = m.finish();
            let i = r.integrated_lufs.unwrap();
            assert!((i + 20.0).abs() < 0.1, "{sr}: {i}");
            assert!((r.sample_peak_dbfs + 20.0).abs() < 0.05);
            assert!((r.true_peak_dbtp.unwrap() + 20.0).abs() < 0.2);
            assert!(r.range_lu.unwrap() < 0.5);
            assert!((r.correlation.unwrap() - 1.0).abs() < 1e-4);
            assert!(r.active_pct > 99.0);
        }
    }

    #[test]
    fn silence_is_ignored_by_integrated_loudness() {
        let sr = 48_000.0;
        let mut s = sine(997.0, 0.1, sr, 4.0);
        s.extend(std::iter::repeat_n(0.0, (sr * 4.0) as usize));
        let mut m = Loudness::new(sr, false);
        m.push(&s, &s);
        let r = m.finish();
        assert!((r.integrated_lufs.unwrap() + 20.0).abs() < 0.2);
        assert!((r.active_pct - 50.0).abs() < 5.0, "{}", r.active_pct);
    }

    #[test]
    fn true_peak_sees_between_the_samples() {
        // A sine at a quarter of the sample rate, phased so every sample lands at 0.707 of
        // the real peak: sample peak reads -3 dB, true peak should read about 0 dB.
        let sr = 48_000.0;
        let s: Vec<f32> = (0..48_000).map(|i| (std::f32::consts::FRAC_PI_2 * i as f32 + std::f32::consts::FRAC_PI_4).sin()).collect();
        let mut m = Loudness::new(sr, true);
        m.push(&s, &s);
        let r = m.finish();
        assert!((r.sample_peak_dbfs + 3.0).abs() < 0.1, "{}", r.sample_peak_dbfs);
        assert!(r.true_peak_dbtp.unwrap() > -0.5, "{:?}", r.true_peak_dbtp);
    }

    #[test]
    fn spectrum_puts_a_sine_in_its_band() {
        let sr = 48_000.0;
        let mut sp = Spectrum::new(sr);
        sp.push(sine(1000.0, 1.0, sr, 2.0));
        let avg = average(&sp.frames).unwrap();
        let k = BANDS.iter().position(|&f| f == 1000.0).unwrap();
        let loudest = (0..NB).max_by(|&a, &b| avg[a].total_cmp(&avg[b])).unwrap();
        assert_eq!(loudest, k);
        // Mean square of a full-scale sine = 0.5 = -3 dB.
        assert!((power_db(avg[k] as f64) + 3.0).abs() < 0.5, "{}", power_db(avg[k] as f64));
        assert!(power_db(avg[k - 3] as f64) < -60.0);
        assert_eq!(band_label(k), "1 kHz");
        assert_eq!(band_label(21), "3.15 kHz");
    }
}
