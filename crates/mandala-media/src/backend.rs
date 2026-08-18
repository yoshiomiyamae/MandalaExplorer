//! The seam between the UI and whatever actually decodes media.

use crate::frame::Frame;
use anyhow::Result;
use std::path::Path;
use std::time::Duration;

/// Result of asking a stream to catch up to a point in time.
#[derive(Debug)]
pub enum Advance {
    /// A newly decoded frame. Upload it to the tile's texture.
    Frame(Frame),
    /// The frame already on screen is still the correct one; nothing to do.
    Unchanged,
    /// Playback ran off the end. Call [`VideoStream::restart`] to loop.
    EndOfStream,
}

/// A video opened for inline playback in a tile.
///
/// Tiles pull frames by presentation time rather than being pushed to, so a
/// tile that falls behind skips ahead instead of accumulating a backlog. With a
/// dozen videos playing at once, that difference is what keeps the grid from
/// drifting out of sync with itself. Skipped frames are never converted to
/// RGBA, so falling behind costs almost nothing.
pub trait VideoStream: Send {
    /// Decodes forward until reaching a frame at or after `target`.
    fn advance_to(&mut self, target: Duration) -> Result<Advance>;

    /// Total length, when the container reports one.
    fn duration(&self) -> Option<Duration>;

    /// Jumps to a position; playback carries on from there.
    ///
    /// Seeking lands on the nearest keyframe at or before the request, so the
    /// frame that comes back may be a little earlier than asked for.
    fn seek(&mut self, position: Duration) -> Result<()>;

    /// Seeks back to the start, for looping playback.
    fn restart(&mut self) -> Result<()> {
        self.seek(Duration::ZERO)
    }

    /// Size frames are decoded at, which is at most the requested bound.
    fn size(&self) -> (u32, u32);
}

/// A poster frame together with what else was learned while opening the file.
pub struct VideoThumbnail {
    pub frame: Frame,
    /// Running time, when the container reports one.
    pub duration: Option<Duration>,
}

pub trait MediaBackend: Send + Sync + 'static {
    /// Opens a video, decoding no larger than `max` since a tile cannot show
    /// more pixels than that anyway.
    fn open_video(&self, path: &Path, max: (u32, u32)) -> Result<Box<dyn VideoStream>>;

    /// Grabs a single representative frame for a still thumbnail.
    ///
    /// The running time comes back with it because opening the file is the
    /// expensive part, and having it means sorting by length later costs
    /// nothing for anything already thumbnailed.
    fn video_thumbnail(&self, path: &Path, max: (u32, u32)) -> Result<VideoThumbnail>;

    /// Reads just the running time, without decoding a frame.
    ///
    /// Used when sorting by length needs to know about files that are not on
    /// screen and so have never been thumbnailed.
    fn probe_duration(&self, path: &Path) -> Result<Option<Duration>>;
}

/// Where in a video to grab the poster frame.
///
/// The opening frames are often a black or blank lead-in, so sampling a little
/// way in gives a far more useful thumbnail.
pub const THUMBNAIL_POSITION: f64 = 0.10;

/// Poster-frame timestamp for a video of the given length.
pub fn thumbnail_timestamp(duration: Option<Duration>) -> Duration {
    match duration {
        Some(d) => d.mul_f64(THUMBNAIL_POSITION),
        None => Duration::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poster_frame_is_taken_shortly_after_the_start() {
        assert_eq!(thumbnail_timestamp(Some(Duration::from_secs(100))), Duration::from_secs(10));
    }

    #[test]
    fn poster_frame_falls_back_to_the_start_for_unknown_length() {
        assert_eq!(thumbnail_timestamp(None), Duration::ZERO);
    }
}
