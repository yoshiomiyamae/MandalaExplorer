//! Decoding stills through Windows Imaging Component.
//!
//! The wrapper is exercised with PNG rather than HEIC on purpose. What belongs
//! to us is the same set of things the Media Foundation round trip pins down --
//! channel order, row orientation, the scale -- and all of those fail silently
//! and look almost right. What belongs to Windows is whether a HEIF codec is
//! installed at all, which is a runtime fact about the machine rather than
//! something our code can be wrong about, so it is checked separately and never
//! asserted to be true: the HEVC extension HEIC needs is a paid download.

#![cfg(windows)]

use mandala_media::wic;
use std::path::{Path, PathBuf};

/// A test image with a different colour in each quadrant, so a decoder that
/// flips rows or swaps channels cannot produce the same answer as one that does
/// not.
fn write_quadrants(dir: &Path) -> PathBuf {
    let path = dir.join("quadrants.png");
    let mut image = image::RgbaImage::new(64, 32);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = match (x < 32, y < 16) {
            (true, true) => image::Rgba([255, 0, 0, 255]),
            (false, true) => image::Rgba([0, 255, 0, 255]),
            (true, false) => image::Rgba([0, 0, 255, 255]),
            (false, false) => image::Rgba([255, 255, 0, 255]),
        };
    }
    image.save(&path).unwrap();
    path
}

fn pixel_at(frame: &mandala_media::Frame, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * frame.width + x) * 4) as usize;
    frame.rgba[i..i + 4].try_into().unwrap()
}

#[test]
fn decodes_at_its_own_size_when_it_already_fits() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_quadrants(tmp.path());
    let frame = wic::load_thumbnail(&path, (256, 256)).unwrap();
    assert_eq!((frame.width, frame.height), (64, 32));
    assert_eq!(frame.rgba.len(), 64 * 32 * 4);
}

#[test]
fn keeps_channels_in_rgba_order_and_rows_the_right_way_up() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_quadrants(tmp.path());
    let frame = wic::load_thumbnail(&path, (256, 256)).unwrap();

    // Red top-left rules out a BGRA swap; blue bottom-left rules out a flip.
    assert_eq!(pixel_at(&frame, 8, 4), [255, 0, 0, 255], "top left");
    assert_eq!(pixel_at(&frame, 48, 4), [0, 255, 0, 255], "top right");
    assert_eq!(pixel_at(&frame, 8, 24), [0, 0, 255, 255], "bottom left");
    assert_eq!(pixel_at(&frame, 48, 24), [255, 255, 0, 255], "bottom right");
}

#[test]
fn scales_down_into_the_box_keeping_its_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_quadrants(tmp.path());
    let frame = wic::load_thumbnail(&path, (32, 32)).unwrap();
    assert_eq!((frame.width, frame.height), (32, 16));

    // The quadrants survive the scale, so the scaler did not mirror anything.
    assert_eq!(pixel_at(&frame, 4, 2)[0], 255, "top left is still red");
    assert_eq!(pixel_at(&frame, 4, 13)[2], 255, "bottom left is still blue");
}

#[test]
fn reports_an_error_rather_than_panicking_on_a_file_that_is_not_an_image() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nope.png");
    std::fs::write(&path, b"definitely not a png").unwrap();
    assert!(wic::load_thumbnail(&path, (256, 256)).is_err());
}

#[test]
fn reports_whether_this_machine_can_decode_heif_without_claiming_it_can() {
    // Deliberately not an assertion about the answer. HEIC needs both the HEIF
    // Image Extension and the HEVC Video Extension, the second of which is a
    // paid download, so a machine without it is a supported machine and a CI
    // runner is quite likely to be one.
    let available = wic::heif_available();
    println!("HEIF codec available: {available}");
}

/// Encodes a HEIF with WIC so the decode path can be exercised on a real one.
///
/// Returns `None` when the machine has no HEIF codec, which is a supported
/// machine rather than a failure -- see the note at the top of this file.
#[cfg(windows)]
fn write_heif(dir: &Path) -> Option<PathBuf> {
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::core::HSTRING;

    if !wic::heif_available() {
        return None;
    }
    let path = dir.join("quadrants.heic");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER).ok()?;

        let stream = factory.CreateStream().ok()?;
        stream
            .InitializeFromFilename(
                &HSTRING::from(path.as_os_str()),
                windows::Win32::Foundation::GENERIC_WRITE.0,
            )
            .ok()?;

        let encoder = factory.CreateEncoder(&GUID_ContainerFormatHeif, std::ptr::null()).ok()?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache).ok()?;

        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut()).ok()?;
        let frame = frame?;
        frame.Initialize(None).ok()?;
        frame.SetSize(64, 32).ok()?;
        // In and out: the encoder moves it to whatever it can actually write.
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut format).ok()?;

        // BGRA, because that is what was just negotiated.
        let mut pixels = vec![0u8; 64 * 32 * 4];
        for y in 0..32u32 {
            for x in 0..64u32 {
                let i = ((y * 64 + x) * 4) as usize;
                let (b, g, r) = match (x < 32, y < 16) {
                    (true, true) => (0, 0, 255),
                    (false, true) => (0, 255, 0),
                    (true, false) => (255, 0, 0),
                    (false, false) => (0, 255, 255),
                };
                pixels[i..i + 4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        frame.WritePixels(32, 64 * 4, &pixels).ok()?;
        frame.Commit().ok()?;
        encoder.Commit().ok()?;
    }
    Some(path)
}

#[test]
fn decodes_a_heic_when_windows_has_the_codec() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(path) = write_heif(tmp.path()) else {
        println!("no HEIF codec on this machine; skipping");
        return;
    };

    let frame = wic::load_thumbnail(&path, (256, 256)).unwrap();
    assert_eq!((frame.width, frame.height), (64, 32));

    // HEIC is lossy, so the corners are checked for which channel dominates
    // rather than for an exact value.
    let brightest = |p: [u8; 4]| (0..3).max_by_key(|&i| p[i]).unwrap();
    assert_eq!(brightest(pixel_at(&frame, 8, 4)), 0, "top left should be red");
    assert_eq!(brightest(pixel_at(&frame, 48, 4)), 1, "top right should be green");
    assert_eq!(brightest(pixel_at(&frame, 8, 24)), 2, "bottom left should be blue");
}

#[test]
fn the_ordinary_loader_falls_through_to_windows_for_a_heic() {
    // The routing, rather than the decoder: still::load_thumbnail is what the
    // rest of the program calls, and a HEIC only reaches WIC if it hands over
    // when the image crate refuses.
    let tmp = tempfile::tempdir().unwrap();
    let Some(path) = write_heif(tmp.path()) else {
        println!("no HEIF codec on this machine; skipping");
        return;
    };
    let frame = mandala_media::still::load_thumbnail(&path, (32, 32)).unwrap();
    assert_eq!((frame.width, frame.height), (32, 16));
}

#[test]
fn a_broken_file_reports_both_decoders_failing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("broken.jpg");
    std::fs::write(&path, b"not a jpeg at all").unwrap();
    let error = mandala_media::still::load_thumbnail(&path, (32, 32)).unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("Windows could not read it either"),
        "the second attempt should be mentioned: {text}"
    );
}
