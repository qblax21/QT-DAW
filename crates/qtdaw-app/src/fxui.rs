//! Effect editor windows: one floating window per open insert, with a knob per setting,
//! a gain-reduction meter for dynamics, and a draggable curve for the EQ.

use eframe::egui::{self, Align, Align2, Color32, ComboBox, FontId, Layout, Pos2, Rect, RichText, Sense, Shape, Stroke, Vec2};
use qtdaw_engine::fx::eq::{self, BANDS, HIGH_CUT, HIGH_FREQ, HIGH_GAIN, LOW_CUT, LOW_FREQ, LOW_GAIN, MID1_FREQ, MID1_GAIN, MID1_Q, MID2_FREQ, MID2_GAIN, MID2_Q};
use qtdaw_engine::fx::{EffectKind, ParamSpec, Params, Unit};

use crate::app::QtDaw;
use crate::mixer::knob;
use crate::ui::{ACCENT, GRID_BAR, LCD, SOLO, TEXT_DIM};

const BAND_COLORS: [Color32; 6] = [
    Color32::from_rgb(0xb0, 0x80, 0xff),
    Color32::from_rgb(0xff, 0x80, 0x60),
    Color32::from_rgb(0xf0, 0xc0, 0x40),
    Color32::from_rgb(0x50, 0xd0, 0x80),
    Color32::from_rgb(0x4d, 0xa3, 0xff),
    Color32::from_rgb(0xd0, 0x60, 0xc0),
];

impl QtDaw {
    /// Draw every open effect window.
    pub fn fx_windows(&mut self, ctx: &egui::Context) {
        let ids = self.view.open_fx.clone();
        let mut close = Vec::new();
        for (n, id) in ids.into_iter().enumerate() {
            let Some(owner) = self.insert_owner_name(id) else {
                close.push(id); // its track or bus was deleted
                continue;
            };
            let Some(kind) = self.session.insert_mut(id).map(|m| m.kind) else { continue };
            let mut open = true;
            egui::Window::new(format!("{owner} · {}", kind.name()))
                .id(egui::Id::unique(("fx-window", id)))
                .open(&mut open)
                .collapsible(true)
                .resizable(false)
                .default_pos(Pos2::new(420.0 + 60.0 * (n % 6) as f32, 170.0 + 50.0 * (n % 6) as f32))
                .show(ctx, |ui| self.fx_editor(ui, id));
            if !open {
                close.push(id);
            }
        }
        self.view.open_fx.retain(|id| !close.contains(id));
    }

    /// "Vocal" for an insert on the Vocal track, "Master" for the master...
    fn insert_owner_name(&self, id: u64) -> Option<String> {
        let s = &self.session;
        if let Some(t) = s.tracks.iter().find(|t| t.inserts.iter().any(|i| i.id == id)) {
            return Some(t.name.clone());
        }
        if let Some(b) = s.buses.iter().find(|b| b.inserts.iter().any(|i| i.id == id)) {
            return Some(b.name.clone());
        }
        s.master_inserts.iter().any(|i| i.id == id).then(|| "Master".to_string())
    }

    fn fx_editor(&mut self, ui: &mut egui::Ui, id: u64) {
        let sr = self.session.sample_rate as f32;
        let reduction = self.view.fx_reduction.get(&id).copied().unwrap_or(0.0);
        let Some(model) = self.session.insert_mut(id) else { return };
        let kind = model.kind;
        let specs = kind.params();
        let mut p = model.values();
        let before = p;
        let mut bypass = model.bypass;

        ui.horizontal(|ui| {
            let label = if bypass { "Bypassed" } else { "On" };
            let color = if bypass { Color32::from_gray(0x50) } else { ACCENT };
            if ui
                .add(egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(color))
                .on_hover_text("Turn the effect off without removing it, to compare")
                .clicked()
            {
                bypass = !bypass;
            }
            if kind.shows_reduction() {
                reduction_meter(ui, reduction);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Reset").on_hover_text("All settings back to default").clicked() {
                    p = kind.defaults();
                }
            });
        });
        ui.label(RichText::new(kind.description()).small().color(TEXT_DIM));
        ui.add_space(4.0);

        if kind == EffectKind::Eq {
            eq_graph(ui, id, &mut p, sr);
            ui.add_space(4.0);
            ui.horizontal_top(|ui| {
                for (band, (name, params)) in BANDS.iter().enumerate() {
                    ui.allocate_ui_with_layout(Vec2::new(66.0, 230.0), Layout::top_down(Align::Center), |ui| {
                        ui.label(RichText::new(*name).strong().color(BAND_COLORS[band]));
                        for &i in *params {
                            // The column title already names the band.
                            let label = match specs[i].unit {
                                Unit::Hz => "Freq",
                                Unit::Db => "Gain",
                                _ => "Q",
                            };
                            param_control(ui, (id, i), label, &specs[i], &mut p[i], BAND_COLORS[band]);
                        }
                    });
                }
            });
        } else {
            ui.horizontal_top(|ui| {
                for (i, spec) in specs.iter().enumerate() {
                    ui.allocate_ui_with_layout(Vec2::new(70.0, 90.0), Layout::top_down(Align::Center), |ui| {
                        param_control(ui, (id, i), spec.label, spec, &mut p[i], ACCENT);
                    });
                }
            });
        }
        ui.label(RichText::new("Drag a knob up/down (Shift = fine). Double-click resets it.").small().color(TEXT_DIM));

        let changed = p != before || bypass != model.bypass;
        if changed {
            for i in 0..specs.len() {
                if p[i] != before[i] {
                    model.set(i, p[i]);
                }
            }
            model.bypass = bypass;
            self.dirty = true;
        }
    }
}

