//! The mixer view (press X): a channel strip per track, then the buses, then the master.
//! Also the fader and knob widgets used here and in the track headers.
//!
//! A strip, top to bottom: input, audio FX (inserts), sends, output, pan, volume readout, fader + meter,
//! mute / solo (/ record), name. Every strip uses the same row heights, so the faders
//! line up across the whole mixer.

use eframe::egui::{
    self, Align, Align2, Color32, ComboBox, FontId, Layout, Painter, Pos2, Rect, Response, Sense, Shape, Stroke,
    TextEdit, UiBuilder, Vec2,
};
use qtdaw_engine::engine::db_to_gain;
use qtdaw_engine::fx::EffectKind;
use qtdaw_engine::fx::presets::{CHAIN_PRESETS, PresetFor};
use qtdaw_engine::session::{InsertModel, MASTER, MAX_INSERTS, SendModel};
use qtdaw_engine::to_dbfs;

use crate::app::{Chain, MeterState, QtDaw};
use crate::ui::{ACCENT, BG, HEADER, LCD, MUTE, PLAY, REC, SOLO, TEXT_DIM, small_toggle, track_color};

// ------------------------------------------------------------------------- dB <-> fader position

/// Fader bottom. Anything at or below this is silence ("-∞").
pub const MIN_DB: f32 = -96.0;
/// Fader top.
pub const MAX_DB: f32 = 6.0;

/// Fader travel (0..1) for a level in dB. Uses the 4th root of the gain, which spreads the
/// useful range (-30..+6 dB) over most of the fader like a real console; 0 dB sits at 84%.
pub fn db_to_pos(db: f32) -> f32 {
    if db <= MIN_DB { 0.0 } else { (db_to_gain(db.min(MAX_DB)) / 2.0).powf(0.25).min(1.0) }
}

/// The inverse of [`db_to_pos`].
pub fn pos_to_db(pos: f32) -> f32 {
    if pos <= 0.0 { MIN_DB } else { (20.0 * (2.0 * pos.powi(4)).log10()).clamp(MIN_DB, MAX_DB) }
}

pub fn fmt_db(db: f32) -> String {
    if db <= MIN_DB + 0.05 { "-∞".into() } else { format!("{db:.1}") }
}

/// Fader cap length; the travel is inset by half of it at each end.
const CAP: f32 = 14.0;

/// Screen coordinate of fader position `pos` inside `rect` (shared by fader, scale and
/// meters, so they line up).
fn axis(rect: Rect, pos: f32, vertical: bool) -> f32 {
    if vertical {
        rect.bottom() - CAP / 2.0 - pos * (rect.height() - CAP)
    } else {
        rect.left() + CAP / 2.0 + pos * (rect.width() - CAP)
    }
}

// ------------------------------------------------------------------------- widgets

/// A volume fader in dB. Drag to move (hold Shift for fine moves), double-click for 0 dB.
pub fn fader(ui: &mut egui::Ui, db: &mut f32, size: Vec2, vertical: bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let travel = (if vertical { rect.height() } else { rect.width() }) - CAP;
    if resp.double_clicked() {
        *db = 0.0;
        resp.mark_changed();
    } else if resp.dragged() {
        let d = resp.drag_delta();
        let mut delta = (if vertical { -d.y } else { d.x }) / travel.max(1.0);
        if ui.input(|i| i.modifiers.shift) {
            delta *= 0.1;
        }
        if delta != 0.0 {
            *db = pos_to_db((db_to_pos(*db) + delta).clamp(0.0, 1.0));
            resp.mark_changed();
        }
    }

    let p = ui.painter();
    let at = axis(rect, db_to_pos(*db), vertical);
    let zero = axis(rect, db_to_pos(0.0), vertical);
    let cap_color = Color32::from_gray(if resp.hovered() || resp.dragged() { 0xe0 } else { 0xb8 });
    if vertical {
        let x = rect.center().x;
        p.rect_filled(Rect::from_center_size(rect.center(), Vec2::new(4.0, travel)), 2.0, LCD);
        p.hline((x - 6.0)..=(x + 6.0), zero, Stroke::new(1.0, TEXT_DIM));
        let cap = Rect::from_center_size(Pos2::new(x, at), Vec2::new(rect.width().min(30.0), CAP));
        p.rect_filled(cap, 3.0, cap_color);
        p.hline((cap.left() + 4.0)..=(cap.right() - 4.0), at, Stroke::new(1.5, Color32::from_gray(0x30)));
    } else {
        let y = rect.center().y;
        p.rect_filled(Rect::from_center_size(rect.center(), Vec2::new(travel, 4.0)), 2.0, LCD);
        p.vline(zero, (y - 5.0)..=(y + 5.0), Stroke::new(1.0, TEXT_DIM));
        let cap = Rect::from_center_size(Pos2::new(at, y), Vec2::new(10.0, rect.height().min(18.0)));
        p.rect_filled(cap, 2.0, cap_color);
        p.vline(at, (cap.top() + 3.0)..=(cap.bottom() - 3.0), Stroke::new(1.5, Color32::from_gray(0x30)));
    }
    resp.on_hover_text(format!("{} dB\nDrag (Shift = fine), double-click = 0 dB", fmt_db(*db)))
}

