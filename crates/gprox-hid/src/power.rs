//! Battery voltage → charge, from G HUB's power model for this headset.
//!
//! G HUB ships a per-device power model (`pro_x_wireless_headset_battery.xml`
//! in its `pro_headset` depot, next to the preset file). The model is data from
//! the user's own G HUB install, so gprox reads it at run time and never ships
//! it. Layout and our reading of it:
//!
//! - `<discharge>`: rows `minutes, volts, mAh drawn` of a full discharge at a
//!   constant load, voltage falling. Charge left at voltage *v* = 1 − mAh(v) /
//!   mAh(end); hours left = (mAh(end) − mAh(v)) / load.
//! - `<charging><ascending>`: rows `volts, SoC %` while the voltage rises (CC
//!   phase); `<descending>`: rows `volts, SoC %` after the peak, where the
//!   voltage sags while the charge still rises (topping).
//! - `<blocks><mcu power="53"/>`: the load in mA.
//!
//! With G HUB's own readout (64 %, "approx 15 hours") this
//! gives 64 % and 15.3 h at 3804 mV.

use std::path::Path;

/// File name of the model in G HUB's depot.
pub const GHUB_FILE: &str = "pro_x_wireless_headset_battery.xml";

#[derive(Debug, Clone, PartialEq)]
pub struct PowerModel {
    /// (volts, mAh drawn), sorted by voltage.
    discharge: Vec<(f64, f64)>,
    /// mAh drawn at the end of the discharge curve.
    capacity_mah: f64,
    /// (volts, SoC %) while the voltage rises, sorted by voltage.
    ascending: Vec<(f64, f64)>,
    /// (volts, SoC %) after the voltage peak, sorted by voltage.
    descending: Vec<(f64, f64)>,
    /// Load in mA.
    load_ma: f64,
}

/// Charge estimate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Charge {
    /// 0–100.
    pub percent: f64,
    /// Hours of use left (only while discharging).
    pub hours_left: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
#[error("power model: {0}")]
pub struct PowerModelError(String);

fn err(s: impl Into<String>) -> PowerModelError {
    PowerModelError(s.into())
}

/// The CDATA text of the first `<tag …>` element after `from`.
fn section<'a>(xml: &'a str, tag: &str) -> Result<&'a str, PowerModelError> {
    let start = xml
        .find(&format!("<{tag}"))
        .ok_or_else(|| err(format!("no <{tag}>")))?;
    let rest = &xml[start..];
    let end = rest
        .find(&format!("</{tag}>"))
        .ok_or_else(|| err(format!("no </{tag}>")))?;
    let body = &rest[..end];
    let c = body
        .find("<![CDATA[")
        .ok_or_else(|| err(format!("no CDATA in <{tag}>")))?;
    let body = &body[c + 9..];
    let e = body.find("]]>").unwrap_or(body.len());
    Ok(&body[..e])
}

fn rows(text: &str, cols: usize) -> Result<Vec<Vec<f64>>, PowerModelError> {
    text.split_whitespace()
        .map(|line| {
            let v: Result<Vec<f64>, _> = line.split(',').map(str::parse::<f64>).collect();
            match v {
                Ok(v) if v.len() == cols && v.iter().all(|x| x.is_finite()) => Ok(v),
                _ => Err(err(format!("bad row {line:?}"))),
            }
        })
        .collect()
}

/// Linear interpolation of `y` at `x` over points sorted by `x` ascending.
fn interp(pts: &[(f64, f64)], x: f64) -> f64 {
    let (first, last) = (pts[0], pts[pts.len() - 1]);
    if x <= first.0 {
        return first.1;
    }
    if x >= last.0 {
        return last.1;
    }
    let i = pts.partition_point(|p| p.0 <= x);
    let (a, b) = (pts[i - 1], pts[i]);
    if b.0 == a.0 {
        return a.1;
    }
    a.1 + (b.1 - a.1) * (x - a.0) / (b.0 - a.0)
}

/// Sort by voltage and merge equal voltages (mean of their values), so the
/// curve is a function of voltage.
fn by_voltage(mut pts: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64, usize)> = Vec::new();
    for (v, y) in pts {
        match out.last_mut() {
            Some(l) if l.0 == v => {
                l.1 += y;
                l.2 += 1;
            }
            _ => out.push((v, y, 1)),
        }
    }
    out.into_iter().map(|(v, y, n)| (v, y / n as f64)).collect()
}

