//! Generates a folder of sample images and videos to browse.
//!
//! Useful for exercising the grid without pointing it at personal files. What
//! it writes is chosen to reach the awkward cases rather than the pretty ones:
//! subfolders holding one, two, three and four pictures so every mosaic layout
//! appears, a folder holding nothing, a folder holding only more folders, and
//! a HEIC where Windows has the codec for one.
//!
//! ```
//! cargo run -p mandala-media --example make_samples -- C:\some\folder 40
//! ```

#![cfg(windows)]

use std::path::{Path, PathBuf};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::HSTRING;

const FPS: u32 = 30;
const FRAME_HNS: i64 = 10_000_000 / FPS as i64;

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "samples".into()));
    let count: u32 = args.next().and_then(|n| n.parse().ok()).unwrap_or(24);

    std::fs::create_dir_all(&dir)?;
    println!("writing {count} samples to {}", dir.display());

    for i in 0..count {
        // Numbered in one sequence so stills and clips interleave in the grid,
        // which is the arrangement worth looking at.
        if i % 3 == 0 {
            let path = dir.join(format!("item{i:03}.mp4"));
            write_video(&path, i)?;
        } else {
            let path = dir.join(format!("item{i:03}.png"));
            write_image(&path, i)?;
        }
    }
    write_folders(&dir)?;

    // Only where Windows has the codec, which is not everywhere: a .heic that
    // cannot be decoded is itself worth looking at, but it is not what this is
    // for.
    let heic = dir.join("from-a-phone.heic");
    match write_heic(&heic, 7) {
        Ok(()) => println!("wrote {}", heic.display()),
        Err(e) => println!("no HEIC written: {e}"),
    }

    println!("done");
    Ok(())
}

/// Writes the subfolders a folder tile is worth testing against.
fn write_folders(dir: &Path) -> anyhow::Result<()> {
    // One of each count the mosaic arranges differently, and one past it.
    for (name, items) in [("one", 1u32), ("two", 2), ("three", 3), ("four", 4), ("many", 9)] {
        let sub = dir.join(format!("folder-{name}"));
        std::fs::create_dir_all(&sub)?;
        for i in 0..items {
            write_image(&sub.join(format!("{name}-{i}.png")), 100 + i)?;
        }
    }

    // Nothing to build a tile from, which has to fall back rather than fail.
    std::fs::create_dir_all(dir.join("folder-empty"))?;

    // The shape a photo library actually takes: a year of dated folders, with
    // no pictures at the level you are looking at.
    for month in 1..=3u32 {
        let inner = dir.join("folder-by-date").join(format!("2026-{month:02}"));
        std::fs::create_dir_all(&inner)?;
        for i in 0..2u32 {
            write_image(&inner.join(format!("{month:02}-{i}.png")), 200 + month * 10 + i)?;
        }
    }
    Ok(())
}

/// Writes a HEIC through WIC, which only works where the codec is installed.
fn write_heic(path: &Path, seed: u32) -> anyhow::Result<()> {
    use anyhow::Context;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

    const W: u32 = 960;
    const H: u32 = 720;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER)
                .context("no WIC factory")?;

        let encoder = factory
            .CreateEncoder(&GUID_ContainerFormatHeif, std::ptr::null())
            .context("no HEIF encoder on this machine")?;

        let stream = factory.CreateStream()?;
        stream.InitializeFromFilename(
            &HSTRING::from(path.as_os_str()),
            windows::Win32::Foundation::GENERIC_WRITE.0,
        )?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;

        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.context("the encoder made no frame")?;
        frame.Initialize(None)?;
        frame.SetSize(W, H)?;
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut format)?;

        // Something recognisable rather than a flat colour, so a decode that
        // half works is visibly different from one that works.
        let mut pixels = vec![0u8; (W * H * 4) as usize];
        for y in 0..H {
            for x in 0..W {
                let i = ((y * W + x) * 4) as usize;
                let u = x as f32 / W as f32;
                let v = y as f32 / H as f32;
                let ring = ((u - 0.5).hypot(v - 0.5) * 14.0 + seed as f32).sin() * 0.5 + 0.5;
                pixels[i] = (255.0 * ring * u) as u8;
                pixels[i + 1] = (255.0 * (1.0 - ring)) as u8;
                pixels[i + 2] = (255.0 * ring * v) as u8;
                pixels[i + 3] = 255;
            }
        }
        frame.WritePixels(H, W * 4, &pixels)?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}

/// Writes a still with a distinct hue and a size that varies, so the grid has
/// both landscape and portrait tiles to fit.
fn write_image(path: &Path, seed: u32) -> anyhow::Result<()> {
    let (w, h) = match seed % 3 {
        0 => (800u32, 600u32),
        1 => (600, 900),
        _ => (1000, 1000),
    };
    let (r, g, b) = hue(seed);
    let mut image = image::RgbImage::from_pixel(w, h, image::Rgb([r, g, b]));
    // A corner marker makes it obvious if a thumbnail is ever drawn flipped.
    for y in 0..h / 8 {
        for x in 0..w / 8 {
            image.put_pixel(x, y, image::Rgb([255, 255, 255]));
        }
    }
    image.save(path)?;
    Ok(())
}

/// Writes a short clip whose colour sweeps, so motion is obvious even in a
/// small tile. Lengths vary between clips so that sorting by duration has
/// something to actually sort.
fn write_video(path: &Path, seed: u32) -> anyhow::Result<()> {
    const W: u32 = 640;
    const H: u32 = 360;
    let seconds = 1 + (seed % 5);
    let frames = FPS * seconds;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;

        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, None)?;

        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        output.SetUINT32(&MF_MT_AVG_BITRATE, 3_000_000)?;
        output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        output.SetUINT64(&MF_MT_FRAME_SIZE, pack(W, H))?;
        output.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        let stream = writer.AddStream(&output)?;

        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, pack(W, H))?;
        input.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, W * 4)?;
        writer.SetInputMediaType(stream, &input, None)?;

        writer.BeginWriting()?;

        let bytes = (W * H * 4) as usize;
        for index in 0..frames {
            let buffer = MFCreateMemoryBuffer(bytes as u32)?;
            let mut data: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            let pixels = std::slice::from_raw_parts_mut(data, bytes);

            // A bar that sweeps left to right over a hue that shifts per frame.
            let sweep = (index * W / frames) as i64;
            let (r, g, b) = hue(seed + index);
            for y in 0..H {
                for x in 0..W {
                    let at = ((y * W + x) * 4) as usize;
                    let near_bar = (x as i64 - sweep).abs() < 24;
                    let (pr, pg, pb) = if near_bar { (255, 255, 255) } else { (r, g, b) };
                    pixels[at] = pb;
                    pixels[at + 1] = pg;
                    pixels[at + 2] = pr;
                    pixels[at + 3] = 255;
                }
            }
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(index as i64 * FRAME_HNS)?;
            sample.SetSampleDuration(FRAME_HNS)?;
            writer.WriteSample(stream, &sample)?;
        }
        writer.Finalize()?;
    }
    Ok(())
}

/// Spreads seeds around the colour wheel so neighbouring tiles look different.
fn hue(seed: u32) -> (u8, u8, u8) {
    let phase = (seed as f32 * 0.37).fract() * 6.0;
    let level = |offset: f32| {
        let v = (phase + offset).rem_euclid(6.0);
        let v = (2.0 - (v - 2.0).abs()).clamp(0.0, 1.0);
        (v * 200.0 + 30.0) as u8
    };
    (level(0.0), level(4.0), level(2.0))
}
