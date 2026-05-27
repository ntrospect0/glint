// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Gallery widget — rotating inline image slideshow.
//!
//! Uses `ratatui-image` to pick the host terminal's image protocol
//! (iTerm2 / Kitty / Sixel, falling back to unicode halfblocks). Images
//! are downscaled to fit the pane but never upscaled past native size.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use image::{imageops::FilterType, DynamicImage};
use ratatui::{
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use ratatui_image::{picker::Picker, protocol::StatefulProtocol, Resize, StatefulImage};
use serde::Deserialize;

use crate::cache::ScopedCache;
use crate::theme::{ColorScheme, Theme};
use crate::ui::apply_title_row;

use super::{AppContext, EventResult, Widget};

#[derive(Debug, Clone, Deserialize)]
pub struct GalleryConfig {
    /// Image sources. Each entry is either a literal path or a simple glob:
    ///
    /// - `/abs/path/to/img.png` — single file.
    /// - `~/Pictures/*` — every image-format file in `~/Pictures/`.
    /// - `/path/*.jpg` — every `.jpg` in `/path/`.
    ///
    /// `~/` expands to `$HOME`. Globs are non-recursive (no `**`); for
    /// richer patterns add another entry. Failed loads skip with a warning.
    #[serde(default)]
    pub images: Vec<String>,

    /// Seconds between rotations. `0` starts paused (`p` toggles, `n`/`N` step).
    /// Floored to 1s when non-zero.
    #[serde(default = "default_rotation_secs")]
    pub rotation_secs: u64,

    /// Seconds between directory rescans for glob entries in `images`.
    /// `0` disables periodic rescans (initial scan still runs); literal
    /// paths in `images` are unaffected by this either way. Floored to
    /// 30s when non-zero so misconfigured intervals don't hammer the disk.
    #[serde(default = "default_rescan_interval_secs")]
    pub rescan_interval_secs: u64,

    /// Per-widget overrides layered on the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to `['g', 'a', 'l', 'r', 'y']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,
}

fn default_rotation_secs() -> u64 {
    10
}

fn default_rescan_interval_secs() -> u64 {
    300
}

impl Default for GalleryConfig {
    fn default() -> Self {
        Self {
            images: Vec::new(),
            rotation_secs: default_rotation_secs(),
            rescan_interval_secs: default_rescan_interval_secs(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

/// One slideshow slot. `protocol` is `None` for files that failed to decode;
/// `label` is the user-facing path for status display.
struct Slide {
    /// Resolved on-disk path. Used as the identity key during rescan
    /// diffs so a re-discovered image isn't re-decoded.
    source_path: PathBuf,
    label: String,
    /// `Mutex` because `StatefulImage::render` needs `&mut state` but the
    /// widget's `render` only has `&self`.
    protocol: Option<Mutex<Box<dyn StatefulProtocol>>>,
    /// Native pixel size, used to center the render rect with the right aspect.
    pixel_size: (u32, u32),
}

pub struct GalleryWidget {
    id: String,
    instance: String,
    display_name_cache: String,
    /// Loaded slides, populated incrementally by the background loader.
    /// We pay the one-time cost of image decode + protocol-encode off
    /// the main thread so app startup isn't blocked on disk I/O for
    /// large slideshows.
    slides: Arc<Mutex<Vec<Slide>>>,
    /// Total number of images currently resolved from the config. Updated
    /// by the rescan loop so the "Loading m/n images…" status line reflects
    /// the latest glob expansion, not just the startup snapshot.
    target_count: Arc<std::sync::atomic::AtomicUsize>,
    current: Arc<Mutex<GalleryState>>,
    rotation_interval: Duration,
    /// Cell size in pixels (width, height) as reported by the image
    /// picker. Used at render time to translate each image's pixel
    /// dimensions into terminal cells for horizontal centering.
    font_size: (u16, u16),
    /// Cached widget-level `[colors]` override so `set_app_theme` can
    /// rebuild the merged theme without re-reading TOML.
    colors_override: ColorScheme,
    app_theme: Arc<Theme>,
    theme: Theme,
    shortcut: Option<char>,
    shortcut_prefs: Vec<char>,
    /// Persistent thumb cache. The loader writes downscaled JPEGs here
    /// keyed by source-path hash; subsequent startups skip the source
    /// decode + resize and load the small thumb instead.
    cache: ScopedCache,
    /// Set on Drop / `apply_config` so the background loader exits its
    /// rescan loop instead of leaking after a config reload.
    loader_stop: Arc<AtomicBool>,
}

impl Drop for GalleryWidget {
    fn drop(&mut self) {
        self.loader_stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
struct GalleryState {
    /// Index into `slides`. Always valid when `slides` is non-empty;
    /// undefined (but unread) when `slides` is empty.
    idx: usize,
    paused: bool,
    last_rotation: Instant,
}

impl GalleryWidget {
    pub fn with_config(
        instance: String,
        config: GalleryConfig,
        app_theme: Arc<Theme>,
        cache: ScopedCache,
    ) -> Self {
        let id = if instance == "main" {
            "gallery".to_string()
        } else {
            format!("gallery@{instance}")
        };
        let display_name_cache = if instance == "main" {
            "Gallery".to_string()
        } else {
            format!("Gallery ({instance})")
        };
        let theme = app_theme.with_overrides(&config.colors);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['g', 'a', 'l', 'r', 'y']
        } else {
            config.shortcuts.clone()
        };

        // Detect the host terminal's image protocol once on the main
        // thread. The Picker query itself is cheap (a single ioctl for
        // font size + env-var sniff for protocol). Image decoding —
        // which is where the slow path lives — happens off-thread below.
        let mut picker = Picker::from_termios().unwrap_or_else(|err| {
            tracing::debug!(error = %err, "image picker probe failed, falling back to halfblocks");
            Picker::new((10, 20))
        });
        picker.guess_protocol();
        // In v2 the field is public (in v3+ it became a method); access
        // directly so the same syntax works regardless of the dep bump.
        let font_size = picker.font_size;

        // Floor non-zero rotation intervals at 1s. `0` is a sentinel for
        // "start paused" and is preserved as `Duration::ZERO`.
        let rotation_interval = if config.rotation_secs == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(config.rotation_secs.max(1))
        };
        let paused = rotation_interval.is_zero();
        let current = Arc::new(Mutex::new(GalleryState {
            idx: 0,
            paused,
            last_rotation: Instant::now(),
        }));
        let colors_override = config.colors.clone();

        // Resolve every config entry once, up front. `target_count` reflects
        // the post-expansion total so "Loading m/n images…" gives an honest
        // denominator from the first frame; it's shared with the loader so
        // periodic rescans can update it.
        let mut initial_paths = expand_all_patterns(&config.images);
        let total_matched = initial_paths.len();
        if initial_paths.len() > MAX_LOADED_SLIDES {
            tracing::info!(
                matched = total_matched,
                cap = MAX_LOADED_SLIDES,
                "gallery: matched paths exceed slide cap; truncating"
            );
            initial_paths.truncate(MAX_LOADED_SLIDES);
        }
        let target_count = Arc::new(std::sync::atomic::AtomicUsize::new(initial_paths.len()));

        // Floor non-zero rescan intervals at 30s; `0` disables the loop.
        let rescan_interval = if config.rescan_interval_secs == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(config.rescan_interval_secs.max(30))
        };

        // The background loader: decode every path once on startup, then
        // (when rescan is enabled and the config has at least one glob)
        // re-expand patterns periodically and reconcile the slide list.
        // Reads from the same Arc that `render` walks, so the first image
        // appears the moment it's decoded.
        let slides: Arc<Mutex<Vec<Slide>>> =
            Arc::new(Mutex::new(Vec::with_capacity(initial_paths.len())));
        let loader_stop = Arc::new(AtomicBool::new(false));

        if !config.images.is_empty() {
            let slides_for_loader = slides.clone();
            let target_for_loader = target_count.clone();
            let cache_for_loader = cache.clone();
            let patterns = config.images.clone();
            let stop = loader_stop.clone();
            let current_for_loader = current.clone();
            let has_globs = config.images.iter().any(|s| s.contains('*'));
            std::thread::Builder::new()
                .name("glint-gallery-loader".into())
                .spawn(move || {
                    let mut picker = picker;

                    // Initial pass — order matches expand_all_patterns,
                    // which preserves the user's config order.
                    for path in initial_paths {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        if let Some(slide) = build_slide(&mut picker, &cache_for_loader, &path) {
                            if let Ok(mut guard) = slides_for_loader.lock() {
                                guard.push(slide);
                            }
                        }
                    }

                    if rescan_interval.is_zero() || !has_globs {
                        return;
                    }
                    // Periodic rescan loop: wake on `rescan_interval`,
                    // re-expand patterns, add new images, drop vanished
                    // ones. Sleep is chunked into 1s slices so a config
                    // reload's stop flag is honoured promptly.
                    loop {
                        if !sleep_with_cancel(rescan_interval, &stop) {
                            return;
                        }
                        let mut next = expand_all_patterns(&patterns);
                        target_for_loader.store(next.len(), Ordering::Relaxed);
                        if next.len() > MAX_LOADED_SLIDES {
                            next.truncate(MAX_LOADED_SLIDES);
                        }
                        reconcile_slides(
                            &mut picker,
                            &cache_for_loader,
                            &slides_for_loader,
                            &current_for_loader,
                            &next,
                        );
                    }
                })
                .expect("spawn gallery loader thread");
        }

        Self {
            id,
            instance,
            display_name_cache,
            slides,
            target_count,
            current,
            rotation_interval,
            font_size,
            colors_override,
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
            cache,
            loader_stop,
        }
    }

    fn slide_count(&self) -> usize {
        self.slides
            .lock()
            .map(|g| g.len())
            .unwrap_or(0)
    }

    fn advance(&self, forward: bool) {
        let n = self.slide_count();
        if n == 0 {
            return;
        }
        let mut st = self.current.lock().expect("gallery state poisoned");
        st.idx = if forward {
            (st.idx + 1) % n
        } else {
            (st.idx + n - 1) % n
        };
        st.last_rotation = Instant::now();
    }
}

/// Carve a horizontally-centered sub-rect inside `area` that matches
/// the image's aspect ratio in *terminal cells*. Vertical alignment
/// stays anchored to the top of `area` (matching ratatui-image's
/// default placement); only the x position shifts.
///
/// `image_px` is the source image's natural size in pixels;
/// `font_size_px` is the terminal's cell size in pixels (width,
/// height) — both reported by `ratatui-image::picker::Picker`. We
/// convert image px → cell-equivalent units, then ask: in this pane's
/// aspect ratio (in cells), is the image bound by the width or the
/// height of the area?
///
///   - Width-bound: image stretches across the full pane width; no
///     horizontal offset needed.
///   - Height-bound: image fills vertically and leaves space on
///     either side; we split that space evenly to center the column.
fn centered_horizontal_area(
    area: Rect,
    image_px: (u32, u32),
    font_size_px: (u16, u16),
) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let (img_w, img_h) = (image_px.0 as f32, image_px.1 as f32);
    let (cell_w, cell_h) = (font_size_px.0 as f32, font_size_px.1 as f32);
    if img_w <= 0.0 || img_h <= 0.0 || cell_w <= 0.0 || cell_h <= 0.0 {
        return area;
    }
    // Image dimensions expressed in cell units (not yet rounded). The
    // pane's aspect ratio in the same units is just area.width /
    // area.height — both are already in cells.
    let img_cells_w = img_w / cell_w;
    let img_cells_h = img_h / cell_h;
    let area_w = area.width as f32;
    let area_h = area.height as f32;
    let img_aspect = img_cells_w / img_cells_h;
    let area_aspect = area_w / area_h;
    if img_aspect >= area_aspect {
        // Width-bound: full pane width, no horizontal offset.
        return area;
    }
    // Height-bound: scale so img height = area height, then derive width.
    let scale = area_h / img_cells_h;
    let target_w = (img_cells_w * scale).round() as u16;
    let mut target_w = target_w.min(area.width).max(1);
    // Centering correction: integer division `(area.width - target_w) /
    // 2` rounds the offset toward zero, so any odd gap (e.g. area=50,
    // target=47 → gap=3) systematically biases the image one cell to
    // the left of pane center. We'd rather lose a single cell of
    // width than render every height-bound image off-centre, so when
    // the natural target_w produces an odd gap we shrink it by 1.
    // Shrinking (vs. growing) avoids any horizontal stretching of the
    // source image's aspect ratio.
    if target_w >= 2 && (area.width - target_w) % 2 != 0 {
        target_w -= 1;
    }
    let x_offset = (area.width - target_w) / 2;
    Rect {
        x: area.x + x_offset,
        y: area.y,
        width: target_w,
        height: area.height,
    }
}

#[async_trait]
impl Widget for GalleryWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        "gallery"
    }

    fn instance(&self) -> &str {
        &self.instance
    }

    fn display_name(&self) -> &str {
        &self.display_name_cache
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        // Auto-rotate when not paused, when we have ≥2 slides loaded so
        // far, and when enough wall time has elapsed since the last
        // advance. `slide_count` reads through the shared loader vec, so
        // rotation kicks in incrementally as more images come online.
        if self.rotation_interval.is_zero() {
            return Ok(());
        }
        let n = self.slide_count();
        if n < 2 {
            return Ok(());
        }
        let mut st = self.current.lock().expect("gallery state poisoned");
        if st.paused {
            return Ok(());
        }
        if st.last_rotation.elapsed() >= self.rotation_interval {
            st.idx = (st.idx + 1) % n;
            st.last_rotation = Instant::now();
        }
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let title_base = if self.instance == "main" {
            "Gallery".to_string()
        } else {
            format!("Gallery ({})", self.instance)
        };
        // Metadata snapshot — show "{m}/{n} images" while loading or
        // just "{n} images" when fully loaded. None when there are
        // no images configured.
        let loaded = self.slides.lock().expect("gallery slides poisoned").len();
        let target = self.target_count.load(Ordering::Relaxed);
        let metadata = if target == 0 {
            None
        } else if loaded < target {
            Some(format!("{loaded}/{target} images"))
        } else {
            Some(format!("{target} images"))
        };
        let block = apply_title_row(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(focused)),
            focused,
            &title_base,
            metadata.as_deref(),
            self.shortcut,
            &self.theme,
            area.width,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Snapshot once per render so all status-line variants agree on the
        // same denominator even if the rescan loop swaps it underneath us.
        let target_total = self.target_count.load(Ordering::Relaxed);

        // No images configured at all — easy to spot guidance message.
        if target_total == 0 {
            let msg = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "No images configured.",
                    self.theme.text_brilliant,
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Add `images = [\"~/Pictures/foo.png\", ...]` to gallery.toml.",
                    self.theme.text_dim,
                )),
            ])
            .alignment(Alignment::Center);
            frame.render_widget(msg, inner);
            return;
        }

        // Reserve one row at the bottom for the status line ("3/7 · paused").
        let (image_area, status_area) = if inner.height >= 2 {
            (
                Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: inner.height - 1,
                },
                Some(Rect {
                    x: inner.x,
                    y: inner.y + inner.height - 1,
                    width: inner.width,
                    height: 1,
                }),
            )
        } else {
            (inner, None)
        };

        // Snapshot just the bits we need from state so the StatefulImage
        // mutex doesn't deadlock with the status-line render below.
        let (idx, paused) = {
            let st = self.current.lock().expect("gallery state poisoned");
            (st.idx, st.paused)
        };

        // Hold the slides lock for as little as possible — clone what
        // render needs (pixel_size, label) and grab a fresh handle to
        // the protocol mutex by re-locking only when we actually render.
        let snapshot = {
            let guard = self.slides.lock().expect("gallery slides poisoned");
            let loaded = guard.len();
            let slide = guard.get(idx).map(|s| (s.label.clone(), s.pixel_size));
            (loaded, slide)
        };
        let (loaded_count, current_slide) = snapshot;

        // Still loading the first image — show a friendly placeholder so
        // the user sees the widget is alive while the background loader
        // catches up.
        if loaded_count == 0 {
            let msg = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("Loading 0/{target_total}…"),
                    self.theme.text_dim,
                )),
            ])
            .alignment(Alignment::Center);
            frame.render_widget(msg, image_area);
            return;
        }

        if let Some((label, pixel_size)) = current_slide {
            // Re-lock just to render the protocol's stateful widget,
            // then release.
            let centered = centered_horizontal_area(image_area, pixel_size, self.font_size);
            {
                let mut guard = self.slides.lock().expect("gallery slides poisoned");
                if let Some(slide) = guard.get_mut(idx) {
                    if let Some(proto_mutex) = slide.protocol.as_ref() {
                        let mut proto =
                            proto_mutex.lock().expect("gallery protocol poisoned");
                        let widget = StatefulImage::new(None).resize(Resize::Fit(None));
                        frame.render_stateful_widget(widget, centered, &mut *proto);
                    }
                }
            }

            if let Some(status_area) = status_area {
                let mut line = format!("{}/{}  ·  {}", idx + 1, target_total, label);
                if loaded_count < target_total {
                    line.push_str(&format!("  ·  loading {}/{}", loaded_count, target_total));
                }
                if paused {
                    line.push_str("  ·  paused");
                } else if !self.rotation_interval.is_zero() {
                    line.push_str(&format!(
                        "  ·  {}s rotation",
                        self.rotation_interval.as_secs()
                    ));
                }
                frame.render_widget(
                    Paragraph::new(Span::styled(line, self.theme.text_dim))
                        .alignment(Alignment::Center),
                    status_area,
                );
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // Ctrl/Alt are reserved for app-wide commands; SHIFT is allowed so
        // shifted non-letter chars (`?`, `%`, `$`, …) remain available, but
        // uppercase ASCII letters are off-limits because `Shift+<letter>`
        // is the app-wide focus-jump dispatcher.
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_uppercase() {
                return EventResult::Ignored;
            }
        }
        match key.code {
            // `p` toggles pause. When `rotation_secs = 0` was set in
            // config we start paused; the user can hit `p` to begin a
            // timer-driven slideshow at the configured cadence — which
            // is zero, so we treat it as "advance only on `n`". Keeping
            // the toggle anyway in case the user reloads config.
            KeyCode::Char('p') => {
                let mut st = self.current.lock().expect("gallery state poisoned");
                st.paused = !st.paused;
                if !st.paused {
                    // Reset the timer so the next advance is a full
                    // rotation_secs away, not whatever residual time
                    // had accumulated while paused.
                    st.last_rotation = Instant::now();
                }
                EventResult::Handled
            }
            // Manual cycling — handy when paused or when the user wants
            // to skip ahead without waiting for the timer. Previous is
            // bound to ← / `h` only; Shift+N would collide with the
            // global focus-jump shortcut.
            KeyCode::Char('n') | KeyCode::Right | KeyCode::Char('l') => {
                self.advance(true);
                EventResult::Handled
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.advance(false);
                EventResult::Handled
            }
            // ↑ / ↓ tune the auto-rotation cadence on the fly. Down
            // floors at 1 second (anything less is just a strobe);
            // use `p` if you actually want to stop rotation.
            KeyCode::Up => {
                let secs = self.rotation_interval.as_secs().max(1).saturating_add(1);
                self.rotation_interval = Duration::from_secs(secs);
                EventResult::Handled
            }
            KeyCode::Down => {
                let secs = self.rotation_interval.as_secs().saturating_sub(1).max(1);
                self.rotation_interval = Duration::from_secs(secs);
                EventResult::Handled
            }
            _ => EventResult::Ignored,
        }
    }

    fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> {
        Ok(false)
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("p", "pause / resume slideshow"),
            ("n / →", "next image"),
            ("h / ←", "previous image"),
            ("↑ / ↓", "rotation interval +1s / -1s (floor 1s)"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        let labels: Vec<String> = self
            .slides
            .lock()
            .map(|g| g.iter().map(|s| s.label.clone()).collect())
            .unwrap_or_default();
        serde_json::json!({
            "images": labels,
            "rotation_secs": self.rotation_interval.as_secs(),
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: GalleryConfig =
            serde_json::from_value(config).context("invalid gallery config payload")?;
        let app_theme = self.app_theme.clone();
        let cache = self.cache.clone();
        let instance = self.instance.clone();
        *self = Self::with_config(instance, new_config, app_theme, cache);
        Ok(())
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.with_overrides(&self.colors_override);
        self.app_theme = theme;
    }

    fn shortcut_preferences(&self) -> &[char] {
        &self.shortcut_prefs
    }

    fn set_shortcut(&mut self, shortcut: Option<char>) {
        self.shortcut = shortcut;
    }

    fn shortcut(&self) -> Option<char> {
        self.shortcut
    }
}

/// Upper bound on each image's long side after pre-resize. Sized to
/// fit typical TUI panes without upscaling: a 200-col terminal × ~60%
/// pane width × ~10 px/cell ≈ 1200 px. ratatui_image's `Resize::Fit`
/// won't upscale beyond the source, so panes that paint wider than
/// this render at source size and leave a gap; bump it up if you run
/// hi-DPI fonts (cell width 15–20 px) with very wide gallery panes.
/// Each step down quadratically reduces the protocol's cached source
/// (1280² is 64% of 1600²).
const MAX_IMAGE_DIM: u32 = 1280;

/// Upper bound on concurrently-decoded slides held in RAM. Each one
/// keeps a ratatui_image `StatefulProtocol` whose internal pixel cache
/// scales with `MAX_IMAGE_DIM²`, so the per-slide cost is bounded but
/// non-trivial (~3 MB on landscape sources). Slideshows with more
/// matched files than this stop loading at the cap; the rotation walks
/// only the loaded subset until a future change adds rolling
/// pre-decode. Documented in the wizard help text.
const MAX_LOADED_SLIDES: usize = 60;

/// If either side of `img` exceeds `max_dim` pixels, return an
/// aspect-correct downscaled copy. Otherwise return the input
/// unchanged. `Triangle` (bilinear) is a good balance — faster than
/// Lanczos3 by 3-5× while producing visibly indistinguishable output
/// at terminal-grid resolution.
fn downscale_to_max_dim(img: DynamicImage, max_dim: u32) -> DynamicImage {
    let long_side = img.width().max(img.height());
    if long_side <= max_dim {
        return img;
    }
    // `DynamicImage::resize` fits within (nwidth, nheight) preserving
    // aspect ratio; passing max_dim for both gives an aspect-correct
    // shrink to the long side.
    img.resize(max_dim, max_dim, FilterType::Triangle)
}

/// Expand a leading `~/` to `$HOME`. Everything else passes through.
fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

/// Image-format extensions recognised by directory expansion. Match the
/// formats `image` crate decodes by default; cased-insensitively at match
/// time so `IMG_1234.JPG` and `cover.PNG` both qualify.
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "ico",
];

fn is_image_file(path: &Path) -> bool {
    let Some(ext) = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
    else {
        return false;
    };
    IMAGE_EXTENSIONS.iter().any(|e| *e == ext)
}

/// Cheap glob matcher for the basename portion of a `images` entry.
/// Supports `*` standalone, `*<suffix>` (e.g. `*.jpg`), and `<prefix>*`
/// (e.g. `cover_*`). Anything else is treated as a literal filename.
fn match_basename(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        if !suffix.contains('*') {
            return name.ends_with(suffix);
        }
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        if !prefix.contains('*') {
            return name.starts_with(prefix);
        }
    }
    name == pattern
}

