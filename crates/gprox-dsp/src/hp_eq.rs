//! G HUB headphone EQ (PRO X Wireless render path).
//!
//! * One 12-section biquad cascade, always running: sections 0–9 are the
//!   "advanced" 10-band EQ, 10–11 the "basic" 2-band EQ. The mode only picks
//!   which set gets its stored gains; the other set targets 0 dB.
//! * Fixed band table, RBJ design in `f64` rounded to `f32` taps
//!   ([`crate::biquad`]).
//! * Gains ramp linearly in dB over 100 ms. The ramp advances once per ramp
//!   block (G HUB: one 480-frame APO call at 48 kHz) and the taps of the
//!   changed bands are recomputed before that block is filtered. A new stream
//!   starts every band at 0 dB and ramps to the stored gains.
//!   The ramp arithmetic is `f32`; checking the snap before adding the step
//!   reproduces G HUB's hooked tap sequences bit for bit.
//! * The cascade runs G HUB's IPP `f32` kernel ([`IppF32`]) by
//!   default. Its output depends on the block grid, so G HUB's block (the
//!   ramp block, 480 frames at 48 kHz from stream start) is the processing
//!   grid too. The grid is independent of the host quantum: [`HeadphoneEq`]
//!   splits host buffers at block boundaries and the kernel streams within a
//!   block, so the output is bit-identical for any PipeWire quantum and no
//!   latency is added. With the defaults the output is bit-exact with G HUB.
//!
//! Real-time notes: [`HeadphoneEq::process_in_place`] does no allocation,
//! locking, syscalls or logging. Parameter changes arrive through a
//! [`crate::shared`] sequence lock. Taps are recomputed on the RT thread only
//! while a ramp is running (at most 12 `f64` designs per ramp block, pure
//! math); that is inherent to a per-block ramp of designed coefficients and
//! matches what G HUB does.

use std::marker::PhantomData;
use std::num::NonZeroUsize;

use crate::biquad::{Shape, Taps, design_rbj_bw};
use crate::kernel::{CascadeKernel, IppF32, MAX_STATE_PER_SECTION};
use crate::shared::{ParamReader, ParamWriter, param_cell};
use crate::{MAX_CHANNELS, Processor};

/// Number of sections in the cascade.
pub const BANDS: usize = 12;
/// Sections of the advanced (10-band graphic) EQ.
pub const ADVANCED_BANDS: std::ops::Range<usize> = 0..10;
/// Sections of the basic (bass/treble) EQ.
pub const BASIC_BANDS: std::ops::Range<usize> = 10..12;
/// Gains with a larger magnitude are ignored per band.
pub const MAX_GAIN_DB: f32 = 12.0;
/// Ramp time for gain changes.
pub const RAMP_MS: f32 = 100.0;
/// G HUB's ramp/APO block at 48 kHz.
pub const GHUB_BLOCK_FRAMES_48K: usize = 480;

/// One section of the fixed band table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandDef {
    pub shape: Shape,
    /// Centre/corner frequency in Hz, as the `f32` G HUB stores.
    pub f0: f32,
    /// Bandwidth in octaves, as the `f32` G HUB stores.
    pub bw: f32,
}

const fn band(shape: Shape, f0: f32, bw: f32) -> BandDef {
    BandDef { shape, f0, bw }
}

/// The hard-coded band table, cascade order. The top advanced
/// band is labelled "16 kHz" in the UI; its stored corner is 16000/√2 as
/// `f32`, 11313.7001953125 Hz.
pub const BAND_TABLE: [BandDef; BANDS] = [
    band(Shape::Peaking, 32.0, 0.7),
    band(Shape::Peaking, 64.0, 0.7),
    band(Shape::Peaking, 125.0, 0.7),
    band(Shape::Peaking, 250.0, 0.7),
    band(Shape::Peaking, 500.0, 0.7),
    band(Shape::Peaking, 1000.0, 0.7),
    band(Shape::Peaking, 2000.0, 0.7),
    band(Shape::Peaking, 4000.0, 0.7),
    band(Shape::Peaking, 8000.0, 0.7),
    // Exact f32 value of 16000/√2 as stored by G HUB (= 11313.7f32).
    #[allow(clippy::excessive_precision)]
    band(Shape::HighShelf, 11_313.700_195_312_5, 0.7),
    band(Shape::LowShelf, 200.0, 1.0),
    band(Shape::HighShelf, 4500.0, 1.0),
];

