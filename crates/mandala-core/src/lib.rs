//! Domain logic for mandala.
//!
//! Nothing here depends on the UI or on the platform. Grid virtualization and
//! playback scheduling are the two places where this app lives or dies on
//! performance, so they are kept as pure functions and pinned down by tests.

pub mod cache_key;
pub mod kind;
pub mod layout;
pub mod scan;
pub mod schedule;

pub use cache_key::CacheKey;
pub use kind::MediaKind;
pub use layout::{GridLayout, TileSize};
pub use scan::{Entry, scan_dir};
pub use schedule::{PlaybackCandidate, plan_playback};
