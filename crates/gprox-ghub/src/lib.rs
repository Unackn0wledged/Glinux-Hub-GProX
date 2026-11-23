//! G HUB import: settings.db / preset JSON into gprox.
//!
//! Two sources, both read from the user's own G HUB data at runtime (no G HUB
//! data is shipped with gprox):
//!
//! - G HUB's device preset file
//!   (`…/depots/<n>/pro_headset/pro_x_wireless_headset_device_presets.json`):
//!   the built-in Blue VO!CE presets, [`bluevoice_presets`].
//! - The user's `settings.db` (`%LOCALAPPDATA%\LGHUB\settings.db`): every
//!   headphone EQ and Blue VO!CE preset G HUB knows (built-in, own and
//!   downloaded) and the active selection per profile, [`settings`].
//!
//! Blue VO!CE mapping: G HUB stores each
//! preset's settings under the UI names (`voiceEq.low.gain`,
//! `deEsser.frequency`, …) and sends them unchanged to the engine under its own
//! names (`veq.low_gain`, `des.freq`, …), plus `micopt.enable` = on. Gains stay
//! linear (the engine converts), switches become 1/0. `inputGain` is not an
//! engine parameter: G HUB applies it as the Windows capture-endpoint level, so
//! it is returned separately.
//!
//! The settings are protobuf messages in proto3 JSON form:
//! a setting equal to 0 or false is left out
//! of the JSON, so an absent setting means 0 / false. settings.db leaves out
//! every such value; the device preset file mostly writes them, except in the
//! six ICEPOP presets (`balance`, `expander.thresholdAutoEnabled`).

pub mod settings;

use serde_json::Value;

/// File name of G HUB's device preset file for the PRO X Wireless, in its
/// `pro_headset` depot folder.
pub const DEVICE_PRESETS_FILE: &str = "pro_x_wireless_headset_device_presets.json";

/// File name of G HUB's battery model for the PRO X Wireless (voltage →
/// percentage and time left), next to [`DEVICE_PRESETS_FILE`].
pub const BATTERY_MODEL_FILE: &str = "pro_x_wireless_headset_battery.xml";

/// G HUB preset name prefix for the built-in Blue VO!CE presets.
const BUILTIN_PREFIX: &str = "BLUE_VOICE_PRESET_";

/// Errors while reading G HUB data.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no `presets` array in the file")]
    NoPresets,
    #[error("preset {preset}: setting `{key}` is not a number/bool")]
    Setting { preset: String, key: &'static str },
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("settings.db has no settings document (table `data` is empty)")]
    NoDocument,
    #[error("the settings document is not UTF-8 text")]
    NotText,
    #[error("no `cards.cards` list in the settings document")]
    NoCards,
}

/// One Blue VO!CE preset, as engine parameter values.
#[derive(Debug, Clone, PartialEq)]
pub struct BlueVoicePreset {
    /// G HUB's preset name (`BLUE_VOICE_PRESET_BROADCASTER_1`, `TSM Myth`, …).
    pub name: String,
    /// G HUB's preset id (UUID string).
    pub id: String,
    /// Whether G HUB marks the preset read-only (built-in or downloaded).
    pub read_only: bool,
    /// Every engine parameter (name → value) in G HUB's send order, switches
    /// 1.0/0.0. Settings absent from the JSON are 0 / off (proto3).
    pub params: Vec<(&'static str, f32)>,
    /// G HUB settings absent from the JSON (set to 0 / off in `params`).
    pub missing: Vec<&'static str>,
    /// `inputGain` (0…1): G HUB's mic level, applied outside the engine.
    pub input_gain: Option<f32>,
}

impl BlueVoicePreset {
    /// Short display name: the built-in prefix removed, e.g. `BROADCASTER_1`.
    pub fn short_name(&self) -> &str {
        self.name.strip_prefix(BUILTIN_PREFIX).unwrap_or(&self.name)
    }

    /// Whether `query` names this preset: case-insensitive, with or without the
    /// built-in prefix, spaces and underscores treated alike
    /// (`broadcaster 1`, `BROADCASTER_1`, `tsm myth`).
    pub fn matches(&self, query: &str) -> bool {
        let norm = |s: &str| s.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        let q = norm(query);
        q == norm(&self.name) || q == norm(self.short_name())
    }

