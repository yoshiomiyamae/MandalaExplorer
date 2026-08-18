//! Background thumbnail production.
//!
//! Thumbnails are produced off the UI thread, cached on disk between runs, and
//! served newest-request-first: when someone flings the scrollbar, the tiles
//! they are looking at now matter far more than the ones they flew past.

use crate::cache::{CACHE_LIMIT_BYTES, mark_used, sweep};
use anyhow::{Context, Result, bail};
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

/// A request to produce one thumbnail.
#[derive(Debug, Clone)]
pub struct ThumbnailRequest {
    pub key: CacheKey,
    pub path: PathBuf,
    pub kind: MediaKind,
    pub tier: u32,
}

/// A finished thumbnail, or the failure that stops it being asked for again.
pub struct ThumbnailResult {
    pub key: CacheKey,
    pub outcome: Result<Frame>,
}

/// Bounds the backlog. Anything older than this many requests is stale enough
/// that the user has almost certainly scrolled past it.
const MAX_PENDING: usize = 1024;

struct Queue {
    inner: Mutex<QueueInner>,
    wake: Condvar,
}

struct QueueInner {
    pending: VecDeque<ThumbnailRequest>,
    shutdown: bool,
}

impl Queue {
    fn push(&self, request: ThumbnailRequest) {
        let mut inner = self.inner.lock().unwrap();
        // Newest first: the tiles on screen right now are at the front.
        inner.pending.push_front(request);
        inner.pending.truncate(MAX_PENDING);
        drop(inner);
        self.wake.notify_one();
    }

