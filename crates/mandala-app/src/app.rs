//! The application shell: navigation, the tile grid, and the loop that keeps
//! thumbnails and playback pointed at whatever is on screen.

use crate::player::PlaybackService;
use crate::slots::plan_slots;
use crate::thumbs::{Job, ThumbnailService, thumbnail_tier};
use eframe::egui::{
    self, Align2, Color32, Context, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, TextureHandle,
    TextureOptions, Ui, Vec2,
};
use mandala_core::layout::{GridLayout, TileSize};
use mandala_core::schedule::{PlaybackCandidate, ScheduleParams, plan_playback};
use mandala_core::sort::{Sort, SortKey, SortOrder, sort_entries};
use mandala_core::{CacheKey, Entry, MediaKind, scan_dir};
use mandala_media::Frame;
use mandala_media::mf::MediaFoundation;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const GAP: f32 = 8.0;
const PADDING: f32 = 12.0;
/// Rows of tiles kept ready above and below the viewport.
const OVERSCAN_ROWS: usize = 2;
/// How far outside the visible range textures are kept before being dropped.
/// Wide enough that a flick of the wheel stays inside it, but every kept row of
/// large thumbnails is real memory.
const TEXTURE_KEEP_ROWS: usize = 6;
const LABEL_HEIGHT: f32 = 22.0;
/// Range the tile size slider covers. The top end is deliberately larger than
/// any sensible grid: at that size the point is to inspect two or three files
/// at once, not to browse.
pub const MIN_TILE_PX: f32 = 96.0;
pub const MAX_TILE_PX: f32 = 1600.0;
/// How still the view has to be before playback is allowed to start.
///
/// Opening a decoder for a tile that is about to scroll away wastes the open
/// and briefly floods Media Foundation with sources it will never read.
const SCROLL_SETTLE: Duration = Duration::from_millis(180);
/// Key the settings live under in eframe's own storage.
const SETTINGS_KEY: &str = "mandala.settings";
/// Height of the scrub strip along the bottom of a playing tile.
const SEEK_BAR_HEIGHT: f32 = 10.0;

/// Watches the scroll offset so expensive work can wait for it to stop moving.
#[derive(Default)]
struct ScrollSettle {
    last_offset: f32,
    /// When the offset last changed. `None` means it has never moved, which is
    /// the state at startup -- nothing should have to wait for that.
    moved_at: Option<Instant>,
}

