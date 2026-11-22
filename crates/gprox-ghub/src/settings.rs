//! G HUB's `settings.db`: the user's presets and the active selection.
//!
//! Format: SQLite in WAL mode with one row in
//! table `data` whose `file` column is the whole settings document as JSON.
//! Presets are `cards.cards[]` (protobuf messages in proto3 JSON form, so
//! absent fields are 0 / false). The headphone EQ presets are the
//! `EQUALIZER` cards, the Blue VO!CE presets the `BLUE_VOICE_SETTINGS` cards;
//! each profile's `assignments[] {slotId, cardId}` says which card a device
//! uses (`proxwirelessheadset_equalizer`, `…_blue_voice_settings`,
//! `…_microphone_settings`).
//!
//! While G HUB runs, recent changes live only in `settings.db-wal`, so
//! [`read_settings_db`] copies the database together with its WAL to a
//! private temporary directory and reads the copy. The user's files are only
//! read. The `-shm` index is not copied (a copied one can be stale); SQLite
//! rebuilds it from the WAL.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gprox_core::{EqMode, HeadphoneEqPreset};
use rusqlite::types::ValueRef;
use serde_json::Value;

use crate::{BlueVoicePreset, Error, bluevoice_preset, lookup};

/// G HUB's slot prefix for the PRO X Wireless headset.
pub const PRO_X_WIRELESS: &str = "proxwirelessheadset";

/// G HUB preset name prefix for the built-in headphone EQ presets.
const EQ_PREFIX: &str = "EQUALIZER_";

/// The settings document read from a `settings.db`.
#[derive(Debug, Clone)]
pub struct SettingsDocument {
    /// The JSON text.
    pub json: String,
    /// When G HUB saved it (`_date_created`, UTC, as SQLite text).
    pub saved: Option<String>,
}

/// Read the settings document from a G HUB `settings.db` (or the directory
/// holding it), including changes still in `settings.db-wal`.
pub fn read_settings_db(path: &Path) -> Result<SettingsDocument, Error> {
    let db = if path.is_dir() {
        path.join("settings.db")
    } else {
        path.to_owned()
    };
    let tmp = tempfile::tempdir()?;
    let copy = tmp.path().join("settings.db");
    std::fs::copy(&db, &copy)?;
    let wal = sidecar(&db, "-wal");
    if wal.is_file() {
        std::fs::copy(&wal, sidecar(&copy, "-wal"))?;
    }
    // Read-write on the private copy so SQLite can replay the WAL.
    let conn = rusqlite::Connection::open_with_flags(
        &copy,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let row = conn.query_row(
        "SELECT file, _date_created FROM data ORDER BY _id DESC LIMIT 1",
        [],
        |r| {
            let bytes = match r.get_ref(0)? {
                ValueRef::Blob(b) | ValueRef::Text(b) => Some(b.to_vec()),
                _ => None,
            };
            Ok((bytes, r.get::<_, Option<String>>(1).ok().flatten()))
        },
    );
    let (bytes, saved) = match row {
        Ok(r) => r,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Err(Error::NoDocument),
        Err(e) => return Err(e.into()),
    };
    let json = String::from_utf8(bytes.ok_or(Error::NotText)?).map_err(|_| Error::NotText)?;
    Ok(SettingsDocument { json, saved })
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// A headphone EQ preset (`EQUALIZER` card).
#[derive(Debug, Clone, PartialEq)]
pub struct EqPreset {
    /// G HUB's card name (`EQUALIZER_FLAT`, `PREMIUM  SOUND/BASS`, …).
    pub name: String,
    /// Card id (UUID string).
    pub id: String,
    /// Read-only in G HUB (built-in).
    pub read_only: bool,
    /// Set when the card is G HUB's edited copy of a read-only preset: the
    /// original's id. Such copies keep the original's name.
    pub source_id: Option<String>,
    pub preset: HeadphoneEqPreset,
}

impl EqPreset {
    /// Display name: the built-in prefix removed (`FLAT`), and ` (edited)`
    /// for G HUB's edited copy of a read-only preset.
    pub fn display_name(&self) -> String {
        let base = self.name.strip_prefix(EQ_PREFIX).unwrap_or(&self.name);
        match self.source_id {
            Some(_) => format!("{base} (edited)"),
            None => base.to_owned(),
        }
    }
}

/// Which presets a profile uses for one device.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Selection {
    /// Profile name (`PROFILE_NAME_DEFAULT` = G HUB's default profile).
    pub profile: String,
    pub profile_id: String,
    /// Device slot prefix, e.g. [`PRO_X_WIRELESS`].
    pub device: String,
    /// Card id of the headphone EQ preset.
    pub eq: Option<String>,
    /// Card id of the Blue VO!CE preset.
    pub bluevoice: Option<String>,
    /// Blue VO!CE on/off (from the device's microphone settings card).
    pub bluevoice_enabled: Option<bool>,
}

/// A card that could not be imported.
#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    /// `"EQ"` or `"Blue VO!CE"`.
    pub kind: &'static str,
    pub name: String,
    pub id: String,
    pub reason: String,
}

