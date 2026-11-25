//! `gprox-gui`: a small, functional front-end for a running `gproxd`.
//!
//! Headphone EQ preset picker, the EQ's magnitude curve and its band sliders;
//! Blue VO!CE on/off, preset picker and a slider per engine parameter; saving
//! and deleting user presets; and the headset (battery, mic mute, sidetone,
//! auto-off). Everything goes through the control socket
//! (`gprox_core::control`); the GUI never touches audio. The curve uses the
//! DSP's own coefficient design (`gprox_dsp::steady_state_taps`), so it shows
//! exactly what the render path converges to.
//!
//! Editing: slider moves are sent while dragging, at
//! most every [`EDIT_INTERVAL`], and the final value on release; gproxd ramps
//! and smooths them like a preset switch. Until a change is acknowledged the
//! GUI shows its own values (a "draft"), so the sliders and the curve follow
//! the pointer without waiting for the daemon.
//!
//! `GPROX_GUI_SCREENSHOT=<file.ppm>`: save a screenshot of this window (only
//! this window) shortly after start and exit. For docs and bug reports.

mod bv_params;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use gprox_core::control::{Client, ControlError, FLAT_PRESET, Request, Status};
use gprox_core::{HeadphoneEqPreset, param_range};
use gprox_dsp::hp_eq::{BAND_TABLE, BANDS};
use gprox_dsp::{EqMode, HpEqSettings, biquad::Taps, cascade_magnitude_db, steady_state_taps};

/// How often the GUI re-reads the daemon's state (other clients may change it).
const REFRESH: Duration = Duration::from_secs(2);
/// Sample rate the curve is evaluated at (gproxd's default rate).
const FS: u32 = 48_000;
const F_MIN: f64 = 20.0;
const F_MAX: f64 = 20_000.0;
const DB_RANGE: f64 = 15.0;
const CURVE_POINTS: usize = 400;
/// Shortest time between two live edits sent while a slider is dragged.
const EDIT_INTERVAL: Duration = Duration::from_millis(50);

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("gprox")
            .with_app_id("gprox")
            .with_inner_size([720.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native(
        "gprox",
        options,
        Box::new(|_cc| {
            Ok(Box::new(GproxApp {
                screenshot: std::env::var_os("GPROX_GUI_SCREENSHOT").map(PathBuf::from),
                started: Some(Instant::now()),
                ..Default::default()
            }))
        }),
    )
}

#[derive(Debug, Default)]
struct GproxApp {
    status: Option<Status>,
    error: Option<String>,
    last_refresh: Option<Instant>,
    /// Curve cache: the preset it was computed for and its points (f, dB).
    curve: Option<(HeadphoneEqPreset, Vec<(f64, f64)>)>,
    /// Sidetone slider value while it is being dragged (sent on release).
    sidetone_drag: Option<u8>,
    /// Pending self-screenshot (`GPROX_GUI_SCREENSHOT`).
    screenshot: Option<PathBuf>,
    started: Option<Instant>,
    /// Kept-open connection to gproxd (re-opened when it breaks).
    client: Option<Client>,
    /// Headphone EQ values being edited, shown instead of the daemon's.
    hp_draft: Option<HeadphoneEqPreset>,
    /// `hp_draft` has changes gproxd hasn't got yet.
    hp_unsent: bool,
    /// Blue VO!CE values being edited, shown instead of the daemon's.
    bv_draft: BTreeMap<String, f32>,
    /// Edited Blue VO!CE values gproxd hasn't got yet.
    bv_unsent: BTreeMap<String, f32>,
    /// When the last live edit was sent.
    last_edit: Option<Instant>,
    /// Open dialog (save as, overwrite, delete).
    dialog: Option<Dialog>,
}

/// Which preset family a dialog is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    HpEq,
    BlueVoice,
}

#[derive(Debug, Clone, PartialEq)]
enum Dialog {
    SaveAs { kind: Kind, name: String },
    Overwrite { kind: Kind, name: String },
    Delete { kind: Kind, name: String },
}

/// Preset names as gproxd matches them: case-insensitive, spaces, dashes and
/// underscores alike.
fn same_name(a: &str, b: &str) -> bool {
    let n = |s: &str| s.trim().to_lowercase().replace([' ', '-'], "_");
    n(a) == n(b)
}

