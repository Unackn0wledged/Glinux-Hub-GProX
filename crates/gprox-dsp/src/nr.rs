//! Blue VO!CE noise reduction.
//!
//! Written from our own specification, no decompiled code. A 32-band
//! spectral-gain noise reducer:
//!
//! * STFT with N = 1024, hop 512, a sine window on analysis and synthesis
//!   (`sin(π(n + ½)/N)`, so `w² + w²(shifted)` = 1 and the weighted
//!   overlap-add reconstructs exactly at unity gain).
//! * Per frame and channel: 32 band powers over a fixed triangular mel
//!   filterbank ([`BandTable::mel`]), converted to dB relative to a full-scale
//!   sine (`10·log10(P) − 20·log10(N/2)`), floored at −144 dB.
//! * Two two-speed trackers per band follow the band level: a slow average
//!   (rise 5 s, fall 1.25 s) and a floor tracker (rise 5 s, fall 50 ms).
//! * Noise estimate `e = min((avg − floor)·bias, sensitivity) + floor`. A band
//!   whose level is above `e` gets an attenuation of `(e − L) − 1` dB (always
//!   ≤ −1 dB), otherwise 0 dB. The applied value is the minimum of this frame's
//!   and the previous frame's (a one-frame hold for onsets).
//! * Target band gain `x = 1 − 10^(att/20)·(1 − 10^(attenuation_max/20))`, so a
//!   band judged to be pure noise (att = 0) gets the full `attenuation_max`
//!   reduction and a band clearly above the estimate gets close to 1.
//! * Two gain smoothers (fast: rise instantly, fall at 0.2·release; slow: rise
//!   in 100 ms, fall at release) are combined with `min`.
//! * Band gains are spread to the 512 bins with the same filterbank weights
//!   (they sum to 1 in every bin); the Nyquist bin is left untouched.
//!
//! All time constants are one-pole coefficients `1 − exp(−1000/(92·t_ms))` at
//! the engine's NR control rate of 92 Hz (48 kHz).
//!
//! Timing: G HUB runs the engine on 256-frame blocks and starts the input
//! buffer 768 samples deep, so the NR adds exactly [`LATENCY`] = 768 samples.
//! [`NrTiming::Engine`] reproduces that and needs every `process` call to keep
//! to the 256-frame grid (any multiple of 256 works). [`NrTiming::AnyQuantum`]
//! accepts any buffer size at the price of 256 more samples of latency.
//!
//! Enable/disable is a cross-fade in the engine and belongs to the mic chain,
//! not to this block.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use crate::shared::{ParamReader, ParamWriter, param_cell};
use crate::{MAX_CHANNELS, Processor};

/// STFT frame length.
pub const FRAME: usize = 1024;
/// Hop between frames (50 % overlap).
pub const HOP: usize = FRAME / 2;
/// Bins that get a gain (0 … N/2 − 1; the Nyquist bin is passed through).
pub const BINS: usize = FRAME / 2;
/// Number of bands.
pub const BANDS: usize = 32;
/// G HUB's engine block. The NR's input buffer starts `FRAME − ENGINE_BLOCK`
/// samples deep, which sets the latency.
pub const ENGINE_BLOCK: usize = 256;
/// Latency of the block in [`NrTiming::Engine`] mode, in samples.
pub const LATENCY: usize = FRAME - ENGINE_BLOCK;

/// Longest band of the filterbank, in bins (the top band, 412 … 511).
pub const MAX_BAND_LEN: usize = 100;

