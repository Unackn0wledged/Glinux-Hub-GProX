//! Per-sample biquad cascade kernels.
//!
//! The headphone EQ is generic over [`CascadeKernel`]. Two kernels exist:
//! * [`IppF32`]: G HUB's Intel IPP single-precision biquad kernel,
//!   re-implemented from its observed arithmetic. It is bit-exact with the hooked G HUB output
//!   when run on G HUB's block grid (480 frames at 48 kHz from stream start).
//!   This is the default.
//! * [`Tdf2F64Acc`]: a clean transposed direct form II with `f64` arithmetic,
//!   kept as a reference (about −49 dB from G HUB on LF-heavy material).
//!
//! Contract shared by all kernels:
//! * taps are `f32` ([`Taps`]), the signal between sections is `f32`;
//! * each section keeps `STATE_LEN` `f32` state words, and the state is kept
//!   across tap changes (G HUB re-initialises its engine with the current
//!   delay line when new taps are pushed);
//! * processing is in place on one channel, and a call never crosses a block
//!   boundary of the processing grid (`pos` is the position of `buf[0]` in a
//!   block of `block_len` frames). Real-time safe: no allocation, locks,
//!   syscalls or logging.

use crate::biquad::Taps;

/// Largest per-section state any kernel may use. Fixes the size of the state
/// arrays the EQ keeps inline.
pub const MAX_STATE_PER_SECTION: usize = 12;

/// A biquad cascade implementation.
pub trait CascadeKernel: 'static {
    /// `f32` state words per section (≤ [`MAX_STATE_PER_SECTION`]).
    const STATE_LEN: usize;

    /// The leading words of each section's state that carry the filter state
    /// across a block boundary (the TDF2 `z1`, `z2`); the rest is scratch
    /// within a block. Setting these at a block start defines the state.
    const CARRIED_STATE: usize = 2;

    /// Run `buf` through the sections in `taps` order, in place.
    /// `state.len() == taps.len() * STATE_LEN`; section `i` owns
    /// `state[i * STATE_LEN..(i + 1) * STATE_LEN]`. `buf[0]` sits at frame
    /// `pos` of a processing block of `block_len` frames, and `buf` does not
    /// extend past that block.
    fn process(taps: &[Taps], state: &mut [f32], buf: &mut [f32], pos: usize, block_len: usize);
}

/// Transposed direct form II with `f32` state and `f64` arithmetic.
///
/// Per section and sample (`x`, `y` are the `f32` signal promoted to `f64`):
/// `y = b0·x + s1`, `s1' = b1·x − a1·y + s2`, `s2' = b2·x − a2·y`; `y`, `s1'`
/// and `s2'` are rounded to `f32`. State words below the smallest normal
/// `f32` are flushed to zero so decaying tails never run on denormals.
/// Ignores the block grid.
///
/// Against a plain `f64` cascade this is about −120 dB; against G HUB's IPP
/// kernel about −49 dB on LF-heavy material.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tdf2F64Acc;

#[inline(always)]
fn ftz(v: f64) -> f32 {
    let f = v as f32;
    if f.abs() < f32::MIN_POSITIVE { 0.0 } else { f }
}

impl CascadeKernel for Tdf2F64Acc {
    const STATE_LEN: usize = 2;

    fn process(taps: &[Taps], state: &mut [f32], buf: &mut [f32], _pos: usize, _block_len: usize) {
        for (t, st) in taps.iter().zip(state.chunks_exact_mut(Self::STATE_LEN)) {
            let (b0, b1, b2) = (f64::from(t.b0), f64::from(t.b1), f64::from(t.b2));
            let (a1, a2) = (f64::from(t.a1), f64::from(t.a2));
            let mut s1 = f64::from(st[0]);
            let mut s2 = f64::from(st[1]);
            for v in buf.iter_mut() {
                let x = f64::from(*v);
                let y = b0 * x + s1;
                s1 = f64::from(ftz(b1 * x - a1 * y + s2));
                s2 = f64::from(ftz(b2 * x - a2 * y));
                *v = y as f32;
            }
            st[0] = s1 as f32;
            st[1] = s2 as f32;
        }
    }
}

