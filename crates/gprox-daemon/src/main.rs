//! gproxd: creates the gprox virtual sink and source and runs their DSP.
//!
//! Render path: G HUB's headphone EQ, always running; the "flat" preset
//! is all 0 dB. Capture path: the Blue VO!CE chain when a G HUB
//! preset file is known (`--ghub-presets`, remembered), otherwise a bit-exact
//! pass-through. Presets are switched at runtime over the control socket
//! ([`control`], used by `gprox` and `gprox-gui`); the last selection is
//! persisted in `<config dir>/state.toml` and restored at start.
//!
//! Headset controls: a [`gprox_hid::HeadsetService`] thread talks HID++
//! to the dongle (sidetone, auto-off, battery, hotplug, mute-button reports);
//! the capture path reports when the raw mic last had audio and when its
//! stream started, which the mute tracking needs (`gprox_hid::mute`).

mod control;
mod state;

use std::cell::Cell;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use clap::Parser;
use gprox_core::control::{Request, Response};
use gprox_core::{BlueVoicePreset, HeadphoneEqPreset};
use gprox_dsp::bluevoice::BlueVoice;
use gprox_dsp::silence::{SilenceMeter, silence_meter};
use gprox_dsp::{HeadphoneEq, Passthrough, Processor};
use gprox_pw::{Direction, Duplex, DuplexSpec, StatsSnapshot, position};
use pipewire as pw;
use pw::loop_::Signal;

use control::{Config, Controller};
use state::State;

/// Node names of the PRO X Wireless on the host (from `pw-dump`, 2026-11-20).
const HEADSET_SINK: &str =
    "alsa_output.usb-Logitech_PRO_X_Wireless_Gaming_Headset-00.analog-stereo";
const HEADSET_SOURCE: &str =
    "alsa_input.usb-Logitech_PRO_X_Wireless_Gaming_Headset-00.mono-fallback";

/// `priority.session` of the gprox sink and source. WirePlumber makes the
/// available node with the highest priority the default device unless the
/// user picked one (its stored choices win), so while gproxd runs the gprox
/// nodes become the defaults, and when it stops WirePlumber falls back to the
/// next one (the headset itself). ALSA nodes get at most about 2100 here (the
/// PRO X mic has 2100, `pw-dump` 2026-11-22); 3000 is above any of them and
/// far below WirePlumber's thresholds for stored choices (15000+).
const NODE_PRIORITY: u32 = 3000;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// node.name of the headset sink the render path plays into.
    #[arg(long, default_value = HEADSET_SINK)]
    sink_target: String,
    /// node.name of the headset source the capture path records from.
    #[arg(long, default_value = HEADSET_SOURCE)]
    source_target: String,
    /// Requested quantum in frames.
    #[arg(long, default_value_t = 256)]
    quantum: u32,
    /// Processing rate in Hz.
    #[arg(long, default_value_t = 48_000)]
    rate: u32,
    /// Seconds between stats log lines (0 = off).
    #[arg(long, default_value_t = 10)]
    stats_interval: u64,
    /// Exit after this many seconds (0 = run until signalled). For tests.
    #[arg(long, default_value_t = 0)]
    run_for: u64,
    /// Config directory: `state.toml` (last selections), `hp-eq/*.toml`
    /// (headphone EQ presets) and `bluevoice/*.toml` (Blue VO!CE presets, e.g.
    /// from `gprox import ghub`). Default: $XDG_CONFIG_HOME/gprox.
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,
    /// Control socket. Default: $XDG_RUNTIME_DIR/gprox/control.sock.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// Start with this headphone EQ preset file (TOML, see gprox-core
    /// `HeadphoneEqPreset`). It is listed under its file stem.
    #[arg(long, value_name = "PRESET.toml")]
    hp_eq: Option<PathBuf>,
    /// Start with Blue VO!CE on with this preset, by G HUB name (e.g.
    /// "broadcaster 1", "FM_STATION", "TSM Myth"; see --list-bluevoice).
    #[arg(long, value_name = "NAME")]
    bluevoice: Option<String>,
    /// G HUB's device preset file (`pro_x_wireless_headset_device_presets.json`
    /// from a G HUB install). Remembered in state.toml. Default: the
    /// remembered one, else the copy `gprox setup` made in the config
    /// directory.
    #[arg(long, value_name = "PRESETS.json")]
    ghub_presets: Option<PathBuf>,
    /// Print the available Blue VO!CE presets and exit.
    #[arg(long)]
    list_bluevoice: bool,
    /// Don't talk to the headset dongle (no sidetone, auto-off, battery).
    #[arg(long)]
    no_hid: bool,
    /// G HUB's battery model (`pro_x_wireless_headset_battery.xml`) for the
    /// charge percentage. Remembered in state.toml. Default: the remembered
    /// one, else the copy `gprox setup` made in the config directory, else the
    /// one next to the G HUB preset file.
    #[arg(long, value_name = "BATTERY.xml")]
    battery_model: Option<PathBuf>,
}

