//! Blue VO!CE capture chain: parameter smoothing, de-popper, noise reduction
//! ([`crate::nr`]), expander, de-esser, voice EQ, compressor and limiter.
//!
//! Engine order per block: parameter smoothing → de-popper → NR →
//! expander → de-esser → voice EQ → compressor → limiter (always on) →
//! volume → balance. Mono in gives identical L/R out.
//!
//! The engine works on 256-frame blocks (48 kHz) whose dynamics run in
//! 32-frame sub-blocks. [`BlueVoice`] does the same: host buffers of any size
//! are re-blocked through a one-engine-block FIFO (like the APO's own
//! re-blocking), and each engine block runs every stage in engine order, the
//! dynamics on the 32-frame grid and the NR on the whole block in its exact
//! engine timing ([`NrTiming::Engine`]). The output is the engine's output
//! delayed by exactly [`BlueVoice::FIFO_LATENCY`] frames, independent of the
//! PipeWire quantum, and enable cross-fades line up exactly (the NR's fade
//! mixes the undelayed dry signal with its 768-sample-late output, as G HUB).
//!
//! Real-time notes: `process` does no allocation, locking, syscalls or
//! logging. Parameter changes arrive through a [`crate::shared`] sequence
//! lock and take effect at the next 256-frame block, like G HUB's `set`.

pub mod blocks;
pub mod dynamics;
pub mod filters;
pub mod params;

use blocks::{BlockBuf, CH, Geometry, MAX_BLOCK};
use dynamics::{Compressor, DeEsser, Expander, Limiter};
use filters::{DePopper, VoiceEq};
pub use params::{BvParams, P};

use crate::nr::{NoiseReduction, NrSettings, NrTiming};
use crate::shared::{ParamReader, ParamWriter, param_cell};
use crate::smoothing::{EnableFade, ParamSmoother, coef};
use crate::{MAX_CHANNELS, Processor};

/// G HUB's default parameter-smoothing time.
pub const DEFAULT_SMOOTHING_MS: f32 = 50.0;

/// Modules with an enable cross-fade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fx {
    Dpop,
    Nr,
    Exp,
    Des,
    Veq,
    Comp,
}
const FX: [Fx; 6] = [Fx::Dpop, Fx::Nr, Fx::Exp, Fx::Des, Fx::Veq, Fx::Comp];

/// Control-side handle for a running [`BlueVoice`].
#[derive(Debug)]
pub struct BlueVoiceHandle {
    writer: ParamWriter<{ params::COUNT }>,
}

impl BlueVoiceHandle {
    /// Publish a parameter set; it takes effect at the next engine block.
    pub fn set(&mut self, p: &BvParams) {
        self.writer.write(&p.to_words());
    }
}

/// Smoothing coefficients derived from the smoothing time.
#[derive(Debug, Clone, Copy)]
struct SmoothK {
    /// Per-sample enable cross-fade.
    xfade: f32,
    /// Per-block parameter smoothing.
    block: f32,
    /// Per-block de-popper corner (max(2·ms, 100 ms)).
    slow: f32,
}

/// The Blue VO!CE chain as a [`Processor`].
#[derive(Debug)]
pub struct BlueVoice {
    geo: Geometry,
    smoothing_ms: f32,
    k: SmoothK,
    params: BvParams,
    reader: Option<ParamReader<{ params::COUNT }>>,
    first: bool,

    volume: ParamSmoother,
    balance: ParamSmoother,
    des_freq: ParamSmoother,
    fades: [EnableFade; 6],
    dpop: DePopper,
    nr: NoiseReduction,
    veq: VoiceEq,
    exp: Expander,
    des: DeEsser,
    comp: Compressor,
    lim: Limiter,

    dry: BlockBuf,
    work: BlockBuf,
    /// Re-blocking FIFOs (one engine block).
    fifo_in: BlockBuf,
    fifo_out: BlockBuf,
    fill: usize,
}

