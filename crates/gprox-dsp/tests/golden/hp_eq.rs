//! Golden tests for the headphone EQ against reference recordings of G HUB's
//! own EQ output, taken inside the Windows audio engine.
//!
//! Vectors are local and not distributed: `re/vectors/eq/<session>/<case>/`
//! (override the `re/vectors/eq` root with `GPROX_EQ_VECTORS`). Every test
//! skips with a note when they are absent, so CI passes without them.
//!
//! Layout per case: `in.f32` / `out.f32` = little-endian f32, per block 8
//! channels × 480 frames planar, block-major; `blocks.jsonl` one object per
//! block (`seq`, `tapsSeqLast`, …); `taps.bin` 12 × 6 f32 taps
//! (b0 b1 b2 a0 a1 a2) per push, numbered by `seq` in `taps.jsonl`. Seq
//! numbers restart with each recording run, so taps are looked up in the case's
//! own directory, except where a case's taps were pushed while the previous
//! case was being recorded in the same run (noted per case, checked by the
//! `ms` timestamps).
//!
//! Run with `cargo test -p gprox-dsp --test golden_hp_eq -- --nocapture` to
//! see the measured numbers; set `GOLDEN_DUMP=<dir>` to also write our
//! channel-0 output per case/kernel/quantum as raw f32 for analysis.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gprox_dsp::biquad::Taps;
use gprox_dsp::hp_eq::{BANDS, HpEqControl};
use gprox_dsp::kernel::{CascadeKernel, IppF32, Tdf2F64Acc};
use gprox_dsp::{EqMode, HeadphoneEq, HpEqSettings, Processor, RampBlock};

const R1: &str = "re5-20261121-r1";
const R2: &str = "re5-20261121-r2";

const NCH: usize = 8;
const FRAMES: usize = 480;
const FS: u32 = 48_000;

// ---------------------------------------------------------------- loading

fn vectors_root() -> PathBuf {
    std::env::var_os("GPROX_EQ_VECTORS")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../re/vectors/eq"))
}

/// The r1 session directory (taps and ramp tests).
fn session_dir() -> Option<PathBuf> {
    let dir = vectors_root().join(R1);
    if dir.is_dir() {
        Some(dir)
    } else {
        eprintln!(
            "SKIP: EQ vectors not found at {} (gitignored, local only)",
            dir.display()
        );
        None
    }
}

fn case_dir(session: &str, name: &str) -> Option<PathBuf> {
    let d = vectors_root().join(session).join(name);
    if d.is_dir() {
        Some(d)
    } else {
        eprintln!("SKIP: case {session}/{name} missing (gitignored, local only)");
        None
    }
}