impl PowerModel {
    pub fn parse(xml: &str) -> Result<PowerModel, PowerModelError> {
        let discharge: Vec<(f64, f64)> = rows(section(xml, "discharge")?, 3)?
            .into_iter()
            .map(|r| (r[1], r[2]))
            .collect();
        let ascending: Vec<(f64, f64)> = rows(section(xml, "ascending")?, 2)?
            .into_iter()
            .map(|r| (r[0], r[1]))
            .collect();
        let descending: Vec<(f64, f64)> = rows(section(xml, "descending")?, 2)?
            .into_iter()
            .map(|r| (r[0], r[1]))
            .collect();
        let load_ma = xml
            .find("<mcu")
            .and_then(|i| {
                let s = &xml[i..];
                let p = s.find("power=\"")? + 7;
                let e = s[p..].find('"')?;
                s[p..p + e].parse::<f64>().ok()
            })
            .ok_or_else(|| err("no <mcu power=…>"))?;
        if discharge.len() < 2 || ascending.len() < 2 || descending.is_empty() {
            return Err(err("curves too short"));
        }
        if load_ma.is_nan() || load_ma <= 0.0 {
            return Err(err("load must be > 0"));
        }
        let capacity_mah = discharge.iter().map(|r| r.1).fold(0.0, f64::max);
        if capacity_mah.is_nan() || capacity_mah <= 0.0 {
            return Err(err("discharge curve has no capacity"));
        }
        Ok(PowerModel {
            discharge: by_voltage(discharge),
            capacity_mah,
            ascending: by_voltage(ascending),
            descending: by_voltage(descending),
            load_ma,
        })
    }

    pub fn load(path: &Path) -> Result<PowerModel, PowerModelError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| err(format!("read {}: {e}", path.display())))?;
        PowerModel::parse(&text)
    }

    /// Charge while running on battery.
    pub fn discharging(&self, millivolts: u16) -> Charge {
        let v = f64::from(millivolts) / 1000.0;
        let cap = self.capacity_mah;
        let used = interp(&self.discharge, v).clamp(0.0, cap);
        Charge {
            percent: (100.0 * (1.0 - used / cap)).clamp(0.0, 100.0),
            hours_left: Some((cap - used) / self.load_ma),
        }
    }

    /// Charge while charging. `topping`: the voltage has passed the
    /// ascending curve's peak (it sags during topping while the charge still
    /// rises), so the descending curve applies.
    pub fn charging(&self, millivolts: u16, topping: bool) -> Charge {
        let v = f64::from(millivolts) / 1000.0;
        let curve = if topping {
            &self.descending
        } else {
            &self.ascending
        };
        Charge {
            percent: interp(curve, v).clamp(0.0, 100.0),
            hours_left: None,
        }
    }

    /// Voltage at the end of the ascending (CC) curve.
    pub fn charge_peak_millivolts(&self) -> u16 {
        let v = self.ascending.last().map_or(0.0, |r| r.0);
        (v * 1000.0).round() as u16
    }
}

/// Tracks charging across readings: recognises the topping phase and never
/// shows less charge while charging than before the charger was plugged in
/// or than the last charging reading.
///
/// The charging curve is a charge from empty: right after plugging in, the
/// voltage jumps and the curve reads far too low (live test: 93 % on battery,
/// then 39 % at 4045 mV a second after plugging in). How
/// G HUB handles this is not known; holding the pre-charge value is our
/// choice.
#[derive(Debug, Clone, Default)]
pub struct ChargeTracker {
    topping: bool,
    last_charging_percent: Option<f64>,
    last_discharging_percent: Option<f64>,
}

