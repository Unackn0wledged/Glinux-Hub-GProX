//! HID++ 2.0 framing and the command allow-list.
//!
//! The only way to get bytes for the device is [`Command::encode`], which
//! returns a [`Report`]. `Report` has no public constructor, so code outside
//! this module cannot build an arbitrary frame, and [`Command`] only has the
//! variants G HUB was seen sending. Feature ids are a closed
//! enum ([`Feature`]), so a DFU/firmware feature can't even be
//! looked up. Parameters are range-checked newtypes ([`SidetoneLevel`],
//! [`AutoOffMinutes`]).
//!
//! Frame: `11 ff <feature index> <function << 4 | sw_id> <16 parameter bytes>`
//! (report 0x11 "long", 20 bytes, device index 0xFF). Replies echo the first
//! four bytes. Errors: `11 ff ff <index> <function|sw_id> <code>`. Device
//! notifications have sw_id 0.

use std::fmt;

/// Report id of a HID++ long report.
pub const REPORT_LONG: u8 = 0x11;
/// Device index G HUB addresses (the dongle's wireless device).
pub const DEVICE_INDEX: u8 = 0xFF;
/// Bytes in a long report, report id included.
pub const REPORT_LEN: usize = 20;
/// Software id in every request. G HUB uses 0xB; using the same value keeps
/// gprox's frames byte-identical to the captured ones.
pub const SW_ID: u8 = 0x0B;

/// Feature ids that must never be used: DFU and firmware update.
/// [`Feature`] has no variant for them; this list exists for the tests
/// and for rejecting a device that reports one of them where gprox expected an
/// allowed feature.
pub const BLOCKED_FEATURE_IDS: [u16; 6] = [0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00D0, 0x00D1];

/// HID++ features gprox talks to (besides Root, index 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    /// 0x8300 Sidetone.
    Sidetone,
    /// 0x1F20 ADC measurement (battery) + power-off timer.
    AdcMeasurement,
}

impl Feature {
    pub const ALL: [Feature; 2] = [Feature::Sidetone, Feature::AdcMeasurement];

    pub const fn id(self) -> u16 {
        match self {
            Feature::Sidetone => 0x8300,
            Feature::AdcMeasurement => 0x1F20,
        }
    }
}

/// Sidetone level, 0–100 (G HUB's slider).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidetoneLevel(u8);

impl SidetoneLevel {
    pub const MAX: u8 = 100;

    pub fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for SidetoneLevel {
    type Error = HidppError;

    fn try_from(v: u8) -> Result<Self, HidppError> {
        if v <= Self::MAX {
            Ok(SidetoneLevel(v))
        } else {
            Err(HidppError::Range(format!(
                "sidetone level {v} (allowed 0–{})",
                Self::MAX
            )))
        }
    }
}

/// Auto-off timer in minutes; 0 = never.
///
/// Only the values in G HUB's sleep-timer menu for this headset are allowed
/// (the UI's option list; 0 is its "never"). G HUB was seen sending 10, 15 and
/// 30.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoOffMinutes(u8);

impl AutoOffMinutes {
    pub const OPTIONS: [u8; 7] = [0, 1, 2, 5, 10, 15, 30];

    pub fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for AutoOffMinutes {
    type Error = HidppError;

    fn try_from(v: u8) -> Result<Self, HidppError> {
        if Self::OPTIONS.contains(&v) {
            Ok(AutoOffMinutes(v))
        } else {
            Err(HidppError::Range(format!(
                "auto-off {v} min (allowed: {:?}, 0 = never)",
                Self::OPTIONS
            )))
        }
    }
}

/// Every request gprox can send. Nothing else can be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Root fn1 getProtocolVersion; the ping byte is echoed.
    GetProtocolVersion { ping: u8 },
    /// Root fn0 getFeature(id) → index.
    GetFeature(Feature),
    /// Sidetone fn0 → level.
    GetSidetone,
    /// Sidetone fn1 (write).
    SetSidetone(SidetoneLevel),
    /// ADC fn0 → millivolts + flags.
    ReadBattery,
    /// ADC fn1 → minutes.
    GetAutoOff,
    /// ADC fn2 (write).
    SetAutoOff(AutoOffMinutes),
}

/// Where a command goes: Root (index 0) or a looked-up feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Root,
    Feature(Feature),
}