/// Expand one `images` entry into the list of concrete file paths it
/// resolves to. Literal paths (no `*`) round-trip as a single-element vec;
/// glob entries enumerate their parent directory and filter by image
/// extension. Returns an empty vec when a directory is unreadable so
/// failures degrade gracefully.
fn expand_pattern(raw: &str) -> Vec<PathBuf> {
    let resolved = expand_tilde(raw);
    let as_str = resolved.to_string_lossy().into_owned();
    if !as_str.contains('*') {
        return vec![resolved];
    }

    // Split into "directory" / "basename pattern" at the final separator.
    let (dir_part, basename) = match as_str.rsplit_once('/') {
        Some((d, n)) => (PathBuf::from(d), n.to_string()),
        None => (PathBuf::from("."), as_str),
    };

    let rd = match std::fs::read_dir(&dir_part) {
        Ok(rd) => rd,
        Err(err) => {
            tracing::warn!(
                dir = %dir_part.display(),
                error = %err,
                "gallery: directory unreadable for glob"
            );
            return Vec::new();
        }
    };

    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !match_basename(&basename, name) {
            continue;
        }
        // Untyped `/dir/*` should only catch image files. Typed patterns
        // (`/dir/*.jpg`) already self-filter via the basename match, so the
        // extension check is a no-op there.
        if !is_image_file(&p) {
            continue;
        }
        matches.push(p);
    }
    // Alphabetical order so the slideshow has a stable cycle even when the
    // filesystem returns entries in directory order.
    matches.sort();
    matches
}

