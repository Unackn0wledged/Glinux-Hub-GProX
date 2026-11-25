//! Render audio offline through gprox's DSP with a preset, for listening
//! tests and level checks.
//!
//! ```text
//! cargo run --release -p gprox-dsp --example render_preset -- \
//!     hp|bv PRESET IN.f32 OUT.f32 [CHANNELS]
//! ```
//!
//! * `hp`: headphone EQ. IN/OUT are interleaved f32 at 48 kHz with
//!   CHANNELS channels (default 2).
//! * `bv`: Blue VO!CE. IN is mono f32 at 48 kHz; OUT is mono (the chain's
//!   first output channel), aligned to IN by removing the re-blocking
//!   latency (the DSP's own look-ahead latency stays).
//!
//! PRESET is a TOML file (`HeadphoneEqPreset` / `BlueVoicePreset`) or the
//! name of a built-in preset ("gprox Movies", "gprox Natural", …). The
//! Blue VO!CE parameters are set like gproxd does: engine defaults, then the
//! preset's values, balance 0.

use std::path::Path;

use gprox_core::{BlueVoicePreset, HeadphoneEqPreset, builtin};
use gprox_dsp::bluevoice::{BlueVoice, BvParams, P};
use gprox_dsp::{EqMode, HeadphoneEq, HpEqSettings, Processor};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn read_f32(path: &str) -> Res<Vec<f32>> {
    let b = std::fs::read(path)?;
    Ok(b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn write_f32(path: &str, x: &[f32]) -> Res<()> {
    let b: Vec<u8> = x.iter().flat_map(|v| v.to_le_bytes()).collect();
    Ok(std::fs::write(path, b)?)
}

fn hp_preset(arg: &str) -> Res<HeadphoneEqPreset> {
    if Path::new(arg).is_file() {
        return Ok(HeadphoneEqPreset::from_toml_str(&std::fs::read_to_string(
            arg,
        )?)?);
    }
    builtin::hp_eq_presets()
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(arg))
        .map(|(_, p)| p)
        .ok_or_else(|| format!("no preset file or built-in preset {arg:?}").into())
}

fn bv_preset(arg: &str) -> Res<BlueVoicePreset> {
    if Path::new(arg).is_file() {
        return Ok(BlueVoicePreset::from_toml_str(&std::fs::read_to_string(
            arg,
        )?)?);
    }
    builtin::bluevoice_presets()
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(arg))
        .ok_or_else(|| format!("no preset file or built-in preset {arg:?}").into())
}

const BLOCK: usize = 256;

fn render_hp(p: &HeadphoneEqPreset, x: &[f32], nch: usize) -> Vec<f32> {
    let s = HpEqSettings {
        mode: match p.mode {
            gprox_core::EqMode::Basic => EqMode::Basic,
            gprox_core::EqMode::Advanced => EqMode::Advanced,
        },
        basic: p.basic,
        advanced: p.advanced,
    };
    let mut eq: HeadphoneEq = HeadphoneEq::new(&s);
    eq.prepare(48_000, BLOCK);
    let frames = x.len() / nch;
    let mut out = vec![0.0; frames * nch];
    let mut chans = vec![vec![0.0f32; BLOCK]; nch];
    let mut pos = 0;
    while pos < frames {
        let n = BLOCK.min(frames - pos);
        for (c, ch) in chans.iter_mut().enumerate() {
            for i in 0..n {
                ch[i] = x[(pos + i) * nch + c];
            }
        }
        let mut refs: Vec<&mut [f32]> = chans.iter_mut().map(|c| &mut c[..n]).collect();
        eq.process_in_place(&mut refs);
        for (c, ch) in chans.iter().enumerate() {
            for i in 0..n {
                out[(pos + i) * nch + c] = ch[i];
            }
        }
        pos += n;
    }
    out
}

fn render_bv(p: &BlueVoicePreset, x: &[f32]) -> Res<Vec<f32>> {
    let mut params = BvParams::default();
    for (name, v) in p.params() {
        params.set(&name, v)?;
    }
    params.set_f(P::Balance, 0.0);
    let mut bv = BlueVoice::new(&params);
    bv.prepare(48_000, BLOCK);
    let lat = BlueVoice::FIFO_LATENCY;
    // Pad so the tail comes out, then drop the FIFO latency.
    let mut input = x.to_vec();
    input.resize(x.len() + lat + BLOCK, 0.0);
    let mut out = Vec::with_capacity(input.len());
    let mut l = vec![0.0f32; BLOCK];
    let mut r = vec![0.0f32; BLOCK];
    for chunk in input.chunks(BLOCK) {
        let n = chunk.len();
        let mut o: [&mut [f32]; 2] = [&mut l[..n], &mut r[..n]];
        bv.process(&[chunk], &mut o);
        out.extend_from_slice(&l[..n]);
    }
    Ok(out[lat..lat + x.len()].to_vec())
}

fn main() -> Res<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 4 {
        return Err("usage: render_preset hp|bv PRESET IN.f32 OUT.f32 [CHANNELS]".into());
    }
    let x = read_f32(&a[2])?;
    let out = match a[0].as_str() {
        "hp" => {
            let nch: usize = a.get(4).map(|s| s.parse()).transpose()?.unwrap_or(2);
            render_hp(&hp_preset(&a[1])?, &x, nch)
        }
        "bv" => render_bv(&bv_preset(&a[1])?, &x)?,
        k => return Err(format!("unknown kind {k:?} (hp or bv)").into()),
    };
    if let Some(i) = out.iter().position(|v| !v.is_finite()) {
        return Err(format!("non-finite output at sample {i}").into());
    }
    let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    eprintln!("peak {:.2} dBFS", 20.0 * peak.max(1e-12).log10());
    write_f32(&a[3], &out)
}
