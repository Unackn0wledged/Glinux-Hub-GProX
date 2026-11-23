//! Control channel between `gproxd` and its clients (`gprox` CLI, GUI).
//!
//! Transport: a Unix stream socket at [`socket_path`] (`$XDG_RUNTIME_DIR/
//! gprox/control.sock`, directory mode 0700, so only the owning user can
//! connect). No network. Framing: one JSON object per line in each
//! direction; every [`Request`] gets exactly one [`Response`] line. A client
//! may send several requests on one connection.
//!
//! ```text
//! → {"cmd":"status"}
//! ← {"ok":{"hp_eq":{…},"bluevoice":{…}}}
//! → {"cmd":"set_hp_eq","name":"bass boost"}
//! ← {"ok":{…}}                      (new status after the change)
//! → {"cmd":"set_bluevoice","name":"nope"}
//! ← {"error":"no Blue VO!CE preset \"nope\"; …"}
//! → {"cmd":"set_sidetone","level":30}
//! ← {"ok":{…,"headset":{"connected":true,"sidetone":30,…}}}
//! → {"cmd":"set_always_on_sidetone","on":true}
//! ← {"ok":{…,"headset":{…,"always_on_sidetone":true,"mic_kept_open":true}}}
//! ```

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::HeadphoneEqPreset;

/// Name of the always-available flat headphone EQ preset.
pub const FLAT_PRESET: &str = "flat";

/// Longest request or response line accepted, in bytes.
pub const MAX_LINE: u64 = 256 * 1024;

/// A client request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Current selection and the available presets.
    Status,
    /// Select a headphone EQ preset by name.
    SetHpEq { name: String },
    /// Select a Blue VO!CE preset by name (G HUB name, with or without the
    /// built-in prefix, case-insensitive). Does not change on/off.
    SetBluevoice { name: String },
    /// Switch Blue VO!CE on or off.
    BluevoiceEnable { on: bool },
    /// Headset sidetone level, 0–100 (HID++ write).
    SetSidetone { level: u8 },
    /// Headset auto-off timer in minutes: 0 (never), 1, 2, 5, 10, 15 or 30
    /// (HID++ write).
    SetAutoOff { minutes: u8 },
    /// Always-on sidetone: keep the headset's mic stream open even when no
    /// app records, because the headset only mixes sidetone while it
    /// streams. No HID++ write; persisted.
    SetAlwaysOnSidetone { on: bool },
}

/// The daemon's answer: the status after the request, or an error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok(Status),
    Error(String),
}

/// Daemon state as seen by clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub hp_eq: HpEqStatus,
    pub bluevoice: BlueVoiceStatus,
    /// Headset controls (HID++). Absent from older daemons.
    #[serde(default)]
    pub headset: HeadsetStatus,
}

/// Headset state from the dongle (HID++) and the mic stream.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HeadsetStatus {
    /// The dongle's HID interface is open.
    pub dongle: bool,
    /// The headset answers (switched on and in range).
    pub connected: bool,
    pub battery: Option<BatteryStatus>,
    /// Sidetone level 0–100, as read from the headset.
    pub sidetone: Option<u8>,
    /// Auto-off timer in minutes, 0 = never, as read from the headset.
    pub auto_off_minutes: Option<u8>,
    /// Mic muted on the headset (best effort, see `gprox_hid::mute`): mute
    /// button presses seen on HID toggle it, live audio on the mic means
    /// unmuted. `None` = not known yet (no mic audio seen since gproxd
    /// started) or headset off.
    pub mic_muted: Option<bool>,
    /// Why the dongle can't be used (e.g. permission denied), if so.
    pub error: Option<String>,
    /// The always-on sidetone setting ([`Request::SetAlwaysOnSidetone`]).
    pub always_on_sidetone: bool,
    /// gproxd's keep-alive stream on the headset mic is streaming right now
    /// (only while `always_on_sidetone` is on and the headset mic exists).
    pub mic_kept_open: bool,
}