/// Which gain set is active. G HUB's `SelectedEQ`: 0 = basic, 1 = advanced;
/// missing means basic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EqMode {
    #[default]
    Basic,
    Advanced,
}

/// User-facing EQ settings in dB (G HUB's `BasicEqParams`/`AdvancedEqParams`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HpEqSettings {
    pub mode: EqMode,
    /// 200 Hz low shelf, 4.5 kHz high shelf.
    pub basic: [f32; 2],
    /// 32, 64, 125, 250, 500, 1k, 2k, 4k, 8k, "16k" (high shelf).
    pub advanced: [f32; 10],
}

impl HpEqSettings {
    /// Flat (all 0 dB) in the given mode.
    pub fn flat(mode: EqMode) -> Self {
        HpEqSettings {
            mode,
            ..Default::default()
        }
    }

    /// Per-section target gains for these settings: the active set's gains,
    /// 0 dB for the other set.
    pub fn targets(&self) -> [f32; BANDS] {
        let mut t = [0.0; BANDS];
        match self.mode {
            EqMode::Advanced => t[ADVANCED_BANDS].copy_from_slice(&self.advanced),
            EqMode::Basic => t[BASIC_BANDS].copy_from_slice(&self.basic),
        }
        t
    }

    const WORDS: usize = 13;

    fn to_words(self) -> [u32; Self::WORDS] {
        let mut w = [0u32; Self::WORDS];
        w[0] = match self.mode {
            EqMode::Basic => 0,
            EqMode::Advanced => 1,
        };
        for (d, g) in w[1..3].iter_mut().zip(self.basic) {
            *d = g.to_bits();
        }
        for (d, g) in w[3..].iter_mut().zip(self.advanced) {
            *d = g.to_bits();
        }
        w
    }

    fn from_words(w: &[u32; Self::WORDS]) -> Self {
        let mut s = HpEqSettings {
            mode: if w[0] == 1 {
                EqMode::Advanced
            } else {
                EqMode::Basic
            },
            ..Default::default()
        };
        for (g, &b) in s.basic.iter_mut().zip(&w[1..3]) {
            *g = f32::from_bits(b);
        }
        for (g, &b) in s.advanced.iter_mut().zip(&w[3..]) {
            *g = f32::from_bits(b);
        }
        s
    }
}

/// Steady-state taps for `settings` at `fs`: what the cascade runs once every
/// ramp has finished. Same design as the DSP ([`design_rbj_bw`] from
/// [`BAND_TABLE`]); gains outside ±12 dB (or not finite) read as 0 dB, and a
/// band above `fs / 2.2` stays a pass-through, as in [`HpEqControl`].
pub fn steady_state_taps(settings: &HpEqSettings, fs: u32) -> [Taps; BANDS] {
    let targets = settings.targets();
    std::array::from_fn(|k| {
        let b = &BAND_TABLE[k];
        let g = targets[k];
        let g = if g.is_finite() && g.abs() <= MAX_GAIN_DB {
            g
        } else {
            0.0
        };
        design_rbj_bw(
            b.shape,
            f64::from(b.f0),
            f64::from(g),
            f64::from(b.bw),
            f64::from(fs as f32),
        )
        .unwrap_or(Taps::IDENTITY)
    })
}

/// Magnitude response of the whole cascade in dB at `f` Hz (for curve views).
pub fn cascade_magnitude_db(taps: &[Taps; BANDS], f: f64, fs: u32) -> f64 {
    taps.iter().map(|t| t.magnitude_db(f, f64::from(fs))).sum()
}

