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
//! Names must be engine parameters and values inside [`PARAM_RANGES`]: G HUB's
//! own slider limits where its UI defines them, so every preset G HUB writes
//! passes, and values the DSP can't run (a zero filter width, a frequency
//! above half the sample rate, a +40 dB output volume) are refused when the
//! preset is read rather than turning the mic output into NaN.

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
    #[error("unknown Blue VO!CE parameter {0:?}")]
    Unknown(String),
    #[error("parameter {name}: {value} is outside {lo}…{hi}")]
    OutOfRange {
        name: String,
        value: f32,
        lo: f32,
        hi: f32,
    },
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

    /// Names must be engine parameters, values finite and inside
    /// [`PARAM_RANGES`], `input_gain` in 0…1.
    pub fn validate(&self) -> Result<(), BlueVoicePresetError> {
        for (b, m) in &self.engine {
            for (k, &v) in m {
                let name = format!("{b}.{k}");
                split_name(&name)?;
                if !v.is_finite() {
                    return Err(BlueVoicePresetError::NotFinite { name, value: v });
                }
                let (lo, hi) = param_range(&name)
                    .ok_or_else(|| BlueVoicePresetError::Unknown(name.clone()))?;
                if !(lo..=hi).contains(&v) {
                    return Err(BlueVoicePresetError::OutOfRange {
                        name,
                        value: v,
                        lo,
                        hi,
                    });
                }
            }
        }
        match self.input_gain {
            Some(g) if !(0.0..=1.0).contains(&g) => Err(BlueVoicePresetError::InputGain(g)),
            _ => Ok(()),
        }
    }
}

/// 10^(±15/20): G HUB's voice-EQ gain limits (±15 dB, linear), widened by
/// one part in 10⁶ so the `f32` values G HUB stores pass.
const VEQ_GAIN: (f32, f32) = (0.177_827_9 * (1.0 - 1e-6), 5.623_413 * (1.0 + 1e-6));
/// 10^(±20/20): G HUB's limiter boost slider (±20 dB, linear).
const BOOST: (f32, f32) = (0.1 * (1.0 - 1e-6), 10.0 * (1.0 + 1e-6));

/// Allowed value range of every engine parameter, inclusive.
///
/// From G HUB's Blue VO!CE panel definition (in its UI bundle): voice EQ
/// frequencies, widths and ±15 dB
/// gains, de-popper frequency, NR, expander, de-esser, compressor and
/// limiter sliders. All 243 Blue VO!CE presets we have seen (G HUB's
/// built-in, downloaded and own) are inside them. Not in the panel, so
/// chosen here to be safe for the DSP and wider than any preset seen:
/// `micopt.volume` 0…+12 dB (presets 0.65–1), `micopt.balance` −1…1,
/// `comp.makeup` 0…+20 dB (presets 1), `exp.thresh_auto_sense` 0…60 dB
/// (presets 20). Switches are 0 or 1. `micopt.meter_enable` (meters only) is
/// accepted and ignored by the engine.
pub const PARAM_RANGES: &[(&str, f32, f32)] = &[
    ("micopt.enable", 0.0, 1.0),
    ("micopt.balance", -1.0, 1.0),
    ("micopt.volume", 0.0, 3.981_072),
    ("micopt.meter_enable", 0.0, 1.0),
    ("dpop.enable", 0.0, 1.0),
    ("dpop.freq", 20.0, 400.0),
    ("nr.enable", 0.0, 1.0),
    ("nr.release", 0.0, 1000.0),
    ("nr.sensitivity", 0.0, 40.0),
    ("nr.bias", 0.0, 5.0),
    ("nr.attenuation_max", -60.0, 0.0),
    ("exp.enable", 0.0, 1.0),
    ("exp.attack", 0.0, 200.0),
    ("exp.release", 50.0, 1000.0),
    ("exp.hysteresis", 0.0, 200.0),
    ("exp.ratio", 1.0, 20.0),
    ("exp.thresh", -72.0, -24.0),
    ("exp.thresh_auto_enable", 0.0, 1.0),
    ("exp.thresh_auto_sense", 0.0, 60.0),
    ("exp.attenuation_max", -60.0, 0.0),
    ("veq.enable", 0.0, 1.0),
    ("veq.low_gain", VEQ_GAIN.0, VEQ_GAIN.1),
    ("veq.low_freq", 80.0, 600.0),
    ("veq.low_width", 0.5, 10.0),
    ("veq.mid_gain", VEQ_GAIN.0, VEQ_GAIN.1),
    ("veq.mid_freq", 400.0, 5000.0),
    ("veq.mid_width", 0.5, 10.0),
    ("veq.high_gain", VEQ_GAIN.0, VEQ_GAIN.1),
    ("veq.high_freq", 3000.0, 12_000.0),
    ("veq.high_width", 0.5, 10.0),
    ("des.enable", 0.0, 1.0),
    ("des.attack", 0.0, 200.0),
    ("des.release", 50.0, 1000.0),
    ("des.ratio", 1.0, 20.0),
    ("des.thresh", -24.0, 24.0),
    ("des.freq", 3000.0, 12_000.0),
    ("comp.enable", 0.0, 1.0),
    ("comp.attack", 0.0, 200.0),
    ("comp.release", 50.0, 1000.0),
    ("comp.ratio", 1.0, 20.0),
    ("comp.thresh", -48.0, 0.0),
    ("comp.makeup", 0.0, 10.0),
    ("lim.boost", BOOST.0, BOOST.1),
    ("lim.attack", 0.0, 10.0),
    ("lim.release", 10.0, 250.0),
];

