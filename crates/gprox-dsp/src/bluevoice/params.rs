//! Blue VO!CE parameters in G HUB's API units: the 44 values G HUB's `set_defaults` writes, addressed by the
//! engine's parameter names. Gains are linear where the API is linear
//! (`veq.*_gain`, `micopt.volume`, `comp.makeup`, `lim.boost`), thresholds in
//! dB, times in ms, frequencies in Hz.

/// Parameter ids, in G HUB's `set_defaults` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum P {
    Enable,
    Balance,
    Volume,
    DpopEnable,
    DpopFreq,
    NrEnable,
    NrRelease,
    NrSensitivity,
    NrBias,
    NrAttenuationMax,
    ExpEnable,
    ExpAttack,
    ExpRelease,
    ExpHysteresis,
    ExpRatio,
    ExpThresh,
    ExpThreshAutoEnable,
    ExpThreshAutoSense,
    ExpAttenuationMax,
    VeqEnable,
    VeqLowGain,
    VeqLowFreq,
    VeqLowWidth,
    VeqMidGain,
    VeqMidFreq,
    VeqMidWidth,
    VeqHighGain,
    VeqHighFreq,
    VeqHighWidth,
    DesEnable,
    DesAttack,
    DesRelease,
    DesRatio,
    DesThresh,
    DesFreq,
    CompEnable,
    CompAttack,
    CompRelease,
    CompRatio,
    CompThresh,
    CompMakeup,
    LimBoost,
    LimAttack,
    LimRelease,
}

/// Number of parameters.
pub const COUNT: usize = 44;

/// Whether a parameter is an integer switch (any non-zero bit pattern = on).
const fn is_switch(p: usize) -> bool {
    matches!(p, 0 | 3 | 5 | 10 | 16 | 19 | 29 | 35)
}

/// Engine names, indexed by [`P`].
pub const NAMES: [&str; COUNT] = [
    "micopt.enable",
    "micopt.balance",
    "micopt.volume",
    "dpop.enable",
    "dpop.freq",
    "nr.enable",
    "nr.release",
    "nr.sensitivity",
    "nr.bias",
    "nr.attenuation_max",
    "exp.enable",
    "exp.attack",
    "exp.release",
    "exp.hysteresis",
    "exp.ratio",
    "exp.thresh",
    "exp.thresh_auto_enable",
    "exp.thresh_auto_sense",
    "exp.attenuation_max",
    "veq.enable",
    "veq.low_gain",
    "veq.low_freq",
    "veq.low_width",
    "veq.mid_gain",
    "veq.mid_freq",
    "veq.mid_width",
    "veq.high_gain",
    "veq.high_freq",
    "veq.high_width",
    "des.enable",
    "des.attack",
    "des.release",
    "des.ratio",
    "des.thresh",
    "des.freq",
    "comp.enable",
    "comp.attack",
    "comp.release",
    "comp.ratio",
    "comp.thresh",
    "comp.makeup",
    "lim.boost",
    "lim.attack",
    "lim.release",
];

/// G HUB's `set_defaults` values, switches as 0/1.
const DEFAULTS: [f32; COUNT] = [
    1.0, 0.0, 1.0, // micopt
    0.0, 80.0, // dpop
    0.0, 75.0, 25.0, 2.5, -20.0, // nr
    0.0, 5.0, 250.0, 5.0, 2.0, -48.0, 0.0, 20.0, -20.0, // exp
    0.0, 1.0, 250.0, 1.0, 1.0, 3000.0, 4.0, 1.0, 8000.0, 1.0, // veq
    0.0, 5.0, 250.0, 4.0, 0.0, 8000.0, // des
    0.0, 5.0, 250.0, 4.0, -20.0, 1.0, // comp
    1.0, 5.0, 250.0, // lim
];

/// Error from [`BvParams::set`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownParam(pub String);

impl std::fmt::Display for UnknownParam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown Blue VO!CE parameter {:?}", self.0)
    }
}

impl std::error::Error for UnknownParam {}

/// A complete Blue VO!CE parameter set (what G HUB sends for a preset).
///
/// Stored as 32-bit words: switches as 0/1, everything else as `f32` bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BvParams {
    words: [u32; COUNT],
}

impl Default for BvParams {
    /// G HUB's defaults: Blue VO!CE on, every module off, volume 1.
    fn default() -> Self {
        let mut words = [0u32; COUNT];
        for (i, (w, &d)) in words.iter_mut().zip(DEFAULTS.iter()).enumerate() {
            *w = if is_switch(i) {
                u32::from(d != 0.0)
            } else {
                d.to_bits()
            };
        }
        BvParams { words }
    }
}

impl BvParams {
    /// Value of a continuous parameter.
    pub fn f(&self, p: P) -> f32 {
        f32::from_bits(self.words[p as usize])
    }

    /// State of a switch.
    pub fn on(&self, p: P) -> bool {
        self.words[p as usize] != 0
    }

    /// Set a continuous parameter.
    pub fn set_f(&mut self, p: P, v: f32) {
        self.words[p as usize] = v.to_bits();
    }

    /// Set a switch.
    pub fn set_on(&mut self, p: P, on: bool) {
        self.words[p as usize] = u32::from(on);
    }

    /// Look up a parameter by its engine name.
    pub fn id(name: &str) -> Option<P> {
        NAMES.iter().position(|&n| n == name).map(Self::from_index)
    }

