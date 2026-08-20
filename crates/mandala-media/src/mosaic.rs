//! Building one tile out of several pictures.

use crate::frame::Frame;
use anyhow::{Result, bail};
use std::time::Duration;

/// The most pictures a tile is built from.
pub const MAX_CELLS: usize = 4;

/// Space left between cells, as a fraction of the tile. Small enough to read as
/// a seam rather than a border.
const GAP_FRACTION: u32 = 64;

/// Lays pictures out as one square tile of `size` pixels.
///
/// One fills the square; two take a side each; three put one down the left and
/// stack the others on the right; four take a corner each. No arrangement
/// leaves a cell empty, which is both better looking and one less thing to
/// think about: the tile is stored as JPEG like every other thumbnail, and
/// JPEG has nowhere to put a transparent corner. What is left between cells is
/// a seam, which earns its place when two photographs of the same colour sit
/// side by side.
///
/// Every picture is cropped to its cell rather than squashed into it: a
/// portrait photograph stretched to a square is unrecognisable, while its
/// middle is exactly what a thumbnail is for.
pub fn mosaic(frames: &[Frame], size: u32) -> Result<Frame> {
    if frames.is_empty() {
        bail!("a mosaic needs at least one picture");
    }
    if frames.len() > MAX_CELLS {
        bail!("a mosaic holds at most {MAX_CELLS} pictures, not {}", frames.len());
    }
    if size == 0 {
        bail!("a mosaic cannot be zero pixels across");
    }

    // Opaque black rather than transparent: what shows through the seams has
    // to survive being cached as a JPEG, and transparency does not -- it comes
    // back white, which on a dark tile reads as damage rather than as a seam.
    let mut rgba = [0u8, 0, 0, 255].repeat((size * size) as usize);
    let gap = (size / GAP_FRACTION).max(1);

    for (index, frame) in frames.iter().enumerate() {
        let cell = cell_rect(frames.len(), index, size, gap);
        draw_cropped(frame, &mut rgba, size, cell);
    }

    Ok(Frame { width: size, height: size, rgba, timestamp: Duration::ZERO })
}

/// Where one cell sits in a tile of `size`, as (x, y, width, height).
fn cell_rect(count: usize, index: usize, size: u32, gap: u32) -> (u32, u32, u32, u32) {
    match count {
        1 => (0, 0, size, size),
        2 => {
            let w = (size - gap) / 2;
            (if index == 0 { 0 } else { size - w }, 0, w, size)
        }
        3 => {
            let half = (size - gap) / 2;
            let quarter = (size - gap) / 2;
            match index {
                0 => (0, 0, half, size),
                1 => (size - half, 0, half, quarter),
                _ => (size - half, size - quarter, half, quarter),
            }
        }
        _ => {
            let half = (size - gap) / 2;
            let (col, row) = (index % 2, index / 2);
            let x = if col == 0 { 0 } else { size - half };
            let y = if row == 0 { 0 } else { size - half };
            (x, y, half, half)
        }
    }
}