/// Capture-path DSP chain.
enum CaptureChain {
    Passthrough(Passthrough),
    BlueVoice(Box<BlueVoice>),
}

/// The capture chain plus the mute meter on its raw input.
struct Capture {
    chain: CaptureChain,
    silence: SilenceMeter,
}

impl Processor for Capture {
    fn prepare(&mut self, sample_rate: u32, max_frames: usize) {
        match &mut self.chain {
            CaptureChain::Passthrough(p) => p.prepare(sample_rate, max_frames),
            CaptureChain::BlueVoice(b) => b.prepare(sample_rate, max_frames),
        }
    }

    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        if let Some(mic) = input.first() {
            self.silence.observe(mic);
        }
        match &mut self.chain {
            CaptureChain::Passthrough(p) => p.process(input, output),
            CaptureChain::BlueVoice(b) => b.process(input, output),
        }
    }
}

/// Render-path DSP chain (boxed: the EQ state is large).
struct RenderChain(Box<HeadphoneEq>);

impl Processor for RenderChain {
    fn prepare(&mut self, sample_rate: u32, max_frames: usize) {
        self.0.prepare(sample_rate, max_frames);
    }

    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]]) {
        self.0.process(input, output);
    }
}

/// Blue VO!CE presets from G HUB's device preset file, in gprox form.
fn read_bluevoice_presets(path: &std::path::Path) -> anyhow::Result<Vec<BlueVoicePreset>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let presets = gprox_ghub::bluevoice_presets(&text)
        .with_context(|| format!("G HUB presets {}", path.display()))?;
    Ok(presets
        .iter()
        .map(gprox_ghub::BlueVoicePreset::to_core)
        .collect())
}

/// Blue VO!CE presets in `dir/*.toml` (gprox form), sorted by file name and
/// named by it (the importer makes file names unique where G HUB names
/// repeat, e.g. `BG [78863648]`). Files that don't parse are skipped with a
/// warning.
fn read_bluevoice_dir(dir: &std::path::Path) -> Vec<BlueVoicePreset> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let r = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|t| Ok(BlueVoicePreset::from_toml_str(&t)?));
        match r {
            Ok(mut p) => {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    p.name = stem.to_owned();
                }
                out.push(p);
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "skipping Blue VO!CE preset")
            }
        }
    }
    out
}

fn read_hp_preset(path: &std::path::Path) -> anyhow::Result<(String, HeadphoneEqPreset)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let preset = HeadphoneEqPreset::from_toml_str(&text)
        .with_context(|| format!("headphone EQ preset {}", path.display()))?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("preset file name is not UTF-8")?
        .to_owned();
    Ok((name, preset))
}

/// The G HUB data files gproxd uses (see [`ghub_files`]).
#[derive(Debug, Default, PartialEq)]
struct GhubFiles {
    /// Device preset file (built-in Blue VO!CE presets).
    presets: Option<PathBuf>,
    /// Battery model candidates, tried in order.
    battery: Vec<PathBuf>,
    /// The battery model was given on the command line: report it if broken.
    battery_explicit: bool,
}

