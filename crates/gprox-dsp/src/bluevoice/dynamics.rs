//! Blue VO!CE expander, compressor, de-esser and limiter. One gain decision
//! per sub-block
//! (32 samples at 48 kHz), linear gain ramps, look-ahead delays.

use super::blocks::{CH, Geometry, comp_style_coefs, db, lin, ms_to_ctl, slope};
use super::params::{BvParams, P};
use crate::biquad::Taps;
use crate::lookahead::{MAX_SUB, PeakHold, SLOTS, SubBlockDelay, peak, ring_len};
use crate::rbj::{Df2Section, highpass_butter2, lowpass_butter2};
use crate::smoothing::{AttackOn, TwoSpeed, coef};

fn as_ro<'a>(chans: &'a [&'a mut [f32]]) -> [&'a [f32]; CH] {
    let mut r: [&[f32]; CH] = [&[]; CH];
    for (d, s) in r.iter_mut().zip(chans.iter()) {
        *d = s;
    }
    r
}

fn chan_peak(chans: &[&mut [f32]], n: usize) -> f32 {
    let ro = as_ro(chans);
    peak(&ro[..chans.len().min(CH)], n)
}

// ------------------------------------------------------------------ expander

/// Length of the auto-threshold history (2 s of control periods at 48 kHz).
pub const AUTO_RING: usize = 3000;

/// Expander / gate.
#[derive(Debug, Clone)]
pub struct Expander {
    thresh: f32,
    auto_on: bool,
    sense: f32,
    ratio: f32,
    att_max: f32,
    hold: u32,
    det: PeakHold,
    held: f32,
    count: u32,
    s1: TwoSpeed,
    s2: TwoSpeed,
    auto_s: TwoSpeed,
    auto_ring: Box<[f32; AUTO_RING]>,
    auto_idx: usize,
    delay: SubBlockDelay<CH>,
    g_prev: f32,
    pub(crate) last_gain: f32,
}

impl Expander {
    pub fn new(g: &Geometry) -> Self {
        let s = TwoSpeed::new(AttackOn::Rise, 1.0, 1.0, 1.0, 1.0);
        Expander {
            thresh: 0.0,
            auto_on: false,
            sense: 0.0,
            ratio: 1.0,
            att_max: 0.0,
            hold: 0,
            det: PeakHold::new(ring_len(g.fs, g.sub, 20)),
            held: 1.0,
            count: 0,
            s1: s,
            s2: s,
            auto_s: TwoSpeed::new(AttackOn::Rise, 1.0, 1.0, 1.0, 0.0),
            auto_ring: Box::new([0.0; AUTO_RING]),
            auto_idx: 0,
            delay: SubBlockDelay::new(SLOTS),
            g_prev: 1.0,
            last_gain: 1.0,
        }
    }

    /// Take over parameters (the conversions `set` does).
    pub fn configure(&mut self, p: &BvParams, g: &Geometry) {
        let r = g.ctl_rate();
        let (a, rel) = (p.f(P::ExpAttack), p.f(P::ExpRelease));
        self.thresh = p.f(P::ExpThresh);
        self.auto_on = p.on(P::ExpThreshAutoEnable);
        self.sense = p.f(P::ExpThreshAutoSense);
        self.ratio = p.f(P::ExpRatio);
        self.att_max = -p.f(P::ExpAttenuationMax).abs();
        self.hold = ms_to_ctl(r, p.f(P::ExpHysteresis));
        self.s1.ka = coef(r, a);
        self.s1.kb = coef(r, (0.05 * rel).max(5.0));
        self.s2.ka = coef(r, (20.0 * a).max(5.0));
        self.s2.kb = coef(r, rel);
        self.auto_s.ka = coef(r, 2000.0);
        self.auto_s.kb = 1.0;
    }

    /// State after the module is switched off (gains back to 1, history and
    /// look-ahead cleared). The auto-threshold floor is kept, as in the engine.
    pub fn reset(&mut self) {
        self.det.reset();
        self.held = 1.0;
        self.count = 0;
        self.s1.y = 1.0;
        self.s2.y = 1.0;
        self.g_prev = 1.0;
        self.auto_ring.fill(0.0);
        self.delay.reset();
    }

    /// [`Expander::reset`] plus the auto-threshold floor (after non-finite
    /// output, [`super::BlueVoice`]).
    pub fn clear(&mut self) {
        self.reset();
        self.auto_s.y = 0.0;
    }

