//! Background thumbnail and metadata production.
//!
//! Work happens off the UI thread, is cached on disk between runs, and is
//! served newest-request-first: when someone flings the scrollbar, the tiles
//! they are looking at now matter far more than the ones they flew past.

use crate::cache::{CACHE_LIMIT_BYTES, mark_used, sweep};
use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, unbounded};
use mandala_core::{CacheKey, MediaKind};
use mandala_media::{Frame, MediaBackend};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Sizes thumbnails are generated at.
///
/// The tile size slider is continuous, but generating at every intermediate
/// size would fill the cache with near-duplicates and re-encode constantly.
/// Snapping to a few tiers means dragging the slider reuses what is already
/// there and lets the GPU do the final scaling for free.
pub const TIERS: [u32; 5] = [128, 256, 512, 1024, 2048];

/// Smallest tier that covers a tile of `tile_px`.
pub fn thumbnail_tier(tile_px: u32) -> u32 {
    TIERS.iter().copied().find(|&t| t >= tile_px).unwrap_or(*TIERS.last().unwrap())
}

/// A piece of work for the pool.
#[derive(Debug, Clone)]
pub enum Job {
    /// A thumbnail, plus whatever else opening the file happens to reveal.
    Thumbnail {
        path: PathBuf,
        kind: MediaKind,
        key: CacheKey,
        /// Size-independent key, for facts like running time.
        meta_key: CacheKey,
        tier: u32,
    },
    /// Only the running time, for a video that is not on screen.
    ///
    /// Sorting by length has to know about files nobody has looked at yet, and
    /// reading a container header is far cheaper than decoding a poster frame.
    Duration { path: PathBuf, meta_key: CacheKey },
}

/// What a worker found.
///
/// Results are keyed by path rather than by cache key, so they still find their
/// tile if the thumbnail size changed while the work was in flight.
pub struct JobDone {
    pub path: PathBuf,
    /// Present only for thumbnail jobs. The error is kept so the UI can stop
    /// asking for a thumbnail that cannot be made.
    pub thumbnail: Option<Result<Frame>>,
    pub duration: Option<Duration>,
}

/// Bounds the backlog. Anything older than this many requests is stale enough
/// that the user has almost certainly scrolled past it.
const MAX_PENDING: usize = 1024;

struct Queue {
    inner: Mutex<QueueInner>,
    wake: Condvar,
}

struct QueueInner {
    pending: VecDeque<Job>,
    shutdown: bool,
}

impl Queue {
    fn push(&self, job: Job) {
        let mut inner = self.inner.lock().unwrap();
        // Newest first: the tiles on screen right now are at the front.
        inner.pending.push_front(job);
        inner.pending.truncate(MAX_PENDING);
        drop(inner);
        self.wake.notify_one();
    }

    fn pop(&self) -> Option<Job> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.shutdown {
                return None;
            }
            if let Some(job) = inner.pending.pop_front() {
                return Some(job);
            }
            inner = self.wake.wait(inner).unwrap();
        }
    }

    fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.shutdown = true;
        inner.pending.clear();
        drop(inner);
        self.wake.notify_all();
    }
}

