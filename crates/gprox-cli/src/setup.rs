//! `gprox setup <G HUB folder>`: the one-time setup after installing gprox.
//!
//! 1. Copies two files from the user's own G HUB install into gprox's config
//!    directory, where gproxd looks for them: the device preset file (G HUB's
//!    built-in Blue VO!CE presets) and the battery model (charge percentage).
//!    gprox ships no G HUB data.
//! 2. Records "PRO X (gprox)" / "PRO X Mic (gprox)" as the user's chosen
//!    default output and input (`default.configured.audio.*` in PipeWire's
//!    `default` metadata, what a mixer writes when you pick a device; WirePlumber
//!    keeps it in its history). Without this, a user who once picked the headset
//!    itself would keep getting the headset, because WirePlumber prefers stored
//!    choices over node priority. When gproxd stops, WirePlumber falls back to
//!    the next device in the history or by priority. `--keep-defaults` skips it.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Args;
use gprox_ghub::{BATTERY_MODEL_FILE, DEVICE_PRESETS_FILE};

#[derive(Debug, Args)]
pub struct SetupArgs {
    /// G HUB's `pro_headset` folder (Windows:
    /// C:\ProgramData\LGHUB\depots\<number>\pro_headset), or any folder above
    /// it, e.g. a copy of C:\ProgramData\LGHUB.
    ghub_dir: PathBuf,
    /// Config directory to copy into. Default: $XDG_CONFIG_HOME/gprox (else
    /// ~/.config/gprox), where gproxd looks.
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,
    /// Leave the default output and input devices as they are.
    #[arg(long)]
    keep_defaults: bool,
    /// Show what would be done, change nothing.
    #[arg(long)]
    dry_run: bool,
}

/// How deep below the given folder to look for the preset file
/// (`LGHUB/depots/<n>/pro_headset/<file>` is 3 levels).
const MAX_DEPTH: usize = 6;

/// Every `DEVICE_PRESETS_FILE` at most `depth` levels below `dir`.
fn find_presets(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_file() && e.file_name() == DEVICE_PRESETS_FILE {
            out.push(path);
        } else if ft.is_dir() && depth > 0 {
            find_presets(&path, depth - 1, out);
        }
    }
}

/// G HUB's preset file for `arg` (the file itself, its folder, or a folder
/// above). With several depots, the most recently modified file wins.
fn locate(arg: &Path) -> anyhow::Result<PathBuf> {
    if arg.is_file() {
        anyhow::ensure!(
            arg.file_name().is_some_and(|n| n == DEVICE_PRESETS_FILE),
            "{} is not G HUB's {DEVICE_PRESETS_FILE}",
            arg.display()
        );
        return Ok(arg.to_path_buf());
    }
    anyhow::ensure!(arg.is_dir(), "{}: no such folder", arg.display());
    let mut found = Vec::new();
    find_presets(arg, MAX_DEPTH, &mut found);
    let modified = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let newest = found
        .iter()
        .max_by_key(|p| modified(p))
        .cloned()
        .with_context(|| {
            format!(
                "no {DEVICE_PRESETS_FILE} in {} (point at G HUB's depots/<number>/pro_headset folder)",
                arg.display()
            )
        })?;
    if found.len() > 1 {
        println!(
            "Found {} G HUB preset files; using the newest:",
            found.len()
        );
        for p in &found {
            println!(" {} {}", if *p == newest { "*" } else { " " }, p.display());
        }
    }
    Ok(newest)
}

/// What happened to one copied file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    New,
    Updated,
    Unchanged,
}

/// Copy `from` to `to` (atomically: temp file + rename) unless it is already
/// identical.
fn copy(from: &Path, to: &Path, dry_run: bool) -> anyhow::Result<Outcome> {
    let data = std::fs::read(from).with_context(|| format!("read {}", from.display()))?;
    let outcome = match std::fs::read(to) {
        Ok(old) if old == data => return Ok(Outcome::Unchanged),
        Ok(_) => Outcome::Updated,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Outcome::New,
        Err(e) => return Err(e).with_context(|| format!("read {}", to.display())),
    };
    if !dry_run {
        let dir = to.parent().context("target has no directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let tmp = to.with_extension("tmp");
        std::fs::write(&tmp, &data).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, to).with_context(|| format!("replace {}", to.display()))?;
    }
    Ok(outcome)
}

fn report(what: &str, to: &Path, outcome: Outcome, dry_run: bool) {
    let verb = match (outcome, dry_run) {
        (Outcome::Unchanged, _) => "already up to date",
        (Outcome::New, false) => "copied",
        (Outcome::Updated, false) => "updated",
        (Outcome::New, true) => "would copy",
        (Outcome::Updated, true) => "would update",
    };
    println!("{what}: {verb}: {}", to.display());
}

