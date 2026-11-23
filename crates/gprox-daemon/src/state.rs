//! Persistent daemon state: the last selections, `<config dir>/state.toml`.
//!
//! ```toml
//! hp_eq = "bass boost"          # headphone EQ preset name
//! bluevoice = "BROADCASTER_1"   # Blue VO!CE preset (short G HUB name)
//! bluevoice_enabled = true
//! ghub_presets = "/path/to/pro_x_wireless_headset_device_presets.json"
//! battery_model = "/path/to/pro_x_wireless_headset_battery.xml"
//! sidetone = 30                 # last sidetone level set through gprox
//! always_on_sidetone = true     # keep the headset mic stream open
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub hp_eq: Option<String>,
    pub bluevoice: Option<String>,
    pub bluevoice_enabled: bool,
    pub ghub_presets: Option<PathBuf>,
    /// G HUB's battery model, when given with `--battery-model`.
    pub battery_model: Option<PathBuf>,
    /// Sidetone level the user set through gprox; re-applied whenever the
    /// headset connects (as G HUB does). `None` = never set, never written.
    pub sidetone: Option<u8>,
    /// Keep the headset's mic stream open so its sidetone works with no app
    /// recording. Off when missing.
    pub always_on_sidetone: bool,
}

/// `$XDG_CONFIG_HOME/gprox`, else `$HOME/.config/gprox`.
pub fn default_config_dir() -> anyhow::Result<PathBuf> {
    gprox_core::config_dir().context("neither XDG_CONFIG_HOME nor HOME is set; pass --config-dir")
}

impl State {
    /// Read `path`. A missing file is the default state; a broken one is
    /// reported and replaced by the default (it is rewritten on the next
    /// change).
    pub fn load(path: &Path) -> State {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable state file");
                State::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read state file");
                State::default()
            }
        }
    }

    /// Write `path` atomically (temp file + rename), creating its directory.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path.parent().context("state path has no directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let text = toml::to_string(self).context("serialise state")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_fallbacks() {
        let dir = std::env::temp_dir().join(format!("gprox-state-test-{}", std::process::id()));
        let path = dir.join("sub").join("state.toml");
        assert_eq!(State::load(&path), State::default());
        let s = State {
            hp_eq: Some("bass".into()),
            bluevoice: Some("BROADCASTER_1".into()),
            bluevoice_enabled: true,
            ghub_presets: Some("/x/presets.json".into()),
            battery_model: Some("/x/battery.xml".into()),
            sidetone: Some(30),
            always_on_sidetone: true,
        };
        s.save(&path).unwrap();
        assert_eq!(State::load(&path), s);
        // Files written before the setting existed: off.
        std::fs::write(&path, "sidetone = 30\n").unwrap();
        let old = State::load(&path);
        assert_eq!(old.sidetone, Some(30));
        assert!(!old.always_on_sidetone);
        std::fs::write(&path, "hp_eq = [").unwrap();
        assert_eq!(State::load(&path), State::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
