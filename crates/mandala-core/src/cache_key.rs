
//! Stable identity for a cached thumbnail.
//!
//! Thumbnails live on disk between runs, so the key has to change whenever the
//! source file changes or the requested size changes -- otherwise a re-encoded
//! video silently keeps its stale thumbnail.

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
    fn relative_path_shards_on_the_first_byte() {
        let k = key("a/b.mp4", 1, 2, 256);
        let hex = k.as_str().to_owned();
        assert_eq!(k.relative_path("webp"), format!("{}/{hex}.webp", &hex[..2]));
    }
}