/// Write an image as binary PPM (no image crate needed).
fn write_ppm(path: &Path, img: &egui::ColorImage) -> std::io::Result<()> {
    let [w, h] = img.size;
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for c in &img.pixels {
        out.extend_from_slice(&c.to_srgba_unmultiplied()[..3]);
    }
    std::fs::write(path, out)
}

fn settings(p: &HeadphoneEqPreset) -> HpEqSettings {
    HpEqSettings {
        mode: match p.mode {
            gprox_core::EqMode::Basic => EqMode::Basic,
            gprox_core::EqMode::Advanced => EqMode::Advanced,
        },
        basic: p.basic,
        advanced: p.advanced,
    }
}

fn curve_points(p: &HeadphoneEqPreset) -> Vec<(f64, f64)> {
    let taps: [Taps; BANDS] = steady_state_taps(&settings(p), FS);
    (0..CURVE_POINTS)
        .map(|i| {
            let t = i as f64 / (CURVE_POINTS - 1) as f64;
            let f = F_MIN * (F_MAX / F_MIN).powf(t);
            (f, cascade_magnitude_db(&taps, f, FS))
        })
        .collect()
}

impl GproxApp {
    /// One request on the kept-open connection, or a new one if that is
    /// gone (local socket; 2 s timeouts). A refused request keeps the
    /// connection.
    fn request(&mut self, req: &Request) -> Result<Status, ControlError> {
        if let Some(c) = &mut self.client {
            match c.call(req) {
                Ok(s) => return Ok(s),
                Err(e @ ControlError::Daemon(_)) => return Err(e),
                // Broken (e.g. gproxd restarted, or closed an idle connection):
                // gproxd didn't act on it, so try once more on a new one.
                Err(_) => self.client = None,
            }
        }
        let mut c = Client::connect()?;
        let r = c.call(req);
        if matches!(r, Ok(_) | Err(ControlError::Daemon(_))) {
            self.client = Some(c);
        }
        r
    }

    /// Send a request; show its status, or its error. Returns whether it
    /// was accepted.
    fn call(&mut self, req: Request) -> bool {
        let r = self.request(&req);
        self.last_refresh = Some(Instant::now());
        match r {
            Ok(s) => {
                self.status = Some(s);
                self.error = None;
                true
            }
            Err(e) => {
                // Keep showing the last state for a failed change; drop it when
                // the daemon is gone.
                if matches!(req, Request::Status) {
                    self.status = None;
                }
                self.error = Some(e.to_string());
                false
            }
        }
    }

    /// Send the edits not sent yet: when `force`, or [`EDIT_INTERVAL`] after
    /// the last one. Drafts are dropped once everything is sent and no
    /// slider is held, so the daemon's state shows again.
    fn flush_edits(&mut self, ctx: &egui::Context, force: bool) {
        let due = force || self.last_edit.is_none_or(|t| t.elapsed() >= EDIT_INTERVAL);
        if self.hp_unsent || !self.bv_unsent.is_empty() {
            if !due {
                ctx.request_repaint_after(EDIT_INTERVAL);
                return;
            }
            self.last_edit = Some(Instant::now());
            if self.hp_unsent
                && let Some(preset) = self.hp_draft.clone()
            {
                self.hp_unsent = false;
                if !self.call(Request::EditHpEq { preset }) {
                    self.hp_draft = None;
                }
            }
            if !self.bv_unsent.is_empty() {
                let params = std::mem::take(&mut self.bv_unsent);
                if !self.call(Request::EditBluevoice { params }) {
                    self.bv_draft.clear();
                }
            }
        }
        if !ctx.input(|i| i.pointer.any_down()) {
            if !self.hp_unsent {
                self.hp_draft = None;
            }
            if self.bv_unsent.is_empty() {
                self.bv_draft.clear();
            }
        }
    }

