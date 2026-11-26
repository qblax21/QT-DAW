//! Biquad filters: the building block of EQs, crossovers and filtered feedback loops.
//!
//! Formulas from Robert Bristow-Johnson's "Audio EQ Cookbook". Each filter is five numbers
//! ([`Coeffs`]) plus two numbers of memory per channel ([`State`]).

use std::f32::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Default for Coeffs {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Keep filter frequencies a safe distance below Nyquist (half the sample rate).
fn omega(freq: f32, sr: f32) -> (f32, f32) {
    let w = 2.0 * PI * freq.clamp(5.0, sr * 0.49) / sr;
    (w.cos(), w.sin())
}

impl Coeffs {
    /// Passes everything through unchanged.
    pub const IDENTITY: Coeffs = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    fn normalized(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    pub fn lowpass(freq: f32, q: f32, sr: f32) -> Self {
        let (cos, sin) = omega(freq, sr);
        let alpha = sin / (2.0 * q);
        let b1 = 1.0 - cos;
        Self::normalized(b1 / 2.0, b1, b1 / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    pub fn highpass(freq: f32, q: f32, sr: f32) -> Self {
        let (cos, sin) = omega(freq, sr);
        let alpha = sin / (2.0 * q);
        let b1 = -(1.0 + cos);
        Self::normalized(-b1 / 2.0, b1, -b1 / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    /// A bell boost or cut of `gain_db` around `freq`. Higher `q` = narrower.
    pub fn peak(freq: f32, q: f32, gain_db: f32, sr: f32) -> Self {
        let (cos, sin) = omega(freq, sr);
        let a = 10f32.powf(gain_db / 40.0);
        let alpha = sin / (2.0 * q);
        Self::normalized(1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a)
    }

    /// Boost or cut everything below `freq`.
    pub fn low_shelf(freq: f32, gain_db: f32, sr: f32) -> Self {
        let (cos, sin) = omega(freq, sr);
        let a = 10f32.powf(gain_db / 40.0);
        let alpha = sin / 2.0 * 2f32.sqrt(); // shelf slope 1
        let k = 2.0 * a.sqrt() * alpha;
        Self::normalized(
            a * ((a + 1.0) - (a - 1.0) * cos + k),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - k),
            (a + 1.0) + (a - 1.0) * cos + k,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - k,
        )
    }

    /// Boost or cut everything above `freq`.
    pub fn high_shelf(freq: f32, gain_db: f32, sr: f32) -> Self {
        let (cos, sin) = omega(freq, sr);
        let a = 10f32.powf(gain_db / 40.0);
        let alpha = sin / 2.0 * 2f32.sqrt();
        let k = 2.0 * a.sqrt() * alpha;
        Self::normalized(
            a * ((a + 1.0) + (a - 1.0) * cos + k),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - k),
            (a + 1.0) - (a - 1.0) * cos + k,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - k,
        )
    }

    /// How much this filter boosts (+) or cuts (-) a sine at `freq`, in dB.
    pub fn magnitude_db(&self, freq: f32, sr: f32) -> f32 {
        let w = 2.0 * PI * freq / sr;
        let (c1, s1) = (w.cos(), w.sin());
        let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di).max(1e-30)).max(1e-30).log10()
    }
}

/// One channel's filter memory.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    z1: f32,
    z2: f32,
}

impl State {
    /// Filter one sample (transposed direct form II, which copes well with coefficients
    /// changing while audio runs).
    #[inline]
    pub fn run(&mut self, c: &Coeffs, x: f32) -> f32 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnitudes_match_the_design() {
        let sr = 48_000.0;
        let pk = Coeffs::peak(1000.0, 1.0, 6.0, sr);
        assert!((pk.magnitude_db(1000.0, sr) - 6.0).abs() < 0.05);
        assert!(pk.magnitude_db(50.0, sr).abs() < 0.2);
        let hp = Coeffs::highpass(100.0, std::f32::consts::FRAC_1_SQRT_2, sr);
        assert!((hp.magnitude_db(100.0, sr) + 3.01).abs() < 0.1);
        assert!(hp.magnitude_db(10_000.0, sr).abs() < 0.05);
        let ls = Coeffs::low_shelf(200.0, -6.0, sr);
        assert!((ls.magnitude_db(20.0, sr) + 6.0).abs() < 0.2);
        assert!(ls.magnitude_db(10_000.0, sr).abs() < 0.1);
    }

    #[test]
    fn running_a_filter_matches_its_magnitude() {
        let sr = 48_000.0;
        let c = Coeffs::peak(1000.0, 2.0, -12.0, sr);
        let mut st = State::default();
        let mut peak = 0.0f32;
        for i in 0..48_000 {
            let x = (2.0 * PI * 1000.0 * i as f32 / sr).sin();
            let y = st.run(&c, x);
            if i > 24_000 {
                peak = peak.max(y.abs());
            }
        }
        assert!((20.0 * peak.log10() + 12.0).abs() < 0.2, "{peak}");
    }
}
