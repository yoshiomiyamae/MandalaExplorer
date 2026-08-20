//! Choosing what a folder's tile should show.
//!
//! A folder icon says nothing a filename does not. A handful of the pictures
//! inside says what the folder is, which is the same argument the whole program
//! rests on.

use crate::kind::MediaKind;
use crate::scan::Entry;

/// How many pictures a folder's tile is built from.
///
/// Four fills a square evenly. More would each be small enough that the tile
/// stops reading as a folder and starts reading as a contact sheet.
pub const COVER_TILES: usize = 4;

/// How many subfolders are opened to make up a shortfall.
///
/// A photo library sorted into dated folders has nothing at its top level but
/// more folders, and stopping there would leave every tile blank. Descending
/// one level answers that; a bound keeps it from turning a folder of a thousand
/// folders into a thousand directory reads.
pub const COVER_SUBFOLDERS: usize = 8;

/// Picks the entries whose thumbnails stand for a folder.
///
/// What is in the folder itself comes first, in the order given. `nested`
/// supplies the listings of its subfolders and is consumed lazily, so a caller
/// that reads a directory per item only pays for the ones actually needed --
/// and a folder whose own pictures already fill the tile costs no extra reads
/// at all.
///
/// Only one level down is considered. Deeper is a policy this cannot see the
/// cost of: the caller knows whether it is looking at a local disk or a share
/// on the other side of a VPN, and this does not.
pub fn cover<I>(direct: &[Entry], nested: I) -> Vec<Entry>
where
    I: IntoIterator<Item = Vec<Entry>>,
{
    let mut picked: Vec<Entry> = thumbnailable(direct).take(COVER_TILES).cloned().collect();
    if picked.len() == COVER_TILES {
        return picked;
    }

    for listing in nested {
        let room = COVER_TILES - picked.len();
        picked.extend(thumbnailable(&listing).take(room).cloned());
        if picked.len() == COVER_TILES {
            break;
        }
    }
    picked
}

/// The entries in a listing that can carry a thumbnail, in the order given.
///
/// Directories are not among them even though they now have thumbnails of
/// their own: building one folder's tile out of another folder's tile would
/// recurse, and a mosaic of mosaics shows nothing at tile size anyway.
fn thumbnailable(entries: &[Entry]) -> impl Iterator<Item = &Entry> {
    entries.iter().filter(|e| matches!(e.kind, MediaKind::Image | MediaKind::Video))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;

    fn entry(name: &str, kind: MediaKind) -> Entry {
        Entry { path: PathBuf::from(name), name: name.to_owned(), kind, len: 1, modified: None }
    }

    fn image(name: &str) -> Entry {
        entry(name, MediaKind::Image)
    }

    fn names(entries: &[Entry]) -> Vec<String> {
        entries.iter().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn takes_the_first_four_pictures_in_the_folder() {
        let direct = vec![image("a"), image("b"), image("c"), image("d"), image("e")];
        assert_eq!(names(&cover(&direct, [])), ["a", "b", "c", "d"]);
    }

    #[test]
    fn skips_what_cannot_carry_a_thumbnail() {
        let direct = vec![
            entry("notes.txt", MediaKind::Other),
            image("a"),
            entry("song.flac", MediaKind::Audio),
            entry("clip.mp4", MediaKind::Video),
        ];
        assert_eq!(names(&cover(&direct, [])), ["a", "clip.mp4"]);
    }

    #[test]
    fn a_folder_never_stands_for_another_folder() {
        // Otherwise building one mosaic would mean building the ones inside it.
        let direct = vec![entry("inner", MediaKind::Directory), image("a")];
        assert_eq!(names(&cover(&direct, [])), ["a"]);
    }

    #[test]
    fn descends_to_make_up_the_shortfall() {
        let direct = vec![image("a")];
        let nested = vec![vec![image("x"), image("y")], vec![image("z")]];
        assert_eq!(names(&cover(&direct, nested)), ["a", "x", "y", "z"]);
    }

    #[test]
    fn a_folder_of_folders_is_filled_entirely_from_below() {
        let direct = vec![entry("2026-01", MediaKind::Directory)];
        let nested = vec![vec![image("p"), image("q"), image("r"), image("s"), image("t")]];
        assert_eq!(names(&cover(&direct, nested)), ["p", "q", "r", "s"]);
    }

    #[test]
    fn returns_what_little_there_is_rather_than_nothing() {
        let direct = vec![image("only")];
        assert_eq!(names(&cover(&direct, [])), ["only"]);
        assert!(cover(&[], []).is_empty());
    }

    #[test]
    fn stops_reading_subfolders_the_moment_it_has_enough() {
        // The point of taking a lazy iterator: a caller that reads a directory
        // per item must not be made to read all of them.
        let reads = Cell::new(0);
        let listings = (0..100).map(|i| {
            reads.set(reads.get() + 1);
            vec![image(&format!("f{i}"))]
        });
        let picked = cover(&[], listings);
        assert_eq!(picked.len(), COVER_TILES);
        assert_eq!(reads.get(), COVER_TILES, "read one subfolder per picture needed");
    }

    #[test]
    fn a_folder_that_already_fills_the_tile_reads_no_subfolders() {
        let reads = Cell::new(0);
        let direct = vec![image("a"), image("b"), image("c"), image("d")];
        let listings = (0..100).map(|_| {
            reads.set(reads.get() + 1);
            vec![image("unreachable")]
        });
        assert_eq!(cover(&direct, listings).len(), COVER_TILES);
        assert_eq!(reads.get(), 0);
    }
}
