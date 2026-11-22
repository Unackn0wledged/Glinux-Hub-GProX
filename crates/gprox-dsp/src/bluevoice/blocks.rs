//! Shared helpers for the Blue VO!CE blocks: engine geometry, dB conversions,
//! and the derived-parameter conversions of `set`.

use crate::lookahead::MAX_SUB;
use crate::smoothing::coef;

/// Most channels the engine processes (mono or stereo mic).
pub const CH: usize = 2;

/// Engine geometry for a sample rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub fs: u32,
    /// Frames per engine block (parameter-smoothing step).
    pub block: usize,
    /// Frames per dynamics sub-block (gain decision).
    pub sub: usize,
}

impl Geometry {
    /// 44.1/48 kHz → 256/32, 88.2/96 kHz → 512/64, 176.4/192 kHz → 1024/128.
    /// Other rates (which the engine rejects) use the nearest lower class.
    pub fn for_rate(fs: u32) -> Self {
        let (block, sub) = if fs < 88_200 {
            (256, 32)
        } else if fs < 176_400 {
            (512, 64)
        } else {
            (1024, 128)
        };
        debug_assert!(sub <= MAX_SUB);
        Geometry { fs, block, sub }
    }

    /// Control rate of the dynamics, `fs / sub` as an integer (1500 Hz).
    pub fn ctl_rate(&self) -> f32 {
        (self.fs as usize / self.sub) as f32
    }

    /// Rate of the per-block parameter smoothing, `fs / block` (187 Hz).
    pub fn block_rate(&self) -> f32 {
        (self.fs as usize / self.block) as f32
    }

    /// `1 / sub`, the gain-ramp step factor.
    pub fn inv_sub(&self) -> f32 {
        1.0 / self.sub as f32
    }
}

/// `20·log10(max(x, FLT_MIN))` in `f32`.
#[inline]
pub fn db(x: f32) -> f32 {
    x.max(f32::MIN_POSITIVE).log10() * 20.0
}

/// `10^(x·0.05)` in `f32`.
#[inline]
pub fn lin(x: f32) -> f32 {
    10f32.powf(x * 0.05)
}

/// `1 − 1/r` for ratios ≥ 1 (compressor and de-esser slope), else 0.
pub fn slope(r: f32) -> f32 {
    if r >= 1.0 { 1.0 - 1.0 / r } else { 0.0 }
}

/// Attack/release coefficient pairs of the compressor and de-esser:
/// smoother 1 = (coef(a), coef(max(5, 0.2·r))), smoother 2 =
/// (coef(max(5, 5·a)), coef(r)).
pub fn comp_style_coefs(rate: f32, attack: f32, release: f32) -> [[f32; 2]; 2] {
    [
        [coef(rate, attack), coef(rate, (0.2 * release).max(5.0))],
        [coef(rate, (5.0 * attack).max(5.0)), coef(rate, release)],
    ]
}

/// Number of whole control periods in `ms`, truncated (expander hold).
pub fn ms_to_ctl(rate: f32, ms: f32) -> u32 {
    let v = rate * ms / 1000.0;
    if v > 0.0 { v as u32 } else { 0 }
}

/// Largest engine block (176.4/192 kHz).
pub const MAX_BLOCK: usize = 1024;

/// Planar scratch for up to [`CH`] channels of one engine block.
pub type BlockBuf = [[f32; MAX_BLOCK]; CH];
