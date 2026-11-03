//! ZIP folders use ordinary paths, such as `photos.zip/holiday/picture.jpg`.
//! Only selected images are extracted, never paths supplied by an archive.

use crate::{Entry, MediaKind};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock, Weak},
    time::{Duration, SystemTime},
};
use zip::ZipArchive;

pub fn is_zip(path: &Path) -> bool {
    path.extension().is_some_and(|s| s.eq_ignore_ascii_case("zip"))
}

/// Finds the real archive ancestor without interpreting ZIP-like directory names.
pub fn location(path: &Path) -> Option<(PathBuf, PathBuf)> {
    path.ancestors()
        .find(|p| is_zip(p) && p.is_file())
        .map(|p| (p.to_path_buf(), path.strip_prefix(p).unwrap().to_path_buf()))
}

fn safe_name(name: &str) -> Option<PathBuf> {
    let name = name.replace('\\', "/");
    if name.starts_with('/')
        || name.split('/').any(|s| s == ".." || s.contains(':') || s.contains('\0'))
    {
        return None;
    }
    let path: PathBuf = name.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    (!path.as_os_str().is_empty()).then_some(path)
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct ArchiveId {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}
impl ArchiveId {
    fn from_metadata(path: &Path, metadata: &std::fs::Metadata) -> Self {
        Self { path: path.to_path_buf(), len: metadata.len(), modified: metadata.modified().ok() }
    }
    fn tag(&self, crc: u32) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        hash.update(format!("{:?}", self).as_bytes());
        hash.update(&crc.to_le_bytes());
        *hash.finalize().as_bytes()
    }
}

struct Member {
    index: usize,
    len: u64,
    crc: u32,
    directory: bool,
    modified: Option<SystemTime>,
}
struct Index {
    id: ArchiveId,
    metadata: Arc<zip::read::ZipArchiveMetadata>,
    members: BTreeMap<PathBuf, Member>,
}
type IndexResult = Result<Arc<Index>, (io::ErrorKind, String)>;
type IndexCell = Arc<OnceLock<IndexResult>>;
type IndexCache = VecDeque<(ArchiveId, IndexCell)>;

fn index_for(path: &Path) -> io::Result<Arc<Index>> {
    let id = ArchiveId::from_metadata(path, &path.metadata()?);
    static INDEXES: OnceLock<Mutex<IndexCache>> = OnceLock::new();
    let cache = INDEXES.get_or_init(Default::default);
    let cell = {
        let mut cache = cache.lock().map_err(|_| io::Error::other("ZIP index lock poisoned"))?;
        if let Some(position) = cache.iter().position(|(key, _)| key == &id) {
            let entry = cache.remove(position).unwrap();
            let cell = entry.1.clone();
            cache.push_back(entry);
            cell
        } else {
            cache.retain(|(key, _)| key.path != id.path);
            let cell = Arc::new(OnceLock::new());
            cache.push_back((id.clone(), cell.clone()));
            while cache.len() > 8 {
                cache.pop_front();
            }
            cell
        }
    };
    // Only requests for this archive wait on each other. No global lock spans I/O.
    match cell.get_or_init(|| {
        build_index(id.clone()).map(Arc::new).map_err(|e| (e.kind(), e.to_string()))
    }) {
        Ok(index) => Ok(index.clone()),
        Err((kind, message)) => {
            cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(_, entry)| !Arc::ptr_eq(entry, &cell));
            Err(io::Error::new(*kind, message.clone()))
        }
    }
}

fn build_index(id: ArchiveId) -> io::Result<Index> {
    let file = File::open(&id.path)?;
    if ArchiveId::from_metadata(&id.path, &file.metadata()?) != id {
        return Err(io::Error::other("ZIP changed while opening"));
    }
    let mut zip = ZipArchive::new(file)?;
    let mut members = BTreeMap::new();
    for i in 0..zip.len() {
        let file = zip.by_index_raw(i)?;
        let Ok(name) = file.name() else { continue };
        let Some(path) = safe_name(&name) else { continue };
        members.entry(path).or_insert(Member {
            index: i,
            len: file.size(),
            crc: file.crc32(),
            directory: file.is_dir(),
            modified: file.last_modified().and_then(zip_time),
        });
    }
    Ok(Index { id, metadata: zip.metadata(), members })
}

