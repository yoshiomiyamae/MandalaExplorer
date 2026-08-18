//! The pool of decoders that drives inline playback.
//!
//! One thread per slot, each owning a decoder. A slot waits on its command
//! channel with a timeout equal to its frame interval, so it stays responsive
//! to being reassigned while still pacing itself -- no polling loop and no
//! separate timer.

use crate::slots::SlotPlan;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use mandala_media::backend::Advance;
use mandala_media::{Frame, MediaBackend, VideoStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Frame rate for a tile that is merely on screen.
///
/// Thumbnail-sized playback reads as motion well below the source frame rate,
/// and a dozen tiles at full rate is a lot of texture upload for no gain.
pub const AMBIENT_FPS: f32 = 15.0;

/// Frame rate for the tile under the cursor, which is the one being looked at.
pub const HOVER_FPS: f32 = 30.0;

/// A frame ready for its tile.
pub struct DecodedFrame {
    pub tile: usize,
    pub generation: u64,
    pub frame: Frame,
    /// Length of the clip, so the UI can draw a seek bar without opening the
    /// file a second time to ask.
    pub duration: Option<Duration>,
}

enum Command {
    Play { tile: usize, path: PathBuf, max: (u32, u32), generation: u64, fps: f32 },
    SetFps(f32),
    Seek(Duration),
    Stop,
}

/// Spawns one worker thread, wired to its command channel and the shared sink.
type SpawnSlot = Box<dyn Fn(usize, Receiver<Command>, Sender<DecodedFrame>)>;

struct SlotHandle {
    commands: Sender<Command>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Tile this slot is playing, as far as the UI thread knows.
    holding: Option<usize>,
    fps: f32,
}

pub struct PlaybackService {
    slots: Vec<SlotHandle>,
    frames: Receiver<DecodedFrame>,
    /// Kept so resizing the pool can spawn more slots wired to the same sink.
    frame_sink: Sender<DecodedFrame>,
    spawn: SpawnSlot,
}

impl PlaybackService {
    pub fn new<B: MediaBackend + Clone>(
        backend: B,
        on_frame: impl Fn() + Send + Clone + 'static,
    ) -> Self {
        let (frame_sink, frames) = unbounded();
        let spawn = Box::new(move |slot: usize, commands, sink| {
            let backend = backend.clone();
            let on_frame = on_frame.clone();
            std::thread::Builder::new()
                .name(format!("mandala-play-{slot}"))
                .spawn(move || run_slot(commands, sink, backend, on_frame))
                .expect("spawning a playback worker");
        });
        Self { slots: Vec::new(), frames, frame_sink, spawn }
    }

    /// What each slot is playing, in slot order -- the input to slot planning.
    pub fn holding(&self) -> Vec<Option<usize>> {
        self.slots.iter().map(|s| s.holding).collect()
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Grows or shrinks the pool to `count` slots.
    pub fn resize(&mut self, count: usize) {
        while self.slots.len() > count {
            // Dropping the sender ends the worker once it finishes its frame.
            self.slots.pop();
        }
        while self.slots.len() < count {
            let (commands, receiver) = unbounded();
            (self.spawn)(self.slots.len(), receiver, self.frame_sink.clone());
            self.slots.push(SlotHandle { commands, worker: None, holding: None, fps: AMBIENT_FPS });
        }
    }

    /// Carries out a slot plan. `source` supplies the path and decode size for
    /// a tile, and returning `None` simply leaves that slot idle.
    pub fn apply(
        &mut self,
        plan: &SlotPlan,
        generation: u64,
        hovered: Option<usize>,
        source: impl Fn(usize) -> Option<(PathBuf, (u32, u32))>,
    ) {
        for &slot in &plan.stop {
            if let Some(handle) = self.slots.get_mut(slot) {
                let _ = handle.commands.send(Command::Stop);
                handle.holding = None;
            }
        }
        for &(slot, tile) in &plan.start {
            let Some((path, max)) = source(tile) else { continue };
            let Some(handle) = self.slots.get_mut(slot) else { continue };
            let fps = if hovered == Some(tile) { HOVER_FPS } else { AMBIENT_FPS };
            let _ = handle.commands.send(Command::Play { tile, path, max, generation, fps });
            handle.holding = Some(tile);
            handle.fps = fps;
        }
    }

    /// Raises the frame rate of the hovered tile and drops everyone else back
    /// to ambient. Cheap enough to call every frame.
    pub fn set_hover(&mut self, hovered: Option<usize>) {
        for handle in &mut self.slots {
            let wanted = if handle.holding.is_some() && handle.holding == hovered {
                HOVER_FPS
            } else {
                AMBIENT_FPS
            };
            if handle.fps != wanted {
                let _ = handle.commands.send(Command::SetFps(wanted));
                handle.fps = wanted;
            }
        }
    }

    /// Jumps the tile playing in some slot to a position. A tile that is not
    /// playing is ignored, which is the right thing for a seek on a still.
    pub fn seek(&mut self, tile: usize, position: Duration) {
        for handle in &self.slots {
            if handle.holding == Some(tile) {
                let _ = handle.commands.send(Command::Seek(position));
            }
        }
    }

    pub fn stop_all(&mut self) {
        for handle in &mut self.slots {
            if handle.holding.is_some() {
                let _ = handle.commands.send(Command::Stop);
                handle.holding = None;
            }
        }
    }

    pub fn drain(&self) -> impl Iterator<Item = DecodedFrame> + '_ {
        self.frames.try_iter()
    }
}

impl Drop for PlaybackService {
    fn drop(&mut self) {
        // Dropping every command sender is what tells the workers to finish.
        let workers: Vec<_> = self.slots.drain(..).filter_map(|mut s| s.worker.take()).collect();
        for worker in workers {
            let _ = worker.join();
        }
    }
}

struct Playing {
    stream: Box<dyn VideoStream>,
    tile: usize,
    generation: u64,
    /// Position in the clip that `started` corresponds to. The two together
    /// turn wall-clock time into a position in the video.
    origin: Duration,
    started: Instant,
    interval: Duration,
    next_frame_due: Instant,
}

impl Playing {
    fn target(&self) -> Duration {
        self.origin + self.started.elapsed()
    }

    /// Restarts the clock at a position, after a seek or a loop.
    fn rebase(&mut self, position: Duration) {
        self.origin = position;
        self.started = Instant::now();
        self.next_frame_due = Instant::now();
    }
}

fn interval_for(fps: f32) -> Duration {
    Duration::from_secs_f32(1.0 / fps.clamp(1.0, 120.0))
}

fn run_slot(
    commands: Receiver<Command>,
    frames: Sender<DecodedFrame>,
    backend: impl MediaBackend,
    on_frame: impl Fn(),
) {
    let mut playing: Option<Playing> = None;

    loop {
        // Idle slots block; playing slots wait only until their next frame is
        // due, so a reassignment lands within one frame interval either way.
        let command = match &playing {
            None => match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => return,
            },
            Some(state) => {
                let wait = state.next_frame_due.saturating_duration_since(Instant::now());
                match commands.recv_timeout(wait) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        };

        match command {
            Some(Command::Stop) => {
                playing = None;
                continue;
            }
            Some(Command::SetFps(fps)) => {
                if let Some(state) = &mut playing {
                    state.interval = interval_for(fps);
                }
                continue;
            }
            Some(Command::Seek(position)) => {
                if let Some(state) = &mut playing
                    && state.stream.seek(position).is_ok()
                {
                    state.rebase(position);
                }
                continue;
            }
            Some(Command::Play { tile, path, max, generation, fps }) => {
                playing = match backend.open_video(&path, max) {
                    Ok(stream) => Some(Playing {
                        stream,
                        tile,
                        generation,
                        origin: Duration::ZERO,
                        started: Instant::now(),
                        interval: interval_for(fps),
                        // Due immediately, so the first frame appears without
                        // waiting out an interval.
                        next_frame_due: Instant::now(),
                    }),
                    // An unplayable file just leaves its still thumbnail up.
                    Err(_) => None,
                };
                continue;
            }
            None => {}
        }

        let Some(state) = &mut playing else { continue };
        state.next_frame_due = Instant::now() + state.interval;

        match state.stream.advance_to(state.target()) {
            Ok(Advance::Frame(frame)) => {
                let decoded = DecodedFrame {
                    tile: state.tile,
                    generation: state.generation,
                    duration: state.stream.duration(),
                    frame,
                };
                if frames.send(decoded).is_err() {
                    return;
                }
                on_frame();
            }
            Ok(Advance::Unchanged) => {}
            Ok(Advance::EndOfStream) => {
                // Loop. Rebasing the clock keeps the next target near zero
                // rather than seeking forward through the whole file again.
                if state.stream.restart().is_err() {
                    playing = None;
                } else {
                    state.rebase(Duration::ZERO);
                }
            }
            Err(_) => playing = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// A decoder that produces numbered 1x1 frames and ends after a few.
    struct FakeStream {
        frames_left: usize,
        restarts: Arc<AtomicUsize>,
        seeks: Seeks,
        clock: Duration,
    }

    impl VideoStream for FakeStream {
        fn advance_to(&mut self, _target: Duration) -> Result<Advance> {
            if self.frames_left == 0 {
                return Ok(Advance::EndOfStream);
            }
            self.frames_left -= 1;
            self.clock += Duration::from_millis(33);
            Ok(Advance::Frame(Frame {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
                timestamp: self.clock,
            }))
        }

        fn duration(&self) -> Option<Duration> {
            Some(Duration::from_secs(1))
        }

        fn seek(&mut self, position: Duration) -> Result<()> {
            self.seeks.lock().unwrap().push(position);
            self.frames_left = 3;
            self.clock = position;
            Ok(())
        }

        fn restart(&mut self) -> Result<()> {
            self.restarts.fetch_add(1, Ordering::SeqCst);
            self.frames_left = 3;
            self.clock = Duration::ZERO;
            Ok(())
        }

        fn size(&self) -> (u32, u32) {
            (1, 1)
        }
    }

    #[derive(Clone)]
    struct FakeBackend {
        opened: Arc<AtomicUsize>,
        restarts: Arc<AtomicUsize>,
        seeks: Seeks,
        /// Paths containing this fragment fail to open.
        fail_on: &'static str,
    }

    impl MediaBackend for FakeBackend {
        fn open_video(&self, path: &Path, _max: (u32, u32)) -> Result<Box<dyn VideoStream>> {
            if path.to_string_lossy().contains(self.fail_on) {
                anyhow::bail!("cannot open");
            }
            self.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeStream {
                frames_left: 3,
                restarts: Arc::clone(&self.restarts),
                seeks: Arc::clone(&self.seeks),
                clock: Duration::ZERO,
            }))
        }

        fn video_thumbnail(
            &self,
            _path: &Path,
            _max: (u32, u32),
        ) -> Result<mandala_media::VideoThumbnail> {
            anyhow::bail!("not used in these tests")
        }

        fn probe_duration(&self, _path: &Path) -> Result<Option<Duration>> {
            Ok(Some(Duration::from_secs(1)))
        }
    }

    /// Every seek the fake streams received, so tests can assert routing.
    type Seeks = Arc<Mutex<Vec<Duration>>>;

    fn service_with_seeks() -> (PlaybackService, Arc<AtomicUsize>, Arc<AtomicUsize>, Seeks) {
        let opened = Arc::new(AtomicUsize::new(0));
        let restarts = Arc::new(AtomicUsize::new(0));
        let seeks: Seeks = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            opened: Arc::clone(&opened),
            restarts: Arc::clone(&restarts),
            seeks: Arc::clone(&seeks),
            fail_on: "broken",
        };
        (PlaybackService::new(backend, || {}), opened, restarts, seeks)
    }

    fn service() -> (PlaybackService, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (service, opened, restarts, _) = service_with_seeks();
        (service, opened, restarts)
    }

    fn source(_tile: usize) -> Option<(PathBuf, (u32, u32))> {
        Some((PathBuf::from("fake.mp4"), (64, 64)))
    }

    /// Waits for a condition, so tests do not depend on worker scheduling.
    fn eventually(mut check: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn resizing_creates_and_removes_slots() {
        let (mut service, _, _) = service();
        service.resize(3);
        assert_eq!(service.slot_count(), 3);
        assert_eq!(service.holding(), vec![None, None, None]);

        service.resize(1);
        assert_eq!(service.slot_count(), 1);
    }

    #[test]
    fn a_started_slot_reports_what_it_holds_and_delivers_frames() {
        let (mut service, opened, _) = service();
        service.resize(2);

        let plan = SlotPlan { stop: vec![], start: vec![(0, 42)] };
        service.apply(&plan, 1, None, source);
        assert_eq!(service.holding(), vec![Some(42), None]);

        assert!(eventually(|| opened.load(Ordering::SeqCst) > 0), "video was never opened");
        assert!(
            eventually(|| service.drain().next().is_some()),
            "no frame arrived from a playing slot"
        );
    }

    #[test]
    fn frames_carry_the_tile_and_generation_they_belong_to() {
        let (mut service, _, _) = service();
        service.resize(1);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 7)] }, 99, None, source);

        let mut seen = None;
        eventually(|| {
            if let Some(frame) = service.drain().next() {
                seen = Some((frame.tile, frame.generation));
                return true;
            }
            false
        });
        assert_eq!(seen, Some((7, 99)), "frames must be attributable to a tile and generation");
    }

    #[test]
    fn a_stopped_slot_goes_idle() {
        let (mut service, _, _) = service();
        service.resize(1);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 1)] }, 1, None, source);
        assert!(eventually(|| service.drain().next().is_some()));

        service.apply(&SlotPlan { stop: vec![0], start: vec![] }, 1, None, source);
        assert_eq!(service.holding(), vec![None]);

        // Drain whatever was already in flight, then confirm it stays quiet.
        let _ = service.drain().count();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(service.drain().count(), 0, "a stopped slot kept decoding");
    }

    #[test]
    fn playback_loops_at_the_end_of_a_clip() {
        let (mut service, _, restarts) = service();
        service.resize(1);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 1)] }, 1, None, source);

        assert!(
            eventually(|| {
                let _ = service.drain().count();
                restarts.load(Ordering::SeqCst) > 0
            }),
            "a finished clip should have looped"
        );
    }

    #[test]
    fn a_file_that_cannot_be_opened_leaves_the_slot_quiet() {
        let (mut service, _, _) = service();
        service.resize(1);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 1)] }, 1, None, |_| {
            Some((PathBuf::from("broken.mp4"), (64, 64)))
        });

        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(service.drain().count(), 0, "a broken file should not produce frames");
    }

    #[test]
    fn a_tile_with_no_source_leaves_its_slot_idle() {
        let (mut service, opened, _) = service();
        service.resize(1);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 1)] }, 1, None, |_| None);

        assert_eq!(service.holding(), vec![None]);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(opened.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn frame_intervals_follow_the_requested_rate() {
        assert_eq!(interval_for(30.0), Duration::from_secs_f32(1.0 / 30.0));
        // Nonsense rates are clamped rather than producing an infinite wait.
        assert_eq!(interval_for(0.0), Duration::from_secs(1));
        assert_eq!(interval_for(f32::INFINITY), interval_for(120.0));
    }

    #[test]
    fn hovering_a_tile_raises_only_that_slots_rate() {
        let (mut service, _, _) = service();
        service.resize(2);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 1), (1, 2)] }, 1, None, source);

        service.set_hover(Some(2));
        assert_eq!(service.slots[0].fps, AMBIENT_FPS);
        assert_eq!(service.slots[1].fps, HOVER_FPS);

        service.set_hover(None);
        assert_eq!(service.slots[1].fps, AMBIENT_FPS);
    }

    #[test]
    fn seeking_reaches_the_stream_playing_that_tile() {
        let (mut service, _, _, seeks) = service_with_seeks();
        service.resize(2);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 5)] }, 1, None, source);
        assert!(eventually(|| service.drain().next().is_some()));

        service.seek(5, Duration::from_secs(3));
        assert!(
            eventually(|| seeks.lock().unwrap().contains(&Duration::from_secs(3))),
            "the seek never reached the stream"
        );
    }

    #[test]
    fn seeking_a_tile_that_is_not_playing_is_ignored() {
        let (mut service, _, _, seeks) = service_with_seeks();
        service.resize(1);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 5)] }, 1, None, source);
        assert!(eventually(|| service.drain().next().is_some()));

        service.seek(99, Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(150));
        assert!(seeks.lock().unwrap().is_empty(), "a seek reached the wrong tile");
    }

    #[test]
    fn frames_report_the_clip_duration() {
        let (mut service, _, _) = service();
        service.resize(1);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 1)] }, 1, None, source);

        let mut seen = None;
        eventually(|| {
            if let Some(decoded) = service.drain().next() {
                seen = Some(decoded.duration);
                return true;
            }
            false
        });
        assert_eq!(seen, Some(Some(Duration::from_secs(1))));
    }

    #[test]
    fn stop_all_clears_every_slot() {
        let (mut service, _, _) = service();
        service.resize(2);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 1), (1, 2)] }, 1, None, source);
        service.stop_all();
        assert_eq!(service.holding(), vec![None, None]);
    }
}
