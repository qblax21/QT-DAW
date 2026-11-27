//! QT DAW desktop app.
//!
//! - `app.rs`: the app state and everything that changes the session or engine.
//! - `ui.rs`: drawing the window (control bar, tracks, timeline, dialogs).
//! - `settings.rs`: per-computer settings (audio device, last session).
//! - `remote.rs` + `claude.rs`: the local connection Claude uses, and its mixing tools.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod claude;
mod fxui;
mod mixer;
mod remote;
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