    /// "Revert", "Save", "Save as…" and "Delete" for the selected preset.
    fn preset_buttons(
        &mut self,
        ui: &mut egui::Ui,
        kind: Kind,
        selected: Option<&str>,
        modified: bool,
        writable: &[String],
    ) {
        let own = selected.is_some_and(|n| writable.iter().any(|w| w == n));
        ui.horizontal(|ui| {
            if let Some(sel) = selected {
                if modified {
                    if ui
                        .button("Revert")
                        .on_hover_text("Drop the unsaved changes")
                        .clicked()
                    {
                        self.discard_drafts();
                        self.call(match kind {
                            Kind::HpEq => Request::SetHpEq {
                                name: sel.to_owned(),
                            },
                            Kind::BlueVoice => Request::SetBluevoice {
                                name: sel.to_owned(),
                            },
                        });
                    }
                    if own
                        && ui
                            .button("Save")
                            .on_hover_text(format!("Overwrite {sel:?}"))
                            .clicked()
                    {
                        self.save(&ui.ctx().clone(), kind, sel.to_owned());
                    }
                }
                if ui.button("Save as…").clicked() {
                    let name = if own || sel == FLAT_PRESET {
                        String::new()
                    } else {
                        format!("{sel} copy")
                    };
                    self.dialog = Some(Dialog::SaveAs { kind, name });
                }
                if own && ui.button("Delete").clicked() {
                    self.dialog = Some(Dialog::Delete {
                        kind,
                        name: sel.to_owned(),
                    });
                }
                if !own && sel != FLAT_PRESET {
                    ui.weak("read-only preset: save a copy to keep changes");
                }
            }
        });
    }

    fn discard_drafts(&mut self) {
        self.hp_draft = None;
        self.hp_unsent = false;
        self.bv_draft.clear();
        self.bv_unsent.clear();
    }

    /// Send pending edits, then save under `name`.
    fn save(&mut self, ctx: &egui::Context, kind: Kind, name: String) {
        self.flush_edits(ctx, true);
        self.call(match kind {
            Kind::HpEq => Request::SaveHpEq { name },
            Kind::BlueVoice => Request::SaveBluevoice { name },
        });
    }

