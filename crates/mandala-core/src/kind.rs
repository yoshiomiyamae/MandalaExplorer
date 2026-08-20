use std::path::Path;

/// What an entry is, decided from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MediaKind {
    Directory,
    Image,
    Video,
    Audio,
    Other,
}

/// Formats the loader can decode: everything the `image` crate reads, plus
/// what it hands to Windows when it cannot.
///
/// `heic` and `heif` are the ones that depend on the machine rather than on
/// us. They need the HEIF Image Extension and, because the pictures inside are
/// coded with HEVC, the HEVC Video Extension as well -- and that one is a paid
/// download. Listing them anyway is the lesser wrong: a file browser that
/// hides photographs because it might not be able to draw them is worse than
/// one that shows a tile without a preview.
const IMAGE_EXTENSIONS: &[&str] = &[
    "avif", "bmp", "dds", "exr", "gif", "hdr", "heic", "heif", "ico", "jpeg", "jpg", "png", "pnm",
    "qoi", "tga", "tif", "tiff", "webp",
];

const VIDEO_EXTENSIONS: &[&str] = &[
    "3gp", "asf", "avi", "divx", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "mts",
    "ogv", "rm", "rmvb", "ts", "vob", "webm", "wmv",
];

const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aiff", "alac", "ape", "flac", "m4a", "mid", "midi", "mp3", "ogg", "opus", "wav", "wma",
];

impl MediaKind {
    pub fn from_extension(ext: &str) -> Self {
        let lowered = ext.to_ascii_lowercase();
        let ext = lowered.as_str();
        if IMAGE_EXTENSIONS.contains(&ext) {
            Self::Image
        } else if VIDEO_EXTENSIONS.contains(&ext) {
            Self::Video
        } else if AUDIO_EXTENSIONS.contains(&ext) {
            Self::Audio
        } else {
            Self::Other
        }
    }

    pub fn from_path(path: &Path) -> Self {
        path.extension().and_then(|e| e.to_str()).map(Self::from_extension).unwrap_or(Self::Other)
    }

    /// Whether this can play inline inside its tile.
    pub fn is_playable(self) -> bool {
        matches!(self, Self::Video)
    }

    /// Whether a thumbnail can be produced for this.
    /// Whether the grid can draw a picture for this rather than an icon.
    ///
    /// Directories are included because theirs is built from what is inside
    /// them. That is not circular: a folder's tile is made of the pictures and
    /// videos below it, never of another folder's tile.
    pub fn has_thumbnail(self) -> bool {
        matches!(self, Self::Image | Self::Video | Self::Directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension_case_insensitively() {
        assert_eq!(MediaKind::from_extension("jpg"), MediaKind::Image);
        assert_eq!(MediaKind::from_extension("JPEG"), MediaKind::Image);
        assert_eq!(MediaKind::from_extension("PnG"), MediaKind::Image);
        assert_eq!(MediaKind::from_extension("webp"), MediaKind::Image);
        assert_eq!(MediaKind::from_extension("mp4"), MediaKind::Video);
        assert_eq!(MediaKind::from_extension("MKV"), MediaKind::Video);
        assert_eq!(MediaKind::from_extension("flac"), MediaKind::Audio);
        assert_eq!(MediaKind::from_extension("txt"), MediaKind::Other);
        assert_eq!(MediaKind::from_extension(""), MediaKind::Other);
    }

    #[test]
    fn classifies_paths_without_extension_as_other() {
        assert_eq!(MediaKind::from_path(Path::new("README")), MediaKind::Other);
        assert_eq!(MediaKind::from_path(Path::new("a/b/c.mp4")), MediaKind::Video);
    }

    #[test]
    fn only_video_is_playable_inline() {
        assert!(MediaKind::Video.is_playable());
        assert!(!MediaKind::Image.is_playable());
        assert!(!MediaKind::Audio.is_playable());
        assert!(!MediaKind::Directory.is_playable());
    }

    #[test]
    fn images_and_videos_have_thumbnails() {
        assert!(MediaKind::Image.has_thumbnail());
        assert!(MediaKind::Video.has_thumbnail());
        assert!(!MediaKind::Audio.has_thumbnail());
        assert!(!MediaKind::Other.has_thumbnail());
    }
}
