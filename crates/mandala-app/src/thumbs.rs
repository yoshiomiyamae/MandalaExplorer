//! Background thumbnail and metadata production.
//!
//! Work happens off the UI thread, is cached on disk between runs, and is
//! prioritises the newest visible requests. Tiles scrolled past remain in a
//! FIFO background queue, ready to resume or be promoted when visible again.

use crate::cache::{CACHE_LIMIT_BYTES, mark_used, sweep};
use crate::load::{BackgroundBudget, CpuMonitor};
use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, unbounded};
use mandala_core::{
    COVER_SUBFOLDERS, CacheKey, Entry, MediaKind, Sort, SortKey, SortOrder, cover, scan_dir,
    sort_entries,
};
use mandala_media::mosaic::mosaic;
use mandala_media::{Frame, MediaBackend};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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
    /// A mosaic of what is inside a folder.
    ///
    /// Ordinary folders need a listing before their cache key is known. ZIP
    /// folders can use their archive metadata first. Both belong on workers.
    Folder { path: PathBuf, tier: u32 },
    /// Only the running time, for a video that is not on screen.
    ///
    /// Sorting by length has to know about files nobody has looked at yet, and
    /// reading a container header is far cheaper than decoding a poster frame.
    Duration { path: PathBuf, meta_key: CacheKey },
}

impl Job {
    pub fn path(&self) -> &Path {
        match self {
            Self::Thumbnail { path, .. }
            | Self::Folder { path, .. }
            | Self::Duration { path, .. } => path,
        }
    }

    pub fn tier(&self) -> Option<u32> {
        match self {
            Self::Thumbnail { tier, .. } | Self::Folder { tier, .. } => Some(*tier),
            Self::Duration { .. } => None,
        }
    }

    /// A thumbnail at a given tier.
    ///
    /// Both keys come from the tier here rather than from the caller: passing
    /// a key for one size and a tier for another would store an N-pixel image
    /// under the name of an M-pixel one, and poison that file's cache entry
    /// with no error and nothing to notice short of looking at the pixels.
    pub fn thumbnail(entry: &Entry, tier: u32) -> Self {
        if entry.is_dir() {
            return Job::Folder { path: entry.path.clone(), tier };
        }
        Job::Thumbnail {
            path: entry.path.clone(),
            kind: entry.kind,
            key: CacheKey::for_entry(entry, tier),
            meta_key: CacheKey::metadata_for(entry),
            tier,
        }
    }

    /// Just the running time.
    pub fn duration(entry: &Entry) -> Self {
        Job::Duration { path: entry.path.clone(), meta_key: CacheKey::metadata_for(entry) }
    }
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
    pub tier: Option<u32>,
    /// Prefetched pixels stay on disk and never become off-screen GPU textures.
    pub prefetch_epoch: Option<u64>,
}

struct Queue {
    inner: Mutex<QueueInner>,
    wake: Condvar,
}

#[derive(Default)]
struct QueueInner {
    pending: VecDeque<Job>,
    background: BTreeMap<u64, Work>,
    background_ids: HashMap<RequestId, u64>,
    next_background: u64,
    active: usize,
    active_background: HashMap<(RequestId, u64), bool>,
    budget: BackgroundBudget,
    maintenance: usize,
    epoch: u64,
    prefetched_since_sweep: usize,
    shutdown: bool,
}

#[derive(Debug)]
struct Work {
    job: Job,
    prefetch_epoch: Option<u64>,
}

type RequestId = (PathBuf, Option<u32>);

struct Finished {
    promoted: bool,
    sweep: bool,
}

fn same_request(left: &Job, right: &Job) -> bool {
    left.path() == right.path() && left.tier() == right.tier()
}

impl QueueInner {
    #[cfg(test)]
    fn idle(&self) -> bool {
        self.pending.is_empty()
            && self.active == 0
            && self.active_background.is_empty()
            && self.background.is_empty()
            && self.maintenance == 0
    }
    fn remove_background(&mut self, job: &Job) {
        if let Some(sequence) = self.background_ids.remove(&(job.path().to_path_buf(), job.tier()))
        {
            self.background.remove(&sequence);
        }
    }

    fn defer(&mut self, job: Job, epoch: u64) {
        self.remove_background(&job);
        let sequence = self.next_background;
        self.next_background += 1;
        self.background_ids.insert((job.path().to_path_buf(), job.tier()), sequence);
        self.background.insert(sequence, Work { job, prefetch_epoch: Some(epoch) });
    }

    fn foreground_busy(&self) -> bool {
        !self.pending.is_empty()
            || self.active > 0
            || self.active_background.values().any(|promoted| *promoted)
    }