    /// The modal dialog, if one is open.
    fn dialog_ui(&mut self, ctx: &egui::Context, s: &Status) {
        let Some(mut dialog) = self.dialog.clone() else {
            return;
        };
        enum Act {
            Keep,
            Close,
            Save(Kind, String),
            Delete(Kind, String),
        }
        let mut act = Act::Keep;
        let mut next = None;
        let modal = egui::Modal::new(egui::Id::unique("gprox-dialog")).show(ctx, |ui| {
            ui.set_width(340.0);
            match &mut dialog {
                Dialog::SaveAs { kind, name } => {
                    let (presets, writable) = match kind {
                        Kind::HpEq => (&s.hp_eq.presets, &s.hp_eq.writable),
                        Kind::BlueVoice => (&s.bluevoice.presets, &s.bluevoice.writable),
                    };
                    ui.heading("Save preset as");
                    let r = ui.text_edit_singleline(name);
                    if !r.has_focus() && !r.lost_focus() {
                        r.request_focus();
                    }
                    let clash = presets.iter().find(|p| same_name(p, name));
                    let read_only = clash.is_some_and(|c| !writable.contains(c));
                    if read_only {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "That name is a read-only preset (flat or from G HUB). Pick another.",
                        );
                    }
                    let ok = !name.trim().is_empty() && !read_only;
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        if ui.add_enabled(ok, egui::Button::new("Save")).clicked() || (enter && ok)
                        {
                            match clash {
                                Some(c) => {
                                    next = Some(Dialog::Overwrite {
                                        kind: *kind,
                                        name: c.clone(),
                                    })
                                }
                                None => act = Act::Save(*kind, name.trim().to_owned()),
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            act = Act::Close;
                        }
                    });
                }
                Dialog::Overwrite { kind, name } => {
                    ui.heading("Overwrite preset?");
                    ui.label(format!(
                        "{name:?} exists. Replace it with the current settings?"
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Overwrite").clicked() {
                            act = Act::Save(*kind, name.clone());
                        }
                        if ui.button("Cancel").clicked() {
                            act = Act::Close;
                        }
                    });
                }
                Dialog::Delete { kind, name } => {
                    ui.heading("Delete preset?");
                    ui.label(format!("Delete {name:?}? Its file is removed."));
                    ui.horizontal(|ui| {
                        if ui.button("Delete").clicked() {
                            act = Act::Delete(*kind, name.clone());
                        }
                        if ui.button("Cancel").clicked() {
                            act = Act::Close;
                        }
                    });
                }
            }
        });
        if let Some(d) = next {
            dialog = d;
        } else if modal.should_close() && matches!(act, Act::Keep) {
            act = Act::Close;
        }
        match act {
            Act::Keep => self.dialog = Some(dialog),
            Act::Close => self.dialog = None,
            Act::Save(kind, name) => {
                self.dialog = None;
                self.save(ctx, kind, name);
            }
            Act::Delete(kind, name) => {
                self.dialog = None;
                self.discard_drafts();
                self.call(match kind {
                    Kind::HpEq => Request::DeleteHpEq { name },
                    Kind::BlueVoice => Request::DeleteBluevoice { name },
                });
            }
        }
    }

    fn hp_eq_ui(&mut self, ui: &mut egui::Ui, s: &Status) {
        ui.heading("Headphone EQ");
        let hp = &s.hp_eq;
        let mut pick = None;
        ui.horizontal(|ui| {
            ui.label("Preset");
            let shown = if hp.modified || self.hp_draft.is_some() {
                format!("{} (modified)", hp.selected)
            } else {
                hp.selected.clone()
            };
            egui::ComboBox::from_id_salt("hp_eq")
                .selected_text(shown)
                .width(220.0)
                .show_ui(ui, |ui| {
                    for n in &hp.presets {
                        if ui.selectable_label(*n == hp.selected, n).clicked() {
                            pick = Some(n.clone());
                        }
                    }
                });
        });
        if let Some(name) = pick {
            self.discard_drafts();
            self.call(Request::SetHpEq { name });
            return;
        }
        let modified = hp.modified || self.hp_draft.as_ref().is_some_and(|d| *d != hp.preset);
        self.preset_buttons(ui, Kind::HpEq, Some(&hp.selected), modified, &hp.writable);

        let mut p = self.hp_draft.clone().unwrap_or_else(|| hp.preset.clone());
        let before = p.clone();
        ui.horizontal(|ui| {
            ui.label("Mode");
            ui.radio_value(&mut p.mode, gprox_core::EqMode::Basic, "Basic (2 band)");
            ui.radio_value(
                &mut p.mode,
                gprox_core::EqMode::Advanced,
                "Advanced (10 band)",
            );
        });
        if self.curve.as_ref().is_none_or(|(c, _)| *c != p) {
            self.curve = Some((p.clone(), curve_points(&p)));
        }
        if let Some((c, pts)) = &self.curve {
            curve_view(ui, c, pts);
        }
        let max = f64::from(HeadphoneEqPreset::MAX_GAIN_DB);
        let (gains, labels): (&mut [f32], &[&str]) = match p.mode {
            gprox_core::EqMode::Basic => (&mut p.basic, &HeadphoneEqPreset::BASIC_LABELS),
            gprox_core::EqMode::Advanced => (&mut p.advanced, &HeadphoneEqPreset::ADVANCED_LABELS),
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = 130.0;
            for (g, label) in gains.iter_mut().zip(labels) {
                ui.vertical(|ui| {
                    ui.set_width(58.0);
                    let r = ui
                        .add(
                            egui::Slider::new(g, -max as f32..=max as f32)
                                .vertical()
                                .custom_formatter(fmt_tenths),
                        )
                        .on_hover_text("Double-click: 0 dB");
                    if r.double_clicked() {
                        *g = 0.0;
                    }
                    ui.small(*label);
                });
            }
        });
        if p != before {
            // 0.1 dB steps, as exact as f32 allows (0.0 stays 0.0).
            for g in p.basic.iter_mut().chain(p.advanced.iter_mut()) {
                *g = (*g * 10.0).round() / 10.0;
            }
            self.hp_draft = Some(p);
            self.hp_unsent = true;
        }
    }

    fn bluevoice_ui(&mut self, ui: &mut egui::Ui, s: &Status) {
        ui.heading("Blue VO!CE");
        let bv = &s.bluevoice;
        if !bv.available {
            ui.label("Not available: start gproxd once with --ghub-presets <G HUB preset file>.");
            return;
        }
        let mut req = None;
        ui.horizontal(|ui| {
            let mut on = bv.enabled;
            if ui.checkbox(&mut on, "On").changed() {
                req = Some(Request::BluevoiceEnable { on });
            }
            ui.label("Preset");
            let sel = match (&bv.selected, bv.modified || !self.bv_draft.is_empty()) {
                (Some(n), true) => format!("{n} (modified)"),
                (Some(n), false) => n.clone(),
                (None, _) => "(none)".to_owned(),
            };
            egui::ComboBox::from_id_salt("bluevoice")
                .selected_text(sel)
                .width(220.0)
                .show_ui(ui, |ui| {
                    for n in &bv.presets {
                        if ui
                            .selectable_label(bv.selected.as_ref() == Some(n), n)
                            .clicked()
                        {
                            req = Some(Request::SetBluevoice { name: n.clone() });
                        }
                    }
                });
        });
        if let Some(r) = req {
            if matches!(r, Request::SetBluevoice { .. }) {
                self.discard_drafts();
            }
            self.call(r);
            return;
        }
        if bv.params.is_empty() {
            if bv.selected.is_some() {
                ui.weak("This gproxd can't edit presets (update gproxd).");
            }
            return;
        }
        let modified = bv.modified
            || self
                .bv_draft
                .iter()
                .any(|(k, v)| bv.params.get(k) != Some(v));
        self.preset_buttons(
            ui,
            Kind::BlueVoice,
            bv.selected.as_deref(),
            modified,
            &bv.writable,
        );
        if !bv.enabled {
            ui.weak("Blue VO!CE is off: changes are heard once it is on.");
        }
        let value = |app: &Self, name: &str| -> Option<f32> {
            app.bv_draft
                .get(name)
                .or_else(|| bv.params.get(name))
                .copied()
        };
        let mut changed: Vec<(&'static str, f32)> = Vec::new();
        for m in bv_params::MODULES {
            let title = match m.enable.and_then(|e| value(self, e)) {
                Some(v) if v != 0.0 => format!("{} (on)", m.title),
                Some(_) => format!("{} (off)", m.title),
                None => m.title.to_owned(),
            };
            egui::CollapsingHeader::new(title)
                .id_salt(m.title)
                .default_open(false)
                .show(ui, |ui| {
                    if let Some(e) = m.enable
                        && let Some(v) = value(self, e)
                    {
                        let mut on = v != 0.0;
                        if ui.checkbox(&mut on, "On").changed() {
                            changed.push((e, f32::from(u8::from(on))));
                        }
                    }
                    egui::Grid::new(m.title)
                        .num_columns(2)
                        .min_col_width(130.0)
                        .show(ui, |ui| {
                            for prm in m.params {
                                let (Some(v), Some((lo, hi))) =
                                    (value(self, prm.name), param_range(prm.name))
                                else {
                                    continue;
                                };
                                ui.label(prm.label);
                                if let Some(nv) = param_slider(ui, prm.unit, v, lo, hi)
                                    && nv != v
                                {
                                    changed.push((prm.name, nv));
                                }
                                ui.end_row();
                            }
                        });
                });
        }
        for (name, v) in changed {
            self.bv_draft.insert(name.to_owned(), v);
            self.bv_unsent.insert(name.to_owned(), v);
        }
    }
}

