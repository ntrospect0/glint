// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Resources widget — htop-style CPU / memory / process view.
//! Backed by `sysinfo` (cross-platform; no FFI of our own).
//!
//! # Responsive tiers
//!
//! `Compact`, `Standard`, and `Expanded` render exactly as before Phase 2 —
//! no new content, no layout changes.
//!
//! `Full` (≥ 105 cols AND ≥ 30 rows — e.g., Focus Zoom or a large layout
//! cell) adds four enhancements, all gated on `ViewTier::Full`:
//!
//! 1. **CPU sparkline** — up to 10-row braille chart of the last ≤ 60
//!    aggregate CPU% readings, drawn above the per-core bars. Row count
//!    is `row_split(available, SPARKLINE_MAX_ROWS).0`; the process list
//!    receives the remainder.
//! 2. **Taller process list** — process count inferred from available rows
//!    (up to 40; respects the hard cap). Always 40 rows are sampled so the
//!    Full tier can show more without a fresh poll.
//! 3. **j/k navigation cursor** — selected row is highlighted with
//!    reversed-video at Full. Navigation keys are consumed only when
//!    rendered at Full so they don't steal 'm'/'r' shortcuts elsewhere.
//! 4. **Extra process columns** — ST (single-char status), THRD (thread
//!    count; Linux/Android only, '-' elsewhere), TIME (elapsed run_time).
//!    These fields are always captured from sysinfo — no additional
//!    `ProcessRefreshKind` flags required for `status()` or `run_time()`.
//!
//! # Network I/O — deferred
//!
//! Per-interface rx/tx rates require the `"network"` feature flag in
//! `sysinfo` (currently only `"system"` is enabled in Cargo.toml). Adding
//! that feature and the `Networks` refresh lifecycle is left for a
//! follow-on PR to keep this change self-contained.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use serde::Deserialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, RefreshKind, System};

use crate::text::truncate;
use crate::theme::{ColorScheme, Theme};
use crate::ui::chart::braille;
use crate::ui::{apply_title_row, MetadataEmphasis};
use crate::widgets::view_tier::row_split;
use crate::widgets::ViewTier;

use super::{AppContext, EventResult, Widget};

/// Maximum rows the CPU sparkline may claim at Full tier.
/// The process list receives whatever rows remain after the sparkline
/// and the header/CPU-bar/memory lines are accounted for.
const SPARKLINE_MAX_ROWS: u16 = 10;

/// Loaded from `~/.config/glint/resources.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ResourcesConfig {
    /// Background refresh cadence — used when the widget is *not* the
    /// focused pane. Each refresh walks every process on the system to
    /// pick the top-N, so going faster than this costs O(processes)
    /// syscalls. Clamped to ≥1s.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Fast refresh cadence used while the widget is the active stack
    /// child *and* holds keyboard focus. Defaults to 2s to match what
    /// users typically want while actively watching processes.
    #[serde(default = "default_focused_poll_interval")]
    pub focused_poll_interval_secs: u64,

    /// Top processes to surface at Standard/Expanded. Clamped to ≤40 at
    /// construction. At Full the displayed count is inferred from
    /// available rows (still capped at 40).
    #[serde(default = "default_top_n")]
    pub top_n_processes: usize,

    /// Sort processes by memory instead of CPU.
    #[serde(default)]
    pub sort_by_memory: bool,

    /// Per-widget overrides layered on the app theme.
    #[serde(default)]
    pub colors: ColorScheme,

    /// `Shift+<letter>` focus shortcuts; falls back to `['r', 'e', 's', 'm']`.
    #[serde(default)]
    pub shortcuts: Vec<char>,
}

fn default_poll_interval() -> u64 {
    5
}
fn default_focused_poll_interval() -> u64 {
    2
}
fn default_top_n() -> usize {
    10
}

/// How recently `render()` must have been called with `focused = true`
/// to count as "currently focused" for cadence purposes. Same value as
/// the stocks widget — see [`stocks::FOCUS_FRESHNESS_WINDOW`] for the
/// reasoning.
const FOCUS_FRESHNESS_WINDOW: Duration = Duration::from_secs(2);

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            focused_poll_interval_secs: default_focused_poll_interval(),
            top_n_processes: default_top_n(),
            sort_by_memory: false,
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

/// How many aggregate CPU% samples to keep in the sparkline ring buffer.
/// At the focused cadence (2 s/sample) this covers the last ~2 minutes;
/// at the background cadence (5 s/sample) it covers the last ~5 minutes.
const CPU_HISTORY_CAP: usize = 60;

