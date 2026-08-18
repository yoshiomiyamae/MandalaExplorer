//! Generates a folder of sample images and videos to browse.
//!
//! Useful for exercising the grid without pointing it at personal files.
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
    println!("done");
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

/// Writes a two second clip whose colour sweeps, so motion is obvious even in
/// a small tile.
fn write_video(path: &Path, seed: u32) -> anyhow::Result<()> {
    const W: u32 = 640;
    const H: u32 = 360;
    let frames = FPS * 2;

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
