//! Stable identity for a cached thumbnail.
//!
//! Thumbnails live on disk between runs, so the key has to change whenever the
//! source file changes or the requested size changes -- otherwise a re-encoded
//! video silently keeps its stale thumbnail.

use crate::scan::Entry;
use std::path::Path;

/// Bytes of hash kept. 16 bytes is far past the point where collisions matter
/// for a per-user thumbnail cache, and it keeps file names short.
const KEY_BYTES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CacheKey(String);

impl CacheKey {
    pub fn new(path: &Path, mtime_unix_nanos: i128, len: u64, target_px: u32) -> Self {
        let mut hasher = blake3::Hasher::new();
        // A separator after the path keeps a path ending in digits from
        // colliding with a shorter path plus a different mtime.
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hasher.update(&mtime_unix_nanos.to_le_bytes());
        hasher.update(&len.to_le_bytes());
        hasher.update(&target_px.to_le_bytes());
        Self(hasher.finalize().to_hex()[..KEY_BYTES * 2].to_owned())
    }

    /// Key for metadata that has nothing to do with thumbnail size, such as a
    /// running time. Kept separate so changing the tile size does not throw
    /// away facts about the file that never depended on it.
    pub fn metadata(path: &Path, mtime_unix_nanos: i128, len: u64) -> Self {
        // Zero is not a real target size, so this cannot collide with a
        // thumbnail key for the same file.
        Self::new(path, mtime_unix_nanos, len, 0)
    }

    /// Thumbnail key for a scanned entry.
    ///
    /// Which fields of an [`Entry`] identify a file, and in what order, is a
    /// fact about the key rather than about whoever is asking for one. Getting
    /// it wrong produces no error, just permanent cache misses or a stale
    /// thumbnail for a re-encoded file.
    pub fn for_entry(entry: &Entry, target_px: u32) -> Self {
        Self::new(&entry.path, entry.mtime_unix_nanos(), entry.len, target_px)
    }

    /// Size-independent key for a scanned entry.
    pub fn metadata_for(entry: &Entry) -> Self {
        Self::metadata(&entry.path, entry.mtime_unix_nanos(), entry.len)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Relative path of the cache file, sharded by the first byte so no single
    /// directory ends up holding a hundred thousand entries.
    pub fn relative_path(&self, extension: &str) -> String {
        format!("{}/{}.{extension}", &self.0[..2], self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str, mtime: i128, len: u64, px: u32) -> CacheKey {
        CacheKey::new(Path::new(path), mtime, len, px)
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(key("a/b.mp4", 1, 2, 256), key("a/b.mp4", 1, 2, 256));
    }

    #[test]
    fn is_fixed_width_hex() {
        let k = key("a/b.mp4", 1, 2, 256);
        assert_eq!(k.as_str().len(), 32);
        assert!(k.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn every_input_participates_in_the_key() {
        let base = key("a/b.mp4", 1, 2, 256);
        assert_ne!(base, key("a/c.mp4", 1, 2, 256), "path must matter");
        assert_ne!(base, key("a/b.mp4", 9, 2, 256), "mtime must matter");
        assert_ne!(base, key("a/b.mp4", 1, 9, 256), "size must matter");
        assert_ne!(base, key("a/b.mp4", 1, 2, 512), "target size must matter");
    }

    #[test]
    fn a_metadata_key_is_independent_of_thumbnail_size() {
        let path = Path::new("a/b.mp4");
        let first = CacheKey::metadata(path, 1, 2);
        assert_eq!(first, CacheKey::metadata(path, 1, 2));
        // It must not collide with any thumbnail key for the same file.
        for px in [128, 256, 512, 1024, 2048] {
            assert_ne!(first, key("a/b.mp4", 1, 2, px));
        }
    }

    #[test]
    fn a_metadata_key_still_tracks_the_file_itself() {
        let base = CacheKey::metadata(Path::new("a/b.mp4"), 1, 2);
        assert_ne!(base, CacheKey::metadata(Path::new("a/c.mp4"), 1, 2));
        assert_ne!(base, CacheKey::metadata(Path::new("a/b.mp4"), 9, 2));
        assert_ne!(base, CacheKey::metadata(Path::new("a/b.mp4"), 1, 9));
    }

    #[test]
    fn entry_keys_agree_with_the_fields_they_are_built_from() {
        use crate::kind::MediaKind;
        use std::time::{Duration, SystemTime};

        // A whole second, because Windows timestamps land on 100ns boundaries
        // and a single nanosecond would be rounded away before it was read back.
        let entry = Entry {
            path: std::path::PathBuf::from("a/b.mp4"),
            name: "b.mp4".into(),
            kind: MediaKind::Video,
            len: 2,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
        };
        let nanos = 1_000_000_000i128;
        assert_eq!(entry.mtime_unix_nanos(), nanos, "precondition for the keys below");

        assert_eq!(CacheKey::for_entry(&entry, 256), key("a/b.mp4", nanos, 2, 256));
        assert_eq!(
            CacheKey::metadata_for(&entry),
            CacheKey::metadata(Path::new("a/b.mp4"), nanos, 2)
        );
    }

    #[test]
    fn relative_path_shards_on_the_first_byte() {
        let k = key("a/b.mp4", 1, 2, 256);
        let hex = k.as_str().to_owned();
        assert_eq!(k.relative_path("webp"), format!("{}/{hex}.webp", &hex[..2]));
    }
}
