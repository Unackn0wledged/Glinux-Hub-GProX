//! `gprox`: command-line control of a running `gproxd` over its control
//! socket (`gprox_core::control`), preset import from G HUB and the
//! one-time setup after installing.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gprox_core::control::{Client, Request, Status};

mod import;
mod setup;

#[derive(Debug, Parser)]
#[command(version, about = "Control a running gproxd")]
struct Cli {
    /// Control socket. Default: $XDG_RUNTIME_DIR/gprox/control.sock.
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<std::path::PathBuf>,
    /// Print the daemon's status after the command as JSON (for scripts and
    /// the Plasma tray widget); errors become `{"error": "…"}` on stdout.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Show the current selection.
    Status,
    /// Headphone EQ presets.
    #[command(subcommand)]
    Preset(PresetCmd),
    /// Blue VO!CE (mic chain).
    #[command(subcommand)]
    Bluevoice(BvCmd),
    /// Set the headset's sidetone (mic monitoring) level, or switch
    /// always-on sidetone on or off.
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Sidetone {
        /// 0–100.
        #[arg(required = true, value_parser = clap::value_parser!(u8).range(0..=100))]
        level: Option<u8>,
        #[command(subcommand)]
        mode: Option<SidetoneCmd>,
    },
    /// Set the headset's auto-off timer.
    AutoOff {
        /// Minutes: 1, 2, 5, 10, 15, 30, or "never".
        #[arg(value_parser = parse_auto_off)]
        minutes: u8,
    },
    /// Import presets from other software into gprox's preset files (no
    /// running gproxd needed).
    #[command(subcommand)]
    Import(import::Source),
    /// One-time setup: copy G HUB's Blue VO!CE presets and battery model from
    /// your G HUB install into ~/.config/gprox, and make the gprox devices
    /// your default output and input (no running gproxd needed).
    Setup(setup::SetupArgs),
}

#[derive(Debug, Subcommand)]
enum SidetoneCmd {
    /// Keep the headset's mic stream open so sidetone works even when no app
    /// uses the mic (the headset only mixes sidetone while the mic streams).
    /// Off by default.
    AlwaysOn {
        #[arg(value_enum)]
        state: OnOff,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Debug, Subcommand)]