/// G HUB's processing block: the step of the gain ramp and the block grid of
/// the IPP kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RampBlock {
    /// 10 ms at the stream rate (480 frames at 48 kHz), like G HUB's APO.
    /// Required for bit-exactness.
    #[default]
    Auto,
    /// A fixed number of frames per block.
    Frames(NonZeroUsize),
}

impl RampBlock {
    fn frames(self, fs: u32) -> usize {
        match self {
            RampBlock::Auto => (fs as usize / 100).max(1),
            RampBlock::Frames(n) => n.get(),
        }
    }
}

/// Linear-in-dB gain ramp of one band (`f32` arithmetic, as G HUB).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct GainRamp {
    current: f32,
    target: f32,
    /// dB per sample.
    step: f32,
}

impl GainRamp {
    fn retarget(&mut self, target: f32, ramp_samples: f32) {
        self.target = target;
        self.step = (target - self.current) / ramp_samples;
    }

    /// Advance by one block. Returns whether the gain changed.
    fn tick(&mut self, frames: f32) -> bool {
        if self.current == self.target {
            return false;
        }
        let inc = self.step * frames;
        // Snap when less than one block's step remains (checked before
        // adding, which yields G HUB's extra "snap" tap push), or if the
        // step underflowed to zero.
        if (self.target - self.current).abs() < inc.abs() || inc == 0.0 {
            self.current = self.target;
        } else {
            self.current += inc;
        }
        true
    }
}

/// Control state of the EQ: stored settings, per-band ramps and the current
/// taps. Deterministic and free of audio; [`HeadphoneEq`] drives it once per
/// ramp block.
#[derive(Debug, Clone)]
pub struct HpEqControl {
    fs: u32,
    ramp_block: RampBlock,
    stored: HpEqSettings,
    ramps: [GainRamp; BANDS],
    taps: [Taps; BANDS],
    taps_version: u64,
}

impl HpEqControl {
    /// New control state for a stream at `fs`, starting at 0 dB and ramping
    /// to `settings` (out-of-range gains in `settings` read as 0 dB).
    pub fn new(fs: u32, ramp_block: RampBlock, settings: &HpEqSettings) -> Self {
        let mut c = HpEqControl {
            fs,
            ramp_block,
            stored: HpEqSettings::flat(settings.mode),
            ramps: [GainRamp::default(); BANDS],
            taps: [Taps::IDENTITY; BANDS],
            taps_version: 0,
        };
        c.accept(settings);
        c.start_stream();
        c
    }

