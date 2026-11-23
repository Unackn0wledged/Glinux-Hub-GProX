//! Golden tests for the Blue VO!CE chain (with NR) against reference
//! recordings of the real engine driven with test signals.
//!
//! Vectors are local and not distributed:
//! `re/vectors/bluevoice-harness/re5h-20261121-r1/<case>/{in.f32,out.f32,job.json}`
//! (override the session directory with `GPROX_BV_VECTORS`). Tests skip with a
//! note when they are absent, so CI passes without them.
//!
//! `in.f32` is planar f32 (`ch × frames`), `out.f32` planar `ch × nblk·256`
//! (the engine's output for the zero-padded input), `job.json` the exact
//! parameter bits (`sets`), the smoothing time and mid-signal `events`
//! (applied before engine block `blk`).
//!
//! Bar: residual ≤ −50 dB relative to the reference output,
//! static gain-reduction curves within 0.2 dB on the staircase signals.
//! Cases with the NR on (H7 in the chain, H8 full chain, H9 stereo) use the
//! −40 dB bar. Each case is held tighter than that: bit-exact where it is
//! today, else about 8 dB above its measured residual; a dynamics case must
//! also move its block's gain by 3 dB, and
//! `cases_notice_wrong_dynamics_parameters` checks that wrong settings fail.
//! Run with `-- --nocapture` for the numbers; set `GPROX_BV_DUMP=<dir>` to
//! write our output per case as `<case>.ours.f32` (planar) for inspection.

use std::path::{Path, PathBuf};

use gprox_dsp::Processor;
use gprox_dsp::bluevoice::{BlueVoice, BvParams, DEFAULT_SMOOTHING_MS};

const BLOCK: usize = 256;
const FS: f64 = 48_000.0;
const FIFO: usize = BlueVoice::FIFO_LATENCY;

// ---------------------------------------------------------------- tiny JSON

#[derive(Debug, Clone)]
enum J {
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
    Other,
}

impl J {
    fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(v) => v.iter().find(|(n, _)| n == k).map(|(_, j)| j),
            _ => None,
        }
    }
    fn num(&self) -> f64 {
        match self {
            J::Num(n) => *n,
            _ => panic!("not a number: {self:?}"),
        }
    }
    fn arr(&self) -> &[J] {
        match self {
            J::Arr(v) => v,
            _ => panic!("not an array: {self:?}"),
        }
    }
    fn str(&self) -> &str {
        match self {
            J::Str(s) => s,
            _ => panic!("not a string: {self:?}"),
        }
    }
}

fn parse_json(s: &str) -> J {
    fn ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    }
    fn val(b: &[u8], i: &mut usize) -> J {
        ws(b, i);
        match b[*i] {
            b'{' => {
                *i += 1;
                let mut v = Vec::new();
                loop {
                    ws(b, i);
                    if b[*i] == b'}' {
                        *i += 1;
                        return J::Obj(v);
                    }
                    let k = match val(b, i) {
                        J::Str(s) => s,
                        o => panic!("key {o:?}"),
                    };
                    ws(b, i);
                    assert_eq!(b[*i], b':');
                    *i += 1;
                    v.push((k, val(b, i)));
                    ws(b, i);
                    if b[*i] == b',' {
                        *i += 1;
                    }
                }
            }
            b'[' => {
                *i += 1;
                let mut v = Vec::new();
                loop {
                    ws(b, i);
                    if b[*i] == b']' {
                        *i += 1;
                        return J::Arr(v);
                    }
                    v.push(val(b, i));
                    ws(b, i);
                    if b[*i] == b',' {
                        *i += 1;
                    }
                }
            }
            b'"' => {
                *i += 1;
                let st = *i;
                while b[*i] != b'"' {
                    *i += 1;
                }
                *i += 1;
                J::Str(String::from_utf8_lossy(&b[st..*i - 1]).into_owned())
            }
            c if c == b'-' || c.is_ascii_digit() => {
                let st = *i;
                while *i < b.len()
                    && (b[*i] == b'-'
                        || b[*i] == b'+'
                        || b[*i] == b'.'
                        || b[*i] == b'e'
                        || b[*i] == b'E'
                        || b[*i].is_ascii_digit())
                {
                    *i += 1;
                }
                J::Num(std::str::from_utf8(&b[st..*i]).unwrap().parse().unwrap())
            }
            _ => {
                while *i < b.len() && b[*i].is_ascii_alphabetic() {
                    *i += 1;
                }
                J::Other
            }
        }
    }
    let mut i = 0;
    val(s.as_bytes(), &mut i)
}

