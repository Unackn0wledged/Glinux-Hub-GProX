//! Presets imported from G HUB's settings.db against what G HUB used.
//!
//! Local only (every test skips when its data is absent):
//! - settings.db snapshots (db + WAL) taken at known G HUB states
//!   (`settings-snapshots/<tag>/`, override: `GPROX_GHUB_SNAPSHOTS`);
//! - reference recordings of the exact bits G HUB sent to the Blue VO!CE
//!   engine for Broadcaster 1, FM Station and AM Radio;
//! - the headphone EQ taps G HUB computed for known settings.
//!
//! `GPROX_RE_DIR` points all three at the directory holding them.
//!
//! Every preset goes through the gprox TOML form (write, read back) before it
//! is compared, so the check covers what `gprox import ghub` writes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gprox_core::{BlueVoicePreset as CorePreset, HeadphoneEqPreset};
use gprox_dsp::biquad::Taps;
use gprox_dsp::hp_eq::{BANDS, HpEqControl};
use gprox_dsp::{EqMode, HpEqSettings, RampBlock};
use gprox_ghub::settings::{self, GhubSettings, PRO_X_WIRELESS};
use serde_json::Value;

fn re_dir() -> PathBuf {
    std::env::var_os("GPROX_RE_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../re"),
        PathBuf::from,
    )
}

fn snapshots_dir() -> PathBuf {
    std::env::var_os("GPROX_GHUB_SNAPSHOTS")
        .map_or_else(|| re_dir().join("ghub/settings-snapshots"), PathBuf::from)
}

fn load(tag: &str) -> Option<GhubSettings> {
    let db = snapshots_dir().join(tag).join("settings.db");
    if !db.is_file() {
        eprintln!("SKIP: {} missing (local only)", db.display());
        return None;
    }
    let doc = settings::read_settings_db(&db).unwrap();
    Some(settings::parse(&doc.json).unwrap())
}

const TAGS: [&str; 8] = [
    "full-20261120",
    "e3-flat",
    "e4-32+12-16k+6",
    "e5-basic-bass6-trebleminus6",
    "b2-broadcaster1",
    "b2-fmstation",
    "b2-amradio",
    "b4-inputgain89",
];

fn eq_via_toml(p: &HeadphoneEqPreset) -> HeadphoneEqPreset {
    HeadphoneEqPreset::from_toml_str(&p.to_toml_string().unwrap()).unwrap()
}

fn bv_via_toml(p: &gprox_ghub::BlueVoicePreset) -> CorePreset {
    CorePreset::from_toml_str(&p.to_core().to_toml_string().unwrap()).unwrap()
}

/// Every EQ and Blue VO!CE card in every snapshot imports, and survives the
/// TOML round trip bit for bit.
#[test]
fn every_preset_imports() {
    let mut seen = 0;
    for tag in TAGS {
        let Some(s) = load(tag) else { continue };
        seen += 1;
        assert!(s.skipped.is_empty(), "{tag}: skipped {:?}", s.skipped);
        assert!(s.eq.len() >= 9, "{tag}: {} EQ presets", s.eq.len());
        assert_eq!(s.bluevoice.len(), 28, "{tag}");
        for p in &s.eq {
            assert_eq!(eq_via_toml(&p.preset), p.preset, "{tag}: {}", p.name);
        }
        for p in &s.bluevoice {
            assert_eq!(p.params.len(), 44, "{tag}: {}", p.name);
            let c = bv_via_toml(p);
            let mut ours: Vec<_> = p.params.iter().map(|&(k, v)| (k, v.to_bits())).collect();
            ours.sort();
            let back: Vec<_> = c
                .params()
                .into_iter()
                .map(|(k, v)| (k, v.to_bits()))
                .collect();
            let back: Vec<_> = back.iter().map(|(k, b)| (k.as_str(), *b)).collect();
            assert_eq!(back, ours, "{tag}: {}", p.name);
        }
        eprintln!(
            "{tag}: {} EQ + {} Blue VO!CE presets imported",
            s.eq.len(),
            s.bluevoice.len()
        );
    }
    if seen == 0 {
        eprintln!("SKIP: no settings.db snapshots");
    }
}