impl ChargeTracker {
    pub fn estimate(&mut self, model: &PowerModel, millivolts: u16, charging: bool) -> Charge {
        if !charging {
            self.topping = false;
            self.last_charging_percent = None;
            let c = model.discharging(millivolts);
            self.last_discharging_percent = Some(c.percent);
            return c;
        }
        if millivolts >= model.charge_peak_millivolts() {
            self.topping = true;
        }
        let mut c = model.charging(millivolts, self.topping);
        for p in [self.last_charging_percent, self.last_discharging_percent]
            .into_iter()
            .flatten()
        {
            c.percent = c.percent.max(p);
        }
        self.last_charging_percent = Some(c.percent);
        c
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A made-up model in G HUB's layout (not G HUB data): 1000 mAh linear
    /// from 4.1 V to 3.3 V, 50 mA load.
    pub(crate) const SYNTH: &str = r#"<?xml version="1.0"?>
<powermodel vid="0x46d" pid="0x0aba"><battery>
<discharge format="default"><![CDATA[
0,4.1,0
600,3.7,500
600.5,3.7,500.5
1200,3.3,1000
]]></discharge>
<charging format="charging_topping"><ascending><![CDATA[
3.4,0
4.0,50
4.2,80
]]></ascending><descending><![CDATA[
4.19,85
4.15,100
]]></descending></charging></battery>
<blocks default_unit="milliamps"><mcu power="50"/></blocks></powermodel>"#;

    #[test]
    fn synthetic_model() {
        let m = PowerModel::parse(SYNTH).unwrap();
        let c = m.discharging(3700);
        assert!((c.percent - 50.0).abs() < 0.1, "{c:?}");
        assert!((c.hours_left.unwrap() - 10.0).abs() < 0.02);
        assert_eq!(m.discharging(4200).percent, 100.0);
        assert_eq!(m.discharging(3000).percent, 0.0);
        assert!((m.discharging(3900).percent - 75.0).abs() < 0.05);
        assert_eq!(m.charge_peak_millivolts(), 4200);
        assert!((m.charging(3700, false).percent - 25.0).abs() < 1e-9);
        assert!((m.charging(4170, true).percent - 92.5).abs() < 1e-9);
    }

    #[test]
    fn tracker_topping_and_monotonic() {
        let m = PowerModel::parse(SYNTH).unwrap();
        let mut t = ChargeTracker::default();
        assert!((t.estimate(&m, 4000, true).percent - 50.0).abs() < 1e-9);
        // Voltage blip down while charging: no backwards jump.
        assert!((t.estimate(&m, 3900, true).percent - 50.0).abs() < 1e-9);
        assert!((t.estimate(&m, 4200, true).percent - 85.0).abs() < 1e-9);
        // Sagging voltage in the topping phase reads the descending curve.
        assert!((t.estimate(&m, 4150, true).percent - 100.0).abs() < 1e-9);
        // Unplugged: back to the discharge curve.
        assert!(t.estimate(&m, 4100, false).hours_left.is_some());
        // Plugged in at 75 %: the voltage jump reads 50 % on the charging
        // curve, but the shown charge stays at 75 % until the curve passes it.
        let mut t = ChargeTracker::default();
        assert!((t.estimate(&m, 3900, false).percent - 75.0).abs() < 0.05);
        assert!((t.estimate(&m, 4000, true).percent - 75.0).abs() < 0.05);
        assert!((t.estimate(&m, 4100, true).percent - 75.0).abs() < 0.05);
        assert!((t.estimate(&m, 4180, true).percent - 77.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_garbage() {
        assert!(PowerModel::parse("<powermodel/>").is_err());
        assert!(PowerModel::parse(&SYNTH.replace("600,3.7,500\n", "600,x,500\n")).is_err());
        assert!(PowerModel::parse(&SYNTH.replace("power=\"50\"", "power=\"0\"")).is_err());
    }

    /// G HUB's own model file (local only, skipped when absent; `GPROX_RE_DIR`
    /// points at its location). Checks the reading of the model against G
    /// HUB's readout "64 %, approx 15 hours" and voltages seen in USB captures.
    #[test]
    fn ghub_model_matches_ghub_readout() {
        let re = std::env::var_os("GPROX_RE_DIR").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../re"),
            std::path::PathBuf::from,
        );
        let p = re
            .join("ghub/collect-20261120-233900/files/ProgramData_LGHUB/depots/891982/pro_headset")
            .join(GHUB_FILE);
        let Ok(m) = PowerModel::load(&p) else {
            if std::env::var_os("GPROX_REQUIRE_VECTORS").is_some_and(|v| v == "1") {
                panic!("GPROX_REQUIRE_VECTORS=1, but {} not found", p.display());
            }
            eprintln!("SKIP: {} not found", p.display());
            return;
        };
        let c = m.discharging(3804);
        assert_eq!(c.percent.round(), 64.0, "{c:?}");
        assert_eq!(c.hours_left.unwrap().round(), 15.0, "{c:?}");
        let c = m.discharging(3798);
        assert!((62.0..63.5).contains(&c.percent), "{c:?}");
        // The curve starts at 4.102 V with 0.34 mAh already used, so "full"
        // is 99.97 %: 100 % as displayed (whole percent, like G HUB).
        assert_eq!(m.discharging(4110).percent.round(), 100.0);
        assert_eq!(m.discharging(3200).percent, 0.0);
    }
}
