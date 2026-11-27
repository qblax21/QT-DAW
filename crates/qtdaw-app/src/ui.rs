//! Drawing the QT DAW window: menu, control bar, tracks + timeline, status bar, dialogs.

use std::sync::atomic::Ordering;

use eframe::egui::{
    self, Align, Align2, Color32, ComboBox, CornerRadius, FontId, Frame, Key, Layout, Margin, Modifiers, Pos2,
    Rect, Sense, Shape, Slider, Stroke, StrokeKind, TextEdit, UiBuilder, Vec2,
};
use qtdaw_engine::clip::OVERVIEW_FRAMES;
use qtdaw_engine::session;
use qtdaw_engine::to_dbfs;

use crate::app::{MainView, MeterState, QtDaw};
use crate::mixer::{fader, fmt_pan};

// ------------------------------------------------------------------------- look

pub const BG: Color32 = Color32::from_rgb(0x1c, 0x1d, 0x21);
pub const PANEL: Color32 = Color32::from_rgb(0x26, 0x28, 0x2d);
pub const HEADER: Color32 = Color32::from_rgb(0x2c, 0x2e, 0x34);
pub const LANE: Color32 = Color32::from_rgb(0x21, 0x22, 0x27);
pub const LANE_ARMED: Color32 = Color32::from_rgb(0x2a, 0x1f, 0x22);
pub const GRID_BAR: Color32 = Color32::from_rgb(0x34, 0x36, 0x3d);
pub const GRID_BEAT: Color32 = Color32::from_rgb(0x28, 0x2a, 0x30);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8a, 0x8e, 0x99);
pub const ACCENT: Color32 = Color32::from_rgb(0x4d, 0xa3, 0xff);
pub const REC: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
pub const PLAY: Color32 = Color32::from_rgb(0x46, 0xc8, 0x6e);
pub const SOLO: Color32 = Color32::from_rgb(0xf0, 0xc0, 0x40);
pub const MUTE: Color32 = Color32::from_rgb(0x4d, 0xa3, 0xff);
pub const LCD: Color32 = Color32::from_rgb(0x14, 0x15, 0x18);

const TRACK_COLORS: [Color32; 8] = [
    Color32::from_rgb(0x4d, 0xa3, 0xff),
    Color32::from_rgb(0x46, 0xc8, 0x6e),
    Color32::from_rgb(0xf0, 0xa0, 0x40),
    Color32::from_rgb(0xc0, 0x6c, 0xf0),
    Color32::from_rgb(0x40, 0xd0, 0xd0),
    Color32::from_rgb(0xf0, 0x70, 0x90),
    Color32::from_rgb(0xb0, 0xd0, 0x40),
    Color32::from_rgb(0x90, 0x9c, 0xf0),
];

const HEADER_W: f32 = 260.0;
const ROW_H: f32 = 64.0;
const RULER_H: f32 = 26.0;

pub fn apply_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = LCD;
    v.selection.bg_fill = ACCENT.gamma_multiply(0.6);
    ctx.set_visuals(v);
}

pub(crate) fn track_color(i: usize) -> Color32 {
    TRACK_COLORS[i % TRACK_COLORS.len()]
}

/// Meter fill for a level, -60 dBFS..0 dBFS -> 0..1, green/yellow/red.
fn meter_fraction(level: f32) -> (f32, Color32) {
    let db = to_dbfs(level);
    let f = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let color = if db >= -0.5 {
        REC
    } else if db >= -12.0 {
        SOLO
    } else {
        PLAY
    };
    (f, color)
}

/// A small meter (track headers, inputs, control bar) with a peak-hold tick.
fn paint_meter(painter: &egui::Painter, rect: Rect, m: &MeterState, vertical: bool) {
    painter.rect_filled(rect, 1.0, LCD);
    let (f, color) = meter_fraction(m.level);
    if f > 0.0 {
        let fill = if vertical {
            Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - rect.height() * f), rect.max)
        } else {
            Rect::from_min_max(rect.min, Pos2::new(rect.left() + rect.width() * f, rect.bottom()))
        };
        painter.rect_filled(fill, 1.0, if m.clipped { REC } else { color });
    }
    let (h, hold_color) = meter_fraction(m.hold);
    if h > 0.0 {
        if vertical {
            painter.hline(rect.x_range(), rect.bottom() - rect.height() * h, Stroke::new(1.0, hold_color));
        } else {
            painter.vline(rect.left() + rect.width() * h, rect.y_range(), Stroke::new(1.0, hold_color));
        }
    }
}

// ------------------------------------------------------------------------- app