/// A round knob for a 0..1 value. Drag up/down (Shift = fine), double-click = `default`.
/// `bipolar` knobs (pan) light up from the middle.
pub fn knob(ui: &mut egui::Ui, value: &mut f32, default: f32, size: f32, bipolar: bool, color: Color32) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
    if resp.double_clicked() {
        *value = default;
        resp.mark_changed();
    } else if resp.dragged() {
        let mut d = -resp.drag_delta().y / 150.0;
        if ui.input(|i| i.modifiers.shift) {
            d *= 0.1;
        }
        if d != 0.0 {
            *value = (*value + d).clamp(0.0, 1.0);
            resp.mark_changed();
        }
    }

    let p = ui.painter();
    let c = rect.center();
    let r = size / 2.0 - 2.0;
    // 0 sits at bottom-left, 1 at bottom-right, sweeping clockwise over the top.
    let dir = |v: f32| {
        let a = (135.0 + v * 270.0f32).to_radians();
        Vec2::new(a.cos(), a.sin())
    };
    let arc = |from: f32, to: f32| -> Vec<Pos2> {
        let steps = ((to - from).abs() * 40.0).ceil().max(1.0) as usize;
        (0..=steps).map(|i| c + dir(from + (to - from) * i as f32 / steps as f32) * r).collect()
    };
    p.circle_filled(c, r - 2.0, Color32::from_gray(if resp.hovered() { 0x4a } else { 0x3c }));
    p.add(Shape::line(arc(0.0, 1.0), Stroke::new(2.0, Color32::from_gray(0x55))));
    let start = if bipolar { 0.5 } else { 0.0 };
    if (*value - start).abs() > 0.002 {
        p.add(Shape::line(arc(start.min(*value), start.max(*value)), Stroke::new(2.0, color)));
    }
    p.line_segment([c, c + dir(*value) * (r - 3.0)], Stroke::new(1.5, Color32::WHITE));
    resp
}

/// The dB scale between a fader and its meter.
fn paint_scale(p: &Painter, rect: Rect) {
    for db in [6.0, 0.0, -6.0, -12.0, -18.0, -24.0, -36.0, -48.0, MIN_DB] {
        let y = axis(rect, db_to_pos(db), true);
        let label = if db <= MIN_DB {
            "∞".to_string()
        } else if db > 0.0 {
            format!("+{db}")
        } else {
            format!("{db}")
        };
        p.text(Pos2::new(rect.center().x, y), Align2::CENTER_CENTER, label, FontId::proportional(9.0), TEXT_DIM);
    }
}

/// A tall strip meter on the same scale as the fader: green, yellow from -12 dB, red over
/// 0 dBFS, with a peak-hold line.
fn paint_strip_meter(p: &Painter, rect: Rect, m: &MeterState) {
    p.rect_filled(rect, 1.0, LCD);
    let level = db_to_pos(to_dbfs(m.level));
    for (lo, hi, color) in [(MIN_DB, -12.0, PLAY), (-12.0, 0.0, SOLO), (0.0, MAX_DB, REC)] {
        let a = db_to_pos(lo);
        let b = db_to_pos(hi).min(level);
        if b > a {
            let r = Rect::from_x_y_ranges(rect.x_range(), axis(rect, b, true)..=axis(rect, a, true));
            p.rect_filled(r, 0.0, color);
        }
    }
    let hold_db = to_dbfs(m.hold);
    if hold_db > -90.0 {
        let color = if hold_db > 0.0 { REC } else if hold_db > -12.0 { SOLO } else { PLAY };
        p.hline(rect.x_range(), axis(rect, db_to_pos(hold_db), true), Stroke::new(1.5, color));
    }
}