impl BlueVoice {
    /// Extra latency of the re-blocking FIFO (one engine block at 48 kHz;
    /// [`Geometry::block`] frames in general).
    pub const FIFO_LATENCY: usize = 256;

    /// A chain at 48 kHz with the given parameters and G HUB's 50 ms
    /// smoothing. [`Processor::prepare`] sets the real rate.
    pub fn new(p: &BvParams) -> Self {
        Self::with_smoothing(p, DEFAULT_SMOOTHING_MS)
    }

    /// Like [`BlueVoice::new`] with an explicit smoothing time in ms
    /// (`set_smoothing`; ≤ 0 = none).
    pub fn with_smoothing(p: &BvParams, smoothing_ms: f32) -> Self {
        let geo = Geometry::for_rate(48_000);
        let mut bv = BlueVoice {
            geo,
            smoothing_ms,
            k: SmoothK {
                xfade: 1.0,
                block: 1.0,
                slow: 1.0,
            },
            params: *p,
            reader: None,
            first: true,
            volume: ParamSmoother::new(1.0, 1e-4),
            balance: ParamSmoother::new(0.0, 1e-4),
            des_freq: ParamSmoother::new(6000.0, 0.01),
            fades: [EnableFade::new(); 6],
            dpop: DePopper::new(),
            nr: NoiseReduction::with_timing(CH, &nr_settings(p), NrTiming::Engine),
            veq: VoiceEq::new(),
            exp: Expander::new(&geo),
            des: DeEsser::new(&geo),
            comp: Compressor::new(&geo),
            lim: Limiter::new(&geo),
            dry: [[0.0; MAX_BLOCK]; CH],
            work: [[0.0; MAX_BLOCK]; CH],
            fifo_in: [[0.0; MAX_BLOCK]; CH],
            fifo_out: [[0.0; MAX_BLOCK]; CH],
            fill: 0,
        };
        bv.reset_stream(48_000);
        bv
    }

    /// A chain plus a handle for live parameter changes from another thread.
    pub fn new_shared(p: &BvParams) -> (Self, BlueVoiceHandle) {
        let (writer, reader) = param_cell(p.to_words());
        let mut bv = Self::new(p);
        bv.reader = Some(reader);
        (bv, BlueVoiceHandle { writer })
    }

    /// Start a new stream at `fs`, like the APO creating a fresh engine.
    pub fn reset_stream(&mut self, fs: u32) {
        let geo = Geometry::for_rate(fs);
        self.geo = geo;
        self.first = true;
        self.fill = 0;
        self.fifo_in = [[0.0; MAX_BLOCK]; CH];
        self.fifo_out = [[0.0; MAX_BLOCK]; CH];
        self.fades = [EnableFade::new(); 6];
        self.volume = ParamSmoother::new(1.0, 1e-4);
        self.balance = ParamSmoother::new(0.0, 1e-4);
        self.des_freq = ParamSmoother::new(6000.0, 0.01);
        self.dpop = DePopper::new();
        self.nr.prepare(fs, 0);
        self.nr.reset();
        self.veq = VoiceEq::new();
        self.exp = Expander::new(&geo);
        self.des = DeEsser::new(&geo);
        self.comp = Compressor::new(&geo);
        self.lim = Limiter::new(&geo);
        self.set_smoothing(self.smoothing_ms);
        let p = self.params;
        self.apply(&p);
    }

    /// `set_smoothing(ms)`: cross-fade and parameter smoothing time.
    pub fn set_smoothing(&mut self, ms: f32) {
        self.smoothing_ms = ms;
        let g = &self.geo;
        self.k = if ms > 0.0 {
            SmoothK {
                xfade: coef(g.fs as f32, ms),
                block: coef(g.block_rate(), ms),
                slow: coef(g.block_rate(), (2.0 * ms).max(100.0)),
            }
        } else {
            SmoothK {
                xfade: 1.0,
                block: 1.0,
                slow: 1.0,
            }
        };
    }

