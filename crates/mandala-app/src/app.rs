//! The application shell: navigation, the tile grid, and the loop that keeps
//! thumbnails and playback pointed at whatever is on screen.

use crate::lang::{Language, Phrase};
use crate::player::{PlaybackService, SlotEvent};
use crate::prefetch::Prefetch;
use crate::thumbs::{Job, ThumbnailService, thumbnail_tier};
use eframe::egui::{
    self, Align2, Color32, Context, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, TextureHandle,
    TextureOptions, Ui, Vec2,
};
use mandala_core::layout::{GridLayout, TileSize};
use mandala_core::schedule::{PlaybackCandidate, ScheduleParams, plan_playback};
use mandala_core::slots::plan_slots;
use mandala_core::sort::{Sort, SortKey, SortOrder, sort_entries_with_folder_stats};
use mandala_core::{Entry, MediaKind, bookmarks, filter, scan_dir};
use mandala_media::Frame;
use std::collections::{BTreeMap, HashMap, HashSet};
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
    /// Folders worth coming back to, oldest first.
    #[serde(default)]
    pub bookmarks: Vec<PathBuf>,
    /// Whether the tile under the pointer is audible.
    ///
    /// Off unless asked for. A grid that starts making noise because the
    /// pointer crossed it is not one anyone would leave open.
    #[serde(default)]
    pub sound: bool,
    /// Captured once from the legacy settings, before the first folder opens.
    directory_defaults: Option<DirectorySettings>,
    directories: BTreeMap<PathBuf, DirectorySettings>,
    /// Keep the key while open, including if a network share disconnects.
    #[serde(skip)]
    active_directory: Option<(PathBuf, PathBuf)>,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct DirectorySettings {
    tile_px: f32,
    autoplay: bool,
    budget: usize,
    show_labels: bool,
    sort: Sort,
    sound: bool,
}

impl Default for DirectorySettings {
    fn default() -> Self {
        Settings::default().view()
    }
}

impl DirectorySettings {
    fn sanitized(mut self) -> Self {
        if !self.tile_px.is_finite() {
            self.tile_px = Self::default().tile_px;
        }
        self.tile_px = self.tile_px.clamp(MIN_TILE_PX, MAX_TILE_PX);
        self.budget = self.budget.clamp(1, MAX_BUDGET);
        self
    }
}

/// Resolve aliases on navigation, never on every frame. ZIP members have no
/// filesystem identity of their own, so resolve their archive instead.
fn directory_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    if let Some((archive, member)) = mandala_core::archive::location(path)
        && let Ok(canonical) = std::fs::canonicalize(archive)
    {
        return canonical.join(member);
    }
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

impl Settings {
    fn view(&self) -> DirectorySettings {
        DirectorySettings {
            tile_px: self.tile_px,
            autoplay: self.autoplay,
            budget: self.budget,
            show_labels: self.show_labels,
            sort: self.sort,
            sound: self.sound,
        }
    }

    fn apply_view(&mut self, view: DirectorySettings) {
        self.tile_px = view.tile_px;
        self.autoplay = view.autoplay;
        self.budget = view.budget;
        self.show_labels = view.show_labels;
        self.sort = view.sort;
        self.sound = view.sound;
    }

    fn remember_directory(&mut self, path: &Path) {
        if !path.as_os_str().is_empty() {
            let key = self
                .active_directory
                .as_ref()
                .filter(|(active, _)| active == path)
                .map(|(_, key)| key.clone())
                .unwrap_or_else(|| directory_key(path));
            self.directories.insert(key, self.view().sanitized());
        }
    }

    fn enter_directory(&mut self, previous: &Path, next: &Path) {
        let initial = self.view().sanitized();
        let defaults = *self.directory_defaults.get_or_insert(initial);
        self.remember_directory(previous);
        let key = directory_key(next);
        let view = self.directories.get(&key).copied().unwrap_or(defaults);
        self.apply_view(view);
        self.active_directory = Some((next.to_path_buf(), key));
    }

    /// Clamps values loaded from disk. A settings file edited by hand, or left
    /// over from a version with different limits, must not wedge the grid.
    fn sanitized(mut self) -> Self {
        self.apply_view(self.view().sanitized());
        self.directory_defaults = self.directory_defaults.map(DirectorySettings::sanitized);
        for view in self.directories.values_mut() {
            *view = view.sanitized();
        }
        self.bookmarks = bookmarks::sanitized(std::mem::take(&mut self.bookmarks));
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
            budget: ScheduleParams::default().budget,
            show_labels: true,
            sort: Sort::default(),
            bookmarks: Vec::new(),
            sound: false,
            directory_defaults: None,
            directories: BTreeMap::new(),
            active_directory: None,
        }
    }
}

pub struct MandalaApp {
    clipboard_keys: crate::shell::ClipboardKeys,
    selection: crate::selection::Selection,
    rename_edit: Option<(PathBuf, String, bool)>,
    operation_error: Option<String>,
    counts: crate::counts::Counts,
    file_counts: HashMap<PathBuf, Option<u64>>,
    folder_sizes: HashMap<PathBuf, Option<u64>>,
    count_request: Option<(SortKey, u64)>,
    tabs: Vec<crate::tabs::Tab>,
    active_tab: usize,
    scroll_offset: f32,
    restore_scroll: Option<f32>,
    current_dir: PathBuf,
    /// Everything the folder holds, as read from disk.
    scanned: Vec<Entry>,
    /// What the grid shows: `scanned`, narrowed by the filter and sorted.
    ///
    /// Two lists rather than one so that clearing the filter costs nothing and
    /// needs no second trip to the disk -- and so every existing reader of
    /// `entries` keeps meaning "the tiles on screen", which is what they all
    /// wanted anyway.
    entries: Vec<Entry>,
    /// What was typed into the filter box. Not persisted: an application that
    /// starts up hiding most of a folder, for a reason recorded in a settings
    /// file, is one nobody can debug from the outside.
    filter: String,
    /// Bumped on every navigation, so frames decoded for the previous folder
    /// are recognisable as stale and dropped.
    generation: u64,
    error: Option<String>,
    path_edit: String,

    settings: Settings,
    thumbs: ThumbnailService,
    player: PlaybackService,
    sound: crate::sound::SoundService,

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
    /// Thumbnails asked for, by path and the tier they were asked at. A
    /// failure stays in here too: nothing is ever retried within a tier, so a
    /// separate set of failures would only be a subset of this one.
    requested_thumbs: HashSet<(PathBuf, u32)>,
    requested_durations: HashSet<PathBuf>,
    prefetch: Prefetch,
    thumbnail_view: Option<(std::ops::Range<usize>, u32)>,
    /// Videos still to probe for length, newest folder first. Refilled on
    /// navigation and re-sort, so a folder whose lengths are all known costs
    /// nothing per frame.
    unprobed: Vec<usize>,
    /// The item count line in the toolbar, which changes only on navigation.
    summary: String,
    /// Tier the on-screen thumbnails were asked for at.
    current_tier: u32,
    /// Sort the entries are currently in, to notice when the setting changes.
    applied_sort: Sort,
    /// Set when a newly learned duration invalidates the current order.
    needs_resort: bool,