/// G HUB's IPP `f32` biquad kernel, run sample by sample.
///
/// Within a block (sub-blocks of at most [`IppF32::CHUNK`] frames) each
/// section computes an FIR part `w[n] = (B2·x[n−2] + B1·x[n−1]) + B0·x[n]`,
/// the first two outputs from the carried TDF2 state `z1`, `z2`, then the
/// rest in groups of four whose outputs are written directly in terms of the
/// two outputs before the group (look-ahead coefficients `c2..c4`, `d1..d3`),
/// then a scalar tail; `z1`, `z2` are formed at the block end. All
/// arithmetic is single `f32` operations in the spec's order: no fused
/// multiply-add, no reassociation.
///
/// Every output of a group depends only on inputs up to its own sample, so
/// this implementation streams: it keeps the group's partial terms in its
/// state and needs no look-ahead buffering. A host buffer may end anywhere
/// in a block; the result is bit-identical to processing whole blocks, with
/// no added latency.
///
/// Verified bit-exact against G HUB for 480-frame blocks. The behaviour for
/// blocks of ≤ 4·sections or > 1024 frames is not verified (IPP may take
/// another path there).
#[derive(Debug, Default, Clone, Copy)]
pub struct IppF32;

// State word indices of one IppF32 section.
const Z1: usize = 0; // carried TDF2 state, valid at block boundaries
const Z2: usize = 1;
const X1: usize = 2; // x[n−1], x[n−2] within the block
const X2: usize = 3;
const Y1: usize = 4; // y[n−1], y[n−2] within the block
const Y2: usize = 5;
const GA: usize = 6; // y[g−2], y[g−1] at the start of the current group
const GB: usize = 7;
const W0: usize = 8; // w of the current group's first three lanes
const W1: usize = 9;
const W2: usize = 10;

/// Per-section coefficients as the IPP init prepares them.
#[derive(Debug, Clone, Copy)]
struct IppCoefs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    p: f32,
    q: f32,
    c2: f32,
    c3: f32,
    c4: f32,
    d1: f32,
    d2: f32,
    d3: f32,
}

impl IppCoefs {
    fn new(t: &Taps) -> Self {
        // r = 1/a0, computed in f32 by IPP; a0 is 1.0 in every tap set G HUB
        // pushes, so r is exactly 1. Kept so the products below follow the
        // spec's operation list.
        let r = 1.0f32;
        let p = (-r) * t.a1;
        let q = (-r) * t.a2;
        let c2 = p * p + q;
        let c3 = p * c2 + q * p;
        let c4 = q * c2 + p * c3;
        let d1 = p * q;
        let d2 = p * d1 + q * q;
        let d3 = d2 * p + q * d1;
        IppCoefs {
            b0: t.b0 * r,
            b1: t.b1 * r,
            b2: t.b2 * r,
            a1: r * t.a1,
            p,
            q,
            c2,
            c3,
            c4,
            d1,
            d2,
            d3,
        }
    }
}

impl IppF32 {
    /// Largest sub-block IPP processes in one pass.
    pub const CHUNK: usize = 1024;

    fn sub_block(pos: usize, block_len: usize) -> (usize, usize) {
        let start = (pos / Self::CHUNK) * Self::CHUNK;
        (
            pos - start,
            block_len.saturating_sub(start).min(Self::CHUNK),
        )
    }

