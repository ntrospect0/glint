// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod icons;
pub mod provider;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Datelike;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;

use crate::cache::ScopedCache;
use crate::geolocation::{self, GeoLocation};
use crate::theme::{ColorScheme, Theme};
use crate::ui::{apply_title_row, MetadataEmphasis};

use super::{AppContext, EventResult, Widget};

use provider::{
    describe_code, icon_for_code, render_icon_clipped, OpenMeteoProvider, Units, WeatherData,
};

/// Loaded from `~/.config/glint/weather.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct WeatherConfig {
    /// Display label. Falls back to the IP-geolocation result.
    #[serde(default)]
    pub label: Option<String>,

    #[serde(default)]
    pub latitude: Option<f64>,

    #[serde(default)]
    pub longitude: Option<f64>,

    #[serde(default = "default_units")]
    pub units: Units,

    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// IP-geolocate (via ipapi.co) when lat/lon are missing. Cached per session.
    #[serde(default = "default_auto_locate")]
    pub auto_locate: bool,

    /// Per-widget overrides layered on the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to `['w', 'e', 'a', 't', 'h', 'r']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,

    /// Extra cities the user has swiped onto the carousel. Persisted
    /// as `[[cities]]` blocks. The widget's "home" city (the one
    /// driven by the top-level `label` / `latitude` / `longitude` or
    /// the IP geolocator) sits at carousel index 0; these entries
    /// occupy 1..=N and can be added (`+` on an active `:weather`
    /// lookup) or removed (`-` on the highlighted row).
    #[serde(default)]
    pub cities: Vec<WeatherCity>,
}

/// One extra city on the multi-city carousel. Stores the same triple
/// the home city does (label + lat/lon) so each carousel slot can
/// drive its own Open-Meteo fetch without re-geocoding the label on
/// every refresh.
#[derive(Debug, Clone, Deserialize)]
pub struct WeatherCity {
    pub label: String,
    pub latitude: f64,
    pub longitude: f64,
}

fn default_units() -> Units {
    Units::Metric
}
fn default_poll_interval() -> u64 {
    600
}
fn default_auto_locate() -> bool {
    true
}

impl Default for WeatherConfig {
    fn default() -> Self {
        // Without a weather.toml on disk we default to Richmond, BC. To opt
        // into IP geolocation, write a weather.toml that leaves latitude and
        // longitude unset (auto_locate defaults to true).
        Self {
            label: Some("Richmond, BC".into()),
            latitude: Some(49.166),
            longitude: Some(-123.133),
            units: default_units(),
            poll_interval_secs: default_poll_interval(),
            auto_locate: default_auto_locate(),
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
            cities: Vec::new(),
        }
    }
}

#[derive(Default)]
struct WeatherState {
    location: Option<GeoLocation>,
    locating: bool,
    geolocation_error: Option<String>,
    /// Latest weather snapshot per city, keyed by [`loc_key`]. Lets
    /// the user swipe across cached cities without flashing a "Loading…"
    /// state each time. The currently-selected city's entry is what
    /// the body renders.
    data_by_key: HashMap<String, WeatherData>,
    /// Last fetch error per city; cleared on each successful fetch.
    /// Drives the "⚠ stale" footer on the data view.
    error_by_key: HashMap<String, String>,
    /// Per-city poll tracker; lazy-initialized when a new city
    /// joins the carousel so each city polls on its own cadence
    /// (independent jitter so the cities aren't all due at once).
    poll_by_key: HashMap<String, crate::polling::PollTracker>,
    /// Cities with an Open-Meteo fetch currently in flight. We only
    /// allow one fetch per city; concurrent fetches across cities are
    /// fine — the carousel may need them when the user is rapidly
    /// swiping.
    inflight_keys: HashSet<String>,
    /// Set by `:weather <city>` — when Some, displays as the
    /// trailing carousel entry overriding `selected` until the user
    /// presses `+` (adopt) or `x` (discard).
    transient_location: Option<GeoLocation>,
    /// True while a `:weather <city>` lookup is in flight.
    transient_searching: bool,
    /// Currently-selected carousel index — see [`WeatherWidget::carousel`].
    /// `0` = home; `1..=N` = `config.cities[idx - 1]`; trailing entry
    /// is the transient lookup when present.
    selected: usize,
    /// `(label, kind_id)` for the row pending removal confirmation.
    /// We carry the label so the modal title doesn't drift if the
    /// underlying list reshuffles between the request and the user
    /// pressing `y` / Esc; the kind_id is the IANA-style key we use
    /// to locate the entry in `config.cities` at confirm time.
    confirm_remove: Option<(String, String)>,
    /// Display-state dirty bit drained by `take_dirty`. Set true by
    /// every async-task / tick-time mutation site so the main loop's
    /// dirty-flag gate triggers a redraw.
    dirty: bool,
}

/// Cache key for one city's weather data. Truncates to 4 decimal
/// places of lat/lon (~11m precision) — enough to disambiguate
/// individual cities without hashing trailing noise.
fn loc_key(lat: f64, lon: f64) -> String {
    format!("{lat:.4},{lon:.4}")
}

/// Layout constants shared between `render` (which decides whether
/// to reserve the toggle / hint rows) and `render_with_art` (which
/// applies the same threshold to decide whether to paint the icon
/// at all). Kept at module scope so both paths agree exactly — a
/// mismatch would have one path reserve a row the other path
/// declined to use.
const HEADER_ROWS: u16 = 3;
const FIXED_ART_ROWS: u16 = 8;
/// Number of rows between the art slot and the bottom block.
const ART_TO_BOTTOM_SPACER: u16 = 1;
/// Minimum essential bottom rows under the most compressed art
/// layout (L3 — temp+feels merged into the header line, no blank
/// padding between sections). 6 rows = humidity/wind, blank,
/// "Next 3 days" header, 3 forecasts. The trailing blank + "Updated
/// X ago" footer are bonus rows that drop out first when space is
/// tight via ratatui's natural top-down `Paragraph` truncation.
const MIN_BOTTOM_ROWS_L3: u16 = 6;
/// Lowest body-height threshold under which any art layout fits
/// (L3, the most aggressive compression). Below this, art is
/// hidden and the body falls back to the no-art text layout.
const ART_THRESHOLD: u16 =
    HEADER_ROWS + FIXED_ART_ROWS + ART_TO_BOTTOM_SPACER + MIN_BOTTOM_ROWS_L3;

/// Progressive compression tier for the art layout. Picked from
/// `body_h` in [`pick_art_layout`]; each step buys 1 row of
/// vertical space by removing one of the lowest-signal rows from
/// the bottom block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtLayout {
    /// Spacious: temp, feels, blank, humidity, blank, Next 3 days,
    /// forecast×3 (+ optional blank + "Updated X ago").
    Full,
    /// Merge "temp" and "Feels like X" onto a single line.
    CombinedTemp,
    /// Also remove the blank between the temp/feels line and
    /// humidity/wind.
    TightSpacing,
    /// Also move "<temp>, Feels like X" into the header line so it
    /// reads `<emoji> <label> | <temp>, Feels like <feels>`.
    TempInHeader,
}