/// Snapshot of one process row used by `render`. Kept owned + plain so
/// rendering doesn't hold the `System` mutex while painting.
#[derive(Debug, Clone)]
struct ProcRow {
    name: String,
    pid: u32,
    cpu_percent: f32,
    /// Resident set size — physical memory currently held (htop's "RES").
    memory_bytes: u64,
    /// Virtual memory size — total address space mapped by the process,
    /// including file-backed regions and anonymous mappings that haven't
    /// been touched (htop's "VIRT").
    virtual_bytes: u64,
    /// Single-char status abbreviation: R/S/Z/I/T/D/? (Full tier only).
    /// Derived from `sysinfo::Process::status()` which is always
    /// populated — no extra `ProcessRefreshKind` flag needed.
    status_char: char,
    /// Seconds the process has been running (`sysinfo::Process::run_time()`).
    /// Always populated alongside status; shown in Full tier.
    run_time_secs: u64,
    /// Number of threads (from `sysinfo::Process::tasks()`). `None` on
    /// platforms where `tasks()` returns `None` (everything except
    /// Linux/Android); displayed as `-` at Full tier.
    thread_count: Option<usize>,
}

/// Compact metrics snapshot — built inside the tick under the `System`
/// mutex and cloned into `render`.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    /// Per-core CPU usage 0..=100.
    cpu_per_core: Vec<f32>,
    total_memory: u64,
    used_memory: u64,
    total_swap: u64,
    used_swap: u64,
    /// `(one_min, five_min, fifteen_min)` load average; zeros on Windows.
    load_average: (f64, f64, f64),
    /// Seconds since boot.
    uptime_secs: u64,
    /// Pretty hostname or "(unknown)".
    hostname: String,
    /// Top processes (up to 40) sorted by the configured key. The Full
    /// tier infers a visible count from available rows; other tiers cap
    /// display at `config.top_n_processes`.
    top: Vec<ProcRow>,
    /// Aggregate CPU% history for the braille sparkline. Cloned from the
    /// ring buffer in `ResourcesState` on each snapshot rebuild.
    cpu_history: Vec<f32>,
    /// Currently-selected process index (0-based). Updated both on each
    /// refresh (to clamp against the new list length) and on j/k key
    /// presses (so the highlight moves without waiting for the next poll).
    selected_row: usize,
    fetched_at: Option<Instant>,
}

struct ResourcesState {
    system: System,
    last_refresh: Option<Instant>,
    /// Wall-clock timestamp of the most recent *full* process sweep
    /// (`ProcessesToUpdate::All`). Between sweeps we refresh just the
    /// PIDs in `tracked_pids` so a 500-process macOS doesn't pay an
    /// O(P) syscall on every tick — see
    /// [`FULL_SWEEP_INTERVAL`].
    last_full_sweep_at: Option<Instant>,
    /// PIDs of the most-recent top-N processes; refreshed every tick
    /// between full sweeps. New hot processes get picked up on the
    /// next full sweep (≤ FULL_SWEEP_INTERVAL latency).
    tracked_pids: Vec<Pid>,
    snapshot: Snapshot,
    /// Most-recent `render()` call where the widget held focus. See
    /// [`FOCUS_FRESHNESS_WINDOW`] — drives the fast vs slow cadence
    /// choice inside `refresh_if_due`.
    last_focused_at: Option<Instant>,
    /// Ring buffer of aggregate CPU% readings for the Full-tier sparkline.
    /// Capped at [`CPU_HISTORY_CAP`] entries; oldest dropped on overflow.
    cpu_history: VecDeque<f32>,
}

impl Default for ResourcesState {
    fn default() -> Self {
        // Boot the System with everything off so the first explicit
        // refresh decides what to load — no surprise allocations.
        Self {
            system: System::new_with_specifics(RefreshKind::new()),
            last_refresh: None,
            last_full_sweep_at: None,
            tracked_pids: Vec::new(),
            snapshot: Snapshot::default(),
            last_focused_at: None,
            cpu_history: VecDeque::new(),
        }
    }
}

/// How long between *full* process sweeps. Partial sweeps in between
/// refresh just the top-N tracked PIDs (cheap). A new hot process
/// becomes visible at most this long after it starts running — long
/// enough to skip the O(processes) walk on most ticks, short enough
/// that the dashboard still feels live.
const FULL_SWEEP_INTERVAL: Duration = Duration::from_secs(20);

