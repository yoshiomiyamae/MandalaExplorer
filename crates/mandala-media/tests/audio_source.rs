//! Reading a file's sound back in the format the caller asked for.
//!
//! Whether a speaker made a noise is not something a test can ask. Whether a
//! file yields the samples that were requested, at the rate and width that
//! were requested, is entirely checkable -- and that is the half of the job
//! this program is responsible for.
//!
//! A tone is encoded first so the test owns both ends: silence coming back
//! from a silent file proves nothing, and a file that happens to be on the
//! machine proves nothing twice.

#![cfg(windows)]

use mandala_media::mf::audio::AudioSource;
use std::f32::consts::TAU;
use std::path::{Path, PathBuf};
use windows::Win32::Media::Audio::{WAVE_FORMAT_PCM, WAVEFORMATEX};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::HSTRING;

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;

/// Sixteen-bit stereo at 48 kHz, which is what a device would usually ask for.
fn wave_format() -> WAVEFORMATEX {
    let bits = 16u16;
    let block = CHANNELS * bits / 8;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * block as u32,
        nBlockAlign: block,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

/// Writes an AAC file holding a loud tone, and returns how long it runs.
fn write_tone(dir: &Path, seconds: u32) -> Option<PathBuf> {
    let path = dir.join("tone.mp4");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).ok()?;

        let writer =
            MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, None).ok()?;

        let target = MFCreateMediaType().ok()?;
        target.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).ok()?;
        target.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).ok()?;
        target.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32).ok()?;
        target.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE).ok()?;
        target.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).ok()?;
        target.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000).ok()?;
        let stream = writer.AddStream(&target).ok()?;

        let source = MFCreateMediaType().ok()?;
        source.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).ok()?;
        source.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).ok()?;
        source.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32).ok()?;
        source.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE).ok()?;
        source.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).ok()?;
        source.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, (CHANNELS * 2) as u32).ok()?;
        source.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, RATE * CHANNELS as u32 * 2).ok()?;
        writer.SetInputMediaType(stream, &source, None).ok()?;
        writer.BeginWriting().ok()?;

        // A 440 Hz tone at two thirds of full scale: loud enough that anything
        // arriving quiet has been resampled to nothing rather than decoded.
        let frames = RATE * seconds;
        let mut pcm = Vec::with_capacity((frames * CHANNELS as u32 * 2) as usize);
        for frame in 0..frames {
            let value = ((frame as f32 / RATE as f32 * 440.0 * TAU).sin() * 21000.0) as i16;
            for _ in 0..CHANNELS {
                pcm.extend_from_slice(&value.to_le_bytes());
            }
        }

        let buffer = MFCreateMemoryBuffer(pcm.len() as u32).ok()?;
        let mut data: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None).ok()?;
        std::slice::from_raw_parts_mut(data, pcm.len()).copy_from_slice(&pcm);
        buffer.Unlock().ok()?;
        buffer.SetCurrentLength(pcm.len() as u32).ok()?;

        let sample = MFCreateSample().ok()?;
        sample.AddBuffer(&buffer).ok()?;
        sample.SetSampleTime(0).ok()?;
        sample.SetSampleDuration((seconds as i64) * 10_000_000).ok()?;
        writer.WriteSample(stream, &sample).ok()?;
        writer.Finalize().ok()?;
    }
    Some(path)
}

/// Every sample in a file, and the loudest of them.
fn drain(source: &mut AudioSource) -> (usize, i16) {
    let mut bytes = 0usize;
    let mut peak = 0i16;
    while let Ok(Some(block)) = source.next_block() {
        bytes += block.len();
        for pair in block.as_chunks::<2>().0 {
            peak = peak.max(i16::from_le_bytes(*pair).saturating_abs());
        }
    }
    (bytes, peak)
}