/// Pick the compression tier given the body height after the toggle
/// / hint rows have been subtracted. Returns `None` when even L3
/// can't fit — caller falls back to the no-art text layout.
fn pick_art_layout(body_h: u16) -> Option<ArtLayout> {
    let base = HEADER_ROWS + FIXED_ART_ROWS + ART_TO_BOTTOM_SPACER;
    let bottom = body_h.saturating_sub(base);
    if bottom >= MIN_BOTTOM_ROWS_L3 + 3 {
        Some(ArtLayout::Full)
    } else if bottom >= MIN_BOTTOM_ROWS_L3 + 2 {
        Some(ArtLayout::CombinedTemp)
    } else if bottom >= MIN_BOTTOM_ROWS_L3 + 1 {
        Some(ArtLayout::TightSpacing)
    } else if bottom >= MIN_BOTTOM_ROWS_L3 {
        Some(ArtLayout::TempInHeader)
    } else {
        None
    }
}

pub struct WeatherWidget {
    id: String,
    instance: String,
    /// Cached `Weather` / `Weather (instance)` label so `display_name()`
    /// can hand out a `&str` without per-call allocation.
    display_name_cache: String,
    config: WeatherConfig,
    state: Arc<Mutex<WeatherState>>,
    /// App-level theme; kept so live config reloads can rebuild `theme`
    /// from updated `colors` overrides.
    app_theme: Arc<Theme>,
    /// Merged theme (app + widget overrides). Rebuilt on `apply_config`.
    theme: Theme,
    /// Letter assigned by the app for `Shift+<letter>` focus, painted in
    /// the title via `text.shortcut`. `None` = no shortcut claimed.
    shortcut: Option<char>,
    /// Effective shortcut preference list (TOML override or built-in).
    shortcut_prefs: Vec<char>,
    /// Persistent cache of the last successful WeatherData snapshot.
    cache: ScopedCache,
    /// Resolved poll interval, clamped to the 30-second floor. Held
    /// so the lazy `+`-add path can spin up a tracker for a newly-
    /// adopted city without re-deriving it from `config`.
    poll_interval: Duration,
}

impl Default for WeatherWidget {
    fn default() -> Self {
        Self::with_config(
            "main".to_string(),
            WeatherConfig::default(),
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        )
    }
}

