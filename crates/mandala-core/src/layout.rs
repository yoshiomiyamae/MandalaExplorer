use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileSize {
    pub w: f32,
    pub h: f32,
}

impl TileSize {
    pub fn square(side: f32) -> Self {
        Self { w: side, h: side }
    }
}

/// Placement math for a uniform grid.
///
/// Coordinates are derived from an index rather than by walking entries, so a
/// folder with thousands of files only ever materializes the visible slice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridLayout {
    tile: TileSize,
    gap: f32,
    padding: f32,
    columns: usize,
}

impl GridLayout {
    pub fn new(viewport_width: f32, tile: TileSize, gap: f32, padding: f32) -> Self {
        let usable = viewport_width - padding * 2.0;
        let stride = tile.w + gap;
        // n tiles need n*tile.w + (n-1)*gap of width, which solves to
        // n = (usable + gap) / stride. A viewport narrower than a single tile
        // still gets one column, or the divisions by `columns` below would be
        // divisions by zero.
        let columns = if stride > 0.0 { ((usable + gap) / stride).floor() as usize } else { 1 };
        Self { tile, gap, padding, columns: columns.max(1) }
    }

    /// Like [`GridLayout::new`], but widens the tiles so they divide the
    /// viewport evenly.
    ///
    /// With fixed-size tiles the remainder shows up as a dead strip down the
    /// right-hand side, which at large tile sizes can be most of a column wide.
    /// `target` becomes a minimum rather than an exact size.
    pub fn stretched(viewport_width: f32, target: TileSize, gap: f32, padding: f32) -> Self {
        let base = Self::new(viewport_width, target, gap, padding);
        let columns = base.columns as f32;
        let usable = (viewport_width - padding * 2.0).max(1.0);
        let width = ((usable - gap * (columns - 1.0)) / columns).max(1.0);
        // Scale height with width so tiles keep the aspect ratio asked for.
        let scale = if target.w > 0.0 { width / target.w } else { 1.0 };
        Self { tile: TileSize { w: width, h: target.h * scale }, ..base }
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn tile(&self) -> TileSize {
        self.tile
    }

    pub fn rows(&self, count: usize) -> usize {
        count.div_ceil(self.columns)
    }

    /// Total height of the scrollable content.
    pub fn content_height(&self, count: usize) -> f32 {
        let rows = self.rows(count);
        if rows == 0 {
            return 0.0;
        }
        self.padding * 2.0 + rows as f32 * self.tile.h + (rows - 1) as f32 * self.gap
    }

    /// Top-left corner of the tile at `index`.
    pub fn tile_origin(&self, index: usize) -> (f32, f32) {
        let col = index % self.columns;
        let row = index / self.columns;
        (
            self.padding + col as f32 * (self.tile.w + self.gap),
            self.padding + row as f32 * (self.tile.h + self.gap),
        )
    }

    /// Center of the tile at `index` on the content's y axis. This is what the
    /// playback scheduler ranks tiles by.
    pub fn tile_center_y(&self, index: usize) -> f32 {
        self.tile_origin(index).1 + self.tile.h / 2.0
    }

    /// Index range that should be drawn. `overscan_rows` adds rows of
    /// look-ahead on each side so scrolling does not reveal empty tiles.
    pub fn visible_indices(
        &self,
        scroll_y: f32,
        viewport_height: f32,
        count: usize,
        overscan_rows: usize,
    ) -> Range<usize> {
        let total_rows = self.rows(count);
        if total_rows == 0 {
            return 0..0;
        }
        let stride = self.tile.h + self.gap;
        if stride <= 0.0 {
            return 0..count;
        }

        // A row is visible when its bottom edge is past the top of the viewport
        // and its top edge is before the bottom; those two conditions invert
        // into this pair of row bounds.
        let first_row = ((scroll_y - self.padding - self.tile.h) / stride).ceil();
        let last_row = ((scroll_y + viewport_height - self.padding) / stride).floor();

        // Casting a negative float to usize saturates at 0, so scrolled-above
        // and scrolled-past both land on bounds the emptiness check catches.
        let first_row = (first_row.max(0.0) as usize).saturating_sub(overscan_rows);
        let last_row = (last_row.max(0.0) as usize).saturating_add(overscan_rows);
        if first_row >= total_rows || last_row < first_row {
            return 0..0;
        }
        let last_row = last_row.min(total_rows - 1);

        let start = first_row * self.columns;
        let end = ((last_row + 1) * self.columns).min(count);
        if start >= end { 0..0 } else { start..end }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> GridLayout {
        // 1000 wide minus 10 padding each side leaves 980; a 200 tile plus a
        // 10 gap fits 4 times.
        GridLayout::new(1000.0, TileSize::square(200.0), 10.0, 10.0)
    }

    #[test]
    fn computes_column_count_from_viewport_width() {
        assert_eq!(layout().columns(), 4);
    }

    #[test]
    fn always_keeps_at_least_one_column() {
        // A viewport narrower than one tile must still report a column, or
        // every downstream division by `columns` blows up.
        let narrow = GridLayout::new(50.0, TileSize::square(200.0), 10.0, 10.0);
        assert_eq!(narrow.columns(), 1);
    }

    #[test]
    fn rows_round_up() {
        let l = layout();
        assert_eq!(l.rows(0), 0);
        assert_eq!(l.rows(1), 1);
        assert_eq!(l.rows(4), 1);
        assert_eq!(l.rows(5), 2);
        assert_eq!(l.rows(10), 3);
    }

    #[test]
    fn content_height_includes_padding_on_both_sides() {
        let l = layout();
        // 3 rows: 200*3 + 10*2 gaps + 10*2 padding = 640
        assert_eq!(l.content_height(10), 640.0);
        assert_eq!(l.content_height(0), 0.0);
    }

    #[test]
    fn tile_origin_walks_row_major() {
        let l = layout();
        assert_eq!(l.tile_origin(0), (10.0, 10.0));
        assert_eq!(l.tile_origin(1), (220.0, 10.0));
        assert_eq!(l.tile_origin(4), (10.0, 220.0));
        assert_eq!(l.tile_origin(5), (220.0, 220.0));
    }

    #[test]
    fn tile_center_sits_in_the_middle_of_the_tile() {
        assert_eq!(layout().tile_center_y(0), 110.0);
        assert_eq!(layout().tile_center_y(4), 320.0);
    }

    #[test]
    fn stretching_uses_the_whole_width() {
        // 4 columns of a 200 tile in 980 usable leaves 150 spare; stretched,
        // that goes into the tiles instead of the right margin.
        let l = GridLayout::stretched(1000.0, TileSize::square(200.0), 10.0, 10.0);
        assert_eq!(l.columns(), 4);
        assert_eq!(l.tile().w, 237.5);
        let right_edge = l.tile_origin(3).0 + l.tile().w;
        assert_eq!(right_edge, 990.0, "the last column should end at the padding");
    }

    #[test]
    fn stretching_keeps_the_requested_aspect_ratio() {
        let l = GridLayout::stretched(1000.0, TileSize { w: 200.0, h: 100.0 }, 10.0, 10.0);
        assert_eq!(l.tile().w, 237.5);
        assert_eq!(l.tile().h, 118.75);
    }

    #[test]
    fn stretching_a_viewport_narrower_than_one_tile_shrinks_it() {
        let l = GridLayout::stretched(120.0, TileSize::square(200.0), 10.0, 10.0);
        assert_eq!(l.columns(), 1);
        assert_eq!(l.tile().w, 100.0);
    }

    #[test]
    fn visible_range_covers_partially_visible_rows() {
        let l = layout();
        // Row 0 spans y=10..210, row 1 220..420, row 2 430..630.
        // A 0..450 viewport clips row 2, which still has to be drawn.
        assert_eq!(l.visible_indices(0.0, 450.0, 100, 0), 0..12);
    }

    #[test]
    fn visible_range_follows_scroll() {
        let l = layout();
        // Scrolled to 430, row 2 is at the top edge and rows 0-1 are gone.
        let r = l.visible_indices(430.0, 200.0, 100, 0);
        assert_eq!(r, 8..12);
    }

    #[test]
    fn visible_range_drops_a_row_once_it_scrolls_fully_past_the_top() {
        let l = layout();
        // At 215 the bottom of row 0 (210) is above the viewport.
        assert_eq!(l.visible_indices(215.0, 200.0, 100, 0), 4..8);
    }

    #[test]
    fn overscan_extends_range_in_both_directions() {
        let l = layout();
        let r = l.visible_indices(430.0, 200.0, 100, 1);
        assert_eq!(r, 4..16);
    }

    #[test]
    fn visible_range_is_clamped_to_count() {
        let l = layout();
        assert_eq!(l.visible_indices(0.0, 10_000.0, 6, 0), 0..6);
        assert_eq!(l.visible_indices(0.0, 450.0, 0, 2), 0..0);
    }

    #[test]
    fn visible_range_is_empty_when_scrolled_past_the_end() {
        let l = layout();
        let r = l.visible_indices(100_000.0, 200.0, 8, 0);
        assert!(r.is_empty(), "got {r:?}");
    }
}