/// NR control rate at 48 kHz (frames per second as the engine counts them).
const CONTROL_RATE_48K: f64 = 92.0;
/// Band levels are floored here (dB).
const LEVEL_FLOOR_DB: f32 = -144.0;
/// Start value of both level trackers (dB).
const TRACKER_INIT_DB: f32 = -60.0;
/// Fixed tracker time constants (ms).
const AVG_RISE_MS: f64 = 5000.0;
const AVG_FALL_MS: f64 = 1250.0;
const FLOOR_RISE_MS: f64 = 5000.0;
const FLOOR_FALL_MS: f64 = 50.0;
/// Rise time of the slow gain smoother (ms).
const GAIN_SLOW_RISE_MS: f64 = 100.0;
/// The fast gain smoother falls at this fraction of `release`.
const GAIN_FAST_FALL_FRACTION: f64 = 0.2;
/// Output ring per channel; must hold `HOP + ENGINE_BLOCK + extra delay`.
const RING: usize = 1024;

/// One-pole coefficient for time constant `t_ms` at `rate_hz` updates per
/// second: `1 − exp(−1000/(rate·t))`, and 1 (no smoothing) for `t ≤ 0`.
pub fn one_pole_coef(rate_hz: f64, t_ms: f64) -> f32 {
    if t_ms <= 0.0 {
        1.0
    } else {
        (1.0 - (-1000.0 / (rate_hz * t_ms)).exp()) as f32
    }
}

/// The four user parameters (G HUB names `nr.*`, same units).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NrSettings {
    /// `nr.release`, ms: how fast the reduction engages on noise.
    pub release_ms: f32,
    /// `nr.sensitivity`, dB: cap on how far above the noise floor the
    /// estimate may sit.
    pub sensitivity_db: f32,
    /// `nr.bias`: multiplier on the spread between the average and the floor.
    pub bias: f32,
    /// `nr.attenuation_max`, dB (≤ 0): the gain applied to pure noise.
    pub attenuation_max_db: f32,
}

impl NrSettings {
    /// The engine's `set_defaults` values.
    pub const GHUB_DEFAULTS: NrSettings = NrSettings {
        release_ms: 75.0,
        sensitivity_db: 25.0,
        bias: 2.5,
        attenuation_max_db: -20.0,
    };

    /// Broadcaster 1, the preset used for most reference vectors.
    pub const BROADCASTER_1: NrSettings = NrSettings {
        release_ms: 250.0,
        sensitivity_db: 6.0,
        bias: 3.0,
        attenuation_max_db: -18.0,
    };

    /// Words in the lock-free parameter cell.
    pub const WORDS: usize = 4;

    /// Pack for [`NrHandle`].
    pub fn to_words(&self) -> [u32; Self::WORDS] {
        [
            self.release_ms.to_bits(),
            self.sensitivity_db.to_bits(),
            self.bias.to_bits(),
            self.attenuation_max_db.to_bits(),
        ]
    }

    /// Unpack from [`NrSettings::to_words`].
    pub fn from_words(w: &[u32; Self::WORDS]) -> Self {
        NrSettings {
            release_ms: f32::from_bits(w[0]),
            sensitivity_db: f32::from_bits(w[1]),
            bias: f32::from_bits(w[2]),
            attenuation_max_db: f32::from_bits(w[3]),
        }
    }
}

impl Default for NrSettings {
    fn default() -> Self {
        Self::GHUB_DEFAULTS
    }
}

/// How `process` buffers line up with the engine's 256-frame blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NrTiming {
    /// Exactly G HUB's timing: [`LATENCY`] = 768 samples. Every `process`
    /// call must keep to the 256-frame grid (lengths that are multiples of
    /// 256, or that add up to one before the next frame is due). Samples
    /// that are not ready because of a misaligned call are output as 0 and
    /// counted in [`NoiseReduction::underruns`].
    #[default]
    Engine,
    /// Any buffer size, 1024 samples of latency (G HUB's output delayed by
    /// another 256 samples).
    AnyQuantum,
}

impl NrTiming {
    fn extra_delay(self) -> usize {
        match self {
            NrTiming::Engine => 0,
            NrTiming::AnyQuantum => ENGINE_BLOCK,
        }
    }
}

