//! Resources widget — htop-style CPU / memory / process view.
//! Backed by `sysinfo` (cross-platform; no FFI of our own).

use std::{
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
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

use crate::theme::{ColorScheme, Theme};
use crate::ui::apply_title_row;

use super::{AppContext, EventResult, Widget};

/// Loaded from `~/.config/glint/resources.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ResourcesConfig {
    /// Clamped to ≥1s (sub-second refresh hammers sysinfo for no visual gain).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Top processes to surface. Clamped to ≤40 at construction.
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
    2
}
fn default_top_n() -> usize {
    10
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            top_n_processes: default_top_n(),
            sort_by_memory: false,
            colors: ColorScheme::default(),
            shortcuts: Vec::new(),
        }
    }
}

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
    /// Top-N processes by the configured sort key.
    top: Vec<ProcRow>,
    fetched_at: Option<Instant>,
}

struct ResourcesState {
    system: System,
    last_refresh: Option<Instant>,
    snapshot: Snapshot,
}

impl Default for ResourcesState {
    fn default() -> Self {
        // Boot the System with everything off so the first explicit
        // refresh decides what to load — no surprise allocations.
        Self {
            system: System::new_with_specifics(RefreshKind::new()),
            last_refresh: None,
            snapshot: Snapshot::default(),
        }
    }
}

pub struct ResourcesWidget {
    id: String,
    instance: String,
    display_name_cache: String,
    config: ResourcesConfig,
    state: Arc<Mutex<ResourcesState>>,
    poll_interval: Duration,
    /// App-level theme; cached for `apply_config` / `:scheme` reloads.
    app_theme: Arc<Theme>,
    /// Merged theme (app + widget `[colors]` overrides).
    theme: Theme,
    shortcut: Option<char>,
    /// Effective shortcut preference list (TOML override or built-in).
    shortcut_prefs: Vec<char>,
}

impl ResourcesWidget {
    pub fn with_config(
        instance: String,
        config: ResourcesConfig,
        app_theme: Arc<Theme>,
    ) -> Self {
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
        // Floor the interval at 1s — sysinfo's CPU sampling needs at
        // least ~200ms between calls to produce a stable %; sub-second
        // ticking is mostly visual noise on a dashboard.
        let interval_secs = config.poll_interval_secs.max(1);
        Self {
            id,
            instance,
            display_name_cache,
            config,
            state: Arc::new(Mutex::new(ResourcesState::default())),
            poll_interval: Duration::from_secs(interval_secs),
            app_theme,
            theme,
            shortcut: None,
            shortcut_prefs,
        }
    }

    fn refresh_if_due(&self) {
        let mut st = self.state.lock().expect("resources state poisoned");
        let now = Instant::now();
        let due = match st.last_refresh {
            None => true,
            Some(t) => now.duration_since(t) >= self.poll_interval,
        };
        if !due {
            return;
        }
        // Refresh just what we render. `refresh_cpu_usage` requires a
        // prior baseline call; `sysinfo` handles the bookkeeping for us
        // as long as we keep the same `System` instance across calls.
        st.system.refresh_cpu_usage();
        st.system.refresh_memory();
        st.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::new().with_cpu().with_memory(),
        );

        let cpu_per_core: Vec<f32> = st.system.cpus().iter().map(|c| c.cpu_usage()).collect();
        let total_memory = st.system.total_memory();
        let used_memory = st.system.used_memory();
        let total_swap = st.system.total_swap();
        let used_swap = st.system.used_swap();
        let load = System::load_average();
        let load_average = (load.one, load.five, load.fifteen);
        let uptime_secs = System::uptime();
        let hostname = System::host_name().unwrap_or_else(|| "(unknown)".into());

