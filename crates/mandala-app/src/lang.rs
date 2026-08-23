//! The words the interface uses, in the languages it speaks.
//!
//! Small enough to be a match rather than a framework: there are a dozen
//! strings and no plurals, dates or numbers to agree with. A translation file
//! format would be more machinery than the thing being translated.
//!
//! Sort keys get their wording here rather than in `mandala_core`, which has no
//! business knowing what language anyone reads.

use mandala_core::sort::{SortKey, SortOrder};

/// A language the interface is available in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    #[default]
    English,
    Japanese,
}

impl Language {
    /// Picks a language from a list of BCP 47 tags such as `ja-JP` or
    /// `en-GB`, most preferred first.
    ///
    /// Walks the whole list instead of reading only the front of it: a list of
    /// `["fr-FR", "ja-JP"]` belongs to someone who reads Japanese and would
    /// rather have it than the English fallback, and Windows hands us the list
    /// precisely so we can make that choice.
    pub fn from_tags<'a>(tags: impl IntoIterator<Item = &'a str>) -> Self {
        tags.into_iter().find_map(Self::spoken).unwrap_or_default()
    }

    /// The language a tag names, or `None` if the interface does not speak it.
    ///
    /// Matches on the primary subtag, so `ja-JP` and `ja` agree, as do `en-GB`
    /// and `en-US`.
    fn spoken(tag: &str) -> Option<Self> {
        let primary = tag.split(['-', '_']).next().unwrap_or_default();
        if primary.eq_ignore_ascii_case("ja") {
            Some(Self::Japanese)
        } else if primary.eq_ignore_ascii_case("en") {
            Some(Self::English)
        } else {
            None
        }
    }

    /// The language Windows is set to.
    pub fn from_system() -> Self {
        Self::from_tags(system_tags().iter().map(String::as_str))
    }
}

/// The languages the user reads Windows in, most preferred first.
///
/// Deliberately the display languages rather than the regional format: those
/// are separate settings and can disagree, and only the first says anything
/// about what the person wants to read. It is also a ranked list rather than a
/// single value, which is what lets [`Language::from_tags`] fall through to a
/// second choice we do speak.
#[cfg(windows)]
fn system_tags() -> Vec<String> {
    use windows::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
    use windows::core::PWSTR;

    let mut languages = 0u32;
    let mut len = 0u32;
    // The first call sizes the buffer, the second fills it.
    let sized =
        unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut languages, None, &mut len) };
    if sized.is_err() {
        return Vec::new();
    }
    let mut buffer = vec![0u16; len as usize];
    let filled = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut languages,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut len,
        )
    };
    if filled.is_err() {
        return Vec::new();
    }

    // Null-separated and null-terminated, which leaves an empty last split.
    String::from_utf16_lossy(&buffer)
        .split('\0')
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(not(windows))]
fn system_tags() -> Vec<String> {
    std::env::var("LANG").into_iter().collect()
}

/// Every piece of text the interface shows.
///
/// An enum rather than string keys, so a language missing a phrase is a
/// compile error instead of a blank label someone notices in a screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phrase {
    Size,
    Autoplay,
    AtOnce,
    Sort,
    Names,
    Ascending,
    Descending,
    UpOneLevel,
    SortByName,
    SortByType,
    SortBySize,
    SortByModified,
    SortByLength,
    Filter,
    Bookmark,
    BookmarkAdd,
    BookmarkRemove,
    BookmarksEmpty,
    Reload,
}