pub struct ResourcesWidget {
    id: String,
    instance: String,
    display_name_cache: String,
    config: ResourcesConfig,
    state: Arc<Mutex<ResourcesState>>,
    /// Slow cadence used when the widget isn't the focused pane.
    background_poll_interval: Duration,
    /// Fast cadence used while the widget holds focus.
    focused_poll_interval: Duration,
    /// App-level theme; cached for `apply_config` / `:scheme` reloads.
    app_theme: Arc<Theme>,
    /// Merged theme (app + widget `[colors]` overrides).
    theme: Theme,
    shortcut: Option<char>,
    /// Effective shortcut preference list (TOML override or built-in).
    shortcut_prefs: Vec<char>,
    /// Display-state dirty flag drained by `take_dirty`. True on
    /// construction so the first render lands; subsequently flipped
    /// only when `refresh_if_due` actually re-samples sysinfo.
    dirty: bool,
}

impl ResourcesWidget {
    pub fn with_config(instance: String, config: ResourcesConfig, app_theme: Arc<Theme>) -> Self {
        let id = if instance == "main" {
            "resources".to_string()
        } else {
            format!("resources@{instance}")
        };
        let display_name_cache = if instance == "main" {
            "Resources".to_string()
        } else {
            format!("Resources ({instance})")
        };
        let theme = app_theme.with_overrides(&config.colors);
        let shortcut_prefs = if config.shortcuts.is_empty() {
            vec!['r', 'e', 's', 'm']
        } else {
            config.shortcuts.clone()
        };
        // Floor each interval at 1s — sysinfo's CPU sampling needs at
        // least ~200ms between calls to produce a stable %; sub-second
        // ticking is mostly visual noise on a dashboard.
        let background = config.poll_interval_secs.max(1);
        let focused = config.focused_poll_interval_secs.max(1);
        Self {
            id,
            instance,
            display_name_cache,
            config,
            state: Arc::new(Mutex::new(ResourcesState::default())),
            background_poll_interval: Duration::from_secs(background),
            focused_poll_interval: Duration::from_secs(focused),
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
            dirty: true,
        }
    }