/// One decimal, without "-0.0".
fn fmt_tenths(v: f64, _: std::ops::RangeInclusive<usize>) -> String {
    let v = (v * 10.0).round() / 10.0;
    format!("{:.1}", if v == 0.0 { 0.0 } else { v })
}

/// One Blue VO!CE parameter's control; the new value if it was changed.
fn param_slider(ui: &mut egui::Ui, unit: bv_params::Unit, v: f32, lo: f32, hi: f32) -> Option<f32> {
    use bv_params::Unit;
    ui.spacing_mut().slider_width = 260.0;
    match unit {
        Unit::Switch => {
            let mut on = v != 0.0;
            ui.checkbox(&mut on, "")
                .changed()
                .then(|| f32::from(u8::from(on)))
        }
        Unit::LinDb { floor_db } => {
            let (a, b) = bv_params::db_range(lo, hi, floor_db);
            let mut db = bv_params::lin_to_db(v).clamp(a, b);
            // Rounded to 0.1 dB here, not with step_by (which counts from
            // the range's lower end, e.g. −15.000009 dB, so 0 dB would be
            // sent as 0.99999).
            let r = ui.add(
                egui::Slider::new(&mut db, a..=b)
                    .custom_formatter(fmt_tenths)
                    .suffix(" dB"),
            );
            r.changed().then(|| {
                let db = (db * 10.0).round() / 10.0;
                bv_params::db_value_to_lin(db, lo, hi, floor_db)
            })
        }
        _ => {
            let mut x = v.clamp(lo, hi);
            let (suffix, decimals, log) = match unit {
                Unit::Db => (" dB", 1, false),
                Unit::Hz => (" Hz", 0, true),
                Unit::Ms => (" ms", if hi <= 10.0 { 2 } else { 1 }, false),
                Unit::Ratio => (":1", 1, false),
                _ => ("", 2, false),
            };
            let r = ui.add(
                egui::Slider::new(&mut x, lo..=hi)
                    .logarithmic(log)
                    .fixed_decimals(decimals)
                    .suffix(suffix),
            );
            r.changed().then(|| x.clamp(lo, hi))
        }
    }
}