    /// Sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.fs
    }

    /// Frames per ramp step at the current rate.
    pub fn ramp_frames(&self) -> usize {
        self.ramp_block.frames(self.fs)
    }

    fn ramp_samples(&self) -> f32 {
        self.fs as f32 * RAMP_MS / 1000.0
    }

    /// Change the sample rate and start a new stream.
    pub fn set_sample_rate(&mut self, fs: u32) {
        self.fs = fs;
        self.start_stream();
    }

    /// Start a new stream: every band at 0 dB, ramping to the stored gains
    /// from the first block on.
    pub fn start_stream(&mut self) {
        let rs = self.ramp_samples();
        let targets = self.stored.targets();
        for (r, &t) in self.ramps.iter_mut().zip(&targets) {
            r.current = 0.0;
            r.retarget(t, rs);
        }
        for k in 0..BANDS {
            self.taps[k] = Taps::IDENTITY;
            self.redesign(k);
        }
        self.taps_version += 1;
    }

    /// Take over new settings like G HUB's registry handlers: a band changes
    /// only if it differs from the stored value by more than `f32::EPSILON`
    /// and `|gain| ≤ 12 dB`; other bands keep their old value. Bands whose
    /// target changed start a new 100 ms ramp from their current gain.
    /// Returns whether any target changed.
    pub fn apply(&mut self, settings: &HpEqSettings) -> bool {
        let before = self.stored.targets();
        self.accept(settings);
        let after = self.stored.targets();
        let rs = self.ramp_samples();
        let mut changed = false;
        for k in 0..BANDS {
            if after[k].to_bits() != before[k].to_bits() {
                self.ramps[k].retarget(after[k], rs);
                changed = true;
            }
        }
        changed
    }

    fn accept(&mut self, s: &HpEqSettings) {
        fn take(dst: &mut [f32], src: &[f32]) {
            for (d, &g) in dst.iter_mut().zip(src) {
                if (g - *d).abs() > f32::EPSILON && g.abs() <= MAX_GAIN_DB {
                    *d = g;
                }
            }
        }
        take(&mut self.stored.basic, &s.basic);
        take(&mut self.stored.advanced, &s.advanced);
        self.stored.mode = s.mode;
    }

    /// Advance the ramps by one ramp block and recompute the taps of the
    /// bands that moved. Returns whether the taps were (re)computed, i.e.
    /// whether G HUB would push new taps for this block.
    pub fn tick(&mut self) -> bool {
        let frames = self.ramp_frames() as f32;
        let mut any = false;
        for k in 0..BANDS {
            if self.ramps[k].tick(frames) {
                self.redesign(k);
                any = true;
            }
        }
        if any {
            self.taps_version += 1;
        }
        any
    }

    fn redesign(&mut self, k: usize) {
        let b = &BAND_TABLE[k];
        if let Some(t) = design_rbj_bw(
            b.shape,
            f64::from(b.f0),
            f64::from(self.ramps[k].current),
            f64::from(b.bw),
            f64::from(self.fs as f32),
        ) {
            self.taps[k] = t;
        }
    }

    /// Taps in force, cascade order.
    pub fn taps(&self) -> &[Taps; BANDS] {
        &self.taps
    }

    /// Increments whenever the taps are recomputed (stream start or a ramp step).
    pub fn taps_version(&self) -> u64 {
        self.taps_version
    }

    /// Current (ramping) gain per section in dB.
    pub fn current_gains(&self) -> [f32; BANDS] {
        self.ramps.map(|r| r.current)
    }

    /// Target gain per section in dB.
    pub fn target_gains(&self) -> [f32; BANDS] {
        self.ramps.map(|r| r.target)
    }

    /// The accepted (stored) settings.
    pub fn settings(&self) -> &HpEqSettings {
        &self.stored
    }

    /// Whether any band is still ramping.
    pub fn is_ramping(&self) -> bool {
        self.ramps.iter().any(|r| r.current != r.target)
    }
}

/// Control-side handle that sends settings to a running [`HeadphoneEq`].
#[derive(Debug)]
pub struct HpEqHandle {
    writer: ParamWriter<{ HpEqSettings::WORDS }>,
}

impl HpEqHandle {
    /// Publish new settings; the EQ picks them up at its next block. Wait-free.
    pub fn set(&mut self, settings: &HpEqSettings) {
        self.writer.write(&settings.to_words());
    }
}

const STATE_WORDS: usize = BANDS * MAX_STATE_PER_SECTION;

/// The headphone EQ as a [`Processor`], generic over the biquad kernel
/// (default [`IppF32`], bit-exact with G HUB).
#[derive(Debug)]
pub struct HeadphoneEq<K: CascadeKernel = IppF32> {
    ctl: HpEqControl,
    state: [[f32; STATE_WORDS]; MAX_CHANNELS],
    /// Frames left before the next ramp tick; 0 = tick before the next sample.
    until_tick: usize,
    reader: Option<ParamReader<{ HpEqSettings::WORDS }>>,
    _kernel: PhantomData<fn() -> K>,
}