    fn background_capacity(&self) -> usize {
        if self.shutdown || self.foreground_busy() {
            return 0;
        }
        self.budget
            .limit()
            .saturating_sub(self.maintenance + self.active_background.len() + self.background.len())
    }

    fn take_ready(&mut self) -> Option<Work> {
        if let Some(job) = self.pending.pop_front() {
            self.active += 1;
            return Some(Work { job, prefetch_epoch: None });
        }
        if !self.foreground_busy()
            && self.active_background.len() < self.budget.limit().saturating_sub(self.maintenance)
            && let Some((_, work)) = self.background.pop_first()
        {
            self.background_ids.remove(&(work.job.path().to_path_buf(), work.job.tier()));
            self.active_background.insert(
                ((work.job.path().to_path_buf(), work.job.tier()), work.prefetch_epoch.unwrap()),
                false,
            );
            return Some(work);
        }
        None
    }
}

impl Queue {
    fn push(&self, job: Job) {
        let mut inner = self.inner.lock().unwrap();
        inner.budget.pause();
        let active_key = ((job.path().to_path_buf(), job.tier()), inner.epoch);
        if let Some(promoted) = inner.active_background.get_mut(&active_key) {
            // Keep the running decode, but deliver its pixels to the visible
            // tile instead of starting the same work a second time.
            *promoted = true;
            return;
        }
        inner.remove_background(&job);
        inner.pending.retain(|queued| !same_request(queued, &job));
        // Newest first: the tiles on screen right now are at the front.
        inner.pending.push_front(job);
        drop(inner);
        self.wake.notify_one();
    }

    fn pop(&self) -> Option<Work> {
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.shutdown {
                return None;
            }
            if let Some(work) = inner.take_ready() {
                return Some(work);
            }
            inner = self.wake.wait(inner).unwrap();
        }
    }

    fn try_prefetch(&self, job: Job, epoch: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let id = (job.path().to_path_buf(), job.tier());
        if inner.background_capacity() == 0
            || inner.background_ids.contains_key(&id)
            || inner.active_background.contains_key(&(id, epoch))
        {
            return false;
        }
        inner.epoch = epoch;
        inner.defer(job, epoch);
        drop(inner);
        self.wake.notify_one();
        true
    }

    fn defer_outside(
        &self,
        visible: &std::collections::HashSet<PathBuf>,
        tier: u32,
        epoch: u64,
    ) -> Vec<Job> {
        let mut inner = self.inner.lock().unwrap();
        inner.epoch = epoch;
        let mut deferred = Vec::new();
        let pending = std::mem::take(&mut inner.pending);
        for job in pending {
            let keep = job.tier().is_none_or(|t| t == tier && visible.contains(job.path()));
            if keep {
                inner.pending.push_back(job);
            } else {
                deferred.push(job.clone());
                if job.tier() == Some(tier) {
                    inner.defer(job, epoch);
                }
            }
        }
        drop(inner);
        self.wake.notify_all();
        deferred
    }

    fn finish(&self, work: &Work) -> Finished {
        let mut inner = self.inner.lock().unwrap();
        let (promoted, sweep) = if let Some(epoch) = work.prefetch_epoch {
            let promoted = inner
                .active_background
                .remove(&((work.job.path().to_path_buf(), work.job.tier()), epoch))
                .unwrap_or(false);
            inner.prefetched_since_sweep += 1;
            (promoted, inner.prefetched_since_sweep.is_multiple_of(32))
        } else {
            inner.active -= 1;
            (false, false)
        };
        if sweep {
            inner.maintenance += 1;
        }
        drop(inner);
        self.wake.notify_all();
        Finished { promoted, sweep }
    }

    fn finish_sweep(&self) {
        self.inner.lock().unwrap().maintenance -= 1;
        self.wake.notify_all();
    }

    fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.shutdown = true;
        inner.pending.clear();
        inner.background.clear();
        inner.background_ids.clear();
        drop(inner);
        self.wake.notify_all();
    }
}

pub struct ThumbnailService {
    queue: Arc<Queue>,
    results: Receiver<JobDone>,
    workers: Vec<std::thread::JoinHandle<()>>,
    cpu: CpuMonitor,
}