impl GproxApp {
    fn headset_ui(&mut self, ui: &mut egui::Ui, s: &Status) {
        ui.heading("Headset");
        let h = &s.headset;
        ui.label(h.summary());
        let mut req = None;
        ui.horizontal(|ui| {
            if h.connected {
                ui.label("Sidetone");
                let mut v = self.sidetone_drag.or(h.sidetone).unwrap_or(0);
                let r = ui.add(egui::Slider::new(&mut v, 0..=100));
                if r.dragged() || r.changed() {
                    self.sidetone_drag = Some(v);
                }
                // Send once the slider is let go (or changed by keyboard/click),
                // not on every drag step: each value is one HID++ write.
                if (r.drag_stopped() || (r.changed() && !r.dragged()))
                    && let Some(level) = self.sidetone_drag.take()
                    && Some(level) != h.sidetone
                {
                    req = Some(Request::SetSidetone { level });
                }
            }
            // No HID involved, so it can be set with the headset off too.
            let mut always = h.always_on_sidetone;
            if ui
                .checkbox(
                    &mut always,
                    if h.connected {
                        "Always on"
                    } else {
                        "Always-on sidetone"
                    },
                )
                .on_hover_text(
                    "Always-on sidetone: keep the headset's mic stream open so sidetone works \
                     even when no app uses the mic. The headset only mixes sidetone while its \
                     mic streams.",
                )
                .changed()
            {
                req = Some(Request::SetAlwaysOnSidetone { on: always });
            }
        });
        if !h.connected {
            if let Some(r) = req {
                self.call(r);
            }
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Auto-off");
            let label = |m: u8| {
                if m == 0 {
                    "never".to_owned()
                } else {
                    format!("{m} min")
                }
            };
            let cur = h.auto_off_minutes;
            egui::ComboBox::from_id_salt("auto_off")
                .selected_text(cur.map_or("?".to_owned(), label))
                .show_ui(ui, |ui| {
                    for m in [1u8, 2, 5, 10, 15, 30, 0] {
                        if ui.selectable_label(cur == Some(m), label(m)).clicked() && cur != Some(m)
                        {
                            req = Some(Request::SetAutoOff { minutes: m });
                        }
                    }
                });
        });
        if let Some(r) = req {
            self.call(r);
        }
    }
}

