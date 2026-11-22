//! Blue VO!CE de-popper and voice EQ. Both are plain filters without latency.

use super::blocks::CH;
use super::params::{BvParams, P};
use crate::biquad::Taps;
use crate::rbj::{Df2Section, Tdf2Section, VoiceShape, design_voice, highpass_butter2};
use crate::smoothing::ParamSmoother;

/// Snap threshold of the frequency/width/gain smoothers (Hz, width, dB).
const SNAP: f32 = 0.01;

/// De-popper: the RBJ 2nd-order Butterworth high-pass three times in series.
#[derive(Debug, Clone)]
pub struct DePopper {
    pub(crate) freq: ParamSmoother,
    taps: Taps,
    sec: [[Df2Section; 3]; CH],
}

impl DePopper {
    pub fn new() -> Self {
        DePopper {
            freq: ParamSmoother::new(80.0, SNAP),
            taps: Taps::IDENTITY,
            sec: [[Df2Section::default(); 3]; CH],
        }
    }

    pub fn configure(&mut self, p: &BvParams) {
        self.freq.target = p.f(P::DpopFreq);
    }

    pub fn design(&mut self, fs: u32) {
        self.taps = highpass_butter2(self.freq.cur, fs);
    }

    pub fn reset(&mut self) {
        self.sec = [[Df2Section::default(); 3]; CH];
    }

    /// Filter in place; `block_end` applies the end-of-block state flush.
    pub fn process(&mut self, chans: &mut [&mut [f32]], n: usize, block_end: bool) {
        for (x, secs) in chans.iter_mut().zip(self.sec.iter_mut()) {
            for s in secs.iter_mut() {
                s.run(&self.taps, &mut x[..n], block_end);
            }
        }
    }
}

impl Default for DePopper {
    fn default() -> Self {
        Self::new()
    }
}

/// Band shapes in cascade order (engine types 1, 8, 2).
pub const VEQ_SHAPES: [VoiceShape; 3] = [
    VoiceShape::LowShelf,
    VoiceShape::Peaking,
    VoiceShape::HighShelf,
];

/// One voice-EQ band's smoothed fields.
#[derive(Debug, Clone, Copy)]
pub struct VeqBand {
    pub freq: ParamSmoother,
    pub width: ParamSmoother,
    /// Gain in dB (the API's linear gain converted on `set`).
    pub gain_db: ParamSmoother,
}

/// Three-band voice EQ, `f32` TDF2 kernel.
#[derive(Debug, Clone)]
pub struct VoiceEq {
    pub(crate) bands: [VeqBand; 3],
    pub(crate) taps: [Taps; 3],
    sec: [[Tdf2Section; 3]; CH],
}

impl VoiceEq {
    pub fn new() -> Self {
        let b = |f: f32, w: f32| VeqBand {
            freq: ParamSmoother::new(f, SNAP),
            width: ParamSmoother::new(w, SNAP),
            gain_db: ParamSmoother::new(0.0, SNAP),
        };
        VoiceEq {
            bands: [b(250.0, 1.0), b(3000.0, 4.0), b(8000.0, 1.0)],
            taps: [Taps::IDENTITY; 3],
            sec: [[Tdf2Section::default(); 3]; CH],
        }
    }

    pub fn configure(&mut self, p: &BvParams) {
        let ids = [
            [P::VeqLowFreq, P::VeqLowWidth, P::VeqLowGain],
            [P::VeqMidFreq, P::VeqMidWidth, P::VeqMidGain],
            [P::VeqHighFreq, P::VeqHighWidth, P::VeqHighGain],
        ];
        for (b, [f, w, g]) in self.bands.iter_mut().zip(ids) {
            b.freq.target = p.f(f);
            b.width.target = p.f(w);
            b.gain_db.target = super::blocks::db(p.f(g));
        }
    }

    /// One smoothing step of all fields (`k`), or a jump on the first block.
    /// Returns whether any field moved.
    pub fn smooth(&mut self, k: f32, first: bool) -> bool {
        let mut moved = false;
        for b in &mut self.bands {
            for s in [&mut b.gain_db, &mut b.freq, &mut b.width] {
                if first {
                    s.settle();
                } else {
                    moved |= s.step(k);
                }
            }
        }
        moved || first
    }

    /// Recompute the band taps from the current fields. A band at or above
    /// fs/2 stops the update (it and later bands keep their taps).
    pub fn design(&mut self, fs: u32) {
        for (k, b) in self.bands.iter().enumerate() {
            match design_voice(VEQ_SHAPES[k], b.freq.cur, b.width.cur, b.gain_db.cur, fs) {
                Some(t) => self.taps[k] = t,
                None => break,
            }
        }
    }

    pub fn reset(&mut self) {
        self.sec = [[Tdf2Section::default(); 3]; CH];
    }

    pub fn process(&mut self, chans: &mut [&mut [f32]], n: usize, block_end: bool) {
        for (x, secs) in chans.iter_mut().zip(self.sec.iter_mut()) {
            for (s, t) in secs.iter_mut().zip(self.taps.iter()) {
                s.run(t, &mut x[..n]);
                if block_end {
                    s.flush();
                }
            }
        }
    }
}

impl Default for VoiceEq {
    fn default() -> Self {
        Self::new()
    }
}