#[test]
fn a_file_gives_back_the_sound_that_was_put_in() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(path) = write_tone(tmp.path(), 2) else {
        println!("no AAC encoder on this machine; skipping");
        return;
    };

    let format = wave_format();
    let mut source = AudioSource::open(&path, &format, size_of::<WAVEFORMATEX>() as u32).unwrap();
    let (bytes, peak) = drain(&mut source);

    // Two seconds of it, give or take what the encoder pads with.
    let seconds = bytes as f64 / format.nAvgBytesPerSec as f64;
    assert!((1.5..3.0).contains(&seconds), "got {seconds:.2}s of audio");
    // Loud, not silence. A pipeline that negotiated itself into nothing would
    // still return the right number of bytes.
    assert!(peak > 8000, "peaked at {peak}, which is not the tone that went in");
}

#[test]
fn the_end_is_reported_once_and_stays_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(path) = write_tone(tmp.path(), 1) else { return };
    let format = wave_format();
    let mut source = AudioSource::open(&path, &format, size_of::<WAVEFORMATEX>() as u32).unwrap();

    drain(&mut source);
    assert!(source.finished());
    assert!(source.next_block().unwrap().is_none(), "asking again must not start it over");
}

#[test]
fn rewinding_plays_it_again() {
    // A tile loops its video, so its sound has to loop with it.
    let tmp = tempfile::tempdir().unwrap();
    let Some(path) = write_tone(tmp.path(), 1) else { return };
    let format = wave_format();
    let mut source = AudioSource::open(&path, &format, size_of::<WAVEFORMATEX>() as u32).unwrap();

    let (first, _) = drain(&mut source);
    assert!(source.finished());
    source.restart().unwrap();
    assert!(!source.finished(), "rewinding has to clear the end");
    let (second, peak) = drain(&mut source);

    assert!(second > first / 2, "the second pass gave {second} bytes against {first}");
    assert!(peak > 8000, "and it has to still be the tone, not silence");
}

#[test]
fn a_file_with_no_sound_says_so_rather_than_playing_nothing() {
    // A silent tile that was meant to be audible is indistinguishable from a
    // broken speaker, so the caller has to be told.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("silent.png");
    image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255])).save(&path).unwrap();

    let format = wave_format();
    assert!(AudioSource::open(&path, &format, size_of::<WAVEFORMATEX>() as u32).is_err());
}

#[test]
fn seeking_lands_where_it_was_asked_to() {
    // The sound of a tile has to start where its picture already is, so a
    // seek that quietly starts from the beginning would be the whole bug.
    let tmp = tempfile::tempdir().unwrap();
    let Some(path) = write_tone(tmp.path(), 4) else { return };
    let format = wave_format();
    let mut source = AudioSource::open(&path, &format, size_of::<WAVEFORMATEX>() as u32).unwrap();

    source.seek(std::time::Duration::from_secs(3)).unwrap();
    let (bytes, peak) = drain(&mut source);

    // A second of a four-second clip, not four.
    let left = bytes as f64 / format.nAvgBytesPerSec as f64;
    assert!(left < 2.0, "seeking to three seconds left {left:.2}s, so it did not seek");
    assert!(left > 0.2, "and it has to leave something, not everything");
    assert!(peak > 8000, "what is left still has to be the tone");
}

#[test]
fn seeking_past_the_end_does_not_strand_the_reader() {
    // Media Foundation refuses every later request once a seek has gone past
    // the end, which for video had to be found the hard way.
    let tmp = tempfile::tempdir().unwrap();
    let Some(path) = write_tone(tmp.path(), 1) else { return };
    let format = wave_format();
    let mut source = AudioSource::open(&path, &format, size_of::<WAVEFORMATEX>() as u32).unwrap();

    let _ = source.seek(std::time::Duration::from_secs(30));
    source.seek(std::time::Duration::ZERO).unwrap();
    let (bytes, peak) = drain(&mut source);
    assert!(bytes > 0, "the reader has to still work after an over-long seek");
    assert!(peak > 8000);
}