impl HeadsetStatus {
    /// One-line headset summary, e.g. `on, battery 64 % (3804 mV, ~15 h),
    /// sidetone 0, auto-off 30 min, mic live`.
    pub fn summary(&self) -> String {
        let h = self;
        if !h.dongle {
            return match &h.error {
                Some(e) => e.clone(),
                None => "dongle not found".into(),
            };
        }
        if !h.connected {
            return "off or out of range".into();
        }
        let mut parts = vec!["on".to_string()];
        if let Some(b) = &h.battery {
            let mut s = match b.percent {
                Some(p) => format!("battery {p} % ({} mV", b.millivolts),
                None => format!("battery {} mV", b.millivolts),
            };
            if b.charging {
                s.push_str(", charging");
            } else if let Some(hl) = b.hours_left {
                s.push_str(&format!(", ~{hl:.0} h left"));
            }
            if b.percent.is_some() {
                s.push(')');
            }
            parts.push(s);
        }
        let always = match (h.always_on_sidetone, h.mic_kept_open) {
            (false, _) => "",
            (true, true) => " (always on)",
            (true, false) => " (always on, mic stream not open yet)",
        };
        match h.sidetone {
            Some(l) => parts.push(format!("sidetone {l}{always}")),
            None if h.always_on_sidetone => parts.push(format!("sidetone{always}")),
            None => {}
        }
        match h.auto_off_minutes {
            Some(0) => parts.push("auto-off never".into()),
            Some(m) => parts.push(format!("auto-off {m} min")),
            None => {}
        }
        match h.mic_muted {
            Some(true) => parts.push("mic muted".into()),
            Some(false) => parts.push("mic live".into()),
            None => {}
        }
        parts.join(", ")
    }
}

/// Battery reading.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BatteryStatus {
    pub millivolts: u16,
    /// Raw flags byte (0x01 on battery, 0x03 charging).
    pub flags: u8,
    pub charging: bool,
    /// Charge from G HUB's power model; `None` without the model file.
    pub percent: Option<u8>,
    /// Hours of use left (on battery, with the model).
    pub hours_left: Option<f64>,
}

/// Headphone EQ (render path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HpEqStatus {
    /// Selected preset name.
    pub selected: String,
    /// The selected preset's settings (what the DSP is ramping to).
    pub preset: HeadphoneEqPreset,
    /// Available preset names, [`FLAT_PRESET`] first.
    pub presets: Vec<String>,
}

/// Blue VO!CE (capture path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlueVoiceStatus {
    /// Whether a G HUB preset file was loaded. Without one the mic path is a
    /// pass-through and the other fields are empty.
    pub available: bool,
    pub enabled: bool,
    /// Selected preset (short name, e.g. `BROADCASTER_1`, `TSM Myth`).
    pub selected: Option<String>,
    /// Available presets, short names (built-in prefix removed), file order.
    pub presets: Vec<String>,
}

/// Control-channel errors on the client side.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("cannot reach gproxd at {path} (is gproxd running?)")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("control socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("control protocol: {0}")]
    Json(#[from] serde_json::Error),
    #[error("gproxd closed the connection without answering")]
    Closed,
    #[error("{0}")]
    Daemon(String),
    #[error("cannot find a runtime directory: set XDG_RUNTIME_DIR")]
    NoRuntimeDir,
}

/// `$XDG_RUNTIME_DIR/gprox`, falling back to `/run/user/<uid>/gprox`.
pub fn runtime_dir() -> Result<PathBuf, ControlError> {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d).join("gprox"));
    }
    // The owner of /proc/self is this process's uid (std only, no libc).
    let uid = std::fs::metadata("/proc/self")
        .map_err(|_| ControlError::NoRuntimeDir)?
        .uid();
    let d = PathBuf::from(format!("/run/user/{uid}"));
    if d.is_dir() {
        Ok(d.join("gprox"))
    } else {
        Err(ControlError::NoRuntimeDir)
    }
}

/// The control socket path (`$XDG_RUNTIME_DIR/gprox/control.sock`).
pub fn socket_path() -> Result<PathBuf, ControlError> {
    Ok(runtime_dir()?.join("control.sock"))
}

