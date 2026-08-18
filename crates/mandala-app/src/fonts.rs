//! Making non-Latin filenames render as text rather than boxes.
//!
//! egui ships with Latin coverage only, so a folder of Japanese filenames comes
//! out as a row of tofu. Rather than embedding a font -- CJK faces run to tens
//! of megabytes -- the system ones are loaded at startup and appended as
//! fallbacks, leaving egui's own font in charge of everything it can already
//! draw.

use eframe::egui::{Context, FontData, FontDefinitions, FontFamily, FontTweak};
use std::path::Path;
use std::sync::Arc;

const FONT_DIR: &str = r"C:\Windows\Fonts";

/// A system font to fall back to.
struct Fallback {
    file: &'static str,
    /// Name it is registered under. Faces sharing a name replace each other, so
    /// the alternatives for one script all use the same one.
    name: &'static str,
    /// Downward shift as a fraction of the font size, to line this face up with
    /// the Latin font egui draws with.
    ///
    /// Measured per font rather than shared, because every face puts its
    /// baseline somewhere slightly different. At 12pt, Yu Gothic sat three
    /// pixels high -- which showed as the ".png" sliding below the name it
    /// belonged to -- while Malgun Gothic and YaHei already agreed with egui.
    nudge: f32,
}

/// Nudges measured off a 12pt label, as a fraction so they hold at any size.
/// Positive moves glyphs down.
///
/// Measured by rendering a name in one script, finding the lowest lit row of
/// each run of glyphs, and comparing that with the Latin run beside it. That
/// works when the script has characters reaching down to the baseline -- hiragana
/// does -- and not otherwise, which is why only two of the three are set here.
const YU_GOTHIC_NUDGE: f32 = 3.0 / 12.0;

/// Malgun Gothic sits low rather than high, hence the sign. Hangul has no one
/// baseline to measure against, since syllables with a final consonant reach
/// further down than those without, so this was matched against the Latin text
/// beside it rather than to the pixel.
const MALGUN_NUDGE: f32 = -3.0 / 12.0;

/// Left alone: Han characters do not reach the baseline, so the measurement
/// that worked for the other two says nothing here, and a guessed value is
/// worse than none. In practice this font only draws the simplified forms Yu
/// Gothic has no glyph for, which on a Japanese machine is a rare sight.
const YAHEI_NUDGE: f32 = 0.0;

/// Japanese faces in preference order. Only the first one present is loaded:
/// they cover the same characters, and each is a good 10 MB.
const JAPANESE: &[Fallback] = &[
    Fallback { file: "YuGothR.ttc", name: "japanese", nudge: YU_GOTHIC_NUDGE },
    // The older two are unmeasured; they only come up on a machine without Yu
    // Gothic, where being a pixel out still beats rendering boxes.
    Fallback { file: "meiryo.ttc", name: "japanese", nudge: YU_GOTHIC_NUDGE },
    Fallback { file: "msgothic.ttc", name: "japanese", nudge: YU_GOTHIC_NUDGE },
];

/// Other scripts, each loaded when present.
///
/// Japanese comes first in the fallback chain deliberately. Han characters are
/// unified in Unicode but drawn differently in Japanese and Chinese, and the
/// first font holding a glyph is the one that gets used -- so on a Japanese
/// machine, Japanese shapes should win. Chinese here is what covers the
/// simplified forms Yu Gothic has no glyph for.
const OTHER_SCRIPTS: &[Fallback] = &[
    Fallback { file: "malgun.ttf", name: "korean", nudge: MALGUN_NUDGE },
    Fallback { file: "msyh.ttc", name: "chinese", nudge: YAHEI_NUDGE },
];

/// Loads system fallback fonts into `ctx`.
///
/// Missing fonts are not an error: the app still runs, some characters just
/// stay as boxes, so it says so rather than failing to start.
pub fn install_fallbacks(ctx: &Context) {
    let dir = Path::new(FONT_DIR);
    let mut fonts = FontDefinitions::default();
    let mut loaded = Vec::new();

    if let Some(japanese) = first_present(dir, JAPANESE)
        && let Some(name) = load(&mut fonts, dir, japanese)
    {
        loaded.push(name);
    }
    for fallback in OTHER_SCRIPTS {
        if dir.join(fallback.file).is_file()
            && let Some(name) = load(&mut fonts, dir, fallback)
        {
            loaded.push(name);
        }
    }

    if loaded.is_empty() {
        eprintln!("no system fallback fonts found; non-Latin text will show as boxes");
        return;
    }

    // Appended, not prepended: egui's own font keeps drawing everything it
    // covers, and these only get consulted for what it does not.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        let chain = fonts.families.entry(family).or_default();
        chain.extend(loaded.iter().cloned());
    }
    ctx.set_fonts(fonts);
}

