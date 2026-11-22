//! RBJ cookbook biquad designs used by the Blue VO!CE capture chain, and the
//! two section kernels it runs them with.
//!
//! Voice EQ: peaking with width = Q and shelves with width = slope S, f32 TDF2
//! kernel. De-popper and de-esser: 2nd-order Butterworth HP/LP, DF2 kernel
//! with `f64` arithmetic and `f32` state. Designs run in `f64` and round the normalised taps to `f32`.
//! The headphone EQ's bandwidth-based variant lives in [`crate::biquad`].

use std::f64::consts::{SQRT_2, TAU};

use crate::biquad::Taps;

/// Denormal guard applied to section state at the end of a kernel call.
pub const FLUSH: f64 = 1e-15;

fn norm(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Taps {
    Taps {
        b0: (b0 / a0) as f32,
        b1: (b1 / a0) as f32,
        b2: (b2 / a0) as f32,
        a1: (a1 / a0) as f32,
        a2: (a2 / a0) as f32,
    }
}

/// 2nd-order Butterworth high-pass (RBJ HPF, Q = 1/√2) at `f` Hz.
pub fn highpass_butter2(f: f32, fs: u32) -> Taps {
    let w0 = f64::from(f) * TAU / f64::from(fs);
    let alpha = w0.sin() / SQRT_2;
    let c = w0.cos();
    let a0 = 1.0 + alpha;
    norm(
        (1.0 + c) * 0.5,
        -(1.0 + c),
        (1.0 + c) * 0.5,
        a0,
        c * -2.0,
        1.0 - alpha,
    )
}

/// 2nd-order Butterworth low-pass (RBJ LPF, Q = 1/√2) at `f` Hz.
pub fn lowpass_butter2(f: f32, fs: u32) -> Taps {
    let w0 = f64::from(f) * TAU / f64::from(fs);
    let alpha = w0.sin() / SQRT_2;
    let c = w0.cos();
    let a0 = 1.0 + alpha;
    norm(
        (1.0 - c) * 0.5,
        1.0 - c,
        (1.0 - c) * 0.5,
        a0,
        c * -2.0,
        1.0 - alpha,
    )
}

/// Voice-EQ band shapes (engine type codes 8, 1 and 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceShape {
    /// RBJ peaking, width = Q.
    Peaking,
    /// RBJ low shelf, width = shelf slope S, corner at f0.
    LowShelf,
    /// RBJ high shelf, width = shelf slope S, corner at f0.
    HighShelf,
}

/// Voice-EQ band design. Returns `None` when `f0 ≥ fs/2` (the engine then
/// keeps this band's and the following bands' previous taps).
pub fn design_voice(shape: VoiceShape, f0: f32, width: f32, gain_db: f32, fs: u32) -> Option<Taps> {
    if f0 >= (fs / 2) as f32 {
        return None;
    }
    let v = 10f64.powf(f64::from(gain_db) / 20.0);
    let a = v.sqrt();
    let w0 = TAU * f64::from(f0) / f64::from(fs);
    let s = w0.sin();
    let c = w0.cos();
    let w = f64::from(width);
    let alpha = s / (2.0 * w);
    Some(match shape {
        VoiceShape::Peaking => {
            let ap = alpha / a;
            let a0 = 1.0 + ap;
            norm(1.0 + v * ap, -2.0 * c, 1.0 - v * ap, a0, -2.0 * c, 1.0 - ap)
        }
        VoiceShape::LowShelf | VoiceShape::HighShelf => {
            // 2·√A·α_S with α_S from the slope S, written as the engine does
            // (the inner term multiplied by A, floored at 0.0081).
            let inner = ((1.0 / w - 1.0) * (v + 1.0) + 2.0 * a).max(0.0081);
            let t = alpha * inner.sqrt() * (2.0 * w);
            if shape == VoiceShape::LowShelf {
                norm(
                    a * ((a + 1.0) - (a - 1.0) * c + t),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                    a * ((a + 1.0) - (a - 1.0) * c - t),
                    (a + 1.0) + (a - 1.0) * c + t,
                    -2.0 * ((a - 1.0) + (a + 1.0) * c),
                    (a + 1.0) + (a - 1.0) * c - t,
                )
            } else {
                norm(
                    a * ((a + 1.0) + (a - 1.0) * c + t),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                    a * ((a + 1.0) + (a - 1.0) * c - t),
                    (a + 1.0) - (a - 1.0) * c + t,
                    2.0 * ((a - 1.0) - (a + 1.0) * c),
                    (a + 1.0) - (a - 1.0) * c - t,
                )
            }
        }
    })
}