    /// Take over a parameter set (on the owning thread). Effective from the
    /// next engine block.
    pub fn apply(&mut self, p: &BvParams) {
        self.params = *p;
        let g = self.geo;
        self.volume.target = p.f(P::Volume);
        self.balance.target = p.f(P::Balance);
        self.des_freq.target = p.f(P::DesFreq);
        self.dpop.configure(p);
        let nr = nr_settings(p);
        if nr != *self.nr.settings() {
            self.nr.set_settings(&nr);
        }
        self.veq.configure(p);
        self.exp.configure(p, &g);
        self.des.configure(p, &g);
        self.comp.configure(p, &g);
        self.lim.configure(p, &g);
    }

    /// The parameters in force.
    pub fn params(&self) -> &BvParams {
        &self.params
    }

    /// Engine geometry (rate, block, sub-block).
    pub fn geometry(&self) -> Geometry {
        self.geo
    }

    /// Last gain decision of each dynamics block (expander, de-esser,
    /// compressor, limiter), for meters and tests.
    pub fn gains(&self) -> [f32; 4] {
        [
            self.exp.last_gain,
            self.des.last_gain,
            self.comp.last_gain,
            self.lim.last_gain,
        ]
    }

    fn enabled(&self, fx: Fx) -> bool {
        let id = match fx {
            Fx::Dpop => P::DpopEnable,
            Fx::Nr => P::NrEnable,
            Fx::Exp => P::ExpEnable,
            Fx::Des => P::DesEnable,
            Fx::Veq => P::VeqEnable,
            Fx::Comp => P::CompEnable,
        };
        self.params.on(P::Enable) && self.params.on(id)
    }

    /// Block start: new parameters, smoothing step, filter redesigns, fades.
    fn begin_block(&mut self) {
        if let Some(w) = self.reader.as_mut().and_then(ParamReader::poll) {
            self.apply(&BvParams::from_words(w));
        }
        let (k, first, fs) = (self.k, self.first, self.geo.fs);
        for s in [&mut self.volume, &mut self.balance] {
            if first {
                s.settle();
            } else {
                s.step(k.block);
            }
        }
        if first {
            self.dpop.freq.settle();
        }
        if self.dpop.freq.step(k.slow) || first {
            self.dpop.design(fs);
        }
        if first {
            self.des_freq.settle();
        }
        if self.des_freq.step(k.block) || first {
            self.des.design(self.des_freq.cur, fs);
        }
        if self.veq.smooth(k.block, first) {
            self.veq.design(fs);
        }
        for (i, fx) in FX.iter().enumerate() {
            let on = self.enabled(*fx);
            if first && matches!(fx, Fx::Dpop | Fx::Veq) {
                self.fades[i].mix = if on { 1.0 } else { 0.0 };
            }
            self.fades[i].begin_block(on);
        }
    }

    /// Block end: cross-fade snaps and resets.
    fn end_block(&mut self) {
        let micopt = if self.params.on(P::Enable) { 1.0 } else { 0.0 };
        for (i, fx) in FX.iter().enumerate() {
            let f = &mut self.fades[i];
            // Quirk: the voice EQ compares against micopt.enable.
            let reference = if *fx == Fx::Veq { micopt } else { f.target };
            if f.end_block(reference) {
                match fx {
                    Fx::Dpop => self.dpop.reset(),
                    Fx::Nr => self.nr.reset(),
                    Fx::Exp => self.exp.reset(),
                    Fx::Des => self.des.reset(),
                    Fx::Veq => self.veq.reset(),
                    Fx::Comp => self.comp.reset(),
                }
            }
        }
        self.first = false;
    }

