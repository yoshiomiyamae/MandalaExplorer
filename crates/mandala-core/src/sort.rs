//! Ordering the contents of a folder.
//!
//! Directories sort ahead of files whatever the key, and stay there when the
//! order is reversed: a folder is somewhere to go, not a thing being compared
//! against the files beside it. Reversing "largest first" should not bury the
//! way back out of the folder at the bottom of the grid.

use crate::kind::MediaKind;
use crate::scan::{Entry, natural_cmp};
use std::cmp::Ordering;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum SortKey {
    #[default]
    Name,
    /// Media kind first, then extension, so all the video lands together and
    /// the mp4s sit apart from the mkvs within it.
    Kind,
    Size,
    Modified,
    /// Running time. Anything without one -- stills, and video not yet probed
    /// -- sorts last in both directions.
    Duration,
}

impl SortKey {
    pub const ALL: [SortKey; 5] =
        [SortKey::Name, SortKey::Kind, SortKey::Size, SortKey::Modified, SortKey::Duration];

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Kind => "Type",
            SortKey::Size => "Size",
            SortKey::Modified => "Modified",
            SortKey::Duration => "Length",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum SortOrder {
    #[default]
    Ascending,
    Descending,
}

impl SortOrder {
    pub fn flipped(self) -> Self {
        match self {
            SortOrder::Ascending => SortOrder::Descending,
            SortOrder::Descending => SortOrder::Ascending,
        }
    }