    #[inline(always)]
    fn section(c: &IppCoefs, st: &mut [f32], buf: &mut [f32], pos: usize, block_len: usize) {
        let (mut n, mut len) = Self::sub_block(pos, block_len);
        let mut chunk_start = pos - n;
        for v in buf.iter_mut() {
            if len == 0 {
                // Defensive: the caller overran the block; start a new one.
                (chunk_start, n, len) = (0, 0, block_len.clamp(1, Self::CHUNK));
            }
            let x = *v;
            let y = if n == 0 {
                let y = c.b0 * x + st[Z1];
                if len == 1 {
                    st[Z1] = (st[Z2] - y * c.a1) + x * c.b1;
                    st[Z2] = c.q * y + c.b2 * x;
                }
                y
            } else if n == 1 {
                ((st[Z2] - st[Y1] * c.a1) + st[X1] * c.b1) + c.b0 * x
            } else {
                let w = (c.b2 * st[X2] + c.b1 * st[X1]) + c.b0 * x;
                let k = n - 2;
                if k < (len - 2) / 4 * 4 {
                    match k % 4 {
                        0 => {
                            st[GA] = st[Y2];
                            st[GB] = st[Y1];
                            st[W0] = w;
                            (st[GA] * c.q + st[GB] * c.p) + w
                        }
                        1 => {
                            st[W1] = w;
                            (st[GA] * c.d1 + st[GB] * c.c2) + (c.p * st[W0] + w)
                        }
                        2 => {
                            st[W2] = w;
                            (st[GA] * c.d2 + st[GB] * c.c3) + ((c.c2 * st[W0] + c.p * st[W1]) + w)
                        }
                        _ => {
                            (st[GA] * c.d3 + st[GB] * c.c4)
                                + ((c.c3 * st[W0] + c.c2 * st[W1]) + (c.p * st[W2] + w))
                        }
                    }
                } else {
                    (w + st[Y2] * c.q) + st[Y1] * c.p
                }
            };
            st[X2] = st[X1];
            st[X1] = x;
            st[Y2] = st[Y1];
            st[Y1] = y;
            *v = y;
            n += 1;
            if n == len {
                if len >= 2 {
                    let f1 = c.b2 * st[X2] + c.b1 * st[X1];
                    let f2 = c.b2 * st[X1];
                    st[Z1] = (c.q * st[Y2] + c.p * st[Y1]) + f1;
                    st[Z2] = c.q * st[Y1] + f2;
                }
                chunk_start += len;
                n = 0;
                len = block_len.saturating_sub(chunk_start).min(Self::CHUNK);
            }
        }
    }
}

impl CascadeKernel for IppF32 {
    const STATE_LEN: usize = 11;

