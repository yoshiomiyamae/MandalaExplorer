//! Sound for the tile under the pointer.
//!
//! One clip at a time, always. A grid where every playing tile is also audible
//! is not a feature anyone wants twice, and the pointer is already how this
//! program is told which tile is the interesting one.
//!
//! The device and the decoder both live on a worker thread. Opening either can
//! block for a moment -- a device that is asleep, a file on a share -- and the
//! grid is redrawn sixty times a second.
//!
//! The picture leads and the sound follows it. A tile is already partway
//! through its clip by the time anyone points at it, it loops on its own
//! schedule, and its seek bar can be dragged -- so the sound is told where the
//! picture is and moves to meet it. Only when they have drifted apart, though:
//! a clip that cannot be decoded at full speed plays slowly, and chasing that
//! exactly would mean re-seeking several times a second, which sounds like
//! damage.

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, unbounded};
use mandala_media::mf::audio::AudioSource;
use mandala_media::mf::speaker::Speaker;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long the worker waits for a command before topping the device up.
///
/// Short against the device's own buffer, so the queue never runs dry between
/// visits, and long enough that an idle grid is not a thread spinning.
const TICK: Duration = Duration::from_millis(20);

/// How far the sound may drift from the picture before it is moved.
///
/// Small enough that nobody would call it out of step, large enough that the
/// ordinary wobble of two clocks does not send it seeking. Below about a
/// quarter of a second the re-seeks themselves become the thing you hear.
const TOLERANCE: Duration = Duration::from_millis(400);

/// How often the picture's position is worth repeating to the worker.
///
/// The grid knows it sixty times a second and the worker cannot use it that
/// often; anything that moves the picture further than the tolerance will
/// still be caught within this.
const REPORT_EVERY: Duration = Duration::from_millis(150);

enum Command {
    Play {
        path: PathBuf,
        at: Duration,
    },
    /// Where the picture has got to, for the sound to keep up with.
    At(Duration),
    Stop,
}

/// Plays the sound of one file, and follows the pointer from tile to tile.
pub struct SoundService {
    commands: Sender<Command>,
    worker: Option<JoinHandle<()>>,
    /// What was last asked for, so the same request every frame is free.
    wanted: Option<PathBuf>,
    /// When the picture's position was last passed on.
    reported: Option<Instant>,
}

impl SoundService {
    pub fn new() -> Self {
        let (commands, orders) = unbounded();
        let worker = std::thread::Builder::new()
            .name("mandala-sound".into())
            .spawn(move || run(orders))
            .ok();
        Self { commands, worker, wanted: None, reported: None }
    }

    /// Plays `path` from where its picture has got to, or nothing at all.
    ///
    /// Called every frame with whatever the pointer is over. A change of file
    /// goes through at once; the position that follows is passed on a few
    /// times a second, which is often enough for the worker to notice a seek
    /// or a loop and rare enough not to be a message per frame.
    pub fn follow(&mut self, path: Option<&Path>, at: Duration) {
        if self.wanted.as_deref() != path {
            self.wanted = path.map(Path::to_path_buf);
            self.reported = Some(Instant::now());
            let _ = self.commands.send(match path {
                Some(path) => Command::Play { path: path.to_path_buf(), at },
                None => Command::Stop,
            });
            return;
        }
        if path.is_some() && self.reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
            self.reported = Some(Instant::now());
            let _ = self.commands.send(Command::At(at));
        }
    }
}

impl Default for SoundService {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SoundService {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        drop(std::mem::replace(&mut self.commands, unbounded().0));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Everything the worker is holding while something is audible.
struct Playing {
    source: AudioSource,
    queued: VecDeque<u8>,
    /// Where the clip was last moved to, and how much has been read since.
    /// The two together say where the sound has got to, which is what the
    /// picture's position is compared against.
    from: Duration,
    read: usize,
}

impl Playing {
    /// How far into the clip the sound has been read.
    ///
    /// What has been handed to the device rather than what has come out of it:
    /// the device holds a fraction of a second, and counting that would mean
    /// asking it, every time, for a number that changes nothing.
    fn position(&self, bytes_per_second: u32) -> Duration {
        self.from + Duration::from_secs_f64(self.read as f64 / bytes_per_second.max(1) as f64)
    }