    /// As a gprox Blue VO!CE preset (engine parameters, TOML form), named by
    /// [`short_name`](Self::short_name). Fails if a value is not finite or
    /// outside its range ([`gprox_core::PARAM_RANGES`]), e.g. from a damaged
    /// file or a preset with a whole band missing.
    pub fn to_core(&self) -> Result<gprox_core::BlueVoicePreset, gprox_core::BlueVoicePresetError> {
        let mut p = gprox_core::BlueVoicePreset::from_params(
            self.short_name(),
            self.params.iter().map(|&(k, v)| (k, v)),
        )?;
        p.ghub_id = (!self.id.is_empty()).then(|| self.id.clone());
        p.input_gain = self.input_gain.filter(|g| (0.0..=1.0).contains(g));
        Ok(p)
    }
}

/// (engine name, G HUB settings path) in the order G HUB sends them (by pid).
const MAP: &[(&str, &str)] = &[
    ("micopt.balance", "balance"),
    ("micopt.volume", "volume"),
    ("dpop.enable", "dePopper.enabled"),
    ("dpop.freq", "dePopper.frequency"),
    ("nr.enable", "noiseReduction.enabled"),
    ("nr.release", "noiseReduction.release"),
    ("nr.sensitivity", "noiseReduction.sensitivity"),
    ("nr.bias", "noiseReduction.bias"),
    ("nr.attenuation_max", "noiseReduction.attenuationMax"),
    ("exp.enable", "expander.enabled"),
    ("exp.attack", "expander.attack"),
    ("exp.release", "expander.release"),
    ("exp.hysteresis", "expander.hysteresis"),
    ("exp.ratio", "expander.ratio"),
    ("exp.thresh", "expander.threshold"),
    ("exp.thresh_auto_enable", "expander.thresholdAutoEnabled"),
    ("exp.thresh_auto_sense", "expander.thresholdAutoSensibility"),
    ("exp.attenuation_max", "expander.attenuationMax"),
    ("veq.enable", "voiceEq.enabled"),
    ("veq.low_gain", "voiceEq.low.gain"),
    ("veq.low_freq", "voiceEq.low.frequency"),
    ("veq.low_width", "voiceEq.low.width"),
    ("veq.mid_gain", "voiceEq.mid.gain"),
    ("veq.mid_freq", "voiceEq.mid.frequency"),
    ("veq.mid_width", "voiceEq.mid.width"),
    ("veq.high_gain", "voiceEq.high.gain"),
    ("veq.high_freq", "voiceEq.high.frequency"),
    ("veq.high_width", "voiceEq.high.width"),
    ("des.enable", "deEsser.enabled"),
    ("des.attack", "deEsser.attack"),
    ("des.release", "deEsser.release"),
    ("des.ratio", "deEsser.ratio"),
    ("des.thresh", "deEsser.threshold"),
    ("des.freq", "deEsser.frequency"),
    ("comp.enable", "compressor.enabled"),
    ("comp.attack", "compressor.attack"),
    ("comp.release", "compressor.release"),
    ("comp.ratio", "compressor.ratio"),
    ("comp.thresh", "compressor.threshold"),
    ("comp.makeup", "compressor.makeup"),
    ("lim.boost", "limiter.boost"),
    ("lim.attack", "limiter.attack"),
    ("lim.release", "limiter.release"),
];

fn lookup<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |v, k| v.get(k))
}

fn number(v: &Value) -> Option<f32> {
    match v {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        // JSON numbers are f64; G HUB sends the f32 nearest to them. One
        // beyond f32 (e.g. 1e39) would become ±inf: not a value.
        Value::Number(n) => n.as_f64().map(|x| x as f32).filter(|x| x.is_finite()),
        _ => None,
    }
}

/// All Blue VO!CE presets in a G HUB device preset file, in file order.
/// Entries of other kinds (e.g. EQ presets) are skipped.
pub fn bluevoice_presets(json: &str) -> Result<Vec<BlueVoicePreset>, Error> {
    let root: Value = serde_json::from_str(json)?;
    let list = root
        .get("presets")
        .and_then(Value::as_array)
        .ok_or(Error::NoPresets)?;
    let mut out = Vec::new();
    for p in list {
        if let Some(settings) = lookup(p, "blueVoiceSettings.settings") {
            out.push(bluevoice_preset(p, settings)?);
        }
    }
    Ok(out)
}