    pub fn process(&mut self, chans: &mut [&mut [f32]], n: usize, inv_sub: f32) {
        let pk = self.det.update(chan_peak(chans, n));
        let l = db(pk).max(-120.0);
        let mut t = self.thresh;
        if self.auto_on {
            self.auto_ring[self.auto_idx] = l;
            self.auto_idx += 1;
            if self.auto_idx >= AUTO_RING {
                self.auto_idx = 0;
            }
            let mut mn = l;
            for &v in self.auto_ring.iter() {
                if mn >= v {
                    mn = v;
                }
            }
            self.auto_s.step(mn);
            if self.sense + self.auto_s.y > t {
                self.auto_s.y = t - self.sense;
            }
            t = self.sense + self.auto_s.y;
        }
        let mut gr = 0.0;
        if t > l {
            let d = l - t;
            gr = d * self.ratio - d;
            // Floor at att_max; NaN also takes the floor.
            if gr.is_nan() || gr < self.att_max {
                gr = self.att_max;
            }
        }
        let mut g = lin(gr);
        if g > self.held || self.count >= self.hold {
            self.held = g;
            self.count = 0;
        } else {
            g = self.held;
            self.count += 1;
        }
        let a = self.s1.step(g);
        let b = self.s2.step(g);
        let gn = a.max(b);
        let inc = (gn - self.g_prev) * inv_sub;
        self.delay.ramp_through(chans, n, self.g_prev, inc);
        self.g_prev = gn;
        self.last_gain = gn;
    }
}

// ------------------------------------------------------------------ compressor

/// Compressor. Makeup is applied by the limiter.
#[derive(Debug, Clone)]
pub struct Compressor {
    thresh: f32,
    slope: f32,
    det: PeakHold,
    s1: TwoSpeed,
    s2: TwoSpeed,
    delay: SubBlockDelay<CH>,
    g_prev: f32,
    pub(crate) last_gain: f32,
}

impl Compressor {
    pub fn new(g: &Geometry) -> Self {
        let s = TwoSpeed::new(AttackOn::Fall, 1.0, 1.0, 1.0, 1.0);
        Compressor {
            thresh: 0.0,
            slope: 0.0,
            det: PeakHold::new(ring_len(g.fs, g.sub, 20)),
            s1: s,
            s2: s,
            delay: SubBlockDelay::new(SLOTS),
            g_prev: 1.0,
            last_gain: 1.0,
        }
    }

    pub fn configure(&mut self, p: &BvParams, g: &Geometry) {
        self.thresh = p.f(P::CompThresh);
        self.slope = slope(p.f(P::CompRatio));
        let k = comp_style_coefs(g.ctl_rate(), p.f(P::CompAttack), p.f(P::CompRelease));
        (self.s1.ka, self.s1.kb) = (k[0][0], k[0][1]);
        (self.s2.ka, self.s2.kb) = (k[1][0], k[1][1]);
    }

    pub fn reset(&mut self) {
        self.det.reset();
        self.s1.y = 1.0;
        self.s2.y = 1.0;
        self.g_prev = 1.0;
        self.delay.reset();
    }

    pub fn process(&mut self, chans: &mut [&mut [f32]], n: usize, inv_sub: f32) {
        let l = db(self.det.update(chan_peak(chans, n)));
        let x2 = (self.thresh - l).min(0.0) * self.slope;
        let x1 = (x2 + l).max(0.0);
        let g = lin(x2 - x1);
        let a = self.s1.step(g);
        let b = self.s2.step(a);
        let gn = a.min(b);
        let inc = (gn - self.g_prev) * inv_sub;
        self.delay.ramp_through(chans, n, self.g_prev, inc);
        self.g_prev = gn;
        self.last_gain = gn;
    }
}

// ------------------------------------------------------------------ de-esser

/// Split-band de-esser: LR4 crossover at `des.freq`,
/// relative detection of the high band against the low band, only the high
/// band is attenuated. The bands are delayed by 3 sub-blocks.
#[derive(Debug, Clone)]
pub struct DeEsser {
    thresh: f32,
    slope: f32,
    lp: Taps,
    hp: Taps,
    /// `[channel][band 0 = high, 1 = low][stage]`.
    sec: [[[Df2Section; 2]; 2]; CH],
    /// `[channel][band][slot][sample]`.
    ring: Box<[[[[f32; MAX_SUB]; SLOTS]; 2]; CH]>,
    slot: usize,
    det_hi: PeakHold,
    det_lo: PeakHold,
    s1: TwoSpeed,
    s2: TwoSpeed,
    g_prev: f32,
    pub(crate) last_gain: f32,
}