    /// Returns `true` when this call actually re-sampled sysinfo (i.e.
    /// the display will change); `false` when the poll interval hadn't
    /// elapsed yet. The boolean drives the per-widget dirty flag.
    fn refresh_if_due(&self) -> bool {
        let mut st = self.state.lock().expect("resources state poisoned");
        let now = Instant::now();
        // Pick fast cadence only when the widget has been rendered with
        // focus very recently — otherwise the background scan is fine.
        let focused_now = st
            .last_focused_at
            .map(|t| t.elapsed() < FOCUS_FRESHNESS_WINDOW)
            .unwrap_or(false);
        let interval = if focused_now {
            self.focused_poll_interval
        } else {
            self.background_poll_interval
        };
        let due = match st.last_refresh {
            None => true,
            Some(t) => now.duration_since(t) >= interval,
        };
        if !due {
            return false;
        }
        // Decide *what* we're refreshing this tick. A full sweep walks
        // every process on the system (O(P) syscalls — ~500 on macOS);
        // a partial sweep only refreshes the PIDs already in our top-N
        // list (O(N), typically ≤ 40). We pay for the full sweep at
        // most once per FULL_SWEEP_INTERVAL, so a previously-quiet
        // process that suddenly spikes becomes visible within that
        // window. `refresh_cpu_usage` + `refresh_memory` are cheap
        // either way (single syscall each).
        let full_sweep = match st.last_full_sweep_at {
            None => true,
            Some(t) => now.duration_since(t) >= FULL_SWEEP_INTERVAL,
        };
        st.system.refresh_cpu_usage();
        st.system.refresh_memory();
        if full_sweep || st.tracked_pids.is_empty() {
            st.system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::new().with_cpu().with_memory(),
            );
            st.last_full_sweep_at = Some(now);
        } else {
            // Have to copy the PIDs first — `refresh_processes_specifics`
            // borrows `&self.system` mutably while we hand it the slice.
            let pids: Vec<Pid> = st.tracked_pids.clone();
            st.system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&pids),
                false,
                ProcessRefreshKind::new().with_cpu().with_memory(),
            );
        }

        let cpu_per_core: Vec<f32> = st.system.cpus().iter().map(|c| c.cpu_usage()).collect();
        let total_memory = st.system.total_memory();
        let used_memory = st.system.used_memory();
        let total_swap = st.system.total_swap();
        let used_swap = st.system.used_swap();
        let load = System::load_average();
        let load_average = (load.one, load.five, load.fifteen);
        let uptime_secs = System::uptime();
        let hostname = System::host_name().unwrap_or_else(|| "(unknown)".into());

        // Push the aggregate CPU% into the sparkline ring buffer.
        let avg_cpu = if cpu_per_core.is_empty() {
            0.0
        } else {
            cpu_per_core.iter().sum::<f32>() / cpu_per_core.len() as f32
        };
        st.cpu_history.push_back(avg_cpu);
        if st.cpu_history.len() > CPU_HISTORY_CAP {
            st.cpu_history.pop_front();
        }

        // Always collect up to 40 processes regardless of the configured
        // top_n. The render layer decides how many to display based on
        // the available rows: Standard/Expanded/Compact show
        // `config.top_n_processes`; Full shows as many as fit.
        let row_from = |pid: u32, p: &sysinfo::Process| ProcRow {
            name: p.name().to_string_lossy().into_owned(),
            pid,
            cpu_percent: p.cpu_usage(),
            memory_bytes: p.memory(),
            virtual_bytes: p.virtual_memory(),
            status_char: proc_status_char(p.status()),
            run_time_secs: p.run_time(),
            thread_count: p.tasks().map(|t| t.len()),
        };
        let mut rows: Vec<ProcRow> = if full_sweep {
            st.system
                .processes()
                .iter()
                .map(|(pid, p)| row_from(pid.as_u32(), p))
                // Drop the bookkeeping-noise row — zero CPU AND zero
                // memory rows are kernel placeholders on some platforms.
                .filter(|r| r.cpu_percent > 0.0 || r.memory_bytes > 0)
                .collect()
        } else {
            // Look each tracked PID up against the freshly-partial-
            // refreshed `system`. A process that exited between
            // sweeps simply disappears here; the next full sweep
            // backfills the top-N with whatever's hot then.
            st.tracked_pids
                .iter()
                .filter_map(|pid| st.system.process(*pid).map(|p| row_from(pid.as_u32(), p)))
                .filter(|r| r.cpu_percent > 0.0 || r.memory_bytes > 0)
                .collect()
        };
        if self.config.sort_by_memory {
            rows.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes));
        } else {
            rows.sort_by(|a, b| {
                b.cpu_percent
                    .partial_cmp(&a.cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        // Hard cap at 40 — enough for any view tier without blowing up
        // the per-tick memory budget.
        rows.truncate(40);
        // Capture the new top-N PIDs so the next partial sweep refreshes
        // just those. After a full sweep this is the rediscovered set;
        // after a partial sweep it's the same set in (possibly) a new
        // order — that's fine, we're tracking PIDs not positions.
        st.tracked_pids = rows.iter().map(|r| Pid::from_u32(r.pid)).collect();

        // Clamp the selection cursor to the new list length so j/k
        // navigation stays valid across process list changes.
        let prev_selected = st.snapshot.selected_row;
        let clamped_selected = prev_selected.min(rows.len().saturating_sub(1));

        st.snapshot = Snapshot {
            cpu_per_core,
            total_memory,
            used_memory,
            total_swap,
            used_swap,
            load_average,
            uptime_secs,
            hostname,
            top: rows,
            cpu_history: st.cpu_history.iter().cloned().collect(),
            selected_row: clamped_selected,
            fetched_at: Some(now),
        };
        st.last_refresh = Some(now);
        true
    }

    fn snapshot(&self) -> Snapshot {
        self.state
            .lock()
            .expect("resources state poisoned")
            .snapshot
            .clone()
    }
}

/// Map `sysinfo::ProcessStatus` to a single display character.
/// The mapping aims to match traditional Unix `ps` output:
///   R = running/runnable, S = sleeping, Z = zombie, I = idle,
///   T = stopped, D = uninterruptible disk sleep, ? = unknown.
fn proc_status_char(s: ProcessStatus) -> char {
    match s {
        ProcessStatus::Run => 'R',
        ProcessStatus::Sleep => 'S',
        ProcessStatus::Zombie => 'Z',
        ProcessStatus::Idle => 'I',
        ProcessStatus::Stop | ProcessStatus::Tracing => 'T',
        ProcessStatus::Dead => 'D',
        ProcessStatus::UninterruptibleDiskSleep => 'D',
        ProcessStatus::Parked | ProcessStatus::Waking | ProcessStatus::Wakekill => 'P',
        ProcessStatus::LockBlocked => 'L',
        ProcessStatus::Unknown(_) => '?',
    }
}

/// Format elapsed run_time (seconds) as a compact ≤5-char string:
///   ` 0:00`  — zero (MM:SS, 5 chars)
///   `12:34`  — minutes and seconds
///   ` 1:05`  — hours and minutes (when h > 0)
///   `999h+`  — capped at 999h for very long-running processes
fn format_run_time_compact(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h >= 100 {
        // Cap at 999h+ (5 chars) so the column never overflows.
        format!("{:>3}h+", h.min(999))
    } else if h > 0 {
        format!("{h:>2}:{m:02}")
    } else {
        format!("{m:>2}:{s:02}")
    }
}

/// Format thread count for the THRD column (4 chars, right-aligned).
/// Returns `"   -"` on platforms where `tasks()` is not supported.
fn format_thread_count(count: Option<usize>) -> String {
    match count {
        Some(n) => format!("{n:>4}"),
        None => "   -".to_string(),
    }
}

/// Render a unicode-block progress bar of length `width` for `pct` (0..=100).
fn bar(pct: f32, width: u16) -> String {
    let width = width as usize;
    if width == 0 {
        return String::new();
    }
    let pct = pct.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * width as f32).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    let mut s = String::with_capacity(width);
    for _ in 0..filled {
        s.push('█');
    }
    for _ in 0..empty {
        s.push('░');
    }
    s
}

fn humanize_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if b >= TB {
        format!("{:.2} TB", b as f64 / TB as f64)
    } else if b >= GB {
        format!("{:.2} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.0} KB", b as f64 / KB as f64)
    } else {
        format!("{b} B")
    }
}