    fn pop(&self) -> Option<ThumbnailRequest> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.shutdown {
                return None;
            }
            if let Some(request) = inner.pending.pop_front() {
                return Some(request);
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
    results: Receiver<ThumbnailResult>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl ThumbnailService {
    pub fn new<B: MediaBackend + Clone>(backend: B, on_ready: impl Fn() + Send + Clone + 'static) -> Self {
        let queue = Arc::new(Queue {
            inner: Mutex::new(QueueInner { pending: VecDeque::new(), shutdown: false }),
            wake: Condvar::new(),
        });
        let (tx, results) = unbounded();
        let cache_dir = cache_dir();

        // Decoding a video poster frame is heavy, so a couple of threads short
        // of the core count keeps the UI thread responsive while a folder of
        // videos is being indexed.
        let parallelism = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let worker_count = parallelism.saturating_sub(2).clamp(2, 8);

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

        let workers = (0..worker_count)
            .map(|_| {
                let queue = Arc::clone(&queue);
                let tx: Sender<ThumbnailResult> = tx.clone();
                let backend = backend.clone();
                let cache_dir = cache_dir.clone();
                let on_ready = on_ready.clone();
                std::thread::Builder::new()
                    .name("mandala-thumbs".into())
                    .spawn(move || {
                        while let Some(request) = queue.pop() {
                            let key = request.key.clone();
                            let outcome = produce(&request, &cache_dir, &backend);
                            if tx.send(ThumbnailResult { key, outcome }).is_err() {
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

    /// Queues a thumbnail. Callers are expected to track what they have already
    /// asked for; this does not deduplicate.
    pub fn request(&self, request: ThumbnailRequest) {
        self.queue.push(request);
    }

    /// Takes whatever finished since the last call.
    pub fn drain(&self) -> impl Iterator<Item = ThumbnailResult> + '_ {
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

/// Loads from the disk cache, or produces and caches it.
fn produce(request: &ThumbnailRequest, cache_dir: &Path, backend: &impl MediaBackend) -> Result<Frame> {
    let cached = cache_dir.join(request.key.relative_path("jpg"));
    if let Ok(frame) = load_cached(&cached) {
        // Records the hit, so eviction can tell live thumbnails from dead ones.
        mark_used(&cached);
        return Ok(frame);
    }

    let max = (request.tier, request.tier);
    let frame = match request.kind {
        MediaKind::Image => mandala_media::still::load_thumbnail(&request.path, max)?,
        MediaKind::Video => backend.video_thumbnail(&request.path, max)?,
        other => bail!("{other:?} has no thumbnail"),
    };

    // A cache miss is not worth failing the request over.
    let _ = store_cached(&cached, &frame);
    Ok(frame)
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let rgb = flatten_onto_white(frame);
    let buffer = image::RgbImage::from_raw(frame.width, frame.height, rgb)
        .context("thumbnail dimensions do not match its pixels")?;

    // Written to a temporary name and renamed, so a crash mid-write cannot
    // leave a truncated JPEG that every later run then fails to decode.
    let temporary = path.with_extension("tmp");
    image::DynamicImage::ImageRgb8(buffer).save_with_format(&temporary, image::ImageFormat::Jpeg)?;
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
        let frame = Frame {
            width: 1,
            height: 1,
            rgba: vec![10, 20, 30, 0],
            timestamp: Duration::ZERO,
        };
        assert_eq!(flatten_onto_white(&frame), vec![255, 255, 255]);
    }

    #[test]
    fn half_transparent_pixels_land_between_the_colour_and_white() {
        let frame = Frame {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 128],
            timestamp: Duration::ZERO,
        };
        let got = flatten_onto_white(&frame);
        assert!(got.iter().all(|&c| (125..=130).contains(&c)), "got {got:?}");
    }

    #[test]
    fn a_stored_thumbnail_reloads_at_the_same_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ab").join("thumb.jpg");
        let frame = Frame {
            width: 4,
            height: 2,
            rgba: vec![200; 4 * 2 * 4],
            timestamp: Duration::ZERO,
        };
        store_cached(&path, &frame).unwrap();

        let loaded = load_cached(&path).unwrap();
        assert_eq!((loaded.width, loaded.height), (4, 2));
        assert_eq!(loaded.rgba.len(), 4 * 2 * 4);
        assert!(loaded.rgba.chunks_exact(4).all(|p| p[3] == 255), "reloaded alpha should be opaque");
    }

    #[test]
    fn loading_a_missing_thumbnail_fails_rather_than_panicking() {
        assert!(load_cached(Path::new("no/such/thumb.jpg")).is_err());
    }

    #[test]
    fn the_queue_serves_the_newest_request_first() {
        let queue = Queue {
            inner: Mutex::new(QueueInner { pending: VecDeque::new(), shutdown: false }),
            wake: Condvar::new(),
        };
        for i in 0..3u32 {
            queue.push(ThumbnailRequest {
                key: CacheKey::new(Path::new("a"), i as i128, 0, 128),
                path: PathBuf::from("a"),
                kind: MediaKind::Image,
                tier: 128,
            });
        }
        // The last one pushed is the one on screen now.
        let first = queue.pop().unwrap();
        assert_eq!(first.key, CacheKey::new(Path::new("a"), 2, 0, 128));
    }

    #[test]
    fn the_queue_drops_the_oldest_requests_when_it_overflows() {
        let queue = Queue {
            inner: Mutex::new(QueueInner { pending: VecDeque::new(), shutdown: false }),
            wake: Condvar::new(),
        };
        for i in 0..(MAX_PENDING + 10) {
            queue.push(ThumbnailRequest {
                key: CacheKey::new(Path::new("a"), i as i128, 0, 128),
                path: PathBuf::from("a"),
                kind: MediaKind::Image,
                tier: 128,
            });
        }
        assert_eq!(queue.inner.lock().unwrap().pending.len(), MAX_PENDING);
    }

    #[test]
    fn a_shut_down_queue_stops_handing_out_work() {
        let queue = Queue {
            inner: Mutex::new(QueueInner { pending: VecDeque::new(), shutdown: false }),
            wake: Condvar::new(),
        };
        queue.push(ThumbnailRequest {
            key: CacheKey::new(Path::new("a"), 0, 0, 128),
            path: PathBuf::from("a"),
            kind: MediaKind::Image,
            tier: 128,
        });
        queue.shutdown();
        assert!(queue.pop().is_none());
    }
}