impl Command {
    /// Target, function number and the non-zero leading parameter bytes.
    fn parts(&self) -> (Target, u8, [u8; 3], usize) {
        match *self {
            Command::GetProtocolVersion { ping } => (Target::Root, 1, [0, 0, ping], 3),
            Command::GetFeature(f) => {
                let [hi, lo] = f.id().to_be_bytes();
                (Target::Root, 0, [hi, lo, 0], 2)
            }
            Command::GetSidetone => (Target::Feature(Feature::Sidetone), 0, [0; 3], 0),
            Command::SetSidetone(l) => (Target::Feature(Feature::Sidetone), 1, [l.0, 0, 0], 1),
            Command::ReadBattery => (Target::Feature(Feature::AdcMeasurement), 0, [0; 3], 0),
            Command::GetAutoOff => (Target::Feature(Feature::AdcMeasurement), 1, [0; 3], 0),
            Command::SetAutoOff(m) => (Target::Feature(Feature::AdcMeasurement), 2, [m.0, 0, 0], 1),
        }
    }

    pub fn target(&self) -> Target {
        self.parts().0
    }

    pub fn function(&self) -> u8 {
        self.parts().1
    }

    /// Whether the command changes device state.
    pub fn is_write(&self) -> bool {
        matches!(self, Command::SetSidetone(_) | Command::SetAutoOff(_))
    }

    /// Feature index this command is sent to, from `map`.
    pub fn index(&self, map: &FeatureMap) -> Result<u8, HidppError> {
        match self.target() {
            Target::Root => Ok(0),
            Target::Feature(f) => map.index(f).ok_or(HidppError::NoFeature(f)),
        }
    }

    /// The 20-byte frame for this command.
    pub fn encode(&self, map: &FeatureMap) -> Result<Report, HidppError> {
        let (_, function, lead, n) = self.parts();
        let mut b = [0u8; REPORT_LEN];
        b[0] = REPORT_LONG;
        b[1] = DEVICE_INDEX;
        b[2] = self.index(map)?;
        b[3] = (function << 4) | SW_ID;
        b[4..4 + n].copy_from_slice(&lead[..n]);
        Ok(Report(b))
    }

    /// Whether `msg` is this command's reply (or error) given `map`.
    pub fn answers(&self, map: &FeatureMap, msg: &Message) -> bool {
        let Ok(index) = self.index(map) else {
            return false;
        };
        let fs = (self.function() << 4) | SW_ID;
        match msg {
            Message::Reply {
                index: i, fn_sw, ..
            }
            | Message::Error {
                index: i, fn_sw, ..
            } => *i == index && *fn_sw == fs,
            _ => false,
        }
    }
}

/// An encoded request. Only [`Command::encode`] makes one.
#[derive(Clone, PartialEq, Eq)]
pub struct Report([u8; REPORT_LEN]);

impl Report {
    pub fn as_bytes(&self) -> &[u8; REPORT_LEN] {
        &self.0
    }
}

impl fmt::Debug for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Report({})", hex(&self.0))
    }
}

/// Space-separated hex, for logs.
pub fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Feature indices of the connected headset, from Root.getFeature. They are
/// assigned by the firmware and are never hard-coded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureMap {
    sidetone: Option<u8>,
    adc: Option<u8>,
}

impl FeatureMap {
    pub fn index(&self, f: Feature) -> Option<u8> {
        match f {
            Feature::Sidetone => self.sidetone,
            Feature::AdcMeasurement => self.adc,
        }
    }

    /// Record a getFeature result. Index 0 is Root and 0xFF marks errors, so
    /// neither can be a feature; nor can an index already used by the other
    /// feature.
    pub fn set(&mut self, f: Feature, index: u8) -> Result<(), HidppError> {
        if index == 0
            || index == 0xFF
            || Feature::ALL
                .iter()
                .any(|&o| o != f && self.index(o) == Some(index))
        {
            return Err(HidppError::Protocol(format!(
                "implausible index {index:#04x} for feature {:#06x}",
                f.id()
            )));
        }
        match f {
            Feature::Sidetone => self.sidetone = Some(index),
            Feature::AdcMeasurement => self.adc = Some(index),
        }
        Ok(())
    }

