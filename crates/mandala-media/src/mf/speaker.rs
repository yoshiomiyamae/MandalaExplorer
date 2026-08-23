//! The sound card, in shared mode.
//!
//! Shared rather than exclusive: a file browser that silenced everything else
//! on the machine to preview a clip would be an unwelcome guest. Shared mode
//! also means the device dictates the format, which is why nothing here
//! chooses one -- it is read off the device and handed to Media Foundation,
//! and the resampling is Windows' problem rather than ours.

use crate::com::ensure_thread_com;
use anyhow::{Context, Result, anyhow};
use std::time::Duration;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};

/// How much sound is kept queued in the device.
///
/// Long enough that a worker briefly held up does not produce a gap, short
/// enough that moving the pointer to another tile stops the old sound rather
/// than finishing a mouthful of it first.
const BUFFER: Duration = Duration::from_millis(200);

/// An open audio device, ready to be fed.
pub struct Speaker {
    client: IAudioClient,
    render: IAudioRenderClient,
    /// The device's own format. Freed on drop; borrowed by nothing else.
    format: *mut WAVEFORMATEX,
    frames: u32,
    block_align: usize,
    started: bool,
}

// Owned and used by the one thread that opened it.
unsafe impl Send for Speaker {}

impl Speaker {
    /// Opens whatever the machine is currently playing sound through.
    pub fn open() -> Result<Self> {
        // The device enumerator is a COM object like any other, and the thread
        // that opens one is not always a thread that has been in an apartment.
        ensure_thread_com();
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("no audio device enumerator")?;
            let device = devices
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("no audio output device")?;
            let client: IAudioClient =
                device.Activate(CLSCTX_ALL, None).context("could not open the audio device")?;

            let format = client.GetMixFormat().context("the device has no mix format")?;
            let block_align = (*format).nBlockAlign as usize;
            let frames_per_second = (*format).nSamplesPerSec;

            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    0,
                    (BUFFER.as_nanos() / 100) as i64,
                    0,
                    format,
                    None,
                )
                .context("the device refused the shared-mode buffer")?;

            let frames = client.GetBufferSize().context("no buffer size")?;
            let render: IAudioRenderClient = client.GetService().context("no render client")?;

            // Not a number anyone asked for, but a device whose buffer is a
            // fraction of a second is not one this can pace against.
            if frames == 0 || frames_per_second == 0 {
                return Err(anyhow!("the device reports a buffer it cannot fill"));
            }

            Ok(Self { client, render, format, frames, block_align, started: false })
        }
    }

    /// The format to decode into, and its size in bytes.
    ///
    /// Handed straight to `AudioSource`, which passes it to Media Foundation:
    /// the device says what it wants and the decoder is told to produce that,
    /// so no code here converts anything.
    pub fn format(&self) -> (&WAVEFORMATEX, u32) {
        // Safety: owned for as long as this Speaker, and never handed out
        // beyond its lifetime.
        let format = unsafe { &*self.format };
        (format, (size_of::<WAVEFORMATEX>() + format.cbSize as usize) as u32)
    }

    /// Room for this many bytes of sound right now.
    pub fn writable(&self) -> Result<usize> {
        unsafe {
            let queued = self.client.GetCurrentPadding().context("could not read the padding")?;
            Ok((self.frames.saturating_sub(queued)) as usize * self.block_align)
        }
    }

    /// Hands over as much of `pcm` as there is room for, and says how much.
    pub fn write(&mut self, pcm: &[u8]) -> Result<usize> {
        let room = self.writable()?;
        let bytes = room.min(pcm.len() - pcm.len() % self.block_align.max(1));
        if bytes == 0 {
            return Ok(0);
        }
        let frames = (bytes / self.block_align) as u32;
        unsafe {
            let buffer = self.render.GetBuffer(frames).context("no buffer to write into")?;
            std::slice::from_raw_parts_mut(buffer, bytes).copy_from_slice(&pcm[..bytes]);
            self.render.ReleaseBuffer(frames, 0).context("could not release the buffer")?;
        }
        Ok(bytes)
    }

    /// Fills the device with silence, so a stop does not leave the last
    /// fraction of a second repeating.
    pub fn write_silence(&mut self) -> Result<()> {
        let room = self.writable()?;
        if room == 0 {
            return Ok(());
        }
        let frames = (room / self.block_align) as u32;
        unsafe {
            let buffer = self.render.GetBuffer(frames).context("no buffer to silence")?;
            std::slice::from_raw_parts_mut(buffer, room).fill(0);
            self.render.ReleaseBuffer(frames, 0)?;
        }
        Ok(())
    }

    pub fn start(&mut self) -> Result<()> {
        if !self.started {
            unsafe { self.client.Start().context("the device would not start")? };
            self.started = true;
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        if self.started {
            unsafe {
                let _ = self.client.Stop();
                let _ = self.client.Reset();
            }
            self.started = false;
        }
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        self.stop();
        unsafe { CoTaskMemFree(Some(self.format as *const _)) };
    }
}