/// The active PRO X Wireless selection matches the G HUB state of each
/// snapshot.
#[test]
fn active_selection_matches_known_states() {
    // (tag, EQ card id prefix, Blue VO!CE card id prefix)
    let cases = [
        ("e3-flat", "0c94ec70", "259c0286"),
        // Collected after E5 had started: shows the Basic EQ.
        ("e4-32+12-16k+6", "597de365", "259c0286"),
        ("e5-basic-bass6-trebleminus6", "597de365", "259c0286"),
        ("b2-broadcaster1", "597de365", "259c0286"),
        ("b2-fmstation", "597de365", "b55ab762"),
        ("b2-amradio", "597de365", "4121c0a3"),
        ("b4-inputgain89", "597de365", "4121c0a3"),
    ];
    for (tag, eq, bv) in cases {
        let Some(s) = load(tag) else { continue };
        let sel = s
            .selections
            .iter()
            .find(|x| x.device == PRO_X_WIRELESS)
            .unwrap();
        let eq_id = sel.eq.as_deref().unwrap();
        let bv_id = sel.bluevoice.as_deref().unwrap();
        assert!(eq_id.starts_with(eq), "{tag}: EQ {eq_id}");
        assert!(bv_id.starts_with(bv), "{tag}: Blue VO!CE {bv_id}");
        assert_eq!(sel.bluevoice_enabled, Some(true), "{tag}");
        assert!(s.eq_by_id(eq_id).is_some() && s.bluevoice_by_id(bv_id).is_some());
    }
}

// ------------------------------------------------------------- Blue VO!CE

fn sent_bits(case: &str) -> Option<Vec<(String, u32)>> {
    let p = re_dir()
        .join("vectors/bluevoice-harness/re5h-20261121-r1")
        .join(case)
        .join("job.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    Some(
        v["sets"]
            .as_array()?
            .iter()
            .map(|s| {
                let bits = s[1]["i"].as_i64().unwrap() as i32 as u32;
                (s[0].as_str().unwrap().to_owned(), bits)
            })
            .collect(),
    )
}

/// Broadcaster 1, FM Station and AM Radio from settings.db, through the TOML
/// form: every engine parameter is bit-identical to what G HUB sent.
#[test]
fn bluevoice_matches_what_ghub_sent() {
    // Each preset from the snapshot taken while it was active.
    for (tag, id, case) in [
        ("b2-broadcaster1", "259c0286", "H8-full-b5state-b5in"),
        ("b2-fmstation", "b55ab762", "H8-full-fm-b5in"),
        ("b2-amradio", "4121c0a3", "H8-full-am-b5in"),
    ] {
        let Some(s) = load(tag) else { continue };
        let Some(sent) = sent_bits(case) else {
            eprintln!("SKIP: reference job {case} missing (local only)");
            continue;
        };
        let p = s.bluevoice.iter().find(|p| p.id.starts_with(id)).unwrap();
        let imported = bv_via_toml(p);
        let params = imported.params();
        assert_eq!(params.len(), 44);
        for (engine, v) in &params {
            let (_, bits) = sent
                .iter()
                .find(|(n, _)| n == engine)
                .unwrap_or_else(|| panic!("{}: {engine} not in what G HUB sent", p.name));
            // Switches: G HUB sends integer 1/0, gprox holds 1.0/0.0.
            let ours = if engine.ends_with("enable") {
                u32::from(*v != 0.0)
            } else {
                v.to_bits()
            };
            assert_eq!(ours, *bits, "{}: {engine} = {v}", p.name);
        }
        eprintln!(
            "{tag}: {} — {} parameters bit-identical to what G HUB sent ({} absent in the card, imported as 0/off)",
            p.name,
            params.len(),
            p.missing.len()
        );
    }
}