/// Find G HUB's data files. Each is taken from the command line, else the
/// path remembered in state.toml (if the file is still there), else the copy
/// `gprox setup` made in the config directory; the battery model is also
/// looked for next to the preset file (same G HUB depot folder).
fn ghub_files(args: &Args, st: &State, config_dir: &Path) -> GhubFiles {
    let existing = |p: &Path| {
        if p.exists() {
            true
        } else {
            tracing::warn!(file = %p.display(), "remembered G HUB file is gone; ignoring it");
            false
        }
    };
    let pick = |flag: &Option<PathBuf>, remembered: &Option<PathBuf>, name: &str| {
        flag.clone()
            .or_else(|| remembered.clone().filter(|p| existing(p)))
            .or_else(|| Some(config_dir.join(name)).filter(|p| p.exists()))
    };
    let presets = pick(
        &args.ghub_presets,
        &st.ghub_presets,
        gprox_ghub::DEVICE_PRESETS_FILE,
    );
    let battery = match pick(
        &args.battery_model,
        &st.battery_model,
        gprox_ghub::BATTERY_MODEL_FILE,
    ) {
        Some(p) => vec![p],
        None => presets
            .as_deref()
            .and_then(Path::parent)
            .map(|d| d.join(gprox_ghub::BATTERY_MODEL_FILE))
            .into_iter()
            .collect(),
    };
    GhubFiles {
        presets,
        battery,
        battery_explicit: args.battery_model.is_some(),
    }
}

/// G HUB's battery model from the first usable candidate. `None` (with a note
/// in the log) when there is none: only the voltage is reported then.
fn power_model(files: &GhubFiles) -> Option<gprox_hid::PowerModel> {
    for p in &files.battery {
        if !p.exists() && !files.battery_explicit {
            continue;
        }
        match gprox_hid::PowerModel::load(p) {
            Ok(m) => {
                tracing::info!(file = %p.display(), "battery model");
                return Some(m);
            }
            Err(e) => tracing::warn!(error = %e, "battery model not usable"),
        }
    }
    tracing::info!(
        "no G HUB battery model found: battery shown in volts only (run `gprox setup`, or see --battery-model)"
    );
    None
}

/// Apply a start-up request (from a command-line flag) like a client would.
fn startup(ctl: &mut Controller, req: Request) -> anyhow::Result<()> {
    match ctl.handle(&req) {
        Response::Ok(_) => Ok(()),
        Response::Error(e) => anyhow::bail!(e),
    }
}

fn config_dir(args: &Args) -> anyhow::Result<PathBuf> {
    match &args.config_dir {
        Some(d) => Ok(d.clone()),
        None => state::default_config_dir(),
    }
}

/// Build the controller from the saved state and the command line.
fn controller(args: &Args) -> anyhow::Result<(Controller, GhubFiles)> {
    let config_dir = config_dir(args)?;
    let state_path = config_dir.join("state.toml");
    let mut st = State::load(&state_path);
    let files = ghub_files(args, &st, &config_dir);
    if let Some(p) = &args.ghub_presets {
        st.ghub_presets = Some(std::path::absolute(p).context("--ghub-presets path")?);
    }
    if let Some(p) = &args.battery_model {
        st.battery_model = Some(std::path::absolute(p).context("--battery-model path")?);
    }
    // G HUB's built-in presets first, then imported ones not already listed.
    let mut presets = match &files.presets {
        Some(p) => read_bluevoice_presets(p)?,
        None => Vec::new(),
    };
    for p in read_bluevoice_dir(&config_dir.join("bluevoice")) {
        if !presets.iter().any(|q| control::bv_matches(q, &p.name)) {
            presets.push(p);
        }
    }
    let bv_presets = if presets.is_empty() {
        if args.bluevoice.is_some() || args.list_bluevoice {
            anyhow::bail!(
                "no Blue VO!CE presets: run `gprox setup <G HUB folder>`, pass --ghub-presets <file> or run `gprox import ghub`"
            );
        }
        None
    } else {
        Some(presets)
    };
    let hp_extra = args.hp_eq.as_deref().map(read_hp_preset).transpose()?;
    let extra_name = hp_extra.as_ref().map(|(n, _)| n.clone());
    let mut ctl = Controller::new(
        Config {
            hp_dir: config_dir.join("hp-eq"),
            hp_extra,
            bv_presets,
            state_path,
        },
        st,
    );
    if let Some(name) = extra_name {
        startup(&mut ctl, Request::SetHpEq { name })?;
    }
    if let Some(name) = &args.bluevoice {
        startup(&mut ctl, Request::SetBluevoice { name: name.clone() })?;
        startup(&mut ctl, Request::BluevoiceEnable { on: true })?;
    }
    if args.ghub_presets.is_some() || args.battery_model.is_some() {
        // Remember the G HUB files even if no selection changed.
        ctl.persist();
    }
    Ok((ctl, files))
}