impl eframe::App for QtDaw {
    /// Runs before every frame, and also while the window is minimized (when something asks
    /// for a repaint), so Claude's requests and finished recordings are handled even then.
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_remote();
        self.poll_recorder();
        self.sync_engine();
        self.sync_inserts();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.tick(&ctx);
        self.shortcuts(&ctx);
        if ctx.input(|i| i.viewport().close_requested()) && self.dirty {
            self.save();
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("control")
            .frame(Frame::default().fill(PANEL).inner_margin(Margin::symmetric(10, 6)))
            .show(ui, |ui| self.control_bar(ui));
        egui::Panel::bottom("status")
            .frame(Frame::default().fill(PANEL).inner_margin(Margin::symmetric(10, 4)))
            .show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().frame(Frame::default().fill(BG)).show(ui, |ui| match self.view.main {
            MainView::Tracks => self.arrange(ui),
            MainView::Mixer => {
                self.engine_banner(ui);
                self.mixer(ui)
            }
        });

        self.dialogs(&ctx);
    }
}

impl QtDaw {
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (mut space, mut rec, mut home, mut click, mut del, mut save, mut new_track) =
            (false, false, false, false, false, false, false);
        let mut toggle_mixer = false;
        ctx.input_mut(|i| {
            toggle_mixer = i.consume_key(Modifiers::NONE, Key::X);
            space = i.consume_key(Modifiers::NONE, Key::Space);
            rec = i.consume_key(Modifiers::NONE, Key::R);
            home = i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Home);
            click = i.consume_key(Modifiers::NONE, Key::K);
            del = i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace);
            save = i.consume_key(Modifiers::COMMAND, Key::S);
            new_track = i.consume_key(Modifiers::COMMAND, Key::T);
        });
        if space {
            self.play_stop();
        }
        if rec {
            self.record();
        }
        if home && !self.busy() {
            self.shared.locate(0);
            self.view.scroll_sec = 0.0;
        }
        if click {
            let on = !self.shared.click_on.load(Ordering::Relaxed);
            self.shared.click_on.store(on, Ordering::Relaxed);
        }
        if del {
            self.delete_selected_clip();
        }
        if save {
            self.save();
        }
        if new_track {
            self.add_track();
        }
        if toggle_mixer {
            self.toggle_mixer();
        }
    }

    fn toggle_mixer(&mut self) {
        self.view.main = match self.view.main {
            MainView::Tracks => MainView::Mixer,
            MainView::Mixer => MainView::Tracks,
        };
    }

    // --------------------------------------------------------------------- menu

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New Session…").clicked() {
                    self.view.new_name = "New Song".into();
                    self.view.new_rate = self.session.sample_rate;
                    self.view.show_new = true;
                    ui.close();
                }
                if ui.button("Open Session…").clicked() {
                    self.view.sessions = session::list_sessions(&session::sessions_root());
                    self.view.show_open = true;
                    ui.close();
                }
                if ui.button("Save   Ctrl+S").clicked() {
                    self.save();
                    ui.close();
                }
            });
            ui.menu_button("Track", |ui| {
                if ui.button("New Audio Track   Ctrl+T").clicked() {
                    self.add_track();
                    ui.close();
                }
                if ui.button("New Bus").clicked() {
                    self.add_bus();
                    self.view.main = MainView::Mixer;
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                if ui.radio(self.view.main == MainView::Tracks, "Tracks").clicked() {
                    self.view.main = MainView::Tracks;
                    ui.close();
                }
                if ui.radio(self.view.main == MainView::Mixer, "Mixer   X").clicked() {
                    self.view.main = MainView::Mixer;
                    ui.close();
                }
            });
            ui.menu_button("Settings", |ui| {
                if ui.button("Audio Settings…").clicked() {
                    self.open_settings();
                    ui.close();
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let name = format!("{}{}", self.session.name, if self.dirty { " •" } else { "" });
                ui.label(egui::RichText::new(name).color(TEXT_DIM));
            });
        });
    }

    fn open_settings(&mut self) {
        self.view.draft = self.settings.clone();
        self.refresh_devices();
        self.view.show_settings = true;
    }

    // --------------------------------------------------------------------- control bar

    fn control_bar(&mut self, ui: &mut egui::Ui) {
        let playing = self.shared.playing.load(Ordering::Acquire);
        let counting = self.shared.counting_in.load(Ordering::Acquire);
        let recording = self.shared.is_recording();

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if icon_button(ui, Icon::ToStart, false, ACCENT).on_hover_text("Go to start (Enter)").clicked() && !self.busy() {
                self.shared.locate(0);
                self.view.scroll_sec = 0.0;
            }
            if icon_button(ui, Icon::Play, playing && !recording, PLAY).on_hover_text("Play / Stop (Space)").clicked() {
                self.play_stop();
            }
            if icon_button(ui, Icon::Stop, false, ACCENT).on_hover_text("Stop (press twice to go to start)").clicked() {
                self.stop();
            }
            let rec_lit = recording || (counting && ui.input(|i| (i.time * 3.0) as i64 % 2 == 0));
            if icon_button(ui, Icon::Record, rec_lit, REC).on_hover_text("Record (R)").clicked() {
                self.record();
            }

            ui.add_space(12.0);
            self.lcd(ui);
            ui.add_space(12.0);

            let mut click = self.shared.click_on.load(Ordering::Relaxed);
            if ui.toggle_value(&mut click, "Click").on_hover_text("Metronome (K)").changed() {
                self.shared.click_on.store(click, Ordering::Relaxed);
            }
            let mut bars = self.shared.count_in_bars.load(Ordering::Relaxed);
            let label = |b: u32| match b {
                0 => "Count-in: Off".to_string(),
                1 => "Count-in: 1 bar".to_string(),
                n => format!("Count-in: {n} bars"),
            };
            ComboBox::from_id_salt("countin").selected_text(label(bars)).show_ui(ui, |ui| {
                for b in [0, 1, 2] {
                    ui.selectable_value(&mut bars, b, label(b));
                }
            });
            self.shared.count_in_bars.store(bars, Ordering::Relaxed);

            ui.add_space(12.0);
            ui.selectable_value(&mut self.view.main, MainView::Tracks, "Tracks").on_hover_text("Tracks view (X)");
            ui.selectable_value(&mut self.view.main, MainView::Mixer, "Mixer").on_hover_text("Mixer view (X)");

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(70.0, 14.0), Sense::hover());
                let half = rect.height() / 2.0 - 1.0;
                let p = ui.painter();
                paint_meter(p, Rect::from_min_size(rect.min, Vec2::new(rect.width(), half)), &self.view.master_levels[0], false);
                paint_meter(
                    p,
                    Rect::from_min_size(Pos2::new(rect.left(), rect.top() + half + 2.0), Vec2::new(rect.width(), half)),
                    &self.view.master_levels[1],
                    false,
                );
                if fader(ui, &mut self.session.master_db, Vec2::new(110.0, 18.0), false).changed() {
                    self.dirty = true;
                }
                ui.label(egui::RichText::new("Master").color(TEXT_DIM));
            });
        });
    }

    /// The position display: bar.beat, time, tempo and time signature.
    fn lcd(&mut self, ui: &mut egui::Ui) {
        let frames = self.shared.playhead.load(Ordering::Acquire).max(0);
        let secs = frames as f64 / self.sample_rate();
        let beats = secs * self.session.tempo as f64 / 60.0;
        let bpb = self.session.beats_per_bar.max(1) as f64;
        let bar = (beats / bpb).floor() as i64 + 1;
        let beat = (beats % bpb).floor() as i64 + 1;
        let sixteenth = ((beats.fract()) * 4.0).floor() as i64 + 1;
        let mins = (secs / 60.0).floor() as i64;

        Frame::default().fill(LCD).corner_radius(4.0).inner_margin(Margin::symmetric(12, 4)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{bar:>3} {beat} {sixteenth}")).font(FontId::monospace(20.0)).color(Color32::WHITE));
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!("{mins:02}:{:06.3}", secs - mins as f64 * 60.0))
                        .font(FontId::monospace(13.0))
                        .color(TEXT_DIM),
                );
                ui.add_space(12.0);
                let changed = ui
                    .add(egui::DragValue::new(&mut self.session.tempo).range(30.0..=300.0).speed(0.2).fixed_decimals(1).suffix(" bpm"))
                    .on_hover_text("Tempo (drag or double-click to type)")
                    .changed();
                ui.add(egui::DragValue::new(&mut self.session.beats_per_bar).range(1..=12).suffix("/4"))
                    .on_hover_text("Beats per bar");
                if changed {
                    self.dirty = true;
                }
            });
        });
    }

    // --------------------------------------------------------------------- status bar

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Inputs").color(TEXT_DIM));
            let n = self.view.input_levels.len();
            for i in 0..n {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(14.0, 20.0), Sense::click());
                let meter = Rect::from_min_size(rect.min, Vec2::new(6.0, rect.height()));
                paint_meter(ui.painter(), meter, &self.view.input_levels[i], true);
                ui.painter().text(
                    Pos2::new(rect.left() + 8.0, rect.bottom()),
                    Align2::LEFT_BOTTOM,
                    format!("{}", i + 1),
                    FontId::proportional(9.0),
                    TEXT_DIM,
                );
                let m = &mut self.view.input_levels[i];
                if resp.clicked() {
                    m.clipped = false;
                }
                resp.on_hover_text(format!(
                    "Input {}: {:.1} dBFS (peak {:.1}){}",
                    i + 1,
                    to_dbfs(m.level),
                    to_dbfs(m.hold),
                    if m.clipped { "\nClipped! Turn the gain down on your interface. Click to reset." } else { "" }
                ));
            }
            ui.separator();
            if let Some(msg) = self.status_text() {
                ui.label(msg.to_string());
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| match &self.engine {
                Some(e) => {
                    let xruns = e.meters.xruns.load(Ordering::Relaxed);
                    let dev = if e.input_name == e.output_name {
                        e.input_name.clone()
                    } else {
                        format!("{} → {}", e.input_name, e.output_name)
                    };
                    ui.label(
                        egui::RichText::new(format!("{dev} · {} kHz · {xruns} dropouts", e.sample_rate as f32 / 1000.0))
                            .color(TEXT_DIM),
                    );
                }
                None => {
                    ui.label(egui::RichText::new("No audio device").color(REC));
                }
            });
        });
    }

    // --------------------------------------------------------------------- tracks + timeline

    fn arrange(&mut self, ui: &mut egui::Ui) {
        self.engine_banner(ui);
        self.tracks_view(ui);
    }

    /// Red banner when the audio device couldn't be opened.
    fn engine_banner(&mut self, ui: &mut egui::Ui) {
        if let Some(err) = self.engine_error.clone() {
            Frame::default().fill(REC.gamma_multiply(0.25)).inner_margin(8).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("Audio device problem: {err}"));
                    if ui.button("Audio Settings…").clicked() {
                        self.open_settings();
                    }
                    if ui.button("Retry").clicked() {
                        self.restart_engine();
                    }
                });
            });
        }
    }

    fn tracks_view(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let lane_left = full.left() + HEADER_W;
        let lane_w = (full.width() - HEADER_W).max(50.0);
        let sr = self.sample_rate();

        // Zoom (Ctrl+wheel) and horizontal scroll (Shift+wheel or trackpad) over the timeline.
        let lanes_area = Rect::from_min_max(Pos2::new(lane_left, full.top()), full.max);
        if let Some(pos) = ui.ctx().pointer_latest_pos().filter(|p| lanes_area.contains(*p)) {
            let (zoom, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta()));
            if zoom != 1.0 {
                let t = self.view.scroll_sec + ((pos.x - lane_left) / self.view.px_per_sec) as f64;
                self.view.px_per_sec = (self.view.px_per_sec * zoom).clamp(2.0, 2000.0);
                self.view.scroll_sec = (t - ((pos.x - lane_left) / self.view.px_per_sec) as f64).max(0.0);
            }
            if scroll.x != 0.0 {
                self.view.scroll_sec = (self.view.scroll_sec - (scroll.x / self.view.px_per_sec) as f64).max(0.0);
            }
        }

        // Follow the playhead while playing.
        let playhead_sec = self.shared.playhead.load(Ordering::Acquire) as f64 / sr;
        let visible_sec = (lane_w / self.view.px_per_sec) as f64;
        if self.busy() && (playhead_sec > self.view.scroll_sec + visible_sec * 0.95 || playhead_sec < self.view.scroll_sec) {
            self.view.scroll_sec = (playhead_sec - visible_sec * 0.05).max(0.0);
        }

        // Ruler row.
        let ruler = Rect::from_min_size(Pos2::new(lane_left, full.top()), Vec2::new(lane_w, RULER_H));
        let corner = Rect::from_min_size(full.min, Vec2::new(HEADER_W, RULER_H));
        ui.painter().rect_filled(corner, 0.0, PANEL);
        let resp = ui.interact(ruler, ui.scope_id().with("ruler"), Sense::click_and_drag());
        self.paint_ruler(ui, ruler);
        if let Some(pos) = resp.interact_pointer_pos().filter(|_| resp.clicked() || resp.dragged()) {
            self.locate_x(pos.x, lane_left);
        }
        ui.scope_builder(UiBuilder::new().max_rect(corner.shrink2(Vec2::new(8.0, 2.0))), |ui| {
            ui.horizontal_centered(|ui| {
                if ui.small_button("+ Track").on_hover_text("New audio track (Ctrl+T)").clicked() {
                    self.add_track();
                }
                ui.label(egui::RichText::new(format!("{} tracks", self.session.tracks.len())).color(TEXT_DIM).small());
            });
        });

        // Track rows.
        let body = Rect::from_min_max(Pos2::new(full.left(), full.top() + RULER_H), full.max);
        let mut delete: Option<u64> = None;
        ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(0.0, 1.0);
                for idx in 0..self.session.tracks.len() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let top = ui.cursor().top();
                        if let Some(id) = self.track_header(ui, idx) {
                            delete = Some(id);
                        }
                        let lane = Rect::from_min_size(Pos2::new(lane_left, top), Vec2::new(lane_w, ROW_H));
                        self.track_lane(ui, idx, lane, lane_left);
                    });
                }
                // Clicking empty space below the tracks moves the playhead too.
                let rest = ui.available_rect_before_wrap();
                if rest.height() > 0.0 {
                    let r = ui.allocate_rect(rest, Sense::click());
                    if r.double_clicked() {
                        self.add_track();
                    }
                }
            });
        });
        if let Some(id) = delete {
            self.delete_track(id);
        }

        // Playhead over everything.
        let x = lane_left + ((playhead_sec - self.view.scroll_sec) as f32) * self.view.px_per_sec;
        if x >= lane_left && x <= full.right() {
            let painter = ui.painter_at(Rect::from_min_max(Pos2::new(lane_left, full.top()), full.max));
            let color = if self.shared.is_recording() { REC } else { Color32::WHITE };
            painter.vline(x, full.top()..=full.bottom(), Stroke::new(1.5, color));
            painter.add(Shape::convex_polygon(
                vec![Pos2::new(x - 6.0, full.top()), Pos2::new(x + 6.0, full.top()), Pos2::new(x, full.top() + 8.0)],
                color,
                Stroke::NONE,
            ));
        }
    }

    fn locate_x(&mut self, x: f32, lane_left: f32) {
        if self.busy() && self.shared.counting_in.load(Ordering::Acquire) {
            return;
        }
        let secs = self.view.scroll_sec + ((x - lane_left) / self.view.px_per_sec) as f64;
        self.shared.locate((secs.max(0.0) * self.sample_rate()) as i64);
    }

    /// Bars to skip between labels so they stay readable at any zoom.
    fn bar_step(&self) -> (f64, i64) {
        let bar_sec = 60.0 / self.session.tempo as f64 * self.session.beats_per_bar.max(1) as f64;
        let bar_px = bar_sec as f32 * self.view.px_per_sec;
        let mut step = 1;
        while (bar_px * step as f32) < 48.0 && step < 4096 {
            step *= 2;
        }
        (bar_sec, step)
    }

    fn sec_to_x(&self, sec: f64, lane_left: f32) -> f32 {
        lane_left + ((sec - self.view.scroll_sec) as f32) * self.view.px_per_sec
    }

    fn paint_ruler(&self, ui: &egui::Ui, rect: Rect) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, PANEL);
        let (bar_sec, step) = self.bar_step();
        let first = (self.view.scroll_sec / bar_sec).floor() as i64;
        let last = first + (rect.width() / (bar_sec as f32 * self.view.px_per_sec)).ceil() as i64 + 1;
        for b in first..=last {
            if b % step != 0 {
                continue;
            }
            let x = self.sec_to_x(b as f64 * bar_sec, rect.left());
            p.vline(x, rect.y_range(), Stroke::new(1.0, GRID_BAR));
            p.text(Pos2::new(x + 4.0, rect.center().y), Align2::LEFT_CENTER, format!("{}", b + 1), FontId::proportional(12.0), TEXT_DIM);
        }
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, GRID_BAR));
    }

    /// Track header (name, record / mute / solo, input, volume, pan). Returns a track id to delete.
    fn track_header(&mut self, ui: &mut egui::Ui, idx: usize) -> Option<u64> {
        let inputs = self.input_count();
        let color = track_color(idx);
        let level = self.view.track_levels.get(&self.session.tracks[idx].id).copied().unwrap_or_default();
        let output = self.session.output_name(self.session.tracks[idx].output).to_string();
        let recording = self.shared.is_recording();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(HEADER_W, ROW_H), Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, HEADER);
        painter.rect_filled(Rect::from_min_size(rect.min, Vec2::new(4.0, rect.height())), 0.0, color);
        paint_meter(&painter, Rect::from_min_max(Pos2::new(rect.right() - 7.0, rect.top() + 4.0), Pos2::new(rect.right() - 3.0, rect.bottom() - 4.0)), &level, true);

        let mut delete = None;
        let mut changed = false;
        let track = &mut self.session.tracks[idx];
        let id = track.id;
        resp.context_menu(|ui| {
            if ui.add_enabled(!recording, egui::Button::new("Delete Track")).clicked() {
                delete = Some(id);
                ui.close();
            }
        });

        let inner = Rect::from_min_max(rect.min + Vec2::new(10.0, 5.0), rect.max - Vec2::new(12.0, 5.0));
        // Own layout (top to bottom): the parent row is left-to-right, and inheriting that
        // would push the second row of controls out into the timeline.
        ui.scope_builder(UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)), |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
            ui.horizontal(|ui| {
                changed |= ui
                    .add(TextEdit::singleline(&mut track.name).desired_width(118.0).frame(Frame::NONE))
                    .changed();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    changed |= small_toggle(ui, &mut track.solo, "S", SOLO, "Solo").changed();
                    changed |= small_toggle(ui, &mut track.mute, "M", MUTE, "Mute").changed();
                    // Arming can't change mid-take (the take's file list is already fixed).
                    ui.add_enabled_ui(!recording, |ui| {
                        small_toggle(ui, &mut track.armed, "R", REC, "Record-enable (also lets you hear the input)");
                    });
                });
            });
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!recording, |ui| {
                    ComboBox::from_id_salt(("input", id))
                        .width(58.0)
                        .selected_text(format!("In {}", track.input + 1))
                        .show_ui(ui, |ui| {
                            for i in 0..inputs.max(track.input + 1) {
                                changed |= ui.selectable_value(&mut track.input, i, format!("Input {}", i + 1)).changed();
                            }
                        });
                });
                changed |= fader(ui, &mut track.volume_db, Vec2::new(84.0, 18.0), false).changed();
                let pan = ui
                    .add(
                        egui::DragValue::new(&mut track.pan)
                            .range(-1.0..=1.0)
                            .speed(0.01)
                            .custom_formatter(|v, _| fmt_pan(v as f32)),
                    )
                    .on_hover_text(format!("Pan (drag; double-click to type)\nOutput: {output}"));
                changed |= pan.changed();
            });
        });
        if changed {
            self.dirty = true;
        }
        delete
    }

    fn track_lane(&mut self, ui: &mut egui::Ui, idx: usize, rect: Rect, lane_left: f32) {
        let lane_w = rect.width();
        let resp = ui.allocate_rect(rect, Sense::click());
        let p = ui.painter_at(rect);
        let track = &self.session.tracks[idx];
        p.rect_filled(rect, 0.0, if track.armed { LANE_ARMED } else { LANE });

        // Grid.
        let (bar_sec, step) = self.bar_step();
        let beat_sec = bar_sec / self.session.beats_per_bar.max(1) as f64;
        let beat_px = beat_sec as f32 * self.view.px_per_sec;
        let first = (self.view.scroll_sec / beat_sec).floor() as i64;
        let last = first + (lane_w / beat_px).ceil() as i64 + 1;
        let bpb = self.session.beats_per_bar.max(1) as i64;
        for b in first..=last {
            let is_bar = b % bpb == 0;
            if !is_bar && beat_px < 14.0 {
                continue;
            }
            if is_bar && (b / bpb) % step != 0 {
                continue;
            }
            let x = self.sec_to_x(b as f64 * beat_sec, lane_left);
            p.vline(x, rect.y_range(), Stroke::new(1.0, if is_bar { GRID_BAR } else { GRID_BEAT }));
        }

        // Clips.
        let sr = self.sample_rate();
        let color = track_color(idx);
        let mut hit: Option<usize> = None;
        let click_pos = resp.clicked().then(|| resp.interact_pointer_pos()).flatten();
        for (ci, clip) in track.clips.iter().enumerate() {
            let Some(audio) = self.audio.get(&clip.file) else { continue };
            let x0 = self.sec_to_x(clip.start as f64 / sr, lane_left);
            let x1 = self.sec_to_x((clip.start + audio.frames() as i64) as f64 / sr, lane_left);
            if x1 < rect.left() || x0 > rect.right() {
                continue;
            }
            let crect = Rect::from_min_max(Pos2::new(x0, rect.top() + 2.0), Pos2::new(x1, rect.bottom() - 2.0));
            let selected = self.view.selected_clip == Some((track.id, ci));
            p.rect_filled(crect, 3.0, color.gamma_multiply(0.35));
            p.rect_stroke(crect, 3.0, Stroke::new(if selected { 2.0 } else { 1.0 }, if selected { Color32::WHITE } else { color }), StrokeKind::Inside);

            // Waveform: one vertical line per pixel column.
            let mid = crect.center().y + 6.0;
            let half_h = (crect.height() - 16.0) / 2.0;
            let frames_per_px = sr / self.view.px_per_sec as f64;
            let ov = &audio.overview;
            let px_from = crect.left().max(rect.left()).floor() as i64;
            let px_to = crect.right().min(rect.right()).ceil() as i64;
            let wave_color = color.gamma_multiply(0.95);
            for px in px_from..px_to {
                let f0 = ((px as f32 - x0) as f64 * frames_per_px) as i64;
                let f1 = (f0 as f64 + frames_per_px) as i64;
                let i0 = (f0.max(0) as usize) / OVERVIEW_FRAMES;
                let i1 = ((f1.max(0) as usize) / OVERVIEW_FRAMES + 1).min(ov.len());
                if i0 >= i1 {
                    continue;
                }
                let (lo, hi) = ov[i0..i1].iter().fold((0.0f32, 0.0f32), |(a, b), &(l, h)| (a.min(l), b.max(h)));
                let y0 = mid - hi.min(1.0) * half_h;
                let y1 = mid - lo.max(-1.0) * half_h;
                p.vline(px as f32 + 0.5, y0..=y1.max(y0 + 1.0), Stroke::new(1.0, wave_color));
            }
            let name = clip.file.rsplit('/').next().unwrap_or(&clip.file).trim_end_matches(".wav");
            p.text(crect.left_top() + Vec2::new(5.0, 2.0), Align2::LEFT_TOP, name, FontId::proportional(11.0), Color32::WHITE);

            if let Some(pos) = click_pos {
                if crect.contains(pos) {
                    hit = Some(ci); // later clips are on top, so the last match wins
                }
            }
        }

        // The take currently being recorded.
        if self.shared.is_recording() && track.armed {
            let start = self.shared.rec_start.load(Ordering::Relaxed) as f64 / sr;
            let now = self.shared.playhead.load(Ordering::Relaxed) as f64 / sr;
            let r = Rect::from_min_max(
                Pos2::new(self.sec_to_x(start, lane_left), rect.top() + 2.0),
                Pos2::new(self.sec_to_x(now, lane_left), rect.bottom() - 2.0),
            );
            p.rect_filled(r, 3.0, REC.gamma_multiply(0.35));
            p.rect_stroke(r, 3.0, Stroke::new(1.0, REC), StrokeKind::Inside);
        }
        p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, BG));

        let track_id = track.id;
        if let Some(pos) = click_pos {
            match hit {
                Some(ci) => self.view.selected_clip = Some((track_id, ci)),
                None => {
                    self.view.selected_clip = None;
                    self.locate_x(pos.x, lane_left);
                }
            }
        }
    }

    // --------------------------------------------------------------------- dialogs

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.fx_windows(ctx);
        if self.view.show_settings {
            let mut open = true;
            egui::Window::new("Audio Settings")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .default_width(420.0)
                .show(ctx, |ui| self.settings_ui(ui));
            self.view.show_settings &= open;
        }

        if self.view.show_open {
            let mut open = true;
            let mut chosen = None;
            egui::Window::new("Open Session").open(&mut open).collapsible(false).default_width(420.0).show(ctx, |ui| {
                ui.label(egui::RichText::new(session::sessions_root().display().to_string()).color(TEXT_DIM).small());
                ui.add_space(4.0);
                if self.view.sessions.is_empty() {
                    ui.label("No sessions yet.");
                }
                egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                    for path in &self.view.sessions {
                        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                        if ui.add_sized([380.0, 24.0], egui::Button::new(name)).clicked() {
                            chosen = Some(path.clone());
                        }
                    }
                });
            });
            self.view.show_open &= open;
            if let Some(path) = chosen {
                self.view.show_open = false;
                if let Err(e) = self.open_session(&path) {
                    self.say(format!("Couldn't open session: {e:#}"));
                }
            }
        }

        if self.view.show_new {
            let mut open = true;
            let mut create = false;
            egui::Window::new("New Session").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name");
                    let r = ui.text_edit_singleline(&mut self.view.new_name);
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        create = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Sample rate");
                    ComboBox::from_id_salt("newrate").selected_text(format!("{} kHz", self.view.new_rate as f32 / 1000.0)).show_ui(ui, |ui| {
                        for r in [44_100, 48_000, 88_200, 96_000] {
                            ui.selectable_value(&mut self.view.new_rate, r, format!("{} kHz", r as f32 / 1000.0));
                        }
                    });
                });
                ui.label(egui::RichText::new("48 kHz is a good default. Your interface must support the rate you pick.").color(TEXT_DIM).small());
                ui.add_space(6.0);
                if ui.button("Create").clicked() {
                    create = true;
                }
            });
            self.view.show_new &= open;
            if create {
                self.view.show_new = false;
                let name = self.view.new_name.clone();
                self.new_session(&name, self.view.new_rate);
            }
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let hosts: Vec<String> = {
            let mut h: Vec<String> = self.view.devices.iter().map(|d| d.host.clone()).collect();
            h.dedup();
            h
        };
        let d = &mut self.view.draft;
        egui::Grid::new("audio-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Driver type");
            ComboBox::from_id_salt("host")
                .width(260.0)
                .selected_text(d.host.clone().unwrap_or_else(|| "System default".into()))
                .show_ui(ui, |ui| {
                    if ui.selectable_label(d.host.is_none(), "System default").clicked() {
                        d.host = None;
                        d.input = None;
                        d.output = None;
                    }
                    for h in &hosts {
                        if ui.selectable_label(d.host.as_deref() == Some(h), h).clicked() {
                            d.host = Some(h.clone());
                            d.input = None;
                            d.output = None;
                        }
                    }
                });
            ui.end_row();

            let host_matches = |dev_host: &str| match &d.host {
                Some(h) => h == dev_host,
                None => dev_host == "WASAPI" || hosts.len() == 1,
            };
            let is_asio = d.host.as_deref().is_some_and(|h| h.eq_ignore_ascii_case("asio"));

            ui.label(if is_asio { "Device" } else { "Input" });
            let inputs: Vec<_> = self.view.devices.iter().filter(|x| host_matches(&x.host) && x.max_inputs > 0).collect();
            ComboBox::from_id_salt("input")
                .width(260.0)
                .selected_text(d.input.clone().unwrap_or_else(|| "Default".into()))
                .show_ui(ui, |ui| {
                    if ui.selectable_label(d.input.is_none(), "Default").clicked() {
                        d.input = None;
                    }
                    for x in &inputs {
                        let label = format!("{}  ({} in)", x.name, x.max_inputs);
                        if ui.selectable_label(d.input.as_deref() == Some(&x.name), label).clicked() {
                            d.input = Some(x.name.clone());
                        }
                    }
                });
            ui.end_row();

            if !is_asio {
                ui.label("Output");
                let outputs: Vec<_> = self.view.devices.iter().filter(|x| host_matches(&x.host) && x.max_outputs > 0).collect();
                ComboBox::from_id_salt("output")
                    .width(260.0)
                    .selected_text(d.output.clone().unwrap_or_else(|| "Default".into()))
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(d.output.is_none(), "Default").clicked() {
                            d.output = None;
                        }
                        for x in &outputs {
                            if ui.selectable_label(d.output.as_deref() == Some(&x.name), &x.name).clicked() {
                                d.output = Some(x.name.clone());
                            }
                        }
                    });
                ui.end_row();
            }

            ui.label("Buffer size");
            let buf_label = |b: Option<u32>| b.map_or("Driver default".to_string(), |b| format!("{b} samples"));
            ComboBox::from_id_salt("buffer").width(260.0).selected_text(buf_label(d.buffer)).show_ui(ui, |ui| {
                for b in [None, Some(64), Some(128), Some(256), Some(512), Some(1024)] {
                    ui.selectable_value(&mut d.buffer, b, buf_label(b));
                }
            });
            ui.end_row();

            ui.label("Recording offset");
            ui.add(egui::DragValue::new(&mut d.record_adjust_ms).range(-100.0..=100.0).speed(0.1).suffix(" ms"))
                .on_hover_text("Fine-tune where new recordings land. Positive = earlier.");
            ui.end_row();

            ui.label("Click volume");
            ui.add(Slider::new(&mut d.click_volume, 0.0..=1.0).show_value(false));
            ui.end_row();
        });

        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "ASIO gives the lowest delay. If your interface has no ASIO driver, use WASAPI and a larger buffer. \
                 Smaller buffers = less delay but more risk of dropouts.",
            )
            .color(TEXT_DIM)
            .small(),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let busy = self.busy();
            if ui.add_enabled(!busy, egui::Button::new("Apply")).on_disabled_hover_text("Stop playback first").clicked() {
                let device_changed = {
                    let (a, b) = (&self.settings, &self.view.draft);
                    a.host != b.host || a.input != b.input || a.output != b.output || a.buffer != b.buffer
                };
                let last = self.settings.last_session.clone();
                self.settings = self.view.draft.clone();
                self.settings.last_session = last;
                self.settings.save();
                if device_changed || self.engine.is_none() {
                    self.restart_engine();
                }
                match &self.engine_error {
                    Some(e) => self.say(format!("Couldn't open the device: {e}")),
                    None => self.say("Audio settings applied."),
                }
            }
            if ui.button("Refresh devices").clicked() {
                self.refresh_devices();
            }
        });
    }
}