/// Two-speed one-pole smoother as used by the NR: `fall` when the input is
/// below `h·y`, otherwise `rise`; `y ← y + k·(x − y)` in `f32`.
///
/// Note the direction: the dynamics blocks' smoother
/// picks its second coefficient for a *rising* input; this one picks it for
/// a falling input. The update form matters for the 5 s trackers: the
/// `(1 − k)·y + k·x` grouping drifts by up to 1e-4 in output against the
/// engine over 10 s of loud white noise, this form stays at float rounding.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TwoSpeed {
    h: f32,
    rise: f32,
    fall: f32,
}

impl TwoSpeed {
    #[inline]
    fn step(&self, y: f32, x: f32) -> f32 {
        let k = if x < y * self.h { self.fall } else { self.rise };
        y + k * (x - y)
    }
}

/// Everything derived from [`NrSettings`] and the sample rate.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Coefs {
    sensitivity: f32,
    bias: f32,
    /// `1 − 10^(attenuation_max/20)`.
    depth: f32,
    avg: TwoSpeed,
    floor: TwoSpeed,
    gain_fast: TwoSpeed,
    gain_slow: TwoSpeed,
}

impl Coefs {
    fn new(s: &NrSettings, sample_rate: u32) -> Self {
        // The control rate scales with the frame rate. Only 48 kHz is verified.
        let rate = CONTROL_RATE_48K * f64::from(sample_rate) / 48_000.0;
        let c = |t: f64| one_pole_coef(rate, t);
        let release = if s.release_ms.is_finite() {
            f64::from(s.release_ms)
        } else {
            0.0
        };
        let att_db = if s.attenuation_max_db.is_finite() {
            f64::from(s.attenuation_max_db.min(0.0))
        } else {
            0.0
        };
        let finite_or = |x: f32, d: f32| if x.is_finite() { x } else { d };
        Coefs {
            sensitivity: finite_or(s.sensitivity_db, 0.0),
            bias: finite_or(s.bias, 0.0),
            depth: (1.0 - 10f64.powf(att_db / 20.0)) as f32,
            avg: TwoSpeed {
                h: 1.0,
                rise: c(AVG_RISE_MS),
                fall: c(AVG_FALL_MS),
            },
            floor: TwoSpeed {
                h: 1.0,
                rise: c(FLOOR_RISE_MS),
                fall: c(FLOOR_FALL_MS),
            },
            gain_fast: TwoSpeed {
                h: 1.0,
                rise: 1.0,
                fall: c(GAIN_FAST_FALL_FRACTION * release),
            },
            gain_slow: TwoSpeed {
                h: 1.0,
                rise: c(GAIN_SLOW_RISE_MS),
                fall: c(release),
            },
        }
    }
}

/// The 32-band triangular filterbank, regenerated from its formula.
///
/// Mel scale `m(f) = 2595·log10(1 + f/700)`; 34 points evenly spaced in mel
/// from 0 to `m(22050)` (the filterbank was designed for 44.1 kHz and the
/// engine uses it unchanged at 48 kHz), converted to fractional bins with
/// 44100/1024 Hz per bin. Band `b` rises linearly from point `b` to point
/// `b + 1` and falls to point `b + 2`; band 0 is flat (1) below its peak and
/// band 31 is flat above its peak up to bin 511. Weights are computed in
/// `f64` and stored as `f32`. This reproduces the engine's 32 start bins,
/// 32 end bins and 32 × 100 weights bit for bit.
#[derive(Debug, Clone)]
pub struct BandTable {
    start: [usize; BANDS],
    len: [usize; BANDS],
    weights: [[f32; MAX_BAND_LEN]; BANDS],
}