impl ThumbnailService {
    pub fn new<B: MediaBackend + Clone>(
        backend: B,
        on_ready: impl Fn() + Send + Clone + 'static,
    ) -> Self {
        let parallelism = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let worker_count = parallelism.saturating_sub(2).clamp(2, 8);
        let queue = Arc::new(Queue {
            inner: Mutex::new(QueueInner {
                budget: BackgroundBudget::for_workers(worker_count),
                ..Default::default()
            }),
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
                        while let Some(work) = queue.pop() {
                            let done = run(&work.job, &cache_dir, &backend);
                            let finished = queue.finish(&work);
                            let done = finish_result(&work, done, finished.promoted);
                            let sent = tx.send(done);
                            if finished.sweep {
                                if sent.is_ok() {
                                    let _ = sweep(&cache_dir, CACHE_LIMIT_BYTES);
                                }
                                queue.finish_sweep();
                            }
                            if sent.is_err() {
                                break;
                            }
                            on_ready();
                        }
                    })
                    .expect("spawning a thumbnail worker")
            })
            .collect();

        Self { queue, results, workers, cpu: CpuMonitor::default() }
    }

    /// Repeated requests promote existing work rather than enqueueing copies.
    pub fn request(&self, job: Job) {
        self.queue.push(job);
    }

    pub fn prefetch_capacity(&self) -> usize {
        self.queue.inner.lock().unwrap().background_capacity()
    }

    pub fn update_load(&mut self, now: Instant) {
        if let Some(load) = self.cpu.poll(now) {
            let mut inner = self.queue.inner.lock().unwrap();
            let foreground = inner.foreground_busy();
            inner.budget.observe(load, foreground);
            drop(inner);
            self.queue.wake.notify_all();
        }
    }

    pub fn has_work(&self) -> bool {
        let inner = self.queue.inner.lock().unwrap();
        !inner.pending.is_empty()
            || inner.active > 0
            || !inner.background.is_empty()
            || !inner.active_background.is_empty()
            || inner.maintenance > 0
    }

    pub fn prefetch(&self, job: Job, epoch: u64) -> bool {
        self.queue.try_prefetch(job, epoch)
    }

    pub fn rebase_background(&self, epoch: u64, tier: u32) {
        let mut inner = self.queue.inner.lock().unwrap();
        inner.epoch = epoch;
        let obsolete: Vec<_> = inner
            .background
            .iter()
            .filter(|(_, work)| work.job.tier() != Some(tier))
            .map(|(sequence, _)| *sequence)
            .collect();
        for sequence in obsolete {
            if let Some(work) = inner.background.remove(&sequence) {
                inner.background_ids.remove(&(work.job.path().to_path_buf(), work.job.tier()));
            }
        }
        for work in inner.background.values_mut() {
            work.prefetch_epoch = Some(epoch);
        }
    }

    pub fn defer_outside(
        &self,
        visible: &std::collections::HashSet<PathBuf>,
        tier: u32,
        epoch: u64,
    ) -> Vec<Job> {
        self.queue.defer_outside(visible, tier, epoch)
    }

    /// Called when all request records are reset on navigation or reload.
    pub fn clear_pending(&self) {
        let mut inner = self.queue.inner.lock().unwrap();
        inner.pending.clear();
        inner.background.clear();
        inner.background_ids.clear();
        inner.epoch = inner.epoch.wrapping_add(1);
        for promoted in inner.active_background.values_mut() {
            *promoted = false;
        }
    }

    /// Signal cancellation and disconnect results without waiting on an OS
    /// decoder or a network filesystem. Running work exits at its next boundary.
    pub fn shutdown(&mut self) {
        self.queue.shutdown();
        self.results = unbounded().1;
        self.workers.clear();
    }

    /// Takes whatever finished since the last call.
    pub fn drain(&self) -> impl Iterator<Item = JobDone> + '_ {
        self.results.try_iter()
    }
}

impl Drop for ThumbnailService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn finish_result(work: &Work, mut done: JobDone, promoted: bool) -> JobDone {
    done.prefetch_epoch = if promoted { None } else { work.prefetch_epoch };
    if done.prefetch_epoch.is_some() {
        done.thumbnail = None;
    }
    done
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
            JobDone {
                path: path.clone(),
                thumbnail: None,
                duration,
                tier: None,
                prefetch_epoch: None,
            }
        }
        Job::Thumbnail { path, kind, key, meta_key, tier } => {
            let (thumbnail, duration) =
                produce(path, *kind, key, meta_key, *tier, cache_dir, backend);
            JobDone {
                path: path.clone(),
                thumbnail: Some(thumbnail),
                duration,
                tier: Some(*tier),
                prefetch_epoch: None,
            }
        }
        Job::Folder { path, tier } => JobDone {
            path: path.clone(),
            thumbnail: Some(produce_folder(path, *tier, cache_dir, backend)),
            duration: None,
            tier: Some(*tier),
            prefetch_epoch: None,
        },
    }
}