// DOS ZIP timestamps carry no timezone. Use one fixed offset for stable ordering
// of their wall-clock components rather than the archive's filesystem timestamp.
fn zip_time(date: zip::DateTime) -> Option<SystemTime> {
    if !date.is_valid() {
        return None;
    }
    let leap = |year: u16| {
        year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
    };
    let mut days = (1970..date.year()).map(|y| if leap(y) { 366u64 } else { 365 }).sum::<u64>();
    for month in 1..date.month() {
        days += match month {
            4 | 6 | 9 | 11 => 30,
            2 => {
                if leap(date.year()) {
                    29
                } else {
                    28
                }
            }
            _ => 31,
        };
    }
    days += u64::from(date.day() - 1);
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(
        days * 86400
            + u64::from(date.hour()) * 3600
            + u64::from(date.minute()) * 60
            + u64::from(date.second()),
    ))
}

fn reader(index: &Index) -> io::Result<ZipArchive<File>> {
    let file = File::open(&index.id.path)?;
    if ArchiveId::from_metadata(&index.id.path, &file.metadata()?) != index.id {
        return Err(io::Error::other("ZIP changed during request"));
    }
    // SAFETY: this metadata was parsed from this archive fingerprint. The newly
    // opened reader is independently positioned and its size/mtime are verified.
    Ok(unsafe { ZipArchive::unsafe_new_with_metadata(file, index.metadata.clone()) })
}

pub fn list(path: &Path) -> io::Result<Vec<Entry>> {
    let (archive, inside) = location(path).ok_or_else(|| io::Error::other("not a ZIP folder"))?;
    let index = index_for(&archive)?;
    let mut entries = BTreeMap::<PathBuf, Entry>::new();
    let mut found = inside.as_os_str().is_empty();
    for (member, file) in &index.members {
        let Ok(relative) = member.strip_prefix(&inside) else { continue };
        let mut parts = relative.components();
        let Some(first) = parts.next() else {
            found |= file.directory;
            continue;
        };
        found = true;
        let child = PathBuf::from(first.as_os_str());
        let directory = parts.next().is_some() || file.directory;
        let entry = Entry {
            cache_tag: Some(index.id.tag(file.crc)),
            path: path.join(&child),
            name: child.to_string_lossy().into_owned(),
            kind: if directory {
                MediaKind::Directory
            } else if MediaKind::from_path(&child) == MediaKind::Image {
                MediaKind::Image
            } else {
                MediaKind::Other
            },
            len: if directory { 0 } else { file.len },
            modified: file.modified,
        };
        entries
            .entry(child)
            .and_modify(|e| {
                if directory {
                    let modified = e.modified.max(entry.modified);
                    *e = entry.clone();
                    e.modified = modified;
                }
            })
            .or_insert(entry);
    }
    if !found {
        return Err(io::Error::new(io::ErrorKind::NotFound, "ZIP folder not found"));
    }
    // Explicit directory records keep their own date; implicit directories use
    // the latest descendant date collected above.
    for (child, entry) in &mut entries {
        if entry.is_dir()
            && let Some(directory) = index.members.get(&inside.join(child)).filter(|m| m.directory)
        {
            entry.modified = directory.modified;
        }
    }
    Ok(entries.into_values().collect())
}

/// Sum uncompressed sizes from ZIP metadata without extracting any payload.
pub fn total_size(path: &Path, cancelled: impl Fn() -> bool) -> io::Result<u64> {
    let (archive, inside) = location(path).ok_or_else(|| io::Error::other("not a ZIP folder"))?;
    let index = index_for(&archive)?;
    let mut found = inside.as_os_str().is_empty();
    let mut total = 0u64;
    for (member, entry) in &index.members {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if member.starts_with(&inside) {
            found = true;
            if !entry.directory {
                total = total
                    .checked_add(entry.len)
                    .ok_or_else(|| io::Error::other("size overflow"))?;
            }
        }
    }
    if !found {
        return Err(io::Error::new(io::ErrorKind::NotFound, "ZIP folder not found"));
    }
    Ok(total)
}

