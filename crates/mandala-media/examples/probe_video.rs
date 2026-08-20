//! Reports what a video is, and where its frames go.
//!
//! Written because "playback is stuttery" has several causes that look
//! identical from the outside. Five plausible guesses were wrong here before
//! this measured the right thing, and the thing that mattered turned out not to
//! be how fast frames decode but how many of them reach the screen.
//!
//! ```
//! cargo run --release -p mandala-media --example probe_video -- "C:\some\clip.mov" 512 4
//! ```
//!
//! The second argument is the tile size to decode for, the third how many
//! copies to play at once -- which is the question a grid actually asks, and
//! one that a single stream keeping up says nothing about.

#![cfg(windows)]

use mandala_media::backend::{Advance, MediaBackend};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::{GUID, HSTRING};

/// The subtypes worth naming. Everything else prints as its GUID, which is
/// still enough to look up.
fn codec_name(subtype: &GUID) -> String {
    let known: [(GUID, &str); 8] = [
        (MFVideoFormat_H264, "H.264"),
        (MFVideoFormat_HEVC, "HEVC"),
        (MFVideoFormat_HEVC_ES, "HEVC (elementary stream)"),
        (MFVideoFormat_MPEG2, "MPEG-2"),
        (MFVideoFormat_MP4V, "MPEG-4 part 2"),
        (MFVideoFormat_WMV3, "WMV9"),
        (MFVideoFormat_MJPG, "Motion JPEG"),
        (MFVideoFormat_AV1, "AV1"),
    ];
    known
        .iter()
        .find(|(guid, _)| guid == subtype)
        .map(|(_, name)| (*name).to_owned())
        .unwrap_or_else(|| format!("{subtype:?}"))
}

fn main() -> anyhow::Result<()> {
    let path = PathBuf::from(
        std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: probe_video <file>"))?,
    );
    let tile: u32 = std::env::args().nth(2).and_then(|n| n.parse().ok()).unwrap_or(512);
    let streams: usize = std::env::args().nth(3).and_then(|n| n.parse().ok()).unwrap_or(1);

    println!("file      {}", path.display());
    if let Ok(meta) = std::fs::metadata(&path) {
        println!("size      {:.1} MB", meta.len() as f64 / 1_048_576.0);
    }
    describe(&path)?;

    // Hardware decoding, settled by taking it away. The performance counter for
    // the video-decode engine reads zero even while decoding H.264 at hundreds
    // of frames a second, so it cannot answer this; a decoder that collapses
    // when the Direct3D device is withheld was using it.
    println!("---- is the GPU decoding? ----");
    for with_d3d in [true, false] {
        let rate = raw_rate(&path, tile, with_d3d)?;
        let label = if with_d3d { "with a D3D device" } else { "without one" };
        println!("  {label:<26} {rate:.1} fps");
    }

    // The measurement that matters, and the one that took longest to reach.
    // Decoding fast is not the same as showing frames: a stream asked for the
    // present moment that cannot reach it spends its whole budget on frames it
    // discards on the way, and falls further behind with every call.
    println!("---- frames decoded, against frames shown ({streams} at once) ----");
    let decoded = decoded_rate(&path, tile, streams)?;
    let chasing = shown_rate(&path, tile, streams, true)?;
    let taking = shown_rate(&path, tile, streams, false)?;
    println!("  {:<26} {decoded:.1} fps each", "decoded");
    println!("  {:<26} {chasing:.1} fps each", "shown, chasing the clock");
    println!("  {:<26} {taking:.1} fps each", "shown, taking what arrives");
    Ok(())
}

/// Prints what the file holds, from its own headers.
fn describe(path: &Path) -> anyhow::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;

        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        let native = reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)?;

        let size = native.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let rate = native.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        println!("codec     {}", codec_name(&native.GetGUID(&MF_MT_SUBTYPE)?));
        println!("frame     {}x{}", (size >> 32) as u32, size as u32);
        println!("rate      {:.2} fps", (rate >> 32) as u32 as f64 / (rate as u32).max(1) as f64);
        // For HEVC, 1 is Main and 2 is Main 10; ten-bit costs noticeably more.
        if let Ok(profile) = native.GetUINT32(&MF_MT_VIDEO_PROFILE) {
            println!("profile   {profile}");
        }
    }
    Ok(())
}

/// Frames a bare Source Reader produces per second, reading straight through.
fn raw_rate(path: &Path, tile: u32, with_d3d: bool) -> anyhow::Result<f64> {
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 2)?;
        let attributes = attributes.unwrap();
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        if with_d3d && let Some(device) = mandala_media::mf::d3d::shared_device() {
            attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, device.manager())?;
        }

        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attributes)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

        // The same output the app asks for, so this measures the same work.
        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        output.SetUINT64(&MF_MT_FRAME_SIZE, ((tile as u64) << 32) | tile as u64)?;
        let _ = reader.SetCurrentMediaType(stream, None, &output);

        let started = Instant::now();
        let mut frames = 0u32;
        while started.elapsed() < Duration::from_secs(2) {
            let mut flags = 0u32;
            let mut sample = None;
            reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if sample.is_some() {
                frames += 1;
            }
        }
        Ok(frames as f64 / started.elapsed().as_secs_f64())
    }
}

/// Frames per second the decoder produces through the app's own stream type,
/// asked for one at a time so nothing is ever skipped.
fn decoded_rate(path: &Path, tile: u32, streams: usize) -> anyhow::Result<f64> {
    each_second(streams, |backend, started| {
        let Ok(mut video) = backend.open_video(path, (tile, tile)) else { return 0 };
        let mut frames = 0u32;
        let mut at = Duration::ZERO;
        while started.elapsed() < Duration::from_secs(4) {
            at += Duration::from_millis(1);
            match video.advance_to(at) {
                Ok(Advance::Frame(_)) => frames += 1,
                Ok(Advance::Unchanged) => {}
                _ => break,
            }
        }
        frames
    })
}

/// Frames per second that actually reach a caller, under either policy.
///
/// `chase_clock` asks for the frame belonging to this instant, which is what
/// the app did before it learned better. The alternative asks for a little past
/// the last frame it was given, and shows whatever the decoder manages.
fn shown_rate(path: &Path, tile: u32, streams: usize, chase_clock: bool) -> anyhow::Result<f64> {
    each_second(streams, move |backend, started| {
        let Ok(mut video) = backend.open_video(path, (tile, tile)) else { return 0 };
        let mut shown = 0u32;
        let mut at = Duration::ZERO;
        while started.elapsed() < Duration::from_secs(4) {
            at = if chase_clock { started.elapsed() } else { at + Duration::from_millis(1) };
            match video.advance_to(at) {
                Ok(Advance::Frame(_)) => shown += 1,
                Ok(Advance::Unchanged) => {}
                _ => break,
            }
        }
        shown
    })
}

/// Runs `work` on `streams` threads and averages what they counted.
fn each_second<F>(streams: usize, work: F) -> anyhow::Result<f64>
where
    F: Fn(mandala_media::MediaFoundation, Instant) -> u32 + Copy + Send + Sync,
{
    let backend = mandala_media::MediaFoundation::new()?;
    let started = Instant::now();
    let counts: Vec<u32> = std::thread::scope(|scope| {
        let handles: Vec<_> =
            (0..streams).map(|_| scope.spawn(move || work(backend, started))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or(0)).collect()
    });
    Ok(counts.iter().sum::<u32>() as f64 / streams as f64 / started.elapsed().as_secs_f64())
}