impl ScrollSettle {
    /// Records the current offset and reports whether the view has been still
    /// long enough to start decoding.
    fn update(&mut self, offset: f32, now: Instant) -> bool {
        // A threshold rather than equality, so sub-pixel trackpad drift does
        // not hold playback off indefinitely.
        if (offset - self.last_offset).abs() > 0.5 {
            self.last_offset = offset;
            self.moved_at = Some(now);
            return false;
        }
        match self.moved_at {
            Some(moved) => now.duration_since(moved) >= SCROLL_SETTLE,
            None => true,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub tile_px: f32,
    pub autoplay: bool,
    pub budget: usize,
    pub show_labels: bool,
    pub sort: Sort,
}

impl Settings {
    /// Clamps values loaded from disk. A settings file edited by hand, or left
    /// over from a version with different limits, must not wedge the grid.
    fn sanitized(mut self) -> Self {
        if !self.tile_px.is_finite() {
            self.tile_px = Self::default().tile_px;
        }
        self.tile_px = self.tile_px.clamp(MIN_TILE_PX, MAX_TILE_PX);
        self.budget = self.budget.clamp(1, MAX_BUDGET);
        self
    }
}

/// Upper bound on simultaneous playback. Past this the slots spend more time
/// competing for decode capacity than showing anything.
pub const MAX_BUDGET: usize = 32;

impl Default for Settings {
    fn default() -> Self {
        Self {
            tile_px: 260.0,
            autoplay: true,
            budget: 12,
            show_labels: true,
            sort: Sort::default(),
        }
    }
}

pub struct MandalaApp {
    current_dir: PathBuf,
    entries: Vec<Entry>,
    /// Bumped on every navigation, so frames decoded for the previous folder
    /// are recognisable as stale and dropped.
    generation: u64,
    error: Option<String>,
    path_edit: String,

    settings: Settings,
    thumbs: ThumbnailService,
    player: PlaybackService,

    /// Still thumbnail per file. Keyed by path rather than by position, so
    /// re-sorting the folder does not throw away everything already loaded.
    thumb_textures: HashMap<PathBuf, TextureHandle>,
    /// Live video frame per tile, drawn in place of the still when present.
    /// Positional, because a playback slot is bound to a position.
    video_textures: HashMap<usize, TextureHandle>,
    /// Position of each file, for routing worker results back to a tile.
    path_to_tile: HashMap<PathBuf, usize>,
    /// Running times learned so far. A stored `None` means the file was asked
    /// about and has no duration, which is not the same as never having asked.
    durations: HashMap<PathBuf, Option<Duration>>,
    requested_thumbs: HashSet<PathBuf>,
    requested_durations: HashSet<PathBuf>,
    failed_thumbs: HashSet<PathBuf>,
    /// Tier the on-screen thumbnails were asked for at.
    current_tier: u32,
    /// Sort the entries are currently in, to notice when the setting changes.
    applied_sort: Sort,
    /// Set when a newly learned duration invalidates the current order.
    needs_resort: bool,

    /// Where each playing tile has got to, for drawing its seek bar.
    playback: HashMap<usize, PlaybackInfo>,

    hovered: Option<usize>,
    visible: std::ops::Range<usize>,
    settle: ScrollSettle,
}

#[derive(Clone, Copy)]
struct PlaybackInfo {
    position: Duration,
    duration: Option<Duration>,
}

impl MandalaApp {
    pub fn new(cc: &eframe::CreationContext<'_>, start: PathBuf) -> anyhow::Result<Self> {
        // Before anything draws, or the first frame is all boxes.
        crate::fonts::install_fallbacks(&cc.egui_ctx);

        let backend = MediaFoundation::new()?;

        // Workers wake the UI thread when they have something to show, rather
        // than the UI polling at a fixed rate and burning frames on nothing.
        let ctx = cc.egui_ctx.clone();
        let repaint = move || ctx.request_repaint();

        let thumbs = ThumbnailService::new(backend, repaint.clone());
        let mut player = PlaybackService::new(backend, repaint);

        let settings = cc
            .storage
            .and_then(|storage| eframe::get_value::<Settings>(storage, SETTINGS_KEY))
            .unwrap_or_default()
            .sanitized();
        player.resize(settings.budget);

        let applied_sort = settings.sort;
        let mut app = Self {
            current_dir: PathBuf::new(),
            entries: Vec::new(),
            generation: 0,
            error: None,
            path_edit: String::new(),
            settings,
            thumbs,
            player,
            thumb_textures: HashMap::new(),
            video_textures: HashMap::new(),
            path_to_tile: HashMap::new(),
            durations: HashMap::new(),
            requested_thumbs: HashSet::new(),
            requested_durations: HashSet::new(),
            failed_thumbs: HashSet::new(),
            current_tier: 0,
            applied_sort,
            needs_resort: false,
            playback: HashMap::new(),
            hovered: None,
            visible: 0..0,
            settle: ScrollSettle::default(),
        };
        app.navigate_to(start);
        Ok(app)
    }

    fn navigate_to(&mut self, path: PathBuf) {
        self.generation += 1;
        self.player.stop_all();
        self.thumb_textures.clear();
        self.video_textures.clear();
        self.playback.clear();
        self.requested_thumbs.clear();
        self.requested_durations.clear();
        self.failed_thumbs.clear();
        self.durations.clear();
        self.hovered = None;
        self.visible = 0..0;

        match scan_dir(&path) {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
                self.path_edit = path.display().to_string();
                self.current_dir = path;
                self.resort();
            }
            Err(e) => {
                self.error = Some(format!("{}: {e}", path.display()));
                self.entries.clear();
                self.path_to_tile.clear();
            }
        }
    }

    /// Puts the entries in the configured order and rebuilds the position map.
    ///
    /// Playback is tied to positions, so anything playing has to stop when
    /// positions move -- but only then. Sorting by length re-sorts every time
    /// another duration arrives, and most of those change nothing; tearing
    /// down playback for each would mean nothing ever gets to play while a
    /// folder is being probed.
    fn resort(&mut self) {
        let durations = &self.durations;
        sort_entries(&mut self.entries, self.settings.sort, |entry| {
            durations.get(&entry.path).copied().flatten()
        });

        if order_changed(&self.entries, &self.path_to_tile) {
            self.path_to_tile = self
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.path.clone(), index))
                .collect();

            self.player.stop_all();
            self.video_textures.clear();
            self.playback.clear();
        }
        self.applied_sort = self.settings.sort;
        self.needs_resort = false;
    }

    /// Cache key for an entry at the current thumbnail tier.
    fn thumbnail_key(&self, entry: &Entry) -> CacheKey {
        CacheKey::new(&entry.path, entry.mtime_unix_nanos(), entry.len, self.current_tier)
    }

    fn metadata_key(entry: &Entry) -> CacheKey {
        CacheKey::metadata(&entry.path, entry.mtime_unix_nanos(), entry.len)
    }

    /// Notices the tile size crossing into another thumbnail tier.
    ///
    /// The textures on screen stay up until their sharper replacements arrive;
    /// a grid that blanks while the size slider is dragged would be far worse
    /// than one that is briefly soft.
    fn retier_thumbnails(&mut self) {
        let tier = thumbnail_tier(self.settings.tile_px as u32);
        if tier == self.current_tier {
            return;
        }
        self.current_tier = tier;
        self.requested_thumbs.clear();
        self.failed_thumbs.clear();
    }

    fn parent_dir(&self) -> Option<PathBuf> {
        self.current_dir.parent().map(Path::to_path_buf)
    }

    fn activate(&mut self, index: usize) {
        let Some(entry) = self.entries.get(index) else { return };
        if entry.is_dir() {
            self.navigate_to(entry.path.clone());
        } else {
            // explorer.exe hands the file to whatever is registered for it.
            let _ = std::process::Command::new("explorer.exe").arg(&entry.path).spawn();
        }
    }

    fn collect_results(&mut self, ctx: &Context) {
        for done in self.thumbs.drain().collect::<Vec<_>>() {
            if let Some(duration) = done.duration {
                // A newly learned length can change where its tile belongs.
                let known = self.durations.insert(done.path.clone(), Some(duration));
                if known != Some(Some(duration)) && self.settings.sort.key == SortKey::Duration {
                    self.needs_resort = true;
                }
            } else if self.requested_durations.contains(&done.path) {
                // Asked and there is none; remembered so it is not asked again.
                self.durations.entry(done.path.clone()).or_insert(None);
            }

            match done.thumbnail {
                Some(Ok(frame)) => {
                    let name = done.path.to_string_lossy().into_owned();
                    let texture = upload(ctx, &name, &frame);
                    self.thumb_textures.insert(done.path, texture);
                }
                // Remembering the failure stops it being retried every frame.
                Some(Err(_)) => {
                    self.failed_thumbs.insert(done.path);
                }
                None => {}
            }
        }

        for decoded in self.player.drain().collect::<Vec<_>>() {
            if decoded.generation != self.generation {
                continue;
            }
            self.playback.insert(
                decoded.tile,
                PlaybackInfo { position: decoded.frame.timestamp, duration: decoded.duration },
            );
            match self.video_textures.get_mut(&decoded.tile) {
                Some(texture) => texture.set(to_image(&decoded.frame), TextureOptions::LINEAR),
                None => {
                    let texture = upload(ctx, &format!("video{}", decoded.tile), &decoded.frame);
                    self.video_textures.insert(decoded.tile, texture);
                }
            }
        }
    }

    /// Asks for the thumbnails of everything on screen that lacks one.
    fn request_visible_thumbnails(&mut self) {
        for index in self.visible.clone() {
            let Some(entry) = self.entries.get(index) else { continue };
            if !entry.kind.has_thumbnail()
                || self.thumb_textures.contains_key(&entry.path)
                || self.requested_thumbs.contains(&entry.path)
                || self.failed_thumbs.contains(&entry.path)
            {
                continue;
            }
            self.requested_thumbs.insert(entry.path.clone());
            self.thumbs.request(Job::Thumbnail {
                path: entry.path.clone(),
                kind: entry.kind,
                key: self.thumbnail_key(entry),
                meta_key: Self::metadata_key(entry),
                tier: self.current_tier,
            });
        }
    }

    /// Asks for the running times that sorting by length is waiting on.
    ///
    /// Only while that sort is selected: probing every video in a large folder
    /// is real work, and nothing else needs the answer. A few per frame keeps
    /// the queue from being flooded by one enormous folder.
    fn request_missing_durations(&mut self) {
        if self.settings.sort.key != SortKey::Duration {
            return;
        }
        const PER_FRAME: usize = 24;

        let mut jobs = Vec::new();
        for entry in &self.entries {
            if jobs.len() >= PER_FRAME {
                break;
            }
            if entry.kind != MediaKind::Video
                || self.durations.contains_key(&entry.path)
                || self.requested_durations.contains(&entry.path)
            {
                continue;
            }
            jobs.push((
                entry.path.clone(),
                Job::Duration { path: entry.path.clone(), meta_key: Self::metadata_key(entry) },
            ));
        }
        for (path, job) in jobs {
            self.requested_durations.insert(path);
            self.thumbs.request(job);
        }
    }

    /// Decides what should be playing and moves the slot pool to match.
    fn schedule_playback(&mut self, layout: &GridLayout, viewport_center_y: f32, settled: bool) {
        if self.player.slot_count() != self.settings.budget {
            self.player.resize(self.settings.budget);
        }

        let candidates: Vec<PlaybackCandidate> = self
            .visible
            .clone()
            .filter(|&i| self.entries.get(i).is_some_and(|e| e.kind.is_playable()))
            .map(|i| PlaybackCandidate { index: i, center_y: layout.tile_center_y(i) })
            .collect();

        // With autoplay off, only the tile under the cursor plays -- which is
        // still the single most useful thing the grid can do.
        let hovered =
            self.hovered.filter(|&i| self.entries.get(i).is_some_and(|e| e.kind.is_playable()));
        let wanted = if self.settings.autoplay {
            plan_playback(
                &candidates,
                viewport_center_y,
                hovered,
                &self.player.holding().into_iter().flatten().collect::<Vec<_>>(),
                ScheduleParams { budget: self.settings.budget, ..Default::default() },
            )
        } else {
            hovered.into_iter().collect()
        };

        let mut plan = plan_slots(&self.player.holding(), &wanted);
        if !settled {
            // Freeing a slot that scrolled away is still worth doing at once;
            // it is only opening new decoders that waits.
            plan.start.clear();
        }
        if plan.is_empty() {
            self.player.set_hover(hovered);
            return;
        }

        for &slot in &plan.stop {
            // The still thumbnail takes over again the moment a tile stops.
            if let Some(tile) = self.player.holding().get(slot).copied().flatten() {
                self.video_textures.remove(&tile);
            }
        }

        let entries = &self.entries;
        // Decode to the tile as drawn, not the size asked for on the slider.
        let decode_max = layout.tile().w.max(layout.tile().h).ceil() as u32;
        let generation = self.generation;
        self.player.apply(&plan, generation, hovered, |tile| {
            entries.get(tile).map(|e| (e.path.clone(), (decode_max, decode_max)))
        });
        self.player.set_hover(hovered);
    }

    /// Drops textures far enough off screen that scrolling back is unlikely.
    fn trim_textures(&mut self, layout: &GridLayout) {
        let margin = TEXTURE_KEEP_ROWS * layout.columns();
        let keep_start = self.visible.start.saturating_sub(margin);
        let keep_end = self.visible.end.saturating_add(margin);
        let positions = &self.path_to_tile;
        self.thumb_textures.retain(|path, _| {
            positions.get(path).is_some_and(|&i| i >= keep_start && i < keep_end)
        });
        // Video textures belong to playing tiles, which are visible by
        // definition; anything else is a leftover from a stopped slot.
        let playing: HashSet<usize> = self.player.holding().into_iter().flatten().collect();
        self.video_textures.retain(|i, _| playing.contains(i));
        self.playback.retain(|i, _| playing.contains(i));
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let parent = self.parent_dir();
                if ui.add_enabled(parent.is_some(), egui::Button::new("↑")).clicked()
                    && let Some(parent) = parent
                {
                    self.navigate_to(parent);
                }

                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.path_edit).desired_width(f32::INFINITY),
                );
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.navigate_to(PathBuf::from(self.path_edit.trim()));
                }
            });

            ui.horizontal(|ui| {
                ui.label("Size");
                ui.add(
                    egui::Slider::new(&mut self.settings.tile_px, MIN_TILE_PX..=MAX_TILE_PX)
                        .show_value(false)
                        .logarithmic(true),
                );
                ui.separator();

                ui.checkbox(&mut self.settings.autoplay, "Autoplay");
                ui.label("at once");
                ui.add(egui::Slider::new(&mut self.settings.budget, 1..=MAX_BUDGET));
                ui.separator();

                ui.label("Sort");
                egui::ComboBox::from_id_salt("sort-key")
                    .selected_text(self.settings.sort.key.label())
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        for key in SortKey::ALL {
                            ui.selectable_value(&mut self.settings.sort.key, key, key.label());
                        }
                    });
                let (arrow, tip) = match self.settings.sort.order {
                    SortOrder::Ascending => ("\u{2191}", "Ascending"),
                    SortOrder::Descending => ("\u{2193}", "Descending"),
                };
                if ui.button(arrow).on_hover_text(tip).clicked() {
                    self.settings.sort.order = self.settings.sort.order.flipped();
                }

                ui.separator();
                ui.checkbox(&mut self.settings.show_labels, "Names");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let videos = self.entries.iter().filter(|e| e.kind == MediaKind::Video).count();
                    ui.label(format!("{} items, {videos} video", self.entries.len()));
                });
            });
        });
    }

    fn image_area(&self, tile: Rect) -> Rect {
        let label_room = if self.settings.show_labels { LABEL_HEIGHT } else { 0.0 };
        Rect::from_min_max(tile.min + Vec2::splat(4.0), tile.max - Vec2::new(4.0, 4.0 + label_room))
    }

    fn draw_tile(&self, ui: &Ui, index: usize, rect: Rect, hovered: bool) {
        let Some(entry) = self.entries.get(index) else { return };
        let painter = ui.painter();
        let visuals = ui.visuals();

        let background =
            if hovered { visuals.widgets.hovered.bg_fill } else { visuals.extreme_bg_color };
        painter.rect_filled(rect, CornerRadius::same(6), background);

        let image_area = self.image_area(rect);

        let texture =
            self.video_textures.get(&index).or_else(|| self.thumb_textures.get(&entry.path));
        match texture {
            Some(texture) => {
                let fitted = fit_rect(image_area, texture.size_vec2());
                painter.image(
                    texture.id(),
                    fitted,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => draw_placeholder(painter, image_area, entry.kind, visuals),
        }

        // A playing tile gets a marker, so it is obvious which motion is live
        // rather than a lucky animated thumbnail.
        if self.video_textures.contains_key(&index) {
            let dot = Pos2::new(image_area.max.x - 10.0, image_area.min.y + 10.0);
            painter.circle_filled(dot, 4.0, Color32::from_rgb(120, 220, 140));
        } else if entry.kind == MediaKind::Video {
            draw_play_badge(painter, image_area);
        }

        // Only while pointing at it: a permanent bar over every tile would be
        // noise, and the bar is only actionable under the cursor anyway.
        if hovered
            && let Some(info) = self.playback.get(&index)
            && let Some(duration) = info.duration
            && duration > Duration::ZERO
        {
            draw_seek_bar(painter, seek_bar_rect(image_area), info.position, duration, visuals);
        }

        if self.settings.show_labels {
            let label_rect = Rect::from_min_max(
                Pos2::new(rect.min.x + 6.0, rect.max.y - LABEL_HEIGHT),
                Pos2::new(rect.max.x - 6.0, rect.max.y),
            );
            // Laid out rather than trimmed by character count: a full-width
            // Japanese character is about twice as wide as a Latin one, so
            // counting characters would either overflow the tile or cut names
            // far shorter than they need to be. egui measures and adds its own
            // ellipsis.
            let mut job = egui::text::LayoutJob::simple_singleline(
                entry.name.clone(),
                FontId::proportional(12.0),
                visuals.text_color(),
            );
            job.wrap.max_width = label_rect.width();
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            let galley = painter.layout_job(job);
            let baseline = label_rect.left_center() - Vec2::new(0.0, galley.size().y / 2.0);
            painter.galley(baseline, galley, visuals.text_color());
        }
    }
}

impl eframe::App for MandalaApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.collect_results(&ctx);

        // Ctrl held, or a trackpad pinch. Plain wheel has to stay as scrolling,
        // or there would be no way to move through a folder.
        let zoom = ctx.input(|i| i.zoom_delta());
        self.settings.tile_px = zoomed_tile_size(self.settings.tile_px, zoom);

        self.top_bar(ui);

        if ctx.input(|i| i.key_pressed(egui::Key::Backspace))
            && let Some(parent) = self.parent_dir()
        {
            self.navigate_to(parent);
        }

        let mut activated = None;
        let mut hovered_now = None;
        let mut seek_request = None;

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
                return;
            }

            let target = TileSize::square(self.settings.tile_px);
            let count = self.entries.len();

            egui::ScrollArea::vertical().auto_shrink([false; 2]).show_viewport(
                ui,
                |ui, viewport| {
                    // Measured inside the scroll area, where the width already has
                    // the scrollbar taken out of it.
                    let layout = GridLayout::stretched(ui.available_width(), target, GAP, PADDING);
                    let tile = layout.tile();
                    ui.set_height(layout.content_height(count));
                    let origin = ui.min_rect().min;

                    self.visible = layout.visible_indices(
                        viewport.min.y,
                        viewport.height(),
                        count,
                        OVERSCAN_ROWS,
                    );

                    for index in self.visible.clone() {
                        let (x, y) = layout.tile_origin(index);
                        let rect = Rect::from_min_size(
                            origin + Vec2::new(x, y),
                            Vec2::new(tile.w, tile.h),
                        );
                        let response =
                            ui.interact(rect, ui.id().with(("tile", index)), Sense::click());
                        let mut hovering = response.hovered();
                        if response.double_clicked() {
                            activated = Some(index);
                        }

                        // The scrub strip sits on top of the tile, so it is claimed
                        // after it and hands hover back to the tile -- otherwise
                        // pointing at the bar would stop the video playing.
                        if let Some(info) = self.playback.get(&index).copied()
                            && let Some(duration) = info.duration
                            && duration > Duration::ZERO
                        {
                            let bar = seek_bar_rect(self.image_area(rect));
                            let bar_response = ui.interact(
                                bar,
                                ui.id().with(("seek", index)),
                                Sense::click_and_drag(),
                            );
                            if bar_response.hovered() {
                                hovering = true;
                            }
                            if let Some(pointer) = bar_response.interact_pointer_pos() {
                                hovering = true;
                                seek_request =
                                    Some((index, seek_position(bar, pointer.x, duration)));
                            }
                        }

                        if hovering {
                            hovered_now = Some(index);
                        }
                        self.draw_tile(ui, index, rect, hovering);
                    }

                    let center = viewport.min.y + viewport.height() / 2.0;
                    let settled = self.settle.update(viewport.min.y, Instant::now());
                    self.hovered = hovered_now;
                    self.retier_thumbnails();
                    self.request_visible_thumbnails();
                    self.request_missing_durations();
                    self.schedule_playback(&layout, center, settled);
                    self.trim_textures(&layout);
                },
            );
        });

        // Re-sorting moves every tile, so it happens once the frame is drawn
        // rather than underneath the loop that is drawing it.
        if self.settings.sort != self.applied_sort || self.needs_resort {
            self.resort();
        }

        if let Some((index, position)) = seek_request {
            self.player.seek(index, position);
        }
        if let Some(index) = activated {
            self.activate(index);
        }
    }
}

