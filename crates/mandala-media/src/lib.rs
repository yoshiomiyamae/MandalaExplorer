//! Decoding of images and video for mandala.
//!
//! Everything platform-specific sits behind [`MediaBackend`]. Today the only
//! implementation is Media Foundation, but keeping the seam here means an
//! FFmpeg backend can be dropped in without the UI noticing.

#[cfg(windows)]
mod com;

pub mod backend;
pub mod frame;
pub mod mosaic;
pub mod sizing;
pub mod still;

#[cfg(windows)]
pub mod mf;

#[cfg(windows)]
pub mod wic;

#[cfg(windows)]
pub use mf::MediaFoundation;

/// The decoding backend for this platform.
///
/// The point of the [`MediaBackend`] seam is that callers never name an
/// implementation, so the choice of one belongs here rather than in whoever
/// happens to construct it.
#[cfg(windows)]
pub fn default_backend() -> anyhow::Result<impl MediaBackend + Copy> {
    MediaFoundation::new()
}

pub use backend::{Advance, MediaBackend, VideoStream, VideoThumbnail};
pub use frame::Frame;
pub use sizing::fit_within;
