//! Sub-block machinery shared by the Blue VO!CE dynamics blocks: the
//! adaptive peak-hold detector, the
//! sub-block look-ahead delay and the per-sub-block linear gain ramp.
//!
//! All of it is fixed-size (no allocation) so it can live in RT state.

/// Longest detector ring the engine allows.
pub const MAX_RING: usize = 150;
/// Largest sub-block (176.4/192 kHz).
pub const MAX_SUB: usize = 128;
/// Look-ahead slots per channel (4 sub-blocks).
pub const SLOTS: usize = 4;

/// Peak magnitude over the first `n` frames of every channel.
pub fn peak(chans: &[&[f32]], n: usize) -> f32 {
    let mut p = 0.0f32;
    for c in chans {
        for &x in &c[..n] {
            let a = x.abs();
            if a >= p {
                p = a;
            }
        }
    }
    p
}

/// Ring size for a detector: `clamp(⌊⌊(fs / sub) / 2⌋ / div⌋, 1, 150)`
/// (compressor, expander and limiter use `div = 20` → 37 at 48 kHz; the
/// de-esser uses 2000 → 1, i.e. just the current sub-block peak).
pub fn ring_len(fs: u32, sub: usize, div: u32) -> usize {
    let r = (fs as usize / sub) / 2 / div as usize;
    r.clamp(1, MAX_RING)
}

/// Adaptive peak-hold detector over a ring of past sub-block peaks.
///
/// Per sub-block the new peak goes into the ring. A crest test over the ring
/// (`M·max` against 3, 2.75 and 2.5 times the ring sum) shrinks the hold window
/// to 2, 4 or 8 sub-blocks after a transient; otherwise the window grows by one
/// sub-block per update until it wraps to 0, which means "the whole ring".
#[derive(Debug, Clone)]
pub struct PeakHold {
    idx: usize,
    len: usize,
    win: usize,
    ring: [f32; MAX_RING],
}

impl PeakHold {
    pub fn new(len: usize) -> Self {
        PeakHold {
            idx: 0,
            len: len.clamp(1, MAX_RING),
            win: 0,
            ring: [0.0; MAX_RING],
        }
    }

    /// Clear the history (keeps the ring length).
    pub fn reset(&mut self) {
        *self = Self::new(self.len);
    }

    /// Push the sub-block peak `p` and return the held level.
    pub fn update(&mut self, p: f32) -> f32 {
        let m = self.len;
        self.ring[self.idx] = p;
        self.idx += 1;
        if self.idx >= m {
            self.idx = 0;
        }
        let mut pk = p;
        let mut sum = 0.0f32;
        for &v in &self.ring[..m] {
            if v > pk {
                pk = v;
            }
            sum += v;
        }
        let mpk = m as f32 * pk;
        if f64::from(mpk) > f64::from(sum) * 3.0 {
            self.win = 2;
        } else if mpk > sum * 2.75 {
            self.win = 4;
        } else if mpk > sum * 2.5 {
            self.win = 8;
        }
        if self.win == 0 {
            return pk;
        }
        let w = self.win.min(m);
        let mut held = 0.0f32;
        let mut j = self.idx;
        for _ in 0..w {
            j = if j == 0 { m - 1 } else { j - 1 };
            if self.ring[j] > held {
                held = self.ring[j];
            }
        }
        self.win = if w + 1 >= m { 0 } else { w + 1 };
        held
    }

    /// Maximum of the `n` most recent ring entries (the limiter's look-ahead
    /// window, `lookahead + 1` sub-blocks).
    pub fn recent_max(&self, n: usize) -> f32 {
        let m = self.len;
        let mut best = 0.0f32;
        let mut j = self.idx;
        for _ in 0..n {
            j = if j == 0 { m - 1 } else { j - 1 };
            if self.ring[j] > best {
                best = self.ring[j];
            }
        }
        best
    }
}

/// Look-ahead delay of `SLOTS` sub-blocks per channel, combined with the
/// linear gain ramp of one sub-block.
///
/// For each sample `i`: `out = g · slot[i]; slot[i] = in; g += inc`, where
/// `g` starts at `start` for every channel. The slot read is the one written
/// `SLOTS` sub-blocks ago, so the delay is `SLOTS · sub` samples.
#[derive(Debug, Clone)]
pub struct SubBlockDelay<const CH: usize> {
    buf: [[[f32; MAX_SUB]; SLOTS]; CH],
    slot: usize,
    slots: usize,
}

impl<const CH: usize> SubBlockDelay<CH> {
    pub fn new(slots: usize) -> Self {
        SubBlockDelay {
            buf: [[[0.0; MAX_SUB]; SLOTS]; CH],
            slot: 0,
            slots: slots.clamp(1, SLOTS),
        }
    }

    pub fn reset(&mut self) {
        self.buf = [[[0.0; MAX_SUB]; SLOTS]; CH];
        self.slot = 0;
    }

    /// Apply the ramp to the delayed signal in place and store the new input.
    pub fn ramp_through(&mut self, chans: &mut [&mut [f32]], n: usize, start: f32, inc: f32) {
        for (c, x) in chans.iter_mut().enumerate().take(CH) {
            let s = &mut self.buf[c][self.slot];
            let mut g = start;
            for (xi, si) in x[..n].iter_mut().zip(s[..n].iter_mut()) {
                let d = *si;
                *si = *xi;
                *xi = g * d;
                g += inc;
            }
        }
        self.slot += 1;
        if self.slot >= self.slots {
            self.slot = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_lengths() {
        assert_eq!(ring_len(48_000, 32, 20), 37);
        assert_eq!(ring_len(48_000, 32, 2000), 1);
        assert_eq!(ring_len(96_000, 64, 20), 37);
    }

    #[test]
    fn single_slot_detector_is_the_sub_block_peak() {
        let mut d = PeakHold::new(1);
        for p in [0.1, 0.5, 0.2, 0.0, 0.3] {
            assert_eq!(d.update(p), p);
        }
    }

    #[test]
    fn transient_shrinks_window_then_it_grows() {
        let mut d = PeakHold::new(37);
        for _ in 0..37 {
            d.update(0.01);
        }
        assert_eq!(d.update(1.0), 1.0); // crest → window 2
        assert_eq!(d.update(0.01), 1.0); // still inside the 2-block window
        // The crest test still fires (the peak is in the ring), so the window
        // is reset to 2 and the transient has left it.
        assert_eq!(d.update(0.01), 0.01);
    }

    #[test]
    fn delay_is_four_sub_blocks() {
        let mut d = SubBlockDelay::<1>::new(4);
        let mut out = Vec::new();
        for b in 0..6 {
            let mut x = [b as f32 + 1.0; 32];
            d.ramp_through(&mut [&mut x], 32, 1.0, 0.0);
            out.push(x[0]);
        }
        assert_eq!(out, [0.0, 0.0, 0.0, 0.0, 1.0, 2.0]);
    }
}