/// Whether any entry sits somewhere other than where `positions` had it.
fn order_changed(entries: &[Entry], positions: &HashMap<PathBuf, usize>) -> bool {
    entries.len() != positions.len()
        || entries
            .iter()
            .enumerate()
            .any(|(index, entry)| positions.get(&entry.path) != Some(&index))
}

/// The strip along the bottom of a tile that scrubs playback.
fn seek_bar_rect(image_area: Rect) -> Rect {
    let height = SEEK_BAR_HEIGHT.min(image_area.height());
    Rect::from_min_max(Pos2::new(image_area.min.x, image_area.max.y - height), image_area.max)
}

/// Position in a clip for a pointer sitting at `x` over the bar.
fn seek_position(bar: Rect, x: f32, duration: Duration) -> Duration {
    let fraction = ((x - bar.min.x) / bar.width().max(1.0)).clamp(0.0, 1.0);
    duration.mul_f32(fraction)
}

fn draw_seek_bar(
    painter: &egui::Painter,
    bar: Rect,
    position: Duration,
    duration: Duration,
    visuals: &egui::Visuals,
) {
    painter.rect_filled(bar, CornerRadius::same(2), Color32::from_black_alpha(160));
    let fraction =
        (position.as_secs_f32() / duration.as_secs_f32().max(f32::EPSILON)).clamp(0.0, 1.0);
    let played =
        Rect::from_min_max(bar.min, Pos2::new(bar.min.x + bar.width() * fraction, bar.max.y));
    painter.rect_filled(played, CornerRadius::same(2), visuals.selection.bg_fill);
}

