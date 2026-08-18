//! Decoding of images and video for mandala.
//!
//! Everything platform-specific sits behind [`MediaBackend`]. Today the only
//! implementation is Media Foundation, but keeping the seam here means an
//! FFmpeg backend can be dropped in without the UI noticing.

pub mod backend;
pub mod frame;
pub mod sizing;
pub mod still;

#[cfg(windows)]
pub mod mf;

#[cfg(windows)]
pub use mf::MediaFoundation;

pub use backend::{Advance, MediaBackend, VideoStream, VideoThumbnail};
pub use frame::Frame;
pub use sizing::fit_within;