    fn from_index(i: usize) -> P {
        const ALL: [P; COUNT] = {
            use P::*;
            [
                Enable,
                Balance,
                Volume,
                DpopEnable,
                DpopFreq,
                NrEnable,
                NrRelease,
                NrSensitivity,
                NrBias,
                NrAttenuationMax,
                ExpEnable,
                ExpAttack,
                ExpRelease,
                ExpHysteresis,
                ExpRatio,
                ExpThresh,
                ExpThreshAutoEnable,
                ExpThreshAutoSense,
                ExpAttenuationMax,
                VeqEnable,
                VeqLowGain,
                VeqLowFreq,
                VeqLowWidth,
                VeqMidGain,
                VeqMidFreq,
                VeqMidWidth,
                VeqHighGain,
                VeqHighFreq,
                VeqHighWidth,
                DesEnable,
                DesAttack,
                DesRelease,
                DesRatio,
                DesThresh,
                DesFreq,
                CompEnable,
                CompAttack,
                CompRelease,
                CompRatio,
                CompThresh,
                CompMakeup,
                LimBoost,
                LimAttack,
                LimRelease,
            ]
        };
        ALL[i]
    }

    /// Set by engine name from the raw 32-bit value G HUB passes: switches
    /// are on for any non-zero pattern (G HUB sends integer 1, `set_defaults`
    /// sends 1.0f), everything else is `f32` bits. `micopt.meter_enable` is
    /// accepted and ignored (meters only).
    pub fn set_raw(&mut self, name: &str, bits: u32) -> Result<(), UnknownParam> {
        if name == "micopt.meter_enable" {
            return Ok(());
        }
        let p = Self::id(name).ok_or_else(|| UnknownParam(name.to_owned()))? as usize;
        self.words[p] = if is_switch(p) {
            u32::from(bits != 0)
        } else {
            bits
        };
        Ok(())
    }

    /// Set by engine name from an `f32` (switches: non-zero = on).
    pub fn set(&mut self, name: &str, v: f32) -> Result<(), UnknownParam> {
        let p = Self::id(name);
        match p {
            Some(p) if is_switch(p as usize) => self.set_raw(name, u32::from(v != 0.0)),
            _ => self.set_raw(name, v.to_bits()),
        }
    }

    pub(crate) fn to_words(self) -> [u32; COUNT] {
        self.words
    }

    pub(crate) fn from_words(words: [u32; COUNT]) -> Self {
        BvParams { words }
    }

    /// The Broadcaster 1 preset (G HUB's values exactly, as sent during B5).
    pub fn broadcaster1() -> Self {
        let mut p = Self::default();
        let vals: [(P, f32); 44] = {
            use P::*;
            [
                (Enable, 1.0),
                (Balance, 0.0),
                (Volume, 0.7),
                (DpopEnable, 1.0),
                (DpopFreq, 85.0),
                (NrEnable, 1.0),
                (NrRelease, 250.0),
                (NrSensitivity, 6.0),
                (NrBias, 3.0),
                (NrAttenuationMax, -18.0),
                (ExpEnable, 1.0),
                (ExpAttack, 2.0),
                (ExpRelease, 100.0),
                (ExpHysteresis, 5.0),
                (ExpRatio, 4.0),
                (ExpThresh, -33.0),
                (ExpThreshAutoEnable, 0.0),
                (ExpThreshAutoSense, 20.0),
                (ExpAttenuationMax, -20.0),
                (VeqEnable, 1.0),
                (VeqLowGain, 4.488_916_4),
                (VeqLowFreq, 200.0),
                (VeqLowWidth, 2.0),
                (VeqMidGain, 1.085_425_5),
                (VeqMidFreq, 900.0),
                (VeqMidWidth, 1.0),
                (VeqHighGain, 2.238_721_1),
                (VeqHighFreq, 4000.0),
                (VeqHighWidth, 1.0),
                (DesEnable, 1.0),
                (DesAttack, 3.0),
                (DesRelease, 250.0),
                (DesRatio, 14.0),
                (DesThresh, -5.0),
                (DesFreq, 6975.0),
                (CompEnable, 1.0),
                (CompAttack, 2.0),
                (CompRelease, 180.0),
                (CompRatio, 4.0),
                (CompThresh, -7.0),
                (CompMakeup, 1.0),
                (LimBoost, 4.466_836),
                (LimAttack, 5.0),
                (LimRelease, 250.0),
            ]
        };
        for (id, v) in vals {
            p.set(NAMES[id as usize], v).expect("known name");
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_ids_agree() {
        for (i, n) in NAMES.iter().enumerate() {
            assert_eq!(BvParams::id(n).map(|p| p as usize), Some(i));
        }
        assert!(BvParams::id("nope").is_none());
    }

    #[test]
    fn switches_accept_int_and_float_patterns() {
        let mut p = BvParams::default();
        p.set_raw("comp.enable", 1).unwrap();
        assert!(p.on(P::CompEnable));
        p.set_raw("comp.enable", 0).unwrap();
        assert!(!p.on(P::CompEnable));
        p.set_raw("comp.enable", 1.0f32.to_bits()).unwrap();
        assert!(p.on(P::CompEnable));
        assert!(p.set_raw("micopt.meter_enable", 1).is_ok());
        assert!(p.set_raw("bogus", 1).is_err());
    }

    #[test]
    fn defaults() {
        let p = BvParams::default();
        assert!(p.on(P::Enable));
        assert!(!p.on(P::VeqEnable));
        assert_eq!(p.f(P::DesFreq), 8000.0);
        assert_eq!(p.f(P::Volume), 1.0);
    }
}
