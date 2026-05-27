//! Gallery widget — rotating inline image slideshow.
//!
//! Renders one image at a time, auto-scaled to fit the pane, and rotates
//! through a configured list every `rotation_secs` seconds. `p` pauses /
//! resumes; `n` and `N` step manually. When `rotation_secs = 0` the
//! slideshow starts paused — useful when the user wants to flip through
//! manually instead of cycling on a timer.
//!
//! Powered by `ratatui-image`, which auto-detects the host terminal's
//! image protocol (iTerm2 inline, Kitty graphics, Sixel, or unicode
//! halfblocks as a last-resort fallback). Images larger than the pane
//! get downscaled to fit; smaller ones aren't upscaled past their native
//! size. Decode failures are logged to `glint.log` via `tracing::warn`
//! and the offending entry is skipped silently.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
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

use crate::theme::{ColorScheme, Theme};
use crate::ui::decorated_title_line;

use super::{AppContext, EventResult, Widget};

#[derive(Debug, Clone, Deserialize)]
pub struct GalleryConfig {
    /// Image file paths. `~` at the start expands to `$HOME`. Anything
    /// else is taken verbatim. Relative paths resolve against the
    /// process's CWD at startup. Failed loads are skipped with a warning
    /// in `glint.log`.
    #[serde(default)]
    pub images: Vec<String>,

    /// Seconds between automatic rotations. `0` = paused from the start;
    /// the user can hit `p` to start rotating or `n`/`N` to step
    /// manually. Floor of 1 second when non-zero to keep the dashboard
    /// from re-rendering every tick.
    #[serde(default = "default_rotation_secs")]
    pub rotation_secs: u64,

    /// Per-widget style overrides layered on the active app scheme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// Prioritized `Shift+<letter>` shortcut preferences. Leave empty
    /// for the built-in default `['g', 'a', 'l', 'r', 'y']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,
}

fn default_rotation_secs() -> u64 {
    10
}

impl Default for GalleryConfig {
    fn default() -> Self {
        Self {
            images: Vec::new(),
            rotation_secs: default_rotation_secs(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

/// One slot in the slideshow — protocol-cached image data or `None` if
/// the underlying file couldn't be decoded. The label is the original
/// path (or filename) used for status/error display.
struct Slide {
    label: String,
    /// Boxed trait object — `ratatui-image` v2 returns `Box<dyn
    /// StatefulProtocol>` so the concrete encoding (sixel / kitty /
    /// iterm2 / halfblocks) is hidden behind the trait. Wrapped in
    /// `Mutex` because `StatefulImage` needs `&mut` to the state, but
    /// the widget's `render` method only has `&self`.
    protocol: Option<Mutex<Box<dyn StatefulProtocol>>>,
    /// Original image dimensions in pixels (width, height). Captured at
    /// decode time and used to compute a horizontally-centered render
    /// rect that matches the image's actual aspect ratio.
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
    /// Total number of image entries the user configured. Surface in
    /// the status line as `Loading m/n images…` while the loader catches
    /// up so the user knows progress is in flight.
    target_count: usize,
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
        let state = GalleryState {
            idx: 0,
            paused,
            last_rotation: Instant::now(),
        };
        let colors_override = config.colors.clone();

        // Spawn a background loader thread that decodes each image in
        // order and pushes the resulting Slide into the shared vec.
        // Render reads from the same Arc — the first image becomes
        // visible the moment it's decoded, even if later ones are still
        // in flight. App startup is no longer blocked on this work.
        let slides: Arc<Mutex<Vec<Slide>>> =
            Arc::new(Mutex::new(Vec::with_capacity(config.images.len())));
        let target_count = config.images.len();
        if target_count > 0 {
            let slides = slides.clone();
            let paths = config.images.clone();
            std::thread::Builder::new()
                .name("glint-gallery-loader".into())
                .spawn(move || {
                    let mut picker = picker;
                    for raw in paths {
                        let path = expand_tilde(&raw);
                        let label = path
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or_else(|| raw.clone());
                        match load_image(&path) {
                            Ok(img) => {
                                // Downscale to a cap that covers ~2× any
                                // realistic pane size on a typical
                                // terminal. The bound is generous (1600 px
                                // on the long side) to handle resizes
                                // without forcing a re-decode. Phone-camera
                                // sources (4032×3024 etc.) shrink to ~6×
                                // smaller, which translates to roughly 6×
                                // less peak RAM per slide and a noticeable
                                // speed-up when the protocol re-encodes
                                // for a new render area.
                                let img = downscale_to_max_dim(img, MAX_IMAGE_DIM);
                                let pixel_size = (img.width(), img.height());
                                let protocol = picker.new_resize_protocol(img);
                                let slide = Slide {
                                    label,
                                    protocol: Some(Mutex::new(protocol)),
                                    pixel_size,
                                };
                                if let Ok(mut guard) = slides.lock() {
                                    guard.push(slide);
                                }
                            }
                            Err(err) => {
                                tracing::warn!(
                                    path = %path.display(),
                                    error = %err,
                                    "gallery: failed to load image, skipping"
                                );
                            }
                        }
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
            current: Arc::new(Mutex::new(state)),
            rotation_interval,
            font_size,
            colors_override,
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
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
    let target_w = target_w.min(area.width).max(1);
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
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.theme.border_style(focused))
            .title(decorated_title_line(
                focused,
                &title_base,
                self.shortcut,
                self.theme.widget_title,
                self.theme.text_shortcut,
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // No images configured at all — easy to spot guidance message.
        if self.target_count == 0 {
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
                    format!("Loading 0/{}…", self.target_count),
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
                let mut line = format!("{}/{}  ·  {}", idx + 1, self.target_count, label);
                if loaded_count < self.target_count {
                    line.push_str(&format!(
                        "  ·  loading {}/{}",
                        loaded_count, self.target_count
                    ));
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
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
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
            // to skip ahead without waiting for the timer.
            KeyCode::Char('n') | KeyCode::Right | KeyCode::Char('l') => {
                self.advance(true);
                EventResult::Handled
            }
            KeyCode::Char('N') | KeyCode::Left | KeyCode::Char('h') => {
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
            ("N / ←", "previous image"),
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
        let instance = self.instance.clone();
        *self = Self::with_config(instance, new_config, app_theme);
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
}

/// Upper bound on each image's long side after pre-resize. Picked to
/// comfortably cover any reasonable pane on any reasonable terminal
/// (say 240-col terminal × ~60% pane width × ~10 px/cell = ~1400 px),
/// with ~2× headroom in case the user grows the window. Loader thread
/// shrinks every source image to this bound before handing it to
/// `Picker::new_resize_protocol`, so the protocol's cached source is
/// already small.
const MAX_IMAGE_DIM: u32 = 1600;

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
        );
        assert_eq!(main.id(), "gallery");
        let inst = GalleryWidget::with_config(
            "kids".into(),
            GalleryConfig::default(),
            Arc::new(Theme::builtin_defaults()),
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
        );
        assert_eq!(w.shortcut_preferences(), &['g', 'a', 'l', 'r', 'y']);
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
        );
        assert_eq!(widget.slide_count(), 0);
    }
}