pub struct ThumbnailService {
    queue: Arc<Queue>,
    results: Receiver<JobDone>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl ThumbnailService {
    pub fn new<B: MediaBackend + Clone>(
        backend: B,
        on_ready: impl Fn() + Send + Clone + 'static,
    ) -> Self {
        let queue = Arc::new(Queue {
            inner: Mutex::new(QueueInner { pending: VecDeque::new(), shutdown: false }),
            wake: Condvar::new(),
        });
        let (tx, results) = unbounded();
        let cache_dir = cache_dir();

        // Sweeping walks the whole cache directory, so it happens once on a
        // thread of its own rather than blocking the first folder from loading.
        let sweep_dir = cache_dir.clone();
        std::thread::Builder::new()
            .name("mandala-cache-sweep".into())
            .spawn(move || match sweep(&sweep_dir, CACHE_LIMIT_BYTES) {
                Ok(report) if report.removed > 0 => eprintln!(
                    "thumbnail cache: evicted {} of {} files, freeing {} MB",
                    report.removed,
                    report.files,
                    report.freed / (1024 * 1024)
                ),
                Ok(_) => {}
                Err(e) => eprintln!("thumbnail cache sweep failed: {e}"),
            })
            .ok();

        // Decoding a video poster frame is heavy, so a couple of threads short
        // of the core count keeps the UI thread responsive while a folder of
        // videos is being indexed.
        let parallelism = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let worker_count = parallelism.saturating_sub(2).clamp(2, 8);

        let workers = (0..worker_count)
            .map(|_| {
                let queue = Arc::clone(&queue);
                let tx: Sender<JobDone> = tx.clone();
                let backend = backend.clone();
                let cache_dir = cache_dir.clone();
                let on_ready = on_ready.clone();
                std::thread::Builder::new()
                    .name("mandala-thumbs".into())
                    .spawn(move || {
                        while let Some(job) = queue.pop() {
                            let done = run(&job, &cache_dir, &backend);
                            if tx.send(done).is_err() {
                                break;
                            }
                            on_ready();
                        }
                    })
                    .expect("spawning a thumbnail worker")
            })
            .collect();

        Self { queue, results, workers }
    }

    /// Queues work. Callers are expected to track what they have already asked
    /// for; this does not deduplicate.
    pub fn request(&self, job: Job) {
        self.queue.push(job);
    }

    /// Takes whatever finished since the last call.
    pub fn drain(&self) -> impl Iterator<Item = JobDone> + '_ {
        self.results.try_iter()
    }
}