/// Direct form II section with `f64` arithmetic and `f32` state between
/// calls (de-popper and de-esser crossover).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Df2Section {
    pub w1: f32,
    pub w2: f32,
}

impl Df2Section {
    /// Filter `x` in place. With `flush`, state below 1e-15 is zeroed at the
    /// end of the call (the engine does this at the end of every kernel call).
    pub fn run(&mut self, t: &Taps, x: &mut [f32], flush: bool) {
        let (b0, b1, b2, a1, a2) = (
            f64::from(t.b0),
            f64::from(t.b1),
            f64::from(t.b2),
            f64::from(t.a1),
            f64::from(t.a2),
        );
        let (mut w1, mut w2) = (f64::from(self.w1), f64::from(self.w2));
        for v in x.iter_mut() {
            let w = (f64::from(*v) - w1 * a1) - w2 * a2;
            *v = ((w1 * b1 + w * b0) + w2 * b2) as f32;
            w2 = w1;
            w1 = w;
        }
        if flush {
            if w1.abs() < FLUSH {
                w1 = 0.0;
            }
            if w2.abs() < FLUSH {
                w2 = 0.0;
            }
        }
        self.w1 = w1 as f32;
        self.w2 = w2 as f32;
    }
}

/// `f32` transposed direct form II section (voice EQ).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tdf2Section {
    pub s1: f32,
    pub s2: f32,
}

impl Tdf2Section {
    /// Filter `x` in place: `y = b0·x + s1; s1 = (b1·x + s2) + (−a1)·y;
    /// s2 = (−a2)·y + b2·x`.
    pub fn run(&mut self, t: &Taps, x: &mut [f32]) {
        let (na1, na2) = (-t.a1, -t.a2);
        let (mut s1, mut s2) = (self.s1, self.s2);
        for v in x.iter_mut() {
            let xi = *v;
            let y = xi * t.b0 + s1;
            s1 = (xi * t.b1 + s2) + y * na1;
            s2 = y * na2 + xi * t.b2;
            *v = y;
        }
        self.s1 = s1;
        self.s2 = s2;
    }

    /// End-of-block denormal guard.
    pub fn flush(&mut self) {
        if f64::from(self.s1).abs() < FLUSH {
            self.s1 = 0.0;
        }
        if f64::from(self.s2).abs() < FLUSH {
            self.s2 = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 48_000;

    #[test]
    fn butterworth_hp_lp_are_minus_3_db_at_the_corner() {
        let hp = highpass_butter2(1000.0, FS);
        let lp = lowpass_butter2(1000.0, FS);
        assert!((hp.magnitude_db(1000.0, 48e3) + 3.0103).abs() < 1e-3);
        assert!((lp.magnitude_db(1000.0, 48e3) + 3.0103).abs() < 1e-3);
        assert!(hp.magnitude_db(20_000.0, 48e3).abs() < 0.01);
        assert!(lp.magnitude_db(20.0, 48e3).abs() < 0.01);
    }

    #[test]
    fn voice_bands_hit_their_gain() {
        let p = design_voice(VoiceShape::Peaking, 900.0, 1.0, 6.0, FS).unwrap();
        assert!((p.magnitude_db(900.0, 48e3) - 6.0).abs() < 1e-3);
        let ls = design_voice(VoiceShape::LowShelf, 200.0, 1.0, -6.0, FS).unwrap();
        assert!((ls.magnitude_db(2.0, 48e3) + 6.0).abs() < 1e-2);
        // S = 1, corner at f0: half the gain in dB.
        assert!((ls.magnitude_db(200.0, 48e3) + 3.0).abs() < 0.05);
        let hs = design_voice(VoiceShape::HighShelf, 4000.0, 1.0, 7.0, FS).unwrap();
        assert!((hs.magnitude_db(23_900.0, 48e3) - 7.0).abs() < 0.05);
        assert!(design_voice(VoiceShape::HighShelf, 24_000.0, 1.0, 0.0, FS).is_none());
    }
}