impl Language {
    pub fn text(self, phrase: Phrase) -> &'static str {
        use Language::{English, Japanese};
        use Phrase::*;
        match (self, phrase) {
            (English, Size) => "Size",
            (English, Autoplay) => "Autoplay",
            (English, AtOnce) => "at once",
            (English, Sort) => "Sort",
            (English, Names) => "Names",
            (English, Ascending) => "Ascending",
            (English, Descending) => "Descending",
            (English, UpOneLevel) => "Up one level",
            (English, SortByName) => "Name",
            (English, SortByType) => "Type",
            (English, SortBySize) => "Size",
            (English, SortByModified) => "Modified",
            (English, SortByLength) => "Length",
            (English, Filter) => "Filter",
            (English, Bookmark) => "Bookmarks",
            (English, BookmarkAdd) => "Keep this folder",
            (English, BookmarkRemove) => "Stop keeping this folder",
            (English, BookmarksEmpty) => "No folders kept yet",
            (English, Reload) => "Read this folder again (F5)",

            (Japanese, Size) => "サイズ",
            (Japanese, Autoplay) => "自動再生",
            (Japanese, AtOnce) => "同時",
            (Japanese, Sort) => "並べ替え",
            (Japanese, Names) => "ファイル名",
            (Japanese, Ascending) => "昇順",
            (Japanese, Descending) => "降順",
            (Japanese, UpOneLevel) => "上の階層へ",
            (Japanese, SortByName) => "名前",
            (Japanese, SortByType) => "種類",
            (Japanese, SortBySize) => "サイズ",
            (Japanese, SortByModified) => "更新日時",
            (Japanese, SortByLength) => "長さ",
            (Japanese, Filter) => "絞り込み",
            (Japanese, Bookmark) => "ブックマーク",
            (Japanese, BookmarkAdd) => "このフォルダーを登録",
            (Japanese, BookmarkRemove) => "このフォルダーの登録を解除",
            (Japanese, BookmarksEmpty) => "登録されたフォルダーはありません",
            (Japanese, Reload) => "このフォルダーを読み直す (F5)",
        }
    }

    /// Wording for a sort key, which lives here rather than in `mandala_core`.
    pub fn sort_key(self, key: SortKey) -> &'static str {
        self.text(match key {
            SortKey::Name => Phrase::SortByName,
            SortKey::Kind => Phrase::SortByType,
            SortKey::Size => Phrase::SortBySize,
            SortKey::Modified => Phrase::SortByModified,
            SortKey::Duration => Phrase::SortByLength,
        })
    }

    pub fn sort_order(self, order: SortOrder) -> &'static str {
        self.text(match order {
            SortOrder::Ascending => Phrase::Ascending,
            SortOrder::Descending => Phrase::Descending,
        })
    }

    /// How many items are in the folder, and how many of them are video.
    ///
    /// A method rather than a phrase because the two languages put the numbers
    /// in different places, which a format string with positional arguments
    /// would only pretend to handle.
    pub fn item_summary(self, items: usize, videos: usize) -> String {
        match self {
            Language::English => format!("{items} items, {videos} video"),
            Language::Japanese => format!("{items} 件中 {videos} 件が動画"),
        }
    }

    /// The same, while a filter is hiding some of the folder.
    ///
    /// Separate wording rather than the plain count, because a count that
    /// silently means "of the ones you can see" is how someone concludes their
    /// files have gone missing.
    pub fn filtered_summary(self, shown: usize, total: usize, videos: usize) -> String {
        match self {
            Language::English => format!("{shown} of {total} shown, {videos} video"),
            Language::Japanese => format!("{total} 件中 {shown} 件を表示、うち動画 {videos} 件"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Language; 2] = [Language::English, Language::Japanese];
    const PHRASES: [Phrase; 19] = [
        Phrase::Size,
        Phrase::Autoplay,
        Phrase::AtOnce,
        Phrase::Sort,
        Phrase::Names,
        Phrase::Ascending,
        Phrase::Descending,
        Phrase::UpOneLevel,
        Phrase::SortByName,
        Phrase::SortByType,
        Phrase::SortBySize,
        Phrase::SortByModified,
        Phrase::SortByLength,
        Phrase::Filter,
        Phrase::Bookmark,
        Phrase::BookmarkAdd,
        Phrase::BookmarkRemove,
        Phrase::BookmarksEmpty,
        Phrase::Reload,
    ];

    #[test]
    fn every_phrase_has_words_in_every_language() {
        for language in ALL {
            for phrase in PHRASES {
                assert!(
                    !language.text(phrase).trim().is_empty(),
                    "{language:?} has nothing for {phrase:?}"
                );
            }
        }
    }

    #[test]
    fn the_languages_actually_differ() {
        // A copy-pasted table that forgot to be translated would still pass the
        // test above.
        let untranslated: Vec<_> = PHRASES
            .into_iter()
            .filter(|p| Language::English.text(*p) == Language::Japanese.text(*p))
            .collect();
        assert!(untranslated.is_empty(), "left in English: {untranslated:?}");
    }

    #[test]
    fn japanese_tags_choose_japanese() {
        for tag in ["ja", "ja-JP", "ja_JP", "JA-jp"] {
            assert_eq!(Language::from_tags([tag]), Language::Japanese, "{tag}");
        }
    }

    #[test]
    fn everything_else_falls_back_to_english() {
        for tag in ["en-US", "en-GB", "de-DE", "zh-Hans-CN", "", "not a tag"] {
            assert_eq!(Language::from_tags([tag]), Language::English, "{tag}");
        }
    }

    #[test]
    fn a_supported_language_wins_even_from_further_down_the_list() {
        // Someone whose first choice we do not speak still gets their second.
        assert_eq!(Language::from_tags(["fr-FR", "ja-JP", "en-US"]), Language::Japanese);
    }

    #[test]
    fn the_order_of_the_list_is_respected() {
        assert_eq!(Language::from_tags(["en-GB", "ja-JP"]), Language::English);
        assert_eq!(Language::from_tags(["ja-JP", "en-GB"]), Language::Japanese);
    }

    #[test]
    fn a_list_of_languages_we_do_not_speak_falls_back_to_english() {
        assert_eq!(Language::from_tags(["fr-FR", "de-DE"]), Language::English);
        assert_eq!(Language::from_tags([]), Language::English);
    }

    #[test]
    fn a_filtered_count_says_what_it_is_counting() {
        // Both numbers have to be there. "48 items" while a filter is on reads
        // as the folder having lost the rest of them.
        for language in ALL {
            let summary = language.filtered_summary(12, 340, 5);
            assert!(summary.contains("12"), "{summary}");
            assert!(summary.contains("340"), "{summary}");
            assert!(summary.contains('5'), "{summary}");
        }
    }

    #[test]
    fn the_filtered_count_differs_from_the_plain_one() {
        assert_ne!(
            Language::Japanese.item_summary(12, 5),
            Language::Japanese.filtered_summary(12, 340, 5)
        );
    }

    #[test]
    fn sort_keys_are_worded_in_both_languages() {
        for key in SortKey::ALL {
            assert_ne!(
                Language::English.sort_key(key),
                Language::Japanese.sort_key(key),
                "{key:?} reads the same in both"
            );
        }
    }

    #[test]
    fn the_summary_carries_both_numbers() {
        for language in ALL {
            let text = language.item_summary(72, 48);
            assert!(text.contains("72"), "{language:?} lost the item count: {text}");
            assert!(text.contains("48"), "{language:?} lost the video count: {text}");
        }
    }

    #[test]
    fn the_system_language_is_one_we_have() {
        // Whatever Windows says, this has to resolve to something drawable.
        let picked = Language::from_system();
        assert!(ALL.contains(&picked));
    }
}
