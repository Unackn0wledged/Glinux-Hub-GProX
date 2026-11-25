//! Runtime control: preset selection and the control socket.
//!
//! [`Controller`] owns the control-side DSP handles ([`HpEqHandle`],
//! [`BlueVoiceHandle`]) and the persisted [`State`]. It lives behind a mutex
//! that only the socket threads take; the PipeWire data thread never sees it.
//! Changes reach the DSP only through the handles' lock-free cells and take
//! effect at the next DSP block (headphone EQ: a 100 ms gain ramp, like G HUB;
//! Blue VO!CE: the next 256-frame engine block with its parameter smoothing).
//!
//! Editing: live edits are kept next to the
//! selection (`hp_preset` vs `hp_saved`, `bv_edit`), never in the selected
//! preset itself, and reach the DSP the same way as a preset switch. `save_*`
//! writes user preset files; presets from G HUB are read-only.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, bail};
use gprox_core::HeadphoneEqPreset;
use gprox_core::control::{
    BlueVoiceStatus, FLAT_PRESET, HeadsetStatus, HpEqStatus, MAX_LINE, Request, Response, Status,
};
use gprox_core::{BlueVoicePreset, param_range};
use gprox_dsp::bluevoice::{BlueVoiceHandle, BvParams, P};
use gprox_dsp::{EqMode, HpEqHandle, HpEqSettings};
use gprox_hid::{AutoOffMinutes, HeadsetService, SidetoneLevel};

use crate::state::State;

/// Fixed inputs of the controller.
#[derive(Debug)]
pub struct Config {
    /// Directory of headphone EQ presets (`<name>.toml`).
    pub hp_dir: PathBuf,
    /// A preset given on the command line (`--hp-eq FILE`), listed by its
    /// file stem next to the directory's presets.
    pub hp_extra: Option<(String, HeadphoneEqPreset)>,
    /// Blue VO!CE presets: G HUB's preset file, then imported presets
    /// (`<config>/bluevoice/*.toml`, from `gprox import ghub`); `None` = none
    /// at all, the mic path is a pass-through.
    pub bv_presets: Option<Vec<BlueVoicePreset>>,
    /// Directory of the user's Blue VO!CE presets (`<config>/bluevoice`),
    /// where `save_bluevoice` writes.
    pub bv_dir: PathBuf,
    /// Where the [`State`] is persisted.
    pub state_path: PathBuf,
}

/// The always-on sidetone stream, shared between the controller (control
/// threads) and gproxd's PipeWire main loop, which creates the stream while
/// `wanted` is set and reports whether it streams.
#[derive(Debug, Default)]
pub struct KeepOpen {
    /// The setting: keep the headset mic stream open.
    pub wanted: AtomicBool,
    /// The keep-alive stream is streaming.
    pub streaming: AtomicBool,
}

#[derive(Debug)]
pub struct Controller {
    cfg: Config,
    state: State,
    hp_selected: String,
    /// The live settings (what the DSP gets).
    hp_preset: HeadphoneEqPreset,
    /// The selected preset as stored; differs from `hp_preset` after edits.
    hp_saved: HeadphoneEqPreset,
    /// gprox's own headphone EQ presets ([`gprox_core::builtin`]).
    hp_builtin: Vec<(String, HeadphoneEqPreset)>,
    /// Index into `cfg.bv_presets`.
    bv_selected: Option<usize>,
    /// The live Blue VO!CE preset after edits (the selected preset with the
    /// edited values); `None` = no edits.
    bv_edit: Option<BlueVoicePreset>,
    bv_enabled: bool,
    hp: Option<HpEqHandle>,
    bv: Option<BlueVoiceHandle>,
    headset: Option<HeadsetService>,
    keep_open: Arc<KeepOpen>,
}

/// Case-insensitive, spaces/dashes/underscores alike.
fn norm(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace([' ', '-'], "_")
}

/// First line of preset files written by `gprox import ghub`.
const IMPORTED_MARK: &str = "# Imported from G HUB";

/// A name for a new user preset file (`<name>.toml`): trimmed, 1–64
/// characters, no path separators or control characters, no leading dot.
fn preset_file_name(name: &str) -> anyhow::Result<&str> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 64 {
        bail!("a preset name must have 1 to 64 characters");
    }
    if n.starts_with('.') || n.chars().any(|c| c == '/' || c == '\\' || c.is_control()) {
        bail!("preset name {n:?}: no '/', '\\', control characters or leading '.'");
    }
    Ok(n)
}