impl DeEsser {
    pub fn new(g: &Geometry) -> Self {
        let s = TwoSpeed::new(AttackOn::Fall, 1.0, 1.0, 1.0, 1.0);
        let m = ring_len(g.fs, g.sub, 2000);
        DeEsser {
            thresh: 0.0,
            slope: 0.0,
            lp: Taps::IDENTITY,
            hp: Taps::IDENTITY,
            sec: [[[Df2Section::default(); 2]; 2]; CH],
            ring: Box::new([[[[0.0; MAX_SUB]; SLOTS]; 2]; CH]),
            slot: 0,
            det_hi: PeakHold::new(m),
            det_lo: PeakHold::new(m),
            s1: s,
            s2: s,
            g_prev: 1.0,
            last_gain: 1.0,
        }
    }

    pub fn configure(&mut self, p: &BvParams, g: &Geometry) {
        self.thresh = p.f(P::DesThresh);
        self.slope = slope(p.f(P::DesRatio));
        let k = comp_style_coefs(g.ctl_rate(), p.f(P::DesAttack), p.f(P::DesRelease));
        (self.s1.ka, self.s1.kb) = (k[0][0], k[0][1]);
        (self.s2.ka, self.s2.kb) = (k[1][0], k[1][1]);
    }

    /// Redesign the crossover for the (smoothed) frequency. A design that
    /// isn't a stable filter (frequency ≤ 0 or ≥ fs/2) keeps the previous
    /// taps.
    pub fn design(&mut self, freq: f32, fs: u32) {
        let (lp, hp) = (lowpass_butter2(freq, fs), highpass_butter2(freq, fs));
        if lp.is_stable() && hp.is_stable() {
            (self.lp, self.hp) = (lp, hp);
        }
    }

    /// Everything [`DeEsser::reset`] clears plus the band delay line (after
    /// non-finite output, [`super::BlueVoice`]).
    pub fn clear(&mut self) {
        self.reset();
        for c in self.ring.iter_mut() {
            for band in c.iter_mut() {
                for slot in band.iter_mut() {
                    slot.fill(0.0);
                }
            }
        }
    }

    pub fn reset(&mut self) {
        self.s1.y = 1.0;
        self.s2.y = 1.0;
        self.g_prev = 1.0;
        self.sec = [[[Df2Section::default(); 2]; 2]; CH];
        self.det_hi.reset();
        self.det_lo.reset();
    }

    pub fn process(&mut self, chans: &mut [&mut [f32]], n: usize, inv_sub: f32) {
        let nch = chans.len().min(CH);
        let slot = self.slot;
        let mut p_hi = 0.0f32;
        let mut p_lo = 0.0f32;
        for (c, x) in chans.iter().enumerate().take(nch) {
            for (band, taps) in [(0, self.hp), (1, self.lp)] {
                let dst = &mut self.ring[c][band][slot][..n];
                dst.copy_from_slice(&x[..n]);
                for st in &mut self.sec[c][band] {
                    st.run(&taps, dst, true);
                }
            }
            let r = &self.ring[c];
            p_hi = r[0][slot][..n]
                .iter()
                .fold(p_hi, |m, v| if v.abs() >= m { v.abs() } else { m });
            p_lo = r[1][slot][..n]
                .iter()
                .fold(p_lo, |m, v| if v.abs() >= m { v.abs() } else { m });
        }
        let ph = self.det_hi.update(p_hi);
        let pl = self.det_lo.update(p_lo);
        let ratio = ((f64::from(ph) + 1e-15) / (f64::from(pl) + 1e-15)) as f32;
        let rel = db(ratio);
        let mut gr = (self.thresh - rel).min(0.0) * self.slope;
        // The engine clamps with the *linear* low-band peak.
        gr -= (gr + pl).max(0.0);
        let g = lin(gr);
        let a = self.s1.step(g);
        let b = self.s2.step(a);
        let gn = a.min(b);
        self.slot = (self.slot + 1) % SLOTS;
        let rd = self.slot;
        let inc = (gn - self.g_prev) * inv_sub;
        for (c, x) in chans.iter_mut().enumerate().take(nch) {
            let r = &self.ring[c];
            let mut gg = self.g_prev;
            for ((o, &hi), &lo) in x[..n].iter_mut().zip(&r[0][rd][..n]).zip(&r[1][rd][..n]) {
                *o = gg * hi + lo;
                gg += inc;
            }
        }
        self.g_prev = gn;
        self.last_gain = gn;
    }
}