/// Compact byte format for the cramped process table: `512`, `12K`,
/// `5.0M`, `1.23G`, `4.5T` (5 chars max, no space). Used for both RES
/// and VIRT columns so they share an alignment width.
fn compact_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    if b >= TB {
        format!("{:.1}T", b as f64 / TB as f64)
    } else if b >= GB {
        format!("{:.2}G", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.1}M", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.0}K", b as f64 / KB as f64)
    } else {
        format!("{b}")
    }
}

fn format_uptime(secs: u64) -> String {
    crate::format::uptime_label(secs)
}

#[async_trait]
impl Widget for ResourcesWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        "resources"
    }

    fn instance(&self) -> &str {
        &self.instance
    }

    fn display_name(&self) -> &str {
        &self.display_name_cache
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        // sysinfo work is synchronous CPU work; running it on the
        // tokio worker is fine because each refresh is short.
        if self.refresh_if_due() {
            self.dirty = true;
        }
        Ok(())
    }

    fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        // Record focus so the next `refresh_if_due()` can pick fast vs
        // slow cadence. Hidden stack children don't get render() at
        // all, so a stale `Some(t)` naturally ages out via the
        // `FOCUS_FRESHNESS_WINDOW` check.
        self.state
            .lock()
            .expect("resources state poisoned")
            .last_focused_at = focused.then(Instant::now);

        let tier = ViewTier::from_rect(area);

        let title_base = if self.instance == "main" {
            "Resources".to_string()
        } else {
            format!("Resources ({})", self.instance)
        };
        let block = apply_title_row(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(self.theme.border_style(focused)),
            focused,
            &title_base,
            None,
            MetadataEmphasis::Default,
            self.shortcut,
            &self.theme,
            area.width,
        );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 4 || inner.height < 4 {
            return;
        }

        let snap = self.snapshot();
        let mut lines: Vec<Line<'static>> = Vec::new();

        // Header: hostname · uptime · load avg.
        let load = snap.load_average;
        let mut header = format!(
            "{}  ·  up {}",
            snap.hostname,
            format_uptime(snap.uptime_secs)
        );
        // load_average() is 0,0,0 on Windows; only show when meaningful.
        if load.0 > 0.0 || load.1 > 0.0 || load.2 > 0.0 {
            header.push_str(&format!(
                "  ·  load {:.2} {:.2} {:.2}",
                load.0, load.1, load.2
            ));
        }
        lines.push(Line::from(Span::styled(header, self.theme.text_brilliant)));
        lines.push(Line::from(""));

        // CPU section — per-core bars. Aggregate % goes in the section title.
        if !snap.cpu_per_core.is_empty() {
            let avg = snap.cpu_per_core.iter().sum::<f32>() / snap.cpu_per_core.len() as f32;
            lines.push(Line::from(vec![
                Span::styled("CPU", self.theme.text_focused),
                Span::raw(format!(
                    "   {avg:>5.1}% avg across {} cores",
                    snap.cpu_per_core.len()
                )),
            ]));

            // At Full: braille sparkline of aggregate CPU% history (up to
            // SPARKLINE_MAX_ROWS rows), inserted between the section header
            // and the per-core bars. The sparkline claims as many rows as the
            // available inner height allows up to the cap; the process list
            // then inherits whatever remains (accounted via `lines.len()` in
            // the proc_display_count block below). Skipped at all other tiers.
            if tier == ViewTier::Full && snap.cpu_history.len() >= 2 {
                let h_f64: Vec<f64> = snap.cpu_history.iter().map(|&v| v as f64).collect();
                // Rows consumed so far (including CPU section header just pushed).
                // `row_split` caps the sparkline at SPARKLINE_MAX_ROWS; the
                // process list picks up the difference via lines.len() later.
                let rows_used_so_far = lines.len() as u16;
                let available = inner.height.saturating_sub(rows_used_so_far);
                let (spark_rows, _) = row_split(available, SPARKLINE_MAX_ROWS);
                let spark_cols = inner.width;
                let sparklines =
                    braille::render_series(&h_f64, spark_rows, spark_cols, 0.0, 100.0);
                for row in sparklines {
                    lines.push(Line::from(Span::styled(row, self.theme.text_focused)));
                }
            }

            // Per-core layout. `prefix_cols` is the non-bar overhead
            // each row carries: "CPUxx  100.0% " = 5 + 2 + 6 + 1 = 14.
            // Two-column packing halves the vertical real estate the
            // CPU section eats — meaningful on machines with 8+ cores
            // where the process list otherwise gets cropped. Fall
            // back to single-column when the pane is too narrow to
            // host two readable bars side-by-side.
            const PREFIX_COLS: u16 = 14;
            const COL_GUTTER: u16 = 2;
            const TRAILING_PAD: u16 = 2;
            const MIN_BAR_PER_COL: u16 = 5;
            let two_col_min_width =
                PREFIX_COLS * 2 + COL_GUTTER + TRAILING_PAD + MIN_BAR_PER_COL * 2;
            let render_row = |idx: usize, pct: f32, bar_width: u16| -> Vec<Span<'static>> {
                let label = format!("CPU{:<2}", idx);
                let bar_str = bar(pct, bar_width);
                vec![
                    Span::styled(label, self.theme.text_dim),
                    Span::raw(format!("  {pct:>5.1}% ")),
                    Span::styled(bar_str, self.theme.text_focused),
                ]
            };
            if inner.width >= two_col_min_width {
                // Balanced columns: left half holds cores 0..rows,
                // right half holds cores rows..n. Odd core counts
                // leave the last row's right cell empty rather than
                // creating a ragged left column.
                let bar_width = (inner.width - PREFIX_COLS * 2 - COL_GUTTER - TRAILING_PAD) / 2;
                let n = snap.cpu_per_core.len();
                let rows_count = n.div_ceil(2);
                for r in 0..rows_count {
                    let left_idx = r;
                    let right_idx = r + rows_count;
                    let mut spans = render_row(left_idx, snap.cpu_per_core[left_idx], bar_width);
                    spans.push(Span::raw(" ".repeat(COL_GUTTER as usize)));
                    if right_idx < n {
                        spans.extend(render_row(
                            right_idx,
                            snap.cpu_per_core[right_idx],
                            bar_width,
                        ));
                    }
                    lines.push(Line::from(spans));
                }
            } else {
                let bar_width = inner.width.saturating_sub(PREFIX_COLS + TRAILING_PAD);
                for (i, pct) in snap.cpu_per_core.iter().enumerate() {
                    lines.push(Line::from(render_row(i, *pct, bar_width)));
                }
            }
            lines.push(Line::from(""));
        }

        // Memory + swap.
        let mem_pct = if snap.total_memory > 0 {
            (snap.used_memory as f64 / snap.total_memory as f64 * 100.0) as f32
        } else {
            0.0
        };
        lines.push(Line::from(vec![
            Span::styled("MEM", self.theme.text_focused),
            Span::raw(format!(
                "   {} / {}  ({:.1}%)",
                humanize_bytes(snap.used_memory),
                humanize_bytes(snap.total_memory),
                mem_pct
            )),
        ]));
        let bar_width = inner.width.saturating_sub(16);
        lines.push(Line::from(vec![
            Span::styled("       ", self.theme.text_dim),
            Span::styled(bar(mem_pct, bar_width), self.theme.text_focused),
        ]));
        if snap.total_swap > 0 {
            let swap_pct = (snap.used_swap as f64 / snap.total_swap as f64 * 100.0) as f32;
            lines.push(Line::from(vec![
                Span::styled("SWAP", self.theme.text_focused),
                Span::raw(format!(
                    "  {} / {}  ({:.1}%)",
                    humanize_bytes(snap.used_swap),
                    humanize_bytes(snap.total_swap),
                    swap_pct
                )),
            ]));
        }
        lines.push(Line::from(""));

        // Processes section.
        //
        // Table layout at Full (≥105 cols AND ≥30 rows):
        //   PID   CPU%  ST  THRD   TIME     RES    VIRT   COMMAND
        //   extra columns: ST (status char), THRD (thread count), TIME (run_time)
        //   fixed prefix width = 48 chars, leaving remainder for COMMAND.
        //
        // Table layout at Compact / Standard / Expanded — unchanged from
        // pre-Phase-2:
        //   wide (≥50 inner cols):  PID   CPU%   RES    VIRT   COMMAND
        //   narrow                :  PID   CPU%   RES    COMMAND
        //
        // Process display count:
        //   Full — inferred from remaining inner rows after all header/CPU/
        //          memory lines, capped at 40.
        //   Other tiers — config.top_n_processes (as today).
        let proc_title = if self.config.sort_by_memory {
            "Top processes (by memory)"
        } else {
            "Top processes (by CPU)"
        };
        lines.push(Line::from(Span::styled(
            proc_title,
            self.theme.text_selected,
        )));

        let show_full_table = tier == ViewTier::Full;

        // Compute how many process rows to show.
        // At Full we infer from available height; at other tiers we use
        // the configured default (same behaviour as before).
        let proc_display_count = if show_full_table {
            // Estimate overhead rows already in `lines`:
            //   lines.len() + 1 (proc header below) = used rows
            // Add 1 for the process column header line we're about to push.
            let used = lines.len() as u16 + 1;
            (inner.height.saturating_sub(used) as usize).min(40).max(1)
        } else {
            self.config.top_n_processes.min(snap.top.len())
        };

        if show_full_table {
            // Full-tier process table with extra columns.
            // Format: "{:>6}  {:>5.1}  {:<2} {:>4}  {:>5}  {:>6}  {:>6}   {}"
            //          PID     CPU%  ST  THRD   TIME    RES    VIRT  NAME
            // Fixed prefix width = 6+2+5+2+2+1+4+2+5+2+6+2+6+3 = 48
            const FULL_FIXED_PREFIX: usize = 48;
            let name_room = (inner.width as usize).saturating_sub(FULL_FIXED_PREFIX).max(6);
            lines.push(Line::from(Span::styled(
                "  PID   CPU%  ST  THRD   TIME     RES    VIRT   COMMAND",
                self.theme.text_dim,
            )));
            let selected_style = Style::default().add_modifier(Modifier::REVERSED);
            for (idx, row) in snap.top.iter().take(proc_display_count).enumerate() {
                let name = truncate(&row.name, name_room);
                let line_str = format!(
                    "{:>6}  {:>5.1}  {:<2} {}  {:>5}  {:>6}  {:>6}   {}",
                    row.pid,
                    row.cpu_percent,
                    row.status_char,
                    format_thread_count(row.thread_count),
                    format_run_time_compact(row.run_time_secs),
                    compact_bytes(row.memory_bytes),
                    compact_bytes(row.virtual_bytes),
                    name
                );
                if idx == snap.selected_row {
                    lines.push(Line::from(Span::styled(line_str, selected_style)));
                } else {
                    lines.push(Line::from(line_str));
                }
            }
        } else {
            // Compact / Standard / Expanded — identical to pre-Phase-2.
            let show_virt = inner.width >= 50;
            let header = if show_virt {
                "  PID   CPU%     RES    VIRT   COMMAND"
            } else {
                "  PID   CPU%     RES   COMMAND"
            };
            lines.push(Line::from(Span::styled(header, self.theme.text_dim)));
            let fixed_prefix = if show_virt { 35 } else { 27 };
            let name_room = (inner.width as usize).saturating_sub(fixed_prefix).max(6);
            for row in snap.top.iter().take(proc_display_count) {
                let name = truncate(&row.name, name_room);
                let line = if show_virt {
                    format!(
                        "{:>6}  {:>5.1}  {:>6}  {:>6}   {}",
                        row.pid,
                        row.cpu_percent,
                        compact_bytes(row.memory_bytes),
                        compact_bytes(row.virtual_bytes),
                        name
                    )
                } else {
                    format!(
                        "{:>6}  {:>5.1}  {:>6}   {}",
                        row.pid,
                        row.cpu_percent,
                        compact_bytes(row.memory_bytes),
                        name
                    )
                };
                lines.push(Line::from(line));
            }
        }

        // First-fetch placeholder.
        if snap.fetched_at.is_none() {
            let body = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Sampling system…",
                    Style::default().add_modifier(Modifier::DIM),
                )),
            ])
            .alignment(Alignment::Center);
            frame.render_widget(body, inner);
            return;
        }

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
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
            // `j` / `k` — move the Full-tier selection cursor down / up.
            // The cursor exists in state at all tiers but is only rendered
            // (and useful) at Full. We update it unconditionally here so
            // it's in the right position when the view next reaches Full.
            KeyCode::Char('j') => {
                let mut st = self.state.lock().expect("resources state poisoned");
                let max = st.snapshot.top.len().saturating_sub(1);
                let new = (st.snapshot.selected_row + 1).min(max);
                st.snapshot.selected_row = new;
                self.dirty = true;
                EventResult::Handled
            }
            KeyCode::Char('k') => {
                let mut st = self.state.lock().expect("resources state poisoned");
                let new = st.snapshot.selected_row.saturating_sub(1);
                st.snapshot.selected_row = new;
                self.dirty = true;
                EventResult::Handled
            }
            // `m` toggles between sort-by-CPU and sort-by-memory.
            KeyCode::Char('m') => {
                self.config.sort_by_memory = !self.config.sort_by_memory;
                // Re-sort the existing snapshot right away so the user
                // doesn't have to wait for the next tick.
                let mut st = self.state.lock().expect("resources state poisoned");
                if self.config.sort_by_memory {
                    st.snapshot
                        .top
                        .sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes));
                } else {
                    st.snapshot.top.sort_by(|a, b| {
                        b.cpu_percent
                            .partial_cmp(&a.cpu_percent)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                }
                EventResult::Handled
            }
            // `r` forces a refresh on the next tick.
            KeyCode::Char('r') => {
                let mut st = self.state.lock().expect("resources state poisoned");
                st.last_refresh = None;
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
            ("j/k", "move selection cursor (Full view)"),
            ("m", "toggle sort: CPU ↔ memory"),
            ("r", "force refresh on next tick"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "poll_interval_secs": self.background_poll_interval.as_secs(),
            "focused_poll_interval_secs": self.focused_poll_interval.as_secs(),
            "top_n_processes": self.config.top_n_processes,
            "sort_by_memory": self.config.sort_by_memory,
        })
    }

    fn apply_config(&mut self, config: serde_json::Value) -> Result<()> {
        let new_config: ResourcesConfig =
            serde_json::from_value(config).context("invalid resources config payload")?;
        let app_theme = self.app_theme.clone();
        let instance = self.instance.clone();
        *self = Self::with_config(instance, new_config, app_theme);
        Ok(())
    }

    fn set_app_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme.with_overrides(&self.config.colors);
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

