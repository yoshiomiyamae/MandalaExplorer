//! Still-image thumbnails.

use crate::frame::Frame;
use crate::sizing::fit_within;
use anyhow::{Context, Result};
use image::imageops::FilterType;
use std::path::Path;
use std::time::Duration;

/// Decodes an image file at its own size.
///
/// The one place image decoding happens, so anything it has to learn later --
/// EXIF orientation, an ICC pass, the first frame of an animation -- is learned
/// once for every caller.
pub fn load(path: &Path) -> Result<Frame> {
    let image = decode(path)?;
    let (width, height) = (image.width(), image.height());
    Ok(Frame { width, height, rgba: image.into_rgba8().into_raw(), timestamp: Duration::ZERO })
}

/// Decodes an image file and scales it down to fit `max`.
pub fn load_thumbnail(path: &Path, max: (u32, u32)) -> Result<Frame> {
    let image = decode(path)?;

    let (w, h) = fit_within((image.width(), image.height()), max);
    // Triangle rather than Lanczos: at thumbnail sizes the difference is barely
    // visible, and this runs on every file in a folder.
    let scaled = image.resize_exact(w, h, FilterType::Triangle).into_rgba8();
    Ok(Frame { width: w, height: h, rgba: scaled.into_raw(), timestamp: Duration::ZERO })
}

fn decode(path: &Path) -> Result<image::DynamicImage> {
    image::ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("sniffing format of {}", path.display()))?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(dir: &Path, name: &str, w: u32, h: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255])).save(&path).unwrap();
        path
    }

    #[test]
    fn scales_a_large_image_down_into_the_box() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_png(tmp.path(), "big.png", 800, 400);
        let frame = load_thumbnail(&path, (256, 256)).unwrap();
        assert_eq!((frame.width, frame.height), (256, 128));
        assert_eq!(frame.rgba.len(), 256 * 128 * 4);
    }

    #[test]
    fn leaves_a_small_image_at_its_own_size() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_png(tmp.path(), "small.png", 32, 16);
        let frame = load_thumbnail(&path, (256, 256)).unwrap();
        assert_eq!((frame.width, frame.height), (32, 16));
    }

    #[test]
    fn preserves_pixel_values_through_the_rgba_conversion() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_png(tmp.path(), "flat.png", 8, 8);
        let frame = load_thumbnail(&path, (256, 256)).unwrap();
        assert_eq!(&frame.rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn loading_full_size_keeps_the_original_dimensions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_png(tmp.path(), "big.png", 800, 400);
        let frame = load(&path).unwrap();
        assert_eq!((frame.width, frame.height), (800, 400));
        assert_eq!(frame.rgba.len(), 800 * 400 * 4);
    }

    #[test]
    fn loading_full_size_reports_an_error_for_a_file_that_is_not_an_image() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.png");
        std::fs::write(&path, b"definitely not a png").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn reports_an_error_for_a_file_that_is_not_an_image() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.png");
        std::fs::write(&path, b"definitely not a png").unwrap();
        assert!(load_thumbnail(&path, (256, 256)).is_err());
    }
}