/// Draws `frame` into a rectangle of `into`, cropped to the middle.
fn draw_cropped(frame: &Frame, into: &mut [u8], stride_px: u32, cell: (u32, u32, u32, u32)) {
    let (cx, cy, cw, ch) = cell;
    if cw == 0 || ch == 0 || frame.width == 0 || frame.height == 0 {
        return;
    }

    // The largest region of the source with the cell's shape, centred. Done in
    // u64 so a wide source at a large tile size cannot overflow the products.
    let (sw, sh) = (frame.width as u64, frame.height as u64);
    let (cw64, ch64) = (cw as u64, ch as u64);
    let (crop_w, crop_h) = if sw * ch64 > sh * cw64 {
        ((sh * cw64 / ch64).max(1), sh)
    } else {
        (sw, (sw * ch64 / cw64).max(1))
    };
    let (left, top) = ((sw - crop_w) / 2, (sh - crop_h) / 2);

    for y in 0..ch {
        // Nearest neighbour: the source has already been decoded at roughly the
        // right size, so this is a small correction rather than a real resize.
        let sy = top + (y as u64 * crop_h) / ch64;
        for x in 0..cw {
            let sx = left + (x as u64 * crop_w) / cw64;
            let from = ((sy * sw + sx) * 4) as usize;
            let to = (((cy + y) as u64 * stride_px as u64 + (cx + x) as u64) * 4) as usize;
            into[to..to + 4].copy_from_slice(&frame.rgba[from..from + 4]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, colour: [u8; 4]) -> Frame {
        Frame {
            width: w,
            height: h,
            rgba: colour.repeat((w * h) as usize),
            timestamp: Duration::ZERO,
        }
    }

    fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * frame.width + x) * 4) as usize;
        frame.rgba[i..i + 4].try_into().unwrap()
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const YELLOW: [u8; 4] = [255, 255, 0, 255];

    #[test]
    fn one_picture_fills_the_tile() {
        let out = mosaic(&[solid(10, 10, RED)], 64).unwrap();
        assert_eq!((out.width, out.height), (64, 64));
        assert_eq!(pixel(&out, 4, 4), RED);
        assert_eq!(pixel(&out, 59, 59), RED);
    }

    #[test]
    fn two_pictures_take_a_side_each() {
        let out = mosaic(&[solid(8, 8, RED), solid(8, 8, BLUE)], 64).unwrap();
        assert_eq!(pixel(&out, 8, 32), RED, "left");
        assert_eq!(pixel(&out, 55, 32), BLUE, "right");
    }

    #[test]
    fn four_pictures_take_a_corner_each() {
        let frames = [solid(8, 8, RED), solid(8, 8, GREEN), solid(8, 8, BLUE), solid(8, 8, YELLOW)];
        let out = mosaic(&frames, 64).unwrap();
        assert_eq!(pixel(&out, 8, 8), RED, "top left");
        assert_eq!(pixel(&out, 55, 8), GREEN, "top right");
        assert_eq!(pixel(&out, 8, 55), BLUE, "bottom left");
        assert_eq!(pixel(&out, 55, 55), YELLOW, "bottom right");
    }

    #[test]
    fn three_pictures_go_one_down_the_side_and_two_stacked() {
        let frames = [solid(8, 8, RED), solid(8, 8, GREEN), solid(8, 8, BLUE)];
        let out = mosaic(&frames, 64).unwrap();
        assert_eq!(pixel(&out, 8, 8), RED, "left, top to bottom");
        assert_eq!(pixel(&out, 8, 55), RED);
        assert_eq!(pixel(&out, 55, 8), GREEN, "right top");
        assert_eq!(pixel(&out, 55, 55), BLUE, "right bottom");
    }

    #[test]
    fn every_pixel_is_opaque_whatever_the_arrangement() {
        // Transparency survives the mosaic but not the JPEG it is cached as,
        // where it comes back white -- so a tile that looked right in memory
        // would grow white seams on the next run and nowhere else.
        for count in 1..=MAX_CELLS {
            let frames: Vec<Frame> = (0..count).map(|_| solid(8, 8, RED)).collect();
            let out = mosaic(&frames, 64).unwrap();
            let clear = out.rgba.as_chunks::<4>().0.iter().filter(|p| p[3] == 0).count();
            assert_eq!(clear, 0, "{count} pictures left {clear} transparent pixels");
        }
    }

    #[test]
    fn a_wide_picture_is_cropped_rather_than_squashed() {
        // Green in the middle, red down both sides. Cropping to the centre
        // shows green; squashing to fit would drag the red in.
        let mut wide = solid(200, 100, RED);
        for y in 0..100u32 {
            for x in 50..150u32 {
                let i = ((y * 200 + x) * 4) as usize;
                wide.rgba[i..i + 4].copy_from_slice(&GREEN);
            }
        }
        let out = mosaic(&[wide], 64).unwrap();
        assert_eq!(pixel(&out, 2, 32), GREEN, "the left edge should be inside the crop");
        assert_eq!(pixel(&out, 61, 32), GREEN, "and so should the right");
    }

    #[test]
    fn the_tile_is_square_whatever_goes_into_it() {
        let out = mosaic(&[solid(300, 20, RED), solid(20, 300, BLUE)], 48).unwrap();
        assert_eq!((out.width, out.height), (48, 48));
    }

    #[test]
    fn nothing_to_show_is_an_error_rather_than_an_empty_tile() {
        // The caller knows it has no pictures before it asks; a blank tile
        // returned here would be drawn over the folder icon and look broken.
        assert!(mosaic(&[], 64).is_err());
    }

    #[test]
    fn more_than_four_pictures_is_an_error_rather_than_a_silent_truncation() {
        let frames: Vec<Frame> = (0..5).map(|_| solid(4, 4, RED)).collect();
        assert!(mosaic(&frames, 64).is_err());
    }
}
