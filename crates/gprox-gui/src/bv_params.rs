//! The Blue VO!CE editor's layout: modules in engine order, their switch and
//! their parameters with display units. Ranges come from
//! `gprox_core::PARAM_RANGES`; linear gains are shown in dB.

/// How a parameter is shown and dragged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unit {
    /// 0/1 switch: a checkbox.
    Switch,
    /// Linear gain shown in dB. `floor_db` is the slider's bottom when the
    /// range starts at 0 (the slider's lowest value maps to 0).
    LinDb { floor_db: f32 },
    /// dB value.
    Db,
    /// Frequency, logarithmic slider.
    Hz,
    /// Time.
    Ms,
    /// Ratio `n:1`.
    Ratio,
    /// Filter width (Q).
    Width,
}

#[derive(Debug, Clone, Copy)]
pub struct Param {
    pub name: &'static str,
    pub label: &'static str,
    pub unit: Unit,
}

#[derive(Debug, Clone, Copy)]
pub struct Module {
    pub title: &'static str,
    /// The module's on/off switch (the limiter has none: it always runs).
    pub enable: Option<&'static str>,
    pub params: &'static [Param],
}

const fn p(name: &'static str, label: &'static str, unit: Unit) -> Param {
    Param { name, label, unit }
}

const GAIN: Unit = Unit::LinDb { floor_db: -40.0 };

/// Voice EQ (the module people touch most) first, then engine order
/// (bluevoice-api.md): de-popper, NR, expander, de-esser, compressor,
/// limiter, then the output volume.
pub const MODULES: &[Module] = &[
    Module {
        title: "Voice EQ",
        enable: Some("veq.enable"),
        params: &[
            p("veq.low_gain", "Low gain", GAIN),
            p("veq.low_freq", "Low frequency", Unit::Hz),
            p("veq.low_width", "Low width", Unit::Width),
            p("veq.mid_gain", "Mid gain", GAIN),
            p("veq.mid_freq", "Mid frequency", Unit::Hz),
            p("veq.mid_width", "Mid width", Unit::Width),
            p("veq.high_gain", "High gain", GAIN),
            p("veq.high_freq", "High frequency", Unit::Hz),
            p("veq.high_width", "High width", Unit::Width),
        ],
    },
    Module {
        title: "De-popper",
        enable: Some("dpop.enable"),
        params: &[p("dpop.freq", "Frequency", Unit::Hz)],
    },
    Module {
        title: "Noise reduction",
        enable: Some("nr.enable"),
        params: &[
            p("nr.sensitivity", "Sensitivity", Unit::Db),
            p("nr.attenuation_max", "Max attenuation", Unit::Db),
            p("nr.bias", "Bias", Unit::Db),
            p("nr.release", "Release", Unit::Ms),
        ],
    },
    Module {
        title: "Expander / gate",
        enable: Some("exp.enable"),
        params: &[
            p("exp.thresh", "Threshold", Unit::Db),
            p("exp.thresh_auto_enable", "Auto threshold", Unit::Switch),
            p("exp.thresh_auto_sense", "Auto sensitivity", Unit::Db),
            p("exp.ratio", "Ratio", Unit::Ratio),
            p("exp.attenuation_max", "Range", Unit::Db),
            p("exp.attack", "Attack", Unit::Ms),
            p("exp.release", "Release", Unit::Ms),
            p("exp.hysteresis", "Hold", Unit::Ms),
        ],
    },
    Module {
        title: "De-esser",
        enable: Some("des.enable"),
        params: &[
            p("des.thresh", "Threshold", Unit::Db),
            p("des.ratio", "Ratio", Unit::Ratio),
            p("des.freq", "Frequency", Unit::Hz),
            p("des.attack", "Attack", Unit::Ms),
            p("des.release", "Release", Unit::Ms),
        ],
    },
    Module {
        title: "Compressor",
        enable: Some("comp.enable"),
        params: &[
            p("comp.thresh", "Threshold", Unit::Db),
            p("comp.ratio", "Ratio", Unit::Ratio),
            p("comp.attack", "Attack", Unit::Ms),
            p("comp.release", "Release", Unit::Ms),
            p("comp.makeup", "Makeup gain", GAIN),
        ],
    },
    Module {
        title: "Limiter",
        enable: None,
        params: &[
            p("lim.boost", "Boost", GAIN),
            p("lim.attack", "Attack", Unit::Ms),
            p("lim.release", "Release", Unit::Ms),
        ],
    },
    Module {
        title: "Output",
        enable: None,
        params: &[p("micopt.volume", "Volume", GAIN)],
    },
];

pub fn lin_to_db(v: f32) -> f32 {
    20.0 * v.max(1e-9).log10()
}

pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// The slider range for a linear-gain parameter with range `lo..=hi`, in dB.
pub fn db_range(lo: f32, hi: f32, floor_db: f32) -> (f32, f32) {
    let lo_db = if lo > 0.0 { lin_to_db(lo) } else { floor_db };
    (lo_db, lin_to_db(hi))
}

/// A dragged dB value back to a linear value inside `lo..=hi` (the slider's
/// bottom is 0 when the range starts at 0).
pub fn db_value_to_lin(db: f32, lo: f32, hi: f32, floor_db: f32) -> f32 {
    if lo <= 0.0 && db <= floor_db {
        return lo;
    }
    db_to_lin(db).clamp(lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gprox_core::{PARAM_RANGES, param_range};

    /// Every editor parameter exists with a range, every engine parameter
    /// except master on/off, balance and the meters is in the editor, once.
    #[test]
    fn layout_covers_the_engine() {
        let mut names: Vec<&str> = Vec::new();
        for m in MODULES {
            names.extend(m.enable);
            names.extend(m.params.iter().map(|p| p.name));
        }
        for n in &names {
            assert!(param_range(n).is_some(), "{n}");
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicates");
        for &(n, _, _) in PARAM_RANGES {
            let hidden = ["micopt.enable", "micopt.balance", "micopt.meter_enable"];
            assert_eq!(names.contains(&n), !hidden.contains(&n), "{n}");
        }
        for m in MODULES {
            for p in m.params {
                assert_eq!(
                    p.unit == Unit::Switch,
                    p.name.ends_with("enable"),
                    "{}",
                    p.name
                );
            }
        }
    }

    /// dB sliders map back into the parameter's range, ends included.
    #[test]
    fn db_mapping_stays_in_range() {
        for name in ["veq.low_gain", "lim.boost", "comp.makeup", "micopt.volume"] {
            let (lo, hi) = param_range(name).unwrap();
            let (lo_db, hi_db) = db_range(lo, hi, -40.0);
            for db in [lo_db - 1.0, lo_db, 0.0, hi_db, hi_db + 1.0] {
                let v = db_value_to_lin(db, lo, hi, -40.0);
                assert!((lo..=hi).contains(&v), "{name} {db} → {v}");
            }
            assert_eq!(db_value_to_lin(0.0, lo, hi, -40.0), 1.0, "{name}");
        }
        assert_eq!(db_value_to_lin(-40.0, 0.0, 10.0, -40.0), 0.0);
        let (lo, hi) = param_range("veq.mid_gain").unwrap();
        let (a, b) = db_range(lo, hi, -40.0);
        assert!(
            (a + 15.0).abs() < 1e-3 && (b - 15.0).abs() < 1e-3,
            "{a} {b}"
        );
    }
}