pub const KIND: &str = "resources";

/// Wizard descriptor. Three flat scalar fields; default field-by-field
/// TOML renderer handles emission.
pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    use crate::wizard::descriptor::{WizardDescriptor, WizardField, WizardFieldKind};
    WizardDescriptor {
        display_name: "Resources",
        blurb: "htop-style CPU, memory, and top-process view. Backed by \
                the `sysinfo` crate (cross-platform, no FFI).",
        load_from_toml: None,
        render_toml: None,
        fields: vec![
            WizardField {
                key: "poll_interval_secs",
                label: "Background refresh interval (seconds)",
                help: "Sample CPU / memory / processes at this cadence \
                       when the widget is *not* the focused pane. Each \
                       refresh walks every process on the system, so a \
                       slower default keeps idle CPU low; the widget \
                       speeds up to `focused_poll_interval_secs` while \
                       it has focus. Clamped to ≥1s.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(5.0),
                    range: Some((1.0, 60.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "focused_poll_interval_secs",
                label: "Focused refresh interval (seconds)",
                help: "Cadence used while the widget is the active stack \
                       child and holds keyboard focus. Defaults to 2s \
                       (sysinfo needs ~200ms between samples for stable \
                       CPU %). Clamped to ≥1s.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(2.0),
                    range: Some((1.0, 60.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "top_n_processes",
                label: "Top processes to show",
                help: "Number of process rows at Standard/Expanded. At \
                       Full (zoomed) the count is inferred from available \
                       rows. Clamped to ≤40.",
                required: true,
                kind: WizardFieldKind::Number {
                    default: Some(10.0),
                    range: Some((1.0, 40.0)),
                    integer: true,
                },
                validate: None,
            },
            WizardField {
                key: "sort_by_memory",
                label: "Sort processes by memory",
                help: "Off — sort by CPU usage (htop's default). On — sort \
                       by RSS memory. Press `m` in the widget to toggle at \
                       runtime.",
                required: false,
                kind: WizardFieldKind::Bool { default: false },
                validate: None,
            },
        ],
    }
}

pub fn build(ctx: &super::WidgetCtx) -> Box<dyn super::Widget> {
    let cfg: ResourcesConfig =
        crate::config::load_widget_toml_for_instance(KIND, &ctx.instance).unwrap_or_default();
    Box::new(ResourcesWidget::with_config(
        ctx.instance.clone(),
        cfg,
        ctx.theme.clone(),
    ))
}

#[cfg(test)]
mod tests;