impl BandTable {
    /// Build the table (not real-time: uses `powf`/`log10` in a loop).
    pub fn mel() -> Self {
        let mel = |f: f64| 2595.0 * (1.0 + f / 700.0).log10();
        let step = mel(22_050.0) / 33.0;
        let bin_hz = 44_100.0 / 1024.0;
        let mut c = [0.0f64; BANDS + 2];
        for (i, ci) in c.iter_mut().enumerate() {
            *ci = 700.0 * (10f64.powf(step * i as f64 / 2595.0) - 1.0) / bin_hz;
        }
        let mut t = BandTable {
            start: [0; BANDS],
            len: [0; BANDS],
            weights: [[0.0; MAX_BAND_LEN]; BANDS],
        };
        for b in 0..BANDS {
            let (lo, pk, hi) = (c[b], c[b + 1], c[b + 2]);
            let s = if b == 0 { 0 } else { lo.floor() as usize + 1 };
            let e = if b == BANDS - 1 {
                BINS - 1
            } else {
                hi.ceil() as usize - 1
            };
            assert!(e - s < MAX_BAND_LEN, "band {b} too long");
            t.start[b] = s;
            t.len[b] = e - s + 1;
            for k in s..=e {
                let kf = k as f64;
                let w = if (b == 0 && kf <= pk) || (b == BANDS - 1 && kf >= pk) {
                    1.0
                } else if kf <= pk {
                    (kf - lo) / (pk - lo)
                } else {
                    (hi - kf) / (hi - pk)
                };
                t.weights[b][k - s] = w as f32;
            }
        }
        t
    }

    /// First bin of band `b`.
    pub fn start(&self, b: usize) -> usize {
        self.start[b]
    }

    /// Last bin of band `b` (inclusive).
    pub fn end(&self, b: usize) -> usize {
        self.start[b] + self.len[b] - 1
    }

    /// The band's weights from its first bin, padded with zeros to
    /// [`MAX_BAND_LEN`] (the engine's table layout).
    pub fn weights_padded(&self, b: usize) -> &[f32; MAX_BAND_LEN] {
        &self.weights[b]
    }

    /// The band's weights over its bins.
    pub fn weights(&self, b: usize) -> &[f32] {
        &self.weights[b][..self.len[b]]
    }
}

/// Per-band control state of one channel, for diagnostics and tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NrBandState {
    /// Slow average tracker (dB).
    pub level_avg_db: [f32; BANDS],
    /// Floor tracker (dB).
    pub level_floor_db: [f32; BANDS],
    /// This frame's attenuation estimate before the one-frame hold (dB, ≤ 0).
    pub attenuation_db: [f32; BANDS],
    /// Fast gain smoother.
    pub gain_fast: [f32; BANDS],
    /// Slow gain smoother.
    pub gain_slow: [f32; BANDS],
}

#[derive(Debug, Clone)]
struct Channel {
    input: [f32; FRAME],
    tail: [f32; HOP],
    ring: [f32; RING],
    bands: NrBandState,
}

impl Channel {
    fn new() -> Self {
        Channel {
            input: [0.0; FRAME],
            tail: [0.0; HOP],
            ring: [0.0; RING],
            bands: NrBandState {
                level_avg_db: [TRACKER_INIT_DB; BANDS],
                level_floor_db: [TRACKER_INIT_DB; BANDS],
                attenuation_db: [0.0; BANDS],
                gain_fast: [1.0; BANDS],
                gain_slow: [1.0; BANDS],
            },
        }
    }
}

/// FFT plans, tables and work buffers shared by all channels. Everything is
/// allocated up front; [`FrameEngine::run`] does not allocate.
struct FrameEngine {
    window: [f32; FRAME],
    table: BandTable,
    /// `20·log10(N/2)`: a full-scale sine reads 0 dB.
    level_offset_db: f32,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    time: Vec<f32>,
    spec: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    bin_gain: [f32; BINS],
}

impl std::fmt::Debug for FrameEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameEngine")
            .field("level_offset_db", &self.level_offset_db)
            .finish_non_exhaustive()
    }
}