/// Keeps a temporary file alive while a decoder uses it. Weak caching shares
/// concurrent readers; releasing the last reader removes the extracted data.
pub struct LocalFile {
    path: PathBuf,
    _temporary: Option<Arc<tempfile::NamedTempFile>>,
}

impl LocalFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

const MAX_ACTIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 256 * 1024 * 1024;
enum Extracted {
    Loading(u64),
    Ready(Weak<tempfile::NamedTempFile>, u64),
}

#[derive(Default)]
struct ExtractionCache {
    entries: Mutex<BTreeMap<String, Extracted>>,
    changed: Condvar,
}

struct Reservation<'a> {
    cache: &'a ExtractionCache,
    key: String,
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut entries = self.cache.entries.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(entries.get(&self.key), Some(Extracted::Loading(_))) {
            entries.remove(&self.key);
        }
        self.cache.changed.notify_all();
    }
}

impl ExtractionCache {
    fn get(
        &self,
        key: String,
        size: u64,
        load: impl FnOnce() -> io::Result<tempfile::NamedTempFile>,
    ) -> io::Result<Arc<tempfile::NamedTempFile>> {
        let mut entries =
            self.entries.lock().map_err(|_| io::Error::other("ZIP cache lock poisoned"))?;
        loop {
            entries.retain(|_, entry| match entry {
                Extracted::Loading(_) => true,
                Extracted::Ready(file, _) => file.strong_count() > 0,
            });
            match entries.get(&key) {
                Some(Extracted::Ready(file, _)) => {
                    if let Some(file) = file.upgrade() {
                        return Ok(file);
                    }
                    entries.remove(&key);
                }
                Some(Extracted::Loading(_)) => {
                    entries = self
                        .changed
                        .wait(entries)
                        .map_err(|_| io::Error::other("ZIP cache lock poisoned"))?;
                    continue;
                }
                None => {}
            }
            let used: u64 = entries
                .values()
                .map(|entry| match entry {
                    Extracted::Loading(size) | Extracted::Ready(_, size) => *size,
                })
                .sum();
            if size > MAX_IMAGE_BYTES || size > MAX_ACTIVE_BYTES.saturating_sub(used) {
                return Err(io::Error::other(
                    "ZIP image exceeds the temporary extraction limit (256 MiB per image, 512 MiB total)",
                ));
            }
            entries.insert(key.clone(), Extracted::Loading(size));
            break;
        }
        drop(entries);
        let _reservation = Reservation { cache: self, key: key.clone() };
        // Opening, decompressing and writing happen with no cache mutex held.
        let temporary = Arc::new(load()?);
        self.entries
            .lock()
            .map_err(|_| io::Error::other("ZIP cache lock poisoned"))?
            .insert(key, Extracted::Ready(Arc::downgrade(&temporary), size));
        Ok(temporary)
    }
}