    /// Moves the sound to where the picture is, if they have come apart.
    fn realign(&mut self, picture: Duration, bytes_per_second: u32) -> Result<()> {
        let sound = self.position(bytes_per_second);
        let apart = sound.max(picture) - sound.min(picture);
        if apart < TOLERANCE {
            return Ok(());
        }
        self.source.seek(picture)?;
        // What is already queued belongs to where the sound used to be.
        self.queued.clear();
        self.from = picture;
        self.read = 0;
        Ok(())
    }
}

fn run(orders: Receiver<Command>) {
    // Opened on the first clip rather than at startup: most sessions never
    // turn sound on, and a machine with no output device should not have one
    // opened on its behalf.
    let mut speaker: Option<Speaker> = None;
    // Set once a device has failed to open, so it is not tried again on every
    // hover for the rest of the session.
    let mut refused = false;
    let mut playing: Option<Playing> = None;

    loop {
        match orders.recv_timeout(TICK) {
            Ok(Command::Play { path, at }) => {
                if speaker.is_none() && !refused {
                    match Speaker::open() {
                        Ok(opened) => speaker = Some(opened),
                        Err(_) => refused = true,
                    }
                }
                let Some(device) = speaker.as_mut() else { continue };

                // A new clip means the old one stops mid-word, which is what
                // moving the pointer is asking for.
                device.stop();
                playing = start(device, &path, at).ok().map(|source| Playing {
                    source,
                    queued: VecDeque::new(),
                    from: at,
                    read: 0,
                });
                if playing.is_some() {
                    let _ = device.start();
                }
            }
            Ok(Command::At(at)) => {
                if let (Some(device), Some(state)) = (speaker.as_ref(), playing.as_mut()) {
                    let (format, _) = device.format();
                    let per_second = format.nAvgBytesPerSec;
                    if state.realign(at, per_second).is_err() {
                        playing = None;
                    }
                }
            }
            Ok(Command::Stop) => {
                playing = None;
                if let Some(device) = speaker.as_mut() {
                    device.stop();
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }

        if let (Some(device), Some(state)) = (speaker.as_mut(), playing.as_mut())
            && feed(device, state).is_err()
        {
            // A device that has gone -- unplugged headphones, a session
            // handed to someone else. Quiet is the right failure.
            playing = None;
            speaker = None;
        }
    }
}

/// Opens a file's sound in the device's own format, at the picture's position.
fn start(device: &Speaker, path: &Path, at: Duration) -> Result<AudioSource> {
    let (format, size) = device.format();
    let mut source = AudioSource::open(path, format, size)?;
    if at > Duration::ZERO {
        source.seek(at)?;
    }
    Ok(source)
}

/// Keeps the device's buffer full, looping the clip when it ends.
fn feed(device: &mut Speaker, state: &mut Playing) -> Result<()> {
    let room = device.writable()?;
    while state.queued.len() < room {
        match state.source.next_block()? {
            Some(block) => {
                state.read += block.len();
                state.queued.extend(block);
            }
            // The tile loops its video, so its sound loops with it -- and the
            // reckoning of where the sound is starts again from the top.
            None => {
                state.source.restart()?;
                state.from = Duration::ZERO;
                state.read = 0;
            }
        }
    }

    // One contiguous run, since the device takes a slice and the queue is a
    // ring. Only as much as there is room for, so nothing is copied twice.
    let ready: Vec<u8> = state.queued.iter().take(room).copied().collect();
    let written = device.write(&ready)?;
    state.queued.drain(..written);
    Ok(())
}