impl FrameEngine {
    fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(FRAME);
        let inv = planner.plan_fft_inverse(FRAME);
        let scratch_len = fwd.get_scratch_len().max(inv.get_scratch_len());
        let mut window = [0.0f32; FRAME];
        for (i, w) in window.iter_mut().enumerate() {
            *w = (std::f64::consts::PI * (i as f64 + 0.5) / FRAME as f64).sin() as f32;
        }
        FrameEngine {
            window,
            table: BandTable::mel(),
            level_offset_db: (20.0 * (HOP as f64).log10()) as f32,
            time: fwd.make_input_vec(),
            spec: fwd.make_output_vec(),
            scratch: vec![Complex::default(); scratch_len],
            fwd,
            inv,
            bin_gain: [0.0; BINS],
        }
    }

    /// One frame of one channel: analyse `ch.input`, update the band state,
    /// and write `HOP` finished output samples to `ch.ring` from `ring_pos`.
    fn run(&mut self, ch: &mut Channel, k: &Coefs, ring_pos: usize) {
        for ((t, &x), &w) in self.time.iter_mut().zip(&ch.input).zip(&self.window) {
            *t = x * w;
        }
        // Lengths are fixed at construction, so this cannot fail.
        let _ = self
            .fwd
            .process_with_scratch(&mut self.time, &mut self.spec, &mut self.scratch);

        let st = &mut ch.bands;
        self.bin_gain = [0.0; BINS];
        for b in 0..BANDS {
            let s = self.table.start[b];
            let mut p = 0.0f32;
            for (c, &w) in self.spec[s..].iter().zip(self.table.weights(b)) {
                p += w * (c.re * c.re + c.im * c.im);
            }
            // log10(0) = −inf, which the floor catches.
            let level = (10.0 * p.log10() - self.level_offset_db).max(LEVEL_FLOOR_DB);

            let avg = k.avg.step(st.level_avg_db[b], level);
            let floor = k.floor.step(st.level_floor_db[b], level);
            st.level_avg_db[b] = avg;
            st.level_floor_db[b] = floor;

            let estimate = ((avg - floor) * k.bias).min(k.sensitivity) + floor;
            let att = if level > estimate {
                (estimate - level) - 1.0
            } else {
                0.0
            };
            let held = att.min(st.attenuation_db[b]);
            st.attenuation_db[b] = att;

            let target = 1.0 - 10f32.powf(held / 20.0) * k.depth;
            let fast = k.gain_fast.step(st.gain_fast[b], target);
            let slow = k.gain_slow.step(st.gain_slow[b], target);
            st.gain_fast[b] = fast;
            st.gain_slow[b] = slow;
            let g = fast.min(slow);

            for (bg, &w) in self.bin_gain[s..].iter_mut().zip(self.table.weights(b)) {
                *bg += g * w;
            }
        }
        for (c, &g) in self.spec.iter_mut().zip(&self.bin_gain) {
            *c *= g;
        }

        let _ = self
            .inv
            .process_with_scratch(&mut self.spec, &mut self.time, &mut self.scratch);
        let scale = 1.0 / FRAME as f32;
        for i in 0..HOP {
            let y = self.window[i] * (self.time[i] * scale) + ch.tail[i];
            ch.ring[(ring_pos + i) % RING] = y;
            ch.tail[i] = self.window[i + HOP] * (self.time[i + HOP] * scale);
        }
        ch.input.copy_within(HOP.., 0);
    }
}

/// Control-side handle that sends settings to a running [`NoiseReduction`].
#[derive(Debug)]
pub struct NrHandle {
    writer: ParamWriter<{ NrSettings::WORDS }>,
}

impl NrHandle {
    /// Publish new settings; they take effect from the next frame. Wait-free.
    pub fn set(&mut self, settings: &NrSettings) {
        self.writer.write(&settings.to_words());
    }
}

/// The noise reduction block as a [`Processor`] (planar, up to
/// [`MAX_CHANNELS`] independent channels).
#[derive(Debug)]
pub struct NoiseReduction {
    settings: NrSettings,
    sample_rate: u32,
    coefs: Coefs,
    timing: NrTiming,
    engine: FrameEngine,
    chans: Vec<Channel>,
    /// Input samples consumed since the stream started.
    in_count: usize,
    /// Output samples finished (`HOP` × frames).
    produced: usize,
    /// Fill level of the input frame buffers.
    fill: usize,
    underruns: usize,
    reader: Option<ParamReader<{ NrSettings::WORDS }>>,
}