impl WeatherWidget {
    pub fn with_config(
        instance: String,
        config: WeatherConfig,
        app_theme: Arc<Theme>,
        cache: ScopedCache,
    ) -> Self {
        // If the user specified explicit lat/lon, seed the location immediately
        // so we skip the geolocation hop.
        let initial_location = match (config.latitude, config.longitude) {
            (Some(lat), Some(lon)) => {
                let label = config
                    .label
                    .clone()
                    .unwrap_or_else(|| format!("{lat:.3}, {lon:.3}"));
                Some(GeoLocation {
                    latitude: lat,
                    longitude: lon,
                    city: label.clone(),
                    city_admin: label.clone(),
                    label,
                    timezone: None,
                })
            }
            _ => None,
        };
        let poll_interval = Duration::from_secs(config.poll_interval_secs.max(30));
        // Seed from cache so the first frame shows the previous reading.
        // Each carousel slot (home + extras) gets its own poll tracker
        // + cache slot keyed by lat/lon so swipes between cities don't
        // collide and individual cities can be refreshed on their own
        // cadence.
        let mut initial_state = WeatherState {
            location: initial_location.clone(),
            ..WeatherState::default()
        };
        let mut seed_poll = |label: &str, lat: f64, lon: f64| {
            let key = loc_key(lat, lon);
            let mut tracker = crate::polling::PollTracker::new(poll_interval);
            if let Some(entry) = cache.load::<WeatherData>(&Self::cache_key(&key)) {
                tracker.seed_from_cache_age(entry.age());
                initial_state.data_by_key.insert(key.clone(), entry.value);
            }
            tracker.apply_jitter(&format!("weather@{instance}/{label}"));
            initial_state.poll_by_key.insert(key, tracker);
        };
        if let Some(home) = &initial_location {
            seed_poll(&home.label, home.latitude, home.longitude);
        }
        for c in &config.cities {
            seed_poll(&c.label, c.latitude, c.longitude);
        }
        let state = Arc::new(Mutex::new(initial_state));
        let theme = app_theme.with_overrides(&config.colors);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['w', 'e', 'a', 't', 'h', 'r']
        } else {
            config.shortcuts.clone()
        };
        let id = if instance == "main" {
            "weather".to_string()
        } else {
            format!("weather@{instance}")
        };
        let display_name_cache = if instance == "main" {
            "Weather".to_string()
        } else {
            format!("Weather ({instance})")
        };
        Self {
            id,
            instance,
            display_name_cache,
            config,
            state,
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
            cache,
            poll_interval,
        }
    }

    /// Persistent-cache key for the weather payload of one city.
    /// Namespaced with a `city:` prefix so a future schema change
    /// can introduce sibling caches without colliding.
    fn cache_key(loc_key: &str) -> String {
        format!("city:{loc_key}")
    }

    /// Snapshot of the current carousel — home (idx 0), then the
    /// user's extra cities in TOML order, then (optionally) the
    /// `:weather <city>` lookup pinned to the trailing slot. Empty
    /// when the IP-geolocator hasn't produced a home yet *and* no
    /// lookup is active.
    fn carousel(&self) -> Vec<CitySlot> {
        let st = self.state.lock().expect("weather state poisoned");
        let mut out: Vec<CitySlot> = Vec::with_capacity(1 + self.config.cities.len() + 1);
        if let Some(home) = &st.location {
            out.push(CitySlot {
                kind: CityKind::Home,
                location: home.clone(),
            });
        }
        for (i, c) in self.config.cities.iter().enumerate() {
            out.push(CitySlot {
                kind: CityKind::Extra(i),
                location: GeoLocation {
                    latitude: c.latitude,
                    longitude: c.longitude,
                    city: c.label.clone(),
                    city_admin: c.label.clone(),
                    label: c.label.clone(),
                    timezone: None,
                },
            });
        }
        if let Some(t) = &st.transient_location {
            out.push(CitySlot {
                kind: CityKind::Lookup,
                location: t.clone(),
            });
        }
        out
    }

    /// Move the carousel cursor by `delta` (negative = left). Clamps
    /// at the ends; returns `Handled` only when the cursor actually
    /// moves so a no-op press doesn't swallow the key from a parent
    /// handler.
    fn select_delta(&self, delta: i32) -> EventResult {
        let total = self.carousel().len();
        if total <= 1 {
            return EventResult::Ignored;
        }
        let mut st = self.state.lock().expect("weather state poisoned");
        let cur = st.selected.min(total - 1) as i32;
        let next = (cur + delta).clamp(0, total as i32 - 1) as usize;
        if next == st.selected.min(total - 1) {
            return EventResult::Ignored;
        }
        st.selected = next;
        st.dirty = true;
        EventResult::Handled
    }

    /// `+` handler — adopt the `:weather <city>` lookup into the
    /// permanent `config.cities` list, persist to weather.toml, and
    /// land the selector on the new entry. No-op when no lookup is
    /// active or the same coords are already tracked.
    fn add_transient_to_cities(&mut self) -> EventResult {
        let transient = {
            let st = self.state.lock().expect("weather state poisoned");
            st.transient_location.clone()
        };
        let Some(loc) = transient else {
            return EventResult::Ignored;
        };
        let key = loc_key(loc.latitude, loc.longitude);
        // Skip duplicates — adding the same coords twice leaves two
        // rows ticking in lockstep and confuses navigation.
        let already_tracked = self.config.cities.iter().any(|c| {
            loc_key(c.latitude, c.longitude) == key
        }) || self
            .state
            .lock()
            .expect("weather state poisoned")
            .location
            .as_ref()
            .map(|h| loc_key(h.latitude, h.longitude) == key)
            .unwrap_or(false);
        if already_tracked {
            // Still discard the transient so `:weather Foo` on an
            // already-tracked city resolves cleanly.
            let mut st = self.state.lock().expect("weather state poisoned");
            st.transient_location = None;
            st.selected = 0;
            st.dirty = true;
            return EventResult::Handled;
        }
        self.config.cities.push(WeatherCity {
            // Persist the "City, Admin1" form so swiping back to
            // this entry reads the same way as it did mid-lookup —
            // we don't want the carousel to silently lose the
            // province/state hint after the user pressed `+`.
            label: loc.city_admin.clone(),
            latitude: loc.latitude,
            longitude: loc.longitude,
        });
        self.persist_cities();
        let new_idx = self.config.cities.len(); // home is 0, extras start at 1
        let mut st = self.state.lock().expect("weather state poisoned");
        st.transient_location = None;
        st.selected = new_idx;
        st.dirty = true;
        EventResult::Handled
    }

    /// `-` handler — opens the confirm modal when the cursor is on
    /// an Extra slot. Home and Lookup rows can't be removed via this
    /// path (home is configured, lookup uses `x`).
    fn request_remove_selected(&self) -> EventResult {
        let entries = self.carousel();
        if entries.is_empty() {
            return EventResult::Ignored;
        }
        let st_sel = self
            .state
            .lock()
            .expect("weather state poisoned")
            .selected
            .min(entries.len() - 1);
        let slot = &entries[st_sel];
        match slot.kind {
            CityKind::Extra(_) => {}
            CityKind::Home | CityKind::Lookup => return EventResult::Ignored,
        }
        let label = slot.location.label.clone();
        let key = loc_key(slot.location.latitude, slot.location.longitude);
        self.state
            .lock()
            .expect("weather state poisoned")
            .confirm_remove = Some((label, key));
        EventResult::Handled
    }

    fn confirm_remove_selected(&mut self) {
        let key = match self
            .state
            .lock()
            .expect("weather state poisoned")
            .confirm_remove
            .clone()
        {
            Some((_, k)) => k,
            None => return,
        };
        let before = self.config.cities.len();
        self.config
            .cities
            .retain(|c| loc_key(c.latitude, c.longitude) != key);
        let removed = self.config.cities.len() < before;
        let mut st = self.state.lock().expect("weather state poisoned");
        st.confirm_remove = None;
        if !removed {
            return;
        }
        // Drop the per-city caches for the removed entry so a future
        // re-add doesn't show a stale reading from a deleted past.
        st.data_by_key.remove(&key);
        st.error_by_key.remove(&key);
        st.poll_by_key.remove(&key);
        st.inflight_keys.remove(&key);
        // Re-clamp the carousel cursor. Land on whichever neighbor
        // is still in range so the user sees an obvious "the row
        // you were on is gone" landing pad rather than a silent jump.
        let new_total = 1 + self.config.cities.len() + {
            if st.transient_location.is_some() { 1 } else { 0 }
        };
        if st.selected >= new_total {
            st.selected = new_total.saturating_sub(1);
        }
        st.dirty = true;
        drop(st);
        self.persist_cities();
    }

    fn cancel_remove(&self) {
        self.state
            .lock()
            .expect("weather state poisoned")
            .confirm_remove = None;
    }

    /// Rewrite the `[[cities]]` blocks in this instance's
    /// weather.toml to match `self.config.cities`. Mirrors the
    /// clock-widget's `persist_secondary_timezones`: strips existing
    /// entries, re-emits the current list, and preserves comments +
    /// unrelated keys via the shared TOML-merge helper.
    fn persist_cities(&self) {
        use std::fmt::Write as _;
        let stem = crate::widgets::widget_config_stem(KIND, &self.instance);
        let path = match crate::config::config_dir() {
            Ok(d) => d.join(format!("{stem}.toml")),
            Err(err) => {
                tracing::warn!(error = %err, "weather: could not resolve config dir");
                return;
            }
        };
        let original = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(error = %err, "weather: failed to read {}", path.display());
                    return;
                }
            }
        } else {
            String::new()
        };
        let mut updated =
            crate::wizard::toml_merge::strip_array_of_tables_blocks(&original, "cities");
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        for c in &self.config.cities {
            if !updated.is_empty() && !updated.ends_with("\n\n") {
                updated.push('\n');
            }
            let _ = writeln!(updated, "[[cities]]");
            let _ = writeln!(updated, "label = {}", toml_quote(&c.label));
            let _ = writeln!(updated, "latitude = {}", c.latitude);
            let _ = writeln!(updated, "longitude = {}", c.longitude);
        }
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                tracing::warn!(error = %err, "weather: failed to mkdir {}", parent.display());
                return;
            }
        }
        let tmp = path.with_extension("toml.tmp");
        if let Err(err) = std::fs::write(&tmp, &updated) {
            tracing::warn!(error = %err, "weather: failed to write {}", tmp.display());
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, &path) {
            tracing::warn!(error = %err, "weather: failed to rename into place at {}", path.display());
        }
    }

    /// What the widget should do on the next tick. Computed inside a single
    /// short lock window. Operates on the *currently selected* carousel
    /// entry so swipes that land on a stale city schedule a refresh
    /// for that city specifically; other cities' trackers tick on
    /// their own.
    fn next_action(&self) -> NextAction {
        let entries = self.carousel();
        let st = self.state.lock().expect("weather state poisoned");
        // No carousel entries yet → either kick off IP geolocation or
        // wait for one to finish.
        if entries.is_empty() {
            if st.locating || st.transient_searching {
                return NextAction::Wait;
            }
            return if self.config.auto_locate {
                NextAction::Locate
            } else {
                NextAction::Wait
            };
        }
        let selected = st.selected.min(entries.len() - 1);
        let slot = &entries[selected];
        let key = loc_key(slot.location.latitude, slot.location.longitude);
        if st.inflight_keys.contains(&key) {
            return NextAction::Wait;
        }
        // No tracker yet for this slot (lookup transient never got
        // a seed, or a city just added) — treat as due so we kick
        // off the first fetch immediately.
        let due = match st.poll_by_key.get(&key) {
            Some(t) => t.is_due(),
            None => true,
        };
        if due {
            NextAction::Fetch(slot.location.latitude, slot.location.longitude)
        } else {
            NextAction::Wait
        }
    }

    /// Resolve a city / place name to lat/lon via Open-Meteo's free geocoding
    /// API, store the result as `transient_location`, and force a refresh.
    /// Errors are logged; the widget keeps showing the previous data.
    fn lookup_location(&self, query: &str) {
        {
            let mut st = self.state.lock().expect("weather state poisoned");
            st.transient_searching = true;
            st.dirty = true;
        }
        let state = self.state.clone();
        let query = query.to_string();
        let total_slots = self.carousel().len();
        tokio::spawn(async move {
            let result = crate::geolocation::by_name(&query).await;
            let mut st = state.lock().expect("weather state poisoned");
            st.transient_searching = false;
            st.dirty = true;
            match result {
                Ok(loc) => {
                    st.transient_location = Some(loc);
                    // Auto-select the freshly-loaded transient (now
                    // the trailing carousel slot) so the body
                    // immediately switches to it. `total_slots` was
                    // captured before adding the transient — the
                    // transient now sits at that index.
                    st.selected = total_slots;
                }
                Err(err) => {
                    tracing::warn!(query = %query, error = %err, "weather geocoding failed");
                }
            }
        });
    }

    /// Clear the `:weather <city>` override and bounce the selection
    /// back to home so the user isn't left on a now-empty slot.
    fn clear_transient(&self) {
        let mut st = self.state.lock().expect("weather state poisoned");
        if st.transient_location.take().is_some() {
            st.selected = 0;
            st.dirty = true;
        }
    }

    fn spawn_geolocate(&self) {
        {
            let mut st = self.state.lock().expect("weather state poisoned");
            st.locating = true;
            st.dirty = true;
        }
        let state = self.state.clone();
        tokio::spawn(async move {
            let result = geolocation::by_ip().await;
            let mut st = state.lock().expect("weather state poisoned");
            st.locating = false;
            st.dirty = true;
            match result {
                Ok(loc) => {
                    st.location = Some(loc);
                    st.geolocation_error = None;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "ip geolocation failed");
                    st.geolocation_error = Some(err.to_string());
                }
            }
        });
    }

    fn spawn_refresh(&self, lat: f64, lon: f64) {
        let key = loc_key(lat, lon);
        {
            let mut st = self.state.lock().expect("weather state poisoned");
            st.inflight_keys.insert(key.clone());
            // Lazy-init a poll tracker for cities that joined the
            // carousel after the constructor ran (the `+`-add path,
            // or the very first transient lookup).
            let interval = self.poll_interval;
            let instance = self.instance.clone();
            let tracker = st
                .poll_by_key
                .entry(key.clone())
                .or_insert_with(|| {
                    let mut t = crate::polling::PollTracker::new(interval);
                    t.apply_jitter(&format!("weather@{instance}/{key}"));
                    t
                });
            tracker.mark_attempted();
            st.dirty = true;
        }
        let units = self.config.units;
        let state = self.state.clone();
        let cache = self.cache.clone();
        let cache_key = Self::cache_key(&key);
        let key_clone = key.clone();
        tokio::spawn(async move {
            let provider = OpenMeteoProvider::new(lat, lon, units);
            let result = provider.fetch().await;
            let mut st = state.lock().expect("weather state poisoned");
            st.inflight_keys.remove(&key_clone);
            st.dirty = true;
            match result {
                Ok(data) => {
                    if let Err(err) = cache.store(&cache_key, &data) {
                        tracing::warn!(error = %err, "weather cache store failed");
                    }
                    st.data_by_key.insert(key_clone.clone(), data);
                    st.error_by_key.remove(&key_clone);
                }
                Err(err) => {
                    tracing::warn!(error = %err, key = %key_clone, "weather fetch failed");
                    st.error_by_key.insert(key_clone, err.to_string());
                }
            }
        });
    }
}