    /// Run one engine block of `nch` channels in `self.work`. Each stage runs
    /// over the whole block before the next (they are causal and independent
    /// of later stages, so this equals the engine's order); the dynamics step
    /// through the block on the sub-block grid.
    fn run_block(&mut self, nch: usize) {
        let (len, sub) = (self.geo.block, self.geo.sub);
        let subs = len / sub;
        self.begin_block();
        let inv = self.geo.inv_sub();
        let k = self.k.xfade;
        for (i, fx) in FX.iter().enumerate() {
            if !self.fades[i].active {
                continue;
            }
            let fading = self.fades[i].fading;
            if fading || *fx == Fx::Nr {
                for c in 0..nch {
                    self.dry[c][..len].copy_from_slice(&self.work[c][..len]);
                }
            }
            if *fx == Fx::Nr {
                let [d0, d1] = &self.dry;
                let ins: [&[f32]; CH] = [&d0[..len], &d1[..len]];
                let [w0, w1] = &mut self.work;
                let mut outs: [&mut [f32]; CH] = [&mut w0[..len], &mut w1[..len]];
                self.nr.process(&ins[..nch], &mut outs[..nch]);
            } else {
                for j in 0..subs {
                    let (a, b) = (j * sub, (j + 1) * sub);
                    let block_end = j + 1 == subs;
                    let [w0, w1] = &mut self.work;
                    let mut chans: [&mut [f32]; CH] = [&mut w0[a..b], &mut w1[a..b]];
                    let chans = &mut chans[..nch];
                    match fx {
                        Fx::Dpop => self.dpop.process(chans, sub, block_end),
                        Fx::Exp => self.exp.process(chans, sub, inv),
                        Fx::Des => self.des.process(chans, sub, inv),
                        Fx::Veq => self.veq.process(chans, sub, block_end),
                        Fx::Comp => self.comp.process(chans, sub, inv),
                        Fx::Nr => unreachable!(),
                    }
                }
            }
            if fading {
                let f = &mut self.fades[i];
                let (omk, tk) = (1.0 - k, f.target * k);
                for s in 0..len {
                    let m = f.next(omk, tk);
                    for c in 0..nch {
                        self.work[c][s] = (1.0 - m) * self.dry[c][s] + m * self.work[c][s];
                    }
                }
            }
        }
        for j in 0..subs {
            let (a, b) = (j * sub, (j + 1) * sub);
            let [w0, w1] = &mut self.work;
            let mut chans: [&mut [f32]; CH] = [&mut w0[a..b], &mut w1[a..b]];
            self.lim.process(&mut chans[..nch], sub, inv);
        }
        // Volume, then balance; mono input feeds both outputs.
        let vol = self.volume.cur;
        for w in self.work.iter_mut().take(nch) {
            for v in &mut w[..len] {
                *v *= vol;
            }
        }
        let b = self.balance.cur;
        let s = 1.0 - b.abs();
        if nch == 1 {
            let [w0, w1] = &mut self.work;
            if b < 0.0 {
                for (r, &l) in w1[..len].iter_mut().zip(w0[..len].iter()) {
                    *r = s * l;
                }
            } else {
                for (r, l) in w1[..len].iter_mut().zip(w0[..len].iter_mut()) {
                    *r = *l;
                    *l *= s;
                }
            }
        } else {
            let side = if b < 0.0 { 1 } else { 0 };
            for v in &mut self.work[side][..len] {
                *v *= s;
            }
        }
        self.end_block();
    }

    /// Process planar input (1 = mono mic, or 2 channels) into up to two
    /// output channels (L, R). Output lags the engine by
    /// [`Self::FIFO_LATENCY`] frames at 48 kHz (one engine block in general).
    pub fn process_planar(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let nch = input.len().clamp(1, CH);
        let frames = output.iter().map(|o| o.len()).min().unwrap_or(0);
        let blk = self.geo.block;
        let mut pos = 0;
        while pos < frames {
            let n = (blk - self.fill).min(frames - pos);
            for (c, fifo) in self.fifo_in.iter_mut().enumerate() {
                let dst = &mut fifo[self.fill..self.fill + n];
                match input.get(c) {
                    Some(s) if c < nch => {
                        let avail = s.len().saturating_sub(pos).min(n);
                        dst[..avail].copy_from_slice(&s[pos..pos + avail]);
                        dst[avail..].fill(0.0);
                    }
                    _ => dst.fill(0.0),
                }
            }
            for (out, fifo) in output.iter_mut().zip(self.fifo_out.iter()) {
                out[pos..pos + n].copy_from_slice(&fifo[self.fill..self.fill + n]);
            }
            self.fill += n;
            pos += n;
            if self.fill == blk {
                self.work = self.fifo_in;
                self.run_block(nch);
                self.fifo_out = self.work;
                self.fill = 0;
            }
        }
    }
}