impl NoiseReduction {
    /// NR for `channels` channels at 48 kHz with G HUB's timing.
    /// Allocates; call off the real-time thread.
    pub fn new(channels: usize, settings: &NrSettings) -> Self {
        Self::with_timing(channels, settings, NrTiming::Engine)
    }

    /// Like [`NoiseReduction::new`] with an explicit timing mode.
    pub fn with_timing(channels: usize, settings: &NrSettings, timing: NrTiming) -> Self {
        let channels = channels.clamp(1, MAX_CHANNELS);
        let mut nr = NoiseReduction {
            settings: *settings,
            sample_rate: 48_000,
            coefs: Coefs::new(settings, 48_000),
            timing,
            engine: FrameEngine::new(),
            chans: vec![Channel::new(); channels],
            in_count: 0,
            produced: 0,
            fill: LATENCY,
            underruns: 0,
            reader: None,
        };
        nr.reset();
        nr
    }

    /// An NR plus a handle for live parameter changes from another thread.
    pub fn new_shared(
        channels: usize,
        settings: &NrSettings,
        timing: NrTiming,
    ) -> (Self, NrHandle) {
        let (writer, reader) = param_cell(settings.to_words());
        let mut nr = Self::with_timing(channels, settings, timing);
        nr.reader = Some(reader);
        (nr, NrHandle { writer })
    }

    /// Back to the state right after the engine's `init`: empty buffers,
    /// trackers at −60 dB, gains at 1. Settings are kept. Real-time safe.
    pub fn reset(&mut self) {
        for ch in &mut self.chans {
            ch.input = [0.0; FRAME];
            ch.tail = [0.0; HOP];
            ch.ring = [0.0; RING];
            ch.bands = Channel::new().bands;
        }
        self.in_count = 0;
        self.produced = 0;
        self.fill = LATENCY;
        self.underruns = 0;
    }

    /// Apply settings directly; they take effect from the next frame.
    /// Live changes from another thread go through [`NrHandle`].
    pub fn set_settings(&mut self, settings: &NrSettings) {
        self.settings = *settings;
        self.coefs = Coefs::new(settings, self.sample_rate);
    }

    /// Current settings.
    pub fn settings(&self) -> &NrSettings {
        &self.settings
    }

    /// Samples of delay this block adds.
    pub fn latency(&self) -> usize {
        LATENCY + self.timing.extra_delay()
    }

    /// Channels this instance processes.
    pub fn channels(&self) -> usize {
        self.chans.len()
    }

    /// Output samples that were due before their frame had run (only in
    /// [`NrTiming::Engine`] with buffers off the 256-frame grid).
    pub fn underruns(&self) -> usize {
        self.underruns
    }

    /// Band state of channel `ch` after the last frame.
    pub fn band_state(&self, ch: usize) -> &NrBandState {
        &self.chans[ch].bands
    }