/// Applies a zoom gesture to the tile size, ignoring nonsense factors.
fn zoomed_tile_size(tile_px: f32, zoom: f32) -> f32 {
    if !zoom.is_finite() || zoom <= 0.0 || zoom == 1.0 {
        return tile_px;
    }
    (tile_px * zoom).clamp(MIN_TILE_PX, MAX_TILE_PX)
}

fn upload(ctx: &Context, name: &str, frame: &Frame) -> TextureHandle {
    ctx.load_texture(name, to_image(frame), TextureOptions::LINEAR)
}

fn to_image(frame: &Frame) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied(
        [frame.width as usize, frame.height as usize],
        &frame.rgba,
    )
}

/// Largest rect with the same aspect ratio as `size` that fits in `bounds`.
fn fit_rect(bounds: Rect, size: Vec2) -> Rect {
    if size.x <= 0.0 || size.y <= 0.0 || bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return Rect::from_center_size(bounds.center(), Vec2::ZERO);
    }
    let scale = (bounds.width() / size.x).min(bounds.height() / size.y);
    Rect::from_center_size(bounds.center(), size * scale)
}

fn draw_placeholder(painter: &egui::Painter, area: Rect, kind: MediaKind, visuals: &egui::Visuals) {
    let glyph = match kind {
        MediaKind::Directory => "🗀",
        MediaKind::Image => "🖼",
        MediaKind::Video => "🎞",
        MediaKind::Audio => "🎵",
        MediaKind::Other => "🗎",
    };
    painter.text(
        area.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::proportional((area.height() * 0.3).clamp(16.0, 64.0)),
        visuals.weak_text_color(),
    );
}