/// The NR's four user parameters from a Blue VO!CE parameter set.
fn nr_settings(p: &BvParams) -> NrSettings {
    NrSettings {
        release_ms: p.f(P::NrRelease),
        sensitivity_db: p.f(P::NrSensitivity),
        bias: p.f(P::NrBias),
        attenuation_max_db: p.f(P::NrAttenuationMax),
    }
}

impl Processor for BlueVoice {
    fn prepare(&mut self, sample_rate: u32, _max_frames: usize) {
        self.reset_stream(sample_rate);
    }

    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let n = output.len().min(MAX_CHANNELS);
        self.process_planar(input, &mut output[..n]);
        for out in output.iter_mut().skip(CH) {
            out.fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(bv: &mut BlueVoice, x: &[f32], quantum: usize) -> (Vec<f32>, Vec<f32>) {
        let mut l = vec![0.0; x.len()];
        let mut r = vec![0.0; x.len()];
        let mut pos = 0;
        while pos < x.len() {
            let n = quantum.min(x.len() - pos);
            bv.process(
                &[&x[pos..pos + n]],
                &mut [&mut l[pos..pos + n], &mut r[pos..pos + n]],
            );
            pos += n;
        }
        (l, r)
    }

    fn signal(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                ((i as f32 * 0.031).sin() * 0.3 + (i as f32 * 0.0007).sin() * 0.2)
                    * if i % 9000 < 4000 { 1.0 } else { 0.05 }
            })
            .collect()
    }

    #[test]
    fn all_off_is_a_pure_delay() {
        let mut p = BvParams::default();
        p.set_on(P::Enable, true);
        let mut bv = BlueVoice::with_smoothing(&p, 0.0);
        let x = signal(4800);
        let (l, r) = run(&mut bv, &x, 256);
        let d = BlueVoice::FIFO_LATENCY + 128;
        assert_eq!(&l[d..], &x[..x.len() - d]);
        assert_eq!(l, r);
    }

    #[test]
    fn output_does_not_depend_on_the_host_quantum() {
        let p = BvParams::broadcaster1();
        let x = signal(48_000);
        let mut a = BlueVoice::new(&p);
        let reference = run(&mut a, &x, 256);
        for q in [32, 37, 480, 1024] {
            let mut b = BlueVoice::new(&p);
            assert_eq!(run(&mut b, &x, q), reference, "quantum {q}");
        }
    }

    #[test]
    fn mono_gives_identical_lr_and_respects_the_clip() {
        let mut p = BvParams::broadcaster1();
        p.set_f(P::Volume, 1.0);
        let mut bv = BlueVoice::new(&p);
        let x: Vec<f32> = signal(24_000).iter().map(|v| v * 4.0).collect();
        let (l, r) = run(&mut bv, &x, 512);
        assert_eq!(l, r);
        assert!(l.iter().all(|v| v.abs() <= 1.0));
    }

    #[test]
    fn handle_changes_params_at_the_next_block() {
        let (mut bv, mut h) = BlueVoice::new_shared(&BvParams::default());
        let x = vec![0.1f32; 512];
        run(&mut bv, &x, 256);
        let mut p = BvParams::default();
        p.set_f(P::Volume, 0.5);
        h.set(&p);
        run(&mut bv, &x, 256);
        assert_eq!(bv.params().f(P::Volume), 0.5);
    }
}