/// One Blue VO!CE preset from a G HUB preset/card object `p` (name, id,
/// readOnly) and its `blueVoiceSettings.settings` object.
pub(crate) fn bluevoice_preset(p: &Value, settings: &Value) -> Result<BlueVoicePreset, Error> {
    let name = p
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let id = p.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
    let read_only = p.get("readOnly").and_then(Value::as_bool).unwrap_or(false);
    let mut params = Vec::with_capacity(MAP.len() + 1);
    let mut missing = Vec::new();
    params.push(("micopt.enable", 1.0));
    for &(engine, path) in MAP {
        let v = match lookup(settings, path) {
            None | Some(Value::Null) => {
                missing.push(path);
                0.0
            }
            Some(v) => number(v).ok_or_else(|| Error::Setting {
                preset: name.clone(),
                key: path,
            })?,
        };
        params.push((engine, v));
    }
    let input_gain = settings.get("inputGain").and_then(number);
    Ok(BlueVoicePreset {
        name,
        id,
        read_only,
        params,
        missing,
        input_gain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A made-up preset in G HUB's file layout (not G HUB data).
    const FIXTURE: &str = r#"{
      "knownPresets": [],
      "presets": [
        { "id": "eq-1", "name": "SOME_EQ", "attribute": "EQ" },
        { "id": "11111111-2222-3333-4444-555555555555", "name": "BLUE_VOICE_PRESET_TEST_ONE",
          "attribute": "BLUE_VOICE_SETTINGS", "readOnly": true,
          "blueVoiceSettings": { "settings": {
            "balance": 0, "volume": 0.5,
            "voiceEq": { "enabled": true,
              "low": { "frequency": 100, "gain": 2.0, "width": 1.5 },
              "mid": { "frequency": 1000, "gain": 1, "width": 1 },
              "high": { "frequency": 5000, "gain": 0.5, "width": 1 } },
            "dePopper": { "enabled": false, "frequency": 90 },
            "noiseReduction": { "enabled": true, "release": 100, "sensitivity": 10, "bias": 2, "attenuationMax": -12 },
            "expander": { "enabled": true, "attack": 1, "release": 50, "hysteresis": 3, "ratio": 2,
              "threshold": -40, "thresholdAutoEnabled": true, "thresholdAutoSensibility": 10, "attenuationMax": -15 },
            "deEsser": { "enabled": false, "attack": 1, "release": 100, "ratio": 4, "threshold": -10, "frequency": 6000 },
            "compressor": { "enabled": true, "attack": 5, "release": 100, "ratio": 3, "threshold": -12, "makeup": 1.5 },
            "limiter": { "boost": 2, "attack": 2, "release": 100 },
            "inputGain": 0.6
          } } }
      ]
    }"#;

    #[test]
    fn parses_fixture() {
        let v = bluevoice_presets(FIXTURE).unwrap();
        assert_eq!(v.len(), 1);
        let p = &v[0];
        assert_eq!(p.short_name(), "TEST_ONE");
        assert!(
            p.matches("test one") && p.matches("BLUE_VOICE_PRESET_TEST_ONE") && !p.matches("test")
        );
        assert_eq!(p.params.len(), MAP.len() + 1);
        let get = |n: &str| p.params.iter().find(|(k, _)| *k == n).unwrap().1;
        assert_eq!(get("micopt.enable"), 1.0);
        assert_eq!(get("dpop.enable"), 0.0);
        assert_eq!(get("exp.thresh_auto_enable"), 1.0);
        assert_eq!(get("veq.low_width"), 1.5);
        assert_eq!(get("comp.makeup"), 1.5);
        assert_eq!(p.input_gain, Some(0.6));
    }

    #[test]
    fn absent_setting_is_zero_and_reported() {
        let v = bluevoice_presets(&FIXTURE.replace("\"makeup\": 1.5", "\"makeupX\": 1.5")).unwrap();
        assert_eq!(v[0].missing, ["compressor.makeup"]);
        assert_eq!(v[0].params.len(), MAP.len() + 1);
        let makeup = v[0].params.iter().find(|(k, _)| *k == "comp.makeup");
        assert_eq!(makeup, Some(&("comp.makeup", 0.0)));
    }

    #[test]
    fn to_core_keeps_every_parameter() {
        let p = &bluevoice_presets(FIXTURE).unwrap()[0];
        assert!(p.read_only);
        let c = p.to_core().unwrap();
        assert_eq!(c.name, "TEST_ONE");
        assert_eq!(c.ghub_id.as_deref(), Some(p.id.as_str()));
        assert_eq!(c.input_gain, Some(0.6));
        let mut ours: Vec<_> = p.params.iter().map(|&(k, v)| (k.to_owned(), v)).collect();
        ours.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(c.params(), ours);
    }

    /// JSON numbers beyond f32 (`1e39` used to become inf and panic
    /// later) are an error for that setting, not a value.
    #[test]
    fn number_beyond_f32_is_an_error() {
        for big in ["1e39", "-1e39"] {
            let bad = FIXTURE.replace("\"makeup\": 1.5", &format!("\"makeup\": {big}"));
            assert!(
                matches!(
                    bluevoice_presets(&bad),
                    Err(Error::Setting {
                        key: "compressor.makeup",
                        ..
                    })
                ),
                "{big}"
            );
        }
    }

    #[test]
    fn non_number_is_an_error() {
        let bad = FIXTURE.replace("\"makeup\": 1.5", "\"makeup\": \"x\"");
        assert!(matches!(
            bluevoice_presets(&bad),
            Err(Error::Setting {
                key: "compressor.makeup",
                ..
            })
        ));
    }
}