fn draw_play_badge(painter: &egui::Painter, area: Rect) {
    let center = Pos2::new(area.max.x - 16.0, area.min.y + 16.0);
    painter.circle_filled(center, 10.0, Color32::from_black_alpha(150));
    let r = 4.5;
    painter.add(egui::Shape::convex_polygon(
        vec![
            Pos2::new(center.x - r * 0.6, center.y - r),
            Pos2::new(center.x - r * 0.6, center.y + r),
            Pos2::new(center.x + r, center.y),
        ],
        Color32::WHITE,
        Stroke::NONE,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_preserves_aspect_ratio_within_the_bounds() {
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 100.0));
        let fitted = fit_rect(bounds, Vec2::new(400.0, 100.0));
        assert_eq!(fitted.width(), 200.0);
        assert_eq!(fitted.height(), 50.0);
        assert_eq!(fitted.center(), bounds.center(), "fitted image should stay centred");
    }

    #[test]
    fn fitting_scales_a_tall_image_by_its_height() {
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 100.0));
        let fitted = fit_rect(bounds, Vec2::new(100.0, 400.0));
        assert_eq!(fitted.height(), 100.0);
        assert_eq!(fitted.width(), 25.0);
    }

    #[test]
    fn fitting_upscales_a_small_thumbnail_to_fill_a_large_tile() {
        // Thumbnails are generated in tiers, so a tile bigger than its tier
        // has to stretch rather than sit small in the middle.
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 400.0));
        let fitted = fit_rect(bounds, Vec2::new(100.0, 100.0));
        assert_eq!(fitted.width(), 400.0);
    }

    #[test]
    fn fitting_a_degenerate_size_does_not_produce_nonsense() {
        let bounds = Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 100.0));
        assert_eq!(fit_rect(bounds, Vec2::ZERO).width(), 0.0);
    }

    fn positions(paths: &[&str]) -> HashMap<PathBuf, usize> {
        paths.iter().enumerate().map(|(i, p)| (PathBuf::from(p), i)).collect()
    }

    fn entries(paths: &[&str]) -> Vec<Entry> {
        paths
            .iter()
            .map(|p| Entry {
                path: PathBuf::from(p),
                name: (*p).to_owned(),
                kind: MediaKind::Video,
                len: 0,
                modified: None,
            })
            .collect()
    }

    #[test]
    fn an_unchanged_order_is_recognised_as_unchanged() {
        let paths = ["a.mp4", "b.mp4", "c.mp4"];
        assert!(!order_changed(&entries(&paths), &positions(&paths)));
    }

    #[test]
    fn a_swapped_pair_counts_as_changed() {
        assert!(order_changed(&entries(&["b.mp4", "a.mp4"]), &positions(&["a.mp4", "b.mp4"])));
    }

    #[test]
    fn a_different_number_of_entries_counts_as_changed() {
        assert!(order_changed(&entries(&["a.mp4"]), &positions(&["a.mp4", "b.mp4"])));
        assert!(order_changed(&entries(&["a.mp4", "b.mp4"]), &positions(&["a.mp4"])));
    }

    #[test]
    fn the_first_sort_of_a_folder_counts_as_changed() {
        // Positions start empty, so the initial sort must be treated as a move.
        assert!(order_changed(&entries(&["a.mp4"]), &HashMap::new()));
    }

    fn image_area() -> Rect {
        Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(200.0, 100.0))
    }

    #[test]
    fn the_seek_bar_hugs_the_bottom_of_the_image() {
        let bar = seek_bar_rect(image_area());
        assert_eq!(bar.max.y, 110.0);
        assert_eq!(bar.height(), SEEK_BAR_HEIGHT);
        assert_eq!(bar.width(), 200.0);
    }

    #[test]
    fn the_seek_bar_never_exceeds_a_tiny_tile() {
        let tiny = Rect::from_min_size(Pos2::ZERO, Vec2::new(20.0, 4.0));
        assert_eq!(seek_bar_rect(tiny).height(), 4.0);
    }

    #[test]
    fn scrubbing_maps_pointer_position_to_a_time() {
        let bar = seek_bar_rect(image_area());
        let clip = Duration::from_secs(100);
        assert_eq!(seek_position(bar, bar.min.x, clip), Duration::ZERO);
        assert_eq!(seek_position(bar, bar.center().x, clip), Duration::from_secs(50));
        assert_eq!(seek_position(bar, bar.max.x, clip), clip);
    }

    #[test]
    fn scrubbing_outside_the_bar_clamps_to_its_ends() {
        // Dragging carries on past the edges of the bar; that must pin to the
        // start or end rather than seeking somewhere impossible.
        let bar = seek_bar_rect(image_area());
        let clip = Duration::from_secs(100);
        assert_eq!(seek_position(bar, -5000.0, clip), Duration::ZERO);
        assert_eq!(seek_position(bar, 5000.0, clip), clip);
    }

    #[test]
    fn zooming_scales_the_tile_size() {
        assert_eq!(zoomed_tile_size(200.0, 1.5), 300.0);
        assert_eq!(zoomed_tile_size(200.0, 0.5), 100.0);
    }

    #[test]
    fn zooming_stops_at_the_ends_of_the_range() {
        assert_eq!(zoomed_tile_size(MAX_TILE_PX, 4.0), MAX_TILE_PX);
        assert_eq!(zoomed_tile_size(MIN_TILE_PX, 0.1), MIN_TILE_PX);
    }

    #[test]
    fn a_neutral_or_nonsense_zoom_leaves_the_size_alone() {
        assert_eq!(zoomed_tile_size(200.0, 1.0), 200.0);
        assert_eq!(zoomed_tile_size(200.0, 0.0), 200.0);
        assert_eq!(zoomed_tile_size(200.0, f32::NAN), 200.0);
        assert_eq!(zoomed_tile_size(200.0, -2.0), 200.0);
    }

    #[test]
    fn loaded_settings_are_clamped_into_range() {
        let wild = Settings { tile_px: 99_999.0, budget: 9_999, ..Settings::default() };
        let fixed = wild.sanitized();
        assert_eq!(fixed.tile_px, MAX_TILE_PX);
        assert_eq!(fixed.budget, MAX_BUDGET);

        let tiny = Settings { tile_px: 1.0, budget: 0, ..Settings::default() };
        let fixed = tiny.sanitized();
        assert_eq!(fixed.tile_px, MIN_TILE_PX);
        assert_eq!(fixed.budget, 1, "zero slots would mean nothing ever plays");
    }

    #[test]
    fn a_corrupt_tile_size_falls_back_to_the_default() {
        let broken = Settings { tile_px: f32::NAN, ..Settings::default() };
        assert_eq!(broken.sanitized().tile_px, Settings::default().tile_px);
    }

    #[test]
    fn settings_survive_a_round_trip_through_serde() {
        let settings = Settings {
            tile_px: 480.0,
            autoplay: false,
            budget: 7,
            show_labels: false,
            sort: Sort { key: SortKey::Size, order: SortOrder::Descending },
        };
        let encoded = ron::to_string(&settings).unwrap();
        let decoded: Settings = ron::from_str(&encoded).unwrap();
        assert_eq!(decoded.tile_px, 480.0);
        assert_eq!(decoded.budget, 7);
        assert!(!decoded.autoplay);
        assert!(!decoded.show_labels);
        assert_eq!(decoded.sort.key, SortKey::Size);
        assert_eq!(decoded.sort.order, SortOrder::Descending);
    }

    #[test]
    fn a_view_that_has_never_scrolled_is_settled() {
        // Nothing should wait out the delay just to show the first screen.
        let mut settle = ScrollSettle::default();
        assert!(settle.update(0.0, Instant::now()));
    }

    #[test]
    fn scrolling_is_not_settled_while_the_offset_moves() {
        let mut settle = ScrollSettle::default();
        let start = Instant::now();
        assert!(!settle.update(100.0, start));
        assert!(!settle.update(200.0, start + SCROLL_SETTLE * 2));
    }

    #[test]
    fn holding_still_settles_after_the_delay() {
        let mut settle = ScrollSettle::default();
        let start = Instant::now();
        settle.update(100.0, start);
        assert!(!settle.update(100.0, start), "should not settle instantly");
        assert!(settle.update(100.0, start + SCROLL_SETTLE));
    }

    #[test]
    fn resuming_a_scroll_unsettles_again() {
        let mut settle = ScrollSettle::default();
        let start = Instant::now();
        settle.update(100.0, start);
        assert!(settle.update(100.0, start + SCROLL_SETTLE));
        assert!(!settle.update(140.0, start + SCROLL_SETTLE), "a new scroll must reset it");
    }

    #[test]
    fn tiny_jitter_does_not_count_as_scrolling() {
        // Sub-pixel drift from a trackpad should not hold playback off forever.
        let mut settle = ScrollSettle::default();
        let start = Instant::now();
        settle.update(100.0, start);
        assert!(settle.update(100.2, start + SCROLL_SETTLE));
    }
}