/// Expand every config entry, preserving the user's outer ordering and
/// deduplicating across patterns (the same file matched by two globs
/// renders once).
fn expand_all_patterns(patterns: &[String]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for raw in patterns {
        for p in expand_pattern(raw) {
            if seen.insert(p.clone()) {
                out.push(p);
            }
        }
    }
    out
}

/// Read + decode an image file. Wraps the `image` crate's reader so the
/// error chain (`failed to open` / `unsupported format` / etc.) reaches
/// the tracing warning intact.
fn load_image(path: &std::path::Path) -> Result<DynamicImage> {
    let reader = image::ImageReader::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("sniff format of {}", path.display()))?;
    let img = reader
        .decode()
        .with_context(|| format!("decode {}", path.display()))?;
    Ok(img)
}

/// Cache-aware thumb loader. Strategy:
///
/// 1. Build a stable cache key from the canonical absolute path (falls back
///    to the raw path string when canonicalisation fails — happens when the
///    image was deleted mid-run).
/// 2. If the cache holds a thumb whose `stored_at` is newer than the source
///    file's mtime, decode just the thumb (JPEG, ~10x faster than full
///    source) and return.
/// 3. Otherwise re-decode + downscale the source and write the result back
///    to the cache.
///
/// JPEG was picked over PNG because thumbs at 1600 px long-side are visually
/// indistinguishable at terminal-grid resolution and the size difference is
/// 3-5×. Files land at `~/.cache/glint/gallery/<instance>/thumb-<sha>.bin`.
fn load_thumb(cache: &ScopedCache, path: &Path) -> Result<DynamicImage> {
    let cache_key = thumb_cache_key(path);
    let src_mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());

    if let Some(entry) = cache.load_bytes(&cache_key) {
        let stored: std::time::SystemTime = entry.stored_at.into();
        // When the source mtime is unreadable (file moved / removed / on a
        // disconnected drive) the cache is the best we have — serve it
        // rather than erroring. Otherwise the cache is fresh iff it was
        // stored at or after the source's last modification.
        let fresh = src_mtime.map_or(true, |m| stored >= m);
        if fresh {
            match image::load_from_memory(&entry.value) {
                // Old caches were encoded at a larger MAX_IMAGE_DIM; re-apply
                // the downscale on read so a stale cache entry doesn't reach
                // the protocol at the previous (larger) size.
                Ok(img) => return Ok(downscale_to_max_dim(img, MAX_IMAGE_DIM)),
                Err(err) => {
                    // Stored bytes won't decode — drop them and fall through
                    // to a fresh source decode + re-encode.
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "gallery: cached thumb undecodable, refreshing"
                    );
                }
            }
        }
    }

    let img = downscale_to_max_dim(load_image(path)?, MAX_IMAGE_DIM);

    // Encode the thumb as JPEG so the cached payload stays small. JPEG can't
    // hold an alpha channel; flatten to RGB before encoding so RGBA sources
    // (PNG, etc.) don't fail.
    let rgb = img.to_rgb8();
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    if let Err(err) = DynamicImage::ImageRgb8(rgb).write_to(
        &mut std::io::Cursor::new(&mut buf),
        image::ImageFormat::Jpeg,
    ) {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "gallery: thumb encode failed; serving uncached"
        );
        return Ok(img);
    }
    if let Err(err) = cache.store_bytes(&cache_key, &buf) {
        tracing::warn!(error = %err, "gallery thumb cache store failed");
    }
    Ok(img)
}