impl<K: CascadeKernel> HeadphoneEq<K> {
    /// EQ at 48 kHz with G HUB's 10 ms ramp steps, starting a stream that
    /// ramps from 0 dB to `settings`. [`Processor::prepare`] sets the real rate.
    pub fn new(settings: &HpEqSettings) -> Self {
        Self::with_ramp_block(settings, RampBlock::Auto)
    }

    /// Like [`HeadphoneEq::new`] with an explicit ramp granularity.
    pub fn with_ramp_block(settings: &HpEqSettings, ramp_block: RampBlock) -> Self {
        const { assert!(K::STATE_LEN <= MAX_STATE_PER_SECTION) };
        HeadphoneEq {
            ctl: HpEqControl::new(48_000, ramp_block, settings),
            state: [[0.0; STATE_WORDS]; MAX_CHANNELS],
            until_tick: 0,
            reader: None,
            _kernel: PhantomData,
        }
    }

    /// An EQ plus a handle for live parameter changes from another thread.
    pub fn new_shared(settings: &HpEqSettings) -> (Self, HpEqHandle) {
        let (writer, reader) = param_cell(settings.to_words());
        let mut eq = Self::new(settings);
        eq.reader = Some(reader);
        (eq, HpEqHandle { writer })
    }

    /// Start a new stream: zero filter state, all bands back to 0 dB and
    /// ramping to the stored settings, ramp phase reset.
    pub fn reset_stream(&mut self) {
        self.ctl.start_stream();
        self.state = [[0.0; STATE_WORDS]; MAX_CHANNELS];
        self.until_tick = 0;
    }

    /// Apply settings directly (same rules as G HUB's registry handlers).
    /// For use on the thread that owns the EQ; live changes from another
    /// thread go through [`HpEqHandle`].
    pub fn set_settings(&mut self, settings: &HpEqSettings) -> bool {
        self.ctl.apply(settings)
    }

    /// Control state (gains, taps).
    pub fn control(&self) -> &HpEqControl {
        &self.ctl
    }

    /// Kernel state of one channel (`BANDS * K::STATE_LEN` words; section
    /// `i` owns `i * K::STATE_LEN..`, of which the first `K::CARRIED_STATE`
    /// words are the carried filter state). For diagnostics and tests that
    /// start from a known filter state at a block boundary.
    pub fn state_mut(&mut self, ch: usize) -> &mut [f32] {
        &mut self.state[ch][..BANDS * K::STATE_LEN]
    }

    /// Filter planar channels in place. Channels beyond [`MAX_CHANNELS`] are
    /// left untouched. Real-time safe.
    pub fn process_in_place(&mut self, chans: &mut [&mut [f32]]) {
        if let Some(w) = self.reader.as_mut().and_then(ParamReader::poll) {
            self.ctl.apply(&HpEqSettings::from_words(&w));
        }
        let frames = chans.iter().map(|c| c.len()).min().unwrap_or(0);
        let nst = BANDS * K::STATE_LEN;
        let mut pos = 0;
        while pos < frames {
            if self.until_tick == 0 {
                self.ctl.tick();
                self.until_tick = self.ctl.ramp_frames();
            }
            let n = self.until_tick.min(frames - pos);
            let block = self.ctl.ramp_frames();
            let block_pos = block - self.until_tick;
            let taps = &self.ctl.taps;
            for (buf, st) in chans.iter_mut().zip(self.state.iter_mut()) {
                K::process(
                    taps,
                    &mut st[..nst],
                    &mut buf[pos..pos + n],
                    block_pos,
                    block,
                );
            }
            pos += n;
            self.until_tick -= n;
        }
    }
}