/// Run `add` in a child UI confined to `rect`, laid out left to right and centered.
fn place<R>(ui: &mut egui::Ui, rect: Rect, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let layout = Layout::left_to_right(Align::Center).with_main_align(Align::Center);
    ui.scope_builder(UiBuilder::new().max_rect(rect).layout(layout), add).inner
}

/// Like [`place`], but centers content that is `width` wide inside `rect`.
fn place_centered<R>(ui: &mut egui::Ui, rect: Rect, width: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let inner = Rect::from_center_size(rect.center(), Vec2::new(width.min(rect.width()), rect.height()));
    ui.scope_builder(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)), add).inner
}

fn dim(ui: &mut egui::Ui, text: &str) -> Response {
    ui.label(egui::RichText::new(text).color(TEXT_DIM).small())
}

/// The volume readout under the pan knob; drag it or double-click to type.
fn db_readout(ui: &mut egui::Ui, db: &mut f32) -> Response {
    ui.add(
        egui::DragValue::new(db)
            .range(MIN_DB..=MAX_DB)
            .speed(0.1)
            .custom_formatter(|v, _| fmt_db(v as f32))
            .custom_parser(|s| {
                let s = s.trim();
                if s.starts_with("-∞") || s.eq_ignore_ascii_case("-inf") {
                    Some(MIN_DB as f64)
                } else {
                    s.parse().ok()
                }
            }),
    )
}

// ------------------------------------------------------------------------- strip layout

const STRIP_W: f32 = 90.0;
const ROW: f32 = 20.0;

struct StripRects {
    input: Rect,
    fx: Vec<Rect>,
    sends: Vec<Rect>,
    output: Rect,
    pan: Rect,
    readout: Rect,
    clip: Rect,
    fader: Rect,
    scale: Rect,
    meters: Rect,
    buttons: Rect,
    name: Rect,
}

/// Rows above the fader area that every strip shares.
#[derive(Clone, Copy)]
struct Slots {
    fx: usize,
    sends: usize,
}

fn strip_rects(rect: Rect, slots: Slots) -> StripRects {
    let x = (rect.left() + 4.0)..=(rect.right() - 4.0);
    let mut y = rect.top() + 4.0;
    let mut row = |h: f32| {
        let r = Rect::from_x_y_ranges(x.clone(), y..=y + h);
        y += h + 3.0;
        r
    };
    let input = row(ROW);
    let fx = (0..slots.fx).map(|_| row(ROW)).collect();
    row(1.0); // small gap between effects and sends
    let sends = (0..slots.sends).map(|_| row(ROW)).collect();
    let output = row(ROW);
    let pan = row(32.0);
    let readout = row(18.0);
    let top = y;

    let name = Rect::from_x_y_ranges(x.clone(), (rect.bottom() - 26.0)..=(rect.bottom() - 4.0));
    let buttons = Rect::from_x_y_ranges(x.clone(), (name.top() - 24.0)..=(name.top() - 4.0));

    // Fader area: [fader | scale | meters], with the clip light above the meters.
    let (l, r) = (*x.start(), *x.end());
    let fader_y = (top + 10.0)..=(buttons.top() - 6.0);
    let fader = Rect::from_x_y_ranges(l..=l + 30.0, fader_y.clone());
    let scale = Rect::from_x_y_ranges(l + 30.0..=l + 56.0, fader_y.clone());
    let meters = Rect::from_x_y_ranges(l + 57.0..=r, fader_y);
    let clip = Rect::from_x_y_ranges(meters.x_range(), top..=top + 6.0);
    StripRects { input, fx, sends, output, pan, readout, clip, fader, scale, meters, buttons, name }
}

/// Paint one or two meters side by side; returns true if the clip light was clicked.
fn meters_and_clip(ui: &mut egui::Ui, s: &StripRects, levels: &[MeterState], id: egui::Id) -> bool {
    let p = ui.painter();
    let n = levels.len() as f32;
    let w = (s.meters.width() - (n - 1.0) * 2.0) / n;
    for (i, m) in levels.iter().enumerate() {
        let left = s.meters.left() + i as f32 * (w + 2.0);
        paint_strip_meter(p, Rect::from_x_y_ranges(left..=left + w, s.meters.y_range()), m);
    }
    paint_scale(p, s.scale);
    let clipped = levels.iter().any(|m| m.clipped);
    let resp = ui.interact(s.clip, id.with("clip"), Sense::click());
    ui.painter().rect_filled(s.clip, 1.0, if clipped { REC } else { LCD });
    resp.on_hover_text(if clipped { "Went over 0 dBFS. Click to reset." } else { "Clip light" }).clicked()
}