pub fn resolve(path: &Path) -> io::Result<LocalFile> {
    let Some((archive, member)) = location(path) else {
        return Ok(LocalFile { path: path.to_path_buf(), _temporary: None });
    };
    if member.as_os_str().is_empty() {
        return Err(io::Error::other("ZIP is a folder"));
    }
    if MediaKind::from_path(&member) != MediaKind::Image {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Only images can be extracted from ZIP folders",
        ));
    }
    let index = index_for(&archive)?;
    let info = index
        .members
        .get(&member)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ZIP member not found"))?;
    let key = format!("{:?}:{:?}:{:?}", index.id, member, info.crc);
    static FILES: OnceLock<ExtractionCache> = OnceLock::new();
    let temporary = FILES.get_or_init(Default::default).get(key, info.len, || {
        let mut zip = reader(&index)?;
        let mut file = zip.by_index(info.index)?;
        let suffix = member
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| format!(".{s}"))
            .unwrap_or_default();
        let mut temporary =
            tempfile::Builder::new().prefix("mandala-zip-").suffix(&suffix).tempfile()?;
        let copied = io::copy(&mut (&mut file).take(info.len + 1), &mut temporary)?;
        if copied != info.len {
            return Err(io::Error::other(
                "ZIP member exceeds or differs from its extraction budget",
            ));
        }
        temporary.flush()?;
        Ok(temporary)
    })?;
    Ok(LocalFile { path: temporary.path().to_path_buf(), _temporary: Some(temporary) })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_blocked_loader_does_not_block_other_members() {
        use std::sync::mpsc;
        let cache = Arc::new(ExtractionCache::default());
        let (started, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let first_cache = cache.clone();
        let first = std::thread::spawn(move || {
            first_cache.get("first".into(), 1, || {
                started.send(()).unwrap();
                wait.recv().unwrap();
                tempfile::NamedTempFile::new()
            })
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done, result) = mpsc::channel();
        let second = std::thread::spawn(move || {
            let file = cache.get("second".into(), 1, tempfile::NamedTempFile::new);
            done.send(file.is_ok()).unwrap();
        });
        let progressed = result.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        first.join().unwrap().unwrap();
        second.join().unwrap();
        assert!(progressed.unwrap());
    }

    #[test]
    fn failed_reservations_release_budget_and_live_files_share_the_limit() {
        let cache = ExtractionCache::default();
        assert!(
            cache
                .get("failed".into(), MAX_IMAGE_BYTES, || Err(io::Error::other("failed")))
                .is_err()
        );
        assert!(cache.entries.lock().unwrap().is_empty());
        let first =
            cache.get("first".into(), MAX_IMAGE_BYTES, tempfile::NamedTempFile::new).unwrap();
        let second =
            cache.get("second".into(), MAX_IMAGE_BYTES, tempfile::NamedTempFile::new).unwrap();
        assert!(
            cache.get("third".into(), 1, || panic!("budget must be reserved before I/O")).is_err()
        );
        drop(first);
        assert!(cache.get("third".into(), 1, tempfile::NamedTempFile::new).is_ok());
        drop(second);
    }

    #[test]
    fn concurrent_same_member_requests_share_one_extraction() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };
        let cache = Arc::new(ExtractionCache::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let (started, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let first_cache = cache.clone();
        let first_calls = calls.clone();
        let first = std::thread::spawn(move || {
            first_cache
                .get("same".into(), 1, || {
                    first_calls.fetch_add(1, Ordering::Relaxed);
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                    tempfile::NamedTempFile::new()
                })
                .unwrap()
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let second_calls = calls.clone();
        let second = std::thread::spawn(move || {
            cache
                .get("same".into(), 1, || {
                    second_calls.fetch_add(1, Ordering::Relaxed);
                    tempfile::NamedTempFile::new()
                })
                .unwrap()
        });
        release.send(()).unwrap();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn replacing_an_archive_preserving_member_sizes_and_dates_changes_all_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("replace.zip");
        let write = |payload: &[u8], comment: &str| {
            let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
            zip.set_comment(comment).unwrap();
            zip.start_file("image.png", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(payload).unwrap();
            zip.finish().unwrap();
        };
        write(b"first", "");
        let timestamp = path.metadata().unwrap().modified().unwrap();
        let first_index = index_for(&path).unwrap();
        let first = list(&path).unwrap();
        let first_key = crate::CacheKey::for_entry(&first[0], 128);
        let first_group = crate::CacheKey::for_group(&path, &first, 128);
        write(b"other", "different archive size");
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(timestamp))
            .unwrap();
        let second_index = index_for(&path).unwrap();
        let second = list(&path).unwrap();
        assert!(!Arc::ptr_eq(&first_index, &second_index));
        assert_eq!(first[0].len, second[0].len);
        assert_eq!(first[0].modified, second[0].modified);
        assert_ne!(first_key, crate::CacheKey::for_entry(&second[0], 128));
        assert_ne!(first_group, crate::CacheKey::for_group(&path, &second, 128));
        assert_eq!(std::fs::read(resolve(&second[0].path).unwrap().path()).unwrap(), b"other");
    }
    #[test]
    fn member_timestamps_are_not_the_archives_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dates.zip");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        for (name, year) in [("a.png", 2025), ("z.png", 2020)] {
            let date = zip::DateTime::from_date_and_time(year, 2, 3, 4, 5, 6).unwrap();
            zip.start_file(name, zip::write::SimpleFileOptions::default().last_modified_time(date))
                .unwrap();
            zip.write_all(b"image").unwrap();
        }
        zip.finish().unwrap();
        let entries = list(&path).unwrap();
        assert!(entries[0].modified > entries[1].modified);
    }

    #[test]
    fn cached_index_is_reused_for_multiple_member_requests() {
        let (_tmp, path) = fixture();
        let first = index_for(&path).unwrap();
        let second = index_for(&path).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("日本語.ZIP");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        for name in ["写真/猫.png", "movie.mp4", "../escape.png", "C:/bad.png"] {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(b"media").unwrap();
        }
        zip.finish().unwrap();
        (tmp, path)
    }
    #[test]
    fn lists_implicit_folders_and_unicode_without_unsafe_members() {
        let (_tmp, path) = fixture();
        let entries = list(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "写真" && e.is_dir()));
        let photos = list(&path.join("写真")).unwrap();
        assert_eq!(photos[0].name, "猫.png");
        assert_eq!(photos[0].len, 5);
        assert!(list(&path.join("missing")).is_err());
    }
    #[test]
    fn extracts_only_requested_member_and_keeps_it_until_last_reader() {
        let (_tmp, path) = fixture();
        let first = resolve(&path.join("写真/猫.png")).unwrap();
        let second = resolve(&path.join("写真/猫.png")).unwrap();
        assert_eq!(first.path(), second.path());
        assert_eq!(std::fs::read(first.path()).unwrap(), b"media");
        let extracted = first.path().to_path_buf();
        drop(first);
        assert!(extracted.exists());
        drop(second);
        assert!(!extracted.exists());
    }
    #[test]
    fn rejects_traversal_and_windows_absolute_names() {
        for name in ["../x", "/x", "C:\\x", "foo/../../x", "foo:bar", "\\\\server\\x"] {
            assert!(safe_name(name).is_none(), "{name}");
        }
    }
    #[test]
    fn videos_are_not_playable_or_extractable() {
        let (_tmp, path) = fixture();
        let entries = list(&path).unwrap();
        let movie = entries.iter().find(|e| e.name == "movie.mp4").unwrap();
        assert_eq!(movie.kind, MediaKind::Other);
        assert!(!movie.kind.is_playable());
        assert!(resolve(&movie.path).is_err());
        assert!(list(&movie.path).is_err());
    }
    #[test]
    fn scan_enters_archives_but_not_real_directories_named_zip() {
        let (tmp, path) = fixture();
        let directory = tmp.path().join("ordinary.zip");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("normal.png"), b"image").unwrap();
        assert!(crate::scan_dir(tmp.path()).unwrap().iter().all(Entry::is_dir));
        assert_eq!(crate::scan_dir(&directory).unwrap()[0].name, "normal.png");
        assert_eq!(crate::scan_dir(&path).unwrap().len(), 2);
    }

    #[test]
    fn rejects_images_claiming_more_than_the_extraction_limit() {
        let (_tmp, path) = fixture();
        let mut bytes = std::fs::read(&path).unwrap();
        let central = bytes.windows(4).position(|b| b == b"PK\x01\x02").unwrap();
        bytes[central + 24..central + 28]
            .copy_from_slice(&((MAX_IMAGE_BYTES + 1) as u32).to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        let error = resolve(&path.join("写真/猫.png")).err().expect("oversized image accepted");
        assert!(error.to_string().contains("extraction limit"), "{error}");
    }

    #[test]
    fn empty_and_invalid_archives_have_distinct_results() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.zip");
        zip::ZipWriter::new(File::create(&path).unwrap()).finish().unwrap();
        assert!(list(&path).unwrap().is_empty());
        std::fs::write(&path, b"not a ZIP").unwrap();
        assert!(list(&path).is_err());
    }
}