impl Drop for ThumbnailService {
    fn drop(&mut self) {
        self.queue.shutdown();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn run(job: &Job, cache_dir: &Path, backend: &impl MediaBackend) -> JobDone {
    match job {
        Job::Duration { path, meta_key } => {
            let duration = load_duration(cache_dir, meta_key).or_else(|| {
                let probed = backend.probe_duration(path).ok().flatten();
                if let Some(duration) = probed {
                    let _ = store_duration(cache_dir, meta_key, duration);
                }
                probed
            });
            JobDone { path: path.clone(), thumbnail: None, duration }
        }
        Job::Thumbnail { path, kind, key, meta_key, tier } => {
            let (thumbnail, duration) =
                produce(path, *kind, key, meta_key, *tier, cache_dir, backend);
            JobDone { path: path.clone(), thumbnail: Some(thumbnail), duration }
        }
    }
}

/// Loads a thumbnail from the disk cache, or makes one and caches it.
fn produce(
    path: &Path,
    kind: MediaKind,
    key: &CacheKey,
    meta_key: &CacheKey,
    tier: u32,
    cache_dir: &Path,
    backend: &impl MediaBackend,
) -> (Result<Frame>, Option<Duration>) {
    let cached = cache_dir.join(key.relative_path("jpg"));
    if let Ok(frame) = load_cached(&cached) {
        // Records the hit, so eviction can tell live thumbnails from dead ones.
        mark_used(&cached);
        // The running time was stored separately and outlives any one tier.
        return (Ok(frame), load_duration(cache_dir, meta_key));
    }

    let max = (tier, tier);
    let (frame, duration) = match kind {
        MediaKind::Image => match mandala_media::still::load_thumbnail(path, max) {
            Ok(frame) => (frame, None),
            Err(e) => return (Err(e), None),
        },
        MediaKind::Video => match backend.video_thumbnail(path, max) {
            Ok(thumbnail) => (thumbnail.frame, thumbnail.duration),
            Err(e) => return (Err(e), None),
        },
        other => return (Err(anyhow!("{other:?} has no thumbnail")), None),
    };

    // Failing to cache is not worth failing the request over.
    let _ = store_cached(&cached, &frame);
    if let Some(duration) = duration {
        let _ = store_duration(cache_dir, meta_key, duration);
    }
    (Ok(frame), duration)
}

fn load_cached(path: &Path) -> Result<Frame> {
    let image = image::ImageReader::open(path)?.with_guessed_format()?.decode()?;
    let (w, h) = (image.width(), image.height());
    Ok(Frame {
        width: w,
        height: h,
        rgba: image.into_rgba8().into_raw(),
        timestamp: Duration::ZERO,
    })
}

fn store_cached(path: &Path, frame: &Frame) -> Result<()> {
    let rgb = flatten_onto_white(frame);
    let buffer = image::RgbImage::from_raw(frame.width, frame.height, rgb)
        .context("thumbnail dimensions do not match its pixels")?;
    write_atomically(path, |temporary| {
        image::DynamicImage::ImageRgb8(buffer)
            .save_with_format(temporary, image::ImageFormat::Jpeg)
            .map_err(Into::into)
    })
}

/// Where a running time is cached.
///
/// Deliberately not under the thumbnail key: how long a video runs for has
/// nothing to do with the size it was last drawn at.
fn duration_path(cache_dir: &Path, meta_key: &CacheKey) -> PathBuf {
    cache_dir.join(meta_key.relative_path("ms"))
}

fn store_duration(cache_dir: &Path, meta_key: &CacheKey, duration: Duration) -> Result<()> {
    let path = duration_path(cache_dir, meta_key);
    write_atomically(&path, |temporary| {
        std::fs::write(temporary, duration.as_millis().to_string()).map_err(Into::into)
    })
}

fn load_duration(cache_dir: &Path, meta_key: &CacheKey) -> Option<Duration> {
    let path = duration_path(cache_dir, meta_key);
    let text = std::fs::read_to_string(&path).ok()?;
    let millis: u64 = text.trim().parse().ok()?;
    mark_used(&path);
    Some(Duration::from_millis(millis))
}

/// Writes through a temporary name, so a crash mid-write cannot leave a
/// truncated file that every later run then fails to read.
fn write_atomically(path: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    write(&temporary)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Drops the alpha channel by compositing onto white, since the cache is JPEG.
fn flatten_onto_white(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::with_capacity((frame.width * frame.height * 3) as usize);
    for pixel in frame.rgba.chunks_exact(4) {
        let alpha = pixel[3] as u32;
        for &channel in &pixel[..3] {
            let value = channel as u32 * alpha + 255 * (255 - alpha);
            out.push((value / 255) as u8);
        }
    }
    out
}

fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("mandala").join("thumbnails")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: i128) -> CacheKey {
        CacheKey::new(Path::new("a"), seed, 0, 128)
    }

    fn thumbnail_job(seed: i128) -> Job {
        Job::Thumbnail {
            path: PathBuf::from("a"),
            kind: MediaKind::Image,
            key: key(seed),
            meta_key: CacheKey::metadata(Path::new("a"), seed, 0),
            tier: 128,
        }
    }

    #[test]
    fn tiers_round_up_to_the_next_size() {
        assert_eq!(thumbnail_tier(1), 128);
        assert_eq!(thumbnail_tier(128), 128);
        assert_eq!(thumbnail_tier(129), 256);
        assert_eq!(thumbnail_tier(512), 512);
    }

    #[test]
    fn tiers_saturate_at_the_largest_size() {
        assert_eq!(thumbnail_tier(1025), 2048);
        // Past the top tier the GPU stretches the largest thumbnail instead.
        assert_eq!(thumbnail_tier(4000), 2048);
    }

    #[test]
    fn opaque_pixels_survive_the_jpeg_flattening_unchanged() {
        let frame = Frame {
            width: 2,
            height: 1,
            rgba: vec![10, 20, 30, 255, 40, 50, 60, 255],
            timestamp: Duration::ZERO,
        };
        assert_eq!(flatten_onto_white(&frame), vec![10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn transparent_pixels_flatten_to_white() {
        let frame =
            Frame { width: 1, height: 1, rgba: vec![10, 20, 30, 0], timestamp: Duration::ZERO };
        assert_eq!(flatten_onto_white(&frame), vec![255, 255, 255]);
    }

    #[test]
    fn half_transparent_pixels_land_between_the_colour_and_white() {
        let frame =
            Frame { width: 1, height: 1, rgba: vec![0, 0, 0, 128], timestamp: Duration::ZERO };
        let got = flatten_onto_white(&frame);
        assert!(got.iter().all(|&c| (125..=130).contains(&c)), "got {got:?}");
    }

    #[test]
    fn a_stored_thumbnail_reloads_at_the_same_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ab").join("thumb.jpg");
        let frame =
            Frame { width: 4, height: 2, rgba: vec![200; 4 * 2 * 4], timestamp: Duration::ZERO };
        store_cached(&path, &frame).unwrap();

        let loaded = load_cached(&path).unwrap();
        assert_eq!((loaded.width, loaded.height), (4, 2));
        assert_eq!(loaded.rgba.len(), 4 * 2 * 4);
        assert!(
            loaded.rgba.chunks_exact(4).all(|p| p[3] == 255),
            "reloaded alpha should be opaque"
        );
    }

    #[test]
    fn loading_a_missing_thumbnail_fails_rather_than_panicking() {
        assert!(load_cached(Path::new("no/such/thumb.jpg")).is_err());
    }

    #[test]
    fn a_stored_duration_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let meta = CacheKey::metadata(Path::new("clip.mp4"), 1, 2);
        store_duration(dir.path(), &meta, Duration::from_millis(90_500)).unwrap();
        assert_eq!(load_duration(dir.path(), &meta), Some(Duration::from_millis(90_500)));
    }

    #[test]
    fn a_duration_that_was_never_stored_reads_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let meta = CacheKey::metadata(Path::new("clip.mp4"), 1, 2);
        assert_eq!(load_duration(dir.path(), &meta), None);
    }

    #[test]
    fn a_corrupt_duration_file_reads_as_unknown_rather_than_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let meta = CacheKey::metadata(Path::new("clip.mp4"), 1, 2);
        let path = duration_path(dir.path(), &meta);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not a number").unwrap();
        assert_eq!(load_duration(dir.path(), &meta), None);
    }

