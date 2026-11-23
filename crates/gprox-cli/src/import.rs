//! `gprox import ghub <settings.db>`: G HUB presets → gprox preset files.
//!
//! Writes every headphone EQ preset to `<dir>/hp-eq/<name>.toml`
//! (`gprox_core::HeadphoneEqPreset`) and every Blue VO!CE preset to
//! `<dir>/bluevoice/<name>.toml` (`gprox_core::BlueVoicePreset`), where
//! `<dir>` is gprox's config directory unless `--out` says otherwise. G HUB's
//! own data is only read (from a private copy).

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Args, Subcommand};
use gprox_core::EqMode;
use gprox_ghub::settings::{self, GhubSettings};

#[derive(Debug, Subcommand)]
pub enum Source {
    /// G HUB: headphone EQ and Blue VO!CE presets from settings.db, including
    /// your own and downloaded ones.
    Ghub(GhubArgs),
}

#[derive(Debug, Args)]
pub struct GhubArgs {
    /// G HUB's settings.db (Windows: %LOCALAPPDATA%\LGHUB\settings.db) or the
    /// folder holding it. Copy settings.db-wal along with it, or close G HUB
    /// first: while G HUB runs, recent changes are only in the -wal file.
    settings_db: PathBuf,
    /// Output directory: writes DIR/hp-eq/*.toml and DIR/bluevoice/*.toml.
    /// Default: $XDG_CONFIG_HOME/gprox (else ~/.config/gprox).
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
    /// Overwrite preset files that already exist with different content.
    #[arg(long)]
    force: bool,
    /// Show what would be written, write nothing.
    #[arg(long)]
    dry_run: bool,
}

pub fn run(source: Source) -> anyhow::Result<()> {
    match source {
        Source::Ghub(args) => ghub(&args),
    }
}

pub fn config_dir() -> anyhow::Result<PathBuf> {
    gprox_core::config_dir().context("neither XDG_CONFIG_HOME nor HOME is set; pass --out")
}

/// What happened to one output file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    New,
    Updated,
    Unchanged,
    /// Exists with other content and `--force` wasn't given.
    Kept,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::New => "new",
            Outcome::Updated => "updated",
            Outcome::Unchanged => "unchanged",
            Outcome::Kept => "kept",
        }
    }
}

