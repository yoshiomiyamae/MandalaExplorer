//! Plays a file's sound, to hear whether the audio path works at all.
//!
//! The tests can prove a file yields the samples that were asked for. Whether
//! those samples reach a speaker is not something a test can ask, so this
//! exists to be listened to.
//!
//! ```
//! cargo run --release -p mandala-media --example play_sound -- "C:\some\clip.mp4" 5
//! ```

#![cfg(windows)]

use mandala_media::mf::audio::AudioSource;
use mandala_media::mf::speaker::Speaker;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The loudest sample in a block, as a fraction of full scale.
///
/// The device says how wide its samples are and it is usually 32-bit float,
/// not the 16-bit integers one reaches for by habit. Reading the bytes as the
/// wrong type gives a number that looks like a measurement and is not one:
/// this reported a tone at a quarter of full scale as being at all of it.
fn loudest(block: &[u8], bits: u16) -> f32 {
    match bits {
        32 => block
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b).abs())
            .fold(0.0, f32::max),
        16 => block
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| (i16::from_le_bytes(*b) as f32 / 32768.0).abs())
            .fold(0.0, f32::max),
        _ => 0.0,
    }
}

fn main() -> anyhow::Result<()> {
    let path = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("usage: play_sound <file> [secs]"))?,
    );
    let seconds: u64 = std::env::args().nth(2).and_then(|n| n.parse().ok()).unwrap_or(5);

    let mut speaker = Speaker::open()?;
    let (format, size) = speaker.format();
    // Copied out first: WAVEFORMATEX is packed, so a reference to one of its
    // fields is not something that may exist even briefly.
    let (rate, channels, bits) = (format.nSamplesPerSec, format.nChannels, format.wBitsPerSample);
    println!("device    {rate} Hz, {channels} channels, {bits} bits");

    let mut source = AudioSource::open(&path, format, size)?;
    println!("file      {}", path.display());
    println!("playing   {seconds}s -- listen");

    let mut queued: VecDeque<u8> = VecDeque::new();
    let mut peak = 0.0f32;
    speaker.start()?;
    let started = Instant::now();

    while started.elapsed() < Duration::from_secs(seconds) {
        // Keep a little ahead of the device, and loop rather than falling
        // silent, since a tile loops its video.
        while queued.len() < speaker.writable()? {
            match source.next_block()? {
                Some(block) => {
                    peak = peak.max(loudest(&block, bits));
                    queued.extend(block);
                }
                None => source.restart()?,
            }
        }

        let ready: Vec<u8> = queued.iter().copied().collect();
        let written = speaker.write(&ready)?;
        queued.drain(..written);
        std::thread::sleep(Duration::from_millis(10));
    }

    speaker.stop();
    println!("peak      {:.2} of full scale", peak);
    println!(
        "verdict   {}",
        if peak > 0.01 {
            "samples with sound in them reached the device"
        } else {
            "nothing but silence went past -- check the file has audio"
        }
    );
    Ok(())
}