/// The allowed range of an engine parameter, `None` if there is no such
/// parameter.
pub fn param_range(name: &str) -> Option<(f32, f32)> {
    PARAM_RANGES
        .iter()
        .find(|&&(n, _, _)| n == name)
        .map(|&(_, lo, hi)| (lo, hi))
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
        assert!(BlueVoicePreset::from_params("x", [("nr.bias", f32::NAN)]).is_err());
        assert!(BlueVoicePreset::from_params("x", [("a.b", 1.0)]).is_err());
        assert!(BlueVoicePreset::from_toml_str("input_gain = 1.5").is_err());
        assert!(BlueVoicePreset::from_toml_str("gain = 1.0").is_err());
        assert!(BlueVoicePreset::from_toml_str("[engine.veq]\nlow_gain = \"x\"").is_err());
    }

    /// Values the DSP can't run and names the engine doesn't have are
    /// refused when a preset is read.
    #[test]
    fn rejects_out_of_range_values_and_unknown_names() {
        for (name, v) in [
            ("veq.low_width", 0.0),
            ("veq.mid_freq", 0.0),
            ("veq.mid_width", 0.0),
            ("des.freq", 30_000.0),
            ("dpop.freq", 30_000.0),
            ("dpop.freq", -80.0),
            ("micopt.volume", 100.0),
            ("micopt.balance", 2.0),
            ("comp.ratio", 0.5),
            ("veq.high_gain", 10.0),
        ] {
            let r = BlueVoicePreset::from_params("x", [(name, v)]);
            assert!(
                matches!(r, Err(BlueVoicePresetError::OutOfRange { .. })),
                "{name} = {v}: {r:?}"
            );
        }
        let r = BlueVoicePreset::from_toml_str("[engine.veq]\nlow_q = 1.0\n");
        assert!(matches!(r, Err(BlueVoicePresetError::Unknown(_))), "{r:?}");
        // The range ends themselves are fine, as are G HUB's ±15 dB gains.
        BlueVoicePreset::from_params(
            "x",
            [
                ("veq.low_freq", 80.0),
                ("veq.high_freq", 12_000.0),
                ("veq.low_gain", 0.177_827_94),
                ("veq.mid_gain", 5.623_413),
                ("micopt.meter_enable", 1.0),
            ],
        )
        .unwrap();
    }
}