/// First fallback in the list whose file exists in `dir`.
fn first_present<'a>(dir: &Path, candidates: &'a [Fallback]) -> Option<&'a Fallback> {
    candidates.iter().find(|fallback| dir.join(fallback.file).is_file())
}

/// Reads a font file into the definitions, returning the name it went in under.
fn load(fonts: &mut FontDefinitions, dir: &Path, fallback: &Fallback) -> Option<String> {
    let bytes = std::fs::read(dir.join(fallback.file)).ok()?;
    // Face 0 of a collection is the regular weight, which is what body text
    // wants; egui synthesises nothing else from it.
    let data = FontData::from_owned(bytes)
        .tweak(FontTweak { y_offset_factor: fallback.nudge, ..Default::default() });
    fonts.font_data.insert(fallback.name.to_owned(), Arc::new(data));
    Some(fallback.name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANDIDATES: &[Fallback] = &[
        Fallback { file: "first.ttc", name: "japanese", nudge: 0.1 },
        Fallback { file: "second.ttc", name: "japanese", nudge: 0.2 },
        Fallback { file: "third.ttc", name: "japanese", nudge: 0.3 },
    ];

    #[test]
    fn picks_the_first_candidate_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("second.ttc"), b"font").unwrap();
        std::fs::write(dir.path().join("third.ttc"), b"font").unwrap();

        let got = first_present(dir.path(), CANDIDATES).expect("should find one");
        assert_eq!(got.file, "second.ttc");
    }

    #[test]
    fn reports_nothing_when_no_candidate_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(first_present(dir.path(), CANDIDATES).is_none());
    }

    #[test]
    fn a_directory_named_like_a_font_is_not_mistaken_for_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("first.ttc")).unwrap();
        assert!(first_present(dir.path(), CANDIDATES).is_none());
    }

    #[test]
    fn loading_registers_the_font_under_its_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fake.ttf"), b"bytes standing in for a font").unwrap();

        let mut fonts = FontDefinitions::default();
        let fallback = Fallback { file: "fake.ttf", name: "japanese", nudge: 0.25 };
        assert_eq!(load(&mut fonts, dir.path(), &fallback), Some("japanese".to_owned()));
        assert!(fonts.font_data.contains_key("japanese"));
    }

    #[test]
    fn each_font_carries_its_own_baseline_nudge() {
        // The whole reason for per-font values: one shared number left Korean
        // and Chinese three pixels low while Japanese was correct.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("jp.ttf"), b"bytes").unwrap();
        std::fs::write(dir.path().join("kr.ttf"), b"bytes").unwrap();

        let mut fonts = FontDefinitions::default();
        load(&mut fonts, dir.path(), &Fallback { file: "jp.ttf", name: "japanese", nudge: 0.25 });
        load(&mut fonts, dir.path(), &Fallback { file: "kr.ttf", name: "korean", nudge: 0.0 });

        assert_eq!(fonts.font_data["japanese"].tweak.y_offset_factor, 0.25);
        assert_eq!(fonts.font_data["korean"].tweak.y_offset_factor, 0.0);
    }

    #[test]
    fn loading_a_missing_file_reports_nothing() {
        let mut fonts = FontDefinitions::default();
        let fallback = Fallback { file: "absent.ttf", name: "japanese", nudge: 0.0 };
        assert_eq!(load(&mut fonts, Path::new("no/such/dir"), &fallback), None);
    }

    #[test]
    fn alternatives_for_one_script_share_a_name() {
        // They replace each other in the font map, so only one can ever load.
        assert!(JAPANESE.iter().all(|f| f.name == JAPANESE[0].name));
    }

    #[test]
    fn this_machine_has_a_japanese_font_to_fall_back_to() {
        // Not a hard requirement -- the app runs without one -- but its absence
        // means every Japanese filename renders as boxes.
        if first_present(Path::new(FONT_DIR), JAPANESE).is_none() {
            eprintln!("warning: no Japanese system font found in {FONT_DIR}");
        }
    }
}