    /// Where each playing tile has got to, for drawing its seek bar.
    playback: HashMap<usize, PlaybackInfo>,

    /// Which language the interface speaks, taken from Windows at startup.
    lang: Language,

    hovered: Option<usize>,
    visible: std::ops::Range<usize>,
    /// The visible range the last texture sweep was made for.
    trimmed_for: std::ops::Range<usize>,
    settle: ScrollSettle,
}

#[derive(Clone, Copy)]
struct PlaybackInfo {
    position: Duration,
    duration: Option<Duration>,
}

impl MandalaApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        start: PathBuf,
        lang: Language,
    ) -> anyhow::Result<Self> {
        // Before anything draws, or the first frame is all boxes.
        crate::fonts::install_fallbacks(&cc.egui_ctx);

        // Which backend that is, is mandala-media's business.
        let backend = mandala_media::default_backend()?;

        // Workers wake the UI thread when they have something to show, rather
        // than the UI polling at a fixed rate and burning frames on nothing.
        let ctx = cc.egui_ctx.clone();
        let repaint = move || ctx.request_repaint();

        let thumbs = ThumbnailService::new(backend, repaint.clone());
        let counts = crate::counts::Counts::new(repaint.clone());
        let mut player = PlaybackService::new(backend, repaint);

        let settings = cc
            .storage
            .and_then(|storage| eframe::get_value::<Settings>(storage, SETTINGS_KEY))
            .unwrap_or_default()
            .sanitized();
        player.resize(settings.budget);

        let applied_sort = settings.sort;
        let mut app = Self {
            clipboard_keys: crate::shell::ClipboardKeys::new(cc)?,
            selection: Default::default(),
            rename_edit: None,
            operation_error: None,
            counts,
            file_counts: HashMap::new(),
            folder_sizes: HashMap::new(),
            count_request: None,
            tabs: vec![crate::tabs::Tab::default()],
            active_tab: 0,
            scroll_offset: 0.0,
            restore_scroll: None,
            current_dir: PathBuf::new(),
            scanned: Vec::new(),
            entries: Vec::new(),
            filter: String::new(),
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
            prefetch: Prefetch::default(),
            thumbnail_view: None,
            unprobed: Vec::new(),
            summary: String::new(),
            current_tier: 0,
            applied_sort,
            needs_resort: false,
            sound: crate::sound::SoundService::new(),
            playback: HashMap::new(),
            lang,
            hovered: None,
            visible: 0..0,
            trimmed_for: 0..0,
            settle: ScrollSettle::default(),
        };
        app.navigate_to(start);
        Ok(app)
    }

    fn navigate_to(&mut self, path: PathBuf) {
        self.remember_tab();
        self.selection = Default::default();
        self.rename_edit = None;
        self.operation_error = None;
        self.counts.cancel();
        self.count_request = None;
        self.file_counts.clear();
        self.folder_sizes.clear();
        self.thumbs.clear_pending();
        self.prefetch.reset(0);
        self.generation += 1;
        self.stop_playback();
        self.sound.follow(None, Duration::ZERO);
        self.thumb_textures.clear();
        self.requested_thumbs.clear();
        self.requested_durations.clear();
        self.durations.clear();
        self.hovered = None;
        self.visible = 0..0;

        // Cleared on arrival. Carrying it into the next folder means opening
        // one and being shown nothing, with the reason in a box further up that
        // nobody is looking at.
        self.filter.clear();

        match scan_dir(&path) {
            Ok(entries) => {
                self.settings.enter_directory(&self.current_dir, &path);
                self.scanned = entries;
                self.error = None;
                self.path_edit = path.display().to_string();
                self.current_dir = path;
                self.scroll_offset = self.tabs[self.active_tab].offset(&self.current_dir);
                self.restore_scroll = Some(self.scroll_offset);
                self.settle = ScrollSettle::default();
                self.resort();
            }
            Err(e) => {
                self.error = Some(format!("{}: {e}", path.display()));
                self.scanned.clear();
                self.entries.clear();
                self.path_to_tile.clear();
                self.unprobed.clear();
                self.summary.clear();
            }
        }
    }

    fn remember_tab(&mut self) {
        if !self.current_dir.as_os_str().is_empty() {
            self.tabs[self.active_tab].remember(
                self.current_dir.clone(),
                self.filter.clone(),
                self.scroll_offset,
            );
        }
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|e| self.selection.paths.contains(&e.path))
            .map(|e| e.path.clone())
            .collect()
    }

    fn begin_rename(&mut self) {
        let paths = self.selected_paths();
        if paths.len() == 1 && mandala_core::archive::location(&self.current_dir).is_none() {
            let path = paths[0].clone();
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            self.rename_edit = Some((path, name, true));
        }
    }

    fn file_shortcuts(&mut self, ctx: &Context, frame: &eframe::Frame) {
        let native_paste = self.clipboard_keys.take_paste();
        if ctx.text_edit_focused() || self.rename_edit.is_some() {
            return;
        }
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::A)) {
            self.selection.paths = self.entries.iter().map(|e| e.path.clone()).collect();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F2)) {
            self.begin_rename();
        }
        if mandala_core::archive::location(&self.current_dir).is_some() {
            return;
        }
        let action = ctx.input(|i| {
            let key = |key| i.modifiers.ctrl && i.key_pressed(key);
            if key(egui::Key::C) || i.events.iter().any(|e| matches!(e, egui::Event::Copy)) {
                Some(crate::shell::Action::Copy)
            } else if key(egui::Key::X) || i.events.iter().any(|e| matches!(e, egui::Event::Cut)) {
                Some(crate::shell::Action::Cut)
            } else if native_paste
                || key(egui::Key::V)
                || i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)))
            {
                Some(crate::shell::Action::Paste)
            } else {
                None
            }
        });
        if let Some(action) = action {
            let paths = self.selected_paths();
            if paths.is_empty() && !matches!(action, crate::shell::Action::Paste) {
                return;
            }
            match crate::shell::action(frame, action, &paths, &self.current_dir) {
                Ok(()) if matches!(action, crate::shell::Action::Paste) => self.reload(),
                Ok(()) => {}
                Err(e) => self.operation_error = Some(e.to_string()),
            }
        }
    }

    fn rename_dialog(&mut self, ctx: &Context, frame: &eframe::Frame) {
        let Some((path, mut name, focus)) = self.rename_edit.take() else { return };
        let mut apply = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("rename-file")).show(ctx, |ui| {
            ui.heading(self.lang.text(Phrase::Rename));
            ui.label(path.display().to_string());
            let edit = ui.text_edit_singleline(&mut name);
            if focus {
                edit.request_focus();
            }
            let valid = crate::selection::valid_name(&name);
            if !valid {
                ui.colored_label(ui.visuals().error_fg_color, self.lang.text(Phrase::InvalidName));
            }
            ui.horizontal(|ui| {
                apply = ui
                    .add_enabled(valid, egui::Button::new(self.lang.text(Phrase::Apply)))
                    .clicked()
                    || (valid && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                cancel = ui.button(self.lang.text(Phrase::Cancel)).clicked()
                    || ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
        });
        if apply {
            match crate::shell::rename(frame, &path, &name) {
                Ok(()) => self.reload(),
                Err(e) => {
                    self.operation_error = Some(e.to_string());
                    self.rename_edit = Some((path, name, false));
                }
            }
        } else if !cancel {
            self.rename_edit = Some((path, name, false));
        }
    }

    fn switch_tab(&mut self, index: usize) {
        if index == self.active_tab || index >= self.tabs.len() {
            return;
        }
        self.remember_tab();
        self.settings.remember_directory(&self.current_dir);
        self.current_dir = PathBuf::new();
        self.active_tab = index;
        let path = self.tabs[index].path.clone();
        let filter = self.tabs[index].filter.clone();
        self.navigate_to(path.clone());
        // Keep a failed tab address usable for retrying or navigating upward.
        if self.current_dir.as_os_str().is_empty() {
            self.path_edit = path.display().to_string();
            self.current_dir = path;
        }
        self.filter = filter;
        self.resort();
    }

    fn open_tab(&mut self, path: PathBuf) {
        self.remember_tab();
        let mut tab = crate::tabs::Tab::default();
        tab.path = path;
        self.tabs.push(tab);
        self.switch_tab(self.tabs.len() - 1);
    }

    fn tab_bar(&mut self, ui: &mut Ui) {
        let mut selected = None;
        let mut closed = None;
        let mut duplicate = false;
        egui::Panel::top("tabs").show(ui, |ui| {
            egui::ScrollArea::horizontal().id_salt("tab-strip").show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (index, tab) in self.tabs.iter().enumerate() {
                        let path =
                            if index == self.active_tab { &self.current_dir } else { &tab.path };
                        let label = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
                        ui.push_id(index, |ui| {
                            if ui
                                .selectable_label(index == self.active_tab, label)
                                .on_hover_text(path.display().to_string())
                                .clicked()
                            {
                                selected = Some(index);
                            }
                            if self.tabs.len() > 1
                                && ui
                                    .small_button("×")
                                    .on_hover_text(self.lang.text(Phrase::CloseTab))
                                    .clicked()
                            {
                                closed = Some(index);
                            }
                        });
                    }
                    duplicate =
                        ui.button("+").on_hover_text(self.lang.text(Phrase::NewTab)).clicked();
                });
            });
        });
        if let Some(index) = selected {
            self.switch_tab(index);
        }
        if let Some(index) = closed {
            if index == self.active_tab {
                self.switch_tab(if index > 0 { index - 1 } else { 1 });
            }
            self.tabs.remove(index);
            let (active, restore) =
                crate::tabs::after_close(self.active_tab, index, self.scroll_offset);
            self.active_tab = active;
            if restore.is_some() {
                self.restore_scroll = restore;
            }
        }
        if duplicate {
            self.open_tab(self.current_dir.clone());
        }
    }

    /// Reads the current folder again.
    ///
    /// Not `navigate_to(current_dir)`, which would be the obvious thing and the
    /// wrong one: that clears the filter and throws away every thumbnail. What
    /// someone reloading wants is the same view of a folder that has changed
    /// underneath them, not a fresh start.
    ///
    /// Textures are kept and merely asked for again. The disk cache is keyed on
    /// path, size and modification time, so a file that did not change comes
    /// back identical -- nothing flickers -- and one that did comes back
    /// different a moment later. Dropping them first would blank the grid to
    /// arrive at the same place.
    ///
    /// Files that are gone lose their tiles, and their textures with them: the
    /// position map is rebuilt from what is still there, and eviction goes by
    /// that map.
    fn reload(&mut self) {
        self.counts.cancel();
        self.count_request = None;
        self.file_counts.clear();
        self.folder_sizes.clear();
        let here = self.current_dir.clone();
        match scan_dir(&here) {
            Ok(entries) => {
                self.thumbs.clear_pending();
                self.scanned = entries;
                self.error = None;
                // Asked for again rather than assumed unchanged. A file whose
                // contents moved on keeps its path, and its record here is all
                // that would stop anyone noticing.
                self.requested_thumbs.clear();
                self.requested_durations.clear();
                self.durations.clear();
                self.resort();
            }
            Err(e) => {
                // The folder went while it was open: a drive unplugged, a share
                // dropped. Say so rather than showing its contents as if they
                // were still there.
                self.error = Some(format!("{}: {e}", here.display()));
                self.scanned.clear();
                self.entries.clear();
                self.path_to_tile.clear();
                self.unprobed.clear();
                self.summary.clear();
                self.stop_playback();
            }
        }
    }

    /// Tears down everything tied to a playback slot.
    ///
    /// Slots are bound to positions, so every caller that moves positions has
    /// to do all of this; one place to add the next such map to.
    fn stop_playback(&mut self) {
        self.player.stop_all();
        self.video_textures.clear();
        self.playback.clear();
    }

    /// Puts the entries in the configured order and rebuilds the position map.
    ///
    /// Playback is tied to positions, so anything playing has to stop when
    /// positions move -- but only then. Sorting by length re-sorts every time
    /// another duration arrives, and most of those change nothing; tearing
    /// down playback for each would mean nothing ever gets to play while a
    /// folder is being probed.
    fn resort(&mut self) {
        // Narrowed first, then sorted: sorting what is about to be discarded is
        // work for nothing, and on a folder of ten thousand files that is the
        // difference between typing smoothly and typing into treacle.
        self.entries = self
            .scanned
            .iter()
            .filter(|e| filter::matches(&e.name, &self.filter))
            .cloned()
            .collect();

        let durations = &self.durations;
        sort_entries_with_folder_stats(
            &mut self.entries,
            self.settings.sort,
            |entry| durations.get(&entry.path).copied().flatten(),
            |entry| self.file_counts.get(&entry.path).copied().flatten(),
            |entry| self.folder_sizes.get(&entry.path).copied().flatten(),
        );
        let present: HashSet<_> = self.entries.iter().map(|e| &e.path).collect();
        self.selection.paths.retain(|path| present.contains(path));
        let key = self.settings.sort.key;
        if matches!(key, SortKey::FileCount | SortKey::Size)
            && self.count_request.is_none_or(|(requested, _)| requested != key)
        {
            let cache = if key == SortKey::Size { &self.folder_sizes } else { &self.file_counts };
            let paths: Vec<_> = self
                .scanned
                .iter()
                .filter(|e| e.is_dir() && !cache.contains_key(&e.path))
                .map(|e| e.path.clone())
                .collect();
            if !paths.is_empty() {
                let metric = if key == SortKey::Size {
                    crate::counts::Metric::Size
                } else {
                    crate::counts::Metric::FileCount
                };
                self.count_request = Some((key, self.counts.request(metric, paths)));
            }
        } else if !matches!(key, SortKey::FileCount | SortKey::Size) {
            self.counts.cancel();
            self.count_request = None;
        }

        if order_changed(&self.entries, &self.path_to_tile) {
            self.path_to_tile = self
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.path.clone(), index))
                .collect();
            self.stop_playback();
        }
        self.refresh_probe_queue();
        self.reset_prefetch();
        self.summary = self.describe_contents();
        self.applied_sort = self.settings.sort;
        self.needs_resort = false;
    }

    /// Rebuilds the list of videos whose length is still unknown.
    ///
    /// Built once per re-sort rather than rediscovered every frame: walking
    /// every entry and hashing its path, forever, to find nothing left to do is
    /// most of the cost of a large folder once the probing has finished.
    fn refresh_probe_queue(&mut self) {
        self.unprobed.clear();
        if !self.settings.sort.key.needs_probe() {
            return;
        }
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.kind == MediaKind::Video && !self.durations.contains_key(&entry.path) {
                self.unprobed.push(index);
            }
        }
        // Popped from the back, so the front of the folder is probed first.
        self.unprobed.reverse();
    }

    fn describe_contents(&self) -> String {
        let videos = self.entries.iter().filter(|e| e.kind == MediaKind::Video).count();
        if self.filter.trim().is_empty() {
            return self.lang.item_summary(self.entries.len(), videos);
        }
        self.lang.filtered_summary(self.entries.len(), self.scanned.len(), videos)
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
        self.reset_prefetch();
        // Requests are keyed by tier, so work already in flight keeps its
        // record and is not queued a second time by the next frame. Dragging
        // the slider across a boundary used to re-request the whole visible
        // set, including the jobs that were running as it did so.
    }

    fn parent_dir(&self) -> Option<PathBuf> {
        self.current_dir.parent().map(Path::to_path_buf)
    }

    fn activate(&mut self, index: usize) {
        let Some(entry) = self.entries.get(index) else { return };
        if entry.is_dir() {
            self.navigate_to(entry.path.clone());
        } else if mandala_core::archive::location(&entry.path).is_some() {
            self.operation_error = Some(self.lang.text(Phrase::ZipPreviewOnly).to_owned());
        } else {
            // explorer.exe hands the file to whatever is registered for it.
            let _ = std::process::Command::new("explorer.exe").arg(&entry.path).spawn();
        }
    }

    fn collect_results(&mut self, ctx: &Context) {
        for (id, counts) in self.counts.results.try_iter() {
            if let Some((key, requested)) = self.count_request
                && requested == id
            {
                if key == SortKey::Size {
                    self.folder_sizes.extend(counts);
                } else {
                    self.file_counts.extend(counts);
                }
                if self.settings.sort.key == key {
                    self.needs_resort = true;
                }
            }
        }
        for done in self.thumbs.drain().collect::<Vec<_>>() {
            if done.prefetch_epoch.is_some_and(|epoch| epoch != self.prefetch.epoch) {
                continue;
            }
            if done.tier == Some(self.current_tier) {
                self.prefetch.completed(done.path.clone());
            }
            if let Some(duration) = done.duration {
                // A newly learned length can change where its tile belongs.
                let known = self.durations.insert(done.path.clone(), Some(duration));
                if known != Some(Some(duration)) && self.settings.sort.key.needs_probe() {
                    self.needs_resort = true;
                }
            } else if done.prefetch_epoch.is_none() && self.requested_durations.contains(&done.path)
            {
                // Asked and there is none; remembered so it is not asked again.
                self.durations.entry(done.path.clone()).or_insert(None);
            }

            if !self.path_to_tile.contains_key(&done.path) {
                if let Some(tier) = done.tier {
                    self.requested_thumbs.remove(&(done.path, tier));
                }
                continue;
            }

            // A failure needs no record of its own: the request set already
            // stops it being asked for again within this tier.
            if let Some(thumbnail) = done.thumbnail {
                let on_screen = self
                    .path_to_tile
                    .get(&done.path)
                    .is_some_and(|index| self.visible.contains(index));
                if done.tier == Some(self.current_tier) && on_screen {
                    if let Ok(frame) = thumbnail {
                        let name = done.path.to_string_lossy().into_owned();
                        let texture = upload(ctx, &name, &frame);
                        self.thumb_textures.insert(done.path, texture);
                    }
                } else if let Some(tier) = done.tier {
                    // The result is cached on disk. Re-request it when visible,
                    // rather than uploading late results outside the keep range.
                    self.requested_thumbs.remove(&(done.path, tier));
                }
            }
        }

        for event in self.player.drain() {
            let decoded = match event {
                SlotEvent::Frame(decoded) => decoded,
                // The slot has already been freed; drop the stale video so the
                // still thumbnail comes back rather than a frozen last frame.
                SlotEvent::Lost { tile } => {
                    self.video_textures.remove(&tile);
                    self.playback.remove(&tile);
                    continue;
                }
            };
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
        let tier = self.current_tier;
        let view = (self.visible.clone(), tier);
        if self.thumbnail_view.as_ref() != Some(&view) {
            let visible =
                self.entries[self.visible.clone()].iter().map(|entry| entry.path.clone()).collect();
            for deferred in self.thumbs.defer_outside(&visible, tier, self.prefetch.epoch) {
                release_request_marker(
                    deferred,
                    &mut self.requested_thumbs,
                    &mut self.requested_durations,
                );
            }
            self.thumbnail_view = Some(view);
        }
        for index in self.visible.clone() {
            let Some(entry) = self.entries.get(index) else { continue };
            if !entry.kind.has_thumbnail()
                || self.requested_thumbs.contains(&(entry.path.clone(), tier))
            {
                continue;
            }
            self.requested_thumbs.insert((entry.path.clone(), tier));
            self.thumbs.request(Job::thumbnail(entry, tier));
        }
    }

    /// Asks for the running times that sorting by length is waiting on.
    ///
    /// Only while that sort is selected: probing every video in a large folder
    /// is real work, and nothing else needs the answer. A few per frame keeps
    /// the queue from being flooded by one enormous folder.
    fn request_missing_durations(&mut self) {
        const PER_FRAME: usize = 24;

        let mut jobs = Vec::new();
        while jobs.len() < PER_FRAME {
            let Some(index) = self.unprobed.pop() else { break };
            let Some(entry) = self.entries.get(index) else { continue };
            if self.durations.contains_key(&entry.path)
                || self.requested_durations.contains(&entry.path)
            {
                continue;
            }
            // A visible tile is already being opened for its thumbnail, and
            // the length comes back with it; probing as well opens the same
            // file twice, for exactly the tiles where latency is felt.
            if self.requested_thumbs.contains(&(entry.path.clone(), self.current_tier)) {
                continue;
            }
            jobs.push((entry.path.clone(), Job::duration(entry)));
        }
        for (path, job) in jobs {
            self.requested_durations.insert(path);
            self.thumbs.request(job);
        }
    }

    fn reset_prefetch(&mut self) {
        self.prefetch.reset(self.entries.len());
        self.thumbs.rebase_background(self.prefetch.epoch, self.current_tier);
        self.thumbnail_view = None;
    }

    fn request_prefetch(&mut self, ctx: &Context, settled: bool) {
        self.thumbs.update_load(Instant::now());
        if self.prefetch.has_remaining() || self.thumbs.has_work() {
            ctx.request_repaint_after(crate::load::LOAD_INTERVAL);
        }
        if !settled {
            return;
        }
        for _ in 0..self.thumbs.prefetch_capacity() {
            if let Some(index) = self.prefetch.next(
                &self.entries,
                self.visible.clone(),
                &self.requested_thumbs,
                self.current_tier,
            ) {
                let entry = &self.entries[index];
                if !self
                    .thumbs
                    .prefetch(Job::thumbnail(entry, self.current_tier), self.prefetch.epoch)
                {
                    self.prefetch.retry(entry.path.clone(), index);
                    break;
                }
            } else {
                if self.prefetch.has_remaining() {
                    // Only needed when a bounded planning pass skipped 64 entries.
                    ctx.request_repaint();
                }
                break;
            }
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
        let playing: Vec<usize> = self.player.playing_tiles().collect();
        let wanted = if self.settings.autoplay {
            plan_playback(
                &candidates,
                viewport_center_y,
                hovered,
                &playing,
                ScheduleParams { budget: self.settings.budget, ..Default::default() },
            )
        } else {
            hovered.into_iter().collect()
        };

        let holding = self.player.holding().to_vec();
        let mut plan = plan_slots(&holding, &wanted);
        if !settled {
            // Freeing a slot that scrolled away is still worth doing at once;
            // it is only opening new decoders that waits.
            plan.start.clear();
        }

        for &slot in &plan.stop {
            // The still thumbnail takes over again the moment a tile stops.
            if let Some(tile) = holding.get(slot).copied().flatten() {
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
    ///
    /// Only when the view has actually moved: the keep margin is several rows
    /// wide, so a still frame has nothing to evict, and sweeping every stored
    /// texture against its position hashes a path apiece for no result.
    fn trim_textures(&mut self, layout: &GridLayout) {
        if self.visible == self.trimmed_for {
            return;
        }
        self.trimmed_for = self.visible.clone();

        let margin = TEXTURE_KEEP_ROWS * layout.columns();
        let keep_start = self.visible.start.saturating_sub(margin);
        let keep_end = self.visible.end.saturating_add(margin);
        evict_thumbnails(
            &mut self.thumb_textures,
            &mut self.requested_thumbs,
            &self.path_to_tile,
            keep_start..keep_end,
        );
        // Video textures belong to playing tiles, which are visible by
        // definition; anything else is a leftover from a stopped slot.
        let playing: HashSet<usize> = self.player.playing_tiles().collect();
        self.video_textures.retain(|i, _| playing.contains(i));
        self.playback.retain(|i, _| playing.contains(i));
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let parent = self.parent_dir();
                let up = ui
                    .add_enabled(parent.is_some(), egui::Button::new("↑"))
                    .on_hover_text(self.lang.text(Phrase::UpOneLevel));
                if up.clicked()
                    && let Some(parent) = parent
                {
                    self.navigate_to(parent);
                }

                if ui.button("\u{21bb}").on_hover_text(self.lang.text(Phrase::Reload)).clicked() {
                    self.reload();
                }

                // Before the path field, not after: that field is told to take
                // every pixel left on the row, so anything following it is laid
                // out with none and never appears at all.
                //
                // One button says whether this folder is kept and changes its
                // mind, rather than two that are each wrong half the time.
                let kept = bookmarks::contains(&self.settings.bookmarks, &self.current_dir);
                let star = if kept { "\u{2605}" } else { "\u{2606}" };
                let tip = if kept { Phrase::BookmarkRemove } else { Phrase::BookmarkAdd };
                if ui.button(star).on_hover_text(self.lang.text(tip)).clicked() {
                    let here = self.current_dir.clone();
                    bookmarks::toggle(&mut self.settings.bookmarks, &here);
                }

                if let Some(chosen) = self.bookmark_menu(ui) {
                    self.navigate_to(chosen);
                }

                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.path_edit).desired_width(f32::INFINITY),
                );
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.navigate_to(PathBuf::from(self.path_edit.trim()));
                }
            });

            ui.horizontal(|ui| {
                ui.label(self.lang.text(Phrase::Size));
                ui.add(
                    egui::Slider::new(&mut self.settings.tile_px, MIN_TILE_PX..=MAX_TILE_PX)
                        .show_value(false)
                        .logarithmic(true),
                );
                ui.separator();

                ui.checkbox(&mut self.settings.autoplay, self.lang.text(Phrase::Autoplay));
                ui.label(self.lang.text(Phrase::AtOnce));
                ui.add(egui::Slider::new(&mut self.settings.budget, 1..=MAX_BUDGET));
                ui.separator();

                ui.label(self.lang.text(Phrase::Sort));
                let lang = self.lang;
                egui::ComboBox::from_id_salt("sort-key")
                    .selected_text(lang.sort_key(self.settings.sort.key))
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for key in SortKey::ALL {
                            ui.selectable_value(
                                &mut self.settings.sort.key,
                                key,
                                lang.sort_key(key),
                            );
                        }
                    });
                let arrow = match self.settings.sort.order {
                    SortOrder::Ascending => "\u{2191}",
                    SortOrder::Descending => "\u{2193}",
                };
                let tip = lang.sort_order(self.settings.sort.order);
                if ui.button(arrow).on_hover_text(tip).clicked() {
                    self.settings.sort.order = self.settings.sort.order.flipped();
                }

                ui.checkbox(&mut self.settings.sound, self.lang.text(Phrase::Sound))
                    .on_hover_text(self.lang.text(Phrase::SoundTip));

                ui.separator();
                ui.checkbox(&mut self.settings.show_labels, self.lang.text(Phrase::Names));

                ui.separator();
                // The placeholder is the label: the row is crowded already, and
                // a word repeated beside the box it belongs to earns nothing.
                let typed = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .desired_width(180.0)
                            .hint_text(self.lang.text(Phrase::Filter)),
                    )
                    .changed();
                if typed {
                    // Re-narrowing goes through the same path as re-sorting,
                    // which already knows how to stand playback back up when
                    // positions move underneath it.
                    self.needs_resort = true;
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(&self.summary);
                });
            });
        });
    }

    /// Points the sound at whatever the pointer is over.
    ///
    /// Only a video that is actually playing: a still has no sound, and a
    /// video whose tile is not playing is not one anyone is watching. Called
    /// every frame, and cheap when the answer has not changed.
    fn follow_sound(&mut self) {
        let playing = self
            .hovered
            .filter(|_| self.settings.sound)
            .and_then(|tile| Some((tile, *self.playback.get(&tile)?)))
            .filter(|(tile, _)| {
                self.entries.get(*tile).is_some_and(|e| e.kind == MediaKind::Video)
            });
        // The picture's position travels with the file, since it is what the
        // sound has to be moved to -- both when it starts and whenever the two
        // have come apart.
        match playing.and_then(|(tile, info)| Some((self.entries.get(tile)?, info))) {
            Some((entry, info)) => self.sound.follow(Some(&entry.path), info.position),
            None => self.sound.follow(None, Duration::ZERO),
        }
    }

    /// Draws the bookmarks menu and reports the folder chosen from it.
    ///
    /// Returns rather than navigating, because navigating borrows all of
    /// `self` and the menu is still holding part of it.
    fn bookmark_menu(&self, ui: &mut Ui) -> Option<PathBuf> {
        let mut chosen = None;
        ui.menu_button(self.lang.text(Phrase::Bookmark), |ui| {
            if self.settings.bookmarks.is_empty() {
                ui.label(self.lang.text(Phrase::BookmarksEmpty));
                return;
            }
            // Newest first: the folder just kept is the one most likely wanted
            // again, and the list is oldest-first so that the cap drops the
            // stalest rather than the freshest.
            for path in self.settings.bookmarks.iter().rev() {
                let label = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                // The whole path on hover: two folders called `2026-08` are
                // indistinguishable by name, which is the usual case here.
                if ui.button(label).on_hover_text(path.display().to_string()).clicked() {
                    chosen = Some(path.clone());
                    ui.close();
                }
            }
        });
        chosen
    }

    /// The scrub strip for a tile, if there is anything to scrub.
    ///
    /// Returns the bar, the position within the clip, and the clip length. One
    /// definition for the drawing and the hit-testing: computed separately,
    /// they drift, and the bar ends up painted where it cannot be clicked.
    fn seek_bar(&self, index: usize, tile: Rect) -> Option<(Rect, Duration, Duration)> {
        let info = *self.playback.get(&index)?;
        let total = info.duration.filter(|d| *d > Duration::ZERO)?;
        Some((seek_bar_rect(self.image_area(tile)), info.position, total))
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
        if self.selection.paths.contains(&entry.path) {
            painter.rect_stroke(
                rect.shrink(1.0),
                CornerRadius::same(6),
                Stroke::new(2.0, visuals.selection.stroke.color),
                egui::StrokeKind::Inside,
            );
        }

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
        if hovered && let Some((bar, position, total)) = self.seek_bar(index, rect) {
            draw_seek_bar(painter, bar, position, total, visuals);
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

impl Drop for MandalaApp {
    fn drop(&mut self) {
        // Stop every producer before fields are dropped in declaration order.
        self.player.shutdown();
        self.sound.shutdown();
        self.thumbs.shutdown();
    }
}

impl eframe::App for MandalaApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.settings.remember_directory(&self.current_dir);
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.collect_results(&ctx);

        // Ctrl held, or a trackpad pinch. Plain wheel has to stay as scrolling,
        // or there would be no way to move through a folder.
        let zoom = ctx.input(|i| i.zoom_delta());
        self.settings.tile_px = zoomed_tile_size(self.settings.tile_px, zoom);

        self.top_bar(ui);
        self.tab_bar(ui);
        if let Some(error) = self.operation_error.clone() {
            egui::Panel::top("file-operation-error").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                    if ui.small_button("×").clicked() {
                        self.operation_error = None;
                    }
                });
            });
        }
        self.file_shortcuts(&ctx, frame);

        // Backspace belongs to whatever text field has focus. Taken globally
        // it did both: the field deleted a character and the folder changed,
        // and changing folder rewrites the path box with the new path -- so
        // one press looked like several characters vanishing at once.
        if self.rename_edit.is_none()
            && !ctx.text_edit_focused()
            && ctx.input(|i| i.key_pressed(egui::Key::Backspace))
            && let Some(parent) = self.parent_dir()
        {
            self.navigate_to(parent);
        }

        // F5 because that is what every file browser and every browser uses,
        // and nobody should have to find the button.
        if self.rename_edit.is_none() && ctx.input(|i| i.key_pressed(egui::Key::F5)) {
            self.reload();
        }

        let mut activated = None;
        let mut context_requested = false;
        let mut hovered_now = None;
        let mut seek_request = None;

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
                return;
            }

            let target = TileSize::square(self.settings.tile_px);
            let count = self.entries.len();

            let mut scroll = egui::ScrollArea::vertical()
                .id_salt(("files", self.active_tab))
                .auto_shrink([false; 2]);
            if let Some(offset) = self.restore_scroll.take() {
                scroll = scroll.vertical_scroll_offset(offset);
            }
            let output = scroll.show_viewport(ui, |ui, viewport| {
                // Measured inside the scroll area, where the width already has
                // the scrollbar taken out of it.
                let layout = GridLayout::stretched(ui.available_width(), target, GAP, PADDING);
                let tile = layout.tile();
                ui.set_height(layout.content_height(count));
                let origin = ui.min_rect().min;

                self.visible =
                    layout.visible_indices(viewport.min.y, viewport.height(), count, OVERSCAN_ROWS);

                for index in self.visible.clone() {
                    let (x, y) = layout.tile_origin(index);
                    let rect =
                        Rect::from_min_size(origin + Vec2::new(x, y), Vec2::new(tile.w, tile.h));
                    let response = ui.interact(rect, ui.id().with(("tile", index)), Sense::click());
                    let mut hovering = response.hovered();
                    if response.clicked() {
                        let modifiers = ui.input(|i| i.modifiers);
                        let paths: Vec<_> = self.entries.iter().map(|e| e.path.clone()).collect();
                        self.selection.click(&paths, index, modifiers.ctrl, modifiers.shift);
                    }
                    if response.secondary_clicked() {
                        if !self.selection.paths.contains(&self.entries[index].path) {
                            let paths: Vec<_> =
                                self.entries.iter().map(|e| e.path.clone()).collect();
                            self.selection.click(&paths, index, false, false);
                        }
                        context_requested = true;
                    }
                    if response.double_clicked() {
                        activated = Some((index, ui.input(|i| i.modifiers.ctrl)));
                    }

                    // The scrub strip sits on top of the tile, so it is claimed
                    // after it and hands hover back to the tile -- otherwise
                    // pointing at the bar would stop the video playing.
                    if let Some((bar, _, total)) = self.seek_bar(index, rect) {
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
                            seek_request = Some((index, seek_position(bar, pointer.x, total)));
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
                self.follow_sound();
                self.retier_thumbnails();
                self.request_visible_thumbnails();
                self.request_missing_durations();
                self.schedule_playback(&layout, center, settled);
                self.trim_textures(&layout);
                self.request_prefetch(&ctx, settled);
            });
            self.scroll_offset = output.state.offset.y;
        });

        // Re-sorting moves every tile, so it happens once the frame is drawn
        // rather than underneath the loop that is drawing it.
        if self.settings.sort != self.applied_sort || self.needs_resort {
            self.resort();
        }

        if let Some((index, position)) = seek_request {
            self.player.seek(index, position);
        }
        if let Some((index, new_tab)) = activated {
            if new_tab && self.entries.get(index).is_some_and(Entry::is_dir) {
                self.open_tab(self.entries[index].path.clone());
            } else {
                self.activate(index);
            }
        }
        if context_requested {
            if mandala_core::archive::location(&self.current_dir).is_some() {
                self.operation_error = Some(self.lang.text(Phrase::ZipPreviewOnly).to_owned());
            } else {
                match crate::shell::popup(frame, &self.selected_paths()) {
                    Ok(Some(true)) => self.begin_rename(),
                    Ok(Some(false)) => self.reload(),
                    Ok(None) => {}
                    Err(e) => self.operation_error = Some(e.to_string()),
                }
            }
        }
        self.rename_dialog(&ctx, frame);
    }
}

