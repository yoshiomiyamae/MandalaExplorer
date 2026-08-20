//! The pool of decoders that drives inline playback.
//!
//! One thread per slot, each owning a decoder. A slot waits on its command
//! channel with a timeout equal to its frame interval, so it stays responsive
//! to being reassigned while still pacing itself -- no polling loop and no
//! separate timer.

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use mandala_core::slots::SlotPlan;
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

/// What a worker sends back.
pub enum SlotEvent {
    Frame(DecodedFrame),
    /// The stream stopped working, so the slot is idle again.
    ///
    /// Worth saying rather than just falling silent: a decoder dies for
    /// reasons outside the file, above all its GPU device being lost, and a
    /// slot that is recorded as playing but is not gets no second chance. Said
    /// out loud, the tile is rescheduled on the next frame and opens against
    /// whatever device exists by then.
    Lost {
        tile: usize,
    },
}

/// A frame ready for its tile.
pub struct DecodedFrame {
    pub tile: usize,
    pub generation: u64,
    pub frame: Frame,
    /// Length of the clip, so the UI can draw a seek bar without opening the
    /// file a second time to ask.
    pub duration: Option<Duration>,
}

/// Frame rate a slot should run at, given what it holds and what is hovered.
///
/// Stated once: two copies of this drifting apart leaves a slot stuck at the
/// wrong rate until the next hover change, which is hard to trace back.
fn rate_for(tile: Option<usize>, hovered: Option<usize>) -> f32 {
    if tile.is_some() && tile == hovered { HOVER_FPS } else { AMBIENT_FPS }
}

enum Command {
    Play { tile: usize, path: PathBuf, max: (u32, u32), generation: u64, fps: f32 },
    SetFps(f32),
    Seek(Duration),
    Stop,
}

/// Spawns one worker thread, wired to its command channel and the shared sink.
type SpawnSlot = Box<dyn Fn(usize, Receiver<Command>, Sender<SlotEvent>)>;

struct SlotHandle {
    commands: Sender<Command>,
    /// Tile this slot is playing, as far as the UI thread knows.
    holding: Option<usize>,
    fps: f32,
}

pub struct PlaybackService {
    slots: Vec<SlotHandle>,
    /// Mirrors `slots[i].holding`, so it can be handed out as a slice.
    holding: Vec<Option<usize>>,
    events: Receiver<SlotEvent>,
    /// Kept so resizing the pool can spawn more slots wired to the same sink.
    event_sink: Sender<SlotEvent>,
    spawn: SpawnSlot,
}

impl PlaybackService {
    pub fn new<B: MediaBackend + Clone>(
        backend: B,
        on_frame: impl Fn() + Send + Clone + 'static,
    ) -> Self {
        let (event_sink, events) = unbounded();
        let spawn = Box::new(move |slot: usize, commands, sink| {
            let backend = backend.clone();
            let on_frame = on_frame.clone();
            std::thread::Builder::new()
                .name(format!("mandala-play-{slot}"))
                .spawn(move || run_slot(commands, sink, backend, on_frame))
                .expect("spawning a playback worker");
        });
        Self { slots: Vec::new(), holding: Vec::new(), events, event_sink, spawn }
    }

    /// What each slot is playing, in slot order -- the input to slot planning.
    ///
    /// Borrowed rather than collected: the draw path asks several times a
    /// frame, and once per stopped slot, which used to be an allocation each.
    pub fn holding(&self) -> &[Option<usize>] {
        &self.holding
    }

