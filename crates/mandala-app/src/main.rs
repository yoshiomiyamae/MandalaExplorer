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

use crate::lang::Language;
use std::path::PathBuf;

/// Shown in the title bar and the taskbar. Matches the name reserved in the
/// Store and the DisplayName in packaging/AppxManifest.xml -- an installed app
/// whose window disagrees with its Start menu entry looks like two programs.
///
/// Also what eframe derives its settings directory from, so changing it starts
/// the settings over. Worth it once, to stop the two names disagreeing.
const APP_NAME: &str = "Mandala Explorer";

/// What the command line asked for. Both parts are optional; whatever is
/// missing is decided by the machine rather than by the person starting it.
#[derive(Debug, Default, PartialEq)]
struct Launch {
    start: Option<PathBuf>,
    language: Option<Language>,
}

/// Reads `[--lang <tag>] [folder]`, in either order.
///
/// The language flag exists for recording: the trailer has to be captured in
/// each language the Store listing offers, and changing the Windows display
/// language to do that costs a sign-out. Anything unrecognised is skipped
/// rather than treated as an error -- a file browser that refuses to start
/// because of a stray argument helps nobody.
fn parse_launch(args: impl IntoIterator<Item = String>) -> Launch {
    let mut launch = Launch::default();
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        if let Some(tag) = arg.strip_prefix("--lang=") {
            launch.language = Some(Language::from_tags([tag]));
        } else if arg == "--lang" {
            // Only consume what follows if there is something there.
            if let Some(tag) = args.next() {
                launch.language = Some(Language::from_tags([tag.as_str()]));
            }
        } else if !arg.starts_with('-') && launch.start.is_none() {
            launch.start = Some(PathBuf::from(arg));
        }
    }
    launch
}

fn main() -> eframe::Result {
    let launch = parse_launch(std::env::args().skip(1));

    let start = launch
        .start
        .filter(|p| p.is_dir())
        .or_else(dirs::picture_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("C:\\"));

    let language = launch.language.unwrap_or_else(Language::from_system);

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
        Box::new(move |cc| Ok(Box::new(app::MandalaApp::new(cc, start, language)?))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Launch {
        parse_launch(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_asks_for_nothing() {
        let launch = parse(&[]);
        assert_eq!(launch.start, None);
        assert_eq!(launch.language, None);
    }

    #[test]
    fn a_bare_path_is_the_folder_to_open() {
        assert_eq!(parse(&["R:\\Demo"]).start, Some(PathBuf::from("R:\\Demo")));
    }

    #[test]
    fn the_language_flag_takes_either_spelling() {
        assert_eq!(parse(&["--lang", "ja"]).language, Some(Language::Japanese));
        assert_eq!(parse(&["--lang=ja"]).language, Some(Language::Japanese));
        assert_eq!(parse(&["--lang", "en-GB"]).language, Some(Language::English));
    }

    #[test]
    fn the_flag_and_the_folder_do_not_care_which_comes_first() {
        for args in [["--lang", "ja", "R:\\Demo"], ["R:\\Demo", "--lang", "ja"]] {
            let launch = parse(&args);
            assert_eq!(launch.start, Some(PathBuf::from("R:\\Demo")), "{args:?}");
            assert_eq!(launch.language, Some(Language::Japanese), "{args:?}");
        }
    }

    #[test]
    fn a_flag_with_nothing_after_it_is_ignored_rather_than_fatal() {
        // Someone typing the flag and forgetting the tag should still get their
        // browser, in whatever language Windows is set to.
        let launch = parse(&["--lang"]);
        assert_eq!(launch.language, None);
        assert_eq!(launch.start, None);
    }

    #[test]
    fn a_language_we_do_not_speak_falls_back_rather_than_refusing() {
        assert_eq!(parse(&["--lang", "fr-FR"]).language, Some(Language::English));
    }

    #[test]
    fn the_value_of_the_flag_is_never_mistaken_for_a_folder() {
        assert_eq!(parse(&["--lang", "ja"]).start, None);
    }
}