fn read_f32(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Integer field `"key": <n>` from one JSON line (the logs are flat objects).
fn json_int(line: &str, key: &str) -> i64 {
    let pat = format!("\"{key}\": ");
    let i = line
        .find(&pat)
        .unwrap_or_else(|| panic!("no {key} in {line}"))
        + pat.len();
    let s: String = line[i..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    s.parse().unwrap()
}

fn read_taps(dir: &Path) -> HashMap<i64, [Taps; BANDS]> {
    let raw = read_f32(&dir.join("taps.bin"));
    let seqs: Vec<i64> = std::fs::read_to_string(dir.join("taps.jsonl"))
        .unwrap()
        .lines()
        .map(|l| json_int(l, "seq"))
        .collect();
    assert_eq!(raw.len(), seqs.len() * BANDS * 6);
    seqs.iter()
        .zip(raw.chunks_exact(BANDS * 6))
        .map(|(&s, push)| {
            let mut t = [Taps::IDENTITY; BANDS];
            for (k, six) in push.chunks_exact(6).enumerate() {
                t[k] = Taps::from_six(six.try_into().unwrap());
            }
            (s, t)
        })
        .collect()
}

/// `(seq, tapsSeqLast)` per block.
fn read_blocks(dir: &Path) -> Vec<(i64, i64)> {
    std::fs::read_to_string(dir.join("blocks.jsonl"))
        .unwrap()
        .lines()
        .map(|l| (json_int(l, "seq"), json_int(l, "tapsSeqLast")))
        .collect()
}

fn taps_eq(a: &[Taps; BANDS], b: &[Taps; BANDS]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.bit_eq(y))
}

fn diff_bands(a: &[Taps; BANDS], b: &[Taps; BANDS]) -> Vec<usize> {
    (0..BANDS).filter(|&k| !a[k].bit_eq(&b[k])).collect()
}

// ---------------------------------------------------------------- settings

fn adv(gains: &[(usize, f32)]) -> HpEqSettings {
    let mut s = HpEqSettings::flat(EqMode::Advanced);
    for &(k, g) in gains {
        s.advanced[k] = g;
    }
    s
}

fn basic(bass: f32, treble: f32) -> HpEqSettings {
    HpEqSettings {
        mode: EqMode::Basic,
        basic: [bass, treble],
        ..Default::default()
    }
}

/// Settle `c` (fresh stream) at its stored settings.
fn settle(c: &mut HpEqControl) {
    for _ in 0..100 {
        c.tick();
    }
    assert!(!c.is_ramping());
}

// ---------------------------------------------------------------- (a) taps

/// Final taps for given gains equal the hooked taps bit for bit.
#[test]
fn a_designed_taps_equal_hooked_taps() {
    let Some(root) = session_dir() else { return };
    let cases: [(&str, i64, HpEqSettings); 7] = [
        ("E1b-stream-start-500", 21, adv(&[(5, 6.0)])),
        ("E2b-ramp-1k--6to+6", 10, adv(&[(5, 6.0)])),
        ("E2-ramp-1k-+6to-6", 32, adv(&[(5, -6.0)])),
        ("E3-flat", 21, adv(&[])),
        ("E3-flat", 32, adv(&[(0, 12.0)])),
        ("E3-flat", 43, adv(&[(0, 12.0), (9, 6.0)])),
        ("E4-32Hz+12-16k+6", 76, basic(6.0, -6.0)),
    ];
    for (dir, seq, s) in cases {
        let hooked = read_taps(&root.join(dir))[&seq];
        let mut c = HpEqControl::new(FS, RampBlock::Auto, &s);
        settle(&mut c);
        let d = diff_bands(c.taps(), &hooked);
        println!("taps {dir} seq {seq}: {} of 72 taps differ", d.len() * 6);
        assert!(d.is_empty(), "{dir} seq {seq}: bands {d:?} differ");
    }
}

// ---------------------------------------------------------------- (b) ramps

struct Ramp {
    dir: &'static str,
    first_seq: i64,
    /// `None` = stream start (ramp from 0 dB to `to`).
    from: Option<HpEqSettings>,
    to: HpEqSettings,
}

fn ramps() -> Vec<Ramp> {
    vec![
        Ramp {
            dir: "E1-stream-start",
            first_seq: 0,
            from: None,
            to: adv(&[(5, 6.0)]),
        },
        Ramp {
            dir: "E1b-stream-start-500",
            first_seq: 11,
            from: None,
            to: adv(&[(5, 6.0)]),
        },
        Ramp {
            dir: "E2b-ramp-1k--6to+6",
            first_seq: 0,
            from: Some(adv(&[(5, -6.0)])),
            to: adv(&[(5, 6.0)]),
        },
        Ramp {
            dir: "E2-ramp-1k-+6to-6",
            first_seq: 22,
            from: Some(adv(&[(5, 6.0)])),
            to: adv(&[(5, -6.0)]),
        },
        Ramp {
            dir: "E3-flat",
            first_seq: 11,
            from: Some(adv(&[(5, 6.0)])),
            to: adv(&[]),
        },
        Ramp {
            dir: "E3-flat",
            first_seq: 22,
            from: Some(adv(&[])),
            to: adv(&[(0, 12.0)]),
        },
        Ramp {
            dir: "E3-flat",
            first_seq: 33,
            from: Some(adv(&[(0, 12.0)])),
            to: adv(&[(0, 12.0), (9, 6.0)]),
        },
        // Mode switch advanced → basic (basic still flat): two bands at once.
        Ramp {
            dir: "E4-32Hz+12-16k+6",
            first_seq: 44,
            from: Some(adv(&[(0, 12.0), (9, 6.0)])),
            to: HpEqSettings {
                mode: EqMode::Basic,
                ..adv(&[(0, 12.0), (9, 6.0)])
            },
        },
        Ramp {
            dir: "E4-32Hz+12-16k+6",
            first_seq: 55,
            from: Some(basic(0.0, 0.0)),
            to: basic(6.0, 0.0),
        },
        Ramp {
            dir: "E4-32Hz+12-16k+6",
            first_seq: 66,
            from: Some(basic(6.0, 0.0)),
            to: basic(6.0, -6.0),
        },
        // Stream restart with basic +6/−6 (Blue VO!CE toggle): both shelves ramp.
        Ramp {
            dir: "B1-bv-off-on",
            first_seq: 77,
            from: None,
            to: basic(6.0, -6.0),
        },
    ]
}

/// Every hooked tap push of every ramp (10 steps + snap) is reproduced bit
/// for bit, and no further push follows.
#[test]
fn b_ramp_tap_sequences_equal_hooked() {
    let Some(root) = session_dir() else { return };
    let mut total = 0;
    for r in ramps() {
        let dir = root.join(r.dir);
        if !dir.is_dir() {
            eprintln!("SKIP ramp {}: missing", r.dir);
            continue;
        }
        let hooked = read_taps(&dir);
        let mut c = match r.from {
            None => HpEqControl::new(FS, RampBlock::Auto, &r.to),
            Some(from) => {
                let mut c = HpEqControl::new(FS, RampBlock::Auto, &from);
                settle(&mut c);
                assert!(c.apply(&r.to), "{}: no target change", r.dir);
                c
            }
        };
        let mut pushes = Vec::new();
        for _ in 0..30 {
            if c.tick() {
                pushes.push(*c.taps());
            }
        }
        assert_eq!(pushes.len(), 11, "{} @{}: push count", r.dir, r.first_seq);
        for (i, ours) in pushes.iter().enumerate() {
            let seq = r.first_seq + i as i64;
            let h = &hooked[&seq];
            assert!(
                taps_eq(ours, h),
                "{} seq {seq}: bands {:?} differ",
                r.dir,
                diff_bands(ours, h)
            );
        }
        total += pushes.len();
        println!(
            "ramp {} seq {}..={}: 11/11 pushes bit-exact",
            r.dir,
            r.first_seq,
            r.first_seq + 10
        );
    }
    println!("ramp total: {total} tap pushes bit-exact");
}

// ---------------------------------------------------------------- (c) output

/// How the EQ is set up before the dumped block 0.
#[derive(Clone, Copy)]
enum Start {
    /// New stream: zero state, ramp from 0 dB starts at block 0.
    Fresh(HpEqSettings),
    /// Mid-stream at `from` (settled); `to` arrives just before block 0. The
    /// filter state at block 0 is unknown; it is estimated from the first
    /// blocks by least squares.
    Change {
        from: HpEqSettings,
        to: HpEqSettings,
    },
}

struct Case {
    session: &'static str,
    dir: &'static str,
    /// Directory holding the taps named by `tapsSeqLast`.
    taps_dir: &'static str,
    start: Start,
    /// Settings changes applied just before the given block.
    changes: Vec<(usize, HpEqSettings)>,
}

fn build_eq<K: CascadeKernel>(start: &Start) -> HeadphoneEq<K> {
    match start {
        Start::Fresh(s) => {
            let mut eq = HeadphoneEq::<K>::new(s);
            eq.prepare(FS, 8192);
            eq
        }
        Start::Change { from, to } => {
            let mut eq = HeadphoneEq::<K>::new(from);
            eq.prepare(FS, 8192);
            let mut z = vec![0.0f32; FRAMES];
            for _ in 0..30 {
                eq.process_in_place(&mut [&mut z]);
            }
            assert!(!eq.control().is_ramping());
            eq.set_settings(to);
            eq
        }
    }
}

/// Run blocks `0..nblk` of channel 0 through `eq` (host buffers of
/// `quantum` frames), applying `changes` at block starts; `check` sees the
/// taps in force for each block.
fn run_ch0<K: CascadeKernel>(
    eq: &mut HeadphoneEq<K>,
    input: &[f32],
    nblk: usize,
    changes: &[(usize, HpEqSettings)],
    quantum: usize,
    mut check: impl FnMut(usize, &[Taps; BANDS]),
) -> Vec<f32> {
    let mut out = Vec::with_capacity(nblk * FRAMES);
    for b in 0..nblk {
        for (_, s) in changes.iter().filter(|(at, _)| *at == b) {
            eq.set_settings(s);
        }
        let mut buf = input[(b * NCH) * FRAMES..(b * NCH + 1) * FRAMES].to_vec();
        // The first chunk triggers this block's ramp tick.
        let first = quantum.min(FRAMES);
        eq.process_in_place(&mut [&mut buf[..first]]);
        check(b, eq.control().taps());
        let mut pos = first;
        while pos < FRAMES {
            let n = quantum.min(FRAMES - pos);
            eq.process_in_place(&mut [&mut buf[pos..pos + n]]);
            pos += n;
        }
        out.extend_from_slice(&buf);
    }
    out
}

fn ch(data: &[f32], c: usize, nblk: usize) -> Vec<f32> {
    (0..nblk)
        .flat_map(|b| {
            data[(b * NCH + c) * FRAMES..(b * NCH + c + 1) * FRAMES]
                .iter()
                .copied()
        })
        .collect()
}

/// Least squares `min |A x − y|` by Householder QR (A is column-major,
/// full column rank).
fn lstsq(mut a: Vec<Vec<f64>>, mut y: Vec<f64>) -> Vec<f64> {
    let n = a.len();
    let m = y.len();
    for k in 0..n {
        let norm = a[k][k..].iter().map(|v| v * v).sum::<f64>().sqrt();
        assert!(norm > 0.0, "rank-deficient basis (column {k})");
        let alpha = if a[k][k] > 0.0 { -norm } else { norm };
        let mut v: Vec<f64> = a[k][k..].to_vec();
        v[0] -= alpha;
        let vn = v.iter().map(|x| x * x).sum::<f64>();
        let refl = |col: &mut [f64]| {
            let d = col.iter().zip(&v).map(|(c, w)| c * w).sum::<f64>() * 2.0 / vn;
            for (c, w) in col.iter_mut().zip(&v) {
                *c -= d * w;
            }
        };
        for col in a.iter_mut().skip(k) {
            refl(&mut col[k..m]);
        }
        refl(&mut y[k..m]);
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let s: f64 = (i + 1..n).map(|j| a[j][i] * x[j]).sum();
        x[i] = (y[i] - s) / a[i][i];
    }
    x
}

/// Estimate the carried filter state (`z1`, `z2` of each section) at block 0
/// so that our output matches the hooked output over the first `fit_blocks`
/// blocks. The zero-input responses of these 24 words span the same space
/// for any second-order realisation.
fn fit_state<K: CascadeKernel>(
    start: &Start,
    input: &[f32],
    hooked: &[f32],
    fit_blocks: usize,
) -> Vec<f32> {
    let n = fit_blocks * FRAMES;
    let zero_in = vec![0.0f32; input.len()];
    let y0 = run_ch0(
        &mut build_eq::<K>(start),
        input,
        fit_blocks,
        &[],
        FRAMES,
        |_, _| {},
    );
    let words: Vec<usize> = (0..BANDS)
        .flat_map(|sec| (0..K::CARRIED_STATE).map(move |w| sec * K::STATE_LEN + w))
        .collect();
    const SCALE: f32 = 0.1;
    let basis: Vec<Vec<f64>> = words
        .iter()
        .map(|&j| {
            let mut eq = build_eq::<K>(start);
            eq.state_mut(0)[j] = SCALE;
            run_ch0(&mut eq, &zero_in, fit_blocks, &[], FRAMES, |_, _| {})
                .iter()
                .map(|&v| f64::from(v))
                .collect()
        })
        .collect();
    let target: Vec<f64> = (0..n)
        .map(|i| f64::from(hooked[i]) - f64::from(y0[i]))
        .collect();
    let mut st = vec![0.0f32; BANDS * K::STATE_LEN];
    for (&j, c) in words.iter().zip(lstsq(basis, target)) {
        st[j] = (c * f64::from(SCALE)) as f32;
    }
    st
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

fn rms(v: impl Iterator<Item = f64>) -> f64 {
    let (s, n) = v.fold((0.0, 0usize), |(s, n), x| (s + x * x, n + 1));
    (s / n.max(1) as f64).sqrt()
}

/// Third-octave band energies 20 Hz–20 kHz (two cascaded f64 RBJ
/// band-pass sections, 0 dB peak, for steeper skirts).
fn third_octave_energy(x: &[f32]) -> Vec<(f64, f64)> {
    let fs = f64::from(FS);
    let mut out = Vec::new();
    let mut fc = 20.0f64;
    while fc <= 20_000.0 {
        let w0 = std::f64::consts::TAU * fc / fs;
        let (s, c) = w0.sin_cos();
        let alpha = s * ((2f64.ln() / 2.0) * (1.0 / 3.0) * w0 / s).sinh();
        let a0 = 1.0 + alpha;
        let (b0, b2, a1, a2) = (alpha / a0, -alpha / a0, -2.0 * c / a0, (1.0 - alpha) / a0);
        let mut st = [[0.0f64; 4]; 2];
        let mut e = 0.0;
        for &v in x {
            let mut v = f64::from(v);
            for [x1, x2, y1, y2] in st.iter_mut() {
                let y = b0 * v + b2 * *x2 - a1 * *y1 - a2 * *y2;
                (*x2, *x1, *y2, *y1) = (*x1, v, *y1, y);
                v = y;
            }
            e += v * v;
        }
        out.push((fc, e));
        fc *= 2f64.powf(1.0 / 3.0);
    }
    out
}

struct Measured {
    residual_db: f64,
    band_lo: f64,
    band_hi: f64,
    max_mag_db: f64,
    max_mag_at: f64,
    exact: usize,
    /// First block from which every sample is bit-exact (`None`: never).
    exact_from_block: Option<usize>,
}

/// Residual over the whole case; magnitude error per third-octave band, in
/// the bands the input excites (within 20 dB of its strongest band; outside
/// them the band energy is dominated by the residual/start-up transient).
fn measure(ours: &[f32], hooked: &[f32], input: &[f32]) -> Measured {
    let err = rms(ours
        .iter()
        .zip(hooked)
        .map(|(&a, &b)| f64::from(a) - f64::from(b)));
    let sig = rms(hooked.iter().map(|&b| f64::from(b)));
    let same = |(a, b): (&f32, &f32)| a.to_bits() == b.to_bits();
    let exact = ours.iter().zip(hooked).filter(|&p| same(p)).count();
    let last_diff = ours.iter().zip(hooked).rposition(|p| !same(p));
    let nblk = ours.len() / FRAMES;
    let exact_from_block = match last_diff {
        None => Some(0),
        Some(i) if i / FRAMES + 1 < nblk => Some(i / FRAMES + 1),
        Some(_) => None,
    };
    let eo = third_octave_energy(ours);
    let eh = third_octave_energy(hooked);
    let ei = third_octave_energy(input);
    let imax = ei.iter().map(|e| e.1).fold(0.0, f64::max);
    let (mut max_mag_db, mut max_mag_at) = (0.0f64, 0.0);
    let (mut band_lo, mut band_hi) = (f64::MAX, 0.0f64);
    for (((fc, o), (_, h)), (_, i)) in eo.iter().zip(&eh).zip(&ei) {
        if *i > imax * 1e-2 {
            band_lo = band_lo.min(*fc);
            band_hi = band_hi.max(*fc);
            let d = 10.0 * (o / h).log10();
            if d.abs() > max_mag_db.abs() {
                (max_mag_db, max_mag_at) = (d, *fc);
            }
        }
    }
    Measured {
        residual_db: db(err / sig),
        band_lo,
        band_hi,
        max_mag_db,
        max_mag_at,
        exact,
        exact_from_block,
    }
}

struct Loaded {
    input: Vec<f32>,
    hooked0: Vec<f32>,
    blocks: Vec<(i64, i64)>,
    taps: HashMap<i64, [Taps; BANDS]>,
}

fn load_case(c: &Case) -> Option<Loaded> {
    let dir = case_dir(c.session, c.dir)?;
    let tdir = case_dir(c.session, c.taps_dir)?;
    let input = read_f32(&dir.join("in.f32"));
    let output = read_f32(&dir.join("out.f32"));
    let blocks = read_blocks(&dir);
    let nblk = blocks.len();
    assert_eq!(input.len(), nblk * NCH * FRAMES);
    // Channels 2–7 carry nothing in these dumps; 0 and 1 are identical.
    assert!(
        input
            .chunks_exact(FRAMES)
            .enumerate()
            .all(|(i, blk)| i % NCH < 2 || blk.iter().all(|&v| v == 0.0))
    );
    let hooked0 = ch(&output, 0, nblk);
    assert_eq!(hooked0, ch(&output, 1, nblk), "hooked ch0 != ch1");
    let input = ch(&input, 0, nblk)
        .chunks(FRAMES)
        .flat_map(|b| {
            b.iter()
                .copied()
                .chain(std::iter::repeat_n(0.0, (NCH - 1) * FRAMES))
        })
        .collect();
    Some(Loaded {
        input,
        hooked0,
        blocks,
        taps: read_taps(&tdir),
    })
}

/// Run one case with kernel `K` and host quantum `quantum`; returns the
/// measurement after checking that the taps in force match in every block.
fn run_case<K: CascadeKernel>(c: &Case, l: &Loaded, quantum: usize, label: &str) -> Measured {
    let nblk = l.blocks.len();
    let mut eq = build_eq::<K>(&c.start);
    if let Start::Change { .. } = c.start {
        let st = fit_state::<K>(&c.start, &l.input, &l.hooked0, 8.min(nblk));
        eq.state_mut(0).copy_from_slice(&st);
    }
    let mut tap_mismatch = Vec::new();
    let ours0 = run_ch0(&mut eq, &l.input, nblk, &c.changes, quantum, |b, taps| {
        if !taps_eq(taps, &l.taps[&l.blocks[b].1]) {
            tap_mismatch.push(b);
        }
    });
    let m = measure(&ours0, &l.hooked0, &ch(&l.input, 0, nblk));
    if let Some(d) = std::env::var_os("GOLDEN_DUMP") {
        let b: Vec<u8> = ours0.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(
            Path::new(&d).join(format!("{}-{label}-q{quantum}.f32", c.dir)),
            b,
        )
        .unwrap();
    }
    println!(
        "{label:5} q{quantum:<4} {:28} {nblk:4} blk: residual {:7.1} dB, bit-exact {}/{} \
         (all from block {}), max 1/3-oct error {:+.4} dB at {:.0} Hz (bands {:.0}–{:.0} Hz)",
        c.dir,
        m.residual_db,
        m.exact,
        ours0.len(),
        m.exact_from_block.map_or("-".into(), |b| b.to_string()),
        m.max_mag_db,
        m.max_mag_at,
        m.band_lo,
        m.band_hi,
    );
    assert!(
        tap_mismatch.is_empty(),
        "{}: taps differ in blocks {:?}",
        c.dir,
        &tap_mismatch[..tap_mismatch.len().min(10)]
    );
    m
}

/// How exact the G HUB kernel must be on a case.
enum Expect {
    /// Every sample bit-exact (known zero state at block 0).
    BitExact,
    /// Bit-exact from `by_block` on: the state left from before block 0 is
    /// unknown but tiny, and silent input lets both filters decay to the
    /// same state.
    ExactFrom { by_block: usize },
    /// The dump starts mid-stream under signal, so the filter state at
    /// block 0 is only estimated. The f32 rounding noise of the flat
    /// 32/64 Hz sections (poles close to the unit circle) depends on the
    /// exact state, so the two trajectories never re-converge bit for bit:
    /// the residual is the difference of two rounding-noise realisations
    /// below ~400 Hz (where these inputs have almost no energy), while
    /// 0.4–3 kHz agrees to about −100 dB. Bit-exactness on such cases needs
    /// the hooked delay line at block 0.
    Diverged { max_residual_db: f64 },
}

/// Golden check: [`IppF32`] (the default kernel) against G HUB, at G HUB's
/// own 480-frame buffers and at PipeWire-like quanta (the result must not
/// depend on the quantum). [`Tdf2F64Acc`] is checked as a reference within
/// the EQ tolerances (≤ 0.05 dB, residual ≤ −48 dB).
///
/// The E-cases' residual ceilings for TDF2 and the `Diverged` cases are
/// the measured levels with ≤ 1 dB headroom.
fn golden(c: Case, expect: Expect) {
    let Some(l) = load_case(&c) else { return };
    let nblk = l.blocks.len();
    for q in [FRAMES, 256, 1024, 37] {
        let m = run_case::<IppF32>(&c, &l, q, "ipp");
        match expect {
            Expect::BitExact => assert_eq!(
                m.exact,
                nblk * FRAMES,
                "{} q{q}: not bit-exact (residual {:.1} dB)",
                c.dir,
                m.residual_db
            ),
            Expect::ExactFrom { by_block } => assert!(
                m.exact_from_block.is_some_and(|b| b <= by_block),
                "{} q{q}: bit-exact only from block {:?}, expected by {by_block}",
                c.dir,
                m.exact_from_block
            ),
            Expect::Diverged { max_residual_db } => assert!(
                m.residual_db <= max_residual_db,
                "{} q{q}: residual {:.1} dB > {max_residual_db:.1} dB",
                c.dir,
                m.residual_db
            ),
        }
        assert!(m.max_mag_db.abs() <= 0.05, "{} q{q}: magnitude", c.dir);
    }
    let m = run_case::<Tdf2F64Acc>(&c, &l, FRAMES, "tdf2");
    assert!(
        m.max_mag_db.abs() <= 0.05,
        "{} tdf2: magnitude {:+.4} dB",
        c.dir,
        m.max_mag_db
    );
    assert!(
        m.residual_db <= -48.0,
        "{} tdf2: residual {:.1} dB",
        c.dir,
        m.residual_db
    );
}

/// E1b: fresh stream, zero state, 1 kHz +6 dB, ramp in blocks 0–10, signal
/// from block 100.
#[test]
fn c_output_e1b_stream_start() {
    golden(
        Case {
            session: R1,
            dir: "E1b-stream-start-500",
            taps_dir: "E1b-stream-start-500",
            start: Start::Fresh(adv(&[(5, 6.0)])),
            changes: vec![],
        },
        Expect::BitExact,
    );
}

/// E6 (r2): fresh stream, basic +6/−6 ramping in, signal from block 100,
/// treble −6 → 0 dB at block 695 under full signal (tap change with live
/// state).
#[test]
fn c_output_e6_start_then_change() {
    golden(
        Case {
            session: R2,
            dir: "E6-start-then-change",
            taps_dir: "E6-start-then-change",
            start: Start::Fresh(basic(6.0, -6.0)),
            changes: vec![(695, basic(6.0, 0.0))],
        },
        Expect::BitExact,
    );
}

/// E2b: ramp −6 → +6 dB at 1 kHz from block 0, estimated initial state.
#[test]
fn c_output_e2b_ramp() {
    golden(
        Case {
            session: R1,
            dir: "E2b-ramp-1k--6to+6",
            taps_dir: "E2b-ramp-1k--6to+6",
            start: Start::Change {
                from: adv(&[(5, -6.0)]),
                to: adv(&[(5, 6.0)]),
            },
            changes: vec![],
        },
        // Measured −60.3 dB.
        Expect::Diverged {
            max_residual_db: -59.5,
        },
    );
}

/// E3: 1 kHz +6 → flat from block 0; input silent until block 14 (the state
/// left from before is small but not zero).
#[test]
fn c_output_e3_flat() {
    golden(
        Case {
            session: R1,
            dir: "E3-flat",
            taps_dir: "E3-flat",
            start: Start::Change {
                from: adv(&[(5, 6.0)]),
                to: adv(&[]),
            },
            changes: vec![],
        },
        // Measured: bit-exact from block 15 (signal starts at block 14).
        Expect::ExactFrom { by_block: 15 },
    );
}

/// E4: 32 Hz +12, 16 kHz +6, constant taps, estimated initial state. Taps
/// seq 43 were pushed during the E3 recording (E3 dir, ms 165 367); E4's
/// blocks start at ms 175 004, before the next push (seq 44, ms 264 834).
#[test]
fn c_output_e4_32hz_16k() {
    let s = adv(&[(0, 12.0), (9, 6.0)]);
    golden(
        Case {
            session: R1,
            dir: "E4-32Hz+12-16k+6",
            taps_dir: "E3-flat",
            start: Start::Change { from: s, to: s },
            changes: vec![],
        },
        // Measured −59.8 dB.
        Expect::Diverged {
            max_residual_db: -59.0,
        },
    );
}

/// E5: basic bass +6, treble −6, constant taps, estimated initial state.
/// Taps seq 76 were pushed during the E4 recording (E4 dir, ms 279 743); E5's
/// blocks start at ms 288 362.
#[test]
fn c_output_e5_basic() {
    let s = basic(6.0, -6.0);
    golden(
        Case {
            session: R1,
            dir: "E5-basic-bass+6-treble-6",
            taps_dir: "E4-32Hz+12-16k+6",
            start: Start::Change { from: s, to: s },
            changes: vec![],
        },
        // Measured −59.0 dB.
        Expect::Diverged {
            max_residual_db: -58.0,
        },
    );
}