/// A connection to `gproxd`.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    /// Connect to the daemon's socket at [`socket_path`].
    pub fn connect() -> Result<Self, ControlError> {
        Self::connect_to(&socket_path()?)
    }

    /// Connect to a socket at `path`. Reads and writes time out after 2 s.
    pub fn connect_to(path: &std::path::Path) -> Result<Self, ControlError> {
        let s = UnixStream::connect(path).map_err(|source| ControlError::Connect {
            path: path.display().to_string(),
            source,
        })?;
        let timeout = Some(Duration::from_secs(2));
        s.set_read_timeout(timeout)?;
        s.set_write_timeout(timeout)?;
        Ok(Client {
            reader: BufReader::new(s.try_clone()?),
            writer: s,
        })
    }

    /// Send one request and wait for its response.
    pub fn call(&mut self, req: &Request) -> Result<Status, ControlError> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut answer = String::new();
        let n = (&mut self.reader).take(MAX_LINE).read_line(&mut answer)?;
        if n == 0 {
            return Err(ControlError::Closed);
        }
        match serde_json::from_str(&answer)? {
            Response::Ok(s) => Ok(s),
            Response::Error(e) => Err(ControlError::Daemon(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r = Request::SetHpEq {
            name: "bass".into(),
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"cmd":"set_hp_eq","name":"bass"}"#
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"bluevoice_enable","on":false}"#).unwrap(),
            Request::BluevoiceEnable { on: false }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"status"}"#).unwrap(),
            Request::Status
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"set_auto_off","minutes":15}"#).unwrap(),
            Request::SetAutoOff { minutes: 15 }
        );
        assert_eq!(
            serde_json::to_string(&Request::SetAlwaysOnSidetone { on: true }).unwrap(),
            r#"{"cmd":"set_always_on_sidetone","on":true}"#
        );
        // A status from a daemon without headset support still parses.
        let old = r#"{"ok":{"hp_eq":{"selected":"flat","preset":{},"presets":["flat"]},
            "bluevoice":{"available":false,"enabled":false,"selected":null,"presets":[]}}}"#;
        let Response::Ok(s) = serde_json::from_str::<Response>(old).unwrap() else {
            panic!()
        };
        assert_eq!(s.headset, HeadsetStatus::default());
        let e = Response::Error("x".into());
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"error":"x"}"#);
    }

    #[test]
    fn status_round_trip() {
        let s = Status {
            hp_eq: HpEqStatus {
                selected: FLAT_PRESET.into(),
                preset: HeadphoneEqPreset::default(),
                presets: vec![FLAT_PRESET.into(), "bass".into()],
            },
            bluevoice: BlueVoiceStatus {
                available: true,
                enabled: false,
                selected: Some("BROADCASTER_1".into()),
                presets: vec!["BROADCASTER_1".into()],
            },
            headset: HeadsetStatus {
                dongle: true,
                connected: true,
                battery: Some(BatteryStatus {
                    millivolts: 3804,
                    flags: 1,
                    charging: false,
                    percent: Some(64),
                    hours_left: Some(15.3),
                }),
                sidetone: Some(0),
                auto_off_minutes: Some(30),
                mic_muted: None,
                error: None,
                always_on_sidetone: true,
                mic_kept_open: false,
            },
        };
        let line = serde_json::to_string(&Response::Ok(s.clone())).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(
            serde_json::from_str::<Response>(&line).unwrap(),
            Response::Ok(s)
        );
    }

    #[test]
    fn headset_summary() {
        let mut h = HeadsetStatus::default();
        assert_eq!(h.summary(), "dongle not found");
        h.dongle = true;
        assert_eq!(h.summary(), "off or out of range");
        h.connected = true;
        h.battery = Some(BatteryStatus {
            millivolts: 3804,
            flags: 1,
            charging: false,
            percent: Some(64),
            hours_left: Some(15.3),
        });
        h.sidetone = Some(0);
        h.auto_off_minutes = Some(30);
        h.mic_muted = Some(true);
        assert_eq!(
            h.summary(),
            "on, battery 64 % (3804 mV, ~15 h left), sidetone 0, auto-off 30 min, mic muted"
        );
        h.battery = Some(BatteryStatus {
            millivolts: 4100,
            flags: 3,
            charging: true,
            percent: None,
            hours_left: None,
        });
        h.mic_muted = None;
        assert_eq!(
            h.summary(),
            "on, battery 4100 mV, charging, sidetone 0, auto-off 30 min"
        );
        h.battery = None;
        h.auto_off_minutes = None;
        h.always_on_sidetone = true;
        assert_eq!(
            h.summary(),
            "on, sidetone 0 (always on, mic stream not open yet)"
        );
        h.mic_kept_open = true;
        assert_eq!(h.summary(), "on, sidetone 0 (always on)");
        h.sidetone = None;
        assert_eq!(h.summary(), "on, sidetone (always on)");
    }
}
