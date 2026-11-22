//! Blue VO!CE preset as engine parameters (TOML).
//!
//! The values are what G HUB sends to the Blue VO!CE engine for a preset,
//! keyed by the engine's parameter names (`<block>.<param>`, e.g.
//! `veq.low_gain`), in G HUB's units: linear gains, Hz, ms, dB, ratios;
//! switches as 1.0 / 0.0. `gprox import ghub` writes this form; gproxd applies
//! it over the engine defaults, the same way as a preset from G HUB's own
//! preset file.
//!
//! ```toml
//! name = "BROADCASTER_1"
//! ghub_id = "259c0286-80b9-4f65-8fa8-da7789e8b762"   # optional
//! input_gain = 0.75                                   # optional, see below
//!
//! [engine.micopt]
//! enable = 1.0
//! volume = 0.7
//!
//! [engine.veq]
//! enable = 1.0
//! low_gain = 2.0
//! ```
//!
//! Parameter names aren't checked here (the engine owns the list and rejects
//! unknown names when the preset is applied); values must be finite.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A Blue VO!CE preset: engine parameter values by block and name.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlueVoicePreset {
    /// Display name (G HUB's preset name for imported presets).
    #[serde(default)]
    pub name: String,
    /// G HUB's preset (card) id, for imported presets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ghub_id: Option<String>,
    /// G HUB's `inputGain` (0…1): the Windows mic level G HUB sets with the
    /// preset. Not an engine parameter; informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_gain: Option<f32>,
    /// `engine.<block>.<param>` = value. Order doesn't matter: the engine
    /// takes the parameters by name.
    #[serde(default)]
    pub engine: BTreeMap<String, BTreeMap<String, f32>>,
}

/// Errors from [`BlueVoicePreset`] parsing and validation.
#[derive(Debug, thiserror::Error)]
pub enum BlueVoicePresetError {
    #[error("TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("TOML: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("parameter name {0:?} is not `<block>.<param>`")]
    BadName(String),
    #[error("parameter {name}: {value} is not finite")]
    NotFinite { name: String, value: f32 },
    #[error("input_gain {0} is outside 0…1")]
    InputGain(f32),
}

impl BlueVoicePreset {
    /// Build from `(engine name, value)` pairs, e.g. `("veq.low_gain", 2.0)`.
    pub fn from_params<'a>(
        name: impl Into<String>,
        params: impl IntoIterator<Item = (&'a str, f32)>,
    ) -> Result<Self, BlueVoicePresetError> {
        let mut p = BlueVoicePreset {
            name: name.into(),
            ..Default::default()
        };
        for (full, v) in params {
            let (block, param) = split_name(full)?;
            p.engine
                .entry(block.to_owned())
                .or_default()
                .insert(param.to_owned(), v);
        }
        p.validate()?;
        Ok(p)
    }

    /// All parameters as `(engine name, value)`, sorted by name.
    pub fn params(&self) -> Vec<(String, f32)> {
        self.engine
            .iter()
            .flat_map(|(b, m)| m.iter().map(move |(k, v)| (format!("{b}.{k}"), *v)))
            .collect()
    }

    /// Parse and validate a TOML preset.
    pub fn from_toml_str(s: &str) -> Result<Self, BlueVoicePresetError> {
        let p: Self = toml::from_str(s)?;
        p.validate()?;
        Ok(p)
    }

    /// Serialise to TOML.
    pub fn to_toml_string(&self) -> Result<String, BlueVoicePresetError> {
        Ok(toml::to_string(self)?)
    }

    /// Names must be `<block>.<param>`, values finite, `input_gain` in 0…1.
    pub fn validate(&self) -> Result<(), BlueVoicePresetError> {
        for (b, m) in &self.engine {
            for (k, &v) in m {
                let name = format!("{b}.{k}");
                split_name(&name)?;
                if !v.is_finite() {
                    return Err(BlueVoicePresetError::NotFinite { name, value: v });
                }
            }
        }
        match self.input_gain {
            Some(g) if !(0.0..=1.0).contains(&g) => Err(BlueVoicePresetError::InputGain(g)),
            _ => Ok(()),
        }
    }
}

fn split_name(full: &str) -> Result<(&str, &str), BlueVoicePresetError> {
    match full.split_once('.') {
        Some((b, p)) if !b.is_empty() && !p.is_empty() && !p.contains('.') => Ok((b, p)),
        _ => Err(BlueVoicePresetError::BadName(full.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BlueVoicePreset {
        let mut p = BlueVoicePreset::from_params(
            "TEST",
            [
                ("micopt.enable", 1.0),
                ("micopt.volume", 0.7),
                ("veq.mid_gain", 0.631_626_7_f32),
                ("exp.thresh", -48.0),
                ("comp.makeup", 1.122_018_5),
            ],
        )
        .unwrap();
        p.ghub_id = Some("11111111-2222-3333-4444-555555555555".into());
        p.input_gain = Some(0.75);
        p
    }

    #[test]
    fn round_trip_is_bit_exact() {
        let p = sample();
        let s = p.to_toml_string().unwrap();
        assert!(s.contains("[engine.veq]"), "{s}");
        let q = BlueVoicePreset::from_toml_str(&s).unwrap();
        assert_eq!(q, p);
        for ((a, x), (b, y)) in p.params().iter().zip(q.params().iter()) {
            assert_eq!((a, x.to_bits()), (b, y.to_bits()));
        }
    }

    #[test]
    fn params_flatten() {
        let p = sample();
        let names: Vec<_> = p.params().into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            [
                "comp.makeup",
                "exp.thresh",
                "micopt.enable",
                "micopt.volume",
                "veq.mid_gain"
            ]
        );
    }

    #[test]
    fn integers_and_minimal_files() {
        let p = BlueVoicePreset::from_toml_str("[engine.micopt]\nenable = 1\n").unwrap();
        assert_eq!(p.params(), [("micopt.enable".to_owned(), 1.0)]);
        assert_eq!(p.name, "");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(BlueVoicePreset::from_params("x", [("nodot", 1.0)]).is_err());
        assert!(BlueVoicePreset::from_params("x", [("a.b.c", 1.0)]).is_err());
        assert!(BlueVoicePreset::from_params("x", [("a.b", f32::NAN)]).is_err());
        assert!(BlueVoicePreset::from_toml_str("input_gain = 1.5").is_err());
        assert!(BlueVoicePreset::from_toml_str("gain = 1.0").is_err());
        assert!(BlueVoicePreset::from_toml_str("[engine.veq]\nlow_gain = \"x\"").is_err());
    }
}