    /// Process planar channels. Inputs beyond [`NoiseReduction::channels`] are
    /// ignored and their outputs zeroed. Real-time safe.
    fn run(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        if let Some(w) = self.reader.as_mut().and_then(ParamReader::poll) {
            self.set_settings(&NrSettings::from_words(&w));
        }
        let nch = self.chans.len().min(input.len()).min(output.len());
        let frames = input[..nch]
            .iter()
            .map(|c| c.len())
            .chain(output[..nch].iter().map(|c| c.len()))
            .min()
            .unwrap_or(0);
        let delay = self.timing.extra_delay();

        let mut pos = 0;
        while pos < frames {
            // Pieces never cross the 256-frame grid, so a frame can only be
            // due at the end of a piece.
            let n = (ENGINE_BLOCK - self.in_count % ENGINE_BLOCK).min(frames - pos);
            for (ch, inp) in self.chans.iter_mut().zip(input.iter()) {
                ch.input[self.fill..self.fill + n].copy_from_slice(&inp[pos..pos + n]);
            }
            self.fill += n;
            if self.fill == FRAME {
                for ch in &mut self.chans[..nch] {
                    self.engine.run(ch, &self.coefs, self.produced);
                }
                self.produced += HOP;
                self.fill = HOP;
            }
            for i in 0..n {
                let abs = self.in_count + i;
                let ready = abs >= delay && abs - delay < self.produced;
                if !ready && abs >= delay {
                    self.underruns += 1;
                }
                for (ch, out) in self.chans.iter().zip(output.iter_mut()) {
                    out[pos + i] = if ready {
                        ch.ring[(abs - delay) % RING]
                    } else {
                        0.0
                    };
                }
            }
            self.in_count += n;
            pos += n;
        }
        for out in output[..nch].iter_mut() {
            out[frames..].fill(0.0);
        }
        for out in output[nch..].iter_mut() {
            out.fill(0.0);
        }
    }
}

