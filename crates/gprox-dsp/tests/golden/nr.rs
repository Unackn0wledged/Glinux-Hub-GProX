//! Golden tests for the Blue VO!CE noise reduction against reference
//! recordings of the real engine driven with test signals.
//!
//! Vectors are local and not distributed: `re/vectors/bluevoice-harness/<session>/`
//! (override that root with `GPROX_BV_VECTORS`). Every test skips with a note
//! when they are absent, so CI passes without them.
//!
//! Layout per case: `in.f32` mono input, `out.f32` channel 0 of the engine
//! output (both little-endian f32), `job.json` with the exact `set` calls
//! (`["nr.release", {"i": <f32 bits>}]`, …), and for `H7-nr-bc1-pink` also
//! `state/blkNNNNN.bin`: the engine state (134 464 bytes) after every second
//! 256-frame block. In the H7 cases every module except NR is off; the
//! always-on limiter runs with boost 1 below its ceiling, i.e. it is a pure
//! 128-sample delay (H0-off is bit-exact pass-through with that delay).
//!
//! The band-table test also reads the capture APO DLL from a local G HUB
//! install (path in `GPROX_BV_DLL`) to check that our
//! formula reproduces the engine's table; it never copies anything out.
//!
//! Run with `cargo test -p gprox-dsp --test golden_nr -- --nocapture` to see
//! the measured numbers; set `GOLDEN_DUMP=<dir>` to also write our output
//! (with the limiter delay) per case and quantum as raw f32.

use std::path::{Path, PathBuf};

use gprox_dsp::nr::{BANDS, BandTable, MAX_BAND_LEN};
use gprox_dsp::{NoiseReduction, NrSettings, NrTiming, Processor};

const SESSION: &str = "re5h-20261121-r1";
/// The always-on limiter's look-ahead.
const LIMITER_DELAY: usize = 128;
/// G HUB's engine block.
const BLOCK: usize = 256;
/// Pass mark: residual ≤ −40 dB.
const M3_BAR_DB: f64 = -40.0;

fn repo_re() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../re")
}

fn session_dir() -> Option<PathBuf> {
    let root = std::env::var_os("GPROX_BV_VECTORS")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_re().join("vectors/bluevoice-harness"));
    let d = root.join(SESSION);
    if d.is_dir() {
        Some(d)
    } else {
        eprintln!(
            "SKIP: Blue VO!CE reference vectors not found at {} (local only)",
            d.display()
        );
        None
    }
}

