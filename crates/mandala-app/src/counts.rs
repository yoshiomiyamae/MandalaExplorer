//! Bounded, cancellable off-thread folder statistics.
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

type CountBatch = (u64, Vec<(PathBuf, Option<u64>)>);

#[derive(Clone, Copy)]
pub enum Metric {
    FileCount,
    Size,
}
type Request = (u64, Metric, Vec<PathBuf>);

pub struct Counts {
    jobs: Sender<Request>,
    pending: Receiver<Request>,
    pub results: Receiver<CountBatch>,
    epoch: Arc<AtomicU64>,
}

impl Counts {
    pub fn new(wake: impl Fn() + Send + 'static) -> Self {
        let (jobs, pending) = bounded::<Request>(1);
        let (send, results) = unbounded();
        let epoch = Arc::new(AtomicU64::new(0));
        let current = epoch.clone();
        let work = pending.clone();
        std::thread::spawn(move || {
            while let Ok((id, metric, paths)) = work.recv() {
                let mut batch = Vec::new();
                for path in paths {
                    if current.load(Ordering::Relaxed) != id {
                        break;
                    }
                    let count = match metric {
                        Metric::FileCount => count_files(&path),
                        Metric::Size => {
                            folder_bytes(&path, || current.load(Ordering::Relaxed) != id)
                        }
                    };
                    batch.push((path, count));
                }
                if current.load(Ordering::Relaxed) == id {
                    if send.send((id, batch)).is_err() {
                        break;
                    }
                    wake();
                }
            }
        });
        Self { jobs, pending, results, epoch }
    }

    pub fn request(&self, metric: Metric, paths: Vec<PathBuf>) -> u64 {
        let id = self.cancel();
        let _ = self.jobs.try_send((id, metric, paths));
        id
    }

    pub fn cancel(&self) -> u64 {
        let id = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        while self.pending.try_recv().is_ok() {}
        while self.results.try_recv().is_ok() {}
        id
    }
}

impl Drop for Counts {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn count_files(path: &Path) -> Option<u64> {
    if mandala_core::archive::location(path).is_some() {
        return mandala_core::archive::list(path)
            .ok()
            .map(|entries| entries.iter().filter(|e| !e.is_dir()).count() as u64);
    }
    let mut count = 0;
    for entry in std::fs::read_dir(path).ok()? {
        let entry = entry.ok()?;
        if entry.metadata().ok()?.is_file() {
            count += 1;
        }
    }
    Some(count)
}

fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn folder_bytes(path: &Path, cancelled: impl Fn() -> bool) -> Option<u64> {
    if cancelled() {
        return None;
    }
    // Inspect the real archive too, so a ZIP symlink is not followed.
    let archive = mandala_core::archive::location(path);
    let physical = archive.as_ref().map_or(path, |(archive, _)| archive.as_path());
    if is_link(&std::fs::symlink_metadata(physical).ok()?) {
        return None;
    }
    if archive.is_some() {
        return mandala_core::archive::total_size(path, cancelled).ok();
    }
    let mut pending = vec![path.to_path_buf()];
    let mut total = 0u64;
    while let Some(directory) = pending.pop() {
        if cancelled() {
            return None;
        }
        // Recheck each directory before opening it, including the root.
        if is_link(&std::fs::symlink_metadata(&directory).ok()?) {
            continue;
        }
        for child in std::fs::read_dir(directory).ok()? {
            if cancelled() {
                return None;
            }
            let child = child.ok()?;
            let metadata = std::fs::symlink_metadata(child.path()).ok()?;
            if is_link(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                pending.push(child.path());
            } else if metadata.is_file() {
                total = total.checked_add(metadata.len())?;
            }
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zip_size_uses_uncompressed_sizes_recursively_without_extraction() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sizes.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for (name, len) in
            [("one.jpg", 1024), ("child/two.mov", 4096), ("children/three", 8), ("../unsafe", 512)]
        {
            zip.start_file(
                name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
            zip.write_all(&vec![0; len]).unwrap();
        }
        zip.finish().unwrap();
        assert_eq!(folder_bytes(&path, || false), Some(5128));
        assert_eq!(folder_bytes(&path.join("child"), || false), Some(4096));
        assert_eq!(folder_bytes(&path.join("missing"), || false), None);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn folder_size_does_not_follow_junctions() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("local"), b"123").unwrap();
        std::fs::write(external.path().join("outside"), b"12345678").unwrap();
        let link = root.path().join("junction");
        let result = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(external.path())
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        assert_eq!(folder_bytes(root.path(), || false), Some(3));
        assert_eq!(folder_bytes(&link, || false), None);
    }
    #[test]
    fn folder_size_includes_descendants_and_counts_zip_files_as_stored_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("child")).unwrap();
        std::fs::write(dir.path().join("a"), b"123").unwrap();
        std::fs::write(dir.path().join("child/b.zip"), b"12345").unwrap();
        assert_eq!(folder_bytes(dir.path(), || false), Some(8));
        assert_eq!(folder_bytes(dir.path(), || true), None);
        assert_eq!(folder_bytes(&dir.path().join("missing"), || false), None);
    }
    #[test]
    fn zip_counts_include_non_images_but_not_nested_files() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("items.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for name in ["picture.jpg", "movie.mov", "child/inside.png"] {
            writer.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(b"test").unwrap();
        }
        writer.finish().unwrap();
        assert_eq!(count_files(&path), Some(2));
        assert_eq!(count_files(&path.join("child")), Some(1));
    }

    #[test]
    fn worker_returns_a_tagged_batch_and_can_be_cancelled() {
        let worker = Counts::new(|| {});
        let dir = tempfile::tempdir().unwrap();
        let id = worker.request(Metric::FileCount, vec![dir.path().to_path_buf()]);
        let (result_id, batch) =
            worker.results.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(result_id, id);
        assert_eq!(batch, vec![(dir.path().to_path_buf(), Some(0))]);
        assert_ne!(worker.cancel(), id);
    }
    #[test]
    fn counts_files_not_descendants_or_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("one.txt"), b"1").unwrap();
        std::fs::create_dir(dir.path().join("child")).unwrap();
        std::fs::write(dir.path().join("child/two.txt"), b"2").unwrap();
        assert_eq!(count_files(dir.path()), Some(1));
        assert_eq!(count_files(&dir.path().join("missing")), None);
    }
}