#[derive(Debug, Clone, Copy)]
enum NextAction {
    Locate,
    /// Refresh the city at the given `(latitude, longitude)`. Carries
    /// the coords so the dispatch in `update()` doesn't need to
    /// re-resolve the selection after dropping the state lock.
    Fetch(f64, f64),
    Wait,
}

/// Origin of a carousel entry — drives both label styling (Home gets
/// the focused-text color, Lookup gets the selected-text color) and
/// `+`/`-` eligibility (only Extra rows can be removed; only Lookup
/// can be adopted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CityKind {
    Home,
    Extra(usize),
    Lookup,
}

/// One carousel slot. Cloned from the underlying `WeatherState` /
/// `config.cities` data so the renderer + key handlers don't need
/// to keep the state lock across the rest of the frame.
#[derive(Debug, Clone)]
struct CitySlot {
    kind: CityKind,
    location: GeoLocation,
}

impl WeatherWidget {
    fn render_city_toggle(
        &self,
        frame: &mut Frame,
        area: Rect,
        carousel: &[CitySlot],
        selected_idx: usize,
    ) {
        let slot = &carousel[selected_idx];
        // Arrows dim out at the ends to signal "no more in this direction"
        // without hiding their slots — keeps the carousel width steady
        // as the user swipes, so the city label doesn't jump horizontally.
        let active_arrow = self.theme.text_dim;
        let inactive_arrow = self.theme.text_dim.add_modifier(Modifier::DIM);
        let has_prev = selected_idx > 0;
        let has_next = selected_idx + 1 < carousel.len();
        let left = Span::styled(
            if has_prev { "◂ " } else { "  " },
            if has_prev { active_arrow } else { inactive_arrow },
        );
        let right = Span::styled(
            if has_next { " ▸" } else { "  " },
            if has_next { active_arrow } else { inactive_arrow },
        );
        // Label color reflects the kind: Home in the focused-text
        // accent (matches the clock widget's primary-row highlight),
        // Lookup in the selected-text accent (the `:weather <city>`
        // override read), and Extra in the default body color.
        let label_style = match slot.kind {
            CityKind::Home => self.theme.text_focused.add_modifier(Modifier::BOLD),
            CityKind::Lookup => self.theme.text_selected.add_modifier(Modifier::BOLD),
            CityKind::Extra(_) => Style::default().add_modifier(Modifier::BOLD),
        };
        let label = Span::styled(format!("[{}]", slot.location.city_admin), label_style);
        frame.render_widget(
            Paragraph::new(Line::from(vec![left, label, right])).alignment(Alignment::Center),
            area,
        );
    }
}

