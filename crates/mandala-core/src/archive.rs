//! ZIP folders use ordinary paths, such as `photos.zip/holiday/picture.jpg`.
//! Only selected images are extracted, never paths supplied by an archive.

use crate::{Entry, MediaKind};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
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

pub fn list(path: &Path) -> io::Result<Vec<Entry>> {
    let (archive, inside) = location(path).ok_or_else(|| io::Error::other("not a ZIP folder"))?;
    let modified = archive.metadata()?.modified().ok();
    let mut zip = ZipArchive::new(File::open(&archive)?)?;
    let mut entries = BTreeMap::<PathBuf, Entry>::new();
    let mut found = inside.as_os_str().is_empty();
    for i in 0..zip.len() {
        let file = zip.by_index_raw(i)?;
        let Ok(name) = file.name() else { continue };
        let Some(member) = safe_name(&name) else { continue };
        let Ok(relative) = member.strip_prefix(&inside) else { continue };
        let mut parts = relative.components();
        let Some(first) = parts.next() else {
            found |= file.is_dir();
            continue;
        };
        found = true;
        let child = PathBuf::from(first.as_os_str());
        let directory = parts.next().is_some() || file.is_dir();
        let entry = Entry {
            path: path.join(&child),
            name: child.to_string_lossy().into_owned(),
            kind: if directory {
                MediaKind::Directory
            } else if MediaKind::from_path(&child) == MediaKind::Image {
                MediaKind::Image
            } else {
                MediaKind::Other
            },
            len: if directory { 0 } else { file.size() },
            modified,
        };
        entries
            .entry(child)
            .and_modify(|e| {
                if directory {
                    *e = entry.clone();
                }
            })
            .or_insert(entry);
    }
    if !found {
        return Err(io::Error::new(io::ErrorKind::NotFound, "ZIP folder not found"));
    }
    Ok(entries.into_values().collect())
}

/// Sum uncompressed sizes from ZIP metadata without extracting any payload.
pub fn total_size(path: &Path, cancelled: impl Fn() -> bool) -> io::Result<u64> {
    let (archive, inside) = location(path).ok_or_else(|| io::Error::other("not a ZIP folder"))?;
    let mut zip = ZipArchive::new(File::open(archive)?)?;
    let mut found = inside.as_os_str().is_empty();
    let mut total = 0u64;
    for i in 0..zip.len() {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let entry = zip.by_index_raw(i)?;
        let Ok(name) = entry.name() else { continue };
        let Some(member) = safe_name(&name) else { continue };
        if member.starts_with(&inside) {
            found = true;
            if !entry.is_dir() {
                total = total
                    .checked_add(entry.size())
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
type Extracted = BTreeMap<String, (Weak<tempfile::NamedTempFile>, u64)>;

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
    let metadata = archive.metadata()?;
    let key = format!("{:?}:{:?}:{}:{:?}", archive, metadata.modified()?, metadata.len(), member);
    static FILES: OnceLock<Mutex<Extracted>> = OnceLock::new();
    let mut cache = FILES
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| io::Error::other("ZIP cache lock poisoned"))?;
    cache.retain(|_, (file, _)| file.strong_count() > 0);
    if let Some(file) = cache.get(&key).and_then(|(file, _)| file.upgrade()) {
        return Ok(LocalFile { path: file.path().to_path_buf(), _temporary: Some(file) });
    }
    let mut zip = ZipArchive::new(File::open(archive)?)?;
    let index = (0..zip.len())
        .find(|&i| {
            zip.by_index_raw(i)
                .ok()
                .and_then(|f| f.name().ok().and_then(|name| safe_name(&name)))
                .as_ref()
                == Some(&member)
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "ZIP member not found"))?;
    let mut file = zip.by_index(index)?;
    let used: u64 = cache.values().map(|(_, size)| size).sum();
    let available = MAX_ACTIVE_BYTES.saturating_sub(used).min(MAX_IMAGE_BYTES);
    if file.size() > available {
        return Err(io::Error::other(
            "ZIP image exceeds the temporary extraction limit (256 MiB per image, 512 MiB total)",
        ));
    }
    let suffix =
        member.extension().and_then(|s| s.to_str()).map(|s| format!(".{s}")).unwrap_or_default();
    let mut temporary =
        tempfile::Builder::new().prefix("mandala-zip-").suffix(&suffix).tempfile()?;
    let copied = io::copy(&mut (&mut file).take(available + 1), &mut temporary)?;
    if copied > available {
        return Err(io::Error::other("ZIP member exceeds its extraction budget"));
    }
    temporary.flush()?;
    let temporary = Arc::new(temporary);
    cache.insert(key, (Arc::downgrade(&temporary), copied));
    Ok(LocalFile { path: temporary.path().to_path_buf(), _temporary: Some(temporary) })
}

#[cfg(test)]
mod tests {
    use super::*;
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
