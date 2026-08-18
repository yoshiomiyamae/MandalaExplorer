//! Round-trip tests against the real Media Foundation stack.
//!
//! A test video is synthesized with the Sink Writer, then decoded back through
//! the backend. Encoding it here rather than checking in a fixture keeps the
//! repository free of binaries, and it makes the expected pixels exact: the top
//! half is red and the bottom half is blue, so a decoder that flips rows or
//! swaps colour channels fails loudly instead of producing plausible garbage.

#![cfg(windows)]

use mandala_media::backend::{Advance, MediaBackend};
use mandala_media::mf::MediaFoundation;
use std::path::Path;
use std::time::Duration;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::HSTRING;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const FRAMES: u32 = 30;
const FPS: u32 = 30;
/// One frame at 30fps, in 100ns units.
const FRAME_HNS: i64 = 10_000_000 / 30;

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

/// Writes an H.264 file whose top half is red and bottom half is blue.
///
/// Returns `Ok(false)` when the machine has no usable H.264 encoder, so the
/// tests skip rather than fail on a stripped-down Windows install.
fn write_test_video(path: &Path) -> windows::core::Result<bool> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;

        let url = HSTRING::from(path.as_os_str());
        let writer = MFCreateSinkWriterFromURL(&url, None, None)?;

        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        output.SetUINT32(&MF_MT_AVG_BITRATE, 4_000_000)?;
        output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        output.SetUINT64(&MF_MT_FRAME_SIZE, pack(WIDTH, HEIGHT))?;
        output.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;

        let Ok(stream) = writer.AddStream(&output) else {
            return Ok(false);
        };

        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, pack(WIDTH, HEIGHT))?;
        input.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        // A positive stride declares the source as top-down, so the encoder
        // does not silently flip the picture before it is ever decoded.
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, WIDTH * 4)?;
        if writer.SetInputMediaType(stream, &input, None).is_err() {
            return Ok(false);
        }

        writer.BeginWriting()?;

        let frame_bytes = (WIDTH * HEIGHT * 4) as usize;
        for index in 0..FRAMES {
            let buffer = MFCreateMemoryBuffer(frame_bytes as u32)?;
            let mut data: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            let pixels = std::slice::from_raw_parts_mut(data, frame_bytes);
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let at = ((y * WIDTH + x) * 4) as usize;
                    // BGRA byte order.
                    let (b, g, r) =
                        if y < HEIGHT / 2 { (0u8, 0u8, 255u8) } else { (255u8, 0u8, 0u8) };
                    pixels[at] = b;
                    pixels[at + 1] = g;
                    pixels[at + 2] = r;
                    pixels[at + 3] = 255;
                }
            }
            buffer.Unlock()?;
            buffer.SetCurrentLength(frame_bytes as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(index as i64 * FRAME_HNS)?;
            sample.SetSampleDuration(FRAME_HNS)?;
            writer.WriteSample(stream, &sample)?;
        }
        writer.Finalize()?;
        Ok(true)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

/// Creates the test video, or `None` when this machine cannot encode H.264.
fn fixture() -> Option<Fixture> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bars.mp4");
    match write_test_video(&path) {
        Ok(true) => Some(Fixture { _dir: dir, path }),
        Ok(false) => {
            eprintln!("skipping: no H.264 encoder available");
            None
        }
        Err(e) => {
            eprintln!("skipping: could not encode test video: {e}");
            None
        }
    }
}

/// Samples a pixel as (r, g, b).
fn pixel(frame: &mandala_media::Frame, x: u32, y: u32) -> (u8, u8, u8) {
    let at = ((y * frame.width + x) * 4) as usize;
    (frame.rgba[at], frame.rgba[at + 1], frame.rgba[at + 2])
}

fn is_reddish((r, _g, b): (u8, u8, u8)) -> bool {
    r > 120 && b < 100
}

fn is_bluish((r, _g, b): (u8, u8, u8)) -> bool {
    b > 120 && r < 100
}