enum PresetCmd {
    /// List headphone EQ and Blue VO!CE presets (* = selected).
    List,
    /// Select a headphone EQ preset ("flat" or a file stem in
    /// ~/.config/gprox/hp-eq/).
    Set {
        /// Preset name; several words are joined with spaces.
        #[arg(required = true, num_args = 1..)]
        name: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum BvCmd {
    /// Switch Blue VO!CE on.
    On,
    /// Switch Blue VO!CE off.
    Off,
    /// Select a Blue VO!CE preset (G HUB name, e.g. "broadcaster 1").
    Set {
        /// Preset name; several words are joined with spaces.
        #[arg(required = true, num_args = 1..)]
        name: Vec<String>,
    },
}

fn parse_auto_off(s: &str) -> Result<u8, String> {
    if s.eq_ignore_ascii_case("never") || s == "0" {
        return Ok(0);
    }
    match s.parse::<u8>() {
        Ok(m) if [1, 2, 5, 10, 15, 30].contains(&m) => Ok(m),
        _ => Err("use 1, 2, 5, 10, 15, 30 or never".into()),
    }
}

fn mark(selected: bool) -> &'static str {
    if selected { "*" } else { " " }
}

fn print_list(s: &Status) {
    println!("Headphone EQ presets:");
    for n in &s.hp_eq.presets {
        println!(" {} {n}", mark(*n == s.hp_eq.selected));
    }
    let bv = &s.bluevoice;
    if !bv.available {
        println!(
            "Blue VO!CE: not available (run `gprox setup <G HUB folder>`, then restart gproxd)"
        );
        return;
    }
    println!(
        "Blue VO!CE presets ({}):",
        if bv.enabled { "on" } else { "off" }
    );
    for n in &bv.presets {
        println!(" {} {n}", mark(bv.selected.as_ref() == Some(n)));
    }
}

fn print_status(s: &Status) {
    let p = &s.hp_eq.preset;
    let gains = match p.mode {
        gprox_core::EqMode::Basic => format!("basic {:?} dB", p.basic),
        gprox_core::EqMode::Advanced => format!("advanced {:?} dB", p.advanced),
    };
    println!("Headphone EQ: {} ({gains})", s.hp_eq.selected);
    let bv = &s.bluevoice;
    if !bv.available {
        println!("Blue VO!CE:   not available");
    } else {
        println!(
            "Blue VO!CE:   {}, preset {}",
            if bv.enabled { "on" } else { "off" },
            bv.selected.as_deref().unwrap_or("(none)")
        );
    }
    println!("Headset:      {}", s.headset.summary());
}

/// Print the status after a control command: JSON with `--json`, else the
/// human-readable form.
fn show(s: &Status, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(s)?);
    } else {
        print_status(s);
    }
    Ok(())
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.cmd {
        Cmd::Import(source) => return import::run(source),
        Cmd::Setup(args) => return setup::run(&args),
        _ => {}
    }
    let mut client = match &cli.socket {
        Some(p) => Client::connect_to(p)?,
        None => Client::connect()?,
    };
    let json = cli.json;
    match cli.cmd {
        Cmd::Status => show(&client.call(&Request::Status)?, json)?,
        Cmd::Import(_) | Cmd::Setup(_) => unreachable!("handled above"),
        Cmd::Preset(PresetCmd::List) => {
            let s = client.call(&Request::Status)?;
            if json {
                show(&s, true)?;
            } else {
                print_list(&s);
            }
        }
        Cmd::Preset(PresetCmd::Set { name }) => {
            let req = Request::SetHpEq {
                name: name.join(" "),
            };
            show(&client.call(&req)?, json)?;
        }
        Cmd::Sidetone { level, mode } => {
            let req = match (level, mode) {
                (_, Some(SidetoneCmd::AlwaysOn { state })) => Request::SetAlwaysOnSidetone {
                    on: state == OnOff::On,
                },
                (Some(level), None) => Request::SetSidetone { level },
                (None, None) => unreachable!("clap requires a level or a subcommand"),
            };
            show(&client.call(&req)?, json)?
        }
        Cmd::AutoOff { minutes } => show(&client.call(&Request::SetAutoOff { minutes })?, json)?,
        Cmd::Bluevoice(c) => {
            let req = match c {
                BvCmd::On => Request::BluevoiceEnable { on: true },
                BvCmd::Off => Request::BluevoiceEnable { on: false },
                BvCmd::Set { name } => Request::SetBluevoice {
                    name: name.join(" "),
                },
            };
            show(&client.call(&req)?, json)?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let msg = format!("{e:#}");
            if json {
                println!("{}", serde_json::json!({ "error": msg }));
            }
            eprintln!("gprox: {msg}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_flag_is_global() {
        // Old form (`status --json`) and the global form both parse.
        for args in [
            &["gprox", "status", "--json"][..],
            &["gprox", "--json", "status"],
            &["gprox", "--json", "preset", "set", "bass", "boost"],
            &["gprox", "sidetone", "30", "--json"],
        ] {
            assert!(Cli::try_parse_from(args).unwrap().json, "{args:?}");
        }
        assert!(!Cli::try_parse_from(["gprox", "status"]).unwrap().json);
    }

    #[test]
    fn sidetone_level_or_always_on() {
        let parse = |args: &[&str]| {
            let mut v = vec!["gprox", "sidetone"];
            v.extend_from_slice(args);
            Cli::try_parse_from(v).map(|c| match c.cmd {
                Cmd::Sidetone { level, mode } => {
                    (level, mode.map(|SidetoneCmd::AlwaysOn { state }| state))
                }
                other => panic!("{other:?}"),
            })
        };
        assert_eq!(parse(&["30"]).unwrap(), (Some(30), None));
        assert_eq!(parse(&["0"]).unwrap(), (Some(0), None));
        assert_eq!(
            parse(&["always-on", "on"]).unwrap(),
            (None, Some(OnOff::On))
        );
        assert_eq!(
            parse(&["always-on", "off"]).unwrap(),
            (None, Some(OnOff::Off))
        );
        for bad in [
            &[][..],
            &["101"],
            &["always-on"],
            &["always-on", "maybe"],
            &["30", "always-on", "on"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn auto_off_values() {
        assert_eq!(parse_auto_off("never"), Ok(0));
        assert_eq!(parse_auto_off("15"), Ok(15));
        assert!(parse_auto_off("7").is_err());
        assert!(parse_auto_off("x").is_err());
    }
}