fn write_preset(path: &Path, text: &str, args: &GhubArgs) -> anyhow::Result<Outcome> {
    let outcome = match std::fs::read_to_string(path) {
        Ok(old) if old == text => return Ok(Outcome::Unchanged),
        Ok(_) if !args.force => return Ok(Outcome::Kept),
        Ok(_) => Outcome::Updated,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Outcome::New,
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    if !args.dry_run {
        let dir = path.parent().context("preset path has no directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        std::fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(outcome)
}

/// TOML comment line naming the G HUB source.
fn origin(name: &str, id: &str, read_only: bool) -> String {
    let kind = if read_only {
        ", read-only in G HUB"
    } else {
        ""
    };
    format!("# Imported from G HUB: {name:?} (id {id}{kind})\n")
}

#[derive(Default)]
struct Tally([usize; 4]);

impl Tally {
    fn add(&mut self, o: Outcome) {
        self.0[o as usize] += 1;
    }

    fn summary(&self) -> String {
        let [n, u, s, k] = self.0;
        format!("{n} new, {u} updated, {s} unchanged, {k} kept")
    }
}

fn ghub(args: &GhubArgs) -> anyhow::Result<()> {
    let doc = settings::read_settings_db(&args.settings_db)
        .with_context(|| format!("read G HUB settings {}", args.settings_db.display()))?;
    let s = settings::parse(&doc.json).context("G HUB settings document")?;
    let out = match &args.out {
        Some(d) => d.clone(),
        None => config_dir()?,
    };
    println!(
        "G HUB settings {} (saved {} UTC){}",
        args.settings_db.display(),
        doc.saved.as_deref().unwrap_or("?"),
        if args.dry_run { ", dry run" } else { "" }
    );
    if args.settings_db.is_file() {
        let mut wal = args.settings_db.as_os_str().to_owned();
        wal.push("-wal");
        if !Path::new(&wal).exists() {
            println!(
                "note: no settings.db-wal next to it. If G HUB was running when the file was \
                 copied, its latest changes are missing."
            );
        }
    }

    let eq_names: Vec<String> = s.eq.iter().map(|p| p.display_name()).collect();
    let eq_stems = settings::file_stems(
        eq_names
            .iter()
            .zip(&s.eq)
            .map(|(n, p)| (n.as_str(), p.id.as_str())),
    );
    let bv_stems =
        settings::file_stems(s.bluevoice.iter().map(|p| (p.short_name(), p.id.as_str())));

    let eq_dir = out.join("hp-eq");
    println!(
        "\nheadphone EQ: {} presets → {}",
        s.eq.len(),
        eq_dir.display()
    );
    let mut tally = Tally::default();
    for (p, stem) in s.eq.iter().zip(&eq_stems) {
        let text = origin(&p.name, &p.id, p.read_only) + &p.preset.to_toml_string()?;
        let path = eq_dir.join(format!("{stem}.toml"));
        let o = write_preset(&path, &text, args)?;
        tally.add(o);
        let mode = match p.preset.mode {
            EqMode::Basic => "basic",
            EqMode::Advanced => "advanced",
        };
        println!("  {:<9} {stem}.toml  ({mode})", o.label());
    }
    println!("  {}", tally.summary());

    let bv_dir = out.join("bluevoice");
    println!(
        "\nBlue VO!CE: {} presets → {}",
        s.bluevoice.len(),
        bv_dir.display()
    );
    let mut tally = Tally::default();
    let mut unusable = Vec::new();
    for (p, stem) in s.bluevoice.iter().zip(&bv_stems) {
        let core = match p.to_core() {
            Ok(c) => c,
            Err(e) => {
                println!("  {:<9} {stem}.toml", "skipped");
                unusable.push((p, e));
                continue;
            }
        };
        let text = origin(&p.name, &p.id, p.read_only) + &core.to_toml_string()?;
        let path = bv_dir.join(format!("{stem}.toml"));
        let o = write_preset(&path, &text, args)?;
        tally.add(o);
        println!("  {:<9} {stem}.toml", o.label());
    }
    println!("  {}", tally.summary());
    if tally.0[Outcome::Kept as usize] > 0 {
        println!("  (kept = an existing file differs; --force overwrites)");
    }

    print_selections(&s, &eq_stems, &bv_stems);

    if !s.skipped.is_empty() || !unusable.is_empty() {
        eprintln!("\nnot imported:");
        for k in &s.skipped {
            eprintln!("  {} {:?} (id {}): {}", k.kind, k.name, k.id, k.reason);
        }
        for (p, e) in &unusable {
            eprintln!("  Blue VO!CE {:?} (id {}): {e}", p.name, p.id);
        }
    }
    Ok(())
}

fn print_selections(s: &GhubSettings, eq_stems: &[String], bv_stems: &[String]) {
    if s.selections.is_empty() {
        return;
    }
    println!("\nactive in G HUB:");
    let eq_ids: Vec<&str> = s.eq.iter().map(|p| p.id.as_str()).collect();
    let bv_ids: Vec<&str> = s.bluevoice.iter().map(|p| p.id.as_str()).collect();
    let stem_of = |id: &Option<String>, ids: &[&str], stems: &[String]| match id.as_deref() {
        None => "-".to_owned(),
        Some(id) => ids
            .iter()
            .position(|x| *x == id)
            .map_or_else(|| format!("(card {id} not found)"), |i| stems[i].clone()),
    };
    for sel in &s.selections {
        let eq = stem_of(&sel.eq, &eq_ids, eq_stems);
        let bv = stem_of(&sel.bluevoice, &bv_ids, bv_stems);
        let on = match sel.bluevoice_enabled {
            Some(true) => " (on)",
            Some(false) => " (off)",
            None => "",
        };
        println!(
            "  {} / {}: headphone EQ {eq}, Blue VO!CE {bv}{on}",
            sel.profile, sel.device
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(force: bool, dry_run: bool) -> GhubArgs {
        GhubArgs {
            settings_db: PathBuf::new(),
            out: None,
            force,
            dry_run,
        }
    }

    #[test]
    fn write_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("x.toml");
        assert_eq!(
            write_preset(&p, "a", &args(false, true)).unwrap(),
            Outcome::New
        );
        assert!(!p.exists(), "dry run wrote");
        assert_eq!(
            write_preset(&p, "a", &args(false, false)).unwrap(),
            Outcome::New
        );
        assert_eq!(
            write_preset(&p, "a", &args(false, false)).unwrap(),
            Outcome::Unchanged
        );
        assert_eq!(
            write_preset(&p, "b", &args(false, false)).unwrap(),
            Outcome::Kept
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a");
        assert_eq!(
            write_preset(&p, "b", &args(true, false)).unwrap(),
            Outcome::Updated
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "b");
    }
}