/// Everything imported from one settings document.
#[derive(Debug, Clone, Default)]
pub struct GhubSettings {
    /// Headphone EQ presets, in card order.
    pub eq: Vec<EqPreset>,
    /// Blue VO!CE presets, in card order.
    pub bluevoice: Vec<BlueVoicePreset>,
    /// Per profile and device: the assigned presets.
    pub selections: Vec<Selection>,
    /// Cards of those two kinds that could not be imported.
    pub skipped: Vec<Skipped>,
}

impl GhubSettings {
    pub fn eq_by_id(&self, id: &str) -> Option<&EqPreset> {
        self.eq.iter().find(|p| p.id == id)
    }

    pub fn bluevoice_by_id(&self, id: &str) -> Option<&BlueVoicePreset> {
        self.bluevoice.iter().find(|p| p.id == id)
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Parse a settings document (the JSON from [`read_settings_db`]).
pub fn parse(json: &str) -> Result<GhubSettings, Error> {
    let root: Value = serde_json::from_str(json)?;
    let cards = lookup(&root, "cards.cards")
        .and_then(Value::as_array)
        .ok_or(Error::NoCards)?;
    let mut out = GhubSettings::default();
    for c in cards {
        let skip = |kind, reason: String| Skipped {
            kind,
            name: str_field(c, "name").to_owned(),
            id: str_field(c, "id").to_owned(),
            reason,
        };
        match str_field(c, "attribute") {
            "EQUALIZER" => match eq_preset(c) {
                Ok(Some(p)) => out.eq.push(p),
                Ok(None) => {}
                Err(reason) => out.skipped.push(skip("EQ", reason)),
            },
            "BLUE_VOICE_SETTINGS" => {
                let settings = lookup(c, "blueVoiceSettings.settings").unwrap_or(&Value::Null);
                // An absent `settings` would be an all-zero message; G HUB
                // never writes one, so treat it as damaged.
                if !settings.is_object() {
                    out.skipped
                        .push(skip("Blue VO!CE", "no settings object".into()));
                    continue;
                }
                match bluevoice_preset(c, settings) {
                    Ok(p) => out.bluevoice.push(p),
                    Err(e) => out.skipped.push(skip("Blue VO!CE", e.to_string())),
                }
            }
            _ => {}
        }
    }
    out.selections = selections(&root, cards);
    Ok(out)
}

/// `Ok(None)`: an EQ card for another kind of device (e.g. speakers).
fn eq_preset(c: &Value) -> Result<Option<EqPreset>, String> {
    let eq = c.get("equalizer").unwrap_or(&Value::Null);
    match eq.get("deviceType").and_then(Value::as_str) {
        None | Some("HEADSET") => {}
        Some(_) => return Ok(None),
    }
    let mode = match eq.get("type").and_then(Value::as_str) {
        Some("SIMPLE") => EqMode::Basic,
        Some("ADVANCED") => EqMode::Advanced,
        other => return Err(format!("EQ type {other:?} is neither SIMPLE nor ADVANCED")),
    };
    let preset = HeadphoneEqPreset {
        mode,
        basic: gains(eq, "simple")?,
        advanced: gains(eq, "advanced")?,
    };
    preset.validate().map_err(|e| e.to_string())?;
    Ok(Some(EqPreset {
        name: str_field(c, "name").to_owned(),
        id: str_field(c, "id").to_owned(),
        read_only: c.get("readOnly").and_then(Value::as_bool).unwrap_or(false),
        source_id: eq
            .get("sourceId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        preset,
    }))
}

/// A gain list (absent = all 0, proto3); JSON numbers to the nearest f32.
fn gains<const N: usize>(eq: &Value, key: &str) -> Result<[f32; N], String> {
    let mut g = [0.0; N];
    let Some(v) = eq.get(key) else { return Ok(g) };
    let list = v
        .as_array()
        .ok_or_else(|| format!("`{key}` is not a list"))?;
    if list.len() != N {
        return Err(format!("`{key}` has {} values, expected {N}", list.len()));
    }
    for (o, x) in g.iter_mut().zip(list) {
        *o = x
            .as_f64()
            .ok_or_else(|| format!("`{key}` holds a non-number"))? as f32;
    }
    Ok(g)
}

fn selections(root: &Value, cards: &[Value]) -> Vec<Selection> {
    let by_id: HashMap<&str, &Value> = cards.iter().map(|c| (str_field(c, "id"), c)).collect();
    let profiles = lookup(root, "profiles.profiles")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut out: Vec<Selection> = Vec::new();
    for prof in profiles {
        let assignments = prof
            .get("assignments")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for a in assignments {
            let slot = str_field(a, "slotId");
            let card = str_field(a, "cardId");
            let (device, feature) = if let Some(d) = slot.strip_suffix("_equalizer") {
                (d, 0)
            } else if let Some(d) = slot.strip_suffix("_blue_voice_settings") {
                (d, 1)
            } else if let Some(d) = slot.strip_suffix("_microphone_settings") {
                (d, 2)
            } else {
                continue;
            };
            let profile_id = str_field(prof, "id");
            let i = match out
                .iter()
                .position(|s| s.profile_id == profile_id && s.device == device)
            {
                Some(i) => i,
                None => {
                    out.push(Selection {
                        profile: str_field(prof, "name").to_owned(),
                        profile_id: profile_id.to_owned(),
                        device: device.to_owned(),
                        ..Default::default()
                    });
                    out.len() - 1
                }
            };
            let s = &mut out[i];
            match feature {
                0 => s.eq = Some(card.to_owned()),
                1 => s.bluevoice = Some(card.to_owned()),
                _ => {
                    s.bluevoice_enabled = by_id.get(card).map(|c| {
                        lookup(c, "microphoneSettings.blueVoiceEnabled")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    });
                }
            }
        }
    }
    // Keep only devices with an EQ or Blue VO!CE assignment.
    out.retain(|s| s.eq.is_some() || s.bluevoice.is_some());
    out
}

/// File stems for presets given as `(display name, id)`: path-unsafe
/// characters become `_`, and names that collide (ignoring case) get the
/// first 8 characters of their id appended, e.g. `BG [de63670c]`.
pub fn file_stems<'a>(items: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    let items: Vec<(String, &str)> = items
        .into_iter()
        .map(|(name, id)| (sanitize(name), id))
        .collect();
    let mut count: HashMap<String, usize> = HashMap::new();
    for (s, _) in &items {
        *count.entry(s.to_lowercase()).or_default() += 1;
    }
    items
        .into_iter()
        .map(|(s, id)| {
            if count[&s.to_lowercase()] > 1 {
                let short: String = id.chars().take(8).collect();
                format!("{s} [{}]", sanitize(&short))
            } else {
                s
            }
        })
        .collect()
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    // No leading dot (hidden file) and no trailing dots.
    let s = s.trim().trim_matches('.').trim();
    if s.is_empty() {
        "unnamed".to_owned()
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A made-up settings document in G HUB's layout (not G HUB data):
    // proto3 JSON, so 0/false values are left out.
    const DOC: &str = r#"{
      "language": "en",
      "cards": { "cards": [
        { "id": "eq-flat", "name": "EQUALIZER_FLAT", "attribute": "EQUALIZER", "readOnly": true,
          "equalizer": { "advanced": [0,0,0,0,0,0,0,0,0,0], "deviceType": "HEADSET",
                         "simple": [0,0], "type": "ADVANCED", "name": "EQUALIZER_FLAT" } },
        { "id": "eq-copy", "name": "EQUALIZER_FLAT", "attribute": "EQUALIZER",
          "equalizer": { "advanced": [12,0,0,0,0,0,0,0,0,6], "deviceType": "HEADSET",
                         "simple": [0,0], "type": "ADVANCED", "sourceId": "eq-flat" } },
        { "id": "eq-basic", "name": "Mine", "attribute": "EQUALIZER",
          "equalizer": { "simple": [6,-6.5], "type": "SIMPLE" } },
        { "id": "eq-bad", "name": "Loud", "attribute": "EQUALIZER",
          "equalizer": { "advanced": [13,0,0,0,0,0,0,0,0,0], "type": "ADVANCED" } },
        { "id": "eq-short", "name": "Short", "attribute": "EQUALIZER",
          "equalizer": { "advanced": [1,2], "type": "ADVANCED" } },
        { "id": "eq-spk", "name": "Speaker", "attribute": "EQUALIZER",
          "equalizer": { "deviceType": "SPEAKER", "type": "ADVANCED" } },
        { "id": "pro-eq", "name": "Other headset", "attribute": "PRO_EQUALIZER",
          "proEqualizer": { "bands": [1,2,3,4,5] } },
        { "id": "bv-1", "name": "My Voice", "attribute": "BLUE_VOICE_SETTINGS",
          "blueVoiceSettings": { "description": { "name": "My Voice" }, "settings": {
            "volume": 0.5,
            "voiceEq": { "enabled": true, "low": { "frequency": 100, "gain": 2, "width": 1.5 } },
            "deEsser": { "attack": 1, "frequency": 6000 },
            "inputGain": 0.6 } } },
        { "id": "bv-2", "name": "Broken", "attribute": "BLUE_VOICE_SETTINGS",
          "blueVoiceSettings": { "description": { "name": "Broken" } } },
        { "id": "mic-ro", "attribute": "MICROPHONE_SETTINGS", "readOnly": true,
          "microphoneSettings": { "microphoneGain": { "gain": 0.5 } } },
        { "id": "mic-p", "attribute": "MICROPHONE_SETTINGS",
          "microphoneSettings": { "blueVoiceEnabled": true } }
      ] },
      "profiles": { "profiles": [
        { "id": "p1", "name": "PROFILE_NAME_DEFAULT", "assignments": [
          { "slotId": "proxwirelessheadset_acoustics", "cardId": "x" },
          { "slotId": "proxwirelessheadset_equalizer", "cardId": "eq-copy" },
          { "slotId": "proxwirelessheadset_blue_voice_settings", "cardId": "bv-1" },
          { "slotId": "proxwirelessheadset_microphone_settings", "cardId": "mic-p" },
          { "slotId": "someheadset_equalizer", "cardId": "eq-flat" },
          { "slotId": "someheadset_microphone_settings", "cardId": "mic-ro" },
          { "slotId": "mouse_g1", "cardId": "y" } ] }
      ] }
    }"#;

    #[test]
    fn eq_cards() {
        let s = parse(DOC).unwrap();
        let ids: Vec<_> = s.eq.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["eq-flat", "eq-copy", "eq-basic"]);
        let flat = s.eq_by_id("eq-flat").unwrap();
        assert!(flat.read_only);
        assert_eq!(flat.display_name(), "FLAT");
        assert_eq!(
            flat.preset,
            HeadphoneEqPreset {
                mode: EqMode::Advanced,
                ..Default::default()
            }
        );
        let copy = s.eq_by_id("eq-copy").unwrap();
        assert_eq!(copy.display_name(), "FLAT (edited)");
        assert_eq!(copy.source_id.as_deref(), Some("eq-flat"));
        assert_eq!(copy.preset.advanced[0], 12.0);
        assert_eq!(copy.preset.advanced[9], 6.0);
        // Absent `advanced` list = all 0 (proto3).
        let basic = s.eq_by_id("eq-basic").unwrap();
        assert_eq!(basic.preset.mode, EqMode::Basic);
        assert_eq!(basic.preset.basic, [6.0, -6.5]);
        assert_eq!(basic.preset.advanced, [0.0; 10]);
    }

    #[test]
    fn bluevoice_absent_settings_are_zero() {
        let s = parse(DOC).unwrap();
        assert_eq!(s.bluevoice.len(), 1);
        let p = s.bluevoice_by_id("bv-1").unwrap();
        assert!(!p.read_only);
        let get = |n: &str| p.params.iter().find(|(k, _)| *k == n).unwrap().1;
        assert_eq!(get("micopt.enable"), 1.0);
        assert_eq!(get("micopt.volume"), 0.5);
        assert_eq!(get("micopt.balance"), 0.0);
        assert_eq!(get("veq.enable"), 1.0);
        assert_eq!(get("veq.low_width"), 1.5);
        assert_eq!(get("veq.mid_gain"), 0.0);
        assert_eq!(get("des.enable"), 0.0);
        assert_eq!(get("des.thresh"), 0.0);
        assert_eq!(get("des.freq"), 6000.0);
        assert_eq!(get("exp.thresh_auto_enable"), 0.0);
        assert!(p.missing.contains(&"deEsser.threshold"));
        assert_eq!(p.input_gain, Some(0.6));
    }

    #[test]
    fn skipped_cards_are_reported() {
        let s = parse(DOC).unwrap();
        let got: Vec<_> = s.skipped.iter().map(|k| (k.kind, k.id.as_str())).collect();
        assert_eq!(
            got,
            [("EQ", "eq-bad"), ("EQ", "eq-short"), ("Blue VO!CE", "bv-2")]
        );
        assert!(s.skipped[0].reason.contains("12"), "{:?}", s.skipped[0]);
    }

    #[test]
    fn selections_per_device() {
        let s = parse(DOC).unwrap();
        assert_eq!(
            s.selections,
            [
                Selection {
                    profile: "PROFILE_NAME_DEFAULT".into(),
                    profile_id: "p1".into(),
                    device: PRO_X_WIRELESS.into(),
                    eq: Some("eq-copy".into()),
                    bluevoice: Some("bv-1".into()),
                    bluevoice_enabled: Some(true),
                },
                Selection {
                    profile: "PROFILE_NAME_DEFAULT".into(),
                    profile_id: "p1".into(),
                    device: "someheadset".into(),
                    eq: Some("eq-flat".into()),
                    bluevoice: None,
                    // The read-only default card leaves `blueVoiceEnabled` out.
                    bluevoice_enabled: Some(false),
                },
            ]
        );
    }

    #[test]
    fn not_a_settings_document() {
        assert!(matches!(parse("{}"), Err(Error::NoCards)));
        assert!(matches!(parse("nope"), Err(Error::Json(_))));
    }

    #[test]
    fn stems() {
        let got = file_stems([
            ("FLAT", "0c94ec70-aaaa"),
            ("PREMIUM  SOUND/BASS", "bbf3c2db-bbbb"),
            ("BG", "de63670c-cccc"),
            ("bg", "78863648-dddd"),
            ("..", "e"),
            ("a\\b:c", "f"),
            ("good presets here..", "g"),
        ]);
        assert_eq!(
            got,
            [
                "FLAT",
                "PREMIUM  SOUND_BASS",
                "BG [de63670c]",
                "bg [78863648]",
                "unnamed",
                "a_b_c",
                "good presets here"
            ]
        );
    }

    #[test]
    fn reads_a_sqlite_file_with_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("settings.db");
        {
            let c = rusqlite::Connection::open(&db).unwrap();
            c.pragma_update(None, "journal_mode", "WAL").unwrap();
            c.execute_batch(
                "CREATE TABLE data(_id INTEGER PRIMARY KEY, \
                 _date_created datetime default current_timestamp, file BLOB NOT NULL);",
            )
            .unwrap();
            c.execute(
                "INSERT INTO data(_id, file) VALUES (1, ?1)",
                [b"{\"old\":1}".to_vec()],
            )
            .unwrap();
            c.pragma_update(None, "wal_checkpoint", "TRUNCATE").unwrap();
            c.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
            c.execute(
                "UPDATE data SET file = ?1 WHERE _id = 1",
                [DOC.as_bytes().to_vec()],
            )
            .unwrap();
            // Keep the change in the WAL only, as a running G HUB does.
            let wal = sidecar(&db, "-wal");
            let saved = dir.path().join("saved");
            std::fs::create_dir(&saved).unwrap();
            std::fs::copy(&db, saved.join("settings.db")).unwrap();
            std::fs::copy(&wal, saved.join("settings.db-wal")).unwrap();
        }
        let saved = dir.path().join("saved");
        let doc = read_settings_db(&saved.join("settings.db")).unwrap();
        assert_eq!(doc.json, DOC);
        assert!(doc.saved.is_some());
        // Same through the directory.
        assert_eq!(read_settings_db(&saved).unwrap().json, DOC);
        // The user's files are untouched (no -shm created next to them).
        assert!(!sidecar(&saved.join("settings.db"), "-shm").exists());
        // Without the WAL only the checkpointed state is visible.
        std::fs::remove_file(saved.join("settings.db-wal")).unwrap();
        assert_eq!(read_settings_db(&saved).unwrap().json, "{\"old\":1}");
    }
}