#[test]
fn decodes_a_video_the_right_way_up_and_with_the_right_colours() {
    let Some(f) = fixture() else { return };
    let backend = MediaFoundation::new().unwrap();
    let mut stream = backend.open_video(&f.path, (WIDTH, HEIGHT)).unwrap();

    let Advance::Frame(frame) = stream.advance_to(Duration::ZERO).unwrap() else {
        panic!("no first frame");
    };
    assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));
    assert_eq!(frame.rgba.len(), (WIDTH * HEIGHT * 4) as usize);

    // Sampled a quarter of the way into each half, clear of the chroma
    // bleeding that subsampling leaves along the boundary.
    let top = pixel(&frame, WIDTH / 2, HEIGHT / 4);
    let bottom = pixel(&frame, WIDTH / 2, HEIGHT * 3 / 4);
    assert!(is_reddish(top), "top half should be red, got {top:?}");
    assert!(is_bluish(bottom), "bottom half should be blue, got {bottom:?}");
}

#[test]
fn decodes_scaled_down_when_the_tile_is_small() {
    let Some(f) = fixture() else { return };
    let backend = MediaFoundation::new().unwrap();
    let stream = backend.open_video(&f.path, (64, 64)).unwrap();
    // 320x240 into a 64x64 box keeps the 4:3 ratio.
    assert_eq!(stream.size(), (64, 48));
}

#[test]
fn advancing_moves_forward_and_holds_between_frames() {
    let Some(f) = fixture() else { return };
    let backend = MediaFoundation::new().unwrap();
    let mut stream = backend.open_video(&f.path, (WIDTH, HEIGHT)).unwrap();

    let Advance::Frame(first) = stream.advance_to(Duration::ZERO).unwrap() else {
        panic!("no first frame");
    };
    // Asking again for the same instant must not decode anything new.
    assert!(
        matches!(stream.advance_to(first.timestamp).unwrap(), Advance::Unchanged),
        "a stream should not re-decode for a target it already reached"
    );

    let Advance::Frame(later) = stream.advance_to(Duration::from_millis(500)).unwrap() else {
        panic!("stream did not advance to 500ms");
    };
    assert!(
        later.timestamp >= Duration::from_millis(500),
        "expected to land at or past 500ms, got {:?}",
        later.timestamp
    );
}

#[test]
fn running_off_the_end_reports_end_of_stream_and_restart_rewinds() {
    let Some(f) = fixture() else { return };
    let backend = MediaFoundation::new().unwrap();
    let mut stream = backend.open_video(&f.path, (WIDTH, HEIGHT)).unwrap();

    // The clip is one second long, so this walks past the end.
    let mut hit_end = false;
    for _ in 0..8 {
        if matches!(stream.advance_to(Duration::from_secs(30)).unwrap(), Advance::EndOfStream) {
            hit_end = true;
            break;
        }
    }
    assert!(hit_end, "never reached the end of a one second clip");

    stream.restart().unwrap();
    let Advance::Frame(frame) = stream.advance_to(Duration::ZERO).unwrap() else {
        panic!("restart did not produce a frame");
    };
    assert!(frame.timestamp < Duration::from_millis(200), "restart should rewind to the start");
}

#[test]
fn reports_the_clip_duration() {
    let Some(f) = fixture() else { return };
    let backend = MediaFoundation::new().unwrap();
    let stream = backend.open_video(&f.path, (WIDTH, HEIGHT)).unwrap();
    let duration = stream.duration().expect("mp4 should carry a duration");
    // 30 frames at 30fps, with room for however the muxer rounds it.
    assert!(
        duration >= Duration::from_millis(800) && duration <= Duration::from_millis(1200),
        "expected about one second, got {duration:?}"
    );
}

#[test]
fn produces_a_poster_frame_for_a_thumbnail() {
    let Some(f) = fixture() else { return };
    let backend = MediaFoundation::new().unwrap();
    let frame = backend.video_thumbnail(&f.path, (128, 128)).unwrap();
    assert_eq!((frame.width, frame.height), (128, 96));
    assert!(is_reddish(pixel(&frame, 64, 24)), "poster frame lost its colours");
}

#[test]
fn opening_a_file_that_is_not_a_video_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-a-video.mp4");
    std::fs::write(&path, b"absolutely not an mp4").unwrap();

    let backend = MediaFoundation::new().unwrap();
    assert!(backend.open_video(&path, (128, 128)).is_err());
}