// ------------------------------------------------------------------ limiter

/// Input-gain one-pole per sub-block (≈ 20 ms), hard-coded in the engine.
const GIN_KEEP: f32 = 0.967_216_1;
const GIN_TAKE: f32 = 0.032_783_9;
/// Detector floor, 1/32767.
const PEAK_FLOOR: f32 = 3.051_851e-5;
/// Fixed limiter constants from the params block.
const CEILING: f32 = 0.8;
const FACTOR: f32 = 1.0;
const LOOKAHEAD: usize = 4;
/// −1 dB hysteresis of the slow path.
const SLOW_HYST: f32 = 0.891_251;
/// Look-ahead path smoother (internal constants, ≈ 3.8 ms / 0.5 ms).
const S3_KA: f32 = 0.161_429;
const S3_KB: f32 = 0.736_403;

/// Limiter, always on: input gain `lim.boost`
/// (× `comp.makeup` while the compressor is enabled), ceiling 0.8, three
/// gain paths, 128-sample look-ahead, hard clip to ±1.
#[derive(Debug, Clone)]
pub struct Limiter {
    det: PeakHold,
    s1: TwoSpeed,
    s2: TwoSpeed,
    s3: TwoSpeed,
    delay: SubBlockDelay<CH>,
    g_in: f32,
    g_prev: f32,
    pub(crate) target: f32,
    pub(crate) last_gain: f32,
}

impl Limiter {
    pub fn new(g: &Geometry) -> Self {
        Limiter {
            det: PeakHold::new(ring_len(g.fs, g.sub, 20)),
            s1: TwoSpeed::new(AttackOn::Fall, 1.0, 1.0, 1.0, 1.0),
            s2: TwoSpeed::new(AttackOn::Fall, SLOW_HYST, 1.0, 1.0, 1.0),
            s3: TwoSpeed::new(AttackOn::Fall, 1.0, S3_KA, S3_KB, 1.0),
            delay: SubBlockDelay::new(LOOKAHEAD),
            g_in: 1.0,
            g_prev: 1.0,
            target: 1.0,
            last_gain: 1.0,
        }
    }

    pub fn configure(&mut self, p: &BvParams, g: &Geometry) {
        let r = g.ctl_rate();
        let (a, rel) = (p.f(P::LimAttack), p.f(P::LimRelease));
        self.s1.ka = coef(r, a);
        self.s1.kb = coef(r, 0.2 * rel);
        self.s2.ka = coef(r, 5.0 * a);
        self.s2.kb = coef(r, rel);
        self.target = p.f(P::LimBoost)
            * if p.on(P::CompEnable) {
                p.f(P::CompMakeup)
            } else {
                1.0
            };
    }

    /// Back to the state of a new limiter, keeping the parameters (after
    /// non-finite output, [`super::BlueVoice`]).
    pub fn reset(&mut self) {
        self.det.reset();
        self.s1.y = 1.0;
        self.s2.y = 1.0;
        self.s3.y = 1.0;
        self.delay.reset();
        self.g_in = 1.0;
        self.g_prev = 1.0;
        self.last_gain = 1.0;
    }

    pub fn process(&mut self, chans: &mut [&mut [f32]], n: usize, inv_sub: f32) {
        let g_in_prev = self.g_in;
        self.g_in = self.g_in * GIN_KEEP + self.target * GIN_TAKE;
        let g_in = self.g_in;
        let mut p = self.det.update(chan_peak(chans, n)) * g_in;
        if p.is_nan() || p < PEAK_FLOOR {
            p = PEAK_FLOOR;
        }
        let g1r = CEILING / p;
        let g1 = if 1.0 >= g1r { g1r } else { 1.0 };
        let g2r = g1r * FACTOR;
        let g2 = if 1.0 >= g2r { g2r } else { 1.0 };
        self.s1.step(g1);
        self.s2.step(g2);
        let w = (self.det.recent_max(LOOKAHEAD + 1) * g_in).max(1.0);
        self.s3.step(1.0 / w);
        let m12 = self.s1.y.min(self.s2.y);
        let gg = if m12 >= self.s3.y { self.s3.y } else { m12 };
        let start = self.g_prev * g_in;
        let inc = (gg * g_in - self.g_prev * g_in_prev) * inv_sub;
        self.delay.ramp_through(chans, n, start, inc);
        for x in chans.iter_mut() {
            for v in x[..n].iter_mut() {
                *v = v.clamp(-1.0, 1.0);
            }
        }
        self.g_prev = gg;
        self.last_gain = gg;
    }
}
