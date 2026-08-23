//! The folders worth coming back to.

use std::path::{Path, PathBuf};

/// How many folders are kept.
///
/// A shortlist that has to be scrolled is not a shortlist. The oldest is
/// dropped when a new one arrives past this, which is the behaviour that needs
/// no explaining: the folder you just marked is always there.
pub const MAX_BOOKMARKS: usize = 24;

/// Whether a folder is already on the list.
pub fn contains(bookmarks: &[PathBuf], path: &Path) -> bool {
    bookmarks.iter().any(|kept| kept == path)
}

/// Adds a folder, or removes it if it was already there.
///
/// One control rather than two: the same button says whether this folder is
/// kept and changes its mind, which is how every application that has this
/// feature behaves.
pub fn toggle(bookmarks: &mut Vec<PathBuf>, path: &Path) {
    if let Some(at) = bookmarks.iter().position(|kept| kept == path) {
        bookmarks.remove(at);
        return;
    }
    bookmarks.push(path.to_path_buf());
    while bookmarks.len() > MAX_BOOKMARKS {
        bookmarks.remove(0);
    }
}

/// Tidies a list loaded from disk.
///
/// Duplicates go and the length is capped, but a folder that is not there
/// right now stays. A bookmark to a share behind a VPN, or to a drive that is
/// not plugged in, is not a mistake to be cleaned up -- deleting it because the
/// application happened to start while the network was down would be the
/// mistake, and the person who marked it would have no way to know why it went.
pub fn sanitized(bookmarks: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for path in bookmarks {
        if !contains(&seen, &path) {
            seen.push(path);
        }
    }
    let excess = seen.len().saturating_sub(MAX_BOOKMARKS);
    seen.drain(..excess);
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[PathBuf]) -> Vec<String> {
        list.iter().map(|p| p.display().to_string()).collect()
    }

    fn p(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn toggling_adds_then_removes() {
        let mut marks = Vec::new();
        toggle(&mut marks, &p("photos"));
        assert_eq!(paths(&marks), ["photos"]);
        toggle(&mut marks, &p("photos"));
        assert!(marks.is_empty());
    }

    #[test]
    fn the_newest_is_kept_and_the_oldest_dropped() {
        let mut marks: Vec<PathBuf> = (0..MAX_BOOKMARKS).map(|i| p(&format!("f{i}"))).collect();
        toggle(&mut marks, &p("newest"));
        assert_eq!(marks.len(), MAX_BOOKMARKS);
        assert!(contains(&marks, &p("newest")), "the folder just marked must be there");
        assert!(!contains(&marks, &p("f0")), "the oldest goes");
    }

    #[test]
    fn removing_one_from_the_middle_leaves_the_order_alone() {
        let mut marks = vec![p("a"), p("b"), p("c")];
        toggle(&mut marks, &p("b"));
        assert_eq!(paths(&marks), ["a", "c"]);
    }

    #[test]
    fn a_list_from_disk_loses_its_duplicates() {
        let marks = sanitized(vec![p("a"), p("b"), p("a"), p("c"), p("b")]);
        assert_eq!(paths(&marks), ["a", "b", "c"]);
    }

    #[test]
    fn a_list_from_disk_is_capped_at_the_newest() {
        let marks: Vec<PathBuf> = (0..MAX_BOOKMARKS + 5).map(|i| p(&format!("f{i}"))).collect();
        let marks = sanitized(marks);
        assert_eq!(marks.len(), MAX_BOOKMARKS);
        assert!(contains(&marks, &p(&format!("f{}", MAX_BOOKMARKS + 4))));
        assert!(!contains(&marks, &p("f0")));
    }

    #[test]
    fn a_folder_that_is_not_there_right_now_is_kept() {
        // A share behind a VPN, a drive not plugged in. Dropping these would
        // lose someone's bookmarks for reasons they cannot see.
        let marks = sanitized(vec![p(r"\\server\share\photos"), p("Z:\\gone")]);
        assert_eq!(marks.len(), 2);
    }
}