#[async_trait]
impl Widget for WeatherWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        "weather"
    }

    fn instance(&self) -> &str {
        &self.instance
    }

    fn display_name(&self) -> &str {
        &self.display_name_cache
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        match self.next_action() {
            NextAction::Locate => self.spawn_geolocate(),
            NextAction::Fetch(lat, lon) => self.spawn_refresh(lat, lon),
            NextAction::Wait => {}
        }
        Ok(())
    }

    fn take_dirty(&mut self) -> bool {
        let mut st = self.state.lock().expect("weather state poisoned");
        std::mem::replace(&mut st.dirty, false)
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let carousel = self.carousel();
        let snapshot = {
            let st = self.state.lock().expect("weather state poisoned");
            let selected = if carousel.is_empty() {
                None
            } else {
                Some(st.selected.min(carousel.len() - 1))
            };
            let (data, last_error) = match selected {
                Some(idx) => {
                    let key = loc_key(
                        carousel[idx].location.latitude,
                        carousel[idx].location.longitude,
                    );
                    (
                        st.data_by_key.get(&key).cloned(),
                        st.error_by_key.get(&key).cloned(),
                    )
                }
                None => (None, None),
            };
            // "Loading…" applies whenever the selected city is mid-fetch
            // OR a `:weather <city>` lookup is itself resolving — both
            // surface as the same in-flight signal to the user.
            let inflight = match selected {
                Some(idx) => {
                    let key = loc_key(
                        carousel[idx].location.latitude,
                        carousel[idx].location.longitude,
                    );
                    st.inflight_keys.contains(&key)
                }
                None => false,
            } || st.transient_searching;
            // Some carousel slot must have attempted its first fetch
            // before we can treat the empty-data state as "we tried
            // and got nothing" vs "first reading still pending."
            let attempted = match selected {
                Some(idx) => {
                    let key = loc_key(
                        carousel[idx].location.latitude,
                        carousel[idx].location.longitude,
                    );
                    st.poll_by_key
                        .get(&key)
                        .map(|t| t.has_attempted())
                        .unwrap_or(false)
                }
                None => false,
            };
            Snapshot {
                location_label: selected.map(|i| carousel[i].location.label.clone()),
                locating: st.locating,
                geolocation_error: st.geolocation_error.clone(),
                data,
                last_error,
                inflight,
                attempted,
            }
        };
        // Title row drops the city metadata when the toggle bar is
        // visible — the bar is the source of truth for which city
        // we're viewing and a duplicate echo just wastes width. When
        // no carousel exists yet (IP-geolocation still running), we
        // fall back to the old "Locating…" metadata so the title row
        // isn't bare.
        let title_prefix = if self.instance == "main" {
            "Weather".to_string()
        } else {
            format!("Weather ({})", self.instance)
        };
        let metadata: Option<String> = if carousel.is_empty() {
            Some("Locating…".to_string())
        } else {
            None
        };
        let block = apply_title_row(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(focused)),
            focused,
            &title_prefix,
            metadata.as_deref(),
            MetadataEmphasis::Default,
            self.shortcut,
            &self.theme,
            area.width,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Layout: optional helper-hint row, then the city-toggle row,
        // then the body. The toggle is shown whenever a carousel
        // exists (≥1 slot); the helper hint is shown when one of the
        // contextual shortcuts (`+ add`, `- remove`) applies to the
        // current selection. Both rows together claim the bottom of
        // the cell; insufficient height drops them in order from the
        // top so the body always wins the remaining space.
        let show_toggle = !carousel.is_empty();
        let selected_idx = if carousel.is_empty() {
            None
        } else {
            Some(
                self.state
                    .lock()
                    .expect("weather state poisoned")
                    .selected
                    .min(carousel.len() - 1),
            )
        };
        let kind_at_selection = selected_idx.map(|i| carousel[i].kind);
        let hint_parts: Vec<&str> = {
            let mut v: Vec<&str> = Vec::with_capacity(2);
            if matches!(kind_at_selection, Some(CityKind::Lookup)) {
                v.push("+ add");
                v.push("x revert");
            }
            if matches!(kind_at_selection, Some(CityKind::Extra(_))) {
                v.push("- remove");
            }
            v
        };
        // Reserve the hint row whenever a carousel exists, *except*
        // when doing so would tip the body past the art threshold —
        // the weather glyph carries more visual signal than the
        // small "- remove" / "+ add" hint, so we drop the hint to
        // protect the icon. Always-reserving on Home matters too: a
        // bare-toggle (no hint) on Home with a toggle+hint on Extra
        // would shrink the body by one row between swipes and let
        // CLOUD vanish while SUN survived.
        let toggle_cost: u16 = if show_toggle { 1 } else { 0 };
        let body_no_hint = inner.height.saturating_sub(toggle_cost);
        let body_with_hint = body_no_hint.saturating_sub(1);
        let art_fits_no_hint = body_no_hint >= ART_THRESHOLD;
        let art_fits_with_hint = body_with_hint >= ART_THRESHOLD;
        let has_data = snapshot.data.is_some();
        // Default policy: reserve a hint row whenever a carousel
        // exists, so layout stays stable across selections.
        // Override: drop the hint when doing so salvages the icon.
        let show_hint = show_toggle
            && inner.height >= 3
            && !(has_data && art_fits_no_hint && !art_fits_with_hint);

        let mut body_h = inner.height;
        let mut footer_y = inner.y + inner.height;
        let toggle_area = if show_toggle && body_h >= 2 {
            body_h -= 1;
            footer_y -= 1;
            Some(Rect {
                x: inner.x,
                y: footer_y,
                width: inner.width,
                height: 1,
            })
        } else {
            None
        };
        let hint_area = if show_hint && body_h >= 2 {
            body_h -= 1;
            footer_y -= 1;
            Some(Rect {
                x: inner.x,
                y: footer_y,
                width: inner.width,
                height: 1,
            })
        } else {
            None
        };
        let body_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: body_h,
        };

        // When we have weather data, the ASCII art needs its own fixed-width
        // sub-rect so each art row lands at the same x offset. Centered
        // Paragraph alignment treats each line independently — lines with
        // different trimmed widths shift relative to each other, which made
        // the symmetric sun look broken on the bottom row.
        if let Some(data) = &snapshot.data {
            render_with_art(
                frame,
                body_area,
                &snapshot,
                data,
                self.config.units,
                &self.theme,
            );
        } else {
            let lines = loading_lines(&snapshot, &self.theme);
            let mut padded: Vec<Line<'_>> = Vec::with_capacity(lines.len() + 1);
            padded.push(Line::from(""));
            padded.extend(lines);
            let body = Paragraph::new(padded).alignment(Alignment::Center);
            frame.render_widget(body, body_area);
        }

        if let (Some(area), Some(idx)) = (toggle_area, selected_idx) {
            self.render_city_toggle(frame, area, &carousel, idx);
        }
        if let Some(area) = hint_area {
            let text = hint_parts.join(" · ");
            let hint = Line::from(Span::styled(text, self.theme.text_dim));
            frame.render_widget(Paragraph::new(hint).alignment(Alignment::Center), area);
        }

        // Confirm-remove modal overlays everything else, so it goes last.
        let pending = self
            .state
            .lock()
            .expect("weather state poisoned")
            .confirm_remove
            .clone();
        if let Some((label, _key)) = pending {
            crate::ui::modal::render(
                frame,
                area,
                &self.theme,
                crate::ui::modal::ConfirmModal {
                    title: " Remove city? ",
                    target: &label,
                    hint: None,
                    max_width: 56,
                },
            );
        }

        // Losing focus dismisses any open confirm-remove modal —
        // mirroring the clock widget's policy that a focus shift
        // mid-prompt is an obvious cancel signal.
        if !focused {
            let mut st = self.state.lock().expect("weather state poisoned");
            st.confirm_remove = None;
        }
    }


    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // Modal eats every keypress while open: y/Y commits, anything
        // else cancels. Runs first so navigation keys don't bypass
        // the prompt.
        if self
            .state
            .lock()
            .expect("weather state poisoned")
            .confirm_remove
            .is_some()
        {
            match crate::ui::modal::dispatch_key(key) {
                crate::ui::modal::ConfirmChoice::Confirm => self.confirm_remove_selected(),
                crate::ui::modal::ConfirmChoice::Cancel => self.cancel_remove(),
            }
            return EventResult::Handled;
        }
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return EventResult::Ignored;
        }
        // Uppercase ASCII letters are reserved for the app-wide
        // `Shift+<letter>` focus-jump dispatcher — never consume them here.
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_uppercase() {
                return EventResult::Ignored;
            }
        }
        match key.code {
            KeyCode::Char('x') => {
                self.clear_transient();
                EventResult::Handled
            }
            // Carousel navigation: vim-style + arrow keys both swipe
            // between cities. Bare-letter so Shift+H/L pass through
            // to the app-level focus dispatcher.
            KeyCode::Char('h') | KeyCode::Left => self.select_delta(-1),
            KeyCode::Char('l') | KeyCode::Right => self.select_delta(1),
            // `+` adopts the `:weather <city>` lookup; `-` removes
            // the highlighted Extra row (modal-confirmed). Both
            // delegate to the carousel-aware helpers so they no-op
            // on rows where the action doesn't apply.
            KeyCode::Char('+') => self.add_transient_to_cities(),
            KeyCode::Char('-') => self.request_remove_selected(),
            _ => EventResult::Ignored,
        }
    }

    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool> {
        match cmd {
            "weather" | "w" => {
                if args.is_empty() {
                    anyhow::bail!("usage: :weather <city>");
                }
                let query = args.join(" ");
                self.lookup_location(&query);
                Ok(true)
            }
            "refresh" => {
                // Force-mark every per-city tracker so the next tick
                // re-fetches everything we know about. Lazy-init isn't
                // needed here — cities without a tracker haven't been
                // viewed yet and will create one on their first visit.
                let mut st = self.state.lock().expect("weather state poisoned");
                for t in st.poll_by_key.values_mut() {
                    t.mark_dirty();
                }
                st.dirty = true;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn keybindings(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("←/→ / h/l", "swipe between cities"),
            ("+", "add `:weather` lookup to city carousel"),
            ("-", "remove highlighted city"),
            ("x", "clear :weather lookup (return to home)"),
            (":weather <city>", "look up weather for a place"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "label": self.config.label,
            "latitude": self.config.latitude,
            "longitude": self.config.longitude,
            "poll_interval_secs": self.config.poll_interval_secs,
            "auto_locate": self.config.auto_locate,
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: WeatherConfig =
            serde_json::from_value(config).context("invalid weather config payload")?;
        let app_theme = self.app_theme.clone();
        let cache = self.cache.clone();
        let instance = self.instance.clone();
        // `assign_shortcuts` only runs once at startup, so a config
        // reload (e.g. one triggered by our own `+`/`-` rewrite of
        // weather.toml) must carry the assigned `Shift+<letter>`
        // through manually — otherwise the title-bar accent vanishes.
        let shortcut = self.shortcut;
        *self = Self::with_config(instance, new_config, app_theme, cache);
        self.shortcut = shortcut;
        Ok(())
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.with_overrides(&self.config.colors);
        self.app_theme = theme;
    }

    fn poll_snapshot(&self) -> Option<crate::polling::PollSnapshot> {
        // Surface the currently-selected city's tracker — that's the
        // one whose freshness the user is staring at. Falls back to
        // `None` when no carousel exists yet (IP geolocation still
        // in flight).
        let carousel = self.carousel();
        if carousel.is_empty() {
            return None;
        }
        let st = self.state.lock().ok()?;
        let idx = st.selected.min(carousel.len() - 1);
        let key = loc_key(
            carousel[idx].location.latitude,
            carousel[idx].location.longitude,
        );
        st.poll_by_key.get(&key).map(|t| t.snapshot())
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

    fn title_metadata(&self) -> Option<String> {
        // With the multi-city toggle bar owning the "which city are
        // we showing" affordance, we surface the active label only
        // when no city has resolved yet — i.e. for the IP-geolocate
        // / lookup-in-flight states the title still echoes useful
        // status. Once we have a carousel, the bottom bar is the
        // source of truth.
        let st = self.state.lock().ok()?;
        if st.location.is_none() && st.transient_location.is_none() {
            return None;
        }
        // Carousel is the source of truth for the visible city, so
        // we don't repeat it in the title.
        None
    }
}

struct Snapshot {
    location_label: Option<String>,
    locating: bool,
    geolocation_error: Option<String>,
    data: Option<WeatherData>,
    last_error: Option<String>,
    inflight: bool,
    attempted: bool,
}

fn render_with_art(
    frame: &mut Frame,
    inner: Rect,
    s: &Snapshot,
    data: &WeatherData,
    units: Units,
    theme: &Theme,
) {
    let (label, icon_glyph) = describe_code(data.weather_code);
    // Layout tier is keyed off the body height alone; falls back to
    // L3 (the most aggressive compression) when even that doesn't
    // fit so the caller can branch on `None` to drop the art
    // entirely. `render` has already done that branch via
    // `ART_THRESHOLD` so by the time we land here, `pick_art_layout`
    // is guaranteed to be `Some`.
    let layout = pick_art_layout(inner.height).unwrap_or(ArtLayout::TempInHeader);

    // Temp + feels combined string — surfaced into the header on
    // L3, and otherwise emitted as its own bottom-block line.
    // Split into two strings so the leading temperature can keep
    // its yellow+bold highlight even when the line is compressed.
    let temp_str = format!("{:.0}{}", data.temperature, data.units.temp_symbol());
    let feels_suffix = format!(
        ", Feels like {:.0}{}",
        data.apparent_temperature,
        data.units.temp_symbol()
    );
    let temp_feels = format!("{temp_str}{feels_suffix}");
    let temp_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    // Header: top blank + condition label (+ "| temp, feels" on L3) + blank.
    //
    // We center the icon + label manually instead of relying on
    // `Alignment::Center`. Ratatui's center math goes through
    // `unicode_width`, which reports the emoji + VS-16 sequence as width 1,
    // but actual terminals render the glyph as 2 cells. Their disagreement
    // would shift the whole line one cell off-center. `chars().count()`
    // matches the real cell width for our icons (emoji + VS-16 = 2 chars,
    // bare "·" fallback = 1 char) so the hand-rolled padding lines up with
    // what the user actually sees.
    let header_text = if layout == ArtLayout::TempInHeader {
        format!("{icon_glyph}  {label}  |  {temp_feels}")
    } else {
        format!("{icon_glyph}  {label}")
    };
    let visual_width = header_text.chars().count() as u16;
    let pad = inner.width.saturating_sub(visual_width) / 2;
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let header_content = if layout == ArtLayout::TempInHeader {
        // Split the header into three spans so the leading
        // temperature keeps its yellow+bold accent — same visual
        // weight it carries in the spacier layouts — while the
        // surrounding label + "Feels like X" stay in plain bold.
        Line::from(vec![
            Span::styled(
                format!("{:pad$}{icon_glyph}  {label}  |  ", "", pad = pad as usize),
                bold,
            ),
            Span::styled(temp_str.clone(), temp_style),
            Span::styled(feels_suffix.clone(), bold),
        ])
    } else {
        Line::from(Span::styled(
            format!("{:pad$}{icon_glyph}  {label}", "", pad = pad as usize),
            bold,
        ))
    };
    let header_lines: Vec<Line<'_>> = vec![Line::from(""), header_content, Line::from("")];
    let header_height: u16 = header_lines.len() as u16;
    let header_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: header_height.min(inner.height),
    };
    frame.render_widget(
        Paragraph::new(header_lines).alignment(Alignment::Left),
        header_area,
    );

    // Art slot. Fixed-height; sprites taller than `FIXED_ART_ROWS`
    // get a symmetric top/bottom crop via `render_icon_clipped`;
    // shorter sprites get vertically centered inside the slot.
    // Both directions of slack absorb into the slot rather than
    // shifting the rows below, so the bottom block stays anchored
    // at the same Y across cities. The "show art at all" decision
    // was made in `render` via `ART_THRESHOLD`; here we just paint.
    let night = data.is_night(chrono::Local::now());
    let icon = icon_for_code(data.weather_code, night);
    let total_icon_rows = (icon.height as u16).div_ceil(2);
    let drawn_icon_rows = total_icon_rows.min(FIXED_ART_ROWS);
    let icon_cols = (icon.width as u16).min(inner.width);
    let mut used_top = header_height;
    let art_x = inner.x + (inner.width.saturating_sub(icon_cols)) / 2;
    // Center the drawn sprite (post-crop) inside the fixed slot.
    let art_y = inner.y
        + header_height
        + FIXED_ART_ROWS.saturating_sub(drawn_icon_rows) / 2;
    let art_area = Rect {
        x: art_x,
        y: art_y,
        width: icon_cols,
        height: drawn_icon_rows,
    };
    frame.render_widget(
        Paragraph::new(render_icon_clipped(icon, Some(FIXED_ART_ROWS))),
        art_area,
    );
    // Advance by the *slot* height (not the actual sprite height)
    // plus a 1-row spacer so the bottom block starts at a stable Y
    // regardless of which sprite is showing.
    used_top = used_top.saturating_add(FIXED_ART_ROWS).saturating_add(ART_TO_BOTTOM_SPACER);

    // Bottom section: temp/feels (or skip on L3), humidity/wind,
    // forecast, footer. Inter-section blank rows are dropped on L2
    // and L3 to claw back vertical space.
    if inner.height <= used_top {
        return;
    }
    let bottom_area = Rect {
        x: inner.x,
        y: inner.y + used_top,
        width: inner.width,
        height: inner.height - used_top,
    };
    let mut lines: Vec<Line<'_>> = Vec::new();
    // Temp/feels styling: the leading temperature is rendered in
    // `temp_style` (yellow+bold) across every layout that keeps it
    // in the body — `", Feels like X°C"` falls back to the default
    // text style so the highlight cue stays on the primary reading.
    match layout {
        ArtLayout::Full => {
            lines.push(Line::from(Span::styled(temp_str.clone(), temp_style)));
            lines.push(Line::from(format!(
                "Feels like {:.0}{}",
                data.apparent_temperature,
                data.units.temp_symbol()
            )));
            lines.push(Line::from(""));
        }
        ArtLayout::CombinedTemp => {
            lines.push(Line::from(vec![
                Span::styled(temp_str.clone(), temp_style),
                Span::raw(feels_suffix.clone()),
            ]));
            lines.push(Line::from(""));
        }
        ArtLayout::TightSpacing => {
            lines.push(Line::from(vec![
                Span::styled(temp_str.clone(), temp_style),
                Span::raw(feels_suffix.clone()),
            ]));
            // No blank between temp/feels and humidity/wind.
        }
        ArtLayout::TempInHeader => {
            // Temp+feels live up in the header; nothing here.
        }
    }
    lines.push(Line::from(format!(
        "Humidity: {:.0}%   Wind: {:.0} {}",
        data.humidity,
        data.wind_speed,
        data.units.wind_label()
    )));

    if data.daily.len() >= 2 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "── Next 3 days ──",
            theme.text_dim,
        )));
        for d in data.daily.iter().skip(1).take(3) {
            let (_, icon) = describe_code(d.weather_code);
            lines.push(Line::from(format!(
                "{}  {}  {:.0}{} / {:.0}{}",
                weekday_short(d.date.weekday()),
                icon,
                d.temperature_high,
                units.temp_symbol(),
                d.temperature_low,
                units.temp_symbol(),
            )));
        }
    }

    lines.push(Line::from(""));
    let age_secs = chrono::Local::now()
        .signed_duration_since(data.fetched_at)
        .num_seconds()
        .max(0);
    // Sub-minute ages aren't useful to surface as a seconds counter —
    // they tick noisily every second when nothing's actually changed.
    // Collapse anything under a minute to a single "Just updated" line.
    let fresh = age_secs < 60;
    let age = format_age(age_secs);
    let footer = if let Some(e) = &s.last_error {
        if fresh {
            format!("⚠ stale ({e}) — just updated")
        } else {
            format!("⚠ stale ({e}) — updated {age} ago")
        }
    } else if fresh {
        "Just updated".to_string()
    } else {
        format!("Updated {age} ago")
    };
    lines.push(Line::from(Span::styled(footer, theme.text_dim)));

    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        bottom_area,
    );
}

