//! A decoded frame, in the layout the GPU wants.

use anyhow::{Result, bail};
use std::time::Duration;

/// One decoded frame as tightly packed RGBA8, ready to hand to the texture
/// uploader without further copying.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub timestamp: Duration,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The pixel payload is megabytes; printing it helps nobody.
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .field("timestamp", &self.timestamp)
            .finish()
    }
}

/// How a decoder laid out the frame it handed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Bytes per row, including any padding.
    pub stride: usize,
    /// Rows run bottom to top, which Media Foundation signals with a negative
    /// stride. A negative stride cannot be expressed as a Rust slice, so it is
    /// carried as a flag alongside the absolute value.
    pub bottom_up: bool,
    /// Red and blue arrive swapped, as they do in Media Foundation's RGB32.
    pub swap_rb: bool,
}

impl Frame {
    /// Builds a frame from a decoder's output buffer, normalising it to tightly
    /// packed, top-down RGBA.
    pub fn from_packed(
        src: &[u8],
        layout: Layout,
        width: u32,
        height: u32,
        timestamp: Duration,
    ) -> Result<Self> {
        Ok(Self { width, height, rgba: repack(src, layout, width, height)?, timestamp })
    }
}

/// Normalises a decoder buffer into tightly packed top-down RGBA: drops row
/// padding, flips row order if needed, and swaps red and blue if needed.
///
/// When the decoder was able to produce RGBA directly this is a straight row
/// copy, which is the whole reason for asking it to.
pub fn repack(src: &[u8], layout: Layout, width: u32, height: u32) -> Result<Vec<u8>> {
    let Layout { stride, bottom_up, swap_rb } = layout;
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return Ok(Vec::new());
    }
    let row_bytes = w * 4;
    if stride < row_bytes {
        bail!("stride {stride} is narrower than a {w}px row");
    }
    // The last row only needs `row_bytes`, not a full stride of padding; a
    // decoder is free to end the buffer right after the final pixel.
    let needed = stride * (h - 1) + row_bytes;
    if src.len() < needed {
        bail!("buffer of {} bytes is short of the {needed} needed", src.len());
    }

    let mut out = vec![0u8; row_bytes * h];
    for y in 0..h {
        let src_row = if bottom_up { h - 1 - y } else { y };
        let from = &src[src_row * stride..src_row * stride + row_bytes];
        let into = &mut out[y * row_bytes..(y + 1) * row_bytes];
        if swap_rb {
            for (px_in, px_out) in from.chunks_exact(4).zip(into.chunks_exact_mut(4)) {
                px_out[0] = px_in[2];
                px_out[1] = px_in[1];
                px_out[2] = px_in[0];
                px_out[3] = px_in[3];
            }
        } else {
            into.copy_from_slice(from);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two pixels of a known BGRA value, laid out as one row.
    fn bgra_row() -> Vec<u8> {
        vec![1, 2, 3, 4, 5, 6, 7, 8]
    }

    fn bgra(stride: usize) -> Layout {
        Layout { stride, bottom_up: false, swap_rb: true }
    }

    fn rgba(stride: usize) -> Layout {
        Layout { stride, bottom_up: false, swap_rb: false }
    }

    #[test]
    fn swaps_blue_and_red_and_keeps_alpha() {
        let got = repack(&bgra_row(), bgra(8), 2, 1).unwrap();
        assert_eq!(got, vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn already_rgba_data_passes_through_untouched() {
        // The fast path taken when the decoder can produce RGBA itself.
        let got = repack(&bgra_row(), rgba(8), 2, 1).unwrap();
        assert_eq!(got, bgra_row());
    }

    #[test]
    fn drops_row_padding() {
        // A 1px-wide image with a 12-byte stride: 4 bytes of pixel, 8 of slack.
        let src = vec![1, 2, 3, 4, 9, 9, 9, 9, 9, 9, 9, 9, 10, 20, 30, 40];
        let got = repack(&src, bgra(12), 1, 2).unwrap();
        assert_eq!(got, vec![3, 2, 1, 4, 30, 20, 10, 40]);
    }

    #[test]
    fn drops_row_padding_on_the_untouched_path_too() {
        let src = vec![1, 2, 3, 4, 9, 9, 9, 9, 10, 20, 30, 40, 9, 9, 9, 9];
        let got = repack(&src, rgba(8), 1, 2).unwrap();
        assert_eq!(got, vec![1, 2, 3, 4, 10, 20, 30, 40]);
    }

    #[test]
    fn flips_row_order_when_bottom_up() {
        let src = vec![1, 2, 3, 4, 10, 20, 30, 40];
        let top_down = repack(&src, bgra(4), 1, 2).unwrap();
        let flipped = Layout { stride: 4, bottom_up: true, swap_rb: true };
        let bottom_up = repack(&src, flipped, 1, 2).unwrap();
        assert_eq!(top_down, vec![3, 2, 1, 4, 30, 20, 10, 40]);
        assert_eq!(bottom_up, vec![30, 20, 10, 40, 3, 2, 1, 4]);
    }

    #[test]
    fn accepts_a_buffer_that_ends_after_the_last_pixel() {
        // Padding on the final row is optional, so 12+4 bytes is enough for a
        // 1px-wide, 2-row image with a 12-byte stride.
        let src = vec![0; 16];
        assert!(repack(&src, bgra(12), 1, 2).is_ok());
        assert!(repack(&src[..15], bgra(12), 1, 2).is_err());
    }

    #[test]
    fn rejects_a_stride_narrower_than_the_row() {
        assert!(repack(&bgra_row(), bgra(4), 2, 1).is_err());
    }

    #[test]
    fn empty_dimensions_produce_no_pixels() {
        assert!(repack(&[], bgra(0), 0, 0).unwrap().is_empty());
    }

    #[test]
    fn frame_carries_its_dimensions_and_timestamp() {
        let f = Frame::from_packed(&bgra_row(), bgra(8), 2, 1, Duration::from_millis(500))
            .unwrap();
        assert_eq!((f.width, f.height), (2, 1));
        assert_eq!(f.rgba.len(), 8);
        assert_eq!(f.timestamp, Duration::from_millis(500));
    }
}