impl Processor for NoiseReduction {
    fn prepare(&mut self, sample_rate: u32, _max_frames: usize) {
        if sample_rate != self.sample_rate {
            self.sample_rate = sample_rate;
            self.coefs = Coefs::new(&self.settings, sample_rate);
            self.reset();
        }
    }

    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        self.run(input, output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    fn run(nr: &mut NoiseReduction, x: &[f32], quantum: usize) -> Vec<f32> {
        let mut y = vec![0.0; x.len()];
        for (xi, yi) in x.chunks(quantum).zip(y.chunks_mut(quantum)) {
            nr.process(&[xi], &mut [yi]);
        }
        y
    }

    #[test]
    fn band_table_partitions_unity() {
        let t = BandTable::mel();
        let mut sum = [0.0f64; BINS];
        for b in 0..BANDS {
            for (j, &w) in t.weights(b).iter().enumerate() {
                sum[t.start(b) + j] += f64::from(w);
            }
        }
        for (k, s) in sum.iter().enumerate() {
            assert!((s - 1.0).abs() < 1e-6, "bin {k}: {s}");
        }
        assert_eq!(t.start(1), 2);
        assert_eq!(t.end(0), 3);
        assert_eq!(t.start(31), 412);
        assert_eq!(t.end(31), 511);
        assert_eq!(t.weights(31).len(), MAX_BAND_LEN);
    }

    #[test]
    fn window_and_offset() {
        let e = FrameEngine::new();
        // Princen-Bradley: w[i]² + w[i + N/2]² = 1.
        for i in 0..HOP {
            let s = e.window[i] * e.window[i] + e.window[i + HOP] * e.window[i + HOP];
            assert!((s - 1.0).abs() < 1e-6);
        }
        assert!((e.level_offset_db - 54.185_4).abs() < 1e-4);
    }

    #[test]
    fn coefficients_match_engine_params() {
        // Values from the engine's params block (Broadcaster 1, release 250 ms).
        // The engine's own exp/rounding differs slightly for the long time
        // constants (up to 1.1e-5 relative), hence the tolerance.
        let k = Coefs::new(&NrSettings::BROADCASTER_1, 48_000);
        let close = |a: f32, b: f32| (a - b).abs() <= 2e-5 * b.abs();
        assert!(close(k.avg.rise, 0.002_171_576));
        assert!(close(k.avg.fall, 0.008_657_932));
        assert!(close(k.floor.fall, 0.195_384_92));
        assert!(close(k.gain_fast.fall, 0.195_384_92));
        assert!(close(k.gain_slow.rise, 0.102_996_65));
        assert!(close(k.gain_slow.fall, 0.042_546_63));
        assert!(close(k.depth, 0.874_107_5));
    }

    #[test]
    fn steady_noise_is_reduced_by_attenuation_max() {
        // Quiet noise (band levels near the trackers' −60 dB start value),
        // since the trackers only rise with a 5 s time constant.
        let x: Vec<f32> = noise(48_000 * 6, 1).iter().map(|v| v * 0.01).collect();
        let mut nr = NoiseReduction::new(1, &NrSettings::BROADCASTER_1);
        let y = run(&mut nr, &x, 256);
        let rms = |v: &[f32]| (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt();
        let tail = 48_000 * 4..48_000 * 6;
        let red = 20.0 * (rms(&y[tail.clone()]) / rms(&x[tail])).log10();
        assert!((-18.5..-14.0).contains(&red), "reduction {red} dB");
    }

    #[test]
    fn unity_gain_reconstructs_with_latency() {
        // Attenuation 0 dB: every gain is exactly 1, so the WOLA must give
        // back the input delayed by LATENCY.
        let s = NrSettings {
            attenuation_max_db: 0.0,
            ..NrSettings::BROADCASTER_1
        };
        let x = noise(48_000, 7);
        let mut nr = NoiseReduction::new(1, &s);
        let y = run(&mut nr, &x, 256);
        for i in LATENCY..x.len() {
            assert!((y[i] - x[i - LATENCY]).abs() < 1e-5, "sample {i}");
        }
        assert!(y[..LATENCY].iter().all(|&v| v.abs() < 1e-6));
    }

    #[test]
    fn output_is_independent_of_quantum() {
        let x = noise(48_000 * 2, 3);
        let s = NrSettings::BROADCASTER_1;
        let mut a = NoiseReduction::new(1, &s);
        let ya = run(&mut a, &x, 256);
        for q in [512, 1024, 768] {
            let mut b = NoiseReduction::new(1, &s);
            assert_eq!(run(&mut b, &x, q), ya, "engine timing, quantum {q}");
            assert_eq!(b.underruns(), 0);
        }
        for q in [1, 37, 256, 480, 1000, 4096] {
            let mut b = NoiseReduction::with_timing(1, &s, NrTiming::AnyQuantum);
            let yb = run(&mut b, &x, q);
            assert_eq!(b.underruns(), 0, "quantum {q}");
            assert_eq!(
                &yb[ENGINE_BLOCK..],
                &ya[..ya.len() - ENGINE_BLOCK],
                "any, quantum {q}"
            );
        }
        // Misaligned buffers in engine mode are reported, not hidden.
        let mut c = NoiseReduction::new(1, &s);
        run(&mut c, &x, 100);
        assert!(c.underruns() > 0);
    }

    #[test]
    fn channels_are_independent() {
        let l = noise(48_000, 11);
        let r: Vec<f32> = noise(48_000, 12).iter().map(|v| v * 0.1).collect();
        let s = NrSettings::BROADCASTER_1;
        let mut st = NoiseReduction::new(2, &s);
        let mut yl = vec![0.0; l.len()];
        let mut yr = vec![0.0; r.len()];
        for i in (0..l.len()).step_by(256) {
            let e = (i + 256).min(l.len());
            let (a, b) = (&mut yl[i..e], &mut yr[i..e]);
            st.process(&[&l[i..e], &r[i..e]], &mut [a, b]);
        }
        let mut m = NoiseReduction::new(1, &s);
        assert_eq!(run(&mut m, &l, 256), yl);
        let mut m = NoiseReduction::new(1, &s);
        assert_eq!(run(&mut m, &r, 256), yr);
    }

    #[test]
    fn handle_updates_settings() {
        let (mut nr, mut h) =
            NoiseReduction::new_shared(1, &NrSettings::GHUB_DEFAULTS, NrTiming::Engine);
        h.set(&NrSettings::BROADCASTER_1);
        let x = [0.0f32; 256];
        let mut y = [0.0f32; 256];
        nr.process(&[&x], &mut [&mut y]);
        assert_eq!(*nr.settings(), NrSettings::BROADCASTER_1);
    }
}