    fn process(taps: &[Taps], state: &mut [f32], buf: &mut [f32], pos: usize, block_len: usize) {
        for (t, st) in taps.iter().zip(state.chunks_exact_mut(Self::STATE_LEN)) {
            Self::section(&IppCoefs::new(t), st, buf, pos, block_len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::biquad::{Shape, design_rbj_bw};

    const L: usize = 480;

    fn sig(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32;
                0.3 * (t * 0.004).sin() + 0.2 * (t * 0.13).sin() + 0.05 * (t * 1.7).cos()
            })
            .collect()
    }

    /// RMS of `a − b` relative to RMS of `b`, in dB.
    fn rel_db(a: &[f32], b: &[f32]) -> f64 {
        let e: f64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
            .sum();
        let s: f64 = b.iter().map(|y| f64::from(*y).powi(2)).sum();
        10.0 * (e / s).log10()
    }

    fn taps() -> Vec<Taps> {
        vec![
            design_rbj_bw(Shape::Peaking, 32.0, 12.0, 0.7, 48_000.0).unwrap(),
            design_rbj_bw(Shape::LowShelf, 200.0, 6.0, 1.0, 48_000.0).unwrap(),
            design_rbj_bw(Shape::HighShelf, 4500.0, -6.0, 1.0, 48_000.0).unwrap(),
        ]
    }

    /// Drive a kernel over `x` in host chunks of `quantum`, never crossing
    /// an `L` boundary, tracking the block position.
    fn run<K: CascadeKernel>(taps: &[Taps], x: &[f32], quantum: usize) -> Vec<f32> {
        let mut st = vec![0.0f32; taps.len() * K::STATE_LEN];
        let mut out = x.to_vec();
        let mut i = 0;
        while i < out.len() {
            let pos = i % L;
            let n = quantum.min(L - pos).min(out.len() - i);
            K::process(taps, &mut st, &mut out[i..i + n], pos, L);
            i += n;
        }
        out
    }

    /// Literal whole-block version of the IPP kernel's five steps for one
    /// section, as an independent check of the streaming implementation.
    fn ipp_block_reference(t: &Taps, z: &mut [f32; 2], x: &mut [f32]) {
        let c = IppCoefs::new(t);
        let l = x.len();
        let mut w = vec![0.0f32; l];
        for n in 2..l {
            w[n] = (c.b2 * x[n - 2] + c.b1 * x[n - 1]) + c.b0 * x[n];
        }
        let f1 = c.b2 * x[l - 2] + c.b1 * x[l - 1];
        let f2 = c.b2 * x[l - 1];
        let mut y = vec![0.0f32; l];
        y[0] = c.b0 * x[0] + z[0];
        y[1] = ((z[1] - y[0] * c.a1) + x[0] * c.b1) + c.b0 * x[1];
        let mut n = 2;
        while n + 4 <= l {
            let (ya, yb) = (y[n - 2], y[n - 1]);
            y[n] = (ya * c.q + yb * c.p) + w[n];
            y[n + 1] = (ya * c.d1 + yb * c.c2) + (c.p * w[n] + w[n + 1]);
            y[n + 2] = (ya * c.d2 + yb * c.c3) + ((c.c2 * w[n] + c.p * w[n + 1]) + w[n + 2]);
            y[n + 3] = (ya * c.d3 + yb * c.c4)
                + ((c.c3 * w[n] + c.c2 * w[n + 1]) + (c.p * w[n + 2] + w[n + 3]));
            n += 4;
        }
        while n < l {
            y[n] = (w[n] + y[n - 2] * c.q) + y[n - 1] * c.p;
            n += 1;
        }
        z[0] = (c.q * y[l - 2] + c.p * y[l - 1]) + f1;
        z[1] = c.q * y[l - 1] + f2;
        x.copy_from_slice(&y);
    }

    #[test]
    fn identity_is_bit_exact() {
        let x = sig(2000);
        let id = [Taps::IDENTITY, Taps::IDENTITY];
        assert_eq!(run::<Tdf2F64Acc>(&id, &x, 256), x);
        assert_eq!(run::<IppF32>(&id, &x, 256), x);
    }

    #[test]
    fn tdf2_matches_f64_direct_form_on_an_impulse() {
        let t = design_rbj_bw(Shape::Peaking, 1000.0, 6.0, 0.7, 48_000.0).unwrap();
        let mut x = vec![0.0f32; 256];
        x[0] = 1.0;
        let got = run::<Tdf2F64Acc>(&[t], &x, 256);
        let (b0, b1, b2, a1, a2) = (
            f64::from(t.b0),
            f64::from(t.b1),
            f64::from(t.b2),
            f64::from(t.a1),
            f64::from(t.a2),
        );
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        for (n, &g) in got.iter().enumerate() {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let y = b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
            (x2, x1, y2, y1) = (x1, x, y1, y);
            assert!((f64::from(g) - y).abs() < 1e-6, "n={n}");
        }
    }

    #[test]
    fn ipp_streaming_equals_the_block_transcription() {
        let x = sig(10 * L);
        let t = taps();
        let mut want = x.clone();
        for tt in &t {
            let mut z = [0.0f32; 2];
            for blk in want.chunks_mut(L) {
                ipp_block_reference(tt, &mut z, blk);
            }
        }
        assert_eq!(run::<IppF32>(&t, &x, L), want);
    }

    #[test]
    fn output_does_not_depend_on_the_host_quantum() {
        let x = sig(10 * L);
        let t = taps();
        let a = run::<IppF32>(&t, &x, L);
        let b = run::<Tdf2F64Acc>(&t, &x, L);
        for q in [1, 3, 64, 137, 256] {
            assert_eq!(run::<IppF32>(&t, &x, q), a, "ipp quantum {q}");
            assert_eq!(run::<Tdf2F64Acc>(&t, &x, q), b, "tdf2 quantum {q}");
        }
    }

    #[test]
    fn ipp_and_tdf2_agree_to_f32_rounding() {
        let x = sig(20 * L);
        let t = taps();
        let a = run::<IppF32>(&t, &x, L);
        let b = run::<Tdf2F64Acc>(&t, &x, L);
        // The signal sits on the 32 Hz +12 dB resonance, where f32 rounding
        // noise is amplified most; the two forms differ by that noise only.
        let d = rel_db(&a, &b);
        println!("ipp vs tdf2: {d:.1} dB");
        assert!(d < -42.0, "relative difference {d:.1} dB");
    }

    #[test]
    fn ipp_handles_odd_block_lengths() {
        // Not verified against G HUB; must still be the same filter.
        let x = sig(3000);
        let t = taps();
        let reference = run::<Tdf2F64Acc>(&t, &x, 3000);
        for l in [1usize, 2, 3, 5, 441, 1500] {
            let mut st = vec![0.0f32; t.len() * IppF32::STATE_LEN];
            let mut out = x.clone();
            for blk in out.chunks_mut(l) {
                let n = blk.len();
                IppF32::process(&t, &mut st, blk, 0, l.max(n));
            }
            let d = rel_db(&out, &reference);
            println!("block {l}: {d:.1} dB");
            assert!(d < -48.0, "block {l}: relative difference {d:.1} dB");
        }
    }
}