    /// The tiles that currently have a decoder, in no particular order.
    pub fn playing_tiles(&self) -> impl Iterator<Item = usize> + '_ {
        self.holding.iter().flatten().copied()
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
            (self.spawn)(self.slots.len(), receiver, self.event_sink.clone());
            self.slots.push(SlotHandle { commands, holding: None, fps: AMBIENT_FPS });
        }
        self.holding.resize(self.slots.len(), None);
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
            let fps = rate_for(Some(tile), hovered);
            let _ = handle.commands.send(Command::Play { tile, path, max, generation, fps });
            handle.holding = Some(tile);
            handle.fps = fps;
        }
        self.sync_holding();
    }

    fn sync_holding(&mut self) {
        self.holding.clear();
        self.holding.extend(self.slots.iter().map(|s| s.holding));
    }

    /// Raises the frame rate of the hovered tile and drops everyone else back
    /// to ambient. Cheap enough to call every frame.
    pub fn set_hover(&mut self, hovered: Option<usize>) {
        for handle in &mut self.slots {
            let wanted = rate_for(handle.holding, hovered);
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
        self.sync_holding();
    }

    /// Takes whatever the workers have produced since the last call.
    ///
    /// Also frees any slot that reported its stream lost, so the scheduler can
    /// hand that tile to a fresh decoder on the next frame.
    pub fn drain(&mut self) -> Vec<SlotEvent> {
        let events: Vec<SlotEvent> = self.events.try_iter().collect();
        let lost = events.iter().any(|e| matches!(e, SlotEvent::Lost { .. }));
        for event in &events {
            if let SlotEvent::Lost { tile } = event
                && let Some(handle) = self.slots.iter_mut().find(|h| h.holding == Some(*tile))
            {
                handle.holding = None;
            }
        }
        if lost {
            self.sync_holding();
        }
        events
    }
}

// No Drop impl: dropping the slots drops every command sender, which is what
// tells the workers to finish. An earlier version kept JoinHandles to wait on,
// except the spawn closure discarded them, so it waited on an empty list and
// read as an orderly shutdown that was not happening.

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
    /// Where in the clip the last frame handed to the UI sat.
    shown: Duration,
}

impl Playing {
    /// The position to ask the decoder for next.
    ///
    /// Two limits, and the second is the one that matters. Never ahead of the
    /// wall clock, so a clip that decodes faster than real time still plays at
    /// its own speed rather than as fast as the machine can manage. And never
    /// more than a frame past what was last shown, so a clip that decodes
    /// *slower* than real time shows the frames it produces instead of
    /// discarding them.
    ///
    /// Without the second limit a 4K60 file that decodes at 40 fps is asked
    /// for the present moment, cannot reach it, and spends its whole decode
    /// budget on frames it throws away on the way: measured at 2 frames a
    /// second shown against 40 decoded. It falls further behind every call, so
    /// it never recovers. Ordinary clips never showed this because decoding
    /// them is far faster than real time, and chasing the clock costs nothing
    /// when you are already there.
    ///
    /// The cost of the second limit is that such a file plays slowly rather
    /// than dropping frames to stay in time. For a grid of thumbnails that is
    /// the better trade: what a tile owes the viewer is motion, not agreement
    /// with a clock nobody can see.
    fn target(&self) -> Duration {
        let by_clock = self.origin + self.started.elapsed();
        by_clock.min(self.shown + self.interval)
    }

    /// Restarts the clock at a position, after a seek or a loop.
    fn rebase(&mut self, position: Duration) {
        self.origin = position;
        self.started = Instant::now();
        self.next_frame_due = Instant::now();
        self.shown = position;
    }
}

/// Tells the UI a slot has gone quiet, and wakes it so it acts on that.
fn report_lost(events: &Sender<SlotEvent>, on_frame: &impl Fn(), tile: usize) {
    if events.send(SlotEvent::Lost { tile }).is_ok() {
        on_frame();
    }
}

fn interval_for(fps: f32) -> Duration {
    Duration::from_secs_f32(1.0 / fps.clamp(1.0, 120.0))
}

