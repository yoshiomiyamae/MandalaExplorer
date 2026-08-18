//! Keeping the thumbnail cache from growing without bound.
//!
//! Thumbnails are cheap individually and ruinous in aggregate: a large library
//! browsed at a large tile size can put gigabytes on disk without anyone ever
//! noticing. The cache is swept back under a cap on startup, evicting whatever
//! has gone longest without being used.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Cap on the thumbnail cache.
pub const CACHE_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Fraction of the cap a sweep trims down to.
///
/// Sweeping to exactly the cap would leave the cache one thumbnail away from
/// being over it again, so each sweep buys some headroom.
const SWEEP_TARGET: f64 = 0.9;

/// How stale a timestamp has to be before a cache hit rewrites it.
///
/// The timestamp is what makes eviction least-recently-used, but touching a
/// file on every hit would mean a metadata write per thumbnail per folder
/// visit. Day-granularity is plenty to tell a live thumbnail from a dead one.
const TOUCH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub files: usize,
    pub removed: usize,
    pub freed: u64,
    /// Bytes still on disk afterwards.
    pub remaining: u64,
}

struct CacheFile {
    path: PathBuf,
    size: u64,
    used: SystemTime,
}

/// Evicts least-recently-used thumbnails until the cache fits in `limit`.
///
/// A missing cache directory is not an error: it just means nothing has been
/// cached yet.
pub fn sweep(dir: &Path, limit: u64) -> io::Result<SweepReport> {
    let mut files = collect(dir)?;
    let mut total: u64 = files.iter().map(|f| f.size).sum();
    let mut report =
        SweepReport { files: files.len(), removed: 0, freed: 0, remaining: total };
    if total <= limit {
        return Ok(report);
    }

    // Oldest first, so eviction takes what has gone longest unused.
    files.sort_by_key(|f| f.used);
    let target = (limit as f64 * SWEEP_TARGET) as u64;
    for file in files {
        if total <= target {
            break;
        }
        if fs::remove_file(&file.path).is_ok() {
            total = total.saturating_sub(file.size);
            report.removed += 1;
            report.freed += file.size;
        }
    }
    report.remaining = total;
    Ok(report)
}

/// Walks the sharded cache directory.
///
/// Leftover `.tmp` files from a write interrupted by a crash are collected too,
/// so they cannot accumulate forever unnoticed.
fn collect(dir: &Path) -> io::Result<Vec<CacheFile>> {
    let mut out = Vec::new();
    let shards = match fs::read_dir(dir) {
        Ok(shards) => shards,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };

    for shard in shards.flatten() {
        let Ok(entries) = fs::read_dir(shard.path()) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            out.push(CacheFile {
                path: entry.path(),
                size: meta.len(),
                // A file whose time cannot be read is treated as ancient, so a
                // broken entry is evicted first rather than pinned forever.
                used: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }
    Ok(out)
}

/// Marks a cached thumbnail as used, if its timestamp has gone stale.
pub fn mark_used(path: &Path) {
    let Ok(meta) = fs::metadata(path) else { return };
    let stale = meta
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > TOUCH_INTERVAL);
    if stale {
        let _ = filetime::set_file_mtime(path, filetime::FileTime::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::FileTime;

    /// Writes a cache file of `size` bytes, last used `age_days` ago.
    fn write(dir: &Path, shard: &str, name: &str, size: usize, age_days: u64) -> PathBuf {
        let path = dir.join(shard).join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![0u8; size]).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_days * 24 * 60 * 60);
        filetime::set_file_mtime(&path, FileTime::from_system_time(when)).unwrap();
        path
    }

    #[test]
    fn a_cache_under_the_limit_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "ab", "one.jpg", 100, 1);
        write(tmp.path(), "cd", "two.jpg", 100, 2);

        let report = sweep(tmp.path(), 1000).unwrap();
        assert_eq!(report.files, 2);
        assert_eq!(report.removed, 0);
        assert_eq!(report.remaining, 200);
    }

    #[test]
    fn eviction_takes_the_least_recently_used_first() {
        let tmp = tempfile::tempdir().unwrap();
        let ancient = write(tmp.path(), "ab", "ancient.jpg", 400, 90);
        let older = write(tmp.path(), "ab", "older.jpg", 400, 30);
        let recent = write(tmp.path(), "cd", "recent.jpg", 400, 1);

        // 1200 bytes against a 1000 limit: sweeping to 90% has to drop enough
        // to reach 900, which is one file.
        let report = sweep(tmp.path(), 1000).unwrap();
        assert_eq!(report.removed, 1);
        assert!(!ancient.exists(), "the oldest file should have gone first");
        assert!(older.exists());
        assert!(recent.exists());
    }

    #[test]
    fn eviction_keeps_going_until_the_cache_fits() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..10 {
            write(tmp.path(), "ab", &format!("f{i}.jpg"), 100, 90 - i);
        }
        let report = sweep(tmp.path(), 500).unwrap();
        // 1000 bytes down to at most 450.
        assert!(report.remaining <= 450, "remaining {}", report.remaining);
        assert_eq!(report.freed, 1000 - report.remaining);
        assert!(report.removed >= 6);
    }

    #[test]
    fn sweeping_collects_leftover_temporary_files() {
        // A crash mid-write leaves these behind; nothing else would clean them.
        let tmp = tempfile::tempdir().unwrap();
        let temporary = write(tmp.path(), "ab", "half-written.tmp", 900, 90);
        write(tmp.path(), "cd", "good.jpg", 100, 1);

        let report = sweep(tmp.path(), 500).unwrap();
        assert!(!temporary.exists(), "stale temp files should be swept too");
        assert_eq!(report.removed, 1);
    }

    #[test]
    fn an_absent_cache_directory_is_not_an_error() {
        let report = sweep(Path::new("no/such/cache"), 1000).unwrap();
        assert_eq!(report, SweepReport::default());
    }

    #[test]
    fn an_empty_cache_directory_reports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(sweep(tmp.path(), 1000).unwrap(), SweepReport::default());
    }

    #[test]
    fn marking_a_fresh_file_leaves_its_timestamp_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "ab", "fresh.jpg", 10, 0);
        let before = fs::metadata(&path).unwrap().modified().unwrap();

        mark_used(&path);
        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "a hit on a fresh file should not write to disk");
    }

    #[test]
    fn marking_a_stale_file_brings_its_timestamp_forward() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "ab", "stale.jpg", 10, 30);
        let before = fs::metadata(&path).unwrap().modified().unwrap();

        mark_used(&path);
        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(after > before, "a stale file should be marked as used again");
    }

    #[test]
    fn marking_a_missing_file_does_nothing() {
        mark_used(Path::new("no/such/thumb.jpg"));
    }
}