fn main() -> anyhow::Result<()> {
    let log = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(std::io::stdout().is_terminal());
    // Under systemd the journal adds its own timestamps.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        log.without_time().init();
    } else {
        log.init();
    }
    let args = Args::parse();
    let (mut ctl, ghub) = controller(&args)?;
    if args.list_bluevoice {
        for p in ctl.status().bluevoice.presets {
            println!("{p}");
        }
        return Ok(());
    }

    // DSP with lock-free parameter handles for the controller.
    let (eq, hp_handle) = HeadphoneEq::new_shared(&ctl.hp_settings());
    let render_dsp = RenderChain(Box::new(eq));
    let (chain, bv_handle) = match ctl.bv_params().transpose()? {
        Some(p) => {
            let (bv, h) = BlueVoice::new_shared(&p);
            (CaptureChain::BlueVoice(Box::new(bv)), Some(h))
        }
        None => (CaptureChain::Passthrough(Passthrough), None),
    };
    let (silence, mut probe) = silence_meter(args.rate);
    let capture_dsp = Capture { chain, silence };
    ctl.attach(hp_handle, bv_handle);
    let headset = if args.no_hid {
        None
    } else {
        let cfg = gprox_hid::Config {
            power_model: power_model(&ghub),
            sidetone: ctl.saved_sidetone(),
            audio: Some(Box::new(move || {
                let a = probe.poll();
                gprox_hid::mute::AudioObservation {
                    last_audio: a.last_audio,
                    stream_started: a.stream_started,
                }
            })),
            ..Default::default()
        };
        Some(gprox_hid::HeadsetService::spawn(cfg).context("start the headset service")?)
    };
    ctl.attach_headset(headset);
    let st = ctl.status();
    tracing::info!(hp_eq = %st.hp_eq.selected, bluevoice_available = st.bluevoice.available,
        bluevoice = st.bluevoice.enabled, bluevoice_preset = ?st.bluevoice.selected,
        "selection");
    let socket = match &args.socket {
        Some(p) => p.clone(),
        None => gprox_core::control::socket_path()?,
    };
    let _server = control::serve(Arc::new(Mutex::new(ctl)), &socket)?;

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("main loop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("context")?;
    let core = context.connect_rc(None).context("connect to PipeWire")?;

    let render = Duplex::new(
        &core,
        DuplexSpec {
            direction: Direction::Render,
            name: gprox_core::SINK_NODE.into(),
            description: "PRO X (gprox)".into(),
            target: Some(args.sink_target.clone()),
            positions: vec![position::FL, position::FR],
            rate: args.rate,
            latency_frames: args.quantum,
            priority: Some(NODE_PRIORITY),
        },
        render_dsp,
    )
    .context("render path")?;

    let capture = Duplex::new(
        &core,
        DuplexSpec {
            direction: Direction::Capture,
            name: gprox_core::SOURCE_NODE.into(),
            description: "PRO X Mic (gprox)".into(),
            target: Some(args.source_target.clone()),
            positions: vec![position::MONO],
            rate: args.rate,
            latency_frames: args.quantum,
            priority: Some(NODE_PRIORITY),
        },
        capture_dsp,
    )
    .context("capture path")?;

    tracing::info!(
        sink = %args.sink_target, source = %args.source_target,
        quantum = args.quantum, rate = args.rate,
        "gproxd running: 'PRO X (gprox)' sink, 'PRO X Mic (gprox)' source"
    );

    let ml = mainloop.clone();
    let _sigint = mainloop
        .loop_()
        .add_signal_local(Signal::INT, move || ml.quit());
    let ml = mainloop.clone();
    let _sigterm = mainloop
        .loop_()
        .add_signal_local(Signal::TERM, move || ml.quit());

    let paths = Rc::new([render, capture]);
    let last = Rc::new(Cell::new([StatsSnapshot::default(); 2]));
    let report = {
        let paths = paths.clone();
        let last = last.clone();
        move |_| {
            let mut prev = last.get();
            for (i, p) in paths.iter().enumerate() {
                let s = p.stats();
                let d = |a: u64, b: u64| a.saturating_sub(b);
                tracing::info!(
                    path = %p.spec().name,
                    nodes = ?p.node_ids(),
                    cycles = d(s.cycles, prev[i].cycles),
                    capture_cycles = d(s.capture_cycles, prev[i].capture_cycles),
                    frames = s.last_frames,
                    no_input = d(s.no_input, prev[i].no_input),
                    no_output = d(s.no_output, prev[i].no_output),
                    dropped_input = d(s.dropped_input, prev[i].dropped_input),
                    truncated = d(s.truncated, prev[i].truncated),
                    trigger_failed = d(s.trigger_failed, prev[i].trigger_failed),
                    "stats"
                );
                prev[i] = s;
            }
            last.set(prev);
        }
    };
    let timer = mainloop.loop_().add_timer(report);
    if args.stats_interval > 0 {
        let iv = Duration::from_secs(args.stats_interval);
        timer
            .update_timer(Some(iv), Some(iv))
            .into_result()
            .context("arm stats timer")?;
    }
    let ml = mainloop.clone();
    let stop = mainloop.loop_().add_timer(move |_| ml.quit());
    if args.run_for > 0 {
        stop.update_timer(Some(Duration::from_secs(args.run_for)), None)
            .into_result()
            .context("arm stop timer")?;
    }

    mainloop.run();

    for p in paths.iter() {
        let s = p.stats();
        tracing::info!(path = %p.spec().name, totals = ?s, "exiting");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghub_file_names_agree() {
        assert_eq!(gprox_ghub::BATTERY_MODEL_FILE, gprox_hid::power::GHUB_FILE);
    }

    #[test]
    fn ghub_files_order() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("cfg");
        let depot = d.path().join("depot");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&depot).unwrap();
        let args = |extra: &[&str]| {
            Args::parse_from(
                ["gproxd"]
                    .iter()
                    .chain(extra.iter())
                    .copied()
                    .collect::<Vec<_>>(),
            )
        };
        let presets = gprox_ghub::DEVICE_PRESETS_FILE;
        let battery = gprox_ghub::BATTERY_MODEL_FILE;
        let none = State::default();

        // Nothing anywhere.
        assert_eq!(ghub_files(&args(&[]), &none, &cfg), GhubFiles::default());

        // The copies in the config directory.
        std::fs::write(cfg.join(presets), "{}").unwrap();
        std::fs::write(cfg.join(battery), "").unwrap();
        let f = ghub_files(&args(&[]), &none, &cfg);
        assert_eq!(f.presets, Some(cfg.join(presets)));
        assert_eq!(f.battery, [cfg.join(battery)]);

        // A remembered path wins while the file exists.
        std::fs::write(depot.join(presets), "{}").unwrap();
        std::fs::write(depot.join(battery), "").unwrap();
        let st = State {
            ghub_presets: Some(depot.join(presets)),
            battery_model: Some(depot.join(battery)),
            ..State::default()
        };
        let f = ghub_files(&args(&[]), &st, &cfg);
        assert_eq!(f.presets, Some(depot.join(presets)));
        assert_eq!(f.battery, [depot.join(battery)]);
        let gone = State {
            ghub_presets: Some(d.path().join("gone.json")),
            battery_model: Some(d.path().join("gone.xml")),
            ..State::default()
        };
        assert_eq!(
            ghub_files(&args(&[]), &gone, &cfg).presets,
            Some(cfg.join(presets))
        );

        // The command line wins, even for a missing file (reported later).
        let f = ghub_files(&args(&["--battery-model", "/nope.xml"]), &st, &cfg);
        assert_eq!(f.battery, [PathBuf::from("/nope.xml")]);
        assert!(f.battery_explicit);

        // Without a battery model of its own: next to the preset file.
        std::fs::remove_file(cfg.join(battery)).unwrap();
        let only_presets = State {
            ghub_presets: Some(depot.join(presets)),
            ..State::default()
        };
        let f = ghub_files(&args(&[]), &only_presets, &cfg);
        assert_eq!(f.battery, [depot.join(battery)]);
    }

    #[test]
    fn bluevoice_dir_names_by_file_and_skips_broken() {
        let d = std::env::temp_dir().join(format!("gprox-bvdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let body = "name = \"BG\"\n[engine.micopt]\nenable = 1.0\n";
        std::fs::write(d.join("BG [aaaa].toml"), body).unwrap();
        std::fs::write(d.join("BG [bbbb].toml"), body).unwrap();
        std::fs::write(d.join("broken.toml"), "engine = 3").unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        let v = read_bluevoice_dir(&d);
        let names: Vec<_> = v.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["BG [aaaa]", "BG [bbbb]"]);
        assert!(control::bv_matches(&v[1], "bg [BBBB]"));
        assert_eq!(v[0].params(), [("micopt.enable".to_owned(), 1.0)]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
