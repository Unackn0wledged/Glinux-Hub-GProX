//! Preset model (serde/TOML), units, parameter ranges and validation.
//!
//! [`HeadphoneEqPreset`]: the G HUB headphone EQ.
//!
//! [`control`]: the gproxd control-socket protocol and client.

pub mod control;

use serde::{Deserialize, Serialize};

mod bluevoice;
pub use bluevoice::{BlueVoicePreset, BlueVoicePresetError};

/// `node.name` of the gprox sink apps play into ("PRO X (gprox)").
pub const SINK_NODE: &str = "gprox_render";
/// `node.name` of the gprox source apps record from ("PRO X Mic (gprox)").
pub const SOURCE_NODE: &str = "gprox_mic";

/// gprox's config directory: `$XDG_CONFIG_HOME/gprox`, else
/// `$HOME/.config/gprox`. `None` when neither variable is set. Holds
/// `state.toml`, `hp-eq/`, `bluevoice/` and the files `gprox setup` copies from
/// G HUB.
pub fn config_dir() -> Option<std::path::PathBuf> {
    let var = |k| std::env::var_os(k).filter(|d| !d.is_empty());
    var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| var("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .map(|d| d.join("gprox"))
}

/// Which headphone EQ is active. G HUB `SelectedEQ`: 0 = basic, 1 = advanced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EqMode {
    #[default]
    Basic,
    Advanced,
}

/// Headphone EQ preset. Gains in dB, in G HUB's registry order.
///
/// ```toml
/// mode = "advanced"
/// basic = [0.0, 0.0]          # 200 Hz low shelf, 4.5 kHz high shelf
/// advanced = [0.0, 0.0, 0.0, 0.0, 0.0, 6.0, 0.0, 0.0, 0.0, 0.0]
///                             # 32 64 125 250 500 1k 2k 4k 8k 16k
/// ```
///
/// Missing fields default to basic mode and 0 dB.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeadphoneEqPreset {
    pub mode: EqMode,
    /// `BasicEqParams`: bass (200 Hz low shelf), treble (4.5 kHz high shelf).
    pub basic: [f32; 2],
    /// `AdvancedEqParams`: 32, 64, 125, 250, 500, 1k, 2k, 4k, 8k, 16k.
    pub advanced: [f32; 10],
}

impl HeadphoneEqPreset {
    /// Largest accepted gain magnitude. G HUB ignores larger values per band.
    pub const MAX_GAIN_DB: f32 = 12.0;
    /// UI labels of the basic bands.
    pub const BASIC_LABELS: [&'static str; 2] = ["Bass", "Treble"];
    /// UI labels of the advanced bands.
    pub const ADVANCED_LABELS: [&'static str; 10] = [
        "32 Hz", "64 Hz", "125 Hz", "250 Hz", "500 Hz", "1 kHz", "2 kHz", "4 kHz", "8 kHz",
        "16 kHz",
    ];

    /// Parse and validate a TOML preset.
    pub fn from_toml_str(s: &str) -> Result<Self, PresetError> {
        let p: Self = toml::from_str(s)?;
        p.validate()?;
        Ok(p)
    }

    /// Serialise to TOML.
    pub fn to_toml_string(&self) -> Result<String, PresetError> {
        Ok(toml::to_string(self)?)
    }

    /// Every gain must be finite and within ±12 dB.
    pub fn validate(&self) -> Result<(), PresetError> {
        let bands = Self::BASIC_LABELS
            .iter()
            .zip(&self.basic)
            .map(|(l, g)| ("basic", *l, *g))
            .chain(
                Self::ADVANCED_LABELS
                    .iter()
                    .zip(&self.advanced)
                    .map(|(l, g)| ("advanced", *l, *g)),
            );
        for (set, band, gain_db) in bands {
            if !gain_db.is_finite() || gain_db.abs() > Self::MAX_GAIN_DB {
                return Err(PresetError::GainOutOfRange { set, band, gain_db });
            }
        }
        Ok(())
    }
}

/// Preset loading/validation errors.
#[derive(Debug, thiserror::Error)]
pub enum PresetError {
    #[error("TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("TOML: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("{set} EQ band {band}: {gain_db} dB is outside ±12 dB")]
    GainOutOfRange {
        set: &'static str,
        band: &'static str,
        gain_db: f32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut p = HeadphoneEqPreset {
            mode: EqMode::Advanced,
            basic: [6.0, -6.0],
            ..Default::default()
        };
        p.advanced[5] = 6.0;
        p.advanced[9] = -1.5;
        let s = p.to_toml_string().unwrap();
        assert!(s.contains("mode = \"advanced\""), "{s}");
        assert_eq!(HeadphoneEqPreset::from_toml_str(&s).unwrap(), p);
    }

    #[test]
    fn defaults_and_partial_files() {
        assert_eq!(
            HeadphoneEqPreset::from_toml_str("").unwrap(),
            HeadphoneEqPreset::default()
        );
        let p = HeadphoneEqPreset::from_toml_str("basic = [3.0, -2.5]").unwrap();
        assert_eq!(p.mode, EqMode::Basic);
        assert_eq!(p.basic, [3.0, -2.5]);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(
            HeadphoneEqPreset::from_toml_str("basic = [12.5, 0.0]"),
            Err(PresetError::GainOutOfRange { band: "Bass", .. })
        ));
        assert!(HeadphoneEqPreset::from_toml_str("advanced = [0.0]").is_err());
        assert!(HeadphoneEqPreset::from_toml_str("mode = \"loud\"").is_err());
        assert!(HeadphoneEqPreset::from_toml_str("gain = 1.0").is_err());
    }
}
