//! The preset mapping against what G HUB actually sent to the engine.
//!
//! Local only (skipped when absent): G HUB's device preset file and reference
//! recordings of the exact bit patterns G HUB sent to the engine for
//! Broadcaster 1, FM Station and AM Radio. Every engine parameter must match
//! bit for bit. `GPROX_RE_DIR` points at the directory holding them.

use std::path::{Path, PathBuf};

use gprox_ghub::bluevoice_presets;
use serde_json::Value;

/// Local data missing: skip with a note, or fail when `GPROX_REQUIRE_VECTORS=1`
/// (set by `tools/ci.sh`), so a run that checked nothing cannot pass.
fn skip(what: impl std::fmt::Display) {
    if std::env::var_os("GPROX_REQUIRE_VECTORS").is_some_and(|v| v == "1") {
        panic!("GPROX_REQUIRE_VECTORS=1, but {what}");
    }
    eprintln!("SKIP: {what}");
}

fn re_dir() -> PathBuf {
    std::env::var_os("GPROX_RE_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../re"),
        PathBuf::from,
    )
}

fn presets_file() -> PathBuf {
    std::env::var_os("GPROX_GHUB_PRESETS").map_or_else(
        || {
            re_dir().join(
                "ghub/collect-20261120-233900/files/ProgramData_LGHUB/depots/891982/\
                 pro_headset/pro_x_wireless_headset_device_presets.json",
            )
        },
        PathBuf::from,
    )
}

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

#[test]
fn presets_match_what_ghub_sent() {
    let Ok(text) = std::fs::read_to_string(presets_file()) else {
        skip("G HUB preset file missing (local only)");
        return;
    };
    let presets = bluevoice_presets(&text).unwrap();
    assert!(presets.len() >= 8, "only {} presets", presets.len());
    for (name, case) in [
        ("broadcaster 1", "H8-full-b5state-b5in"),
        ("fm station", "H8-full-fm-b5in"),
        ("am radio", "H8-full-am-b5in"),
    ] {
        let Some(sent) = sent_bits(case) else {
            skip(format_args!("reference job {case} missing (local only)"));
            return;
        };
        let p = presets.iter().find(|p| p.matches(name)).unwrap();
        assert!(p.missing.is_empty(), "{name}: missing {:?}", p.missing);
        let mut checked = 0;
        for (engine, v) in &p.params {
            let (_, bits) = sent
                .iter()
                .find(|(n, _)| n == engine)
                .unwrap_or_else(|| panic!("{name}: {engine} not in what G HUB sent"));
            // Switches: G HUB sends integer 1/0, we hold 1.0/0.0.
            let ours = if engine.ends_with("enable") {
                u32::from(*v != 0.0)
            } else {
                v.to_bits()
            };
            assert_eq!(ours, *bits, "{name}: {engine} = {v}");
            checked += 1;
        }
        eprintln!("{name}: {checked} parameters bit-identical to what G HUB sent");
    }
}