// ------------------------------------------------------------------------- widgets

#[derive(Clone, Copy)]
enum Icon {
    ToStart,
    Play,
    Stop,
    Record,
}

fn icon_button(ui: &mut egui::Ui, icon: Icon, active: bool, active_color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(38.0, 30.0), Sense::click());
    let bg = if active {
        active_color.gamma_multiply(0.35)
    } else if resp.hovered() {
        HEADER.gamma_multiply(1.4)
    } else {
        HEADER
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(4), bg);
    let c = rect.center();
    let fg = if active { active_color } else { Color32::from_gray(220) };
    match icon {
        Icon::Play => {
            p.add(Shape::convex_polygon(
                vec![c + Vec2::new(-5.0, -7.0), c + Vec2::new(7.0, 0.0), c + Vec2::new(-5.0, 7.0)],
                fg,
                Stroke::NONE,
            ));
        }
        Icon::Stop => {
            p.rect_filled(Rect::from_center_size(c, Vec2::splat(12.0)), 1.0, fg);
        }
        Icon::Record => {
            p.circle_filled(c, 7.0, if active { active_color } else { REC });
        }
        Icon::ToStart => {
            p.rect_filled(Rect::from_center_size(c + Vec2::new(-6.0, 0.0), Vec2::new(2.5, 12.0)), 0.0, fg);
            p.add(Shape::convex_polygon(
                vec![c + Vec2::new(6.0, -6.0), c + Vec2::new(6.0, 6.0), c + Vec2::new(-4.0, 0.0)],
                fg,
                Stroke::NONE,
            ));
        }
    }
    resp
}

/// A small square toggle like Logic's M / S / R buttons.
pub(crate) fn small_toggle(ui: &mut egui::Ui, value: &mut bool, text: &str, on_color: Color32, tip: &str) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(20.0, 18.0), Sense::click());
    if resp.clicked() && ui.is_enabled() {
        *value = !*value;
        resp.mark_changed();
    }
    let p = ui.painter();
    let bg = if *value { on_color } else { Color32::from_gray(0x44) };
    p.rect_filled(rect, CornerRadius::same(3), if ui.is_enabled() { bg } else { bg.gamma_multiply(0.5) });
    let fg = if *value { Color32::BLACK } else { Color32::from_gray(210) };
    p.text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(11.0), fg);
    resp.on_hover_text(tip)
}