// ---------------------------------------------------------------- loading

/// Local data missing: skip with a note, or fail when `GPROX_REQUIRE_VECTORS=1`
/// (set by `tools/ci.sh`), so a run that checked nothing cannot pass.
fn skip(what: impl std::fmt::Display) {
    if std::env::var_os("GPROX_REQUIRE_VECTORS").is_some_and(|v| v == "1") {
        panic!("GPROX_REQUIRE_VECTORS=1, but {what}");
    }
    eprintln!("SKIP: {what}");
}

fn session_dir() -> PathBuf {
    std::env::var_os("GPROX_BV_VECTORS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("GPROX_RE_DIR")
                .map_or_else(
                    || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../re"),
                    PathBuf::from,
                )
                .join("vectors/bluevoice-harness/re5h-20261121-r1")
        })
}

fn read_f32(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Apply a `sets` array (`[[name, {"i": bits}], …]`) to a parameter set.
fn apply_sets(p: &mut BvParams, sets: &J) {
    for s in sets.arr() {
        let a = s.arr();
        let bits = a[1].get("i").expect("raw bits").num() as i64 as i32 as u32;
        p.set_raw(a[0].str(), bits)
            .unwrap_or_else(|e| panic!("{e}"));
    }
}

struct Case {
    name: String,
    ch: usize,
    input: Vec<Vec<f32>>,
    reference: Vec<Vec<f32>>,
    ours: Vec<Vec<f32>>,
    /// Lowest gain each dynamics block reached during the run, in dB, indexed
    /// by [`Dyn`].
    min_gain_db: [f64; 4],
}

fn load_and_run(name: &str) -> Option<Case> {
    load_and_run_with(name, &|_| {})
}

/// Like [`load_and_run`] with `mutate` applied to the case's parameters (for
/// the sensitivity checks).
fn load_and_run_with(name: &str, mutate: &dyn Fn(&mut BvParams)) -> Option<Case> {
    let dir = session_dir().join(name);
    if !dir.is_dir() {
        skip(format_args!(
            "{} missing (gitignored, local only)",
            dir.display()
        ));
        return None;
    }
    let job = parse_json(&std::fs::read_to_string(dir.join("job.json")).unwrap());
    let ch = job.get("ch").map_or(1, |j| j.num() as usize);
    let frames = job.get("frames").unwrap().num() as usize;
    let smoothing = job
        .get("smoothing")
        .map_or(DEFAULT_SMOOTHING_MS, |j| j.num() as f32);
    let raw_in = read_f32(&dir.join("in.f32"));
    let raw_out = read_f32(&dir.join("out.f32"));
    let nblk = frames.div_ceil(BLOCK);
    let total = nblk * BLOCK;
    assert_eq!(raw_in.len(), ch * frames);
    assert_eq!(raw_out.len(), ch * total, "{name}: out.f32 size");
    let input: Vec<Vec<f32>> = (0..ch)
        .map(|c| raw_in[c * frames..(c + 1) * frames].to_vec())
        .collect();
    let reference: Vec<Vec<f32>> = (0..ch)
        .map(|c| raw_out[c * total..(c + 1) * total].to_vec())
        .collect();

    let mut p = BvParams::default();
    apply_sets(&mut p, job.get("sets").unwrap());
    mutate(&mut p);
    let mut events: Vec<(usize, &J)> = job
        .get("events")
        .map(|e| {
            e.arr()
                .iter()
                .map(|ev| {
                    (
                        ev.get("blk").unwrap().num() as usize,
                        ev.get("sets").unwrap(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    events.sort_by_key(|e| e.0);

    let mut bv = BlueVoice::with_smoothing(&p, smoothing);
    bv.prepare(48_000, 8192);
    // Padded input: the engine saw zeros after `frames`, plus FIFO frames to
    // flush our re-blocking delay.
    let delay = FIFO;
    let len = total + delay;
    let padded: Vec<Vec<f32>> = input
        .iter()
        .map(|x| {
            let mut v = x.clone();
            v.resize(len, 0.0);
            v
        })
        .collect();
    let mut out = vec![vec![0.0f32; len]; 2];
    let mut min_gain = [1.0f32; 4];
    let mut pos = 0;
    let mut ev = events.into_iter().peekable();
    // Host quantum deliberately not a divisor of the block: 480 frames.
    while pos < len {
        let mut end = (pos + 480).min(len);
        if let Some(&(b, _)) = ev.peek() {
            if b * BLOCK > pos && b * BLOCK < end {
                end = b * BLOCK;
            }
            if b * BLOCK == pos {
                let (_, sets) = ev.next().unwrap();
                let mut q = *bv.params();
                apply_sets(&mut q, sets);
                mutate(&mut q);
                bv.apply(&q);
            }
        }
        let ins: Vec<&[f32]> = padded.iter().map(|x| &x[pos..end]).collect();
        let [l, r] = &mut out[..] else { unreachable!() };
        bv.process(&ins, &mut [&mut l[pos..end], &mut r[pos..end]]);
        for (m, g) in min_gain.iter_mut().zip(bv.gains()) {
            *m = m.min(g);
        }
        pos = end;
    }
    let ours = (0..ch.max(2)).map(|c| out[c][delay..].to_vec()).collect();
    Some(Case {
        name: name.to_owned(),
        ch,
        input,
        reference,
        ours,
        min_gain_db: min_gain.map(|g| db(f64::from(g))),
    })
}

// ---------------------------------------------------------------- measures

fn rms(v: impl Iterator<Item = f64>) -> f64 {
    let (s, n) = v.fold((0.0, 0usize), |(s, n), x| (s + x * x, n + 1));
    (s / n.max(1) as f64).sqrt()
}

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-300).log10()
}

struct Measured {
    residual_db: f64,
    max_err: f64,
    exact: usize,
    total: usize,
}

fn measure(c: &Case) -> Measured {
    let (mut e2, mut s2, mut exact, mut total, mut max_err) = (0.0f64, 0.0f64, 0, 0, 0.0f64);
    for k in 0..c.ch {
        for (&a, &b) in c.ours[k].iter().zip(&c.reference[k]) {
            let d = f64::from(a) - f64::from(b);
            e2 += d * d;
            s2 += f64::from(b) * f64::from(b);
            max_err = max_err.max(d.abs());
            exact += usize::from(a.to_bits() == b.to_bits());
            total += 1;
        }
    }
    if let Some(dir) = std::env::var_os("GPROX_BV_DUMP") {
        let mut b = Vec::new();
        for ch in &c.ours {
            for v in ch {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        std::fs::write(Path::new(&dir).join(format!("{}.ours.f32", c.name)), b).unwrap();
    }
    if c.ch == 1 {
        // Mono in → identical L/R out.
        assert_eq!(c.ours[0], c.ours[1], "{}: L != R", c.name);
    }
    Measured {
        residual_db: if e2 == 0.0 {
            f64::NEG_INFINITY
        } else {
            10.0 * (e2 / s2).log10()
        },
        max_err,
        exact,
        total,
    }
}

/// Static curve of a staircase: per step, output level − input level in dB,
/// RMS over the second half of each step (shifted by `delay` frames).
fn static_curve(x: &[f32], y: &[f32], step_s: f64, delay: usize) -> Vec<(f64, f64)> {
    let n = (step_s * FS) as usize;
    let mut v = Vec::new();
    let mut k = 0;
    while (k + 1) * n + delay <= y.len() && (k + 1) * n <= x.len() {
        let (a, b) = (k * n + n / 2, (k + 1) * n);
        let li = db(rms(x[a..b].iter().map(|&s| f64::from(s))));
        let lo = db(rms(y[a + delay..b + delay].iter().map(|&s| f64::from(s))));
        v.push((li, lo - li));
        k += 1;
    }
    v
}

/// Largest deviation between our and the reference static curve, over the
/// steps whose input is above `floor_db` dBFS. Returns (max |Δ|, at level).
fn curve_error(c: &Case, step_s: f64, delay: usize, floor_db: f64) -> (f64, f64) {
    let ours = static_curve(&c.input[0], &c.ours[0], step_s, delay);
    let refc = static_curve(&c.input[0], &c.reference[0], step_s, delay);
    let mut worst = (0.0f64, 0.0);
    for ((li, go), (_, gr)) in ours.iter().zip(&refc) {
        if *li > floor_db && (go - gr).abs() > worst.0.abs() {
            worst = (go - gr, *li);
        }
    }
    worst
}

// ---------------------------------------------------------------- cases

/// The dynamics blocks, in the order of `BlueVoice::gains`.
#[derive(Debug, Clone, Copy)]
enum Dyn {
    Exp = 0,
    Des = 1,
    Comp = 2,
    Lim = 3,
}

/// How much a case's own dynamics block must move its gain for the case to
/// say anything about that block (a compressor below its
/// threshold is a pure delay and matches whatever its settings are).
const ENGAGE_DB: f64 = -3.0;

struct Expect {
    /// Maximum residual in dB. The bars are −50 dB (without NR) and −40 dB
    /// (with NR); each case's bar is tightened to 5–10 dB above what it
    /// measures today, so a regression that is still under the bar is caught.
    residual_db: f64,
    /// Require bit-exact output (where it holds today).
    exact: bool,
    /// Staircase step length in s, if the case is a static-curve case.
    stair_s: Option<f64>,
    /// The dynamics block this case is about: its gain must reach
    /// [`ENGAGE_DB`].
    engages: Option<Dyn>,
}

const BAR: Expect = Expect {
    residual_db: -50.0,
    exact: false,
    stair_s: None,
    engages: None,
};

/// Bit-exact today: asserted.
const EXACT: Expect = Expect { exact: true, ..BAR };

/// Not bit-exact; `db` is the tightened residual bar.
const fn below(db: f64) -> Expect {
    Expect {
        residual_db: db,
        ..BAR
    }
}

/// What failed, if anything.
fn verdict(c: &Case, e: &Expect) -> (String, Vec<String>) {
    let name = &c.name;
    let m = measure(c);
    let mut line = format!(
        "{name:28} residual {:7.1} dB  max|err| {:.2e}  exact {}/{}",
        m.residual_db, m.max_err, m.exact, m.total
    );
    let mut fails = Vec::new();
    if let Some(step) = e.stair_s {
        // Gain-reduction curve over steps above −80 dBFS input. Levels are
        // measured on the second half of each step, compensating the chain's
        // look-ahead (256 frames for exp/comp, 128 for the limiter).
        let (d, at) = curve_error(c, step, 256, -80.0);
        line += &format!("  static curve max Δ {d:+.3} dB @ {at:.0} dBFS");
        if d.abs() > 0.2 {
            fails.push(format!("static curve off by {d:.3} dB at {at:.0} dBFS"));
        }
    }
    if let Some(d) = e.engages {
        let g = c.min_gain_db[d as usize];
        line += &format!("  {d:?} gain down to {g:.1} dB");
        if g > ENGAGE_DB {
            fails.push(format!(
                "{d:?} gain only reaches {g:.1} dB: the case doesn't exercise it"
            ));
        }
    }
    if e.exact && m.exact != m.total {
        fails.push(format!("not bit-exact ({}/{})", m.exact, m.total));
    }
    if m.residual_db > e.residual_db {
        fails.push(format!(
            "residual {:.1} dB > {} dB",
            m.residual_db, e.residual_db
        ));
    }
    (line, fails)
}

fn golden(name: &str, e: Expect) {
    let Some(c) = load_and_run(name) else {
        return;
    };
    let (line, fails) = verdict(&c, &e);
    eprintln!("{line}");
    assert!(fails.is_empty(), "{name}: {}", fails.join("; "));
}

#[test]
fn h0_off_noise_is_a_pure_delay() {
    golden("H0-off-noise", EXACT);
}

/// Reference and output are all digital silence: only checks that "off"
/// adds nothing.
#[test]
fn h0_off_silence() {
    golden("H0-off-silence", EXACT);
}

#[test]
fn h1_voice_eq() {
    for c in [
        "H1-veq-bc1-sweep",
        "H1-veq-bc1-noise",
        "H1-veq-fm-sweep",
        "H1-veq-fm-noise",
        "H1-veq-am-sweep",
        "H1-veq-am-noise",
        "H1-veq-bc1-smooth50-step",
    ] {
        golden(c, EXACT);
    }
}

#[test]
fn h2_de_popper() {
    // Measured −102.0, −112.8, −117.1, −112.4 dB.
    golden("H2-dpop-85-thumps", below(-95.0));
    golden("H2-dpop-85-sweep", below(-105.0));
    golden("H2-dpop-150-sweep", below(-110.0));
    golden("H2-dpop-smooth50-step", below(-105.0));
}

const fn stair(step_s: f64, d: Dyn) -> Expect {
    Expect {
        stair_s: Some(step_s),
        engages: Some(d),
        ..EXACT
    }
}

const fn engages(d: Dyn, e: Expect) -> Expect {
    Expect {
        engages: Some(d),
        ..e
    }
}

#[test]
fn h3_expander() {
    golden("H3-exp-bc1-stair", stair(0.4, Dyn::Exp));
    golden("H3-exp-r2-t50-stair", stair(0.4, Dyn::Exp));
    golden("H3-exp-bc1-bursts", engages(Dyn::Exp, EXACT));
    // The expander gates here, but the auto-threshold makes no difference to
    // this case's output (switching it off is still bit-exact): it has no
    // golden coverage until a new case is recorded.
    golden("H3-exp-auto-bursts", engages(Dyn::Exp, EXACT));
}

#[test]
fn h4_compressor() {
    golden("H4-comp-bc1-stair", stair(0.4, Dyn::Comp));
    golden("H4-comp-r10-t20-stair", stair(0.4, Dyn::Comp));
    // The bursts peak at −10 dBFS, below Broadcaster 1's −7 dB threshold:
    // the compressor is a pure delay here, so this case only checks that
    // (no `engages`). Compressor time constants are covered by a10-r50.
    golden("H4-comp-bc1-bursts", EXACT);
    golden("H4-comp-a10-r50-bursts", engages(Dyn::Comp, EXACT));
}

#[test]
fn h5_de_esser() {
    golden("H5-des-bc1-sibilant", engages(Dyn::Des, EXACT));
    // Measured −191.0 dB.
    golden("H5-des-5k-t15-sibilant", engages(Dyn::Des, below(-183.0)));
}

#[test]
fn h6_limiter() {
    golden("H6-lim-boost1-stairhot", stair(0.3, Dyn::Lim));
    golden("H6-lim-bc1-stairhot", stair(0.3, Dyn::Lim));
    // Measured −154.1 dB.
    golden("H6-lim-boost1-burstshot", engages(Dyn::Lim, below(-146.0)));
    golden("H6-lim-a1-r50-burstshot", engages(Dyn::Lim, EXACT));
}

// Cases with the NR on: the bar is −40 dB; each is tightened to about 8 dB
// above its measured residual. About half of the samples in some reference
// cases are digital silence, so their printed "exact" counts say little.

#[test]
fn h7_noise_reduction_in_the_chain() {
    // Measured −121.1, −120.9, −114.8, −117.4 dB.
    golden("H7-nr-bc1-pink", below(-113.0));
    golden("H7-nr-bc1-white", below(-113.0));
    golden("H7-nr-bc1-b5in", below(-107.0));
    golden("H7-nr-att30-sens12-pink", below(-110.0));
}

/// Full chain on recorded speech. The de-esser and compressor stay nearly
/// idle on this input, so these cases mainly cover the
/// de-popper, NR, expander, voice EQ and limiter together.
#[test]
fn h8_full_chain() {
    // Measured −89.1, −89.1, −82.6, −100.9 dB.
    golden("H8-full-b5state-b5in", below(-81.0));
    golden("H8-full-b5state-b5in-smooth0", below(-81.0));
    golden("H8-full-fm-b5in", below(-75.0));
    golden("H8-full-am-b5in", below(-93.0));
}

#[test]
fn h9_stereo_full_chain() {
    // Measured −90.2, −89.9 dB.
    golden("H9-stereo-bc1", below(-82.0));
    golden("H9-stereo-bc1-bal0.5", below(-82.0));
}

/// Each case that claims to cover a dynamics parameter must fail when that
/// parameter is wrong (several cases used to pass bit-exact with a ratio,
/// attack or release off by ×2). Some parameters can't be checked with the
/// existing recordings yet.
#[test]
fn cases_notice_wrong_dynamics_parameters() {
    let scale = |name: &'static str, k: f32| {
        move |p: &mut BvParams| {
            let id = BvParams::id(name).unwrap();
            p.set_f(id, p.f(id) * k);
        }
    };
    let shift = |name: &'static str, d: f32| {
        move |p: &mut BvParams| {
            let id = BvParams::id(name).unwrap();
            p.set_f(id, p.f(id) + d);
        }
    };
    type Mutation = Box<dyn Fn(&mut BvParams)>;
    let cases: Vec<(&str, Expect, &str, Mutation)> = vec![
        (
            "H3-exp-bc1-stair",
            stair(0.4, Dyn::Exp),
            "exp.ratio ×2",
            Box::new(scale("exp.ratio", 2.0)),
        ),
        (
            "H3-exp-bc1-stair",
            stair(0.4, Dyn::Exp),
            "exp.thresh +3 dB",
            Box::new(shift("exp.thresh", 3.0)),
        ),
        (
            "H3-exp-bc1-bursts",
            engages(Dyn::Exp, EXACT),
            "exp.release ×2",
            Box::new(scale("exp.release", 2.0)),
        ),
        (
            "H4-comp-bc1-stair",
            stair(0.4, Dyn::Comp),
            "comp.ratio ×2",
            Box::new(scale("comp.ratio", 2.0)),
        ),
        (
            "H4-comp-r10-t20-stair",
            stair(0.4, Dyn::Comp),
            "comp.thresh +3 dB",
            Box::new(shift("comp.thresh", 3.0)),
        ),
        (
            "H4-comp-a10-r50-bursts",
            engages(Dyn::Comp, EXACT),
            "comp.attack ×2",
            Box::new(scale("comp.attack", 2.0)),
        ),
        (
            "H4-comp-a10-r50-bursts",
            engages(Dyn::Comp, EXACT),
            "comp.release ×2",
            Box::new(scale("comp.release", 2.0)),
        ),
        (
            "H5-des-bc1-sibilant",
            engages(Dyn::Des, EXACT),
            "des.ratio ×2",
            Box::new(scale("des.ratio", 2.0)),
        ),
        (
            "H5-des-bc1-sibilant",
            engages(Dyn::Des, EXACT),
            "des.thresh +3 dB",
            Box::new(shift("des.thresh", 3.0)),
        ),
        (
            "H6-lim-a1-r50-burstshot",
            engages(Dyn::Lim, EXACT),
            "lim.release ×2",
            Box::new(scale("lim.release", 2.0)),
        ),
        (
            "H6-lim-a1-r50-burstshot",
            engages(Dyn::Lim, EXACT),
            "lim.attack ×2",
            Box::new(scale("lim.attack", 2.0)),
        ),
    ];
    for (name, e, what, mutate) in cases {
        let Some(c) = load_and_run_with(name, &*mutate) else {
            return;
        };
        let (line, fails) = verdict(&c, &e);
        eprintln!("{line}  [{what}]");
        assert!(!fails.is_empty(), "{name} still passes with {what}");
    }
}

/// gproxd's mic node: mono in, one output channel (balance 0), host quantum
/// 256 and an odd one. Must equal the reference L channel within the −40 dB bar.
#[test]
fn h8_mono_node_output() {
    // Bars as in h8_full_chain.
    for (name, quantum, bar) in [
        ("H8-full-b5state-b5in", 256, -81.0),
        ("H8-full-fm-b5in", 1000, -75.0),
    ] {
        let dir = session_dir().join(name);
        if !dir.is_dir() {
            skip(format_args!(
                "{} missing (gitignored, local only)",
                dir.display()
            ));
            return;
        }
        let job = parse_json(&std::fs::read_to_string(dir.join("job.json")).unwrap());
        let mut p = BvParams::default();
        apply_sets(&mut p, job.get("sets").unwrap());
        let x = read_f32(&dir.join("in.f32"));
        let reference = read_f32(&dir.join("out.f32"));
        let mut bv = BlueVoice::new(&p);
        bv.prepare(48_000, 8192);
        let len = reference.len() + FIFO;
        let mut xin = x.clone();
        xin.resize(len, 0.0);
        let mut y = vec![0.0f32; len];
        for (xi, yi) in xin.chunks(quantum).zip(y.chunks_mut(quantum)) {
            bv.process(&[xi], &mut [yi]);
        }
        let ours = &y[FIFO..];
        let (mut e2, mut s2) = (0.0f64, 0.0f64);
        for (&a, &b) in ours.iter().zip(&reference) {
            let d = f64::from(a) - f64::from(b);
            e2 += d * d;
            s2 += f64::from(b) * f64::from(b);
        }
        let res = 10.0 * (e2 / s2).log10();
        eprintln!("{name} mono node, quantum {quantum}: residual {res:.1} dB");
        assert!(res <= bar, "{name}: {res:.1} dB > {bar} dB");
    }
}