/// ZIP contents change with the archive, so its metadata can identify a cover
/// before reading its index. Ordinary directories still need the member keys.
fn archive_cover_cache_path(dir: &Path, tier: u32, cache_dir: &Path) -> Option<PathBuf> {
    let (archive, _) = mandala_core::archive::location(dir)?;
    let metadata = archive.metadata().ok()?;
    let modified = metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    let key = CacheKey::new(dir, modified.as_nanos() as i128, metadata.len(), tier);
    Some(cache_dir.join(key.relative_path("zip-cover-v1.jpg")))
}

fn produce_folder(
    dir: &Path,
    tier: u32,
    cache_dir: &Path,
    backend: &impl MediaBackend,
) -> Result<Frame> {
    let quick_cache = archive_cover_cache_path(dir, tier, cache_dir);
    if let Some(path) = &quick_cache
        && let Ok(frame) = load_cached(path)
    {
        return Ok(frame);
    }
    let tile = produce_folder_mosaic(dir, tier, cache_dir, backend, quick_cache.is_none())?;
    // Do not publish a cover under an old identity if the ZIP changed while
    // its pictures were being decoded.
    if let Some(path) = quick_cache
        && archive_cover_cache_path(dir, tier, cache_dir).as_ref() == Some(&path)
    {
        let _ = store_cached(&path, &tile);
    }
    Ok(tile)
}

