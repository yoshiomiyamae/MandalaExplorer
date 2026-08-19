//! Encodes a directory of captured PNG frames into an MP4.
//!
//! Written for producing the Store trailer, where the requirement is exactly
//! 1920x1080 H.264. Screen capture cannot hold a steady rate, so the capture
//! writes the wall-clock time of each frame beside them and this resamples
//! from those timestamps onto a fixed frame rate -- otherwise the trailer
//! would play back at whatever speed the machine happened to manage.
//!
//! ```
//! cargo run --release -p mandala-media --example encode_frames -- frames trailer.mp4
//! ```

#![cfg(windows)]

use std::path::{Path, PathBuf};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::HSTRING;

const FPS: u32 = 30;
const FRAME_HNS: i64 = 10_000_000 / FPS as i64;
/// The Store requires exactly this.
const W: u32 = 1920;
const H: u32 = 1080;

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "frames".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "trailer.mp4".into()));

    let mut frames: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            // BMP is what the capture writes: PNG compression costs more than
            // the screen grab itself and halves the achievable frame rate.
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("bmp") || e.eq_ignore_ascii_case("png"))
        })
        .collect();
    frames.sort();
    if frames.is_empty() {
        anyhow::bail!("no BMP or PNG frames in {}", dir.display());
    }

    let stamps = read_timestamps(&dir.join("timestamps.txt"), frames.len())?;
    let duration = stamps.last().copied().unwrap_or(0.0);
    let output_frames = ((duration * FPS as f64).round() as u32).max(1);
    println!(
        "{} captured frames over {duration:.1}s -> {output_frames} frames at {FPS} fps",
        frames.len()
    );

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;

        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(out.as_os_str()), None, None)?;

        let target = MFCreateMediaType()?;
        target.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        target.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        // Well inside the Store's 2 GB ceiling for a clip of this length, and
        // high enough that a grid of moving thumbnails does not turn to mush.
        target.SetUINT32(&MF_MT_AVG_BITRATE, 12_000_000)?;
        target.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        target.SetUINT64(&MF_MT_FRAME_SIZE, pack(W, H))?;
        target.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        target.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        let stream = writer.AddStream(&target)?;

        let source = MFCreateMediaType()?;
        source.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        source.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        source.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        source.SetUINT64(&MF_MT_FRAME_SIZE, pack(W, H))?;
        source.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        source.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        source.SetUINT32(&MF_MT_DEFAULT_STRIDE, W * 4)?;
        writer.SetInputMediaType(stream, &source, None)?;
        writer.BeginWriting()?;

        let bytes = (W * H * 4) as usize;
        let mut cursor = 0usize;
        let mut loaded: Option<(usize, Vec<u8>)> = None;

        for index in 0..output_frames {
            let want = index as f64 / FPS as f64;
            // The last captured frame at or before this instant, which is what
            // was actually on screen then.
            while cursor + 1 < stamps.len() && stamps[cursor + 1] <= want {
                cursor += 1;
            }

            if loaded.as_ref().is_none_or(|(i, _)| *i != cursor) {
                loaded = Some((cursor, load_bgra(&frames[cursor])?));
            }
            let (_, pixels) = loaded.as_ref().expect("just loaded");

            let buffer = MFCreateMemoryBuffer(bytes as u32)?;
            let mut data: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::slice::from_raw_parts_mut(data, bytes).copy_from_slice(pixels);
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(index as i64 * FRAME_HNS)?;
            sample.SetSampleDuration(FRAME_HNS)?;
            writer.WriteSample(stream, &sample)?;

            if index % 60 == 0 {
                print!(".");
                use std::io::Write;
                std::io::stdout().flush().ok();
            }
        }
        writer.Finalize()?;
    }

    let size = std::fs::metadata(&out)?.len();
    println!("\nwrote {} ({:.1} MB)", out.display(), size as f64 / (1024.0 * 1024.0));
    Ok(())
}

/// Capture timestamps in seconds. Missing or short files fall back to an even
/// spacing, which is wrong but still produces a watchable clip.
fn read_timestamps(path: &Path, frames: usize) -> anyhow::Result<Vec<f64>> {
    let evenly = |n: usize| (0..n).map(|i| i as f64 / FPS as f64).collect::<Vec<_>>();
    let Ok(text) = std::fs::read_to_string(path) else {
        println!("no timestamps.txt; assuming an even {FPS} fps");
        return Ok(evenly(frames));
    };
    let stamps: Vec<f64> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
    if stamps.len() < frames {
        println!(
            "timestamps.txt covers {} of {frames} frames; assuming even spacing",
            stamps.len()
        );
        return Ok(evenly(frames));
    }
    Ok(stamps)
}

/// Loads a frame as the BGRA the encoder wants, scaled to the output size if
/// the capture was not exactly 1920x1080.
fn load_bgra(path: &Path) -> anyhow::Result<Vec<u8>> {
    let image = image::ImageReader::open(path)?.with_guessed_format()?.decode()?;
    let image = if image.width() != W || image.height() != H {
        image.resize_exact(W, H, image::imageops::FilterType::Lanczos3)
    } else {
        image
    };

    let rgba = image.into_rgba8();
    let mut bgra = Vec::with_capacity((W * H * 4) as usize);
    for px in rgba.pixels() {
        bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    Ok(bgra)
}