    #[test]
    fn a_duration_is_found_again_whatever_the_thumbnail_size() {
        // The point of a separate metadata key: resizing tiles must not throw
        // away running times, which never depended on the size.
        let dir = tempfile::tempdir().unwrap();
        let stored = CacheKey::metadata(Path::new("clip.mp4"), 1, 2);
        store_duration(dir.path(), &stored, Duration::from_secs(42)).unwrap();

        let looked_up_later = CacheKey::metadata(Path::new("clip.mp4"), 1, 2);
        assert_eq!(load_duration(dir.path(), &looked_up_later), Some(Duration::from_secs(42)));
    }

    #[test]
    fn the_queue_serves_the_newest_request_first() {
        let queue = Queue {
            inner: Mutex::new(QueueInner { pending: VecDeque::new(), shutdown: false }),
            wake: Condvar::new(),
        };
        for i in 0..3i128 {
            queue.push(thumbnail_job(i));
        }
        // The last one pushed is the one on screen now.
        match queue.pop().unwrap() {
            Job::Thumbnail { key: got, .. } => assert_eq!(got, key(2)),
            other => panic!("expected a thumbnail job, got {other:?}"),
        }
    }

    #[test]
    fn the_queue_drops_the_oldest_requests_when_it_overflows() {
        let queue = Queue {
            inner: Mutex::new(QueueInner { pending: VecDeque::new(), shutdown: false }),
            wake: Condvar::new(),
        };
        for i in 0..(MAX_PENDING as i128 + 10) {
            queue.push(thumbnail_job(i));
        }
        assert_eq!(queue.inner.lock().unwrap().pending.len(), MAX_PENDING);
    }

    #[test]
    fn a_shut_down_queue_stops_handing_out_work() {
        let queue = Queue {
            inner: Mutex::new(QueueInner { pending: VecDeque::new(), shutdown: false }),
            wake: Condvar::new(),
        };
        queue.push(thumbnail_job(0));
        queue.shutdown();
        assert!(queue.pop().is_none());
    }
}
