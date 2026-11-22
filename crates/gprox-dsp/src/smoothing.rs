//! One-pole smoothers shared by the Blue VO!CE blocks (and reusable by NR).
//!
//! Parameter smoothing and the two-speed smoother. Everything is `f32` with
//! the engine's operation grouping, so results track G HUB closely.

/// One-pole coefficient for a time constant of `t_ms` at an update rate of
/// `rate` Hz: `1 − exp(−1 / (rate · t / 1000))`, in `f32`.
///
/// `t_ms ≤ 0` (or NaN) means "no smoothing" and returns 1.
pub fn coef(rate: f32, t_ms: f32) -> f32 {
    // NaN also disables smoothing.
    if t_ms.is_nan() || t_ms <= 0.0 {
        return 1.0;
    }
    let x = rate * t_ms / 1000.0;
    1.0 - (-1.0 / x).exp()
}

/// A per-block parameter smoother: `cur = (1 − k)·cur + k·target`, then a
/// snap to the target once closer than `snap`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamSmoother {
    pub cur: f32,
    pub target: f32,
    snap: f32,
}

impl ParamSmoother {
    /// Starts settled at `value`.
    pub const fn new(value: f32, snap: f32) -> Self {
        ParamSmoother {
            cur: value,
            target: value,
            snap,
        }
    }

    /// Jump to the target (the engine does this on its first block).
    pub fn settle(&mut self) {
        self.cur = self.target;
    }

    /// One step with coefficient `k`. Returns whether `cur` changed.
    pub fn step(&mut self, k: f32) -> bool {
        if self.cur == self.target {
            return false;
        }
        self.cur = (1.0 - k) * self.cur + k * self.target;
        if f64::from((self.cur - self.target).abs()) < f64::from(self.snap) {
            self.cur = self.target;
        }
        true
    }
}

/// Which input direction selects the first ("attack") coefficient of a
/// [`TwoSpeed`] smoother.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttackOn {
    /// Attack when the input falls below `h · y` (gain reduction coming on:
    /// compressor, de-esser, limiter).
    Fall,
    /// Attack when `h · x` rises above `y` (gate opening: expander).
    Rise,
}

/// Two-speed one-pole with hysteresis.
///
/// Parameters `(h, ka, kb)`; per update with input `x`:
/// * [`AttackOn::Fall`]: `k = ka` if `y·h > x`, else `kb`;
/// * [`AttackOn::Rise`]: `k = ka` if `x·h > y`, else `kb`;
///
/// then `y = (1 − k)·y + k·x`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TwoSpeed {
    pub h: f32,
    pub ka: f32,
    pub kb: f32,
    pub dir: AttackOn,
    pub y: f32,
}

impl TwoSpeed {
    pub const fn new(dir: AttackOn, h: f32, ka: f32, kb: f32, y: f32) -> Self {
        TwoSpeed { h, ka, kb, dir, y }
    }

    fn pick(&self, x: f32) -> f32 {
        let attack = match self.dir {
            AttackOn::Fall => self.y * self.h > x,
            AttackOn::Rise => x * self.h > self.y,
        };
        if attack { self.ka } else { self.kb }
    }

    /// Update with `x` as `y = (1 − k)·y + k·x` (the dynamics blocks'
    /// grouping), returning the new state.
    pub fn step(&mut self, x: f32) -> f32 {
        let k = self.pick(x);
        self.y = (1.0 - k) * self.y + k * x;
        self.y
    }

    /// Update with `x` as `y = y + k·(x − y)` (the grouping the NR trackers
    /// need: it does not drift on very long time constants in `f32`).
    pub fn step_delta(&mut self, x: f32) -> f32 {
        let k = self.pick(x);
        self.y += k * (x - self.y);
        self.y
    }
}

/// Per-sample enable cross-fade of one module.
///
/// The engine decides once per processing block whether a fade is running
/// (`target ≠ mix`), moves the mix sample by sample, and at the end of the
/// block snaps within 0.001 of the target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnableFade {
    pub mix: f32,
    pub target: f32,
    /// Latched at block start: a fade runs during this block.
    pub fading: bool,
    /// Latched at block start: the module runs during this block.
    pub active: bool,
}

impl EnableFade {
    pub const fn new() -> Self {
        EnableFade {
            mix: 0.0,
            target: 0.0,
            fading: false,
            active: false,
        }
    }

    /// Block start: latch whether the module runs and whether it fades.
    pub fn begin_block(&mut self, on: bool) {
        self.target = if on { 1.0 } else { 0.0 };
        self.active = on || self.mix > 0.0;
        self.fading = self.active && self.target != self.mix;
    }

    /// Advance one sample with coefficient `k`; returns the mix to use.
    #[inline]
    pub fn next(&mut self, one_minus_k: f32, target_k: f32) -> f32 {
        self.mix = one_minus_k * self.mix + target_k;
        self.mix
    }

    /// Block end. `reference` is the value the snap compares against (the
    /// target, except for the voice EQ's quirk). Returns `true` when the
    /// module just snapped to off and its state must be cleared.
    pub fn end_block(&mut self, reference: f32) -> bool {
        if !self.fading {
            return false;
        }
        if f64::from((reference - self.mix).abs()) < 0.001 {
            self.mix = self.target;
            return self.target == 0.0;
        }
        false
    }
}

impl Default for EnableFade {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coef_matches_the_stored_engine_values() {
        // Params block of the Broadcaster 1 state.
        assert!((coef(1500.0, 2.0) - 0.283_469).abs() < 1e-6);
        assert!((coef(1500.0, 250.0) - 0.002_663_14).abs() < 1e-8);
        assert_eq!(coef(1500.0, 0.0), 1.0);
        assert_eq!(coef(1500.0, f32::NAN), 1.0);
    }

    #[test]
    fn two_speed_directions() {
        let mut c = TwoSpeed::new(AttackOn::Fall, 1.0, 1.0, 0.0, 1.0);
        assert_eq!(c.step(0.5), 0.5); // falls → attack (k = 1)
        assert_eq!(c.step(1.0), 0.5); // rises → release (k = 0)
        let mut e = TwoSpeed::new(AttackOn::Rise, 1.0, 1.0, 0.0, 0.5);
        assert_eq!(e.step(1.0), 1.0);
        assert_eq!(e.step(0.1), 1.0);
    }

    #[test]
    fn smoother_snaps() {
        let mut p = ParamSmoother::new(0.0, 0.01);
        p.target = 1.0;
        let mut n = 0;
        while p.step(0.5) {
            n += 1;
        }
        assert_eq!(p.cur, 1.0);
        assert_eq!(n, 7);
    }
}
