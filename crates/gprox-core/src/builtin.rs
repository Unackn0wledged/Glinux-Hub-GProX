//! gprox's own presets, built into the binaries.
//!
//! Three headphone EQ presets (Movies, FPS, Music) and three Blue VO!CE
//! presets (Natural, Broadcast, Noisy Room), designed for gprox (the TOML files are in
//! `crates/gprox-core/presets/`). They make gprox useful without any G HUB
//! files.
//!
//! Every built-in name starts with [`PREFIX`] ("gprox "), so they can't be
//! mistaken for G HUB presets or the user's own, and they can't be replaced:
//! a user or imported preset whose name matches a built-in one (see
//! [`is_builtin_name`]) is not listed. Save features should refuse such names.

use crate::{BlueVoicePreset, HeadphoneEqPreset};

/// Prefix of every built-in preset name.
pub const PREFIX: &str = "gprox ";

/// Headphone EQ presets: (name, TOML).
const HP_EQ: &[(&str, &str)] = &[
    ("gprox Movies", include_str!("../presets/hp-eq/movies.toml")),
    ("gprox FPS", include_str!("../presets/hp-eq/fps.toml")),
    ("gprox Music", include_str!("../presets/hp-eq/music.toml")),
];

/// Blue VO!CE presets (TOML; the name is in the file).
const BLUEVOICE: &[&str] = &[
    include_str!("../presets/bluevoice/natural.toml"),
    include_str!("../presets/bluevoice/broadcast.toml"),
    include_str!("../presets/bluevoice/noisy-room.toml"),
];

/// The built-in headphone EQ presets, in menu order. The files are checked
/// by this crate's tests, so parsing can't fail at run time.
pub fn hp_eq_presets() -> Vec<(String, HeadphoneEqPreset)> {
    HP_EQ
        .iter()
        .map(|(name, toml)| {
            let p = HeadphoneEqPreset::from_toml_str(toml)
                .unwrap_or_else(|e| panic!("built-in preset {name}: {e}"));
            ((*name).to_owned(), p)
        })
        .collect()
}

/// The built-in Blue VO!CE presets, in menu order.
pub fn bluevoice_presets() -> Vec<BlueVoicePreset> {
    BLUEVOICE
        .iter()
        .map(|toml| {
            BlueVoicePreset::from_toml_str(toml)
                .unwrap_or_else(|e| panic!("built-in Blue VO!CE preset: {e}"))
        })
        .collect()
}

/// Case-insensitive, spaces/dashes/underscores alike (as gproxd matches
/// preset names).
fn norm(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace([' ', '-'], "_")
}

/// Whether `name` is (or would be matched as) a built-in preset name, of
/// either kind.
pub fn is_builtin_name(name: &str) -> bool {
    let q = norm(name);
    HP_EQ.iter().any(|(n, _)| norm(n) == q)
        || bluevoice_presets().iter().any(|p| norm(&p.name) == q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EqMode;

    #[test]
    fn every_builtin_parses_and_validates() {
        let hp = hp_eq_presets();
        assert_eq!(hp.len(), 3);
        for (name, p) in &hp {
            assert!(name.starts_with(PREFIX), "{name}");
            assert_eq!(p.mode, EqMode::Advanced, "{name}");
            p.validate().unwrap();
            // A cut-only design: no band boosts.
            assert!(
                p.advanced.iter().all(|&g| g <= 0.0),
                "{name}: {:?}",
                p.advanced
            );
        }
        let bv = bluevoice_presets();
        assert_eq!(bv.len(), 3);
        for p in &bv {
            assert!(p.name.starts_with(PREFIX), "{}", p.name);
            p.validate().unwrap();
            assert_eq!(p.ghub_id, None);
            // Every value is set explicitly, so the preset doesn't depend on
            // the engine defaults (meter_enable and balance excepted).
            let names: Vec<String> = p.params().into_iter().map(|(n, _)| n).collect();
            for (n, _, _) in crate::PARAM_RANGES {
                if !["micopt.meter_enable", "micopt.balance"].contains(n) {
                    assert!(names.iter().any(|x| x == n), "{}: {n} missing", p.name);
                }
            }
        }
    }

    #[test]
    fn names_are_unique_and_recognised() {
        let mut all: Vec<String> = hp_eq_presets().into_iter().map(|(n, _)| n).collect();
        all.extend(bluevoice_presets().into_iter().map(|p| p.name));
        let mut normed: Vec<String> = all.iter().map(|n| norm(n)).collect();
        normed.sort();
        normed.dedup();
        assert_eq!(normed.len(), all.len());
        assert!(is_builtin_name("gprox movies"));
        assert!(is_builtin_name("GPROX_NOISY-ROOM"));
        assert!(!is_builtin_name("Movies"));
        assert!(!is_builtin_name("FPS"));
    }
}
