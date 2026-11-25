//! gprox's own presets (gprox-core `builtin`) through the real DSP: they load,
//! and full-scale test signals give no NaN and no clipping.

use gprox_core::builtin;
use gprox_dsp::bluevoice::{BlueVoice, BvParams, P};
use gprox_dsp::{
    EqMode, HeadphoneEq, HpEqSettings, Processor, cascade_magnitude_db, steady_state_taps,
};

const FS: u32 = 48_000;

fn hp_settings(p: &gprox_core::HeadphoneEqPreset) -> HpEqSettings {
    assert_eq!(p.mode, gprox_core::EqMode::Advanced);
    HpEqSettings {
        mode: EqMode::Advanced,
        basic: p.basic,
        advanced: p.advanced,
    }
}

/// Logarithmic sine sweep, amplitude 1.0, `secs` long.
fn sweep(f0: f64, f1: f64, secs: f64) -> Vec<f32> {
    let n = (secs * FS as f64) as usize;
    let k = (f1 / f0).ln();
    (0..n)
        .map(|i| {
            let t = i as f64 / FS as f64;
            let phase = 2.0 * std::f64::consts::PI * f0 * secs / k * ((t / secs * k).exp() - 1.0);
            phase.sin() as f32
        })
        .collect()
}

/// Deterministic full-scale white noise (xorshift), uniform in −1…1.
fn noise(n: usize) -> Vec<f32> {
    let mut s = 0x9e37_79b9_7f4a_7c15_u64;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        })
        .collect()
}

fn peak(x: &[f32]) -> f32 {
    assert!(x.iter().all(|v| v.is_finite()), "non-finite output");
    x.iter().fold(0.0, |m, v| m.max(v.abs()))
}

/// Run a stereo signal (same on both channels) through a fresh EQ stream,
/// in 480-frame blocks like G HUB's APO.
fn run_hp(s: &HpEqSettings, x: &[f32]) -> Vec<f32> {
    let mut eq: HeadphoneEq = HeadphoneEq::new(s);
    eq.prepare(FS, 480);
    let mut out = Vec::with_capacity(x.len());
    for blk in x.chunks(480) {
        let mut l = blk.to_vec();
        let mut r = blk.to_vec();
        eq.process_in_place(&mut [&mut l, &mut r]);
        assert_eq!(l, r);
        out.extend_from_slice(&l);
    }
    out
}

#[test]
fn hp_eq_magnitude_never_above_0_db() {
    for (name, p) in builtin::hp_eq_presets() {
        let taps = steady_state_taps(&hp_settings(&p), FS);
        let mut max = f64::MIN;
        let mut f = 10.0;
        while f < 23_900.0 {
            max = max.max(cascade_magnitude_db(&taps, f, FS));
            f *= 1.002;
        }
        // f32 taps: allow rounding, nothing more.
        assert!(max <= 0.01, "{name}: response peaks at {max:+.3} dB");
        println!("{name}: magnitude peak {max:+.3} dB");
    }
}

#[test]
fn hp_eq_full_scale_signals_do_not_clip() {
    let sw = sweep(20.0, 20_000.0, 4.0);
    let nz = noise(FS as usize * 2);
    for (name, p) in builtin::hp_eq_presets() {
        let s = hp_settings(&p);
        // Sine sweep at 0 dBFS, including the 100 ms start ramp.
        let ps = peak(&run_hp(&s, &sw));
        assert!(ps <= 1.0 + 1e-3, "{name}: sweep peak {ps}");
        // Steady sines at each band centre and between bands.
        for f in [
            32.0, 45.0, 64.0, 90.0, 125.0, 1000.0, 3000.0, 8000.0, 15000.0,
        ] {
            let x: Vec<f32> = (0..FS as usize / 2)
                .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / FS as f64).sin() as f32)
                .collect();
            let y = run_hp(&s, &x);
            let pk = peak(&y[FS as usize / 4..]);
            assert!(pk <= 1.0 + 1e-3, "{name}: {f} Hz sine peak {pk}");
        }
        // Full-scale white noise, a stress test: the filters' phase can add
        // sample peaks (the impulse responses' l1 norms are 1.2-2.0), but
        // the cuts keep them within 0.1 dB of full scale.
        let pn = peak(&run_hp(&s, &nz));
        println!("{name}: sweep peak {ps:.4}, full-scale noise peak {pn:.4}");
        assert!(pn <= 1.01, "{name}: noise peak {pn}");
    }
}

fn bv_params(p: &gprox_core::BlueVoicePreset) -> BvParams {
    // As gproxd: engine defaults, the preset's values, balance 0.
    let mut bv = BvParams::default();
    for (n, v) in p.params() {
        bv.set(&n, v)
            .unwrap_or_else(|e| panic!("{}: {n}: {e:?}", p.name));
    }
    bv.set_f(P::Balance, 0.0);
    bv
}

fn run_bv(p: &BvParams, x: &[f32]) -> Vec<f32> {
    let mut bv = BlueVoice::new(p);
    bv.prepare(FS, 256);
    let mut out = Vec::with_capacity(x.len());
    let (mut l, mut r) = (vec![0.0; 128], vec![0.0; 128]);
    // 128-frame host blocks, so the re-blocking FIFO is exercised too.
    for blk in x.chunks(128) {
        let n = blk.len();
        bv.process(&[blk], &mut [&mut l[..n], &mut r[..n]]);
        assert_eq!(l[..n], r[..n]);
        out.extend_from_slice(&l[..n]);
    }
    out
}

#[test]
fn bluevoice_full_scale_signals_are_finite_and_limited() {
    let sw = sweep(20.0, 20_000.0, 3.0);
    let nz = noise(FS as usize * 3);
    let mut clicks = vec![0.0f32; FS as usize * 2];
    for i in (0..clicks.len()).step_by(4800) {
        clicks[i] = 1.0;
        clicks[i + 1] = -1.0;
    }
    let silence = vec![0.0f32; FS as usize];
    for preset in builtin::bluevoice_presets() {
        let p = bv_params(&preset);
        let vol = p.f(P::Volume);
        for (sig, x) in [
            ("sweep", &sw),
            ("noise", &nz),
            ("clicks", &clicks),
            ("silence", &silence),
        ] {
            let y = run_bv(&p, x);
            let pk = peak(&y);
            println!("{}: {sig} peak {pk:.4}", preset.name);
            // The limiter clips at ±1 before the output volume.
            assert!(pk <= vol + 1e-6, "{}: {sig} peak {pk}", preset.name);
            if sig == "silence" {
                assert_eq!(pk, 0.0, "{}: silence must stay silent", preset.name);
            }
        }
    }
}