    /// Which feature lives at `index`, if known.
    pub fn feature_at(&self, index: u8) -> Option<Feature> {
        Feature::ALL
            .into_iter()
            .find(|&f| self.index(f) == Some(index))
    }
}

/// A parsed input report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A reply to a request (sw_id ≠ 0).
    Reply {
        index: u8,
        fn_sw: u8,
        params: [u8; 16],
    },
    /// HID++ 2.0 error reply.
    Error { index: u8, fn_sw: u8, code: u8 },
    /// Unsolicited notification (sw_id 0).
    Event {
        index: u8,
        function: u8,
        params: [u8; 16],
    },
    /// Anything else (consumer-control report 0x01, the `08 20` report, …).
    Other(Vec<u8>),
}

impl Message {
    pub fn parse(b: &[u8]) -> Message {
        if b.len() < REPORT_LEN || b[0] != REPORT_LONG || b[1] != DEVICE_INDEX {
            return Message::Other(b.to_vec());
        }
        if b[2] == 0xFF {
            return Message::Error {
                index: b[3],
                fn_sw: b[4],
                code: b[5],
            };
        }
        let mut params = [0u8; 16];
        params.copy_from_slice(&b[4..REPORT_LEN]);
        if b[3] & 0x0F == 0 {
            Message::Event {
                index: b[2],
                function: b[3] >> 4,
                params,
            }
        } else {
            Message::Reply {
                index: b[2],
                fn_sw: b[3],
                params,
            }
        }
    }
}

/// ADC fn0 reply / event: battery voltage and a flags byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    pub millivolts: u16,
    pub flags: u8,
}

/// Flags byte while running on battery.
pub const BATTERY_FLAG_DISCHARGING: u8 = 0x01;
/// Flags byte while charging.
pub const BATTERY_FLAG_CHARGING: u8 = 0x03;

impl BatteryReading {
    pub fn parse(params: &[u8; 16]) -> BatteryReading {
        BatteryReading {
            millivolts: u16::from_be_bytes([params[0], params[1]]),
            flags: params[2],
        }
    }

    /// 0 mV is what the dongle reports when the headset switches off
    /// (event `11 ff 06 00 00 00 00`).
    pub fn headset_off(&self) -> bool {
        self.millivolts == 0
    }