fn loading_lines(s: &Snapshot, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(""));
    if s.location_label.is_none() {
        if let Some(err) = &s.geolocation_error {
            lines.push(Line::from(Span::styled(
                "Could not auto-locate",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(err.clone()));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Set latitude/longitude in ~/.config/glint/weather.toml",
                theme.text_dim,
            )));
        } else if s.locating {
            lines.push(Line::from("Locating you via IP…"));
        } else {
            lines.push(Line::from("Configure latitude/longitude in weather.toml"));
        }
        return lines;
    }
    if s.inflight {
        lines.push(Line::from("Loading weather…"));
    } else if let Some(err) = &s.last_error {
        lines.push(Line::from(Span::styled(
            "Weather unavailable",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(err.clone()));
    } else if s.attempted {
        lines.push(Line::from("Loading weather…"));
    } else {
        lines.push(Line::from("Fetching first reading…"));
    }
    lines
}

/// Compact `Ns / Nm / Nh / Nd` data-age label. Delegates to the
/// shared [`crate::format::short_duration_label`].
fn format_age(secs: i64) -> String {
    crate::format::short_duration_label(secs)
}

/// TOML scalar string quoter mirroring the clock widget's helper.
/// Escapes `"`, `\`, and control bytes so a city label like
/// `Washington, D.C.` round-trips cleanly through weather.toml.
fn toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn weekday_short(w: chrono::Weekday) -> &'static str {
    use chrono::Weekday::*;
    match w {
        Mon => "Mon",
        Tue => "Tue",
        Wed => "Wed",
        Thu => "Thu",
        Fri => "Fri",
        Sat => "Sat",
        Sun => "Sun",
    }
}