/// Paths in `state.toml` that gproxd prefers over the copies (remembered from
/// `--ghub-presets` / `--battery-model`), with their key.
fn remembered_paths(config_dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(text) = std::fs::read_to_string(config_dir.join("state.toml")) else {
        return Vec::new();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    ["ghub_presets", "battery_model"]
        .iter()
        .filter_map(|k| {
            let p = PathBuf::from(table.get(*k)?.as_str()?);
            p.exists().then(|| ((*k).to_owned(), p))
        })
        .collect()
}

/// `pw-metadata` call that records `node` as the chosen default of `kind`
/// (`audio.sink` / `audio.source`).
fn default_command(kind: &str, node: &str) -> std::process::Command {
    let mut c = std::process::Command::new("pw-metadata");
    c.args(["-n", "default", "0"])
        .arg(format!("default.configured.{kind}"))
        .arg(serde_json::json!({ "name": node }).to_string())
        .arg("Spa:String:JSON");
    c
}

fn set_defaults(dry_run: bool) -> anyhow::Result<()> {
    for (kind, node, label) in [
        ("audio.sink", gprox_core::SINK_NODE, "default output"),
        ("audio.source", gprox_core::SOURCE_NODE, "default input"),
    ] {
        if dry_run {
            println!("{label}: would select {node}");
            continue;
        }
        let out = default_command(kind, node)
            .stdout(std::process::Stdio::null())
            .output()
            .context("run pw-metadata (part of the pipewire package)")?;
        anyhow::ensure!(
            out.status.success(),
            "pw-metadata failed to set the {label}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        println!("{label}: {node}");
    }
    Ok(())
}

pub fn run(args: &SetupArgs) -> anyhow::Result<()> {
    let config_dir = match &args.config_dir {
        Some(d) => d.clone(),
        None => crate::import::config_dir()?,
    };
    let presets = locate(&args.ghub_dir)?;
    let text =
        std::fs::read_to_string(&presets).with_context(|| format!("read {}", presets.display()))?;
    let n = gprox_ghub::bluevoice_presets(&text)
        .with_context(|| format!("G HUB presets {}", presets.display()))?
        .len();
    let to = config_dir.join(DEVICE_PRESETS_FILE);
    let outcome = copy(&presets, &to, args.dry_run)?;
    report(
        &format!("G HUB presets ({n} Blue VO!CE)"),
        &to,
        outcome,
        args.dry_run,
    );

    let battery = presets.with_file_name(BATTERY_MODEL_FILE);
    if battery.is_file() {
        gprox_hid::PowerModel::load(&battery)
            .with_context(|| format!("G HUB battery model {}", battery.display()))?;
        let to = config_dir.join(BATTERY_MODEL_FILE);
        let outcome = copy(&battery, &to, args.dry_run)?;
        report("G HUB battery model", &to, outcome, args.dry_run);
    } else {
        println!(
            "note: no {BATTERY_MODEL_FILE} next to the preset file; the battery shows in volts only"
        );
    }

    for (key, path) in remembered_paths(&config_dir) {
        println!(
            "note: state.toml remembers {key} = {} (from a --{} flag); gproxd keeps using that. \
             Delete the line with gproxd stopped to use the copy.",
            path.display(),
            key.replace('_', "-")
        );
    }

    if !args.keep_defaults {
        if let Err(e) = set_defaults(args.dry_run) {
            println!(
                "note: {e:#}. Pick \"PRO X (gprox)\" and \"PRO X Mic (gprox)\" in your volume settings instead."
            );
        }
    }

    if !args.dry_run && gprox_core::control::Client::connect().is_ok() {
        println!(
            "gproxd is running: restart it to load the files: systemctl --user restart gproxd"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_finds_newest_depot() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("depots/100/pro_headset");
        let b = d.path().join("depots/200/pro_headset");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join(DEVICE_PRESETS_FILE), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(b.join(DEVICE_PRESETS_FILE), "{}").unwrap();
        assert_eq!(locate(d.path()).unwrap(), b.join(DEVICE_PRESETS_FILE));
        assert_eq!(locate(&a).unwrap(), a.join(DEVICE_PRESETS_FILE));
        let file = a.join(DEVICE_PRESETS_FILE);
        assert_eq!(locate(&file).unwrap(), file);
        assert!(locate(&d.path().join("nope")).is_err());
        let empty = d.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(locate(&empty).is_err());
    }

    #[test]
    fn copy_reports_and_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let from = d.path().join("a.xml");
        let to = d.path().join("cfg/a.xml");
        std::fs::write(&from, "one").unwrap();
        assert_eq!(copy(&from, &to, true).unwrap(), Outcome::New);
        assert!(!to.exists());
        assert_eq!(copy(&from, &to, false).unwrap(), Outcome::New);
        assert_eq!(copy(&from, &to, false).unwrap(), Outcome::Unchanged);
        std::fs::write(&from, "two").unwrap();
        assert_eq!(copy(&from, &to, false).unwrap(), Outcome::Updated);
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "two");
    }

    #[test]
    fn remembered_paths_only_existing() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("presets.json");
        std::fs::write(&real, "{}").unwrap();
        std::fs::write(
            d.path().join("state.toml"),
            format!(
                "hp_eq = \"flat\"\nghub_presets = {:?}\nbattery_model = \"/gone.xml\"\n",
                real.display().to_string()
            ),
        )
        .unwrap();
        assert_eq!(
            remembered_paths(d.path()),
            [("ghub_presets".to_owned(), real)]
        );
    }

    #[test]
    fn default_command_args() {
        let c = default_command("audio.sink", "gprox_render");
        let args: Vec<_> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(c.get_program(), "pw-metadata");
        assert_eq!(
            args,
            [
                "-n",
                "default",
                "0",
                "default.configured.audio.sink",
                r#"{"name":"gprox_render"}"#,
                "Spa:String:JSON"
            ]
        );
    }
}
