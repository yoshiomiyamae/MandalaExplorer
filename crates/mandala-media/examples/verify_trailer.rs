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

    let ok = first.width == 1920
        && first.height == 1080
        && bytes < 2 * 1024 * 1024 * 1024
        && duration.is_some_and(|d| d <= Duration::from_secs(60))
        && channels.is_some_and(|n| n >= 2);
    println!("\n{}", if ok { "PASS" } else { "FAILS a Store requirement" });
    if !ok {
        std::process::exit(1);
    }
    Ok(())
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