/// Stable cache key derived from the source path. The path is hashed as-is
/// (no symlink resolution) so a cache lookup gives the same answer whether
/// or not the source file currently exists — important when a previously
/// indexed image was moved or deleted between runs.
fn thumb_cache_key(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let mut key = String::with_capacity(22);
    key.push_str("thumb-");
    for b in &digest[..8] {
        use std::fmt::Write;
        let _ = write!(key, "{b:02x}");
    }
    key
}

/// Decode `path` (cache-aware) and bundle the result into a `Slide`.
/// Returns `None` on decode failure; the loader logs and skips.
fn build_slide(
    picker: &mut Picker,
    cache: &ScopedCache,
    path: &Path,
) -> Option<Slide> {
    let label = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match load_thumb(cache, path) {
        Ok(img) => {
            let pixel_size = (img.width(), img.height());
            let protocol = picker.new_resize_protocol(img);
            Some(Slide {
                source_path: path.to_path_buf(),
                label,
                protocol: Some(Mutex::new(protocol)),
                pixel_size,
            })
        }
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "gallery: failed to load image, skipping"
            );
            None
        }
    }
}

/// Sleep for `total`, but check `stop` once per second so a cancellation
/// signal (typically from a config reload) is honoured promptly. Returns
/// `false` when cancelled, `true` when the full duration elapsed.
fn sleep_with_cancel(total: Duration, stop: &AtomicBool) -> bool {
    let tick = Duration::from_secs(1);
    let mut remaining = total;
    while !remaining.is_zero() {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let step = if remaining > tick { tick } else { remaining };
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    !stop.load(Ordering::Relaxed)
}

/// Reconcile the slide list against a freshly-expanded `next` path set.
///
/// - Newly discovered paths are decoded (via `build_slide`, which routes
///   through the persistent cache) and appended at the end. They appear in
///   the slideshow without further user action.
/// - Paths that vanished from the expansion are removed.
/// - Ordering otherwise follows `next` so the user's pattern order stays
///   authoritative across rescans.
/// - The currently-visible slide's identity (by `source_path`) is preserved
///   when possible; if it was removed, the index snaps to 0.
fn reconcile_slides(
    picker: &mut Picker,
    cache: &ScopedCache,
    slides: &Arc<Mutex<Vec<Slide>>>,
    current: &Arc<Mutex<GalleryState>>,
    next: &[PathBuf],
) {
    // Build new slides outside the lock so decode work doesn't stall render.
    let existing_paths: std::collections::HashSet<PathBuf> = {
        let guard = match slides.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        guard.iter().map(|s| s.source_path.clone()).collect()
    };
    let mut additions: Vec<Slide> = Vec::new();
    for path in next.iter() {
        if existing_paths.contains(path) {
            continue;
        }
        if let Some(slide) = build_slide(picker, cache, path) {
            additions.push(slide);
        }
    }
    let next_set: std::collections::HashSet<&Path> = next.iter().map(|p| p.as_path()).collect();

    // Apply the diff atomically: reorder + drop removed + append new.
    let visible_path = {
        let st = match current.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let guard = match slides.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        guard.get(st.idx).map(|s| s.source_path.clone())
    };

    let mut guard = match slides.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    // Drop slides whose paths are no longer matched by any pattern.
    guard.retain(|s| next_set.contains(s.source_path.as_path()));
    // Reorder retained slides to match `next`'s order.
    guard.sort_by_key(|s| {
        next.iter()
            .position(|p| p == &s.source_path)
            .unwrap_or(usize::MAX)
    });
    guard.extend(additions);
    // Re-locate the previously-visible slide and snap the index back to it
    // (or 0 when it's gone).
    let new_idx = visible_path
        .as_ref()
        .and_then(|p| guard.iter().position(|s| &s.source_path == p))
        .unwrap_or(0);
    drop(guard);

    if let Ok(mut st) = current.lock() {
        st.idx = new_idx;
    }
}

pub const KIND: &str = "gallery";

/// Wizard descriptor. `images` accepts a comma-separated list of literal
/// paths and `/dir/*` globs; rotation + rescan are flat Number fields.
/// Default field-by-field TOML renderer handles emission.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{Separator, WizardDescriptor, WizardField, WizardFieldKind};
    WizardDescriptor {
        display_name: "Gallery",
        blurb: "Rotating inline image slideshow with optional periodic \
                directory rescan. Decoded thumbnails are cached under \
                ~/.cache/glint/gallery/ so subsequent launches are fast.",
        load_from_toml: None,
        render_toml: None,
        fields: vec![
            WizardField {
                key: "images",
                label: "Image sources (comma-separated)",
                help: "Each entry is a literal path (\"~/Pictures/cover.png\") \
                       or a simple glob (\"~/Pictures/*\", \"/photos/*.jpg\"). \
                       `~/` expands to $HOME. Failed loads skip with a \
                       glint.log warning. Patterns that resolve to more than \
                       60 files are truncated — narrow the glob if you want \
                       different images in rotation.",
                required: false,
                kind: WizardFieldKind::TextList {
                    default: Vec::new(),
                    separator: Separator::Comma,
                },
                validate: None,
            },
            WizardField {
                key: "rotation_secs",
                label: "Rotation interval (seconds)",
                help: "Seconds between slides. `0` starts the slideshow \
                       paused — press `p` in the widget to play / pause, \
                       `n`/`N` to step manually.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(10.0),
                    range: Some((0.0, 3600.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "rescan_interval_secs",
                label: "Directory rescan interval (seconds)",
                help: "How often the loader re-walks glob patterns to pick \
                       up newly-added images. `0` disables periodic rescans \
                       (the initial expansion still runs); literal paths in \
                       `images` are unaffected either way. Floored to 30s \
                       when non-zero.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(300.0),
                    range: Some((0.0, 86400.0)),
                    integer: true,
                },
                validate: None,
            },
        ],
    }
}

pub fn build(ctx: &super::WidgetCtx) -> Box<dyn super::Widget> {
    let cfg: GalleryConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(GalleryWidget::with_config(
        ctx.instance.clone(),
        cfg,
        ctx.theme.clone(),
        ctx.cache.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_replaces_leading_tilde() {
        let home = dirs::home_dir().expect("home dir for test");
        assert_eq!(expand_tilde("~/Pictures/x.png"), home.join("Pictures/x.png"));
    }

    #[test]
    fn expand_tilde_passes_through_absolute_paths() {
        assert_eq!(
            expand_tilde("/tmp/x.png"),
            PathBuf::from("/tmp/x.png")
        );
    }

    #[test]
    fn default_rotation_is_ten_seconds() {
        let cfg = GalleryConfig::default();
        assert_eq!(cfg.rotation_secs, 10);
        assert!(cfg.images.is_empty());
    }

    #[test]
    fn zero_rotation_starts_paused() {
        let cfg = GalleryConfig {
            rotation_secs: 0,
            ..GalleryConfig::default()
        };
        let widget = GalleryWidget::with_config(
            "main".to_string(),
            cfg,
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        );
        let st = widget.current.lock().unwrap();
        assert!(st.paused);
        assert!(widget.rotation_interval.is_zero());
    }

    #[test]
    fn non_zero_rotation_starts_running() {
        let cfg = GalleryConfig {
            rotation_secs: 5,
            ..GalleryConfig::default()
        };
        let widget = GalleryWidget::with_config(
            "main".to_string(),
            cfg,
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        );
        let st = widget.current.lock().unwrap();
        assert!(!st.paused);
        assert_eq!(widget.rotation_interval, Duration::from_secs(5));
    }

    #[test]
    fn id_includes_instance_suffix() {
        let main = GalleryWidget::with_config(
            "main".into(),
            GalleryConfig::default(),
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        );
        assert_eq!(main.id(), "gallery");
        let inst = GalleryWidget::with_config(
            "kids".into(),
            GalleryConfig::default(),
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        );
        assert_eq!(inst.id(), "gallery@kids");
        assert_eq!(inst.display_name(), "Gallery (kids)");
    }

    #[test]
    fn shortcut_preferences_default_to_g_a_l_r_y() {
        let w = GalleryWidget::with_config(
            "main".into(),
            GalleryConfig::default(),
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        );
        assert_eq!(w.shortcut_preferences(), &['g', 'a', 'l', 'r', 'y']);
    }

    #[test]
    fn is_image_file_is_case_insensitive() {
        assert!(is_image_file(Path::new("/tmp/a.JPG")));
        assert!(is_image_file(Path::new("/tmp/a.png")));
        assert!(is_image_file(Path::new("/tmp/a.WebP")));
        assert!(!is_image_file(Path::new("/tmp/a.txt")));
        assert!(!is_image_file(Path::new("/tmp/a")));
    }

    #[test]
    fn match_basename_handles_star_suffix_prefix_and_literal() {
        assert!(match_basename("*", "anything.jpg"));
        assert!(match_basename("*.jpg", "cover.jpg"));
        assert!(!match_basename("*.jpg", "cover.png"));
        assert!(match_basename("cover_*", "cover_2024.png"));
        assert!(!match_basename("cover_*", "anything.png"));
        assert!(match_basename("exact.png", "exact.png"));
        assert!(!match_basename("exact.png", "different.png"));
    }

    #[test]
    fn expand_pattern_returns_literal_unchanged() {
        let out = expand_pattern("/some/where/img.png");
        assert_eq!(out, vec![PathBuf::from("/some/where/img.png")]);
    }

    #[test]
    fn expand_pattern_globs_image_files_in_directory() {
        // Write three files into a fresh dir: two images + one non-image.
        // The non-image must be filtered out.
        use image::{Rgb, RgbImage};
        let dir = std::env::temp_dir().join(format!(
            "glint-gallery-glob-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let png_a = dir.join("a.png");
        let png_b = dir.join("b.png");
        let txt = dir.join("readme.txt");
        RgbImage::from_pixel(8, 8, Rgb([0, 0, 0])).save(&png_a).unwrap();
        RgbImage::from_pixel(8, 8, Rgb([0, 0, 0])).save(&png_b).unwrap();
        std::fs::write(&txt, b"hi").unwrap();

        let mut out = expand_pattern(&format!("{}/*", dir.display()));
        out.sort();
        assert_eq!(out, vec![png_a.clone(), png_b.clone()]);

        // Extension-typed glob: only one match.
        let out2 = expand_pattern(&format!("{}/*.png", dir.display()));
        assert_eq!(out2.len(), 2);

        let out3 = expand_pattern(&format!("{}/*.jpg", dir.display()));
        assert!(out3.is_empty());

        // Cleanup.
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn expand_pattern_handles_missing_dir() {
        let out = expand_pattern("/this/directory/does/not/exist/*");
        assert!(out.is_empty());
    }

    #[test]
    fn expand_all_patterns_dedups_across_entries() {
        let dir = std::env::temp_dir().join(format!(
            "glint-gallery-dedup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.png");
        image::RgbImage::from_pixel(8, 8, image::Rgb([0, 0, 0]))
            .save(&p)
            .unwrap();

        let patterns = vec![
            p.to_string_lossy().into_owned(),
            format!("{}/*", dir.display()),
        ];
        let out = expand_all_patterns(&patterns);
        assert_eq!(out, vec![p.clone()], "same file matched twice should appear once");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_thumb_round_trips_through_cache() {
        // Write a tiny PNG to a temp file, load it through load_thumb,
        // then load again and confirm the second call doesn't re-touch the
        // source file (i.e. removing the source between calls still works).
        use image::{Rgb, RgbImage};
        let mut tmp = std::env::temp_dir();
        tmp.push(format!(
            "glint-gallery-test-{}-{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let img = RgbImage::from_pixel(64, 32, Rgb([200, 100, 50]));
        img.save(&tmp).expect("write source image");

        let cache = ScopedCache::ephemeral();
        let first = load_thumb(&cache, &tmp).expect("first decode");
        assert_eq!((first.width(), first.height()), (64, 32));

        // Cache should now hold a thumb. Delete the source; a second
        // load_thumb call must still succeed via the cached bytes.
        std::fs::remove_file(&tmp).expect("remove source");
        let second = load_thumb(&cache, &tmp).expect("second decode (cached)");
        assert_eq!((second.width(), second.height()), (64, 32));
    }

    #[test]
    fn centered_area_unchanged_when_image_is_wider_than_pane() {
        // Image aspect (in cells) wider than pane → width-bound, no
        // horizontal offset. Use generous cell sizes so we control the
        // math without relying on a probe.
        let area = Rect::new(0, 0, 30, 20);
        // 1600×800 image at 10×10 cells = 160×80 cell-equivalents; pane
        // is 30×20 → 3:2 aspect; image is 2:1 (wider).
        let out = centered_horizontal_area(area, (1600, 800), (10, 10));
        assert_eq!(out, area);
    }

    #[test]
    fn centered_area_shrinks_and_offsets_for_portrait_image() {
        // 800×1600 image at 10×10 cells: cell-equivalent 80×160 → 1:2
        // aspect. Pane 30×20 → 3:2 (much wider). Image is height-bound;
        // its width-after-fit in cells = 20 * (80/160) = 10. Offset:
        // (30 - 10) / 2 = 10.
        let area = Rect::new(0, 0, 30, 20);
        let out = centered_horizontal_area(area, (800, 1600), (10, 10));
        assert_eq!(out.width, 10);
        assert_eq!(out.x, 10);
        assert_eq!(out.height, 20);
        assert_eq!(out.y, 0);
    }

    #[test]
    fn centered_area_handles_zero_area_gracefully() {
        let zero = Rect::new(5, 7, 0, 0);
        assert_eq!(centered_horizontal_area(zero, (100, 100), (10, 10)), zero);
    }

    #[test]
    fn centered_area_yields_even_gap_for_perfect_centering() {
        // Construct a height-bound image whose natural target_w would
        // be an odd number of cells against an even pane width — the
        // classic case that pre-fix biased one cell left of centre.
        // 700×1000 at 10×10 cells = 70×100 cell-equivalents → aspect
        // 0.7. Pane 50×20 → height-bound. width-after-fit =
        // 20 * (70/100) = 14 (even, no shrink). gap = 36, x = 18.
        let area = Rect::new(0, 0, 50, 20);
        let out = centered_horizontal_area(area, (700, 1000), (10, 10));
        let left = out.x - area.x;
        let right = area.width - out.width - left;
        assert_eq!(
            left, right,
            "even-gap case must be symmetrically centred: {out:?} in {area:?}"
        );
    }

    #[test]
    fn centered_area_shrinks_one_cell_when_natural_gap_is_odd() {
        // 750×1000 at 10×10 = 75×100 cell-equivalents → aspect 0.75.
        // Pane 50×20 height-bound, raw target_w = 20 * 0.75 = 15
        // (odd) → gap 35 (odd) → would left-bias by 1 cell. We
        // shrink target_w to 14 so gap = 36 (even) → perfect centering.
        let area = Rect::new(0, 0, 50, 20);
        let out = centered_horizontal_area(area, (750, 1000), (10, 10));
        let left = out.x - area.x;
        let right = area.width - out.width - left;
        assert_eq!(left, right, "odd-gap shrink must restore symmetry");
        assert_eq!(out.width, 14, "should have shrunk by 1 cell");
    }

    #[test]
    fn downscale_below_cap_returns_input_dimensions() {
        // 800×600 with cap 1600 should be unchanged.
        let img = DynamicImage::new_rgba8(800, 600);
        let out = downscale_to_max_dim(img, 1600);
        assert_eq!((out.width(), out.height()), (800, 600));
    }

    #[test]
    fn downscale_above_cap_shrinks_aspect_correct() {
        // 4000×3000 (4:3) capped at 1600 → 1600×1200.
        let img = DynamicImage::new_rgba8(4000, 3000);
        let out = downscale_to_max_dim(img, 1600);
        assert_eq!((out.width(), out.height()), (1600, 1200));
    }

    #[test]
    fn missing_image_logs_warning_and_is_skipped() {
        // Path that definitely doesn't exist. Constructor shouldn't
        // panic; the slide just gets dropped from the rotation.
        let cfg = GalleryConfig {
            images: vec!["/tmp/glint-gallery-does-not-exist-12345.png".to_string()],
            rotation_secs: 0,
            ..GalleryConfig::default()
        };
        let widget = GalleryWidget::with_config(
            "main".into(),
            cfg,
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        );
        assert_eq!(widget.slide_count(), 0);
    }
}