fn strip_background(ui: &egui::Ui, rect: Rect) {
    let p = ui.painter();
    p.rect_filled(rect, 0.0, HEADER);
    p.vline(rect.right(), rect.y_range(), Stroke::new(1.0, BG));
}

/// Changes that touch more than one strip, applied after drawing.
enum Action {
    NewBus,
    NewBusAsOutput(u64),
    NewBusAsSend(u64),
    DeleteTrack(u64),
    DeleteBus(u64),
    AddInsert(Chain, EffectKind),
    AddPreset(Chain, usize),
}

// ------------------------------------------------------------------------- insert slots

/// The "Audio FX" rows of a strip: one slot per effect (click = open its window,
/// the dot = on/off, right-click = more), then an empty slot to add one.
fn insert_slots(
    ui: &mut egui::Ui,
    slots: &[Rect],
    chain: &mut Vec<InsertModel>,
    target: Chain,
    open_fx: &mut Vec<u64>,
    actions: &mut Vec<Action>,
) -> bool {
    enum Op {
        Remove,
        Up,
        Down,
    }
    let mut changed = false;
    let mut op = None;
    let len = chain.len();
    for (i, slot) in slots.iter().enumerate() {
        if let Some(ins) = chain.get_mut(i) {
            let uid = ui.scope_id().with(("fx-slot", ins.id));
            let dot = Rect::from_center_size(Pos2::new(slot.left() + 7.0, slot.center().y), Vec2::splat(14.0));
            let body = Rect::from_x_y_ranges((dot.right() + 2.0)..=slot.right(), slot.y_range());
            let d = ui.interact(dot, uid.with("dot"), Sense::click()).on_hover_text("On / off (bypass)");
            let b = ui.interact(body, uid.with("body"), Sense::click());
            let is_open = open_fx.contains(&ins.id);
            let p = ui.painter();
            let on = !ins.bypass;
            if on {
                p.circle_filled(dot.center(), 4.0, PLAY);
            } else {
                p.circle_stroke(dot.center(), 4.0, Stroke::new(1.0, TEXT_DIM));
            }
            let fill = match (on, b.hovered()) {
                (true, false) => Color32::from_rgb(0x2f, 0x4f, 0x7a),
                (true, true) => Color32::from_rgb(0x3a, 0x60, 0x92),
                (false, false) => Color32::from_gray(0x3a),
                (false, true) => Color32::from_gray(0x46),
            };
            p.rect_filled(body.shrink2(Vec2::new(0.0, 1.0)), 3.0, fill);
            if is_open {
                p.rect_stroke(body.shrink2(Vec2::new(0.0, 1.0)), 3.0, Stroke::new(1.0, ACCENT), egui::StrokeKind::Inside);
            }
            let text_color = if on { Color32::WHITE } else { TEXT_DIM };
            p.text(body.center(), Align2::CENTER_CENTER, ins.kind.short(), FontId::proportional(11.5), text_color);
            if d.clicked() {
                ins.bypass = !ins.bypass;
                changed = true;
            }
            if b.clicked() {
                if is_open {
                    open_fx.retain(|x| *x != ins.id);
                } else {
                    open_fx.push(ins.id);
                }
            }
            let b = b.on_hover_text(format!(
                "{}{}\nClick to open, right-click for more",
                ins.kind.name(),
                if on { "" } else { " (bypassed)" }
            ));
            b.context_menu(|ui| {
                if ui.button(if is_open { "Close Window" } else { "Open Window" }).clicked() {
                    if is_open {
                        open_fx.retain(|x| *x != ins.id);
                    } else {
                        open_fx.push(ins.id);
                    }
                    ui.close();
                }
                if ui.checkbox(&mut ins.bypass, "Bypass").changed() {
                    changed = true;
                }
                ui.separator();
                if ui.add_enabled(i > 0, egui::Button::new("Move Up")).clicked() {
                    op = Some((i, Op::Up));
                    ui.close();
                }
                if ui.add_enabled(i + 1 < len, egui::Button::new("Move Down")).clicked() {
                    op = Some((i, Op::Down));
                    ui.close();
                }
                ui.separator();
                if ui.button("Remove").clicked() {
                    op = Some((i, Op::Remove));
                    ui.close();
                }
            });
        } else if i == chain.len() && chain.len() < MAX_INSERTS {
            let btn = place(ui, *slot, |ui| {
                ui.add_sized([slot.width(), ROW - 2.0], egui::Button::new(egui::RichText::new("Audio FX…").small().color(TEXT_DIM)))
            })
            .on_hover_text("Add an effect or a ready-made chain");
            egui::Popup::menu(&btn).show(|ui| {
                for kind in EffectKind::ALL {
                    if ui.button(kind.name()).on_hover_text(kind.description()).clicked() {
                        actions.push(Action::AddInsert(target, kind));
                        ui.close();
                    }
                }
                ui.separator();
                ui.menu_button("Chain Presets", |ui| {
                    let here = match target {
                        Chain::Track(_) => PresetFor::Track,
                        Chain::Bus(_) => PresetFor::Bus,
                        Chain::Master => PresetFor::Master,
                    };
                    // Presets made for this kind of channel first.
                    let mut order: Vec<usize> = (0..CHAIN_PRESETS.len()).collect();
                    order.sort_by_key(|&pi| CHAIN_PRESETS[pi].target != here);
                    let mut last_match = true;
                    for pi in order {
                        let preset = &CHAIN_PRESETS[pi];
                        let matches = preset.target == here;
                        if last_match && !matches {
                            ui.separator();
                        }
                        last_match = matches;
                        if ui.button(preset.name).on_hover_text(preset.hint).clicked() {
                            actions.push(Action::AddPreset(target, pi));
                            ui.close();
                        }
                    }
                });
            });
        }
    }
    if let Some((i, op)) = op {
        match op {
            Op::Remove => {
                let gone = chain.remove(i);
                open_fx.retain(|x| *x != gone.id);
            }
            Op::Up => chain.swap(i, i - 1),
            Op::Down if i + 1 < chain.len() => chain.swap(i, i + 1),
            Op::Down => {}
        }
        changed = true;
    }
    changed
}


