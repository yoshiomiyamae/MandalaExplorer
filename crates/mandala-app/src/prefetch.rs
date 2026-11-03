//! Bounded, incremental planning for disk-only thumbnail prefetch.

use mandala_core::Entry;
use std::{collections::HashSet, ops::Range, path::PathBuf};

#[derive(Default)]
pub struct Prefetch {
    pub epoch: u64,
    attempted: HashSet<PathBuf>,
    visible: Range<usize>,
    next: usize,
    remaining: usize,
}

impl Prefetch {
    pub fn reset(&mut self, count: usize) {
        self.epoch = self.epoch.wrapping_add(1);
        self.attempted.clear();
        self.visible = 0..0;
        self.next = 0;
        self.remaining = count;
    }

    pub fn completed(&mut self, path: PathBuf) {
        self.attempted.insert(path);
    }

    pub fn retry(&mut self, path: PathBuf, index: usize) {
        self.attempted.remove(&path);
        self.next = index;
        self.remaining += 1;
    }

    /// Starts just beyond the viewport, then wraps around. Bounded scanning
    /// keeps a directory of non-media files from monopolising a UI frame.
    pub fn next(
        &mut self,
        entries: &[Entry],
        visible: Range<usize>,
        requested: &HashSet<(PathBuf, u32)>,
        tier: u32,
    ) -> Option<usize> {
        if entries.is_empty() {
            return None;
        }
        if visible != self.visible {
            self.next = visible.end % entries.len();
            self.remaining = entries.len();
            self.visible = visible;
        }
        for _ in 0..self.remaining.min(64) {
            let index = self.next;
            self.next = (index + 1) % entries.len();
            self.remaining -= 1;
            let entry = &entries[index];
            if self.visible.contains(&index)
                || !entry.kind.has_thumbnail()
                || requested.contains(&(entry.path.clone(), tier))
                || self.attempted.contains(&entry.path)
            {
                continue;
            }
            self.attempted.insert(entry.path.clone());
            return Some(index);
        }
        None
    }

    pub fn has_remaining(&self) -> bool {
        self.remaining > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mandala_core::MediaKind;

    fn entries() -> Vec<Entry> {
        (0..6)
            .map(|i| Entry {
                cache_tag: None,
                path: PathBuf::from(format!("{i}.png")),
                name: format!("{i}.png"),
                kind: MediaKind::Image,
                len: 0,
                modified: None,
            })
            .collect()
    }

    #[test]
    fn prefetch_starts_beyond_the_view_and_wraps_without_repeating() {
        let entries = entries();
        let mut plan = Prefetch::default();
        plan.reset(entries.len());
        let requested = HashSet::new();
        let mut picked = Vec::new();
        while let Some(index) = plan.next(&entries, 1..3, &requested, 128) {
            picked.push(index);
        }
        assert_eq!(picked, [3, 4, 5, 0]);
        assert!(!plan.has_remaining());
    }

    #[test]
    fn visible_requests_and_failed_attempts_are_not_prefetched_again() {
        let entries = entries();
        let mut plan = Prefetch::default();
        plan.reset(entries.len());
        let requested = HashSet::from([(entries[2].path.clone(), 128)]);
        plan.completed(entries[3].path.clone());
        assert_eq!(plan.next(&entries, 0..2, &requested, 128), Some(4));
        assert_eq!(plan.next(&entries, 2..4, &requested, 128), Some(5));
        plan.reset(entries.len());
        assert_eq!(plan.next(&entries, 0..2, &requested, 256), Some(2));
    }

    #[test]
    fn large_non_media_directories_are_scanned_in_bounded_passes() {
        let mut entries = vec![entries()[0].clone(); 200];
        for entry in &mut entries {
            entry.kind = MediaKind::Other;
        }
        entries[199].kind = MediaKind::Image;
        let mut plan = Prefetch::default();
        plan.reset(entries.len());
        assert_eq!(plan.next(&entries, 0..1, &HashSet::new(), 128), None);
        assert!(plan.has_remaining());
        let mut found = None;
        for _ in 0..4 {
            if let Some(index) = plan.next(&entries, 0..1, &HashSet::new(), 128) {
                found = Some(index);
                break;
            }
        }
        assert_eq!(found, Some(199));
    }
}
