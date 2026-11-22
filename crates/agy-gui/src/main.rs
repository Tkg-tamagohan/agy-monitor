#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod fonts;
mod theme;

use agy_core::paths::GeminiPaths;
use eframe::egui;

use crate::app::AgyMonitorApp;

fn main() -> eframe::Result<()> {
    let paths = GeminiPaths::discover().unwrap_or_else(|_| GeminiPaths::from_gemini_dir(".gemini"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Antigravity Monitor")
            .with_inner_size([1240.0, 820.0])
            .with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Antigravity Monitor",
        options,
        Box::new(move |cc| Ok(Box::new(AgyMonitorApp::new(cc, paths)))),
    )
}