impl<K: CascadeKernel> Processor for HeadphoneEq<K> {
    fn prepare(&mut self, sample_rate: u32, _max_frames: usize) {
        self.ctl.fs = sample_rate;
        self.reset_stream();
    }

    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        for (ch, out) in output.iter_mut().enumerate() {
            match input.get(ch) {
                Some(inp) => {
                    let n = out.len().min(inp.len());
                    out[..n].copy_from_slice(&inp[..n]);
                    out[n..].fill(0.0);
                }
                None => out.fill(0.0),
            }
        }
        self.process_in_place(output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adv(k: usize, g: f32) -> HpEqSettings {
        let mut s = HpEqSettings::flat(EqMode::Advanced);
        s.advanced[k] = g;
        s
    }

    /// Run the control for `blocks` ramp blocks, returning the gains after
    /// each block that produced new taps.
    fn ramp_log(c: &mut HpEqControl, blocks: usize) -> Vec<[f32; BANDS]> {
        let mut log = Vec::new();
        for _ in 0..blocks {
            if c.tick() {
                log.push(c.current_gains());
            }
        }
        log
    }

    #[test]
    fn steady_state_taps_match_the_converged_control() {
        let mut s = HpEqSettings::flat(EqMode::Advanced);
        s.advanced = [3.0, -2.0, 0.0, 5.5, 0.0, -12.0, 1.0, 0.0, 7.0, 13.0];
        s.basic = [6.0, -6.0];
        let mut c = HpEqControl::new(48_000, RampBlock::Auto, &s);
        ramp_log(&mut c, 50);
        assert!(!c.is_ramping());
        let t = steady_state_taps(&s, 48_000);
        assert!(t.iter().zip(c.taps()).all(|(a, b)| a.bit_eq(b)));
        // +7 dB peak at 8 kHz; the out-of-range 13 dB band reads as 0 dB.
        let at8k = cascade_magnitude_db(&t, 8000.0, 48_000);
        assert!((at8k - 7.0).abs() < 0.5, "{at8k}");
        let flat = steady_state_taps(&HpEqSettings::flat(EqMode::Basic), 48_000);
        assert!(cascade_magnitude_db(&flat, 1000.0, 48_000).abs() < 1e-9);
    }

    #[test]
    fn band_table_constants_are_the_stored_f32_values() {
        assert_eq!(f64::from(BAND_TABLE[9].f0), 11_313.700_195_312_5);
        assert_eq!(f64::from(BAND_TABLE[0].bw), 0.699_999_988_079_071);
    }

    #[test]
    fn stream_start_ramps_in_ten_steps_plus_snap() {
        let mut c = HpEqControl::new(48_000, RampBlock::Auto, &adv(5, 6.0));
        assert_eq!(c.current_gains(), [0.0; BANDS]);
        let log = ramp_log(&mut c, 30);
        assert_eq!(log.len(), 11);
        for (i, g) in log.iter().enumerate().take(10) {
            assert!(
                (g[5] - 0.6 * (i + 1) as f32).abs() < 1e-5,
                "step {i}: {}",
                g[5]
            );
        }
        assert_eq!(log[10][5], 6.0);
        assert!(!c.is_ramping());
    }

    #[test]
    fn out_of_range_gains_are_ignored_per_band() {
        let mut c = HpEqControl::new(48_000, RampBlock::Auto, &adv(0, 3.0));
        let mut s = adv(0, 12.5);
        s.advanced[1] = -12.0;
        c.apply(&s);
        assert_eq!(c.settings().advanced[0], 3.0);
        assert_eq!(c.settings().advanced[1], -12.0);
        s.advanced[0] = f32::NAN;
        c.apply(&s);
        assert_eq!(c.settings().advanced[0], 3.0);
    }

    #[test]
    fn mode_switch_ramps_both_sets() {
        let mut s = adv(0, 12.0);
        s.basic = [6.0, -6.0];
        let mut c = HpEqControl::new(48_000, RampBlock::Auto, &s);
        ramp_log(&mut c, 20);
        assert_eq!(c.current_gains()[0], 12.0);
        assert_eq!(c.current_gains()[10], 0.0);
        s.mode = EqMode::Basic;
        assert!(c.apply(&s));
        let log = ramp_log(&mut c, 20);
        assert_eq!(log.len(), 11);
        assert_eq!(c.current_gains()[0], 0.0);
        assert_eq!(c.current_gains()[10..], [6.0, -6.0]);
    }

    #[test]
    fn unchanged_settings_do_not_restart_a_ramp() {
        let mut c = HpEqControl::new(48_000, RampBlock::Auto, &adv(5, 6.0));
        ramp_log(&mut c, 20);
        assert!(!c.apply(&adv(5, 6.0 + f32::EPSILON / 2.0)));
        assert!(!c.tick());
    }

    #[test]
    fn taps_only_change_while_ramping() {
        let mut c = HpEqControl::new(48_000, RampBlock::Auto, &HpEqSettings::default());
        let v = c.taps_version();
        assert!(!c.tick());
        assert_eq!(c.taps_version(), v);
    }

    #[test]
    fn band_above_fs_over_2_2_keeps_its_previous_taps() {
        // At 22.05 kHz the 11.3 kHz shelf is rejected: it stays at identity.
        let mut c = HpEqControl::new(22_050, RampBlock::Auto, &adv(9, 6.0));
        ramp_log(&mut c, 20);
        assert_eq!(c.taps()[9], Taps::IDENTITY);
        assert_eq!(c.current_gains()[9], 6.0);
    }

    fn signal(frames: usize, ch: usize) -> Vec<Vec<f32>> {
        (0..ch)
            .map(|c| {
                (0..frames)
                    .map(|i| ((i * (c + 1)) as f32 * 0.003).sin() * 0.5)
                    .collect()
            })
            .collect()
    }

    fn run(eq: &mut HeadphoneEq, sig: &[Vec<f32>], quantum: usize) -> Vec<Vec<f32>> {
        let mut out = sig.to_vec();
        let len = sig[0].len();
        let mut pos = 0;
        while pos < len {
            let n = quantum.min(len - pos);
            let mut chans: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[pos..pos + n]).collect();
            eq.process_in_place(&mut chans);
            pos += n;
        }
        out
    }

