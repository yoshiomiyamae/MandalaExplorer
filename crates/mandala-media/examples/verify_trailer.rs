//! Verifies a finished trailer against what the Store requires.
#![cfg(windows)]
use mandala_media::backend::{Advance, MediaBackend};
use std::path::{Path, PathBuf};
use std::time::Duration;
use windows::Win32::Media::MediaFoundation::*;
use windows::core::HSTRING;

fn main() -> anyhow::Result<()> {
    let path = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "trailer.mp4".into()));
    let backend = mandala_media::default_backend()?;

    // Opened at a size far larger than the file, so no downscale is applied and
    // the reported frame size is the file's own.
    let mut stream = backend.open_video(&path, (4096, 4096))?;
    let duration = stream.duration();

    let Advance::Frame(first) = stream.advance_to(Duration::ZERO)? else {
        anyhow::bail!("no first frame");
    };

    let bytes = std::fs::metadata(&path)?.len();
    println!("file      {}", path.display());
    println!("size      {:.1} MB   (Store limit 2048 MB)", bytes as f64 / 1048576.0);
    println!("frame     {}x{}   (Store requires 1920x1080)", first.width, first.height);
    match duration {
        Some(d) => println!("duration  {:.1}s   (60s or less recommended)", d.as_secs_f64()),
        None => println!("duration  unknown"),
    }

    // Walk it to the end, to prove the whole file decodes rather than just its
    // first frame.
    let mut frames = 1;
    let mut t = Duration::ZERO;
    loop {
        t += Duration::from_millis(100);
        match stream.advance_to(t)? {
            Advance::Frame(_) => frames += 1,
            Advance::Unchanged => {}
            Advance::EndOfStream => break,
        }
        if t > Duration::from_secs(120) {
            anyhow::bail!("did not reach the end within two minutes");
        }
    }
    println!("decoded   {frames} sample points to the end without error");

    // The Store rejects a trailer whose audio is not stereo or surround, and
    // counts no audio at all as the same fault.
    let channels = audio_channels(&path)?;
    match channels {
        Some(n) => println!("audio     {n} channels   (Store requires 2 or more)"),
        None => println!("audio     none   (Store requires stereo or surround)"),
    }

    // Silence has to be silent. A buffer that was never written is not, and
    // Media Foundation does not promise to hand out zeroed memory -- so the
    // track can carry whatever was in that page, which is heard as noise.
    let peak = audio_peak(&path)?;
    match peak {
        Some(0) => println!("silence   confirmed: every sample is zero"),
        Some(peak) => println!("silence   NO: peaks at {peak} of 32767 -- this will be audible"),
        None => println!("silence   could not be checked"),
    }

    let ok = first.width == 1920
        && first.height == 1080
        && bytes < 2 * 1024 * 1024 * 1024
        && duration.is_some_and(|d| d <= Duration::from_secs(60))
        && channels.is_some_and(|n| n >= 2)
        && peak == Some(0);
    println!("\n{}", if ok { "PASS" } else { "FAILS a Store requirement" });
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// The loudest sample in the file's audio, as a 16-bit magnitude.
///
/// Zero means the track really is silent. Anything else is something the
/// viewer will hear, whether or not it was meant to be there.
fn audio_peak(path: &Path) -> anyhow::Result<Option<i32>> {
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        let index = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;

        // Decoded to PCM, so the samples can simply be read.
        let Ok(want) = MFCreateMediaType() else { return Ok(None) };
        want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        want.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        want.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        if reader.SetCurrentMediaType(index, None, &want).is_err() {
            return Ok(None);
        }

        let mut peak = 0i32;
        loop {
            let mut flags = 0u32;
            let mut sample = None;
            reader.ReadSample(index, 0, None, Some(&mut flags), None, Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            let Some(sample) = sample else { continue };
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut length = 0u32;
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let bytes = std::slice::from_raw_parts(data, length as usize);
            for pair in bytes.chunks_exact(2) {
                let value = i16::from_le_bytes([pair[0], pair[1]]) as i32;
                peak = peak.max(value.abs());
            }
            let _ = buffer.Unlock();
        }
        Ok(Some(peak))
    }
}

/// Channels on the file's first audio stream, or `None` if it has none.
fn audio_channels(path: &Path) -> anyhow::Result<Option<u32>> {
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        let index = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        // A file without an audio stream fails to describe one, which is the
        // answer rather than an error.
        let Ok(media_type) = reader.GetCurrentMediaType(index) else {
            return Ok(None);
        };
        Ok(media_type.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).ok())
    }
}