/// Label, knob (or a drop-down for choices) and value for one setting.
fn param_control(ui: &mut egui::Ui, salt: (u64, usize), label: &str, spec: &ParamSpec, v: &mut f32, color: Color32) {
    ui.label(RichText::new(label).small().color(TEXT_DIM));
    match spec.unit {
        Unit::Choice(names) => {
            let mut idx = (v.round() as usize).min(names.len() - 1);
            ComboBox::from_id_salt(("fx-choice", salt)).width(64.0).truncate().selected_text(names[idx]).show_ui(ui, |ui| {
                for (i, name) in names.iter().enumerate() {
                    ui.selectable_value(&mut idx, i, *name);
                }
            });
            *v = idx as f32;
            ui.add_space(14.0);
        }
        _ => {
            let mut n = spec.to_norm(*v);
            let bipolar = spec.min < 0.0 && spec.max > 0.0;
            let r = knob(ui, &mut n, spec.to_norm(spec.default), 38.0, bipolar, color)
                .on_hover_text(format!("{}: {}\nDefault {}", spec.label, spec.format(*v), spec.format(spec.default)));
            if r.changed() {
                *v = spec.from_norm(n);
            }
            ui.label(RichText::new(spec.format(*v)).small());
        }
    }
}

/// How much a dynamics effect is turning the signal down right now.
fn reduction_meter(ui: &mut egui::Ui, db: f32) {
    ui.label(RichText::new("Reduction").small().color(TEXT_DIM));
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(120.0, 12.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, LCD);
    let frac = (db / 24.0).clamp(0.0, 1.0);
    if frac > 0.0 {
        // Grows from the right, like a console's gain-reduction meter.
        let r = Rect::from_x_y_ranges((rect.right() - frac * rect.width())..=rect.right(), rect.y_range());
        p.rect_filled(r, 2.0, SOLO);
    }
    ui.label(RichText::new(format!("-{db:.1} dB")).small().monospace());
    resp.on_hover_text("Gain reduction: how many dB the effect is turning the signal down");
}

// ------------------------------------------------------------------------- EQ curve

const F_LO: f32 = 20.0;
const F_HI: f32 = 20_000.0;
const DB_RANGE: f32 = 20.0;

fn freq_to_x(rect: Rect, f: f32) -> f32 {
    rect.left() + (f / F_LO).ln() / (F_HI / F_LO).ln() * rect.width()
}

fn x_to_freq(rect: Rect, x: f32) -> f32 {
    F_LO * (F_HI / F_LO).powf(((x - rect.left()) / rect.width()).clamp(0.0, 1.0))
}

fn db_to_y(rect: Rect, db: f32) -> f32 {
    rect.center().y - db.clamp(-DB_RANGE, DB_RANGE) / DB_RANGE * (rect.height() / 2.0 - 6.0)
}

fn y_to_db(rect: Rect, y: f32) -> f32 {
    (rect.center().y - y) / (rect.height() / 2.0 - 6.0) * DB_RANGE
}

