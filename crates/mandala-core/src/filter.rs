//! Narrowing a folder to the names worth looking at.
//!
//! A folder of ten thousand photographs is exactly the folder this program is
//! for, and it is also the folder where scrolling stops being a way to find
//! anything. Typing part of a name is the cheapest way back.

/// Whether a name matches what was typed.
///
/// Case is ignored, and the query is split on whitespace so that every word
/// has to appear somewhere in the name -- in any order. Typing `beach 2026`
/// finds `2026-08-beach-trip.jpg`, which typing them as one string would not,
/// and remembering which order they were in is not something anyone should
/// have to do to find their own file.
///
/// Matching is on the whole name including the extension, so `mp4` narrows to
/// video without a separate control for it.
pub fn matches(name: &str, query: &str) -> bool {
    let mut terms = query.split_whitespace().peekable();
    if terms.peek().is_none() {
        // An empty query is not a filter that matches nothing; it is no filter.
        return true;
    }
    // Folded once rather than per term: a long name and several terms is the
    // case that happens while someone is still typing.
    let name = name.to_lowercase();
    terms.all(|term| name.contains(&term.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_query_keeps_everything() {
        assert!(matches("anything.jpg", ""));
        assert!(matches("anything.jpg", "   "));
    }

    #[test]
    fn a_substring_anywhere_in_the_name_counts() {
        assert!(matches("2026-08-beach.jpg", "beach"));
        assert!(matches("2026-08-beach.jpg", "2026"));
        assert!(matches("2026-08-beach.jpg", "08-be"));
        assert!(!matches("2026-08-beach.jpg", "mountain"));
    }

    #[test]
    fn case_is_ignored_in_both_directions() {
        assert!(matches("IMG_5199.MOV", "img"));
        assert!(matches("img_5199.mov", "IMG"));
        assert!(matches("Vacation.JPEG", "jPeG"));
    }

    #[test]
    fn every_word_has_to_appear_but_the_order_does_not() {
        assert!(matches("2026-08-beach-trip.jpg", "beach 2026"));
        assert!(matches("2026-08-beach-trip.jpg", "2026 beach"));
        assert!(matches("2026-08-beach-trip.jpg", "trip beach 08"));
        assert!(!matches("2026-08-beach-trip.jpg", "beach mountain"));
    }

    #[test]
    fn the_extension_is_part_of_the_name() {
        // So a filter doubles as a way to see only the videos.
        assert!(matches("holiday.mp4", "mp4"));
        assert!(!matches("holiday.jpg", "mp4"));
    }

    #[test]
    fn names_that_are_not_latin_match_too() {
        // No case to fold, but the substring still has to work -- and this is
        // the half of the world the folding rules were not written for.
        assert!(matches("運動会-2026.mp4", "運動会"));
        assert!(matches("運動会-2026.mp4", "動会 2026"));
        assert!(!matches("運動会-2026.mp4", "遠足"));
    }

    #[test]
    fn a_query_longer_than_the_name_matches_nothing_rather_than_panicking() {
        assert!(!matches("a.jpg", "a much longer query than the name"));
    }
}