        let mut rows: Vec<ProcRow> = st
            .system
            .processes()
            .iter()
            .map(|(pid, p)| ProcRow {
                name: p.name().to_string_lossy().into_owned(),
                pid: pid.as_u32(),
                cpu_percent: p.cpu_usage(),
                memory_bytes: p.memory(),
                virtual_bytes: p.virtual_memory(),
            })
            // Drop the row that is just bookkeeping noise — zero CPU AND
            // zero memory rows are kernel placeholders on some platforms.
            .filter(|r| r.cpu_percent > 0.0 || r.memory_bytes > 0)
            .collect();
        if self.config.sort_by_memory {
            rows.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes));
        } else {
            rows.sort_by(|a, b| {
                b.cpu_percent
                    .partial_cmp(&a.cpu_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        let top_n = self.config.top_n_processes.min(40);
        rows.truncate(top_n);

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
            fetched_at: Some(now),
        };
        st.last_refresh = Some(now);
    }

    fn snapshot(&self) -> Snapshot {
        self.state
            .lock()
            .expect("resources state poisoned")
            .snapshot
            .clone()
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
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
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
        self.refresh_if_due();
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
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
                Span::raw(format!("   {avg:>5.1}% avg across {} cores", snap.cpu_per_core.len())),
            ]));
            // Bar width = inner width minus a small fixed prefix
            // ("CPUxx 100.0% " = 14 chars) so labels never get clipped.
            let prefix_cols: u16 = 14;
            let bar_width = inner.width.saturating_sub(prefix_cols + 2);
            for (i, pct) in snap.cpu_per_core.iter().enumerate() {
                let pct = *pct;
                let label = format!("CPU{:<2}", i);
                let bar_str = bar(pct, bar_width);
                lines.push(Line::from(vec![
                    Span::styled(label, self.theme.text_dim),
                    Span::raw(format!("  {pct:>5.1}% ")),
                    Span::styled(bar_str, self.theme.text_focused),
                ]));
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
            let swap_pct =
                (snap.used_swap as f64 / snap.total_swap as f64 * 100.0) as f32;
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

        // Processes header + rows. Two layouts:
        //   wide (≥ 50 inner cols):  PID   CPU%   RES    VIRT   COMMAND
        //   narrow                :  PID   CPU%   RES    COMMAND
        // VIRT is the per-process virtual memory size (mapped address
        // space). On a 30%-of-screen pane the column doesn't fit, so we
        // drop it and use the freed room for the command name.
        let proc_title = if self.config.sort_by_memory {
            "Top processes (by memory)"
        } else {
            "Top processes (by CPU)"
        };
        lines.push(Line::from(Span::styled(proc_title, self.theme.text_selected)));
        let show_virt = inner.width >= 50;
        let header = if show_virt {
            "  PID   CPU%     RES    VIRT   COMMAND"
        } else {
            "  PID   CPU%     RES   COMMAND"
        };
        lines.push(Line::from(Span::styled(header, self.theme.text_dim)));
        let fixed_prefix = if show_virt { 35 } else { 27 };
        let name_room = (inner.width as usize)
            .saturating_sub(fixed_prefix)
            .max(6);
        for row in &snap.top {
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
            ("m", "toggle sort: CPU ↔ memory"),
            ("r", "force refresh on next tick"),
        ]
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({
            "poll_interval_secs": self.poll_interval.as_secs(),
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

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut out: String = chars.into_iter().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
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
                label: "Refresh interval (seconds)",
                help: "How often to sample CPU / memory / processes. \
                       Clamped to ≥1 second at runtime — sysinfo's CPU \
                       sampling needs ~200ms between calls for a stable %.",
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
                help: "Number of process rows under the CPU / memory bars. \
                       Clamped to ≤40 at construction so a misconfigured \
                       value can't blow out the render budget.",
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
mod tests {
    use super::*;

    fn make_widget() -> ResourcesWidget {
        ResourcesWidget::with_config(
            "main".to_string(),
            ResourcesConfig::default(),
            Arc::new(Theme::builtin_defaults()),
        )
    }

    #[test]
    fn compact_bytes_uses_single_letter_suffix() {
        assert_eq!(compact_bytes(0), "0");
        assert_eq!(compact_bytes(900), "900");
        assert_eq!(compact_bytes(1024), "1K");
        assert!(compact_bytes(5 * 1024 * 1024).starts_with("5.0M"));
        assert!(compact_bytes(1024u64.pow(3)).starts_with("1.00G"));
        assert!(compact_bytes(2 * 1024u64.pow(4)).starts_with("2.0T"));
    }

    #[test]
    fn humanize_bytes_picks_unit() {
        assert_eq!(humanize_bytes(0), "0 B");
        assert_eq!(humanize_bytes(512), "512 B");
        assert_eq!(humanize_bytes(1024), "1 KB");
        assert!(humanize_bytes(1024 * 1024 * 5).starts_with("5.0 MB"));
        assert!(humanize_bytes(1024u64.pow(3) * 8).starts_with("8.00 GB"));
    }

    #[test]
    fn format_uptime_collapses_zero_days_hours() {
        assert_eq!(format_uptime(45), "0m");
        assert_eq!(format_uptime(90), "1m");
        assert_eq!(format_uptime(3600 + 5 * 60), "1h 5m");
        assert_eq!(format_uptime(86_400 * 2 + 3600 * 3), "2d 3h 0m");
    }

    #[test]
    fn bar_renders_filled_and_empty() {
        assert_eq!(bar(0.0, 10), "░░░░░░░░░░");
        assert_eq!(bar(100.0, 10), "██████████");
        assert_eq!(bar(50.0, 10), "█████░░░░░");
        // Clamps out-of-range input.
        assert_eq!(bar(-5.0, 4), "░░░░");
        assert_eq!(bar(150.0, 4), "████");
    }

    #[test]
    fn widget_id_uses_instance_suffix() {
        let main = ResourcesWidget::with_config(
            "main".into(),
            ResourcesConfig::default(),
            Arc::new(Theme::builtin_defaults()),
        );
        assert_eq!(main.id(), "resources");
        let host = ResourcesWidget::with_config(
            "host".into(),
            ResourcesConfig::default(),
            Arc::new(Theme::builtin_defaults()),
        );
        assert_eq!(host.id(), "resources@host");
        assert_eq!(host.display_name(), "Resources (host)");
    }

    #[test]
    fn shortcut_preferences_default_to_r_e_s_m() {
        let w = make_widget();
        assert_eq!(w.shortcut_preferences(), &['r', 'e', 's', 'm']);
    }

    #[test]
    fn shortcut_preferences_use_user_override() {
        let cfg = ResourcesConfig {
            shortcuts: vec!['x', 'y', 'z'],
            ..ResourcesConfig::default()
        };
        let w = ResourcesWidget::with_config(
            "main".into(),
            cfg,
            Arc::new(Theme::builtin_defaults()),
        );
        assert_eq!(w.shortcut_preferences(), &['x', 'y', 'z']);
    }
}