/// The EQ's frequency response, with a handle per band. Drag a handle to move its
/// frequency (left/right) and gain (up/down); scroll over a bell to change its width.
fn eq_graph(ui: &mut egui::Ui, id: u64, p: &mut Params, sr: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(6.0 * 66.0 + 5.0 * ui.spacing().item_spacing.x, 160.0), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, LCD);
    let font = FontId::proportional(9.0);
    for f in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0] {
        let x = freq_to_x(rect, f);
        painter.vline(x, rect.y_range(), Stroke::new(1.0, GRID_BAR));
        let label = if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
        painter.text(Pos2::new(x + 2.0, rect.bottom() - 2.0), Align2::LEFT_BOTTOM, label, font.clone(), TEXT_DIM);
    }
    for db in [-12.0, -6.0, 0.0, 6.0, 12.0] {
        let y = db_to_y(rect, db);
        let stroke = if db == 0.0 { Stroke::new(1.0, TEXT_DIM.gamma_multiply(0.6)) } else { Stroke::new(1.0, GRID_BAR) };
        painter.hline(rect.x_range(), y, stroke);
        painter.text(Pos2::new(rect.left() + 3.0, y - 1.0), Align2::LEFT_BOTTOM, format!("{db:+}"), font.clone(), TEXT_DIM);
    }

    let stages = eq::stages(p, sr);
    let points: Vec<Pos2> = (0..=240)
        .map(|i| {
            let x = rect.left() + rect.width() * i as f32 / 240.0;
            Pos2::new(x, db_to_y(rect, eq::response_db(&stages, x_to_freq(rect, x), sr)))
        })
        .collect();
    // Light shading between the curve and 0 dB.
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        let zero = rect.center().y;
        painter.add(Shape::convex_polygon(
            vec![a, b, Pos2::new(b.x, zero), Pos2::new(a.x, zero)],
            ACCENT.gamma_multiply(0.12),
            Stroke::NONE,
        ));
    }
    painter.add(Shape::line(points, Stroke::new(2.0, ACCENT)));

    // (band, freq param, gain param or None for the cut filters, q param)
    let handles: [(usize, usize, Option<usize>, Option<usize>); 6] = [
        (0, LOW_CUT, None, None),
        (1, LOW_FREQ, Some(LOW_GAIN), None),
        (2, MID1_FREQ, Some(MID1_GAIN), Some(MID1_Q)),
        (3, MID2_FREQ, Some(MID2_GAIN), Some(MID2_Q)),
        (4, HIGH_FREQ, Some(HIGH_GAIN), None),
        (5, HIGH_CUT, None, None),
    ];
    let specs = EffectKind::Eq.params();
    for (band, fi, gi, qi) in handles {
        let gain = gi.map_or(0.0, |g| p[g]);
        let at = Pos2::new(freq_to_x(rect, p[fi]).clamp(rect.left() + 6.0, rect.right() - 6.0), db_to_y(rect, gain));
        let resp = ui.interact(Rect::from_center_size(at, Vec2::splat(16.0)), egui::Id::unique(("eq-handle", id, band)), Sense::drag());
        if resp.dragged() {
            if let Some(pos) = resp.interact_pointer_pos() {
                p[fi] = specs[fi].clamp(x_to_freq(rect, pos.x));
                if let Some(g) = gi {
                    p[g] = specs[g].clamp((y_to_db(rect, pos.y) * 2.0).round() / 2.0);
                }
            }
        }
        if let Some(q) = qi {
            if resp.hovered() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    p[q] = specs[q].clamp(p[q] * (1.0 + scroll * 0.004));
                }
            }
        }
        let color = BAND_COLORS[band];
        let active = match gi {
            Some(g) => p[g].abs() > 0.05,
            None => (fi == LOW_CUT && p[fi] > eq::LOW_CUT_OFF + 0.5) || (fi == HIGH_CUT && p[fi] < eq::HIGH_CUT_OFF - 1.0),
        };
        let r = if resp.hovered() || resp.dragged() { 7.0 } else { 5.5 };
        if active {
            painter.circle_filled(at, r, color);
        } else {
            painter.circle_stroke(at, r, Stroke::new(1.5, color));
        }
        resp.on_hover_text(format!(
            "{}: {}{}{}",
            BANDS[band].0,
            specs[fi].format(p[fi]),
            gi.map_or(String::new(), |g| format!(", {}", specs[g].format(p[g]))),
            if qi.is_some() { "\nScroll to change the width (Q)" } else { "" }
        ));
    }
}