fn case_dir(name: &str) -> Option<PathBuf> {
    let d = session_dir()?.join(name);
    if d.is_dir() {
        Some(d)
    } else {
        eprintln!("SKIP: case {name} missing");
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

/// The f32 value of a `set` in job.json: `["name", {"i": <bits>}]`.
fn job_value(job: &str, name: &str) -> f32 {
    let pat = format!("\"{name}\"");
    let at = job
        .find(&pat)
        .unwrap_or_else(|| panic!("{name} not in job.json"));
    let rest = &job[at + pat.len()..];
    let i = rest.find("\"i\":").expect("\"i\" field") + 4;
    let num: String = rest[i..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    let bits: i64 = num.parse().expect("integer bits");
    f32::from_bits(bits as i32 as u32)
}

fn settings_from_job(dir: &Path) -> NrSettings {
    let job = std::fs::read_to_string(dir.join("job.json")).expect("job.json");
    // Every other module must be off for the case to isolate NR.
    for m in ["veq", "dpop", "exp", "des", "comp"] {
        let en = job_value(&job, &format!("{m}.enable")).to_bits();
        assert_eq!(en, 0, "{m}.enable is on in {}", dir.display());
    }
    assert_ne!(job_value(&job, "nr.enable").to_bits(), 0, "nr off");
    assert_eq!(job_value(&job, "lim.boost"), 1.0);
    NrSettings {
        release_ms: job_value(&job, "nr.release"),
        sensitivity_db: job_value(&job, "nr.sensitivity"),
        bias: job_value(&job, "nr.bias"),
        attenuation_max_db: job_value(&job, "nr.attenuation_max"),
    }
}

fn run_nr(nr: &mut NoiseReduction, x: &[f32], quantum: usize) -> Vec<f32> {
    let mut y = vec![0.0f32; x.len()];
    for (xi, yi) in x.chunks(quantum).zip(y.chunks_mut(quantum)) {
        nr.process(&[xi], &mut [yi]);
    }
    y
}

/// Residual energy relative to the reference, dB (f64 sums).
fn residual_db(ours: &[f32], reference: &[f32]) -> f64 {
    let (mut e, mut r) = (0.0f64, 0.0f64);
    for (&a, &b) in ours.iter().zip(reference) {
        let d = f64::from(a) - f64::from(b);
        e += d * d;
        r += f64::from(b) * f64::from(b);
    }
    10.0 * (e / r).log10()
}

/// Worst residual over 100 ms windows that carry signal (reference RMS above
/// −80 dBFS), dB.
fn worst_window_db(ours: &[f32], reference: &[f32]) -> f64 {
    let w = 4800;
    let mut worst = f64::NEG_INFINITY;
    for (a, b) in ours.chunks(w).zip(reference.chunks(w)) {
        let r: f64 = b.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        if r / (b.len() as f64) < 1e-8 {
            continue;
        }
        worst = worst.max(residual_db(a, b));
    }
    worst
}

struct Measured {
    total_db: f64,
    worst_db: f64,
    max_abs: f32,
}

fn measure(case: &str, quantum: usize) -> Option<Measured> {
    let dir = case_dir(case)?;
    let s = settings_from_job(&dir);
    let x = read_f32(&dir.join("in.f32"));
    let want = read_f32(&dir.join("out.f32"));
    assert_eq!(x.len(), want.len());
    let mut nr = NoiseReduction::new(1, &s);
    let y = run_nr(&mut nr, &x, quantum);
    assert_eq!(nr.underruns(), 0);
    // Our output + the limiter's 128-sample delay vs the engine output.
    let ours: Vec<f32> = std::iter::repeat_n(0.0, LIMITER_DELAY)
        .chain(y.iter().copied())
        .take(x.len())
        .collect();
    if let Some(d) = std::env::var_os("GOLDEN_DUMP") {
        let bytes: Vec<u8> = ours.iter().flat_map(|v| v.to_le_bytes()).collect();
        let f = Path::new(&d).join(format!("{case}-q{quantum}.f32"));
        std::fs::write(&f, bytes).expect("dump");
    }
    let max_abs = ours
        .iter()
        .zip(&want)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    let m = Measured {
        total_db: residual_db(&ours, &want),
        worst_db: worst_window_db(&ours, &want),
        max_abs,
    };
    eprintln!(
        "{case} ({s:?}), quantum {quantum}: residual {:.1} dB, worst 100 ms {:.1} dB, max |err| {:.2e}",
        m.total_db, m.worst_db, m.max_abs
    );
    Some(m)
}

fn check_case(case: &str) {
    for q in [BLOCK, 1024] {
        let Some(m) = measure(case, q) else { return };
        // The −40 dB bar, with a wide margin: the model reproduces the engine to
        // float rounding (about −120 dB overall). Assert well inside that so a
        // regression shows up long before the bar is at risk.
        assert!(m.total_db <= M3_BAR_DB, "{case}: −40 dB bar");
        assert!(
            m.total_db <= -100.0,
            "{case}: residual {:.1} dB",
            m.total_db
        );
        assert!(
            m.worst_db <= -80.0,
            "{case}: worst window {:.1} dB",
            m.worst_db
        );
        assert!(m.max_abs <= 1e-5, "{case}: max abs error {:.2e}", m.max_abs);
    }
}

#[test]
fn h7_nr_bc1_pink() {
    check_case("H7-nr-bc1-pink");
}

#[test]
fn h7_nr_bc1_white() {
    check_case("H7-nr-bc1-white");
}

#[test]
fn h7_nr_bc1_b5in_speech() {
    check_case("H7-nr-bc1-b5in");
}

#[test]
fn h7_nr_att30_sens12_pink() {
    check_case("H7-nr-att30-sens12-pink");
}

#[test]
fn any_quantum_timing_is_engine_plus_one_block() {
    let Some(dir) = case_dir("H7-nr-bc1-b5in") else {
        return;
    };
    let s = settings_from_job(&dir);
    let x = read_f32(&dir.join("in.f32"));
    let x = &x[..48_000 * 4];
    let mut a = NoiseReduction::new(1, &s);
    let ya = run_nr(&mut a, x, BLOCK);
    for q in [37, 480] {
        let mut b = NoiseReduction::with_timing(1, &s, NrTiming::AnyQuantum);
        let yb = run_nr(&mut b, x, q);
        assert_eq!(&yb[BLOCK..], &ya[..ya.len() - BLOCK], "quantum {q}");
    }
}

// ------------------------------------------------------------ engine state

/// NR state inside the engine state block (words).
const NR_BASE: usize = 0x3206;
const OFF_WINDOW: usize = 0x4005;
const OFF_AVG: usize = 0x5005;
const OFF_FLOOR: usize = 0x5045;
const OFF_ATT: usize = 0x5085;
const OFF_GAIN_FAST: usize = 0x50c5;
const OFF_GAIN_SLOW: usize = 0x5105;

fn state_words(path: &Path) -> Vec<f32> {
    let v = read_f32(path);
    assert_eq!(v.len(), 134_464 / 4, "{}", path.display());
    v
}

fn arr(s: &[f32], off: usize) -> &[f32] {
    &s[NR_BASE + off..NR_BASE + off + BANDS]
}

#[test]
fn init_state_window_and_offset() {
    let Some(d) = case_dir("H0-off-noise") else {
        return;
    };
    let s = state_words(&d.join("state/init.bin"));
    let ints: Vec<u32> = s[NR_BASE..NR_BASE + 2]
        .iter()
        .map(|v| v.to_bits())
        .collect();
    assert_eq!(ints, [1, 1024], "channels, N");
    let offset = s[NR_BASE + 2];
    assert!((offset - 20.0 * 512f32.log10()).abs() < 1e-5, "{offset}");
    for i in 0..1024 {
        let w = (std::f64::consts::PI * (i as f64 + 0.5) / 1024.0).sin() as f32;
        assert_eq!(s[NR_BASE + OFF_WINDOW + i], w, "window[{i}]");
    }
    for off in [OFF_AVG, OFF_FLOOR] {
        assert!(arr(&s, off).iter().all(|&v| v == -60.0));
    }
    assert!(arr(&s, OFF_ATT).iter().all(|&v| v == 0.0));
    for off in [OFF_GAIN_FAST, OFF_GAIN_SLOW] {
        assert!(arr(&s, off).iter().all(|&v| v == 1.0));
    }
}

#[test]
fn band_state_tracks_engine_dumps() {
    let Some(dir) = case_dir("H7-nr-bc1-pink") else {
        return;
    };
    let s = settings_from_job(&dir);
    let x = read_f32(&dir.join("in.f32"));
    let mut nr = NoiseReduction::new(1, &s);
    let mut out = [0.0f32; BLOCK];
    let (mut worst_db, mut worst_gain) = (0.0f32, 0.0f32);
    let mut checked = 0;
    for (blk, xi) in x.chunks(BLOCK).enumerate().take(750) {
        nr.process(&[xi], &mut [&mut out]);
        let p = dir.join(format!("state/blk{blk:05}.bin"));
        if !p.is_file() {
            continue;
        }
        let st = state_words(&p);
        let ours = nr.band_state(0);
        for (o, off) in [
            (&ours.level_avg_db, OFF_AVG),
            (&ours.level_floor_db, OFF_FLOOR),
            (&ours.attenuation_db, OFF_ATT),
        ] {
            for (a, b) in o.iter().zip(arr(&st, off)) {
                worst_db = worst_db.max((a - b).abs());
            }
        }
        for (o, off) in [
            (&ours.gain_fast, OFF_GAIN_FAST),
            (&ours.gain_slow, OFF_GAIN_SLOW),
        ] {
            for (a, b) in o.iter().zip(arr(&st, off)) {
                worst_gain = worst_gain.max((a - b).abs());
            }
        }
        checked += 1;
    }
    eprintln!(
        "band state vs engine over {checked} dumps: max |dB err| {worst_db:.2e}, max |gain err| {worst_gain:.2e}"
    );
    assert!(checked >= 300);
    assert!(worst_db < 1e-2, "{worst_db}");
    assert!(worst_gain < 1e-4, "{worst_gain}");
}

// ------------------------------------------------------------ band table

/// File offset of the band tables in the capture APO DLL
/// (`logi_audio_dts_studio_capture_apo.dll`, sha256 274dbc6f…): VA
/// 0x180054010 in `.data` (section VA 0x54000 → file offset 0x51800).
const DLL_TABLES: usize = 0x5_4010 - 0x5_4000 + 0x5_1800;

fn dll_path() -> PathBuf {
    std::env::var_os("GPROX_BV_DLL")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            repo_re().join(
                "ghub/collect-20261120-233900/files/DriverStore/\
                 logi_audio_bluevoice_1_capture_apo.inf_amd64_7a8bf4ac6779d36a/\
                 logi_audio_dts_studio_capture_apo.dll",
            )
        })
}

#[test]
fn band_table_formula_matches_engine_table() {
    let p = dll_path();
    let Ok(dll) = std::fs::read(&p) else {
        eprintln!(
            "SKIP: capture APO DLL not found at {} (local only)",
            p.display()
        );
        return;
    };
    let word = |i: usize| {
        let o = DLL_TABLES + 4 * i;
        [dll[o], dll[o + 1], dll[o + 2], dll[o + 3]]
    };
    let t = BandTable::mel();
    for b in 0..BANDS {
        assert_eq!(
            i32::from_le_bytes(word(b)) as usize,
            t.start(b),
            "start {b}"
        );
        assert_eq!(
            i32::from_le_bytes(word(32 + b)) as usize,
            t.end(b),
            "end {b}"
        );
        for (j, &w) in t.weights_padded(b).iter().enumerate() {
            let e = f32::from_le_bytes(word(64 + b * MAX_BAND_LEN + j));
            assert_eq!(w.to_bits(), e.to_bits(), "band {b} weight {j}");
        }
    }
}
