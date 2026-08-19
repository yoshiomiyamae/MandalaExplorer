//! mandala: a file browser for people whose folders are full of pictures and
//! video rather than documents.

// A file browser has no business flashing a console window behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cache;
mod fonts;
mod lang;
mod player;
mod thumbs;

use std::path::PathBuf;

/// Shown in the title bar and the taskbar. Matches the name reserved in the
/// Store and the DisplayName in packaging/AppxManifest.xml -- an installed app
/// whose window disagrees with its Start menu entry looks like two programs.
///
/// Also what eframe derives its settings directory from, so changing it starts
/// the settings over. Worth it once, to stop the two names disagreeing.
const APP_NAME: &str = "Mandala Explorer";

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
            .with_title(APP_NAME)
            // Ties the window to the installed package, so the taskbar groups
            // it under the Store entry rather than as a stray executable.
            .with_app_id("FamoceSuccellion.MandalaExplorer"),
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(app::MandalaApp::new(cc, start)?))),
    )
}