/// Write `path` atomically (temp file in the same directory + rename).
fn write_atomic(path: &Path, text: &str) -> anyhow::Result<()> {
    let dir = path.parent().context("preset path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().context("preset path has no file name")?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(".tmp");
    let tmp = dir.join(tmp_name);
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

/// Check one edited Blue VO!CE value: a known engine parameter other than
/// `micopt.enable`, finite, inside [`gprox_core::PARAM_RANGES`], switches 0
/// or 1.
fn check_bv_value(name: &str, v: f32) -> anyhow::Result<()> {
    if name == "micopt.enable" {
        bail!("micopt.enable is Blue VO!CE on/off: use bluevoice_enable");
    }
    let (lo, hi) =
        param_range(name).with_context(|| format!("unknown Blue VO!CE parameter {name:?}"))?;
    if !v.is_finite() || !(lo..=hi).contains(&v) {
        bail!("Blue VO!CE parameter {name}: {v} is outside {lo}…{hi}");
    }
    if name.ends_with("enable") && v != 0.0 && v != 1.0 {
        bail!("Blue VO!CE switch {name}: {v} is not 0 or 1");
    }
    Ok(())
}

/// The live preset saved in `state.toml` (all its values) over the selected
/// preset, checked like a preset file.
fn restore_bv_edit(
    selected: &BlueVoicePreset,
    edit: &BTreeMap<String, f32>,
) -> anyhow::Result<BlueVoicePreset> {
    let mut live = BlueVoicePreset::from_params(
        selected.name.clone(),
        edit.iter().map(|(k, v)| (k.as_str(), *v)),
    )?;
    live.ghub_id.clone_from(&selected.ghub_id);
    bv_params(Some(&live), true)?;
    Ok(live)
}

/// The full engine parameter set of a preset (engine defaults with the
/// preset's values applied), by engine name; switches as 0/1.
fn full_params(p: &BlueVoicePreset) -> anyhow::Result<BTreeMap<String, f32>> {
    let mut e = BvParams::default();
    for (name, v) in p.params() {
        e.set(&name, v)?;
    }
    Ok(gprox_dsp::bluevoice::params::NAMES
        .iter()
        .filter_map(|&n| {
            let id = BvParams::id(n)?;
            let v = if n.ends_with("enable") {
                f32::from(u8::from(e.on(id)))
            } else {
                e.f(id)
            };
            Some((n.to_owned(), v))
        })
        .collect())
}

/// Whether `query` names `p`: by its name, with or without G HUB's built-in
/// prefix (`broadcaster 1`, `BLUE_VOICE_PRESET_BROADCASTER_1`, `tsm myth`).
pub fn bv_matches(p: &BlueVoicePreset, query: &str) -> bool {
    let q = norm(query);
    let q = q.strip_prefix("blue_voice_preset_").unwrap_or(&q);
    q == norm(&p.name)
}

pub fn hp_settings(p: &HeadphoneEqPreset) -> HpEqSettings {
    HpEqSettings {
        mode: match p.mode {
            gprox_core::EqMode::Basic => EqMode::Basic,
            gprox_core::EqMode::Advanced => EqMode::Advanced,
        },
        basic: p.basic,
        advanced: p.advanced,
    }
}

/// Engine parameters for a preset, or for "Blue VO!CE off".
///
/// G HUB: a new engine gets `set_defaults` and then
/// the preset's values; "off" is `set_defaults` followed by
/// `micopt.enable = 0` (every module fades out, the limiter, volume and
/// balance keep running). The mic node is mono, and the APO ignores balance
/// for a mono stream, so balance is 0 either way.
pub fn bv_params(preset: Option<&BlueVoicePreset>, enabled: bool) -> anyhow::Result<BvParams> {
    let mut p = BvParams::default();
    match preset {
        Some(preset) if enabled => {
            for (engine, v) in preset.params() {
                p.set(&engine, v)?;
            }
        }
        _ => p.set_on(P::Enable, false),
    }
    p.set_f(P::Balance, 0.0);
    Ok(p)
}

impl Controller {
    /// Resolve the persisted selections against what is available. Names that
    /// no longer exist fall back (headphone EQ: flat; Blue VO!CE: the first
    /// preset) with a warning. No DSP handles yet: see [`Controller::attach`].
    pub fn new(cfg: Config, state: State) -> Self {
        let mut c = Controller {
            cfg,
            state,
            hp_selected: FLAT_PRESET.into(),
            hp_preset: HeadphoneEqPreset::default(),
            hp_saved: HeadphoneEqPreset::default(),
            hp_builtin: gprox_core::builtin::hp_eq_presets(),
            bv_selected: None,
            bv_edit: None,
            bv_enabled: false,
            hp: None,
            bv: None,
            headset: None,
            keep_open: Arc::default(),
        };
        c.keep_open
            .wanted
            .store(c.state.always_on_sidetone, Ordering::Relaxed);
        if let Some(name) = c.state.hp_eq.clone() {
            match c.load_hp(&name) {
                Ok((n, p)) => (c.hp_selected, c.hp_saved, c.hp_preset) = (n, p.clone(), p),
                Err(e) => tracing::warn!(error = %e, "saved headphone EQ preset not usable; flat"),
            }
        }
        if let Some(edit) = c.state.hp_eq_edit.take() {
            match edit.validate() {
                Ok(()) => c.hp_preset = edit,
                Err(e) => tracing::warn!(error = %e, "unsaved headphone EQ edits dropped"),
            }
        }
        if let Some(presets) = &c.cfg.bv_presets {
            c.bv_enabled = c.state.bluevoice_enabled;
            c.bv_selected = match &c.state.bluevoice {
                Some(name) => {
                    let i = presets.iter().position(|p| bv_matches(p, name));
                    if i.is_none() {
                        tracing::warn!(preset = %name, "saved Blue VO!CE preset not found");
                    }
                    i
                }
                None => None,
            };
            if c.bv_selected.is_none() && c.bv_enabled {
                c.bv_selected = (!presets.is_empty()).then_some(0);
            }
            if let Some(Err(e)) = c.bv_params() {
                tracing::warn!(error = %format!("{e:#}"),
                    "saved Blue VO!CE preset not usable; Blue VO!CE off");
                c.bv_enabled = false;
                c.bv_selected = None;
            }
            if let Some(edit) = c.state.bluevoice_edit.take()
                && let Some(i) = c.bv_selected
            {
                match restore_bv_edit(&presets[i], &edit) {
                    Ok(live) => c.bv_edit = Some(live),
                    Err(e) => tracing::warn!(error = %format!("{e:#}"),
                        "unsaved Blue VO!CE edits dropped"),
                }
            }
        }
        c
    }

    /// Headphone EQ settings for the current selection.
    pub fn hp_settings(&self) -> HpEqSettings {
        hp_settings(&self.hp_preset)
    }

    /// Blue VO!CE parameters for the current selection, `None` without a G
    /// HUB preset file.
    pub fn bv_params(&self) -> Option<anyhow::Result<BvParams>> {
        self.cfg.bv_presets.as_ref()?;
        Some(bv_params(self.bv_live(), self.bv_enabled))
    }

    /// The live Blue VO!CE preset: the selected one with the edits.
    fn bv_live(&self) -> Option<&BlueVoicePreset> {
        let presets = self.cfg.bv_presets.as_ref()?;
        self.bv_edit
            .as_ref()
            .or_else(|| self.bv_selected.map(|i| &presets[i]))
    }

    fn bv_modified(&self) -> bool {
        let presets = self.cfg.bv_presets.as_deref().unwrap_or_default();
        match (&self.bv_edit, self.bv_selected) {
            (Some(e), Some(i)) => e.engine != presets[i].engine,
            _ => false,
        }
    }

    /// The saved sidetone level, if the user ever set one (re-applied on
    /// connect). Invalid saved values are ignored.
    pub fn saved_sidetone(&self) -> Option<SidetoneLevel> {
        self.state
            .sidetone
            .and_then(|l| SidetoneLevel::try_from(l).ok())
    }

    /// The always-on sidetone flags for the PipeWire main loop.
    pub fn keep_open(&self) -> Arc<KeepOpen> {
        self.keep_open.clone()
    }

    /// Connect the headset service (HID++).
    pub fn attach_headset(&mut self, headset: Option<HeadsetService>) {
        self.headset = headset;
    }

    /// Connect the DSP handles; from now on changes are published to them.
    pub fn attach(&mut self, hp: HpEqHandle, bv: Option<BlueVoiceHandle>) {
        self.hp = Some(hp);
        self.bv = bv;
    }

    /// Headphone EQ preset names: flat, gprox's own, `--hp-eq`'s, then the
    /// directory's `*.toml` by name. A file whose name matches an earlier
    /// one (a built-in preset's included) is not listed: built-ins can't be
    /// replaced.
    fn hp_names(&self) -> Vec<String> {
        let mut names = vec![FLAT_PRESET.to_string()];
        names.extend(self.hp_builtin.iter().map(|(n, _)| n.clone()));
        if let Some((n, _)) = &self.cfg.hp_extra
            && !names.iter().any(|x| norm(x) == norm(n))
        {
            names.push(n.clone());
        }
        let mut files: Vec<String> = std::fs::read_dir(&self.cfg.hp_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .filter_map(|p| p.file_stem()?.to_str().map(str::to_owned))
            .collect();
        files.sort_by_key(|n| n.to_lowercase());
        for f in files {
            if !names.iter().any(|n| norm(n) == norm(&f)) {
                names.push(f);
            }
        }
        names
    }

    /// The headphone EQ presets the user may save over and delete: files in
    /// the directory, except those imported from G HUB and one shadowed by
    /// `--hp-eq`.
    fn hp_writable(&self) -> Vec<String> {
        let extra = self.cfg.hp_extra.as_ref().map(|(n, _)| norm(n));
        self.hp_names()
            .into_iter()
            .skip(1)
            .filter(|n| Some(norm(n)) != extra)
            .filter(|n| !gprox_core::builtin::is_builtin_name(n))
            .filter(|n| {
                let path = self.cfg.hp_dir.join(format!("{n}.toml"));
                std::fs::read_to_string(path).is_ok_and(|t| !t.starts_with(IMPORTED_MARK))
            })
            .collect()
    }

    /// The Blue VO!CE presets the user may save over and delete: no G HUB id
    /// and backed by a file in the user's preset directory.
    fn bv_writable(&self) -> Vec<String> {
        self.cfg
            .bv_presets
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|p| {
                p.ghub_id.is_none()
                    && !gprox_core::builtin::is_builtin_name(&p.name)
                    && self.cfg.bv_dir.join(format!("{}.toml", p.name)).is_file()
            })
            .map(|p| p.name.clone())
            .collect()
    }

    /// Find and read a headphone EQ preset. Directory files are re-read on
    /// every selection, so edits apply on the next `set`.
    fn load_hp(&self, name: &str) -> anyhow::Result<(String, HeadphoneEqPreset)> {
        let q = norm(name);
        let Some(found) = self.hp_names().into_iter().find(|n| norm(n) == q) else {
            bail!(
                "no headphone EQ preset {name:?}; available: {} (add <name>.toml files to {})",
                self.hp_names().join(", "),
                self.cfg.hp_dir.display()
            );
        };
        if found == FLAT_PRESET {
            return Ok((found, HeadphoneEqPreset::default()));
        }
        if let Some((_, p)) = self.hp_builtin.iter().find(|(n, _)| *n == found) {
            return Ok((found, p.clone()));
        }
        if let Some((n, p)) = &self.cfg.hp_extra
            && *n == found
        {
            return Ok((found, p.clone()));
        }
        let path = self.cfg.hp_dir.join(format!("{found}.toml"));
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let p = HeadphoneEqPreset::from_toml_str(&text)
            .with_context(|| format!("headphone EQ preset {}", path.display()))?;
        Ok((found, p))
    }

    fn headset_status(&self) -> HeadsetStatus {
        match &self.headset {
            Some(svc) => svc.status(),
            None => HeadsetStatus {
                error: Some("headset control is off (--no-hid)".into()),
                ..Default::default()
            },
        }
    }

    pub fn status(&mut self) -> Status {
        let mut headset = self.headset_status();
        headset.always_on_sidetone = self.state.always_on_sidetone;
        headset.mic_kept_open =
            self.state.always_on_sidetone && self.keep_open.streaming.load(Ordering::Relaxed);
        let presets = self.cfg.bv_presets.as_deref();
        Status {
            headset,
            hp_eq: HpEqStatus {
                selected: self.hp_selected.clone(),
                preset: self.hp_preset.clone(),
                presets: self.hp_names(),
                modified: self.hp_preset != self.hp_saved,
                writable: self.hp_writable(),
            },
            bluevoice: BlueVoiceStatus {
                available: presets.is_some(),
                enabled: self.bv_enabled,
                selected: presets
                    .zip(self.bv_selected)
                    .map(|(p, i)| p[i].name.clone()),
                presets: presets
                    .unwrap_or_default()
                    .iter()
                    .map(|p| p.name.clone())
                    .collect(),
                modified: self.bv_modified(),
                writable: self.bv_writable(),
                params: self
                    .bv_live()
                    .and_then(|p| full_params(p).ok())
                    .unwrap_or_default(),
            },
        }
    }

    fn headset(&self) -> anyhow::Result<&HeadsetService> {
        self.headset
            .as_ref()
            .context("headset control is off (gproxd was started with --no-hid)")
    }

    fn set_sidetone(&mut self, level: u8) -> anyhow::Result<()> {
        let l = SidetoneLevel::try_from(level)?;
        self.headset()?
            .set_sidetone(l)
            .map_err(|e| anyhow::anyhow!("sidetone: {e}"))?;
        tracing::info!(level, "sidetone");
        self.state.sidetone = Some(level);
        Ok(())
    }

    fn set_auto_off(&mut self, minutes: u8) -> anyhow::Result<()> {
        let m = AutoOffMinutes::try_from(minutes)?;
        self.headset()?
            .set_auto_off(m)
            .map_err(|e| anyhow::anyhow!("auto-off: {e}"))?;
        tracing::info!(minutes, "auto-off timer");
        Ok(())
    }

    /// Always-on sidetone: no HID, only the keep-alive stream (gproxd's main
    /// loop picks the change up within a second).
    fn set_always_on_sidetone(&mut self, on: bool) {
        tracing::info!(on, "always-on sidetone");
        self.state.always_on_sidetone = on;
        self.keep_open.wanted.store(on, Ordering::Relaxed);
    }

    /// Write the current selections to the state file (errors are logged).
    pub fn persist(&mut self) {
        self.state.hp_eq = Some(self.hp_selected.clone());
        self.state.hp_eq_edit = (self.hp_preset != self.hp_saved).then(|| self.hp_preset.clone());
        if let Some(presets) = &self.cfg.bv_presets {
            self.state.bluevoice = self.bv_selected.map(|i| presets[i].name.clone());
            self.state.bluevoice_enabled = self.bv_enabled;
            self.state.bluevoice_edit = self
                .bv_modified()
                .then(|| {
                    self.bv_edit
                        .as_ref()
                        .map(|e| e.params().into_iter().collect())
                })
                .flatten();
        }
        if let Err(e) = self.state.save(&self.cfg.state_path) {
            tracing::warn!(error = %e, "could not save state");
        }
    }

    fn publish_bv(&mut self) -> anyhow::Result<()> {
        if let Some(r) = self.bv_params() {
            let p = r?;
            if let Some(h) = &mut self.bv {
                h.set(&p);
            }
        }
        Ok(())
    }

    fn set_hp_eq(&mut self, name: &str) -> anyhow::Result<()> {
        let (n, p) = self.load_hp(name)?;
        tracing::info!(preset = %n, mode = ?p.mode, basic = ?p.basic, advanced = ?p.advanced,
            "headphone EQ preset");
        if let Some(h) = &mut self.hp {
            h.set(&hp_settings(&p));
        }
        (self.hp_selected, self.hp_saved, self.hp_preset) = (n, p.clone(), p);
        Ok(())
    }

    /// Live headphone EQ edit: validated, then ramped to like a preset.
    fn edit_hp_eq(&mut self, p: &HeadphoneEqPreset) -> anyhow::Result<()> {
        p.validate()?;
        tracing::debug!(mode = ?p.mode, basic = ?p.basic, advanced = ?p.advanced,
            "headphone EQ edit");
        if let Some(h) = &mut self.hp {
            h.set(&hp_settings(p));
        }
        self.hp_preset = p.clone();
        Ok(())
    }

    /// Save the live headphone EQ settings as user preset `name`.
    fn save_hp_eq(&mut self, name: &str) -> anyhow::Result<()> {
        let n = preset_file_name(name)?;
        let stem = match self.hp_names().into_iter().find(|x| norm(x) == norm(n)) {
            Some(existing) if self.hp_writable().contains(&existing) => existing,
            Some(existing) => bail!(
                "headphone EQ preset {existing:?} is read-only (flat, built-in, --hp-eq or imported \
                 from G HUB); save under another name"
            ),
            None => n.to_owned(),
        };
        self.hp_preset.validate()?;
        let text = self.hp_preset.to_toml_string()?;
        write_atomic(&self.cfg.hp_dir.join(format!("{stem}.toml")), &text)?;
        tracing::info!(preset = %stem, "headphone EQ preset saved");
        self.hp_saved = self.hp_preset.clone();
        self.hp_selected = stem;
        Ok(())
    }

    /// Delete user headphone EQ preset `name`; flat if it was selected.
    fn delete_hp_eq(&mut self, name: &str) -> anyhow::Result<()> {
        let Some(found) = self.hp_names().into_iter().find(|x| norm(x) == norm(name)) else {
            bail!("no headphone EQ preset {name:?}");
        };
        if !self.hp_writable().contains(&found) {
            bail!(
                "headphone EQ preset {found:?} is read-only (flat, built-in, --hp-eq or imported from G HUB)"
            );
        }
        let path = self.cfg.hp_dir.join(format!("{found}.toml"));
        std::fs::remove_file(&path).with_context(|| format!("delete {}", path.display()))?;
        tracing::info!(preset = %found, "headphone EQ preset deleted");
        if found == self.hp_selected {
            self.set_hp_eq(FLAT_PRESET)?;
        }
        Ok(())
    }

    fn set_bluevoice(&mut self, name: &str) -> anyhow::Result<()> {
        let Some(presets) = &self.cfg.bv_presets else {
            bail!(
                "Blue VO!CE is not available: start gproxd once with --ghub-presets <file>, \
                 or import presets with `gprox import ghub`"
            );
        };
        let i = presets
            .iter()
            .position(|p| bv_matches(p, name))
            .with_context(|| {
                let names: Vec<_> = presets.iter().map(|p| p.name.as_str()).collect();
                format!(
                    "no Blue VO!CE preset {name:?}; available: {}",
                    names.join(", ")
                )
            })?;
        let p = &presets[i];
        // Check before committing: a refused preset is neither kept nor
        // persisted.
        bv_params(Some(p), true).with_context(|| format!("Blue VO!CE preset {:?}", p.name))?;
        tracing::info!(preset = %p.name, enabled = self.bv_enabled, "Blue VO!CE preset");
        self.bv_selected = Some(i);
        self.bv_edit = None;
        self.publish_bv()
    }

    /// Change some values of the live Blue VO!CE preset. Every value is
    /// checked, and the result must build engine parameters, before anything
    /// changes.
    fn edit_bluevoice(&mut self, params: &BTreeMap<String, f32>) -> anyhow::Result<()> {
        let Some(live) = self.bv_live() else {
            bail!("no Blue VO!CE preset selected: select one first");
        };
        let mut live = live.clone();
        for (name, &v) in params {
            check_bv_value(name, v)?;
            let (block, param) = name.split_once('.').context("bad parameter name")?;
            live.engine
                .entry(block.to_owned())
                .or_default()
                .insert(param.to_owned(), v);
        }
        live.validate()?;
        bv_params(Some(&live), true)?;
        tracing::debug!(?params, "Blue VO!CE edit");
        self.bv_edit = Some(live);
        self.publish_bv()
    }

    /// Save the live Blue VO!CE preset as user preset `name` (a new file, or
    /// over a user preset; never over a preset from G HUB) and select it.
    fn save_bluevoice(&mut self, name: &str) -> anyhow::Result<()> {
        let n = preset_file_name(name)?.to_owned();
        let Some(live) = self.bv_live() else {
            bail!("no Blue VO!CE preset selected");
        };
        let writable = self.bv_writable();
        let presets = self.cfg.bv_presets.as_deref().unwrap_or_default();
        let existing = presets.iter().position(|p| bv_matches(p, &n));
        let stem = match existing {
            Some(i) if writable.contains(&presets[i].name) => presets[i].name.clone(),
            Some(i) => bail!(
                "Blue VO!CE preset {:?} is built in or comes from G HUB and is never \
                 overwritten; save a copy under another name",
                presets[i].name
            ),
            None => n,
        };
        let saved = BlueVoicePreset {
            name: stem.clone(),
            ghub_id: None,
            input_gain: None,
            engine: live.engine.clone(),
        };
        saved.validate()?;
        let text = saved.to_toml_string()?;
        write_atomic(&self.cfg.bv_dir.join(format!("{stem}.toml")), &text)?;
        tracing::info!(preset = %stem, "Blue VO!CE preset saved");
        let presets = self.cfg.bv_presets.get_or_insert_with(Vec::new);
        let i = match existing {
            Some(i) => {
                presets[i] = saved;
                i
            }
            None => {
                presets.push(saved);
                presets.len() - 1
            }
        };
        self.bv_selected = Some(i);
        self.bv_edit = None;
        self.publish_bv()
    }

    /// Delete user Blue VO!CE preset `name`. If it was selected, the first
    /// preset is selected (none when it was the last one).
    fn delete_bluevoice(&mut self, name: &str) -> anyhow::Result<()> {
        let presets = self.cfg.bv_presets.as_deref().unwrap_or_default();
        let Some(i) = presets.iter().position(|p| bv_matches(p, name)) else {
            bail!("no Blue VO!CE preset {name:?}");
        };
        let found = presets[i].name.clone();
        if !self.bv_writable().contains(&found) {
            bail!("Blue VO!CE preset {found:?} comes from G HUB and can't be deleted");
        }
        let path = self.cfg.bv_dir.join(format!("{found}.toml"));
        std::fs::remove_file(&path).with_context(|| format!("delete {}", path.display()))?;
        tracing::info!(preset = %found, "Blue VO!CE preset deleted");
        let presets = self.cfg.bv_presets.get_or_insert_with(Vec::new);
        presets.remove(i);
        self.bv_selected = match self.bv_selected {
            Some(s) if s == i => {
                self.bv_edit = None;
                if presets.is_empty() {
                    self.bv_enabled = false;
                    None
                } else {
                    Some(0)
                }
            }
            Some(s) if s > i => Some(s - 1),
            s => s,
        };
        self.publish_bv()
    }

    fn bluevoice_enable(&mut self, on: bool) -> anyhow::Result<()> {
        let Some(presets) = &self.cfg.bv_presets else {
            bail!(
                "Blue VO!CE is not available: start gproxd once with --ghub-presets <file>, \
                 or import presets with `gprox import ghub`"
            );
        };
        let mut selected = self.bv_selected;
        if on && selected.is_none() {
            if presets.is_empty() {
                bail!("no Blue VO!CE presets");
            }
            selected = Some(0);
        }
        bv_params(selected.map(|i| &presets[i]), on)?;
        self.bv_selected = selected;
        self.bv_enabled = on;
        tracing::info!(on, "Blue VO!CE");
        self.publish_bv()
    }

    /// Execute one request. Successful changes are persisted.
    pub fn handle(&mut self, req: &Request) -> Response {
        let r = match req {
            Request::Status => Ok(()),
            Request::SetHpEq { name } => self.set_hp_eq(name),
            Request::SetBluevoice { name } => self.set_bluevoice(name),
            Request::BluevoiceEnable { on } => self.bluevoice_enable(*on),
            Request::SetSidetone { level } => self.set_sidetone(*level),
            Request::SetAutoOff { minutes } => self.set_auto_off(*minutes),
            Request::SetAlwaysOnSidetone { on } => {
                self.set_always_on_sidetone(*on);
                Ok(())
            }
            Request::EditHpEq { preset } => self.edit_hp_eq(preset),
            Request::SaveHpEq { name } => self.save_hp_eq(name),
            Request::DeleteHpEq { name } => self.delete_hp_eq(name),
            Request::EditBluevoice { params } => self.edit_bluevoice(params),
            Request::SaveBluevoice { name } => self.save_bluevoice(name),
            Request::DeleteBluevoice { name } => self.delete_bluevoice(name),
        };
        match r {
            Ok(()) => {
                if *req != Request::Status {
                    self.persist();
                }
                Response::Ok(self.status())
            }
            Err(e) => Response::Error(format!("{e:#}")),
        }
    }
}

/// The listening socket; removes the socket file when dropped.
#[derive(Debug)]
pub struct Server {
    path: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind the control socket at `path` and serve it on a background thread,
/// one thread per connection.
pub fn serve(ctl: Arc<Mutex<Controller>>, path: &Path) -> anyhow::Result<Server> {
    let dir = path.parent().context("socket path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 700 {}", dir.display()))?;
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("another gproxd is already listening on {}", path.display());
        }
        std::fs::remove_file(path)
            .with_context(|| format!("remove stale socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))?;
    std::thread::Builder::new()
        .name("gprox-control".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(s) => {
                        let ctl = ctl.clone();
                        let spawned = std::thread::Builder::new()
                            .name("gprox-client".into())
                            .spawn(move || {
                                if let Err(e) = client(&ctl, s) {
                                    tracing::debug!(error = %e, "control client");
                                }
                            });
                        if let Err(e) = spawned {
                            tracing::warn!(error = %e, "cannot spawn control client thread");
                        }
                    }
                    Err(e) => {
                        // E.g. EMFILE: don't spin on it.
                        tracing::warn!(error = %e, "control socket accept");
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
            }
        })
        .context("spawn control thread")?;
    tracing::info!(socket = %path.display(), "control socket ready");
    Ok(Server {
        path: path.to_owned(),
    })
}

/// Serve one connection: a request line in, a response line out, until EOF.
fn client(ctl: &Mutex<Controller>, s: UnixStream) -> std::io::Result<()> {
    // Idle clients are dropped after a minute.
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(s.try_clone()?);
    let mut writer = s;
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut reader).take(MAX_LINE).read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        let too_long = !line.ends_with('\n');
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => ctl.lock().unwrap_or_else(|e| e.into_inner()).handle(&req),
            Err(e) => Response::Error(format!("bad request: {e}")),
        };
        let mut out = serde_json::to_string(&resp).map_err(std::io::Error::other)?;
        out.push('\n');
        writer.write_all(out.as_bytes())?;
        if too_long {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gprox_core::control::Client;

    /// A complete made-up preset (the engine defaults, not G HUB data),
    /// compressor on or off.
    fn preset(name: &str, comp: bool) -> BlueVoicePreset {
        let d = BvParams::default();
        let params = gprox_dsp::bluevoice::params::NAMES.iter().map(|&n| {
            let id = BvParams::id(n).unwrap();
            let v = match n {
                "comp.enable" => f32::from(u8::from(comp)),
                _ if n.ends_with("enable") => f32::from(u8::from(d.on(id))),
                _ => d.f(id),
            };
            (n, v)
        });
        BlueVoicePreset::from_params(name, params).unwrap()
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gprox-ctl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("hp-eq")).unwrap();
        d
    }

    fn cfg(dir: &Path, bv: bool) -> Config {
        Config {
            hp_dir: dir.join("hp-eq"),
            hp_extra: None,
            bv_presets: bv.then(|| vec![preset("BROADCASTER_1", true), preset("TSM Myth", false)]),
            bv_dir: dir.join("bluevoice"),
            state_path: dir.join("state.toml"),
        }
    }

    fn ok(r: Response) -> Status {
        match r {
            Response::Ok(s) => s,
            Response::Error(e) => panic!("{e}"),
        }
    }

    /// A preset the engine refuses (unknown parameter). Presets read
    /// from files are checked when loaded; this one bypasses that.
    fn refused_preset() -> BlueVoicePreset {
        let mut p = preset("Broken", true);
        p.engine
            .entry("veq".into())
            .or_default()
            .insert("low_q".into(), 1.0);
        p
    }

    #[test]
    fn refused_preset_is_not_kept_or_persisted() {
        let dir = scratch("refused");
        let mut cfg = cfg(&dir, true);
        cfg.bv_presets.as_mut().unwrap().push(refused_preset());
        let mut c = Controller::new(cfg, State::default());
        ok(c.handle(&Request::SetBluevoice {
            name: "tsm myth".into(),
        }));
        ok(c.handle(&Request::BluevoiceEnable { on: true }));
        let r = c.handle(&Request::SetBluevoice {
            name: "broken".into(),
        });
        assert!(matches!(r, Response::Error(_)), "{r:?}");
        let s = c.status();
        assert_eq!(s.bluevoice.selected.as_deref(), Some("TSM Myth"));
        assert!(c.bv_params().unwrap().is_ok());
        // The next successful request persists the old selection.
        ok(c.handle(&Request::SetHpEq {
            name: FLAT_PRESET.into(),
        }));
        let state = State::load(&dir.join("state.toml"));
        assert_eq!(state.bluevoice.as_deref(), Some("TSM Myth"));
        // Enabling with a refused selection leaves Blue VO!CE as it was.
        let mut cfg2 = self::cfg(&dir, true);
        cfg2.bv_presets = Some(vec![refused_preset()]);
        let mut c2 = Controller::new(cfg2, State::default());
        assert!(matches!(
            c2.handle(&Request::BluevoiceEnable { on: true }),
            Response::Error(_)
        ));
        let s = c2.status();
        assert!(!s.bluevoice.enabled && s.bluevoice.selected.is_none());
    }

    /// A saved selection the engine refuses starts with Blue VO!CE off
    /// instead of failing the start (and taking the headphone EQ down).
    #[test]
    fn refused_saved_preset_starts_off() {
        let dir = scratch("refused-start");
        let mut cfg = cfg(&dir, true);
        cfg.bv_presets.as_mut().unwrap().push(refused_preset());
        let state = State {
            bluevoice: Some("Broken".into()),
            bluevoice_enabled: true,
            ..Default::default()
        };
        let mut c = Controller::new(cfg, state);
        let p = c.bv_params().unwrap().unwrap();
        assert!(!p.on(P::Enable));
        assert!(!c.status().bluevoice.enabled);
    }

    #[test]
    fn select_persist_and_restore() {
        let dir = scratch("persist");
        std::fs::write(dir.join("hp-eq/Bass Boost.toml"), "basic = [6.0, 0.0]").unwrap();
        std::fs::write(dir.join("hp-eq/broken.toml"), "basic = [60.0, 0.0]").unwrap();
        let mut c = Controller::new(cfg(&dir, true), State::default());
        let s = c.status();
        assert_eq!(s.hp_eq.selected, FLAT_PRESET);
        assert_eq!(
            s.hp_eq.presets,
            [
                "flat",
                "gprox Movies",
                "gprox FPS",
                "gprox Music",
                "Bass Boost",
                "broken"
            ]
        );
        assert!(s.bluevoice.available && !s.bluevoice.enabled);
        assert_eq!(s.bluevoice.presets, ["BROADCASTER_1", "TSM Myth"]);
        // Off: defaults with micopt.enable 0.
        let off = c.bv_params().unwrap().unwrap();
        assert!(!off.on(P::Enable) && !off.on(P::CompEnable));

        let s = ok(c.handle(&Request::SetHpEq {
            name: "bass_boost".into(),
        }));
        assert_eq!(s.hp_eq.selected, "Bass Boost");
        assert_eq!(s.hp_eq.preset.basic, [6.0, 0.0]);
        assert_eq!(c.hp_settings().basic, [6.0, 0.0]);
        assert!(matches!(
            c.handle(&Request::SetHpEq {
                name: "broken".into()
            }),
            Response::Error(e) if e.contains("±12 dB")
        ));
        assert!(matches!(
            c.handle(&Request::SetHpEq {
                name: "nope".into()
            }),
            Response::Error(_)
        ));
        let s = ok(c.handle(&Request::BluevoiceEnable { on: true }));
        assert_eq!(s.bluevoice.selected.as_deref(), Some("BROADCASTER_1"));
        let on = c.bv_params().unwrap().unwrap();
        assert!(on.on(P::Enable) && on.on(P::CompEnable));
        ok(c.handle(&Request::SetBluevoice {
            name: "tsm myth".into(),
        }));
        assert!(!c.bv_params().unwrap().unwrap().on(P::CompEnable));

        // A new controller starts from the saved state.
        let state = State::load(&dir.join("state.toml"));
        assert_eq!(state.hp_eq.as_deref(), Some("Bass Boost"));
        assert_eq!(state.bluevoice.as_deref(), Some("TSM Myth"));
        assert!(state.bluevoice_enabled);
        let mut c2 = Controller::new(cfg(&dir, true), state);
        assert_eq!(c2.status(), c.status());
        // Headset requests without a headset service are errors.
        assert!(matches!(
            c.handle(&Request::SetSidetone { level: 10 }),
            Response::Error(e) if e.contains("--no-hid")
        ));
        assert!(!c.status().headset.connected);
        // Without a preset file Blue VO!CE is unavailable, the EQ still works.
        let mut c3 = Controller::new(cfg(&dir, false), State::load(&dir.join("state.toml")));
        assert!(c3.bv_params().is_none());
        assert!(!c3.status().bluevoice.available);
        assert!(matches!(
            c3.handle(&Request::BluevoiceEnable { on: true }),
            Response::Error(_)
        ));
        assert_eq!(c3.status().hp_eq.selected, "Bass Boost");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The setting is persisted, restored, reported, and reaches the main
    /// loop's flag; it needs no headset service (no HID).
    /// gprox's own headphone EQ presets are listed and selectable, and a
    /// user file with a built-in's name doesn't replace one.
    #[test]
    fn builtin_hp_presets_cannot_be_replaced() {
        let dir = scratch("builtin");
        std::fs::write(dir.join("hp-eq/gprox_movies.toml"), "basic = [6.0, 0.0]").unwrap();
        let mut c = Controller::new(cfg(&dir, false), State::default());
        let names = c.status().hp_eq.presets;
        assert_eq!(
            names.iter().filter(|n| n.contains("ovies")).count(),
            1,
            "{names:?}"
        );
        let s = ok(c.handle(&Request::SetHpEq {
            name: "gprox movies".into(),
        }));
        assert_eq!(s.hp_eq.selected, "gprox Movies");
        let (_, builtin) = &gprox_core::builtin::hp_eq_presets()[0];
        assert_eq!(&s.hp_eq.preset, builtin);
        // Restored by name after a restart.
        c.persist();
        let c2 = Controller::new(cfg(&dir, false), State::load(&dir.join("state.toml")));
        assert_eq!(c2.hp_settings(), hp_settings(builtin));
    }

    /// Saving under a built-in's name is refused for both kinds, even when a
    /// user file of that exact name exists (it would be hidden behind the
    /// built-in), and the file is left alone.
    #[test]
    fn builtin_names_cannot_be_saved_over() {
        let dir = scratch("builtin-save");
        let hp_file = dir.join("hp-eq/gprox Movies.toml");
        std::fs::write(&hp_file, "basic = [6.0, 0.0]").unwrap();
        let bv_builtin = gprox_core::builtin::bluevoice_presets().remove(0);
        std::fs::create_dir_all(dir.join("bluevoice")).unwrap();
        let bv_file = dir.join(format!("bluevoice/{}.toml", bv_builtin.name));
        std::fs::write(&bv_file, "user file").unwrap();
        let mut config = cfg(&dir, true);
        config
            .bv_presets
            .as_mut()
            .unwrap()
            .insert(0, bv_builtin.clone());
        let mut c = Controller::new(config, State::default());
        let s = c.status();
        assert!(s.hp_eq.writable.is_empty(), "{:?}", s.hp_eq.writable);
        let r = c.handle(&Request::SaveHpEq {
            name: "gprox movies".into(),
        });
        assert!(
            matches!(r, Response::Error(ref e) if e.contains("built-in")),
            "{r:?}"
        );
        ok(c.handle(&Request::SetBluevoice {
            name: bv_builtin.name.clone(),
        }));
        let r = c.handle(&Request::SaveBluevoice {
            name: bv_builtin.name.clone(),
        });
        assert!(
            matches!(r, Response::Error(ref e) if e.contains("built in")),
            "{r:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&hp_file).unwrap(),
            "basic = [6.0, 0.0]"
        );
        assert_eq!(std::fs::read_to_string(&bv_file).unwrap(), "user file");
    }

    #[test]
    fn always_on_sidetone_persists_without_hid() {
        let dir = scratch("always-on");
        let mut c = Controller::new(cfg(&dir, false), State::default());
        let flags = c.keep_open();
        assert!(!flags.wanted.load(Ordering::Relaxed));
        let s = c.status();
        assert!(!s.headset.always_on_sidetone && !s.headset.mic_kept_open);

        let s = ok(c.handle(&Request::SetAlwaysOnSidetone { on: true }));
        assert!(s.headset.always_on_sidetone);
        assert!(!s.headset.mic_kept_open, "not streaming yet");
        assert!(flags.wanted.load(Ordering::Relaxed));
        flags.streaming.store(true, Ordering::Relaxed);
        assert!(c.status().headset.mic_kept_open);
        assert!(State::load(&dir.join("state.toml")).always_on_sidetone);

        // A new controller starts with it on.
        let mut c2 = Controller::new(cfg(&dir, false), State::load(&dir.join("state.toml")));
        assert!(c2.keep_open().wanted.load(Ordering::Relaxed));
        assert!(c2.status().headset.always_on_sidetone);

        let s = ok(c.handle(&Request::SetAlwaysOnSidetone { on: false }));
        assert!(!s.headset.always_on_sidetone);
        assert!(
            !s.headset.mic_kept_open,
            "a stream still closing is not reported"
        );
        assert!(!flags.wanted.load(Ordering::Relaxed));
        assert!(!State::load(&dir.join("state.toml")).always_on_sidetone);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn socket_round_trip() {
        let dir = scratch("socket");
        let sock = dir.join("run").join("control.sock");
        let ctl = Arc::new(Mutex::new(Controller::new(
            cfg(&dir, true),
            State::default(),
        )));
        let server = serve(ctl, &sock).unwrap();
        // A second daemon must not steal the socket.
        let other = Arc::new(Mutex::new(Controller::new(
            cfg(&dir, true),
            State::default(),
        )));
        assert!(serve(other, &sock).is_err());
        let mode = std::fs::metadata(sock.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        let mut cl = Client::connect_to(&sock).unwrap();
        assert_eq!(cl.call(&Request::Status).unwrap().hp_eq.selected, "flat");
        let s = cl.call(&Request::BluevoiceEnable { on: true }).unwrap();
        assert!(s.bluevoice.enabled);
        let e = cl
            .call(&Request::SetBluevoice { name: "x".into() })
            .unwrap_err();
        assert!(e.to_string().contains("no Blue VO!CE preset"), "{e}");
        // Garbage gets an error line, and the connection stays usable.
        let mut raw = UnixStream::connect(&sock).unwrap();
        raw.write_all(b"{\"cmd\":\"dance\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(raw.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("{\"error\":\"bad request"), "{line}");
        drop(server);
        assert!(!sock.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    // ---- Editing ----

    fn adv(gains: [f32; 10]) -> HeadphoneEqPreset {
        HeadphoneEqPreset {
            mode: gprox_core::EqMode::Advanced,
            basic: [1.5, -2.25],
            advanced: gains,
        }
    }

    fn eq_bits(p: &HeadphoneEqPreset) -> Vec<u32> {
        p.basic
            .iter()
            .chain(&p.advanced)
            .map(|g| g.to_bits())
            .collect()
    }

    fn edit_eq(c: &mut Controller, p: &HeadphoneEqPreset) -> Response {
        c.handle(&Request::EditHpEq { preset: p.clone() })
    }

    fn edit(c: &mut Controller, params: &[(&str, f32)]) -> Response {
        c.handle(&Request::EditBluevoice {
            params: params.iter().map(|&(k, v)| (k.to_owned(), v)).collect(),
        })
    }

    fn save_bv(c: &mut Controller, name: &str) -> Response {
        c.handle(&Request::SaveBluevoice { name: name.into() })
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// Both presets as imported from G HUB (they have an id).
    fn cfg_ghub(dir: &Path) -> Config {
        let mut c = cfg(dir, true);
        for (i, p) in c.bv_presets.as_mut().unwrap().iter_mut().enumerate() {
            p.ghub_id = Some(format!("00000000-0000-0000-0000-00000000000{i}"));
        }
        c
    }

    fn bits(m: &BTreeMap<String, f32>) -> Vec<(String, u32)> {
        m.iter().map(|(k, v)| (k.clone(), v.to_bits())).collect()
    }

    /// A controller with BROADCASTER_1 selected and Blue VO!CE on.
    fn bv_controller(dir: &Path, ghub: bool) -> Controller {
        let cfg = if ghub { cfg_ghub(dir) } else { cfg(dir, true) };
        let mut c = Controller::new(cfg, State::default());
        ok(c.handle(&Request::SetBluevoice {
            name: "broadcaster 1".into(),
        }));
        ok(c.handle(&Request::BluevoiceEnable { on: true }));
        c
    }

    #[test]
    fn hp_edit_reaches_settings_and_validates() {
        let dir = scratch("hp-edit");
        let mut c = Controller::new(cfg(&dir, false), State::default());
        let e = adv([0.0, 0.1, 0.2, 5.3, 0.0, -6.7, 0.0, 0.0, 12.0, -12.0]);
        let s = ok(edit_eq(&mut c, &e));
        assert_eq!(s.hp_eq.preset, e);
        assert_eq!(s.hp_eq.selected, FLAT_PRESET);
        assert!(s.hp_eq.modified);
        let live = c.hp_settings();
        assert_eq!(live.advanced, e.advanced);
        assert_eq!(live.basic, e.basic);
        assert!(matches!(live.mode, EqMode::Advanced));
        assert!(State::load(&dir.join("state.toml")).hp_eq_edit.is_some());

        let before = c.status();
        for bad in [12.5, -12.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for in_basic in [false, true] {
                let mut p = e.clone();
                if in_basic {
                    p.basic[1] = bad;
                } else {
                    p.advanced[4] = bad;
                }
                let r = edit_eq(&mut c, &p);
                assert!(matches!(r, Response::Error(_)), "{bad}: {r:?}");
                assert_eq!(c.status(), before, "{bad}");
                assert_eq!(c.hp_settings().advanced, e.advanced);
            }
        }
        // Back to the stored values: no longer modified, nothing persisted.
        let s = ok(edit_eq(&mut c, &HeadphoneEqPreset::default()));
        assert!(!s.hp_eq.modified);
        assert!(State::load(&dir.join("state.toml")).hp_eq_edit.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hp_save_overwrite_and_delete() {
        let dir = scratch("hp-save");
        let imported = "# Imported from G HUB\nbasic = [1.0, 0.0]\n";
        std::fs::write(dir.join("hp-eq/Imported.toml"), imported).unwrap();
        let mut c = Controller::new(cfg(&dir, false), State::default());
        let e = adv([0.5, 0.25, 1.1, 3.3, 0.0, -6.7, 0.0, 0.0, 12.0, -12.0]);
        ok(edit_eq(&mut c, &e));
        let s = ok(c.handle(&Request::SaveHpEq {
            name: " My EQ ".into(),
        }));
        assert_eq!(s.hp_eq.selected, "My EQ");
        assert!(!s.hp_eq.modified);
        assert_eq!(s.hp_eq.writable, ["My EQ"]);
        assert!(s.hp_eq.presets.contains(&"My EQ".to_owned()));
        let file = dir.join("hp-eq/My EQ.toml");
        let saved =
            HeadphoneEqPreset::from_toml_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved, e);
        assert_eq!(eq_bits(&saved), eq_bits(&e));
        assert_eq!(
            State::load(&dir.join("state.toml")).hp_eq.as_deref(),
            Some("My EQ")
        );
        assert!(
            files(&dir.join("hp-eq"))
                .iter()
                .all(|f| !f.starts_with('.'))
        );

        // Another case of the name overwrites the same file.
        let e2 = adv([1.0; 10]);
        ok(edit_eq(&mut c, &e2));
        let s = ok(c.handle(&Request::SaveHpEq {
            name: "my eq".into(),
        }));
        assert_eq!(s.hp_eq.selected, "My EQ");
        assert_eq!(files(&dir.join("hp-eq")), ["Imported.toml", "My EQ.toml"]);
        let saved =
            HeadphoneEqPreset::from_toml_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved, e2);

        // A new controller from the saved state selects it unchanged.
        let mut c2 = Controller::new(cfg(&dir, false), State::load(&dir.join("state.toml")));
        assert_eq!(c2.status(), c.status());
        assert_eq!(eq_bits(&c2.status().hp_eq.preset), eq_bits(&e2));

        // Read-only presets and bad names.
        for name in ["flat", "FLAT", "Imported", "imported"] {
            let r = c.handle(&Request::SaveHpEq { name: name.into() });
            assert!(matches!(r, Response::Error(_)), "save {name}: {r:?}");
            let r = c.handle(&Request::DeleteHpEq { name: name.into() });
            assert!(matches!(r, Response::Error(_)), "delete {name}: {r:?}");
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("hp-eq/Imported.toml")).unwrap(),
            imported
        );
        assert_eq!(c.status().hp_eq.writable, ["My EQ"]);
        for name in ["", "  ", "a/b", "a\\b", ".x", "a\nb", &"x".repeat(65)] {
            let r = c.handle(&Request::SaveHpEq { name: name.into() });
            assert!(matches!(r, Response::Error(_)), "{name:?}: {r:?}");
        }
        assert_eq!(files(&dir.join("hp-eq")), ["Imported.toml", "My EQ.toml"]);
        assert!(matches!(
            c.handle(&Request::SaveHpEq {
                name: "x".repeat(64)
            }),
            Response::Ok(_)
        ));
        ok(c.handle(&Request::DeleteHpEq {
            name: "x".repeat(64),
        }));

        // Delete: the selected preset goes, flat is selected.
        let s = ok(c.handle(&Request::SaveHpEq {
            name: "My EQ".into(),
        }));
        assert_eq!(s.hp_eq.selected, "My EQ");
        let s = ok(c.handle(&Request::DeleteHpEq {
            name: "my_eq".into(),
        }));
        assert!(!file.exists());
        assert_eq!(s.hp_eq.selected, FLAT_PRESET);
        assert!(!s.hp_eq.modified && s.hp_eq.writable.is_empty());
        assert_eq!(c.hp_settings().advanced, [0.0; 10]);
        let r = c.handle(&Request::DeleteHpEq {
            name: "nope".into(),
        });
        assert!(matches!(r, Response::Error(_)), "{r:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bv_edit_validation_changes_nothing() {
        let dir = scratch("bv-validate");
        let mut c = bv_controller(&dir, false);
        let before_p = c.bv_params().unwrap().unwrap();
        let before = c.status();
        let bad: [&[(&str, f32)]; 8] = [
            &[("veq.low_gain", 100.0)],
            &[("veq.low_gain", f32::NAN)],
            &[("veq.low_gain", f32::INFINITY)],
            &[("veq.nope", 1.0)],
            &[("nope", 1.0)],
            &[("micopt.enable", 0.0)],
            &[("comp.enable", 0.5)],
            // One good value and one bad one: nothing is applied.
            &[("exp.thresh", -60.0), ("veq.low_gain", 100.0)],
        ];
        for params in bad {
            let r = edit(&mut c, params);
            assert!(matches!(r, Response::Error(_)), "{params:?}: {r:?}");
            assert_eq!(c.bv_params().unwrap().unwrap(), before_p, "{params:?}");
            assert_eq!(c.status(), before, "{params:?}");
        }
        let Response::Error(e) = edit(&mut c, &[("veq.low_gain", 100.0)]) else {
            panic!()
        };
        assert!(e.contains("veq.low_gain"), "{e}");
        assert!(
            !dir.join("state.toml").exists() || {
                State::load(&dir.join("state.toml"))
                    .bluevoice_edit
                    .is_none()
            }
        );

        // A good edit.
        let s = ok(edit(
            &mut c,
            &[
                ("veq.low_gain", 1.5),
                ("comp.enable", 0.0),
                ("exp.thresh", -60.0),
            ],
        ));
        let p = c.bv_params().unwrap().unwrap();
        assert!(p.on(P::Enable));
        assert_eq!(p.f(P::VeqLowGain), 1.5);
        assert!(!p.on(P::CompEnable));
        assert_eq!(p.f(P::ExpThresh), -60.0);
        assert_eq!(p.f(P::VeqMidGain), before_p.f(P::VeqMidGain));
        assert!(s.bluevoice.modified);
        assert_eq!(s.bluevoice.selected.as_deref(), Some("BROADCASTER_1"));
        assert_eq!(s.bluevoice.params.len(), 44);
        for n in gprox_dsp::bluevoice::params::NAMES {
            assert!(s.bluevoice.params.contains_key(n), "{n}");
        }
        assert_eq!(s.bluevoice.params["veq.low_gain"], 1.5);
        assert_eq!(s.bluevoice.params["comp.enable"], 0.0);
        assert_eq!(s.bluevoice.params["exp.thresh"], -60.0);
        // The selected preset itself is untouched.
        assert!(before.bluevoice.params["comp.enable"] == 1.0);

        // No preset selected, or no presets: an error.
        let mut c2 = Controller::new(cfg(&dir, true), State::default());
        let r = edit(&mut c2, &[("veq.low_gain", 1.5)]);
        assert!(matches!(r, Response::Error(_)), "{r:?}");
        let mut c3 = Controller::new(cfg(&dir, false), State::default());
        let r = edit(&mut c3, &[("veq.low_gain", 1.5)]);
        assert!(matches!(r, Response::Error(_)), "{r:?}");
        assert!(c2.status().bluevoice.params.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ghub_presets_are_never_overwritten() {
        let dir = scratch("bv-ghub");
        let mut c = bv_controller(&dir, true);
        let p0 = c.bv_params().unwrap().unwrap();
        let params0 = c.status().bluevoice.params;
        ok(edit(&mut c, &[("veq.low_gain", 2.0), ("des.thresh", 3.0)]));
        let edited = c.bv_params().unwrap().unwrap();
        assert_ne!(edited, p0);
        for name in [
            "broadcaster 1",
            "BROADCASTER_1",
            "BLUE_VOICE_PRESET_BROADCASTER_1",
        ] {
            let r = save_bv(&mut c, name);
            assert!(matches!(r, Response::Error(_)), "{name}: {r:?}");
            let r = c.handle(&Request::DeleteBluevoice { name: name.into() });
            assert!(matches!(r, Response::Error(_)), "delete {name}: {r:?}");
        }
        assert!(!dir.join("bluevoice").exists() || files(&dir.join("bluevoice")).is_empty());
        let s = c.status();
        assert!(s.bluevoice.modified && s.bluevoice.writable.is_empty());
        assert_eq!(c.bv_params().unwrap().unwrap(), edited);

        // Save as a new name.
        let live = c.status().bluevoice.params;
        let s = ok(save_bv(&mut c, "Mine"));
        let file = dir.join("bluevoice/Mine.toml");
        assert_eq!(files(&dir.join("bluevoice")), ["Mine.toml"]);
        assert_eq!(s.bluevoice.selected.as_deref(), Some("Mine"));
        assert!(!s.bluevoice.modified);
        assert_eq!(s.bluevoice.writable, ["Mine"]);
        assert_eq!(s.bluevoice.presets, ["BROADCASTER_1", "TSM Myth", "Mine"]);
        assert_eq!(bits(&s.bluevoice.params), bits(&live));
        assert_eq!(c.bv_params().unwrap().unwrap(), edited);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("ghub_id"), "{text}");
        let loaded = BlueVoicePreset::from_toml_str(&text).unwrap();
        assert_eq!(loaded.name, "Mine");
        assert!(loaded.ghub_id.is_none());
        assert_eq!(bits(&full_params(&loaded).unwrap()), bits(&live));
        let st = State::load(&dir.join("state.toml"));
        assert_eq!(st.bluevoice.as_deref(), Some("Mine"));
        assert!(st.bluevoice_edit.is_none());

        // The G HUB preset in memory is unchanged.
        let s = ok(c.handle(&Request::SetBluevoice {
            name: "BROADCASTER_1".into(),
        }));
        assert_eq!(c.bv_params().unwrap().unwrap(), p0);
        assert_eq!(bits(&s.bluevoice.params), bits(&params0));
        assert!(!s.bluevoice.modified);

        // Saving over the user preset (any case) overwrites its file.
        ok(c.handle(&Request::SetBluevoice {
            name: "mine".into(),
        }));
        ok(edit(&mut c, &[("comp.enable", 0.0), ("lim.boost", 2.0)]));
        let live2 = c.status().bluevoice.params;
        let s = ok(save_bv(&mut c, "mine"));
        assert_eq!(files(&dir.join("bluevoice")), ["Mine.toml"]);
        assert_eq!(s.bluevoice.presets, ["BROADCASTER_1", "TSM Myth", "Mine"]);
        assert_eq!(s.bluevoice.selected.as_deref(), Some("Mine"));
        let loaded =
            BlueVoicePreset::from_toml_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(bits(&full_params(&loaded).unwrap()), bits(&live2));
        assert_ne!(bits(&live2), bits(&live));
        // Bad names.
        ok(edit(&mut c, &[("comp.enable", 1.0)]));
        for name in ["", "a/b", ".x", &"x".repeat(65)] {
            let r = save_bv(&mut c, name);
            assert!(matches!(r, Response::Error(_)), "{name:?}: {r:?}");
        }
        assert_eq!(files(&dir.join("bluevoice")), ["Mine.toml"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saved_presets_load_back_unchanged() {
        let dir = scratch("bv-reload");
        let mut c = bv_controller(&dir, true);
        ok(edit(
            &mut c,
            &[
                ("veq.low_gain", 1.234_567_9),
                ("exp.thresh", -61.3),
                ("micopt.volume", 0.7),
            ],
        ));
        let e = adv([0.5, 0.25, 1.1, 3.3, 0.0, -6.7, 0.0, 0.0, 12.0, -12.0]);
        ok(edit_eq(&mut c, &e));
        ok(save_bv(&mut c, "Mine"));
        ok(c.handle(&Request::SaveHpEq {
            name: "My EQ".into(),
        }));
        let loaded = BlueVoicePreset::from_toml_str(
            &std::fs::read_to_string(dir.join("bluevoice/Mine.toml")).unwrap(),
        )
        .unwrap();
        let mut cfg2 = cfg_ghub(&dir);
        cfg2.bv_presets.as_mut().unwrap().push(loaded);
        let mut c2 = Controller::new(cfg2, State::load(&dir.join("state.toml")));
        let (s1, s2) = (c.status(), c2.status());
        assert_eq!(s1, s2);
        assert_eq!(bits(&s1.bluevoice.params), bits(&s2.bluevoice.params));
        assert_eq!(
            s2.bluevoice.params["veq.low_gain"].to_bits(),
            1.234_567_9_f32.to_bits()
        );
        assert_eq!(
            c.bv_params().unwrap().unwrap(),
            c2.bv_params().unwrap().unwrap()
        );
        assert_eq!(s2.hp_eq.selected, "My EQ");
        assert_eq!(eq_bits(&s2.hp_eq.preset), eq_bits(&e));
        assert_eq!(c2.hp_settings().advanced, e.advanced);
        assert_eq!(c2.hp_settings().basic, e.basic);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unsaved_edits_survive_restart_and_revert_clears_them() {
        let dir = scratch("edits-restart");
        let mut c = bv_controller(&dir, false);
        let e = adv([0.5, 0.25, 1.1, 3.3, 0.0, -6.7, 0.0, 0.0, 12.0, -12.0]);
        ok(edit_eq(&mut c, &e));
        ok(edit(&mut c, &[("veq.low_gain", 1.5), ("comp.enable", 0.0)]));
        let st_path = dir.join("state.toml");
        let st = State::load(&st_path);
        assert_eq!(st.hp_eq_edit.as_ref(), Some(&e));
        assert!(st.bluevoice_edit.is_some());

        let mut c2 = Controller::new(cfg(&dir, true), st);
        assert_eq!(c2.hp_settings().advanced, e.advanced);
        assert_eq!(c2.hp_settings().basic, e.basic);
        assert_eq!(
            c.bv_params().unwrap().unwrap(),
            c2.bv_params().unwrap().unwrap()
        );
        let s = c2.status();
        assert!(s.hp_eq.modified && s.bluevoice.modified);
        assert_eq!(s, c.status());

        // Reverting drops the edits, and the state file says so.
        ok(c.handle(&Request::SetHpEq {
            name: FLAT_PRESET.into(),
        }));
        let st = State::load(&st_path);
        assert!(st.hp_eq_edit.is_none() && st.bluevoice_edit.is_some());
        let s = ok(c.handle(&Request::SetBluevoice {
            name: "BROADCASTER_1".into(),
        }));
        assert!(!s.bluevoice.modified && !s.hp_eq.modified);
        let st = State::load(&st_path);
        assert!(st.hp_eq_edit.is_none() && st.bluevoice_edit.is_none());
        assert!(c.bv_params().unwrap().unwrap().on(P::CompEnable));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_persisted_edits_are_dropped() {
        let dir = scratch("edits-invalid");
        let bad_maps: [&[(&str, f32)]; 3] = [
            &[("veq.low_gain", 100.0)],
            &[("veq.nope", 1.0)],
            &[("exp.thresh", f32::NAN)],
        ];
        for m in bad_maps {
            let state = State {
                bluevoice: Some("BROADCASTER_1".into()),
                bluevoice_enabled: true,
                bluevoice_edit: Some(m.iter().map(|&(k, v)| (k.to_owned(), v)).collect()),
                hp_eq_edit: Some(adv([20.0; 10])),
                ..Default::default()
            };
            let mut c = Controller::new(cfg(&dir, true), state);
            let s = c.status();
            assert!(!s.bluevoice.modified && !s.hp_eq.modified, "{m:?}");
            assert_eq!(s.hp_eq.preset, HeadphoneEqPreset::default());
            assert_eq!(s.bluevoice.selected.as_deref(), Some("BROADCASTER_1"));
            assert_eq!(s.bluevoice.params["veq.low_gain"], 1.0);
            assert!(c.bv_params().unwrap().unwrap().on(P::CompEnable));
            // The next change no longer persists the dropped edits.
            ok(c.handle(&Request::Status));
            ok(c.handle(&Request::SetHpEq {
                name: FLAT_PRESET.into(),
            }));
            let st = State::load(&dir.join("state.toml"));
            assert!(st.hp_eq_edit.is_none() && st.bluevoice_edit.is_none());
        }
        // An edit for a selection that is gone is dropped too.
        let state = State {
            bluevoice: Some("Gone".into()),
            bluevoice_enabled: false,
            bluevoice_edit: Some([("veq.low_gain".to_owned(), 1.5)].into_iter().collect()),
            ..Default::default()
        };
        let mut c = Controller::new(cfg(&dir, true), state);
        assert!(!c.status().bluevoice.modified);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn deleting_bluevoice_presets_keeps_or_resets_selection() {
        let dir = scratch("bv-delete");
        let mut c = bv_controller(&dir, true);
        ok(edit(&mut c, &[("veq.low_gain", 2.0)]));
        ok(save_bv(&mut c, "Mine A"));
        ok(c.handle(&Request::SetBluevoice {
            name: "tsm myth".into(),
        }));
        ok(edit(&mut c, &[("veq.low_gain", 3.0)]));
        let s = ok(save_bv(&mut c, "Mine B"));
        assert_eq!(
            s.bluevoice.presets,
            ["BROADCASTER_1", "TSM Myth", "Mine A", "Mine B"]
        );
        assert_eq!(s.bluevoice.writable, ["Mine A", "Mine B"]);
        let b = c.bv_params().unwrap().unwrap();
        assert_eq!(b.f(P::VeqLowGain), 3.0);

        // Deleting one before the selection keeps the selection by name.
        let s = ok(c.handle(&Request::DeleteBluevoice {
            name: "mine a".into(),
        }));
        assert_eq!(s.bluevoice.selected.as_deref(), Some("Mine B"));
        assert_eq!(s.bluevoice.presets, ["BROADCASTER_1", "TSM Myth", "Mine B"]);
        assert_eq!(files(&dir.join("bluevoice")), ["Mine B.toml"]);
        assert_eq!(c.bv_params().unwrap().unwrap(), b);
        let st = State::load(&dir.join("state.toml"));
        assert_eq!(st.bluevoice.as_deref(), Some("Mine B"));
        // Deleting one after the selection keeps it too.
        ok(c.handle(&Request::SetBluevoice {
            name: "broadcaster 1".into(),
        }));
        ok(c.handle(&Request::SetBluevoice {
            name: "Mine B".into(),
        }));
        ok(save_bv(&mut c, "Mine C"));
        ok(c.handle(&Request::SetBluevoice {
            name: "Mine B".into(),
        }));
        let s = ok(c.handle(&Request::DeleteBluevoice {
            name: "Mine C".into(),
        }));
        assert_eq!(s.bluevoice.selected.as_deref(), Some("Mine B"));

        // Deleting the selected user preset (with unsaved edits) selects the
        // first preset and drops the edits.
        ok(edit(&mut c, &[("veq.low_gain", 4.0)]));
        assert!(c.status().bluevoice.modified);
        let s = ok(c.handle(&Request::DeleteBluevoice {
            name: "Mine B".into(),
        }));
        assert_eq!(s.bluevoice.selected.as_deref(), Some("BROADCASTER_1"));
        assert!(!s.bluevoice.modified && s.bluevoice.writable.is_empty());
        assert_eq!(s.bluevoice.presets, ["BROADCASTER_1", "TSM Myth"]);
        assert!(files(&dir.join("bluevoice")).is_empty());
        assert_eq!(c.bv_params().unwrap().unwrap().f(P::VeqLowGain), 1.0);
        assert!(s.bluevoice.enabled);
        let st = State::load(&dir.join("state.toml"));
        assert_eq!(st.bluevoice.as_deref(), Some("BROADCASTER_1"));
        assert!(st.bluevoice_edit.is_none());
        let r = c.handle(&Request::DeleteBluevoice {
            name: "nope".into(),
        });
        assert!(matches!(r, Response::Error(_)), "{r:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn socket_editing_round_trip() {
        let dir = scratch("socket-edit");
        let sock = dir.join("run").join("control.sock");
        let ctl = Arc::new(Mutex::new(Controller::new(
            cfg(&dir, true),
            State::default(),
        )));
        let server = serve(ctl, &sock).unwrap();
        let mut cl = Client::connect_to(&sock).unwrap();

        let e = adv([0.0, 0.0, 0.0, 0.0, 0.0, 6.0, 0.0, 0.0, 0.0, 0.0]);
        let s = cl.call(&Request::EditHpEq { preset: e.clone() }).unwrap();
        assert!(s.hp_eq.modified);
        assert_eq!(s.hp_eq.preset, e);
        let mut bad = e.clone();
        bad.advanced[0] = 12.5;
        assert!(cl.call(&Request::EditHpEq { preset: bad }).is_err());
        let s = cl
            .call(&Request::SaveHpEq {
                name: "Sock EQ".into(),
            })
            .unwrap();
        assert_eq!(s.hp_eq.selected, "Sock EQ");
        assert!(!s.hp_eq.modified);
        assert_eq!(s.hp_eq.writable, ["Sock EQ"]);
        assert!(dir.join("hp-eq/Sock EQ.toml").is_file());

        cl.call(&Request::BluevoiceEnable { on: true }).unwrap();
        let params = |k: &str, v: f32| Request::EditBluevoice {
            params: [(k.to_owned(), v)].into_iter().collect(),
        };
        let err = cl.call(&params("veq.low_gain", 100.0)).unwrap_err();
        assert!(err.to_string().contains("veq.low_gain"), "{err}");
        let s = cl.call(&params("veq.low_gain", 1.5)).unwrap();
        assert!(s.bluevoice.modified);
        assert_eq!(s.bluevoice.params["veq.low_gain"], 1.5);
        let s = cl
            .call(&Request::SaveBluevoice {
                name: "Sock VO".into(),
            })
            .unwrap();
        assert_eq!(s.bluevoice.selected.as_deref(), Some("Sock VO"));
        assert_eq!(s.bluevoice.writable, ["Sock VO"]);
        assert!(!s.bluevoice.modified);
        assert!(dir.join("bluevoice/Sock VO.toml").is_file());
        let err = cl
            .call(&Request::SaveBluevoice {
                name: "tsm myth".into(),
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("G HUB") || err.to_string().contains("never"),
            "{err}"
        );
        let s = cl
            .call(&Request::DeleteBluevoice {
                name: "Sock VO".into(),
            })
            .unwrap();
        assert_eq!(s.bluevoice.selected.as_deref(), Some("BROADCASTER_1"));
        assert!(s.bluevoice.writable.is_empty());
        assert!(!dir.join("bluevoice/Sock VO.toml").exists());
        let s = cl
            .call(&Request::DeleteHpEq {
                name: "Sock EQ".into(),
            })
            .unwrap();
        assert_eq!(s.hp_eq.selected, "flat");
        drop(server);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