    fn apply(self, ordering: Ordering) -> Ordering {
        match self {
            SortOrder::Ascending => ordering,
            SortOrder::Descending => ordering.reverse(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Sort {
    pub key: SortKey,
    pub order: SortOrder,
}

/// Sorts entries in place.
///
/// `duration_of` supplies running times, which are not part of an [`Entry`]
/// because reading one means opening the file. Returning `None` is always
/// acceptable and simply sorts that entry last under [`SortKey::Duration`].
pub fn sort_entries<F>(entries: &mut [Entry], sort: Sort, duration_of: F)
where
    F: Fn(&Entry) -> Option<Duration>,
{
    entries.sort_by(|a, b| {
        // Directories ahead of files, and never reversed.
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| compare(a, b, sort, &duration_of))
            // Ties fall back to name so the order is total, and so a redraw
            // cannot shuffle equal entries around.
            .then_with(|| natural_cmp(&a.name, &b.name))
    });
}

fn compare<F>(a: &Entry, b: &Entry, sort: Sort, duration_of: &F) -> Ordering
where
    F: Fn(&Entry) -> Option<Duration>,
{
    // Directories have no meaningful size, timestamp ordering worth reversing,
    // or running time, so they keep name order among themselves.
    if a.is_dir() && b.is_dir() {
        return sort.order.apply(natural_cmp(&a.name, &b.name));
    }

    match sort.key {
        SortKey::Name => sort.order.apply(natural_cmp(&a.name, &b.name)),
        SortKey::Kind => sort.order.apply(
            kind_rank(a.kind)
                .cmp(&kind_rank(b.kind))
                .then_with(|| extension(a).cmp(&extension(b))),
        ),
        SortKey::Size => sort.order.apply(a.len.cmp(&b.len)),
        SortKey::Modified => sort.order.apply(a.modified.cmp(&b.modified)),
        SortKey::Duration => match (duration_of(a), duration_of(b)) {
            (Some(x), Some(y)) => sort.order.apply(x.cmp(&y)),
            // Unknown lengths sink to the bottom either way round; flipping
            // the order should not fill the top of the grid with stills.
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        },
    }
}

/// Groups media kinds in the order someone browsing a media folder wants them.
fn kind_rank(kind: MediaKind) -> u8 {
    match kind {
        MediaKind::Directory => 0,
        MediaKind::Video => 1,
        MediaKind::Image => 2,
        MediaKind::Audio => 3,
        MediaKind::Other => 4,
    }
}

fn extension(entry: &Entry) -> String {
    entry
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn entry(name: &str, len: u64, minutes_old: u64) -> Entry {
        let kind = MediaKind::from_path(&PathBuf::from(name));
        Entry {
            path: PathBuf::from(name),
            name: name.to_owned(),
            kind,
            len,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(minutes_old * 60)),
        }
    }

    fn dir(name: &str) -> Entry {
        Entry {
            path: PathBuf::from(name),
            name: name.to_owned(),
            kind: MediaKind::Directory,
            len: 0,
            modified: Some(SystemTime::UNIX_EPOCH),
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn no_durations(_: &Entry) -> Option<Duration> {
        None
    }

    fn sorted(mut entries: Vec<Entry>, key: SortKey, order: SortOrder) -> Vec<Entry> {
        sort_entries(&mut entries, Sort { key, order }, no_durations);
        entries
    }

    #[test]
    fn sorts_by_name_naturally_in_both_directions() {
        let entries = vec![entry("b10.png", 1, 1), entry("b2.png", 1, 1), entry("a.png", 1, 1)];
        let up = sorted(entries.clone(), SortKey::Name, SortOrder::Ascending);
        assert_eq!(names(&up), vec!["a.png", "b2.png", "b10.png"]);

        let down = sorted(entries, SortKey::Name, SortOrder::Descending);
        assert_eq!(names(&down), vec!["b10.png", "b2.png", "a.png"]);
    }

    #[test]
    fn sorts_by_size() {
        let entries =
            vec![entry("m.png", 500, 1), entry("s.png", 100, 1), entry("l.png", 900, 1)];
        assert_eq!(
            names(&sorted(entries.clone(), SortKey::Size, SortOrder::Ascending)),
            vec!["s.png", "m.png", "l.png"]
        );
        assert_eq!(
            names(&sorted(entries, SortKey::Size, SortOrder::Descending)),
            vec!["l.png", "m.png", "s.png"]
        );
    }

    #[test]
    fn sorts_by_modification_time() {
        let entries = vec![entry("mid.png", 1, 50), entry("old.png", 1, 1), entry("new.png", 1, 99)];
        assert_eq!(
            names(&sorted(entries.clone(), SortKey::Modified, SortOrder::Ascending)),
            vec!["old.png", "mid.png", "new.png"]
        );
        assert_eq!(
            names(&sorted(entries, SortKey::Modified, SortOrder::Descending)),
            vec!["new.png", "mid.png", "old.png"]
        );
    }

    #[test]
    fn sorting_by_type_groups_video_then_stills_then_the_rest() {
        let entries = vec![
            entry("note.txt", 1, 1),
            entry("pic.png", 1, 1),
            entry("clip.mp4", 1, 1),
            entry("song.mp3", 1, 1),
        ];
        assert_eq!(
            names(&sorted(entries, SortKey::Kind, SortOrder::Ascending)),
            vec!["clip.mp4", "pic.png", "song.mp3", "note.txt"]
        );
    }

    #[test]
    fn sorting_by_type_separates_extensions_within_a_kind() {
        let entries = vec![
            entry("b.mkv", 1, 1),
            entry("a.mp4", 1, 1),
            entry("c.mkv", 1, 1),
            entry("d.mp4", 1, 1),
        ];
        assert_eq!(
            names(&sorted(entries, SortKey::Kind, SortOrder::Ascending)),
            vec!["b.mkv", "c.mkv", "a.mp4", "d.mp4"]
        );
    }

    #[test]
    fn directories_lead_whatever_the_key_and_order() {
        let entries = vec![entry("huge.png", 9_000, 99), dir("zzz"), entry("tiny.png", 1, 1)];
        for key in SortKey::ALL {
            for order in [SortOrder::Ascending, SortOrder::Descending] {
                let got = sorted(entries.clone(), key, order);
                assert_eq!(
                    got[0].name, "zzz",
                    "directory slipped out of first place for {key:?} {order:?}"
                );
            }
        }
    }

    #[test]
    fn directories_order_among_themselves_by_name() {
        let entries = vec![dir("b"), dir("a"), dir("c")];
        assert_eq!(
            names(&sorted(entries.clone(), SortKey::Size, SortOrder::Ascending)),
            vec!["a", "b", "c"]
        );
        assert_eq!(
            names(&sorted(entries, SortKey::Size, SortOrder::Descending)),
            vec!["c", "b", "a"]
        );
    }

    #[test]
    fn sorts_by_running_time() {
        let entries = vec![entry("long.mp4", 1, 1), entry("short.mp4", 1, 1)];
        let lengths = |e: &Entry| match e.name.as_str() {
            "long.mp4" => Some(Duration::from_secs(600)),
            "short.mp4" => Some(Duration::from_secs(10)),
            _ => None,
        };

        let mut up = entries.clone();
        sort_entries(&mut up, Sort { key: SortKey::Duration, order: SortOrder::Ascending }, lengths);
        assert_eq!(names(&up), vec!["short.mp4", "long.mp4"]);

        let mut down = entries;
        sort_entries(
            &mut down,
            Sort { key: SortKey::Duration, order: SortOrder::Descending },
            lengths,
        );
        assert_eq!(names(&down), vec!["long.mp4", "short.mp4"]);
    }

    #[test]
    fn entries_with_no_known_length_sink_to_the_bottom_both_ways() {
        // Stills have no length, and a video that has not been probed yet has
        // no length known. Reversing must not float either to the top.
        let entries =
            vec![entry("still.png", 1, 1), entry("clip.mp4", 1, 1), entry("unprobed.mp4", 1, 1)];
        let lengths = |e: &Entry| {
            (e.name == "clip.mp4").then_some(Duration::from_secs(30))
        };

        for order in [SortOrder::Ascending, SortOrder::Descending] {
            let mut got = entries.clone();
            sort_entries(&mut got, Sort { key: SortKey::Duration, order }, lengths);
            assert_eq!(got[0].name, "clip.mp4", "the only known length should lead ({order:?})");
        }
    }

    #[test]
    fn equal_entries_fall_back_to_name_order() {
        // Without a tiebreak, equal keys would leave the order to the sort and
        // could change between redraws.
        let entries = vec![entry("b.png", 100, 1), entry("a.png", 100, 1), entry("c.png", 100, 1)];
        assert_eq!(
            names(&sorted(entries, SortKey::Size, SortOrder::Ascending)),
            vec!["a.png", "b.png", "c.png"]
        );
    }

    #[test]
    fn sorting_an_empty_folder_is_fine() {
        let mut empty: Vec<Entry> = Vec::new();
        sort_entries(&mut empty, Sort::default(), no_durations);
        assert!(empty.is_empty());
    }

    #[test]
    fn flipping_an_order_gives_the_other_one() {
        assert_eq!(SortOrder::Ascending.flipped(), SortOrder::Descending);
        assert_eq!(SortOrder::Descending.flipped(), SortOrder::Ascending);
    }
}