/// Counts threads belonging to this process.
fn thread_count() -> usize {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    unsafe {
        let me = GetCurrentProcessId();
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).unwrap();
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut count = 0;
        if Thread32First(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32OwnerProcessID == me {
                    count += 1;
                }
                if Thread32Next(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
        count
    }
}

#[test]
fn opening_and_dropping_videos_does_not_leak_threads() {
    let Some(f) = fixture() else { return };
    let backend = MediaFoundation::new().unwrap();

    // Media Foundation spins up shared worker pools on first use, so the
    // baseline is taken after those exist rather than before.
    for _ in 0..3 {
        let mut stream = backend.open_video(&f.path, (64, 64)).unwrap();
        let _ = stream.advance_to(Duration::ZERO);
    }
    std::thread::sleep(Duration::from_millis(500));
    let before = thread_count();

    for _ in 0..25 {
        let mut stream = backend.open_video(&f.path, (64, 64)).unwrap();
        let _ = stream.advance_to(Duration::ZERO);
    }
    std::thread::sleep(Duration::from_secs(1));
    let after = thread_count();

    // Some slack for pool threads that linger briefly, but 25 opens must not
    // leave 25 threads behind.
    assert!(
        after <= before + 8,
        "threads grew from {before} to {after} over 25 open/drop cycles"
    );
}

#[test]
fn thumbnailing_many_distinct_files_in_parallel_does_not_leak_threads() {
    // Closer to what browsing a folder actually does: many different files,
    // opened concurrently by a pool of workers. A per-file leak hides from a
    // test that reopens one path, because the source gets reused.
    let Some(f) = fixture() else { return };
    let dir = tempfile::tempdir().unwrap();
    let copies: Vec<std::path::PathBuf> = (0..48)
        .map(|i| {
            let path = dir.path().join(format!("copy{i:03}.mp4"));
            std::fs::copy(&f.path, &path).unwrap();
            path
        })
        .collect();

    let backend = MediaFoundation::new().unwrap();
    for path in copies.iter().take(4) {
        let _ = backend.video_thumbnail(path, (128, 128));
    }
    std::thread::sleep(Duration::from_millis(500));
    let before = thread_count();

    std::thread::scope(|scope| {
        for chunk in copies.chunks(6) {
            scope.spawn(move || {
                for path in chunk {
                    let _ = backend.video_thumbnail(path, (128, 128));
                }
            });
        }
    });
    std::thread::sleep(Duration::from_secs(1));
    let after = thread_count();

    assert!(
        after <= before + 12,
        "threads grew from {before} to {after} while thumbnailing {} files",
        copies.len()
    );
}

#[test]
fn seeking_lands_near_the_requested_position() {
    let Some(f) = fixture() else { return };
    let backend = MediaFoundation::new().unwrap();
    let mut stream = backend.open_video(&f.path, (WIDTH, HEIGHT)).unwrap();

    // Play a little way in, then jump back to the start.
    let _ = stream.advance_to(Duration::from_millis(700)).unwrap();
    stream.seek(Duration::from_millis(200)).unwrap();

    let Advance::Frame(frame) = stream.advance_to(Duration::from_millis(200)).unwrap() else {
        panic!("no frame after seeking");
    };
    // A seek resolves to the nearest keyframe at or before the target, so the
    // frame can be earlier -- it must not be far past.
    assert!(
        frame.timestamp <= Duration::from_millis(400),
        "seek to 200ms produced a frame at {:?}",
        frame.timestamp
    );
    assert!(is_reddish(pixel(&frame, WIDTH / 2, HEIGHT / 4)), "seeked frame lost its colours");
}

#[test]
fn seeking_past_the_end_does_not_wedge_the_stream() {
    let Some(f) = fixture() else { return };
    let backend = MediaFoundation::new().unwrap();
    let mut stream = backend.open_video(&f.path, (WIDTH, HEIGHT)).unwrap();

    // The clip is a second long; ask for a minute in.
    stream.seek(Duration::from_secs(60)).unwrap();
    let _ = stream.advance_to(Duration::from_secs(60)).unwrap();

    // Whatever that produced, seeking home has to bring the video back.
    stream.seek(Duration::ZERO).unwrap();
    let Advance::Frame(frame) = stream.advance_to(Duration::ZERO).unwrap() else {
        panic!("stream did not recover after seeking past the end");
    };
    assert!(frame.timestamp < Duration::from_millis(200));
}