/// The EQ magnitude curve on a log-frequency axis, ±15 dB.
fn curve_view(ui: &mut egui::Ui, preset: &HeadphoneEqPreset, pts: &[(f64, f64)]) {
    let width = ui.available_width();
    let height = (ui.available_height() - 140.0).clamp(160.0, 360.0);
    let (resp, painter) = ui.allocate_painter(Vec2::new(width, height), Sense::hover());
    let outer = resp.rect;
    let plot = Rect::from_min_max(
        outer.min + Vec2::new(44.0, 8.0),
        outer.max - Vec2::new(10.0, 22.0),
    );
    let visuals = ui.visuals();
    let grid = Stroke::new(1.0, visuals.weak_text_color().gamma_multiply(0.35));
    let axis = Stroke::new(1.0, visuals.weak_text_color());
    let text = visuals.text_color();
    let font = FontId::proportional(11.0);
    let x_of = |f: f64| {
        let t = (f / F_MIN).ln() / (F_MAX / F_MIN).ln();
        plot.left() + t as f32 * plot.width()
    };
    let y_of = |db: f64| {
        let t = (db.clamp(-DB_RANGE, DB_RANGE) + DB_RANGE) / (2.0 * DB_RANGE);
        plot.bottom() - t as f32 * plot.height()
    };
    painter.rect_filled(plot, 2.0, visuals.extreme_bg_color);
    for (f, label) in [
        (20.0, "20"),
        (50.0, "50"),
        (100.0, "100"),
        (200.0, "200"),
        (500.0, "500"),
        (1000.0, "1k"),
        (2000.0, "2k"),
        (5000.0, "5k"),
        (10_000.0, "10k"),
        (20_000.0, "20k"),
    ] {
        let x = x_of(f);
        painter.line_segment(
            [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
            grid,
        );
        painter.text(
            Pos2::new(x, plot.bottom() + 3.0),
            Align2::CENTER_TOP,
            label,
            font.clone(),
            text,
        );
    }
    for db in (-15..=15).step_by(3) {
        let y = y_of(f64::from(db));
        let s = if db == 0 { axis } else { grid };
        painter.line_segment([Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)], s);
        if db % 6 == 0 {
            painter.text(
                Pos2::new(plot.left() - 4.0, y),
                Align2::RIGHT_CENTER,
                format!("{db:+} dB"),
                font.clone(),
                text,
            );
        }
    }
    let accent = visuals.selection.bg_fill;
    let line: Vec<Pos2> = pts
        .iter()
        .map(|&(f, db)| Pos2::new(x_of(f), y_of(db)))
        .collect();
    painter.line(line, Stroke::new(2.0, accent));
    // Band markers of the active set at their stored gains.
    let s = settings(preset);
    let targets = s.targets();
    let active = match s.mode {
        EqMode::Advanced => gprox_dsp::hp_eq::ADVANCED_BANDS,
        EqMode::Basic => gprox_dsp::hp_eq::BASIC_BANDS,
    };
    for k in active {
        let f = f64::from(BAND_TABLE[k].f0);
        let p = Pos2::new(x_of(f), y_of(f64::from(targets[k])));
        painter.circle_filled(p, 3.5, Color32::from_gray(200));
    }
    // Hover readout.
    if let Some(pos) = resp.hover_pos().filter(|p| plot.contains(*p)) {
        let t = f64::from((pos.x - plot.left()) / plot.width());
        let f = F_MIN * (F_MAX / F_MIN).powf(t);
        let i = ((t * (CURVE_POINTS - 1) as f64).round() as usize).min(pts.len() - 1);
        painter.text(
            Pos2::new(plot.right() - 4.0, plot.top() + 4.0),
            Align2::RIGHT_TOP,
            format!("{f:.0} Hz: {:+.2} dB", pts[i].1),
            font,
            text,
        );
    }
}

impl eframe::App for GproxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.last_refresh.is_none_or(|t| t.elapsed() >= REFRESH) {
            self.call(Request::Status);
        }
        ui.ctx().request_repaint_after(REFRESH);
        if self
            .started
            .is_some_and(|t| t.elapsed() > Duration::from_millis(1500))
            && let Some(path) = self.screenshot.take()
        {
            let ctx = ui.ctx().clone();
            let cb = egui::ScreenshotCallback::new(move |img| {
                match write_ppm(&path, &img) {
                    Ok(()) => eprintln!("gprox-gui: screenshot saved to {}", path.display()),
                    Err(e) => eprintln!("gprox-gui: screenshot {}: {e}", path.display()),
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                ctx.request_repaint();
            });
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Screenshot(cb));
        } else if self.screenshot.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| {
            match self.status.clone() {
                Some(s) => {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        self.hp_eq_ui(ui, &s);
                        ui.add_space(12.0);
                        ui.separator();
                        self.bluevoice_ui(ui, &s);
                        ui.add_space(12.0);
                        ui.separator();
                        self.headset_ui(ui, &s);
                    });
                    self.flush_edits(&ctx, false);
                    self.dialog_ui(&ctx, &s);
                }
                None => {
                    ui.heading("gprox");
                    ui.label("gproxd is not running.");
                    if ui.button("Retry").clicked() {
                        self.call(Request::Status);
                    }
                }
            }
            if let Some(e) = &self.error {
                ui.add_space(8.0);
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
        });
    }
}
