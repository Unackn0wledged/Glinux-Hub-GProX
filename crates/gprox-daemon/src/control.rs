//! Runtime control: preset selection and the control socket.
//!
//! [`Controller`] owns the control-side DSP handles ([`HpEqHandle`],
//! [`BlueVoiceHandle`]) and the persisted [`State`]. It lives behind a mutex
//! that only the socket threads take; the PipeWire data thread never sees it.
//! Changes reach the DSP only through the handles' lock-free cells and take
//! effect at the next DSP block (headphone EQ: a 100 ms gain ramp, like G HUB;
//! Blue VO!CE: the next 256-frame engine block with its parameter smoothing).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, bail};
use gprox_core::BlueVoicePreset;
use gprox_core::HeadphoneEqPreset;
use gprox_core::control::{
    BlueVoiceStatus, FLAT_PRESET, HeadsetStatus, HpEqStatus, MAX_LINE, Request, Response, Status,
};
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
    hp_preset: HeadphoneEqPreset,
    /// Index into `cfg.bv_presets`.
    bv_selected: Option<usize>,
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
            bv_selected: None,
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
                Ok((n, p)) => (c.hp_selected, c.hp_preset) = (n, p),
                Err(e) => tracing::warn!(error = %e, "saved headphone EQ preset not usable; flat"),
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
        let presets = self.cfg.bv_presets.as_ref()?;
        let preset = self.bv_selected.map(|i| &presets[i]);
        Some(bv_params(preset, self.bv_enabled))
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

    /// Headphone EQ preset names: flat, `--hp-eq`'s, then the directory's
    /// `*.toml` by name.
    fn hp_names(&self) -> Vec<String> {
        let mut names = vec![FLAT_PRESET.to_string()];
        if let Some((n, _)) = &self.cfg.hp_extra {
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
        if let Some(presets) = &self.cfg.bv_presets {
            self.state.bluevoice = self.bv_selected.map(|i| presets[i].name.clone());
            self.state.bluevoice_enabled = self.bv_enabled;
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
        (self.hp_selected, self.hp_preset) = (n, p);
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
        assert_eq!(s.hp_eq.presets, ["flat", "Bass Boost", "broken"]);
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
}
