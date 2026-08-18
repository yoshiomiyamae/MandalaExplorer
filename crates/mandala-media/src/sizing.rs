//! Choosing the resolution to decode at.
//!
//! Tiles are small, so decoding a 4K video at full size would waste an order of
//! magnitude of bandwidth per playing tile. Every decode is asked to produce no
//! more pixels than the tile can show.

/// Largest size that fits inside `max` while preserving aspect ratio.
///
/// Never upscales: a small source stays small and is stretched by the GPU at
/// draw time instead, which costs nothing.
pub fn fit_within(src: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = src;
    let (mw, mh) = max;
    if sw == 0 || sh == 0 || mw == 0 || mh == 0 {
        return (1, 1);
    }
    if sw <= mw && sh <= mh {
        return (sw, sh);
    }

    let scale = (mw as f64 / sw as f64).min(mh as f64 / sh as f64);
    let w = (sw as f64 * scale).round().max(1.0) as u32;
    let h = (sh as f64 * scale).round().max(1.0) as u32;
    (w.min(mw), h.min(mh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinks_landscape_to_fit_the_width() {
        assert_eq!(fit_within((1920, 1080), (256, 256)), (256, 144));
    }

    #[test]
    fn shrinks_portrait_to_fit_the_height() {
        assert_eq!(fit_within((1080, 1920), (256, 256)), (144, 256));
    }

    #[test]
    fn never_upscales_a_small_source() {
        assert_eq!(fit_within((100, 60), (256, 256)), (100, 60));
        assert_eq!(fit_within((256, 256), (256, 256)), (256, 256));
    }

    #[test]
    fn respects_a_non_square_bounding_box() {
        assert_eq!(fit_within((1000, 1000), (400, 200)), (200, 200));
    }

    #[test]
    fn keeps_extreme_aspect_ratios_at_least_one_pixel_wide() {
        // A 10000x1 banner must not round its height down to zero.
        assert_eq!(fit_within((10_000, 1), (256, 256)), (256, 1));
    }

    #[test]
    fn degenerate_sizes_do_not_divide_by_zero() {
        assert_eq!(fit_within((0, 0), (256, 256)), (1, 1));
        assert_eq!(fit_within((100, 100), (0, 0)), (1, 1));
    }
}
