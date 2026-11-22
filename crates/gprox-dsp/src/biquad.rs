//! Biquad taps and the RBJ coefficient design used by G HUB's headphone EQ.
//!
//! The design runs entirely in `f64` and the
//! five normalised coefficients are rounded to `f32` once at the end. The
//! filter runs on those `f32` taps (see [`crate::kernel`]).

use std::f64::consts::TAU;

/// `ln(2) / 2`, the octave-bandwidth factor in the digital-bandwidth form of
/// alpha. Written as the decimal G HUB stores.
const LN2_HALF: f64 = 0.346_573_590_279_972_64;

/// Bands whose centre frequency is above `fs / NYQUIST_GUARD` are rejected
/// and keep their previous taps.
pub const NYQUIST_GUARD: f64 = 2.2;

/// Normalised biquad taps (`a0 == 1`), in `f32` as the filter engine uses them.
///
/// Transfer function: `H(z) = (b0 + b1 z⁻¹ + b2 z⁻²) / (1 + a1 z⁻¹ + a2 z⁻²)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Taps {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Taps {
    /// Pass-through section.
    pub const IDENTITY: Taps = Taps {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    /// The six-tap layout of the hooked tap pushes: `b0, b1, b2, a0 (= 1), a1, a2`.
    pub fn to_six(self) -> [f32; 6] {
        [self.b0, self.b1, self.b2, 1.0, self.a1, self.a2]
    }

    /// Inverse of [`Taps::to_six`]. `a0` is ignored (always 1 in G HUB).
    pub fn from_six(t: [f32; 6]) -> Self {
        Taps {
            b0: t[0],
            b1: t[1],
            b2: t[2],
            a1: t[4],
            a2: t[5],
        }
    }

    /// Bitwise equality (distinguishes `0.0`/`-0.0`, which `==` does not).
    pub fn bit_eq(&self, other: &Taps) -> bool {
        self.to_six()
            .iter()
            .zip(other.to_six().iter())
            .all(|(a, b)| a.to_bits() == b.to_bits())
    }

    /// Magnitude response in dB at `f` Hz (f64 evaluation of the f32 taps).
    pub fn magnitude_db(&self, f: f64, fs: f64) -> f64 {
        let w = TAU * f / fs;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let t = |v: f32| f64::from(v);
        let (nr, ni) = (
            t(self.b0) + t(self.b1) * c1 + t(self.b2) * c2,
            -(t(self.b1) * s1 + t(self.b2) * s2),
        );
        let (dr, di) = (
            1.0 + t(self.a1) * c1 + t(self.a2) * c2,
            -(t(self.a1) * s1 + t(self.a2) * s2),
        );
        10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di)).log10()
    }
}

/// Section shape. The engine's type codes are 0 / 1 / 2 in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Peaking,
    LowShelf,
    HighShelf,
}

/// RBJ cookbook biquad with alpha taken from an octave bandwidth.
///
/// * `f0`, `bw_oct`, `gain_db` and `fs` are the `f64` values G HUB computes
///   with (callers promote the `f32` table values; the sample rate is the
///   stream's integer rate).
/// * `A = 10^(gain/40)`, `w0 = 2π f0 / fs`,
///   `alpha = sin w0 · sinh(ln2/2 · BW · w0 / sin w0)`.
/// * Peaking uses `α·A` / `α/A`; both shelves use the `2·√A·α` term with that
///   same alpha (no shelf slope parameter), which makes them resonant.
/// * All coefficients are divided by `a0` in `f64`, then rounded to `f32`.
///
/// Returns `None` when `f0 > fs / 2.2`; the caller then keeps the band's
/// previous taps.
pub fn design_rbj_bw(shape: Shape, f0: f64, gain_db: f64, bw_oct: f64, fs: f64) -> Option<Taps> {
    if f0 > fs / NYQUIST_GUARD {
        return None;
    }
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = TAU * f0 / fs;
    let s = w0.sin();
    let c = w0.cos();
    let alpha = s * (LN2_HALF * bw_oct * w0 / s).sinh();

    let (b0, b1, b2, a0, a1, a2) = match shape {
        Shape::Peaking => (
            1.0 + alpha * a,
            -2.0 * c,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * c,
            1.0 - alpha / a,
        ),
        Shape::LowShelf => {
            let k = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) - (a - 1.0) * c + k),
                2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                a * ((a + 1.0) - (a - 1.0) * c - k),
                (a + 1.0) + (a - 1.0) * c + k,
                -2.0 * ((a - 1.0) + (a + 1.0) * c),
                (a + 1.0) + (a - 1.0) * c - k,
            )
        }
        Shape::HighShelf => {
            let k = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) + (a - 1.0) * c + k),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                a * ((a + 1.0) + (a - 1.0) * c - k),
                (a + 1.0) - (a - 1.0) * c + k,
                2.0 * ((a - 1.0) - (a + 1.0) * c),
                (a + 1.0) - (a - 1.0) * c - k,
            )
        }
    };
    Some(Taps {
        b0: (b0 / a0) as f32,
        b1: (b1 / a0) as f32,
        b2: (b2 / a0) as f32,
        a1: (a1 / a0) as f32,
        a2: (a2 / a0) as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48_000.0;

    #[test]
    fn peaking_hits_gain_at_f0() {
        for g in [-12.0, -6.0, -0.6, 0.6, 6.0, 12.0] {
            let t = design_rbj_bw(Shape::Peaking, 1000.0, g, f64::from(0.7f32), FS).unwrap();
            assert!((t.magnitude_db(1000.0, FS) - g).abs() < 1e-3, "g={g}");
            assert!(t.magnitude_db(20.0, FS).abs() < 0.01);
        }
    }

    #[test]
    fn shelves_reach_gain_at_the_far_end() {
        let ls = design_rbj_bw(Shape::LowShelf, 200.0, 6.0, 1.0, FS).unwrap();
        assert!((ls.magnitude_db(1.0, FS) - 6.0).abs() < 1e-3);
        assert!(ls.magnitude_db(20_000.0, FS).abs() < 0.01);
        let hs = design_rbj_bw(Shape::HighShelf, 4500.0, -6.0, 1.0, FS).unwrap();
        assert!((hs.magnitude_db(23_999.0, FS) + 6.0).abs() < 1e-3);
        assert!(hs.magnitude_db(20.0, FS).abs() < 0.01);
    }

    #[test]
    fn zero_gain_cancels_up_to_f32_rounding() {
        let t = design_rbj_bw(Shape::Peaking, 32.0, 0.0, f64::from(0.7f32), FS).unwrap();
        assert_eq!(t.b0, 1.0);
        assert_eq!(t.b1, t.a1);
        assert_eq!(t.b2, t.a2);
    }

    #[test]
    fn rejects_above_fs_over_2_2() {
        let f0 = f64::from(11_313.7f32);
        assert!(design_rbj_bw(Shape::HighShelf, f0, 6.0, 0.7, 44_100.0).is_some());
        assert!(design_rbj_bw(Shape::HighShelf, f0, 6.0, 0.7, 22_050.0).is_none());
    }
}