    #[test]
    fn output_does_not_depend_on_the_host_quantum() {
        let sig = signal(48_000, 2);
        let s = adv(1, 9.0);
        let mut a = HeadphoneEq::new(&s);
        a.prepare(48_000, 8192);
        let ref_out = run(&mut a, &sig, 480);
        for q in [64, 256, 1000, 1024] {
            let mut b = HeadphoneEq::new(&s);
            b.prepare(48_000, 8192);
            assert_eq!(run(&mut b, &sig, q), ref_out, "quantum {q}");
        }
    }

    #[test]
    fn shared_handle_delivers_settings_at_the_next_block() {
        let (mut eq, mut h) = HeadphoneEq::<IppF32>::new_shared(&HpEqSettings::default());
        eq.prepare(48_000, 8192);
        let mut buf = vec![0.0f32; 480];
        eq.process_in_place(&mut [&mut buf]);
        assert_eq!(eq.control().target_gains(), [0.0; BANDS]);
        h.set(&adv(5, -6.0));
        eq.process_in_place(&mut [&mut buf]);
        assert_eq!(eq.control().target_gains()[5], -6.0);
        assert!((eq.control().current_gains()[5] + 0.6).abs() < 1e-6);
    }

    #[test]
    fn processor_zero_fills_missing_input() {
        let mut eq = HeadphoneEq::<IppF32>::new(&HpEqSettings::default());
        eq.prepare(48_000, 8192);
        let inp = [0.25f32; 4];
        let mut a = [1.0f32; 6];
        let mut b = [1.0f32; 6];
        eq.process(&[&inp], &mut [&mut a, &mut b]);
        assert_eq!(b, [0.0; 6]);
        // Flat is 12 sections at 0 dB: transparent up to f32 rounding.
        assert!(a[..4].iter().all(|&v| (v - 0.25).abs() < 1e-6), "{a:?}");
        assert!(a[4..].iter().all(|&v| v.abs() < 1e-6), "{a:?}");
    }
}
