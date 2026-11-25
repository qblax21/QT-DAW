//! QT DAW desktop app.
//!
//! - `app.rs`: the app state and everything that changes the session or engine.
//! - `ui.rs`: drawing the window (control bar, tracks, timeline, dialogs).
//! - `settings.rs`: per-computer settings (audio device, last session).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod settings;
mod ui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("QT DAW")
            .with_inner_size([1400.0, 860.0])
            .with_min_inner_size([900.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native("QT DAW", options, Box::new(|cc| Ok(Box::new(app::QtDaw::new(cc)))))
}