pub const KIND: &str = "weather";

/// Wizard descriptor. Lat/lon are optional Text fields so users can leave
/// them blank to opt into IP geolocation; a validator rejects malformed
/// numeric input. The custom `render_toml` omits empty optionals so the
/// resulting `weather.toml` parses cleanly into `WeatherConfig`.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{
        ChoiceOption, WizardDescriptor, WizardField, WizardFieldKind, WizardValue,
    };

    fn validate_latitude(v: &WizardValue) -> Result<(), String> {
        if let WizardValue::Text(s) = v {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(());
            }
            match trimmed.parse::<f64>() {
                Ok(n) if (-90.0..=90.0).contains(&n) => Ok(()),
                Ok(_) => Err("Latitude must be between -90 and 90".into()),
                Err(_) => Err("Latitude must be a number (e.g. 49.166) or blank".into()),
            }
        } else {
            Ok(())
        }
    }

    fn validate_longitude(v: &WizardValue) -> Result<(), String> {
        if let WizardValue::Text(s) = v {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(());
            }
            match trimmed.parse::<f64>() {
                Ok(n) if (-180.0..=180.0).contains(&n) => Ok(()),
                Ok(_) => Err("Longitude must be between -180 and 180".into()),
                Err(_) => Err("Longitude must be a number (e.g. -123.133) or blank".into()),
            }
        } else {
            Ok(())
        }
    }

    WizardDescriptor {
        display_name: "Weather",
        blurb: "Open-Meteo current conditions and short-term forecast. \
                Leave latitude/longitude blank to use IP geolocation on \
                first fetch.",
        load_from_toml: None,
        render_toml: Some(render_weather_toml),
        fields: vec![
            WizardField {
                key: "label",
                label: "Location label",
                help: "Optional display name shown in the cell title \
                       (e.g. \"Richmond, BC\"). Falls back to the \
                       IP-geolocation result when blank.",
                required: false,
                kind: WizardFieldKind::Text {
                    default: None,
                    placeholder: Some("(use geolocation)"),
                },
                validate: None,
            },
            WizardField {
                key: "latitude",
                label: "Latitude",
                help: "Decimal degrees in [-90, 90]. Leave blank to \
                       IP-geolocate on first fetch.",
                required: false,
                kind: WizardFieldKind::Text {
                    default: None,
                    placeholder: Some("e.g. 49.166"),
                },
                validate: Some(validate_latitude),
            },
            WizardField {
                key: "longitude",
                label: "Longitude",
                help: "Decimal degrees in [-180, 180]. Leave blank to \
                       IP-geolocate on first fetch.",
                required: false,
                kind: WizardFieldKind::Text {
                    default: None,
                    placeholder: Some("e.g. -123.133"),
                },
                validate: Some(validate_longitude),
            },
            WizardField {
                key: "units",
                label: "Units",
                help: "\"metric\" — °C and km/h. \"imperial\" — °F and mph.",
                required: true,
                kind: WizardFieldKind::Choice {
                    options: vec![
                        ChoiceOption {
                            value: "metric",
                            label: "Metric (°C, km/h)",
                            help: None,
                        },
                        ChoiceOption {
                            value: "imperial",
                            label: "Imperial (°F, mph)",
                            help: None,
                        },
                    ],
                    default: Some("metric"),
                },
                validate: None,
            },
            WizardField {
                key: "poll_interval_secs",
                label: "Refresh interval (seconds)",
                help: "How often to fetch fresh conditions. Open-Meteo is \
                       fast and free; 600 (10 minutes) is plenty for a \
                       dashboard.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(600.0),
                    range: Some((30.0, 3600.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "auto_locate",
                label: "IP-geolocate when lat/lon are blank",
                help: "On — the widget calls ipapi.co on first fetch when \
                       no coordinates are configured. Off — the widget \
                       renders a \"location needed\" placeholder until \
                       coordinates are supplied.",
                required: false,
                kind: WizardFieldKind::Bool { default: true },
                validate: None,
            },
        ],
    }
}

/// Render weather.toml from wizard values. Optional fields (label, lat,
/// lon) are omitted when blank so the on-disk file parses cleanly into
/// `WeatherConfig` with its `Option<…>` shapes.
fn render_weather_toml(
    values: &std::collections::HashMap<String, crate::wizard::descriptor::WizardValue>,
    _existing: Option<&str>,
) -> String {
    use crate::wizard::descriptor::WizardValue;
    let mut out = String::from(
        "# Generated by `glint --setup`. Hand-edit freely; the wizard preserves\n\
         # advanced keys it doesn't manage (e.g. [colors], custom shortcuts).\n\n",
    );

    if let Some(WizardValue::Text(label)) = values.get("label") {
        let trimmed = label.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("label = {}\n", weather_toml_quote(trimmed)));
        }
    }
    if let Some(lat) = optional_float(values.get("latitude")) {
        out.push_str(&format!("latitude = {lat}\n"));
    }
    if let Some(lon) = optional_float(values.get("longitude")) {
        out.push_str(&format!("longitude = {lon}\n"));
    }
    if let Some(WizardValue::Choice(units)) = values.get("units") {
        out.push_str(&format!("units = {}\n", weather_toml_quote(units)));
    }
    if let Some(WizardValue::Number(secs)) = values.get("poll_interval_secs") {
        out.push_str(&format!("poll_interval_secs = {}\n", *secs as i64));
    }
    if let Some(WizardValue::Bool(b)) = values.get("auto_locate") {
        out.push_str(&format!("auto_locate = {b}\n"));
    }
    out
}

/// Coerce either a Text("49.166") or a Number(49.166) wizard value into an
/// f64. Empty / unparseable / wrong-kind inputs return None so the caller
/// can omit the field from the rendered TOML.
fn optional_float(v: Option<&crate::wizard::descriptor::WizardValue>) -> Option<f64> {
    use crate::wizard::descriptor::WizardValue;
    match v? {
        WizardValue::Text(s) => s.trim().parse().ok(),
        WizardValue::Number(n) => Some(*n),
        _ => None,
    }
}

fn weather_toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn build(ctx: &super::WidgetCtx) -> Box<dyn super::Widget> {
    let cfg: WeatherConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(WeatherWidget::with_config(
        ctx.instance.clone(),
        cfg,
        ctx.theme.clone(),
        ctx.cache.clone(),
    ))
}

#[cfg(test)]
mod tests;