// ------------------------------------------------------------- headphone EQ

fn read_f32(path: &Path) -> Vec<f32> {
    std::fs::read(path)
        .unwrap()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Hooked tap pushes of one case: seq → 12 sections.
fn hooked_taps(dir: &Path) -> Option<HashMap<i64, [Taps; BANDS]>> {
    let jsonl = std::fs::read_to_string(dir.join("taps.jsonl")).ok()?;
    let raw = read_f32(&dir.join("taps.bin"));
    let seqs: Vec<i64> = jsonl
        .lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["seq"]
                .as_i64()
                .unwrap()
        })
        .collect();
    assert_eq!(raw.len(), seqs.len() * BANDS * 6);
    Some(
        seqs.iter()
            .zip(raw.chunks_exact(BANDS * 6))
            .map(|(&s, push)| {
                let mut t = [Taps::IDENTITY; BANDS];
                for (k, six) in push.chunks_exact(6).enumerate() {
                    t[k] = Taps::from_six(six.try_into().unwrap());
                }
                (s, t)
            })
            .collect(),
    )
}

fn dsp_settings(p: &HeadphoneEqPreset) -> HpEqSettings {
    HpEqSettings {
        mode: match p.mode {
            gprox_core::EqMode::Basic => EqMode::Basic,
            gprox_core::EqMode::Advanced => EqMode::Advanced,
        },
        basic: p.basic,
        advanced: p.advanced,
    }
}

/// Imported EQ presets give the taps G HUB's APO used (the headphone EQ golden cases):
/// settled final taps equal the hooked ones bit for bit.
#[test]
fn eq_presets_give_hooked_taps() {
    let vectors = re_dir().join("vectors/eq/re5-20261121-r1");
    // (snapshot, card id prefix, case dir, hooked seq): the card whose gains
    // equal the state G HUB had when that tap push was hooked.
    let cases = [
        ("e3-flat", "0c94ec70", "E3-flat", 21), // EQUALIZER_FLAT, active in E3
        ("e3-flat", "2c36c3da", "E1b-stream-start-500", 21), // FLAT edited: 1 kHz +6
        ("e3-flat", "5dafea22", "E3-flat", 32), // Flat Copy 1: 32 Hz +12
        ("e4-32+12-16k+6", "c93a93b7", "E3-flat", 43), // FLAT edited: 32 Hz +12, 16k +6 (E4)
        (
            "e5-basic-bass6-trebleminus6",
            "597de365",
            "E4-32Hz+12-16k+6",
            76,
        ), // Basic +6/-6 (E5)
    ];
    for (tag, id, case, seq) in cases {
        let Some(s) = load(tag) else { continue };
        let Some(hooked) = hooked_taps(&vectors.join(case)) else {
            eprintln!("SKIP: EQ vectors {case} missing (local only)");
            continue;
        };
        let card = s.eq.iter().find(|p| p.id.starts_with(id)).unwrap();
        let preset = eq_via_toml(&card.preset);
        let mut c = HpEqControl::new(48_000, RampBlock::Auto, &dsp_settings(&preset));
        for _ in 0..100 {
            c.tick();
        }
        assert!(!c.is_ramping());
        let want = &hooked[&seq];
        let differ: Vec<usize> = (0..BANDS)
            .filter(|&k| !c.taps()[k].bit_eq(&want[k]))
            .collect();
        assert!(
            differ.is_empty(),
            "{tag} {} ({id}) vs {case} seq {seq}: bands {differ:?} differ",
            card.name
        );
        eprintln!(
            "{tag}: {} ({:?}) = hooked taps {case} seq {seq}, 72/72 bit-identical",
            card.display_name(),
            preset.mode
        );
    }
}
