//! Reading a file's sound, in whatever format the speakers want.
//!
//! Split from the playing of it on purpose. Whether a sound card made a noise
//! is not something a test can ask, but whether a file yields the samples that
//! were asked for is entirely checkable -- and that is the half this program
//! is responsible for.
//!
//! The format comes from the audio device rather than being chosen here, and
//! is handed to Media Foundation as-is. Resampling and channel mixing are the
//! part of audio most likely to be got subtly wrong, and Windows already has
//! a resampler that is not subtly wrong.

use super::{ensure_startup, ensure_thread_com, propvariant_i64};
use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::time::Duration;
use windows::Win32::Media::Audio::WAVEFORMATEX;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::core::{GUID, HSTRING};

const AUDIO_STREAM: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;

/// A file's audio, decoded to a chosen wave format.
pub struct AudioSource {
    reader: IMFSourceReader,
    /// Bytes one second of this format occupies, for pacing and for sizing.
    pub bytes_per_second: u32,
    at_end: bool,
}

// Used from the one thread that owns it, like the video streams beside it.
unsafe impl Send for AudioSource {}

impl AudioSource {
    /// Opens the audio of `path`, decoded to `format`.
    ///
    /// `format` is the device's own, so Media Foundation inserts whatever
    /// resampler is needed and nothing here has to know how.
    pub fn open(path: &Path, format: &WAVEFORMATEX, format_bytes: u32) -> Result<Self> {
        ensure_startup()?;
        ensure_thread_com();

        unsafe {
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)
                .with_context(|| format!("opening {}", path.display()))?;
            reader.SetStreamSelection(ALL_STREAMS, false)?;
            reader
                .SetStreamSelection(AUDIO_STREAM, true)
                .with_context(|| format!("no sound in {}", path.display()))?;

            let want = MFCreateMediaType()?;
            MFInitMediaTypeFromWaveFormatEx(&want, format, format_bytes)
                .context("the device's format is not one a media type can describe")?;
            reader
                .SetCurrentMediaType(AUDIO_STREAM, None, &want)
                .context("this file's audio cannot be decoded to the device's format")?;

            Ok(Self { reader, bytes_per_second: format.nAvgBytesPerSec, at_end: false })
        }
    }

    /// Whether the file has been played to its end.
    pub fn finished(&self) -> bool {
        self.at_end
    }

    /// The next block of samples, or `None` when there is nothing more.
    ///
    /// Blocks are whatever size the decoder feels like producing; the caller
    /// buffers them, since a sound card asks for exactly as many frames as it
    /// has room for and never the same number twice.
    pub fn next_block(&mut self) -> Result<Option<Vec<u8>>> {
        if self.at_end {
            return Ok(None);
        }
        unsafe {
            let mut flags = 0u32;
            let mut sample = None;
            self.reader.ReadSample(
                AUDIO_STREAM,
                0,
                None,
                Some(&mut flags),
                None,
                Some(&mut sample),
            )?;

            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                self.at_end = true;
                return Ok(None);
            }
            let Some(sample) = sample else { return Ok(Some(Vec::new())) };

            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut length = 0u32;
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let block = std::slice::from_raw_parts(data, length as usize).to_vec();
            let _ = buffer.Unlock();
            Ok(Some(block))
        }
    }

    /// Starts again from the beginning, for a clip that loops.
    pub fn restart(&mut self) -> Result<()> {
        self.seek(Duration::ZERO)
    }

    /// Moves to a position in the clip.
    ///
    /// Sound follows the picture rather than the other way round: a tile is
    /// already partway through its clip by the time anyone points at it, and
    /// starting its sound from the beginning is the difference between a
    /// preview and a mess.
    pub fn seek(&mut self, position: Duration) -> Result<()> {
        unsafe {
            // A zeroed GUID as the time format means 100ns units, the only
            // one a Source Reader accepts.
            let mut target = propvariant_i64((position.as_nanos() / 100) as i64);
            let result = self.reader.SetCurrentPosition(&GUID::zeroed(), &target);
            let _ = PropVariantClear(&mut target);
            result.map_err(|e| anyhow!("could not move the sound: {e}"))?;
        }
        self.at_end = false;
        Ok(())
    }
}
