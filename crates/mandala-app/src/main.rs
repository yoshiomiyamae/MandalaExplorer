//! mandala: a file browser for people whose folders are full of pictures and
//! video rather than documents.

// A file browser has no business flashing a console window behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cache;
mod fonts;
mod player;
mod slots;
mod thumbs;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let start = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(dirs::picture_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("C:\\"));

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([480.0, 320.0])
            .with_title("mandala"),
        ..Default::default()
    };

    eframe::run_native(
        "mandala",
        options,
        Box::new(|cc| Ok(Box::new(app::MandalaApp::new(cc, start)?))),
    )
}