    pub fn charging(&self) -> bool {
        self.flags == BATTERY_FLAG_CHARGING
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HidppError {
    #[error("value out of range: {0}")]
    Range(String),
    #[error("feature {:#06x} not found on the headset", .0.id())]
    NoFeature(Feature),
    #[error("HID++ error {0:#04x} from the device")]
    Device(u8),
    #[error("HID++ protocol: {0}")]
    Protocol(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> FeatureMap {
        let mut m = FeatureMap::default();
        m.set(Feature::Sidetone, 5).unwrap();
        m.set(Feature::AdcMeasurement, 6).unwrap();
        m
    }

    fn enc(c: Command) -> [u8; REPORT_LEN] {
        *c.encode(&map()).unwrap().as_bytes()
    }

    fn frame(head: &[u8]) -> [u8; REPORT_LEN] {
        let mut b = [0u8; REPORT_LEN];
        b[..head.len()].copy_from_slice(head);
        b
    }

    /// Byte-for-byte the frames G HUB sent (USB captures of G HUB reading
    /// and setting sidetone and auto-off), with this headset's indices 5 and 6.
    #[test]
    fn frames_match_captures() {
        assert_eq!(
            enc(Command::GetProtocolVersion { ping: 0x5a }),
            frame(&[0x11, 0xff, 0x00, 0x1b, 0, 0, 0x5a])
        );
        assert_eq!(
            enc(Command::GetFeature(Feature::AdcMeasurement)),
            frame(&[0x11, 0xff, 0x00, 0x0b, 0x1f, 0x20])
        );
        assert_eq!(
            enc(Command::GetFeature(Feature::Sidetone)),
            frame(&[0x11, 0xff, 0x00, 0x0b, 0x83, 0x00])
        );
        assert_eq!(enc(Command::GetSidetone), frame(&[0x11, 0xff, 0x05, 0x0b]));
        let l = SidetoneLevel::try_from(50).unwrap();
        assert_eq!(
            enc(Command::SetSidetone(l)),
            frame(&[0x11, 0xff, 0x05, 0x1b, 0x32])
        );
        assert_eq!(enc(Command::ReadBattery), frame(&[0x11, 0xff, 0x06, 0x0b]));
        assert_eq!(enc(Command::GetAutoOff), frame(&[0x11, 0xff, 0x06, 0x1b]));
        let m = AutoOffMinutes::try_from(15).unwrap();
        assert_eq!(
            enc(Command::SetAutoOff(m)),
            frame(&[0x11, 0xff, 0x06, 0x2b, 0x0f])
        );
    }

    #[test]
    fn parameter_ranges() {
        assert!(SidetoneLevel::try_from(100).is_ok());
        assert!(SidetoneLevel::try_from(101).is_err());
        for v in 0..=255u8 {
            assert_eq!(
                AutoOffMinutes::try_from(v).is_ok(),
                [0, 1, 2, 5, 10, 15, 30].contains(&v),
                "{v}"
            );
        }
    }

    #[test]
    fn feature_map_rejects_implausible_indices() {
        let mut m = FeatureMap::default();
        assert!(m.set(Feature::Sidetone, 0).is_err());
        assert!(m.set(Feature::Sidetone, 0xff).is_err());
        m.set(Feature::Sidetone, 5).unwrap();
        assert!(m.set(Feature::AdcMeasurement, 5).is_err());
        // Unknown index: feature-targeted commands can't be encoded.
        let empty = FeatureMap::default();
        assert_eq!(
            Command::ReadBattery.encode(&empty),
            Err(HidppError::NoFeature(Feature::AdcMeasurement))
        );
        assert!(
            Command::GetFeature(Feature::Sidetone)
                .encode(&empty)
                .is_ok()
        );
    }

    #[test]
    fn parse_messages() {
        // From USB captures: battery reply, 3798 mV, flags 1.
        let r = Message::parse(&frame(&[0x11, 0xff, 0x06, 0x0b, 0x0e, 0xd6, 0x01]));
        let Message::Reply {
            index,
            fn_sw,
            params,
        } = r
        else {
            panic!("{r:?}")
        };
        assert_eq!((index, fn_sw), (6, 0x0b));
        let b = BatteryReading::parse(&params);
        assert_eq!((b.millivolts, b.flags, b.charging()), (3798, 1, false));
        assert!(Command::ReadBattery.answers(&map(), &r_clone(&params)));
        // From USB captures: power-off and power-on events.
        let off = Message::parse(&frame(&[0x11, 0xff, 0x06, 0x00]));
        assert!(
            matches!(off, Message::Event { index: 6, function: 0, params } if BatteryReading::parse(&params).headset_off())
        );
        let on = Message::parse(&frame(&[0x11, 0xff, 0x06, 0x00, 0x0e, 0xdc, 0x01]));
        assert!(
            matches!(on, Message::Event { index: 6, function: 0, params } if BatteryReading::parse(&params).millivolts == 3804)
        );
        // From USB captures: error after DeviceInfo fn2.
        let e = Message::parse(&frame(&[0x11, 0xff, 0xff, 0x02, 0x2b, 0x07]));
        assert_eq!(
            e,
            Message::Error {
                index: 2,
                fn_sw: 0x2b,
                code: 7
            }
        );
        // Consumer-control and short reports are not HID++.
        assert!(matches!(
            Message::parse(&[1, 0, 0, 0, 0]),
            Message::Other(_)
        ));
        assert!(matches!(Message::parse(&[8, 0x20]), Message::Other(_)));
    }

    fn r_clone(params: &[u8; 16]) -> Message {
        Message::Reply {
            index: 6,
            fn_sw: 0x0b,
            params: *params,
        }
    }
}
