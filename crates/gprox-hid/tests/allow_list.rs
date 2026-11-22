//! "No unverified write is possible in code."
//!
//! Builds every frame the crate can produce, for every parameter value its
//! types accept, and checks each one against the allow-list of commands G
//! HUB was seen sending, written out here independently of the crate.
//! The match in `spec` has no wildcard, so adding a `Command` variant breaks
//! this test until it's listed here.
//!
//! That raw frames can't be built at all is a `compile_fail` doctest in
//! `lib.rs`.

use gprox_hid::hidpp::{
    AutoOffMinutes, BLOCKED_FEATURE_IDS, Command, Feature, FeatureMap, SidetoneLevel,
};

/// The allow-list, as (feature id, function, write?, allowed first param).
/// Root (0x0000) fn0 getFeature takes a feature id; fn1 a ping byte.
#[derive(Debug)]
struct Allowed {
    feature: u16,
    function: u8,
    write: bool,
    /// Allowed parameter bytes 0..2 (the rest must be zero).
    params: fn(&[u8]) -> bool,
}

const ALLOW: &[Allowed] = &[
    // Root.getFeature, only for the two features gprox uses.
    Allowed {
        feature: 0x0000,
        function: 0,
        write: false,
        params: |p| matches!(p, [0x83, 0x00, 0] | [0x1f, 0x20, 0]),
    },
    // Root.getProtocolVersion(ping).
    Allowed {
        feature: 0x0000,
        function: 1,
        write: false,
        params: |p| p[0] == 0 && p[1] == 0,
    },
    // Sidetone get / set 0–100.
    Allowed {
        feature: 0x8300,
        function: 0,
        write: false,
        params: |p| p == [0, 0, 0],
    },
    Allowed {
        feature: 0x8300,
        function: 1,
        write: true,
        params: |p| p[0] <= 100 && p[1..] == [0, 0],
    },
    // ADC: battery read, auto-off get, auto-off set (G HUB's menu values).
    Allowed {
        feature: 0x1F20,
        function: 0,
        write: false,
        params: |p| p == [0, 0, 0],
    },
    Allowed {
        feature: 0x1F20,
        function: 1,
        write: false,
        params: |p| p == [0, 0, 0],
    },
    Allowed {
        feature: 0x1F20,
        function: 2,
        write: true,
        params: |p| [0, 1, 2, 5, 10, 15, 30].contains(&p[0]) && p[1..] == [0, 0],
    },
];

/// Every command, every accepted parameter value.
fn all_commands() -> Vec<Command> {
    let mut v = Vec::new();
    for ping in 0..=255u8 {
        v.push(Command::GetProtocolVersion { ping });
    }
    for f in Feature::ALL {
        v.push(Command::GetFeature(f));
    }
    v.push(Command::GetSidetone);
    v.push(Command::ReadBattery);
    v.push(Command::GetAutoOff);
    for x in 0..=255u8 {
        if let Ok(l) = SidetoneLevel::try_from(x) {
            v.push(Command::SetSidetone(l));
        }
        if let Ok(m) = AutoOffMinutes::try_from(x) {
            v.push(Command::SetAutoOff(m));
        }
    }
    // Exhaustiveness guard: a new variant must be added above and to ALLOW.
    for c in &v {
        let _: () = match c {
            Command::GetProtocolVersion { .. }
            | Command::GetFeature(_)
            | Command::GetSidetone
            | Command::SetSidetone(_)
            | Command::ReadBattery
            | Command::GetAutoOff
            | Command::SetAutoOff(_) => (),
        };
    }
    v
}

#[test]
fn every_buildable_frame_is_allow_listed() {
    // Any plausible index assignment; here the headset's own (5, 6) and a
    // swapped one.
    for (st, adc) in [(5u8, 6u8), (6, 5), (1, 0xfe)] {
        let mut map = FeatureMap::default();
        map.set(Feature::Sidetone, st).unwrap();
        map.set(Feature::AdcMeasurement, adc).unwrap();
        let id_at = |i: u8| match i {
            0 => 0x0000,
            i if i == st => 0x8300,
            i if i == adc => 0x1F20,
            _ => panic!("frame to unknown index {i}"),
        };
        let mut writes = 0;
        for c in all_commands() {
            let f = *c.encode(&map).unwrap().as_bytes();
            assert_eq!(f[..2], [0x11, 0xff], "{c:?}");
            assert_eq!(f[3] & 0x0f, 0x0b, "sw_id, {c:?}");
            assert!(f[7..].iter().all(|&b| b == 0), "{c:?}: {f:02x?}");
            let (feature, function) = (id_at(f[2]), f[3] >> 4);
            let a = ALLOW
                .iter()
                .find(|a| a.feature == feature && a.function == function)
                .unwrap_or_else(|| panic!("{c:?} → not allow-listed: {f:02x?}"));
            assert!((a.params)(&f[4..7]), "{c:?}: params {f:02x?}");
            assert_eq!(a.write, c.is_write(), "{c:?}");
            writes += usize::from(c.is_write());
        }
        assert_eq!(writes, 101 + 7);
    }
}

#[test]
fn blocked_features_cannot_be_reached() {
    for f in Feature::ALL {
        assert!(!BLOCKED_FEATURE_IDS.contains(&f.id()));
    }
    // No frame of any command carries a blocked feature id.
    let mut map = FeatureMap::default();
    map.set(Feature::Sidetone, 5).unwrap();
    map.set(Feature::AdcMeasurement, 6).unwrap();
    for c in all_commands() {
        let f = *c.encode(&map).unwrap().as_bytes();
        let id = u16::from_be_bytes([f[4], f[5]]);
        assert!(
            !(f[2] == 0 && f[3] >> 4 == 0 && BLOCKED_FEATURE_IDS.contains(&id)),
            "{c:?}"
        );
    }
}

#[test]
fn feature_commands_need_a_looked_up_index() {
    let empty = FeatureMap::default();
    for c in all_commands() {
        let r = c.encode(&empty);
        match c {
            Command::GetProtocolVersion { .. } | Command::GetFeature(_) => assert!(r.is_ok()),
            _ => assert!(r.is_err(), "{c:?} encoded without a feature index"),
        }
    }
}