/// Deferred jobs remain queued, but must be requestable at visible priority.
fn release_request_marker(
    job: Job,
    requested: &mut HashSet<(PathBuf, u32)>,
    durations: &mut HashSet<PathBuf>,
) {
    if let Some(tier) = job.tier() {
        requested.remove(&(job.path().to_path_buf(), tier));
    } else {
        durations.remove(job.path());
    }
}

/// Drops thumbnails outside the keep window, and forgets they were asked for.
///
/// The two have to move together. A texture dropped while its request record
/// stays behind is never asked for again -- nothing retries a request that is
/// already recorded -- so the tile falls back to a placeholder and stays there
/// for the rest of the session, however many times it scrolls back into view.
fn evict_thumbnails<T>(
    textures: &mut HashMap<PathBuf, T>,
    requested: &mut HashSet<(PathBuf, u32)>,
    positions: &HashMap<PathBuf, usize>,
    keep: std::ops::Range<usize>,
) {
    let mut evicted = Vec::new();
    textures.retain(|path, _| {
        let inside = positions.get(path).is_some_and(|i| keep.contains(i));
        if !inside {
            evicted.push(path.clone());
        }
        inside
    });
    // Only for what actually had a texture: a request still in flight has none
    // yet, and forgetting it would queue the same work twice.
    requested.retain(|(path, _)| !evicted.contains(path));
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
    fn deferred_requests_release_only_their_own_visible_priority_markers() {
        let path = PathBuf::from("pictures.zip");
        let mut requested = HashSet::from([(path.clone(), 128), (path.clone(), 256)]);
        let mut durations = HashSet::from([PathBuf::from("movie.mp4")]);
        release_request_marker(
            Job::Folder { path: path.clone(), tier: 128 },
            &mut requested,
            &mut durations,
        );
        assert!(
            !requested.contains(&(path.clone(), 128)),
            "a deferred tile must be promotable when visible again"
        );
        assert!(requested.contains(&(path, 256)), "another tier may still be in flight");
        let video = PathBuf::from("movie.mp4");
        release_request_marker(
            Job::Duration { meta_key: mandala_core::CacheKey::metadata(&video, 0, 0), path: video },
            &mut requested,
            &mut durations,
        );
        assert!(durations.is_empty());
    }

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
                cache_tag: None,
                path: PathBuf::from(p),
                name: (*p).to_owned(),
                kind: MediaKind::Video,
                len: 0,
                modified: None,
            })
            .collect()
    }

    /// Textures, the requests that produced them, and where each file sits.
    type Evictable = (HashMap<PathBuf, ()>, HashSet<(PathBuf, u32)>, HashMap<PathBuf, usize>);

    fn evictable() -> Evictable {
        let paths: Vec<PathBuf> = (0..10).map(|i| PathBuf::from(format!("f{i}.png"))).collect();
        let textures = paths.iter().map(|p| (p.clone(), ())).collect();
        let requested = paths.iter().map(|p| (p.clone(), 256u32)).collect();
        let positions = paths.iter().cloned().enumerate().map(|(i, p)| (p, i)).collect();
        (textures, requested, positions)
    }

    #[test]
    fn eviction_keeps_what_is_inside_the_window() {
        let (mut textures, mut requested, positions) = evictable();
        evict_thumbnails(&mut textures, &mut requested, &positions, 3..7);

        assert_eq!(textures.len(), 4);
        assert!(textures.contains_key(&PathBuf::from("f3.png")));
        assert!(textures.contains_key(&PathBuf::from("f6.png")));
        assert!(!textures.contains_key(&PathBuf::from("f2.png")));
    }

    #[test]
    fn eviction_also_forgets_that_it_asked() {
        // The bug this exists for: dropping a texture while keeping its request
        // record means the tile is never asked for again, and shows a
        // placeholder for the rest of the session however often it comes back.
        let (mut textures, mut requested, positions) = evictable();
        evict_thumbnails(&mut textures, &mut requested, &positions, 3..7);

        for i in [0, 1, 2, 7, 8, 9] {
            let path = PathBuf::from(format!("f{i}.png"));
            assert!(
                !requested.contains(&(path.clone(), 256)),
                "{} was evicted but is still recorded as requested",
                path.display()
            );
        }
        for i in 3..7 {
            let path = PathBuf::from(format!("f{i}.png"));
            assert!(requested.contains(&(path, 256)), "a kept thumbnail lost its record");
        }
    }

    #[test]
    fn eviction_leaves_requests_that_have_not_arrived_yet() {
        // In flight means no texture yet. Forgetting those would queue the same
        // work a second time on the very next frame.
        let (mut textures, mut requested, positions) = evictable();
        let pending = PathBuf::from("f5.png");
        textures.remove(&pending);

        evict_thumbnails(&mut textures, &mut requested, &positions, 3..7);
        assert!(requested.contains(&(pending, 256)), "an in-flight request was dropped");
    }

    #[test]
    fn eviction_drops_thumbnails_for_files_that_are_gone() {
        // A path with no position no longer exists in this folder at all.
        let (mut textures, mut requested, mut positions) = evictable();
        let vanished = PathBuf::from("f4.png");
        positions.remove(&vanished);

        evict_thumbnails(&mut textures, &mut requested, &positions, 0..10);
        assert!(!textures.contains_key(&vanished));
        assert!(!requested.contains(&(vanished, 256)));
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
    fn each_directory_restores_its_own_view() {
        let mut settings = Settings::default();
        settings.enter_directory(Path::new(""), Path::new("A"));
        settings.tile_px = 800.0;
        settings.budget = 2;
        settings.autoplay = false;
        settings.show_labels = false;
        settings.sound = true;
        settings.sort = Sort { key: SortKey::Size, order: SortOrder::Descending };
        settings.enter_directory(Path::new("A"), Path::new("B"));
        settings.tile_px = 120.0;
        settings.budget = 8;
        settings.enter_directory(Path::new("B"), Path::new("A"));
        assert_eq!(settings.tile_px, 800.0);
        assert_eq!(settings.budget, 2);
        assert!(!settings.autoplay);
        assert!(!settings.show_labels);
        assert!(settings.sound);
        assert_eq!(settings.sort.key, SortKey::Size);
        assert_eq!(settings.sort.order, SortOrder::Descending);
    }

    #[test]
    fn unseen_directories_use_the_original_defaults_not_the_previous_view() {
        let mut settings = Settings { tile_px: 400.0, budget: 3, ..Settings::default() };
        settings.enter_directory(Path::new(""), Path::new("A"));
        settings.tile_px = 1000.0;
        settings.budget = 1;
        settings.enter_directory(Path::new("A"), Path::new("B"));
        assert_eq!(settings.tile_px, 400.0);
        assert_eq!(settings.budget, 3);
    }

    #[test]
    fn directory_views_survive_restart_including_the_current_directory() {
        let mut settings = Settings::default();
        settings.enter_directory(Path::new(""), Path::new("A"));
        settings.tile_px = 700.0;
        settings.remember_directory(Path::new("A"));
        let encoded = ron::to_string(&settings).unwrap();
        let mut decoded: Settings = ron::from_str(&encoded).unwrap();
        decoded.enter_directory(Path::new(""), Path::new("B"));
        assert_eq!(decoded.tile_px, 260.0);
        decoded.enter_directory(Path::new("B"), Path::new("A"));
        assert_eq!(decoded.tile_px, 700.0);
    }

    #[test]
    fn bookmarks_remain_shared_between_directories() {
        let mut settings = Settings::default();
        settings.enter_directory(Path::new(""), Path::new("A"));
        bookmarks::toggle(&mut settings.bookmarks, Path::new("saved"));
        settings.enter_directory(Path::new("A"), Path::new("B"));
        assert!(bookmarks::contains(&settings.bookmarks, Path::new("saved")));
    }

    #[test]
    fn directory_settings_are_sanitized_when_loaded() {
        let mut settings = Settings::default();
        settings.enter_directory(Path::new(""), Path::new("A"));
        settings.tile_px = f32::NAN;
        settings.budget = 0;
        settings.remember_directory(Path::new("A"));
        // Loaded profiles must be checked even if an old app or an editor
        // wrote values beyond the current limits.
        for view in settings.directories.values_mut() {
            view.tile_px = f32::NAN;
            view.budget = 0;
        }
        let mut settings = settings.sanitized();
        settings.enter_directory(Path::new(""), Path::new("A"));
        assert_eq!(settings.tile_px, Settings::default().tile_px);
        assert_eq!(settings.budget, 1);
    }

    #[test]
    fn alternate_spellings_of_a_real_directory_share_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.enter_directory(Path::new(""), dir.path());
        settings.tile_px = 600.0;
        settings.enter_directory(dir.path(), Path::new("elsewhere"));
        settings.enter_directory(Path::new("elsewhere"), &dir.path().join("."));
        assert_eq!(settings.tile_px, 600.0);
    }

    #[test]
    fn an_unavailable_directory_keeps_its_original_settings_key() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("photos");
        std::fs::create_dir(&directory).unwrap();
        let mut settings = Settings::default();
        settings.enter_directory(Path::new(""), &directory);
        settings.tile_px = 900.0;
        std::fs::remove_dir(&directory).unwrap();
        settings.remember_directory(&directory);
        std::fs::create_dir(&directory).unwrap();
        settings.enter_directory(Path::new(""), &directory);
        assert_eq!(settings.tile_px, 900.0);
    }

    #[test]
    fn zip_subfolders_have_independent_view_settings() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("photos.zip");
        // Key resolution only needs an archive's filesystem identity.
        std::fs::write(&archive, b"").unwrap();
        let first = archive.join("旅行");
        let second = archive.join("家族");
        let mut settings = Settings::default();
        settings.enter_directory(Path::new(""), &first);
        settings.tile_px = 550.0;
        settings.enter_directory(&first, &second);
        assert_eq!(settings.tile_px, 260.0);
        settings.enter_directory(&second, &first);
        assert_eq!(settings.tile_px, 550.0);
    }

    #[test]
    fn a_settings_file_from_before_bookmarks_still_loads() {
        // Someone updating from 0.2.0 has one of these. A missing field must
        // read as "no bookmarks yet", not as a settings file the app throws
        // away along with everything else in it.
        let older = "(tile_px:300.0,autoplay:true,budget:8,show_labels:true,                     sort:(key:Name,order:Ascending))";
        let mut decoded: Settings = ron::from_str(older).expect("an older file must still parse");
        assert!(decoded.bookmarks.is_empty());
        assert!(!decoded.sound, "and sound stays off until it is asked for");
        assert_eq!(decoded.budget, 8);
        decoded.enter_directory(Path::new(""), Path::new("first"));
        decoded.tile_px = 900.0;
        decoded.enter_directory(Path::new("first"), Path::new("new"));
        assert_eq!(decoded.tile_px, 300.0, "new folders inherit the legacy view");
        assert_eq!(decoded.budget, 8);
    }

    #[test]
    fn settings_survive_a_round_trip_through_serde() {
        let settings = Settings {
            tile_px: 480.0,
            autoplay: false,
            budget: 7,
            show_labels: false,
            sort: Sort { key: SortKey::Size, order: SortOrder::Descending },
            bookmarks: vec![PathBuf::from("photos"), PathBuf::from("clips")],
            sound: true,
            ..Settings::default()
        };
        let encoded = ron::to_string(&settings).unwrap();
        let decoded: Settings = ron::from_str(&encoded).unwrap();
        assert_eq!(decoded.tile_px, 480.0);
        assert_eq!(decoded.budget, 7);
        assert!(!decoded.autoplay);
        assert!(!decoded.show_labels);
        assert_eq!(decoded.bookmarks.len(), 2, "bookmarks have to survive a restart");
        assert!(decoded.sound, "and so does whether sound was wanted");
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