fn run_slot(
    commands: Receiver<Command>,
    events: Sender<SlotEvent>,
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
                        shown: Duration::ZERO,
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
                state.shown = frame.timestamp;
                let decoded = DecodedFrame {
                    tile: state.tile,
                    generation: state.generation,
                    duration: state.stream.duration(),
                    frame,
                };
                if events.send(SlotEvent::Frame(decoded)).is_err() {
                    return;
                }
                on_frame();
            }
            Ok(Advance::Unchanged) => {}
            Ok(Advance::EndOfStream) => {
                // Loop. Rebasing the clock keeps the next target near zero
                // rather than seeking forward through the whole file again.
                if state.stream.restart().is_err() {
                    report_lost(&events, &on_frame, state.tile);
                    playing = None;
                } else {
                    state.rebase(Duration::ZERO);
                }
            }
            Err(_) => {
                report_lost(&events, &on_frame, state.tile);
                playing = None;
            }
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

    /// Builds a `Playing` whose clock is already `behind` past its origin,
    /// which is how a decoder that cannot keep up looks from the outside.
    fn playing_behind(behind: Duration, shown: Duration, fps: f32) -> Playing {
        Playing {
            stream: Box::new(FakeStream {
                frames_left: 1,
                restarts: Arc::new(AtomicUsize::new(0)),
                seeks: Arc::new(Mutex::new(Vec::new())),
                clock: Duration::ZERO,
                die_after: None,
            }),
            tile: 0,
            generation: 0,
            origin: Duration::ZERO,
            started: Instant::now() - behind,
            interval: interval_for(fps),
            next_frame_due: Instant::now(),
            shown,
        }
    }

    #[test]
    fn a_stream_that_has_fallen_behind_asks_for_the_next_frame_not_the_present() {
        // Ten seconds behind, having last shown the frame at one second. Asking
        // for the present would mean decoding nine seconds of frames and
        // discarding every one; asking for the next frame shows them.
        let state = playing_behind(Duration::from_secs(10), Duration::from_secs(1), 60.0);
        let target = state.target();
        assert!(
            target < Duration::from_millis(1100),
            "should ask for just past the last frame shown, asked for {target:?}"
        );
    }

    #[test]
    fn a_stream_that_is_keeping_up_still_follows_the_clock() {
        // Half a second in, having shown a frame moments ago: the clock is the
        // nearer of the two, so playback stays in real time rather than
        // running as fast as the machine can decode.
        let state = playing_behind(Duration::from_millis(500), Duration::from_millis(490), 60.0);
        let target = state.target();
        assert!(
            target >= Duration::from_millis(495) && target <= Duration::from_millis(510),
            "should follow the clock, asked for {target:?}"
        );
    }

    #[test]
    fn playback_never_runs_ahead_of_the_clock() {
        // A decoder that has raced ahead must not be allowed to keep going:
        // the clip would play faster than it was filmed.
        let state = playing_behind(Duration::from_millis(100), Duration::from_secs(5), 60.0);
        assert!(state.target() <= Duration::from_millis(110), "{:?}", state.target());
    }

    #[test]
    fn rebasing_moves_what_was_last_shown_as_well() {
        // Otherwise a loop back to zero would leave the cap five seconds in the
        // future and the chase would start again.
        let mut state = playing_behind(Duration::from_secs(10), Duration::from_secs(5), 60.0);
        state.rebase(Duration::ZERO);
        assert_eq!(state.shown, Duration::ZERO);
        assert!(state.target() <= state.interval + Duration::from_millis(5));
    }

    /// A decoder that produces numbered 1x1 frames and ends after a few.
    struct FakeStream {
        frames_left: usize,
        restarts: Arc<AtomicUsize>,
        seeks: Seeks,
        clock: Duration,
        /// Frames to hand out before failing, standing in for a decoder whose
        /// device disappeared underneath it.
        die_after: Option<usize>,
    }

    impl VideoStream for FakeStream {
        fn advance_to(&mut self, _target: Duration) -> Result<Advance> {
            if let Some(left) = self.die_after.as_mut() {
                if *left == 0 {
                    anyhow::bail!("device lost");
                }
                *left -= 1;
            }
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
    }

    #[derive(Clone)]
    struct FakeBackend {
        opened: Arc<AtomicUsize>,
        restarts: Arc<AtomicUsize>,
        seeks: Seeks,
        /// Paths containing this fragment fail to open.
        fail_on: &'static str,
        /// Frames each stream produces before failing, if it should.
        die_after: Option<usize>,
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
                die_after: self.die_after,
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
            die_after: None,
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

    /// Frames only, for tests that do not care about loss reports.
    fn frames(service: &mut PlaybackService) -> Vec<DecodedFrame> {
        service
            .drain()
            .into_iter()
            .filter_map(|e| match e {
                SlotEvent::Frame(frame) => Some(frame),
                SlotEvent::Lost { .. } => None,
            })
            .collect()
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
        assert_eq!(service.holding(), [None, None, None]);

        service.resize(1);
        assert_eq!(service.slot_count(), 1);
    }

    #[test]
    fn a_started_slot_reports_what_it_holds_and_delivers_frames() {
        let (mut service, opened, _) = service();
        service.resize(2);

        let plan = SlotPlan { stop: vec![], start: vec![(0, 42)] };
        service.apply(&plan, 1, None, source);
        assert_eq!(service.holding(), [Some(42), None]);
        assert_eq!(service.playing_tiles().collect::<Vec<_>>(), vec![42]);

        assert!(eventually(|| opened.load(Ordering::SeqCst) > 0), "video was never opened");
        assert!(
            eventually(|| !frames(&mut service).is_empty()),
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
            if let Some(frame) = frames(&mut service).into_iter().next() {
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
        assert!(eventually(|| !frames(&mut service).is_empty()));

        service.apply(&SlotPlan { stop: vec![0], start: vec![] }, 1, None, source);
        assert_eq!(service.holding(), [None]);

        // Drain whatever was already in flight, then confirm it stays quiet.
        let _ = service.drain();
        std::thread::sleep(Duration::from_millis(200));
        assert!(frames(&mut service).is_empty(), "a stopped slot kept decoding");
    }

    #[test]
    fn playback_loops_at_the_end_of_a_clip() {
        let (mut service, _, restarts) = service();
        service.resize(1);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 1)] }, 1, None, source);

        assert!(
            eventually(|| {
                let _ = service.drain();
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
        assert!(frames(&mut service).is_empty(), "a broken file should not produce frames");
    }

    #[test]
    fn a_tile_with_no_source_leaves_its_slot_idle() {
        let (mut service, opened, _) = service();
        service.resize(1);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 1)] }, 1, None, |_| None);

        assert_eq!(service.holding(), [None]);
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
        assert!(eventually(|| !frames(&mut service).is_empty()));

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
        assert!(eventually(|| !frames(&mut service).is_empty()));

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
            if let Some(decoded) = frames(&mut service).into_iter().next() {
                seen = Some(decoded.duration);
                return true;
            }
            false
        });
        assert_eq!(seen, Some(Some(Duration::from_secs(1))));
    }

    #[test]
    fn a_stream_that_dies_frees_its_slot_for_another_decoder() {
        // What a lost GPU device looks like from here. Without the report, the
        // slot stays recorded as playing, is never rescheduled, and the tile
        // stays dead until the app restarts.
        let opened = Arc::new(AtomicUsize::new(0));
        let backend = FakeBackend {
            opened: Arc::clone(&opened),
            restarts: Arc::new(AtomicUsize::new(0)),
            seeks: Arc::new(Mutex::new(Vec::new())),
            fail_on: "never-matches",
            die_after: Some(2),
        };
        let mut service = PlaybackService::new(backend, || {});
        service.resize(1);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 3)] }, 1, None, source);
        assert_eq!(service.holding(), [Some(3)]);

        assert!(
            eventually(|| {
                service.drain();
                service.holding() == [None]
            }),
            "a dead stream should have freed its slot"
        );
    }

    #[test]
    fn stop_all_clears_every_slot() {
        let (mut service, _, _) = service();
        service.resize(2);
        service.apply(&SlotPlan { stop: vec![], start: vec![(0, 1), (1, 2)] }, 1, None, source);
        service.stop_all();
        assert_eq!(service.holding(), [None, None]);
        assert_eq!(service.playing_tiles().count(), 0);
    }
}