// ------------------------------------------------------------------------- the view

impl QtDaw {
    pub fn mixer(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let h = (full.height() - 14.0).max(380.0); // leave room for the scroll bar
        let send_slots = (self.session.tracks.iter().map(|t| t.sends.len()).max().unwrap_or(0) + 1).clamp(2, 8);
        let longest_chain = self
            .session
            .tracks
            .iter()
            .map(|t| t.inserts.len())
            .chain(self.session.buses.iter().map(|b| b.inserts.len()))
            .chain([self.session.master_inserts.len()])
            .max()
            .unwrap_or(0);
        let slots = Slots { fx: (longest_chain + 1).clamp(2, MAX_INSERTS), sends: send_slots };
        // Keep at least ~150 px of fader travel; with many rows the mixer gets taller.
        let rows = 1 + slots.fx + slots.sends + 1;
        let h = h.max(rows as f32 * (ROW + 3.0) + 4.0 + 32.0 + 18.0 + 3.0 * 3.0 + 150.0 + 60.0);
        let buses: Vec<(u64, String)> = self.session.buses.iter().map(|b| (b.id, b.name.clone())).collect();
        let mut actions = Vec::new();

        let left = Rect::from_min_max(full.min, Pos2::new(full.right() - STRIP_W - 4.0, full.bottom()));
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            egui::ScrollArea::horizontal().auto_shrink(false).show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for idx in 0..self.session.tracks.len() {
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(STRIP_W, h), Sense::hover());
                        self.track_strip(ui, idx, rect, slots, &buses, &mut actions);
                    }
                    ui.add_space(10.0);
                    for idx in 0..self.session.buses.len() {
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(STRIP_W, h), Sense::hover());
                        self.bus_strip(ui, idx, rect, slots, &mut actions);
                    }
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.add_space(4.0);
                        if ui.button("+ Bus").on_hover_text("New aux bus (for a reverb, or to group tracks)").clicked() {
                            actions.push(Action::NewBus);
                        }
                    });
                });
            });
        });
        let master = Rect::from_min_size(Pos2::new(full.right() - STRIP_W, full.top()), Vec2::new(STRIP_W, h));
        self.master_strip(ui, master, slots, &mut actions);

        for action in actions {
            match action {
                Action::NewBus => {
                    self.add_bus();
                }
                Action::NewBusAsOutput(track) => {
                    if let Some(bus) = self.add_bus() {
                        if let Some(t) = self.session.track_mut(track) {
                            t.output = bus;
                        }
                    }
                }
                Action::NewBusAsSend(track) => {
                    if let Some(bus) = self.add_bus() {
                        if let Some(t) = self.session.track_mut(track) {
                            t.sends.push(SendModel { bus, level_db: -10.0, pre_fader: false });
                        }
                    }
                }
                Action::DeleteTrack(id) => self.delete_track(id),
                Action::DeleteBus(id) => self.delete_bus(id),
                Action::AddInsert(chain, kind) => self.add_insert(chain, kind),
                Action::AddPreset(chain, pi) => self.add_preset(chain, &CHAIN_PRESETS[pi]),
            }
        }
    }

    fn track_strip(
        &mut self,
        ui: &mut egui::Ui,
        idx: usize,
        rect: Rect,
        slots: Slots,
        buses: &[(u64, String)],
        actions: &mut Vec<Action>,
    ) {
        let inputs = self.input_count();
        let recording = self.shared.is_recording();
        let color = track_color(idx);
        let id = self.session.tracks[idx].id;
        let level = self.view.track_levels.get(&id).copied().unwrap_or_default();
        let bus_name = |bus: u64| buses.iter().find(|(b, _)| *b == bus).map_or("Master", |(_, n)| n.as_str()).to_string();
        let s = strip_rects(rect, slots);
        strip_background(ui, rect);
        let uid = ui.scope_id().with(("track-strip", id));

        let track = &mut self.session.tracks[idx];
        let mut changed = insert_slots(ui, &s.fx, &mut track.inserts, Chain::Track(id), &mut self.view.open_fx, actions);

        // Input.
        place(ui, s.input, |ui| {
            ui.add_enabled_ui(!recording, |ui| {
                ComboBox::from_id_salt(("mx-in", id))
                    .width(s.input.width() - 4.0)
                    .truncate()
                    .selected_text(format!("In {}", track.input + 1))
                    .show_ui(ui, |ui| {
                        for i in 0..inputs.max(track.input + 1) {
                            changed |= ui.selectable_value(&mut track.input, i, format!("Input {}", i + 1)).changed();
                        }
                    });
            });
        });

        // Sends: one row per send, plus an empty slot to add another.
        let mut remove = None;
        for (si, slot) in s.sends.iter().enumerate() {
            if si < track.sends.len() {
                let send = &mut track.sends[si];
                // Right to left: the knob claims its spot first, the menu gets the rest
                // (a truncating combo box grows to fill whatever space is left).
                let layout = Layout::right_to_left(Align::Center);
                ui.scope_builder(UiBuilder::new().max_rect(*slot).layout(layout), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let mut v = db_to_pos(send.level_db);
                    let knob_color = if send.pre_fader { SOLO } else { ACCENT };
                    let r = knob(ui, &mut v, db_to_pos(0.0), 20.0, false, knob_color).on_hover_text(format!(
                        "Send to {}: {} dB{}",
                        bus_name(send.bus),
                        fmt_db(send.level_db),
                        if send.pre_fader { " (pre-fader)" } else { "" }
                    ));
                    if r.changed() {
                        send.level_db = pos_to_db(v);
                        changed = true;
                    }
                    // A plain button + menu instead of a combo box: no arrow, so the bus
                    // name gets the whole width.
                    let name = bus_name(send.bus);
                    let btn = ui
                        .add_sized([ui.available_width(), ROW - 2.0], egui::Button::new(egui::RichText::new(&name).small()).truncate())
                        .on_hover_text(format!("Send to {name}. Click to change or remove."));
                    egui::Popup::menu(&btn).show(|ui| {
                            for (b, name) in buses {
                                if ui.selectable_label(send.bus == *b, name).clicked() {
                                    send.bus = *b;
                                    changed = true;
                                }
                            }
                            ui.separator();
                            changed |= ui.checkbox(&mut send.pre_fader, "Pre-fader").changed();
                            if ui.button("Remove send").clicked() {
                                remove = Some(si);
                            }
                        });
                });
            } else if si == track.sends.len() {
                place(ui, *slot, |ui| {
                    ComboBox::from_id_salt(("send-new", id))
                        .width(slot.width() - 4.0)
                        .truncate()
                        .selected_text(egui::RichText::new("Send…").color(TEXT_DIM).small())
                        .show_ui(ui, |ui| {
                            for (b, name) in buses {
                                let used = track.sends.iter().any(|x| x.bus == *b);
                                if ui.add_enabled(!used, egui::Button::selectable(false, name)).clicked() {
                                    track.sends.push(SendModel { bus: *b, level_db: -10.0, pre_fader: false });
                                    changed = true;
                                }
                            }
                            if !buses.is_empty() {
                                ui.separator();
                            }
                            if ui.button("New Bus").clicked() {
                                actions.push(Action::NewBusAsSend(id));
                            }
                        });
                });
            }
        }
        if let Some(si) = remove {
            track.sends.remove(si);
            changed = true;
        }

        // Output.
        place(ui, s.output, |ui| {
            ComboBox::from_id_salt(("mx-out", id))
                .width(s.output.width() - 4.0)
                .truncate()
                .selected_text(egui::RichText::new(bus_name(track.output)).small())
                .show_ui(ui, |ui| {
                    changed |= ui.selectable_value(&mut track.output, MASTER, "Master").changed();
                    for (b, name) in buses {
                        changed |= ui.selectable_value(&mut track.output, *b, name).changed();
                    }
                    ui.separator();
                    if ui.button("New Bus").clicked() {
                        actions.push(Action::NewBusAsOutput(id));
                    }
                })
                .response
                .on_hover_text("Output: where this track plays to");
        });

        // Pan.
        place_centered(ui, s.pan, 30.0, |ui| {
            let mut v = (track.pan + 1.0) / 2.0;
            let r = knob(ui, &mut v, 0.5, 30.0, true, color).on_hover_text(format!("Pan {}", fmt_pan(track.pan)));
            if r.changed() {
                track.pan = v * 2.0 - 1.0;
                changed = true;
            }
        });

        // Volume readout + fader + meter.
        place_centered(ui, s.readout, 50.0, |ui| changed |= db_readout(ui, &mut track.volume_db).changed());
        place(ui, s.fader, |ui| changed |= fader(ui, &mut track.volume_db, s.fader.size(), true).changed());
        let reset_clip = meters_and_clip(ui, &s, &[level], uid);

        // Record / mute / solo.
        place_centered(ui, s.buttons, 66.0, |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            ui.add_enabled_ui(!recording, |ui| {
                small_toggle(ui, &mut track.armed, "R", REC, "Record-enable (also lets you hear the input)");
            });
            changed |= small_toggle(ui, &mut track.mute, "M", MUTE, "Mute").changed();
            changed |= small_toggle(ui, &mut track.solo, "S", SOLO, "Solo").changed();
        });

        // Name (right-click to delete).
        ui.painter().rect_filled(s.name, 3.0, color.gamma_multiply(0.45));
        place(ui, s.name, |ui| {
            let r = ui.add(
                TextEdit::singleline(&mut track.name)
                    .frame(egui::Frame::NONE)
                    .align(Align2::CENTER_CENTER)
                    .desired_width(s.name.width() - 6.0)
                    .text_color(Color32::WHITE),
            );
            changed |= r.changed();
            r.context_menu(|ui| {
                if ui.add_enabled(!recording, egui::Button::new("Delete Track")).clicked() {
                    actions.push(Action::DeleteTrack(id));
                    ui.close();
                }
            });
        });

        if reset_clip {
            if let Some(m) = self.view.track_levels.get_mut(&id) {
                m.clipped = false;
                m.hold = 0.0;
            }
        }
        if changed {
            self.dirty = true;
        }
    }

    fn bus_strip(&mut self, ui: &mut egui::Ui, idx: usize, rect: Rect, slots: Slots, actions: &mut Vec<Action>) {
        let bus = &self.session.buses[idx];
        let id = bus.id;
        let levels = self.view.bus_levels.get(&id).copied().unwrap_or_default();
        let fed_by: Vec<&str> = self
            .session
            .tracks
            .iter()
            .filter(|t| t.output == id || t.sends.iter().any(|s| s.bus == id))
            .map(|t| t.name.as_str())
            .collect();
        let fed_tip = if fed_by.is_empty() { "Nothing plays into this bus yet.".to_string() } else { format!("Fed by: {}", fed_by.join(", ")) };
        let s = strip_rects(rect, slots);
        strip_background(ui, rect);
        let uid = ui.scope_id().with(("bus-strip", id));
        let color = Color32::from_rgb(0x9a, 0x8c, 0xd8);

        let bus = &mut self.session.buses[idx];
        let mut changed = insert_slots(ui, &s.fx, &mut bus.inserts, Chain::Bus(id), &mut self.view.open_fx, actions);
        place(ui, s.input, |ui| dim(ui, "Bus")).on_hover_text(fed_tip);
        place(ui, s.output, |ui| dim(ui, "→ Master"));
        place_centered(ui, s.pan, 30.0, |ui| {
            let mut v = (bus.pan + 1.0) / 2.0;
            let r = knob(ui, &mut v, 0.5, 30.0, true, color).on_hover_text(format!("Balance {}", fmt_pan(bus.pan)));
            if r.changed() {
                bus.pan = v * 2.0 - 1.0;
                changed = true;
            }
        });
        place_centered(ui, s.readout, 50.0, |ui| changed |= db_readout(ui, &mut bus.volume_db).changed());
        place(ui, s.fader, |ui| changed |= fader(ui, &mut bus.volume_db, s.fader.size(), true).changed());
        let reset_clip = meters_and_clip(ui, &s, &levels, uid);
        place_centered(ui, s.buttons, 43.0, |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            changed |= small_toggle(ui, &mut bus.mute, "M", MUTE, "Mute").changed();
            changed |= small_toggle(ui, &mut bus.solo, "S", SOLO, "Solo (also plays the tracks feeding it)").changed();
        });
        ui.painter().rect_filled(s.name, 3.0, color.gamma_multiply(0.45));
        place(ui, s.name, |ui| {
            let r = ui.add(
                TextEdit::singleline(&mut bus.name)
                    .frame(egui::Frame::NONE)
                    .align(Align2::CENTER_CENTER)
                    .desired_width(s.name.width() - 6.0)
                    .text_color(Color32::WHITE),
            );
            changed |= r.changed();
            r.context_menu(|ui| {
                if ui.button("Delete Bus").clicked() {
                    actions.push(Action::DeleteBus(id));
                    ui.close();
                }
            });
        });

        if reset_clip {
            if let Some(m) = self.view.bus_levels.get_mut(&id) {
                for m in m.iter_mut() {
                    m.clipped = false;
                    m.hold = 0.0;
                }
            }
        }
        if changed {
            self.dirty = true;
        }
    }

    fn master_strip(&mut self, ui: &mut egui::Ui, rect: Rect, slots: Slots, actions: &mut Vec<Action>) {
        let s = strip_rects(rect, slots);
        strip_background(ui, rect);
        let mut changed =
            insert_slots(ui, &s.fx, &mut self.session.master_inserts, Chain::Master, &mut self.view.open_fx, actions);
        place(ui, s.input, |ui| dim(ui, "Master")).on_hover_text("Master effects run after the master fader");
        place(ui, s.output, |ui| dim(ui, "Stereo Out"));
        place_centered(ui, s.readout, 50.0, |ui| changed |= db_readout(ui, &mut self.session.master_db).changed());
        place(ui, s.fader, |ui| changed |= fader(ui, &mut self.session.master_db, s.fader.size(), true).changed());
        let levels = self.view.master_levels;
        if meters_and_clip(ui, &s, &levels, ui.scope_id().with("master-strip")) {
            for m in &mut self.view.master_levels {
                m.clipped = false;
                m.hold = 0.0;
            }
        }
        ui.painter().rect_filled(s.name, 3.0, Color32::from_gray(0x50));
        ui.painter().text(s.name.center(), Align2::CENTER_CENTER, "Master", FontId::proportional(13.0), Color32::WHITE);
        if changed {
            self.dirty = true;
        }
    }
}

pub fn fmt_pan(pan: f32) -> String {
    let p = (pan * 64.0).round() as i64;
    match p {
        0 => "C".to_string(),
        p if p < 0 => format!("L{}", -p),
        p => format!("R{p}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_mapping_round_trips() {
        for db in [-60.0, -24.0, -6.0, 0.0, 3.0, 6.0] {
            assert!((pos_to_db(db_to_pos(db)) - db).abs() < 0.01, "{db}");
        }
        assert_eq!(db_to_pos(MIN_DB), 0.0);
        assert_eq!(pos_to_db(0.0), MIN_DB);
        assert!((db_to_pos(0.0) - 0.8409).abs() < 0.001);
    }
}