/// Builds a folder's tile out of the first few pictures inside it.
/// Directory reads and image decoding happen only on thumbnail workers.
fn produce_folder_mosaic(
    dir: &Path,
    tier: u32,
    cache_dir: &Path,
    backend: &impl MediaBackend,
    cache_group: bool,
) -> Result<Frame> {
    let by_name = Sort { key: SortKey::Name, order: SortOrder::Ascending };
    let listing = |path: &Path| {
        let mut entries = scan_dir(path).unwrap_or_default();
        // Always by name, whatever the window is sorted by. Sorting these the
        // way the grid happens to be sorted would change every folder's tile,
        // and so every folder's cache key, each time that setting moved.
        sort_entries(&mut entries, by_name, |_| None);
        entries
    };

    let direct = listing(dir);
    let subfolders: Vec<PathBuf> = direct
        .iter()
        .filter(|e| e.is_dir())
        .take(COVER_SUBFOLDERS)
        .map(|e| e.path.clone())
        .collect();
    // Lazily, so a folder whose own pictures fill the tile reads nothing more.
    let picks = cover(&direct, subfolders.iter().map(|p| listing(p)));
    if picks.is_empty() {
        return Err(anyhow!("{} has no pictures to show", dir.display()));
    }

    let key = CacheKey::for_group(dir, &picks, tier);
    let cached = cache_dir.join(key.relative_path("jpg"));
    if let Ok(frame) = load_cached(&cached) {
        return Ok(frame);
    }

    // Cells are half the tile across, so the parts need half the pixels. Asking
    // for a real tier rather than exactly half means these are the same files
    // the folder's own contents will use once someone opens it.
    let part_tier = thumbnail_tier(tier / 2);
    let mut frames = Vec::new();
    for pick in &picks {
        let (frame, _) = produce(
            &pick.path,
            pick.kind,
            &CacheKey::for_entry(pick, part_tier),
            &CacheKey::metadata_for(pick),
            part_tier,
            cache_dir,
            backend,
        );
        // One unreadable picture should cost its cell, not the whole tile.
        if let Ok(frame) = frame {
            frames.push(frame);
        }
    }
    if frames.is_empty() {
        return Err(anyhow!("nothing inside {} could be decoded", dir.display()));
    }

    let tile = mosaic(&frames, tier)?;
    // ZIP covers are stored by the caller under the archive fingerprint.
    // Still read old group entries above, so existing caches migrate lazily.
    if cache_group {
        let _ = store_cached(&cached, &tile);
    }
    Ok(tile)
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
    let frame = mandala_media::still::load(path)?;
    // Marking on read is the loader's job. Left to callers, the next reader
    // added here forgets it, the file keeps an ancient timestamp, and eviction
    // then takes exactly the thumbnails being used most.
    mark_used(path);
    Ok(frame)
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
    for pixel in frame.rgba.as_chunks::<4>().0 {
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

    fn resume_background(queue: &Queue) {
        queue.inner.lock().unwrap().budget.observe(None, false);
    }

    fn parallel_queue() -> Queue {
        let mut inner =
            QueueInner { budget: BackgroundBudget::for_workers(4), ..Default::default() };
        inner.budget.observe(Some(0.1), false);
        inner.budget.observe(Some(0.1), false);
        Queue { inner: Mutex::new(inner), wake: Condvar::new() }
    }

    #[test]
    fn spare_capacity_admits_multiple_prefetches_and_promotes_the_correct_running_job() {
        let queue = parallel_queue();
        for seed in 1..=3 {
            assert!(queue.try_prefetch(thumbnail_job(seed), 5));
        }
        assert!(!queue.try_prefetch(thumbnail_job(4), 5));
        let first = queue.inner.lock().unwrap().take_ready().unwrap();
        let second = queue.inner.lock().unwrap().take_ready().unwrap();
        let third = queue.inner.lock().unwrap().take_ready().unwrap();
        assert_eq!(queue.inner.lock().unwrap().active_background.len(), 3);
        queue.push(thumbnail_job(2));
        assert_eq!(queue.inner.lock().unwrap().background_capacity(), 0);
        assert!(!queue.finish(&third).promoted);
        assert!(!queue.finish(&first).promoted);
        assert!(queue.finish(&second).promoted);
    }

    #[test]
    fn shrinking_the_budget_waits_for_running_jobs_and_keeps_queued_work() {
        let queue = parallel_queue();
        for seed in 1..=3 {
            assert!(queue.try_prefetch(thumbnail_job(seed), 5));
        }
        let mut running = Vec::new();
        for _ in 0..3 {
            running.push(queue.inner.lock().unwrap().take_ready().unwrap());
        }
        {
            let mut inner = queue.inner.lock().unwrap();
            inner.defer(thumbnail_job(4), 5);
            inner.budget.observe(Some(0.95), false);
            assert!(inner.take_ready().is_none());
        }
        for work in &running[..2] {
            queue.finish(work);
            assert!(queue.inner.lock().unwrap().take_ready().is_none());
        }
        queue.finish(&running[2]);
        let next = queue.inner.lock().unwrap().take_ready().unwrap();
        assert_eq!(next.job.path(), Path::new("a4"));
    }

    #[test]
    fn foreground_work_can_start_while_background_slots_are_full() {
        let queue = parallel_queue();
        for seed in 1..=3 {
            assert!(queue.try_prefetch(thumbnail_job(seed), 5));
            queue.inner.lock().unwrap().take_ready().unwrap();
        }
        queue.push(thumbnail_job(4));
        let visible = queue.inner.lock().unwrap().take_ready().unwrap();
        assert_eq!(visible.prefetch_epoch, None);
        assert_eq!(visible.job.path(), Path::new("a4"));
        assert_eq!(queue.inner.lock().unwrap().background_capacity(), 0);
    }

    #[test]
    fn cache_cleanup_also_leaves_a_worker_available_for_visible_requests() {
        let queue = parallel_queue();
        for seed in 0..32 {
            assert!(queue.try_prefetch(thumbnail_job(seed), 5));
            let work = queue.inner.lock().unwrap().take_ready().unwrap();
            assert_eq!(queue.finish(&work).sweep, seed == 31);
        }
        assert_eq!(queue.inner.lock().unwrap().background_capacity(), 2);
        queue.finish_sweep();
        assert_eq!(queue.inner.lock().unwrap().background_capacity(), 3);
    }

    #[test]
    fn a_large_queue_keeps_every_unique_request() {
        let queue = Queue { inner: Mutex::new(QueueInner::default()), wake: Condvar::new() };
        for i in 0..1200 {
            queue.push(thumbnail_job(i));
        }
        assert_eq!(queue.inner.lock().unwrap().pending.len(), 1200);
        let mut completed = 0;
        loop {
            let work = queue.inner.lock().unwrap().take_ready();
            let Some(work) = work else { break };
            completed += 1;
            queue.finish(&work);
        }
        assert_eq!(completed, 1200);
    }

    #[test]
    fn foreground_work_pauses_background_admission_until_the_next_load_sample() {
        let queue = Queue { inner: Mutex::new(QueueInner::default()), wake: Condvar::new() };
        assert!(queue.try_prefetch(thumbnail_job(0), 5));
        queue.push(thumbnail_job(1));
        let foreground = queue.inner.lock().unwrap().take_ready().unwrap();
        assert_eq!(foreground.prefetch_epoch, None);
        assert!(queue.inner.lock().unwrap().take_ready().is_none());
        queue.finish(&foreground);
        assert!(queue.inner.lock().unwrap().take_ready().is_none());
        resume_background(&queue);
        let background = queue.inner.lock().unwrap().take_ready().unwrap();
        assert_eq!(background.prefetch_epoch, Some(5));
        assert!(!queue.try_prefetch(thumbnail_job(2), 5));
        queue.push(thumbnail_job(3));
        let foreground = queue.inner.lock().unwrap().take_ready().unwrap();
        assert_eq!(foreground.prefetch_epoch, None);
        queue.finish(&foreground);
        queue.finish(&background);
        resume_background(&queue);
        assert!(queue.try_prefetch(thumbnail_job(4), 5));
    }

    #[test]
    fn scrolling_defers_offscreen_requests_without_discarding_them() {
        let queue = Queue { inner: Mutex::new(QueueInner::default()), wake: Condvar::new() };
        queue.push(thumbnail_job(1));
        queue.push(thumbnail_job(2));
        let visible = std::collections::HashSet::from([PathBuf::from("a2")]);
        let deferred = queue.defer_outside(&visible, 128, 5);
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].path(), Path::new("a1"));
        let foreground = queue.inner.lock().unwrap().take_ready().unwrap();
        assert_eq!(foreground.job.path(), Path::new("a2"));
        queue.finish(&foreground);
        resume_background(&queue);
        let background = queue.inner.lock().unwrap().take_ready().unwrap();
        assert_eq!(background.job.path(), Path::new("a1"));
        assert_eq!(background.prefetch_epoch, Some(5));
    }

    #[test]
    fn a_visible_request_promotes_a_queued_prefetch_instead_of_duplicating_it() {
        let queue = Queue { inner: Mutex::new(QueueInner::default()), wake: Condvar::new() };
        assert!(queue.try_prefetch(thumbnail_job(1), 5));
        queue.push(thumbnail_job(1));
        assert!(queue.inner.lock().unwrap().background.is_empty());
        assert!(queue.inner.lock().unwrap().background_ids.is_empty());
        let foreground = queue.inner.lock().unwrap().take_ready().unwrap();
        assert_eq!(foreground.prefetch_epoch, None);
        queue.finish(&foreground);
        assert!(queue.inner.lock().unwrap().idle());
    }

    #[test]
    fn prefetch_writes_the_cache_without_sending_pixels_to_the_ui() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("pictures.zip");
        let cache = temp.path().join("cache");
        write_zip_picture(&archive, [240, 10, 10, 255]);
        let backend = mandala_media::default_backend().unwrap();
        let work =
            Work { job: Job::Folder { path: archive.clone(), tier: 128 }, prefetch_epoch: Some(7) };
        let done = finish_result(&work, run(&work.job, &cache, &backend), false);
        assert_eq!(done.prefetch_epoch, Some(7));
        assert!(done.thumbnail.is_none());
        assert!(archive_cover_cache_path(&archive, 128, &cache).unwrap().exists());
        let visible = Work { job: work.job.clone(), prefetch_epoch: None };
        assert!(
            finish_result(&visible, run(&visible.job, &cache, &backend), false)
                .thumbnail
                .unwrap()
                .is_ok()
        );
        let promoted = finish_result(&work, run(&work.job, &cache, &backend), true);
        assert_eq!(promoted.prefetch_epoch, None);
        assert!(promoted.thumbnail.unwrap().is_ok());
    }

    fn cached_archive_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("pictures.zip");
        // A valid cached cover must not need to parse the archive at all.
        std::fs::write(&archive, b"not a readable ZIP").unwrap();
        let cache = temp.path().join("cache");
        (temp, archive, cache)
    }

    fn seed_archive_cover(dir: &Path, tier: u32, cache: &Path) {
        let path = archive_cover_cache_path(dir, tier, cache).unwrap();
        let frame = Frame {
            width: tier,
            height: tier,
            rgba: [220, 30, 10, 255].repeat((tier * tier) as usize),
            timestamp: Duration::ZERO,
        };
        store_cached(&path, &frame).unwrap();
    }

    #[test]
    fn cached_zip_covers_do_not_parse_or_extract_the_archive() {
        let (_temp, archive, cache) = cached_archive_fixture();
        seed_archive_cover(&archive, 128, &cache);
        let backend = mandala_media::default_backend().unwrap();
        let frame = produce_folder(&archive, 128, &cache, &backend).unwrap();
        assert_eq!((frame.width, frame.height), (128, 128));
        assert!(frame.rgba[0] > 200);
    }

    #[test]
    fn cached_zip_subfolder_covers_skip_archive_scanning_too() {
        let (_temp, archive, cache) = cached_archive_fixture();
        let folder = archive.join("旅行");
        seed_archive_cover(&folder, 128, &cache);
        let backend = mandala_media::default_backend().unwrap();
        assert!(produce_folder(&folder, 128, &cache, &backend).is_ok());
        assert!(produce_folder(&archive.join("家族"), 128, &cache, &backend).is_err());
    }

    #[test]
    fn zip_cover_cache_tracks_archive_size_timestamp_and_tier() {
        let (_temp, archive, cache) = cached_archive_fixture();
        seed_archive_cover(&archive, 128, &cache);
        let backend = mandala_media::default_backend().unwrap();
        assert!(produce_folder(&archive, 256, &cache, &backend).is_err());
        let original = std::fs::metadata(&archive).unwrap().modified().unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&archive).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(original + Duration::from_secs(2)))
            .unwrap();
        assert!(produce_folder(&archive, 128, &cache, &backend).is_err());
        file.set_len(100).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(original)).unwrap();
        assert!(produce_folder(&archive, 128, &cache, &backend).is_err());
    }

    fn write_zip_picture(path: &Path, colour: [u8; 4]) {
        use std::io::{Cursor, Write};
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(8, 8, image::Rgba(colour)))
            .write_to(&mut bytes, image::ImageFormat::Bmp)
            .unwrap();
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        zip.start_file(
            "picture.bmp",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(bytes.get_ref()).unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn generated_zip_covers_are_cached_before_the_next_archive_scan() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("pictures.zip");
        let cache = temp.path().join("cache");
        write_zip_picture(&archive, [240, 10, 10, 255]);
        let backend = mandala_media::default_backend().unwrap();
        produce_folder(&archive, 128, &cache, &backend).unwrap();
        let cached = archive_cover_cache_path(&archive, 128, &cache).unwrap();
        assert!(cached.exists());
        let metadata = archive.metadata().unwrap();
        // Preserve the fingerprint but make parsing impossible, to prove the
        // next request does not read the index or extract a source image.
        std::fs::write(&archive, vec![0; metadata.len() as usize]).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
            .unwrap();
        let cached_frame = produce_folder(&archive, 128, &cache, &backend).unwrap();
        assert!(cached_frame.rgba[0] > 200);
        // A corrupt cache must fall back rather than returning an empty tile.
        std::fs::write(cached, b"broken JPEG").unwrap();
        assert!(produce_folder(&archive, 128, &cache, &backend).is_err());
    }

    #[test]
    fn changing_a_zip_regenerates_its_cover_even_at_the_same_file_size() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("pictures.zip");
        let cache = temp.path().join("cache");
        write_zip_picture(&archive, [240, 10, 10, 255]);
        let old = archive.metadata().unwrap();
        let backend = mandala_media::default_backend().unwrap();
        let red = produce_folder(&archive, 128, &cache, &backend).unwrap();
        assert!(red.rgba[0] > 200 && red.rgba[2] < 30);
        write_zip_picture(&archive, [10, 10, 240, 255]);
        assert_eq!(old.len(), archive.metadata().unwrap().len());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(old.modified().unwrap() + Duration::from_secs(2)),
            )
            .unwrap();
        let blue = produce_folder(&archive, 128, &cache, &backend).unwrap();
        assert!(blue.rgba[2] > 200 && blue.rgba[0] < 30);
        let larger = produce_folder(&archive, 256, &cache, &backend).unwrap();
        assert_eq!((larger.width, larger.height), (256, 256));
    }

    #[test]
    fn ordinary_folder_covers_still_notice_changes_inside_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("pictures");
        std::fs::create_dir(&folder).unwrap();
        let picture = folder.join("picture.bmp");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([240, 10, 10, 255])).save(&picture).unwrap();
        let folder_time = folder.metadata().unwrap().modified().unwrap();
        let picture_time = picture.metadata().unwrap().modified().unwrap();
        let cache = temp.path().join("cache");
        let backend = mandala_media::default_backend().unwrap();
        let red = produce_folder(&folder, 128, &cache, &backend).unwrap();
        assert!(red.rgba[0] > 200 && red.rgba[2] < 30);
        image::RgbaImage::from_pixel(8, 8, image::Rgba([10, 10, 240, 255])).save(&picture).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&picture)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(picture_time + Duration::from_secs(2)),
            )
            .unwrap();
        assert_eq!(folder_time, folder.metadata().unwrap().modified().unwrap());
        let blue = produce_folder(&folder, 128, &cache, &backend).unwrap();
        assert!(blue.rgba[2] > 200 && blue.rgba[0] < 30);
    }

    fn key(seed: i128) -> CacheKey {
        CacheKey::new(Path::new("a"), seed, 0, 128)
    }

    fn thumbnail_job(seed: i128) -> Job {
        Job::Thumbnail {
            path: PathBuf::from(format!("a{seed}")),
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
            loaded.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255),
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
        let queue = Queue { inner: Mutex::new(QueueInner::default()), wake: Condvar::new() };
        for i in 0..3i128 {
            queue.push(thumbnail_job(i));
        }
        // The last one pushed is the one on screen now.
        match queue.pop().unwrap().job {
            Job::Thumbnail { key: got, .. } => assert_eq!(got, key(2)),
            other => panic!("expected a thumbnail job, got {other:?}"),
        }
    }

    #[test]
    fn repeated_requests_reorder_a_single_job_without_growing_the_queue() {
        let queue = Queue { inner: Mutex::new(QueueInner::default()), wake: Condvar::new() };
        for _ in 0..1200 {
            queue.push(thumbnail_job(1));
        }
        assert_eq!(queue.inner.lock().unwrap().pending.len(), 1);
    }

    #[test]
    fn a_visible_request_reuses_an_already_running_background_decode() {
        let queue = Queue { inner: Mutex::new(QueueInner::default()), wake: Condvar::new() };
        assert!(queue.try_prefetch(thumbnail_job(1), 5));
        let work = queue.inner.lock().unwrap().take_ready().unwrap();
        assert_eq!(work.prefetch_epoch, Some(5));
        queue.push(thumbnail_job(1));
        assert!(queue.inner.lock().unwrap().pending.is_empty());
        assert!(queue.finish(&work).promoted);
        assert!(queue.inner.lock().unwrap().idle());
    }

    #[test]
    fn repeated_scrolls_keep_deferred_work_in_fifo_order() {
        let queue = Queue { inner: Mutex::new(QueueInner::default()), wake: Condvar::new() };
        queue.push(thumbnail_job(1));
        queue.defer_outside(&Default::default(), 128, 5);
        queue.push(thumbnail_job(2));
        queue.defer_outside(&Default::default(), 128, 5);
        queue.push(thumbnail_job(3));
        queue.defer_outside(&Default::default(), 128, 5);
        resume_background(&queue);
        for name in ["a1", "a2", "a3"] {
            let work = queue.inner.lock().unwrap().take_ready().unwrap();
            assert_eq!(work.job.path(), Path::new(name));
            assert!(work.prefetch_epoch.is_some());
            queue.finish(&work);
        }
        assert!(queue.inner.lock().unwrap().idle());
        assert!(queue.inner.lock().unwrap().background_ids.is_empty());
    }

    fn test_service() -> ThumbnailService {
        ThumbnailService {
            queue: Arc::new(Queue {
                inner: Mutex::new(QueueInner::default()),
                wake: Condvar::new(),
            }),
            results: unbounded().1,
            workers: Vec::new(),
            cpu: CpuMonitor::default(),
        }
    }

    #[test]
    fn dropping_the_thumbnail_service_does_not_wait_for_blocked_external_io() {
        let mut service = test_service();
        let (release, blocked) = crossbeam_channel::bounded::<()>(1);
        service.workers.push(std::thread::spawn(move || {
            let _ = blocked.recv();
        }));
        let (finished, done) = crossbeam_channel::bounded(1);
        let owner = std::thread::spawn(move || {
            drop(service);
            let _ = finished.send(());
        });
        let returned = done.recv_timeout(Duration::from_secs(2)).is_ok();
        // Always release the fake I/O, including when testing a broken Drop.
        let _ = release.send(());
        owner.join().unwrap();
        assert!(returned, "closing the app must not wait for a media/network read");
    }

    #[test]
    fn resorting_keeps_deferred_work_but_changing_tier_cancels_obsolete_sizes() {
        let service = test_service();
        service.request(thumbnail_job(1));
        service.defer_outside(&Default::default(), 128, 5);
        service.rebase_background(6, 128);
        assert_eq!(service.queue.inner.lock().unwrap().background.len(), 1);
        assert_eq!(
            service
                .queue
                .inner
                .lock()
                .unwrap()
                .background
                .first_key_value()
                .unwrap()
                .1
                .prefetch_epoch,
            Some(6)
        );
        service.rebase_background(7, 256);
        assert!(service.queue.inner.lock().unwrap().background.is_empty());
        assert!(service.queue.inner.lock().unwrap().background_ids.is_empty());
    }

    #[test]
    fn a_reload_does_not_reuse_a_decode_from_the_old_context() {
        let service = test_service();
        assert!(service.prefetch(thumbnail_job(1), 5));
        let work = service.queue.inner.lock().unwrap().take_ready().unwrap();
        service.clear_pending();
        service.rebase_background(6, 128);
        service.request(thumbnail_job(1));
        assert_eq!(service.queue.inner.lock().unwrap().pending.len(), 1);
        assert!(!service.queue.finish(&work).promoted);
    }

    #[test]
    fn a_shut_down_queue_stops_handing_out_work() {
        let queue = Queue { inner: Mutex::new(QueueInner::default()), wake: Condvar::new() };
        queue.push(thumbnail_job(0));
        queue.shutdown();
        assert!(queue.pop().is_none());
    }
}
