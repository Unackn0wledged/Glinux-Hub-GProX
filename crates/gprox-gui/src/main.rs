//! `gprox-gui`: a small, functional front-end for a running `gproxd`.
//!
//! Headphone EQ preset picker with the EQ's magnitude curve, Blue VO!CE on/off
//! and preset picker, and the headset (battery, mic mute, sidetone, auto-off). Everything goes through the control socket
//! (`gprox_core::control`); the GUI never touches audio. The curve uses the
//! DSP's own coefficient design (`gprox_dsp::steady_state_taps`), so it shows
//! exactly what the render path converges to.
//!
//! `GPROX_GUI_SCREENSHOT=<file.ppm>`: save a screenshot of this window (only
//! this window) shortly after start and exit. For docs and bug reports.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use gprox_core::HeadphoneEqPreset;
use gprox_core::control::{Client, Request, Status};
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

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("gprox")
            .with_app_id("gprox")
            .with_inner_size([640.0, 640.0]),
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
    /// One request on a fresh connection (local socket; 2 s timeouts).
    fn call(&mut self, req: Request) {
        let r = Client::connect().and_then(|mut c| c.call(&req));
        self.last_refresh = Some(Instant::now());
        match r {
            Ok(s) => {
                self.status = Some(s);
                self.error = None;
            }
            Err(e) => {
                // Keep showing the last state for a failed change; drop it when
                // the daemon is gone.
                if matches!(req, Request::Status) {
                    self.status = None;
                }
                self.error = Some(e.to_string());
            }
        }
    }

    fn hp_eq_ui(&mut self, ui: &mut egui::Ui, s: &Status) {
        ui.heading("Headphone EQ");
        let mut pick = None;
        ui.horizontal(|ui| {
            ui.label("Preset");
            egui::ComboBox::from_id_salt("hp_eq")
                .selected_text(&s.hp_eq.selected)
                .width(220.0)
                .show_ui(ui, |ui| {
                    for n in &s.hp_eq.presets {
                        if ui.selectable_label(*n == s.hp_eq.selected, n).clicked() {
                            pick = Some(n.clone());
                        }
                    }
                });
            let p = &s.hp_eq.preset;
            ui.weak(match p.mode {
                gprox_core::EqMode::Basic => format!(
                    "basic: bass {:+.1} dB, treble {:+.1} dB",
                    p.basic[0], p.basic[1]
                ),
                gprox_core::EqMode::Advanced => "advanced (10 band)".to_owned(),
            });
        });
        if let Some(name) = pick {
            self.call(Request::SetHpEq { name });
        }
        let preset = &s.hp_eq.preset;
        if self.curve.as_ref().is_none_or(|(p, _)| p != preset) {
            self.curve = Some((preset.clone(), curve_points(preset)));
        }
        if let Some((p, pts)) = &self.curve {
            curve_view(ui, p, pts);
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
            let sel = bv.selected.as_deref().unwrap_or("(none)");
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
            self.call(r);
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
        egui::CentralPanel::default().show(ui, |ui| {
            match self.status.clone() {
                Some(s) => {
                    self.hp_eq_ui(ui, &s);
                    ui.add_space(12.0);
                    ui.separator();
                    self.bluevoice_ui(ui, &s);
                    ui.add_space(12.0);
                    ui.separator();
                    self.headset_ui(ui, &s);
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
