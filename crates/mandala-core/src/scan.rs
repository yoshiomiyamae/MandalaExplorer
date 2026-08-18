//! Listing the contents of one directory.

use crate::kind::MediaKind;
use std::cmp::Ordering;
use std::fs;
use std::io;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::str::Chars;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub kind: MediaKind,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == MediaKind::Directory
    }

    /// Modification time in nanoseconds since the Unix epoch, for cache keys.
    /// A file with no readable timestamp falls back to 0 and simply gets its
    /// thumbnail regenerated more often than it needs to.
    pub fn mtime_unix_nanos(&self) -> i128 {
        self.modified
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0)
    }
}

/// Lists `dir` non-recursively, directories first, then entries in natural
/// order. Unreadable children are skipped rather than failing the whole scan,
/// since one permission-denied file should not blank the entire view.
pub fn scan_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        let path = entry.path();
        let kind = if meta.is_dir() { MediaKind::Directory } else { MediaKind::from_path(&path) };
        entries.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            len: if meta.is_dir() { 0 } else { meta.len() },
            modified: meta.modified().ok(),
            path,
        });
    }

    entries
        .sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| natural_cmp(&a.name, &b.name)));
    Ok(entries)
}

/// Orders names the way a person reads them, so `img9` comes before `img10`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut left = a.chars().peekable();
    let mut right = b.chars().peekable();
    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                match compare_digit_runs(&take_digits(&mut left), &take_digits(&mut right)) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(x), Some(y)) => match x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()) {
                Ordering::Equal => {
                    left.next();
                    right.next();
                }
                other => return other,
            },
        }
    }
}

fn take_digits(chars: &mut Peekable<Chars>) -> String {
    let mut run = String::new();
    while let Some(c) = chars.peek().copied() {
        if !c.is_ascii_digit() {
            break;
        }
        run.push(c);
        chars.next();
    }
    run
}

/// Compares two digit runs as numbers without parsing them, so a 40-digit
/// frame number in a filename cannot overflow anything.
fn compare_digit_runs(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn natural_order_compares_digit_runs_as_numbers() {
        assert_eq!(natural_cmp("img2", "img10"), Ordering::Less);
        assert_eq!(natural_cmp("img10", "img2"), Ordering::Greater);
        assert_eq!(natural_cmp("img2", "img2"), Ordering::Equal);
    }

    #[test]
    fn natural_order_ignores_leading_zeros_in_numbers() {
        assert_eq!(natural_cmp("img007", "img8"), Ordering::Less);
        assert_eq!(natural_cmp("img007", "img7"), Ordering::Equal);
    }

    #[test]
    fn natural_order_is_case_insensitive() {
        assert_eq!(natural_cmp("Apple", "apple"), Ordering::Equal);
        assert_eq!(natural_cmp("apple", "Banana"), Ordering::Less);
    }

    #[test]
    fn natural_order_handles_numbers_at_the_start_and_end() {
        assert_eq!(natural_cmp("9x", "10x"), Ordering::Less);
        assert_eq!(natural_cmp("x", "x1"), Ordering::Less);
    }

    #[test]
    fn scan_lists_directories_before_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.mp4"), b"v").unwrap();
        fs::create_dir(tmp.path().join("zdir")).unwrap();

        let got = scan_dir(tmp.path()).unwrap();
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["zdir", "a.mp4"]);
        assert!(got[0].is_dir());
        assert_eq!(got[1].kind, MediaKind::Video);
    }

    #[test]
    fn scan_sorts_files_naturally() {
        let tmp = tempfile::tempdir().unwrap();
        for n in ["img10.png", "img2.png", "img1.png"] {
            fs::write(tmp.path().join(n), b"i").unwrap();
        }
        let got = scan_dir(tmp.path()).unwrap();
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["img1.png", "img2.png", "img10.png"]);
    }

    #[test]
    fn scan_records_file_size() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.png"), b"12345").unwrap();
        let got = scan_dir(tmp.path()).unwrap();
        assert_eq!(got[0].len, 5);
    }

    #[test]
    fn scan_does_not_recurse() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/deep.png"), b"i").unwrap();
        let got = scan_dir(tmp.path()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "sub");
    }

    #[test]
    fn scan_reports_a_missing_directory_as_an_error() {
        assert!(scan_dir(Path::new("does/not/exist/at/all")).is_err());
    }
}
