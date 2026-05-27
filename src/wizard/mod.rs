//! Plain-stdin/stdout setup wizard. Invoked with `glint --setup`.
//!
//! The wizard walks the user through each major TOML config file
//! (`config.toml` layout, `clock.toml`, `weather.toml`, `news.toml`,
//! `stocks.toml`, `calendar.toml`, and the Anthropic API key) one section
//! at a time. Each section presents an Edit/Skip prompt — skipping leaves
//! that file completely untouched, editing rewrites it entirely from a
//! template populated with the user's answers.
//!
//! v1 trade-offs (documented in the welcome banner):
//!   - Editing a section rewrites the file. Hand-edited comments are lost.
//!   - We don't validate IANA timezones, ticker symbols, or URLs. Trust the
//!     user's input; the dashboard surfaces errors at startup.
//!   - No tokio / async required. The wizard is fully synchronous; it
//!     opens stdin once and writes plain text to stdout.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::auth::credentials_dir;
use crate::config::layout::{GridCell, LayoutConfig};
use crate::config::{self, config_dir, config_path};
use crate::widgets::calendar::{CalendarConfig, ProviderEntry, ProviderKind};
use crate::widgets::clock::{ClockConfig, SecondaryTimezone};
#[cfg(feature = "widget-email")]
use crate::widgets::email::EmailConfig;
#[cfg(feature = "widget-gallery")]
use crate::widgets::gallery::GalleryConfig;
use crate::widgets::news::NewsConfig;
use crate::widgets::parse_widget_ref;
#[cfg(feature = "widget-resources")]
use crate::widgets::resources::ResourcesConfig;
use crate::widgets::stocks::StocksConfig;
use crate::widgets::weather::provider::Units;
use crate::widgets::weather::WeatherConfig;

// ── Prompt helpers ──────────────────────────────────────────────────────────

/// Read a line of input from stdin, trimming the trailing newline. Empty input
/// is fine (callers decide what "empty" means in context).
fn read_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let stdin = io::stdin();
    let mut buf = String::new();
    stdin
        .lock()
        .read_line(&mut buf)
        .context("failed to read from stdin")?;
    // Strip a single trailing newline (and optional \r on Windows-style input).
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
    }
    Ok(buf.trim().to_string())
}

/// `[Y/n]` (or `[y/N]`) confirmation prompt. Returns the user's choice,
/// falling back to `default_yes` on empty input.
fn confirm(prompt: &str, default_yes: bool) -> Result<bool> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    loop {
        let answer = read_line(&format!("{prompt} {suffix}: "))?;
        if answer.is_empty() {
            return Ok(default_yes);
        }
        match answer.to_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Please answer y or n."),
        }
    }
}

/// Letter-keyed multiple choice. `choices` is a list of `(letter, label)`
/// pairs. Returns the chosen letter (always one from `choices`).
fn select_letter(prompt: &str, choices: &[(char, &str)]) -> Result<char> {
    loop {
        println!("{prompt}");
        for (ch, label) in choices {
            println!("  [{ch}] {label}");
        }
        let answer = read_line("Choose: ")?.to_lowercase();
        if answer.is_empty() {
            // No letter given — re-prompt.
            continue;
        }
        let first = answer.chars().next().unwrap();
        if choices.iter().any(|(c, _)| *c == first) {
            return Ok(first);
        }
        println!("Unknown choice {answer:?}. Try again.");
    }
}

/// Read a line and split it on commas, trimming each item and dropping empties.
fn read_comma_list(prompt: &str) -> Result<Vec<String>> {
    let raw = read_line(prompt)?;
    Ok(raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// Pretty-print "(unset)" when an Option is None.
fn or_unset(s: Option<&str>) -> String {
    s.unwrap_or("(unset)").to_string()
}

// ── Entry point ─────────────────────────────────────────────────────────────

/// Touched-file accounting so the final summary can list what changed.
struct WizardReport {
    touched: Vec<PathBuf>,
}

impl WizardReport {
    fn new() -> Self {
        Self { touched: Vec::new() }
    }
    fn note(&mut self, path: &Path) {
        self.touched.push(path.to_path_buf());
    }
}

/// Run the wizard end-to-end. Synchronous — call from `main()` outside the
/// tokio runtime if you'd like, or inside; either works.
pub fn run() -> Result<()> {
    let mut report = WizardReport::new();

    // Make sure ~/.config/glint/ exists so we can read existing files and
    // write new ones into it without scattering error handling everywhere.
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config directory at {}", dir.display()))?;

    print_welcome();
    if !confirm("Continue?", true)? {
        println!("Aborted. No files were changed.");
        return Ok(());
    }

    // Step 1+2: layout & widget assignment. These two are coupled — the
    // layout templates ship with `__placeholder__` widget slots that step 2
    // fills in. If the user skips step 1 and the existing layout has named
    // widgets we leave both alone.
    let layout_result = step_layout(&mut report)?;

    // Discover instances per kind from whatever layout is on disk now
    // (post-step 1 if the user wrote, or pre-step 1 if they skipped). Each
    // per-widget step is invoked once per instance so the user can fill
    // in `<kind>@<instance>.toml` for every pane they configured.
    let instances = discover_instances_from_disk();

    // Step 3: per-widget configs.
    for instance in instances_for(&instances, "clock") {
        step_clock(&mut report, &instance)?;
    }
    for instance in instances_for(&instances, "weather") {
        step_weather(&mut report, &instance)?;
    }
    for instance in instances_for(&instances, "news") {
        step_news(&mut report, &instance)?;
    }
    for instance in instances_for(&instances, "stocks") {
        step_stocks(&mut report, &instance)?;
    }
    for instance in instances_for(&instances, "calendar") {
        step_calendar(&mut report, &instance)?;
    }
    #[cfg(feature = "widget-resources")]
    for instance in instances_for(&instances, "resources") {
        step_resources(&mut report, &instance)?;
    }
    #[cfg(feature = "widget-gallery")]
    for instance in instances_for(&instances, "gallery") {
        step_gallery(&mut report, &instance)?;
    }
    #[cfg(feature = "widget-email")]
    for instance in instances_for(&instances, "email") {
        step_email(&mut report, &instance)?;
    }

    // Step 4: LLM key.
    step_llm_key(&mut report)?;

    // Final summary.
    println!();
    println!("Configuration saved.");
    if report.touched.is_empty() {
        println!("No files were modified — every section was skipped.");
    } else {
        println!("Files written:");
        for path in &report.touched {
            println!("  - {}", path.display());
        }
    }
    if matches!(layout_result, LayoutOutcome::Skipped) {
        // No-op: nothing extra to say.
    }
    println!();
    println!("For deeper customization, you can edit any widget's TOML by hand:");
    if let Ok(dir) = crate::config::config_dir() {
        println!("  {}", dir.join("config.toml").display());
        println!("  {}", dir.join("clock.toml").display());
        println!("  {}", dir.join("weather.toml").display());
        println!("  {}", dir.join("news.toml").display());
        println!("  {}", dir.join("stocks.toml").display());
        println!("  {}", dir.join("calendar.toml").display());
        println!("  {}", dir.join("colorschemes.toml").display());
        println!("  {}", dir.join("llm.toml").display());
    } else {
        println!("  ~/.config/glint/<widget>.toml");
    }
    println!("Hand-edits to those files survive future `glint --setup` runs as long");
    println!("as you Skip the matching section.");
    println!();
    println!("You can re-run `glint --setup` any time to tweak a section.");
    println!("Run `glint` to launch the dashboard.");

    Ok(())
}

fn print_welcome() {
    println!();
    println!("============================================================");
    println!("                  glint setup wizard");
    println!("============================================================");
    println!();
    println!("This wizard walks you through configuring glint's TOML files.");
    println!("For each section you can:");
    println!("  - Edit (rewrites that TOML file from a template)");
    println!("  - Skip (leaves the existing file untouched)");
    println!();
    println!("NOTE: editing a section rewrites the entire TOML file for that");
    println!("section. Any hand-edited comments specific to that file will be");
    println!("lost. Skipping preserves the file exactly as-is.");
    println!();
    println!("All files live under {}", config_dir().map(|p| p.display().to_string()).unwrap_or_default());
    println!();
}

// ── Step 1+2: Layout & widget assignment ────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutOutcome {
    Skipped,
    Wrote,
}

fn step_layout(report: &mut WizardReport) -> Result<LayoutOutcome> {
    let path = config_path()?;
    let existing = load_existing_layout(&path);

    println!();
    println!("── Step 1: Layout ──────────────────────────────────────────");
    match &existing {
        Some(layout) => {
            println!("Current layout in {}:", path.display());
            print_layout_diagram(layout);
        }
        None => {
            println!("No existing config.toml found at {}", path.display());
        }
    }

    if !confirm("Edit this section?", false)? {
        return Ok(LayoutOutcome::Skipped);
    }

    // Ask pane count.
    let pane_count = loop {
        let answer = read_line("How many widget panes? (1-8) [3]: ")?;
        let n = if answer.is_empty() {
            3u8
        } else {
            match answer.parse::<u8>() {
                Ok(n) if (1..=8).contains(&n) => n,
                _ => {
                    println!("Please enter a number between 1 and 8.");
                    continue;
                }
            }
        };
        break n;
    };

    let choices = recommended_layouts(pane_count);
    println!();
    println!("Recommended {pane_count}-pane layouts:");
    for (i, (name, layout)) in choices.iter().enumerate() {
        let letter = (b'a' + i as u8) as char;
        println!();
        println!("  [{letter}] {name}");
        print_layout_diagram_indented(layout, "      ");
    }
    println!();

    // Letter-keyed pick (or skip).
    let mut menu: Vec<(char, &str)> =
        choices.iter().enumerate().map(|(i, (name, _))| ((b'a' + i as u8) as char, *name)).collect();
    menu.push(('s', "Skip — keep current layout"));
    let pick = select_letter("Pick a layout:", &menu)?;
    if pick == 's' {
        return Ok(LayoutOutcome::Skipped);
    }
    let idx = (pick as u8 - b'a') as usize;
    let (template_name, mut new_layout) = choices.into_iter().nth(idx).expect("valid choice");
    println!("Selected: {template_name}");

    // Step 2: widget assignment.
    let assignment = assign_widgets(&new_layout)?;
    for (i, cell) in new_layout.cells.iter_mut().enumerate() {
        if let Some(widget) = assignment.get(i) {
            cell.widget = widget.clone();
        }
    }

    // Backup existing config.toml before rewriting.
    if path.exists() {
        let backup = path.with_extension("toml.bak");
        std::fs::copy(&path, &backup).with_context(|| {
            format!("failed to back up config.toml to {}", backup.display())
        })?;
        println!("Backed up existing config.toml to {}", backup.display());
    }

    // Preserve the existing [global] block if we can find one (best-effort
    // string copy). Otherwise emit a sensible default.
    let global_block = extract_global_block(&path).unwrap_or_else(default_global_block);
    let toml = render_config_toml(&new_layout, &global_block);
    std::fs::write(&path, toml).with_context(|| {
        format!("failed to write {}", path.display())
    })?;
    report.note(&path);
    println!("Wrote {}", path.display());
    Ok(LayoutOutcome::Wrote)
}

/// Read config.toml from disk (if any) and return the list of distinct
/// instance names per widget kind, preserving cell order. Used by the
/// wizard so per-widget edit steps run once per instance.
fn discover_instances_from_disk() -> Vec<(String, String)> {
    let path = match config_path() {
        Ok(p) => p,
        Err(_) => return default_main_instances(),
    };
    let Some(layout) = load_existing_layout(&path) else {
        return default_main_instances();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for cell in &layout.cells {
        if cell.widget == "__placeholder__" {
            continue;
        }
        let (kind, instance) = parse_widget_ref(&cell.widget);
        if !out
            .iter()
            .any(|(k, i)| k == &kind && i == &instance)
        {
            out.push((kind, instance));
        }
    }
    if out.is_empty() {
        default_main_instances()
    } else {
        out
    }
}

/// Fallback when no config.toml exists yet — assume the original five-widget
/// layout, single `main` instance each.
fn default_main_instances() -> Vec<(String, String)> {
    ["clock", "calendar", "weather", "news", "stocks", "resources", "gallery", "email"]
        .into_iter()
        .map(|k| (k.to_string(), "main".to_string()))
        .collect()
}

/// Filter `instances` to the entries matching `kind`, returning just the
/// instance names in order. If no instance is recorded for the kind, fall
/// back to a single `"main"` entry so the wizard still offers an edit step.
fn instances_for(instances: &[(String, String)], kind: &str) -> Vec<String> {
    let matched: Vec<String> = instances
        .iter()
        .filter(|(k, _)| k == kind)
        .map(|(_, i)| i.clone())
        .collect();
    if matched.is_empty() {
        vec!["main".to_string()]
    } else {
        matched
    }
}

fn load_existing_layout(path: &Path) -> Option<LayoutConfig> {
    if !path.exists() {
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    let cfg: config::Config = toml::from_str(&contents).ok()?;
    Some(cfg.layout)
}

/// Best-effort copy of the `[global]` block from an existing config.toml so
/// we don't clobber theme / log settings when we rewrite the layout. Returns
/// the block including the `[global]` header line. Returns `None` if no
/// `[global]` section is found.
fn extract_global_block(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut found = false;
    let mut out = String::new();
    for line in contents.lines() {
        let trimmed = line.trim_start();
        if !found {
            if trimmed == "[global]" {
                found = true;
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        // Stop when we hit the next section header.
        if trimmed.starts_with('[') && trimmed != "[global]" {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if found {
        Some(out)
    } else {
        None
    }
}

fn default_global_block() -> String {
    r#"[global]
theme = "default"
command_key = ":"
refresh_all_on_focus = true
log_level = "info"
"#
    .to_string()
}

fn print_layout_diagram(layout: &LayoutConfig) {
    print_layout_diagram_indented(layout, "  ");
}

/// ASCII diagram of a grid layout. Renders each cell as a single token row,
/// e.g. `[clock][calendar]` with cells in row order.
fn print_layout_diagram_indented(layout: &LayoutConfig, indent: &str) {
    if layout.columns.is_empty() || layout.rows.is_empty() {
        println!("{indent}(empty layout)");
        return;
    }
    let n_cols = layout.columns.len();
    let n_rows = layout.rows.len();
    // Build a row -> cols grid by occupant index.
    let mut grid: Vec<Vec<Option<String>>> = vec![vec![None; n_cols]; n_rows];
    for cell in &layout.cells {
        let col_end = (cell.col + cell.col_span.max(1) - 1).min(n_cols.saturating_sub(1));
        let row_end = (cell.row + cell.row_span.max(1) - 1).min(n_rows.saturating_sub(1));
        let label = if cell.widget == "__placeholder__" {
            "·".to_string()
        } else {
            cell.widget.clone()
        };
        for row in grid.iter_mut().take(row_end + 1).skip(cell.row) {
            for slot in row.iter_mut().take(col_end + 1).skip(cell.col) {
                *slot = Some(label.clone());
            }
        }
    }
    let max_label = grid
        .iter()
        .flatten()
        .filter_map(|c| c.as_ref().map(|s| s.len()))
        .max()
        .unwrap_or(8)
        .max(8);
    for row in &grid {
        let mut line = String::new();
        for cell in row {
            let label = cell.as_deref().unwrap_or(" ");
            line.push('[');
            line.push_str(&format!("{label:^max_label$}", max_label = max_label));
            line.push(']');
        }
        println!("{indent}{line}");
    }
}

/// Walks the user through assigning a widget to each cell of the layout.
/// Returns a vector of widget refs (each `kind` or `kind@instance`) indexed
/// by `layout.cells` position. Lifts the v1 single-instance constraint —
/// the user can reuse a kind across panes as long as each instance name
/// is distinct.
fn assign_widgets(layout: &LayoutConfig) -> Result<Vec<String>> {
    let n = layout.cells.len();
    println!();
    println!("Now assign a widget to each of the {n} pane(s):");
    print_layout_with_numbers(layout);
    println!();
    println!("Each pane needs a kind (clock / calendar / weather / news / stocks).");
    println!("To run multiple panes of the same kind, give each one a different instance");
    println!("name when prompted (e.g. \"home\" and \"office\" for two clocks).");
    println!();

    let kinds: &[&str] = &[
        "clock",
        "calendar",
        "weather",
        "news",
        "stocks",
        "resources",
        "gallery",
        "email",
    ];
    let mut out: Vec<String> = Vec::with_capacity(n);
    // (kind, instance) pairs already assigned in this session — used to
    // reject duplicates.
    let mut taken: Vec<(String, String)> = Vec::with_capacity(n);

    for i in 0..n {
        let prompt = format!(
            "Pane {pane} — which widget? Available: {a}",
            pane = i + 1,
            a = kinds.join(", ")
        );
        println!("{prompt}");
        let kind = loop {
            let answer = read_line("> ")?.to_lowercase();
            if answer.is_empty() {
                println!("Please enter one of: {}", kinds.join(", "));
                continue;
            }
            if kinds.contains(&answer.as_str()) {
                break answer;
            }
            println!(
                "{answer:?} is not available. Choose one of: {}",
                kinds.join(", ")
            );
        };

        // Instance prompt — re-prompt if the user picks a name already
        // taken by an earlier pane this session.
        let instance = loop {
            let raw = read_line(&format!(
                "Instance name? (leave empty for \"main\") for {kind}: "
            ))?;
            let candidate = if raw.is_empty() {
                "main".to_string()
            } else {
                raw
            };
            if taken
                .iter()
                .any(|(k, inst)| k == &kind && inst == &candidate)
            {
                println!(
                    "Instance {candidate:?} for {kind} is already assigned to an earlier pane. Pick a different name."
                );
                continue;
            }
            break candidate;
        };

        taken.push((kind.clone(), instance.clone()));
        let widget_ref = if instance == "main" {
            kind
        } else {
            format!("{kind}@{instance}")
        };
        out.push(widget_ref);
    }
    Ok(out)
}

/// Print the layout with sequential placeholder numbers (1, 2, …) so the
/// user can match each cell to the assignment prompts that follow.
fn print_layout_with_numbers(layout: &LayoutConfig) {
    let mut numbered = layout.clone();
    for (i, cell) in numbered.cells.iter_mut().enumerate() {
        cell.widget = format!("pane {}", i + 1);
    }
    print_layout_diagram_indented(&numbered, "  ");
}

/// Recommended layouts for the given pane count. The returned LayoutConfig
/// uses `widget = "__placeholder__"` for every cell; the widget-assignment
/// step fills in real names.
fn recommended_layouts(n: u8) -> Vec<(&'static str, LayoutConfig)> {
    let p = || "__placeholder__".to_string();
    let cell = |w: String, col, row, col_span, row_span| GridCell {
        widget: w,
        col,
        row,
        col_span,
        row_span,
    };
    match n {
        1 => vec![(
            "Full-screen",
            LayoutConfig {
                columns: vec![100],
                rows: vec![100],
                cells: vec![cell(p(), 0, 0, 1, 1)],
            },
        )],
        2 => vec![
            (
                "Side-by-side 50/50",
                LayoutConfig {
                    columns: vec![50, 50],
                    rows: vec![100],
                    cells: vec![cell(p(), 0, 0, 1, 1), cell(p(), 1, 0, 1, 1)],
                },
            ),
            (
                "Top/bottom 50/50",
                LayoutConfig {
                    columns: vec![100],
                    rows: vec![50, 50],
                    cells: vec![cell(p(), 0, 0, 1, 1), cell(p(), 0, 1, 1, 1)],
                },
            ),
        ],
        3 => vec![
            (
                "Top split + bottom full",
                LayoutConfig {
                    columns: vec![50, 50],
                    rows: vec![50, 50],
                    cells: vec![
                        cell(p(), 0, 0, 1, 1),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 0, 1, 2, 1),
                    ],
                },
            ),
            (
                "Left full + right stacked",
                LayoutConfig {
                    columns: vec![50, 50],
                    rows: vec![50, 50],
                    cells: vec![
                        cell(p(), 0, 0, 1, 2),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                    ],
                },
            ),
            (
                "Three rows stacked",
                LayoutConfig {
                    columns: vec![100],
                    rows: vec![33, 33, 34],
                    cells: vec![
                        cell(p(), 0, 0, 1, 1),
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 0, 2, 1, 1),
                    ],
                },
            ),
        ],
        4 => vec![
            (
                "2x2 grid",
                LayoutConfig {
                    columns: vec![50, 50],
                    rows: vec![50, 50],
                    cells: vec![
                        cell(p(), 0, 0, 1, 1),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                    ],
                },
            ),
            (
                "Left full + right 3 rows",
                LayoutConfig {
                    columns: vec![50, 50],
                    rows: vec![33, 33, 34],
                    cells: vec![
                        cell(p(), 0, 0, 1, 3),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                        cell(p(), 1, 2, 1, 1),
                    ],
                },
            ),
            (
                "Top 2 + bottom 2",
                LayoutConfig {
                    columns: vec![50, 50],
                    rows: vec![50, 50],
                    cells: vec![
                        cell(p(), 0, 0, 1, 1),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                    ],
                },
            ),
        ],
        5 => vec![
            (
                "2 cols x 3 rows w/ bottom spanning (default)",
                LayoutConfig {
                    columns: vec![40, 60],
                    rows: vec![35, 35, 30],
                    cells: vec![
                        cell(p(), 0, 0, 1, 1),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                        cell(p(), 0, 2, 2, 1),
                    ],
                },
            ),
            (
                "Top single + 2x2 below",
                LayoutConfig {
                    columns: vec![50, 50],
                    rows: vec![34, 33, 33],
                    cells: vec![
                        cell(p(), 0, 0, 2, 1),
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                        cell(p(), 0, 2, 1, 1),
                        cell(p(), 1, 2, 1, 1),
                    ],
                },
            ),
            (
                "Five rows stacked",
                LayoutConfig {
                    columns: vec![100],
                    rows: vec![20, 20, 20, 20, 20],
                    cells: vec![
                        cell(p(), 0, 0, 1, 1),
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 0, 2, 1, 1),
                        cell(p(), 0, 3, 1, 1),
                        cell(p(), 0, 4, 1, 1),
                    ],
                },
            ),
        ],
        6 => vec![
            (
                "3x2 grid",
                LayoutConfig {
                    columns: vec![34, 33, 33],
                    rows: vec![50, 50],
                    cells: vec![
                        cell(p(), 0, 0, 1, 1),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 2, 0, 1, 1),
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                        cell(p(), 2, 1, 1, 1),
                    ],
                },
            ),
            (
                "2x3 grid",
                LayoutConfig {
                    columns: vec![50, 50],
                    rows: vec![34, 33, 33],
                    cells: vec![
                        cell(p(), 0, 0, 1, 1),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                        cell(p(), 0, 2, 1, 1),
                        cell(p(), 1, 2, 1, 1),
                    ],
                },
            ),
            (
                "Left full + right 5 stacked",
                LayoutConfig {
                    columns: vec![40, 60],
                    rows: vec![20, 20, 20, 20, 20],
                    cells: vec![
                        cell(p(), 0, 0, 1, 5),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                        cell(p(), 1, 2, 1, 1),
                        cell(p(), 1, 3, 1, 1),
                        cell(p(), 1, 4, 1, 1),
                    ],
                },
            ),
        ],
        7 => vec![
            (
                "Top single + 3x2 grid below",
                LayoutConfig {
                    columns: vec![34, 33, 33],
                    rows: vec![28, 36, 36],
                    cells: vec![
                        cell(p(), 0, 0, 3, 1), // top spans full width
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                        cell(p(), 2, 1, 1, 1),
                        cell(p(), 0, 2, 1, 1),
                        cell(p(), 1, 2, 1, 1),
                        cell(p(), 2, 2, 1, 1),
                    ],
                },
            ),
            (
                "Top 3 + middle 2 + bottom 2",
                LayoutConfig {
                    // 6 column units so 3 cells (col_span 2 each) and
                    // 2 cells (col_span 3 each) both tile cleanly.
                    columns: vec![17, 17, 17, 17, 16, 16],
                    rows: vec![33, 34, 33],
                    cells: vec![
                        cell(p(), 0, 0, 2, 1),
                        cell(p(), 2, 0, 2, 1),
                        cell(p(), 4, 0, 2, 1),
                        cell(p(), 0, 1, 3, 1),
                        cell(p(), 3, 1, 3, 1),
                        cell(p(), 0, 2, 3, 1),
                        cell(p(), 3, 2, 3, 1),
                    ],
                },
            ),
            (
                "Left full + right 3x2 grid",
                LayoutConfig {
                    columns: vec![34, 22, 22, 22],
                    rows: vec![50, 50],
                    cells: vec![
                        cell(p(), 0, 0, 1, 2),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 2, 0, 1, 1),
                        cell(p(), 3, 0, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                        cell(p(), 2, 1, 1, 1),
                        cell(p(), 3, 1, 1, 1),
                    ],
                },
            ),
        ],
        8 => vec![
            (
                "4x2 grid",
                LayoutConfig {
                    columns: vec![25, 25, 25, 25],
                    rows: vec![50, 50],
                    cells: vec![
                        cell(p(), 0, 0, 1, 1),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 2, 0, 1, 1),
                        cell(p(), 3, 0, 1, 1),
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                        cell(p(), 2, 1, 1, 1),
                        cell(p(), 3, 1, 1, 1),
                    ],
                },
            ),
            (
                "2x4 grid",
                LayoutConfig {
                    columns: vec![50, 50],
                    rows: vec![25, 25, 25, 25],
                    cells: vec![
                        cell(p(), 0, 0, 1, 1),
                        cell(p(), 1, 0, 1, 1),
                        cell(p(), 0, 1, 1, 1),
                        cell(p(), 1, 1, 1, 1),
                        cell(p(), 0, 2, 1, 1),
                        cell(p(), 1, 2, 1, 1),
                        cell(p(), 0, 3, 1, 1),
                        cell(p(), 1, 3, 1, 1),
                    ],
                },
            ),
            (
                "Top 3 + middle 3 + bottom 2",
                LayoutConfig {
                    // 6 column units for clean 3-of-2 and 2-of-3 tiling.
                    columns: vec![17, 17, 17, 17, 16, 16],
                    rows: vec![33, 34, 33],
                    cells: vec![
                        cell(p(), 0, 0, 2, 1),
                        cell(p(), 2, 0, 2, 1),
                        cell(p(), 4, 0, 2, 1),
                        cell(p(), 0, 1, 2, 1),
                        cell(p(), 2, 1, 2, 1),
                        cell(p(), 4, 1, 2, 1),
                        cell(p(), 0, 2, 3, 1),
                        cell(p(), 3, 2, 3, 1),
                    ],
                },
            ),
        ],
        _ => Vec::new(),
    }
}

/// Format a layout into the TOML body of config.toml. The `global_block`
/// string is inserted verbatim (must already include the `[global]` header).
fn render_config_toml(layout: &LayoutConfig, global_block: &str) -> String {
    let mut out = String::new();
    out.push_str("version = 1\n\n");
    out.push_str(global_block.trim_end());
    out.push_str("\n\n[layout]\n");
    out.push_str(&format!("columns = {}\n", toml_int_array(&layout.columns)));
    out.push_str(&format!("rows = {}\n", toml_int_array(&layout.rows)));
    for cell in &layout.cells {
        out.push_str("\n[[layout.cells]]\n");
        out.push_str(&format!("widget = {}\n", toml_string(&cell.widget)));
        out.push_str(&format!("col = {}\n", cell.col));
        out.push_str(&format!("row = {}\n", cell.row));
        if cell.col_span != 1 {
            out.push_str(&format!("col_span = {}\n", cell.col_span));
        }
        if cell.row_span != 1 {
            out.push_str(&format!("row_span = {}\n", cell.row_span));
        }
    }
    out
}

fn toml_int_array(xs: &[u16]) -> String {
    let parts: Vec<String> = xs.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(", "))
}

fn toml_string(s: &str) -> String {
    // Conservative quoting: escape backslashes and double quotes.
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn toml_string_array(xs: &[String]) -> String {
    let parts: Vec<String> = xs.iter().map(|s| toml_string(s)).collect();
    format!("[{}]", parts.join(", "))
}

/// Compose `<kind>` or `<kind>@<instance>` to match the on-disk TOML
/// filename convention used by load_widget_toml_for_instance.
fn widget_stem(kind: &str, instance: &str) -> String {
    if instance == "main" {
        kind.to_string()
    } else {
        format!("{kind}@{instance}")
    }
}

/// Append ` (instance)` to a widget label when the instance isn't `main`,
/// matching the widget's own display_name format.
fn instance_header(label: &str, instance: &str) -> String {
    if instance == "main" {
        label.to_string()
    } else {
        format!("{label} ({instance})")
    }
}

// ── Step 3a: Clock ──────────────────────────────────────────────────────────

fn step_clock(report: &mut WizardReport, instance: &str) -> Result<()> {
    println!();
    let header = instance_header("Clock", instance);
    println!("── Step 2: {header} ───────────────────────────────");
    let stem = widget_stem("clock", instance);
    let existing: ClockConfig =
        config::load_widget_toml(&stem).unwrap_or_default();
    let path = config_dir()?.join(format!("{stem}.toml"));

    println!("Current timezone: {}", or_unset(existing.timezone.as_deref()));
    if existing.secondary_timezones.is_empty() {
        println!("Current world clocks: (none)");
    } else {
        println!("Current world clocks:");
        for tz in &existing.secondary_timezones {
            println!("  - {} -> {}", tz.label, tz.timezone);
        }
    }

    if !confirm(&format!("Edit {header}?"), false)? {
        return Ok(());
    }

    let current_tz = existing.timezone.clone().unwrap_or_default();
    let tz_prompt = if current_tz.is_empty() {
        "Home timezone (IANA name, e.g. America/Vancouver) [empty = system local]: ".to_string()
    } else {
        format!("Home timezone (IANA name) [current: {current_tz}]: ")
    };
    let tz_input = read_line(&tz_prompt)?;
    let new_tz = if tz_input.is_empty() {
        if current_tz.is_empty() { None } else { Some(current_tz) }
    } else {
        Some(tz_input)
    };

    // World clocks: replace, add, or skip.
    let mode = if existing.secondary_timezones.is_empty() {
        'r'
    } else {
        select_letter(
            "Replace existing world clocks list, add to it, or skip?",
            &[
                ('r', "Replace — discard existing entries"),
                ('a', "Add — keep existing and append new ones"),
                ('s', "Skip — leave world clocks unchanged"),
            ],
        )?
    };
    let mut clocks: Vec<SecondaryTimezone> = match mode {
        'a' => existing.secondary_timezones.clone(),
        's' => existing.secondary_timezones.clone(),
        _ => Vec::new(),
    };
    if mode != 's' {
        const MAX: usize = 8;
        while clocks.len() < MAX {
            if !confirm(
                &format!("Add a world clock? ({} so far)", clocks.len()),
                false,
            )? {
                break;
            }
            let label = read_line("Label (e.g. New York): ")?;
            if label.is_empty() {
                println!("Label can't be empty — skipping this entry.");
                continue;
            }
            let tz = read_line("Timezone (e.g. America/New_York): ")?;
            if tz.is_empty() {
                println!("Timezone can't be empty — skipping this entry.");
                continue;
            }
            clocks.push(SecondaryTimezone { label, timezone: tz });
        }
        if clocks.len() >= MAX {
            println!("(reached max of {MAX} world clocks)");
        }
    }

    let toml = render_clock_toml(new_tz.as_deref(), &clocks);
    std::fs::write(&path, toml)
        .with_context(|| format!("failed to write {}", path.display()))?;
    report.note(&path);
    println!("Wrote {}", path.display());
    Ok(())
}

fn render_clock_toml(timezone: Option<&str>, clocks: &[SecondaryTimezone]) -> String {
    let mut out = String::new();
    out.push_str("# Primary clock timezone (IANA name). Comment out to use the system local time.\n");
    match timezone {
        Some(tz) if !tz.is_empty() => {
            out.push_str(&format!("timezone = {}\n", toml_string(tz)));
        }
        _ => {
            out.push_str("# timezone = \"America/Vancouver\"\n");
        }
    }
    out.push_str("show_seconds = false\n");
    out.push_str("show_seconds_ticker = true\n");
    out.push_str("show_date = true\n");
    out.push_str("hour_format = \"24h\"\n");
    if !clocks.is_empty() {
        out.push('\n');
        out.push_str("# Additional world clocks rendered when there's vertical room.\n");
        for c in clocks {
            out.push_str("[[secondary_timezones]]\n");
            out.push_str(&format!("label = {}\n", toml_string(&c.label)));
            out.push_str(&format!("timezone = {}\n", toml_string(&c.timezone)));
            out.push('\n');
        }
    }
    out
}

// ── Step 3b: Weather ────────────────────────────────────────────────────────

fn step_weather(report: &mut WizardReport, instance: &str) -> Result<()> {
    println!();
    let header = instance_header("Weather", instance);
    println!("── Step 3: {header} ─────────────────────────────────");
    let stem = widget_stem("weather", instance);
    let existing: WeatherConfig =
        config::load_widget_toml(&stem).unwrap_or_default();
    let path = config_dir()?.join(format!("{stem}.toml"));

    println!("Current label: {}", or_unset(existing.label.as_deref()));
    println!(
        "Current lat/lon: {} / {}",
        existing.latitude.map(|v| v.to_string()).unwrap_or_else(|| "(unset)".to_string()),
        existing.longitude.map(|v| v.to_string()).unwrap_or_else(|| "(unset)".to_string()),
    );
    let cur_units = match existing.units {
        Units::Metric => "metric",
        Units::Imperial => "imperial",
    };
    println!("Current units: {cur_units}");

    if !confirm(&format!("Edit {header}?"), false)? {
        return Ok(());
    }

    let label_prompt = match &existing.label {
        Some(l) => format!("City/location label [current: {l}]: "),
        None => "City/location label (e.g. Richmond, BC): ".to_string(),
    };
    let label_in = read_line(&label_prompt)?;
    let label = if label_in.is_empty() {
        existing.label.clone()
    } else {
        Some(label_in)
    };

    let lat = prompt_optional_float("Latitude", existing.latitude)?;
    let lon = prompt_optional_float("Longitude", existing.longitude)?;

    let units_in = read_line(&format!(
        "Units: metric or imperial? [current: {cur_units}]: "
    ))?;
    let units = match units_in.trim().to_lowercase().as_str() {
        "" => existing.units,
        "metric" | "m" => Units::Metric,
        "imperial" | "i" => Units::Imperial,
        other => {
            println!("Unknown units {other:?}, keeping {cur_units}.");
            existing.units
        }
    };

    let toml = render_weather_toml(label.as_deref(), lat, lon, units);
    std::fs::write(&path, toml)
        .with_context(|| format!("failed to write {}", path.display()))?;
    report.note(&path);
    println!("Wrote {}", path.display());
    Ok(())
}

fn prompt_optional_float(name: &str, current: Option<f64>) -> Result<Option<f64>> {
    let prompt = match current {
        Some(v) => format!("{name} (float) [current: {v}]: "),
        None => format!("{name} (float, empty to leave unset): "),
    };
    let raw = read_line(&prompt)?;
    if raw.is_empty() {
        return Ok(current);
    }
    match raw.parse::<f64>() {
        Ok(v) => Ok(Some(v)),
        Err(_) => {
            println!("{raw:?} is not a number — keeping previous value.");
            Ok(current)
        }
    }
}

fn render_weather_toml(
    label: Option<&str>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    units: Units,
) -> String {
    let mut out = String::new();
    out.push_str("# Open-Meteo is free and key-less. Set lat/lon to your city,\n");
    out.push_str("# or leave them commented out and keep auto_locate = true for IP geolocation.\n");
    if let Some(l) = label {
        out.push_str(&format!("label = {}\n", toml_string(l)));
    } else {
        out.push_str("# label = \"Richmond, BC\"\n");
    }
    if let Some(v) = latitude {
        out.push_str(&format!("latitude = {v}\n"));
    } else {
        out.push_str("# latitude = 49.166\n");
    }
    if let Some(v) = longitude {
        out.push_str(&format!("longitude = {v}\n"));
    } else {
        out.push_str("# longitude = -123.133\n");
    }
    let units_str = match units {
        Units::Metric => "metric",
        Units::Imperial => "imperial",
    };
    out.push_str(&format!("units = \"{units_str}\"\n"));
    out.push_str("poll_interval_secs = 600\n");
    out.push_str("auto_locate = true\n");
    out
}

// ── Step 3c: News ───────────────────────────────────────────────────────────

const DEFAULT_FEEDS: &[(&str, &str)] = &[
    ("Hacker News", "https://hnrss.org/frontpage"),
    ("Ars Technica", "https://feeds.arstechnica.com/arstechnica/index"),
    ("The Verge", "https://www.theverge.com/rss/index.xml"),
    ("Engadget", "https://www.engadget.com/rss.xml"),
    ("Phoronix", "https://www.phoronix.com/rss.php"),
    ("BBC News", "http://feeds.bbci.co.uk/news/rss.xml"),
    ("BBC World", "http://feeds.bbci.co.uk/news/world/rss.xml"),
    ("Guardian World", "https://www.theguardian.com/world/rss"),
    ("NPR World", "https://feeds.npr.org/1004/rss.xml"),
    ("BBC Business", "http://feeds.bbci.co.uk/news/business/rss.xml"),
    ("Yahoo Finance", "https://finance.yahoo.com/news/rssindex"),
    ("MarketWatch", "http://feeds.marketwatch.com/marketwatch/topstories/"),
    (
        "CNBC Top",
        "https://www.cnbc.com/id/100003114/device/rss/rss.html",
    ),
    ("CBC News", "https://www.cbc.ca/webfeed/rss/rss-topstories"),
    ("CBC Politics", "https://www.cbc.ca/webfeed/rss/rss-politics"),
    ("CBC Business", "https://www.cbc.ca/webfeed/rss/rss-business"),
    (
        "CTV News",
        "https://www.ctvnews.ca/rss/ctvnews-ca-top-stories-public-rss-1.822009",
    ),
    ("Pitchfork", "https://pitchfork.com/rss/news/"),
    ("Hollywood Reporter", "https://www.hollywoodreporter.com/feed/"),
];

const DEFAULT_TOPICS: &[(&str, &[&str])] = &[
    (
        "Tech",
        &[
            "AI", "OpenAI", "Anthropic", "LLM", "GPU", "developer", "Linux", "Rust",
            "Apple", "Google", "Microsoft", "Meta", "chip", "software", "startup",
            "open source", "GitHub",
        ],
    ),
    (
        "Business",
        &[
            "CEO", "merger", "acquisition", "IPO", "revenue", "earnings", "quarterly",
            "Wall Street", "market", "Fed", "inflation", "interest rate", "Bitcoin",
            "crypto", "yield", "treasury", "stocks", "bonds", "dividend", "trader",
        ],
    ),
    (
        "World",
        &[
            "Ukraine", "Russia", "China", "EU", "UN", "climate", "war", "election",
            "summit", "treaty", "Israel", "Gaza", "Iran", "NATO", "global", "Brussels",
            "international",
        ],
    ),
    (
        "Canada",
        &[
            "Canada", "Canadian", "Ottawa", "Toronto", "Vancouver", "Montreal",
            "Quebec", "Alberta", "B.C.", "Trudeau", "Carney", "CBC", "Bank of Canada",
            "Loonie",
        ],
    ),
    (
        "Entertainment",
        &[
            "movie", "film", "actor", "actress", "Hollywood", "Netflix", "HBO", "Disney",
            "Oscar", "Grammy", "Emmy", "show", "series", "trailer",
            "album", "song", "single", "artist", "band", "concert", "tour", "music",
            "EP", "soundtrack",
        ],
    ),
];

fn step_news(report: &mut WizardReport, instance: &str) -> Result<()> {
    println!();
    let header = instance_header("News", instance);
    println!("── Step 4: {header} ────────────────────────────────────────");
    let stem = widget_stem("news", instance);
    let existing: NewsConfig = config::load_widget_toml(&stem).unwrap_or_default();
    let path = config_dir()?.join(format!("{stem}.toml"));

    if existing.feeds.is_empty() {
        println!("Current feeds: (none)");
    } else {
        let labels: Vec<&str> = existing.feeds.iter().map(|f| f.label.as_str()).collect();
        println!("Current feeds: {}", labels.join(", "));
    }
    if existing.topics.is_empty() {
        println!("Current topics: (none)");
    } else {
        let labels: Vec<&str> = existing.topics.iter().map(|t| t.label.as_str()).collect();
        println!("Current topics: {}", labels.join(", "));
    }

    if !confirm(&format!("Edit {header}?"), false)? {
        return Ok(());
    }

    // Topics.
    println!();
    println!("Available default topics: {}", DEFAULT_TOPICS.iter().map(|(l, _)| *l).collect::<Vec<_>>().join(", "));
    let topic_in = read_line(
        "Pick topics — comma-separated labels, [keep] to leave as-is, or [all] for all defaults: ",
    )?;
    let topics: Vec<(String, Vec<String>)> = match topic_in.trim().to_lowercase().as_str() {
        "keep" | "" => existing
            .topics
            .iter()
            .map(|t| (t.label.clone(), t.keywords.clone()))
            .collect(),
        "all" => DEFAULT_TOPICS
            .iter()
            .map(|(l, kw)| (l.to_string(), kw.iter().map(|s| s.to_string()).collect()))
            .collect(),
        _ => {
            let wanted: Vec<String> = topic_in
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            DEFAULT_TOPICS
                .iter()
                .filter(|(l, _)| wanted.iter().any(|w| w == &l.to_lowercase()))
                .map(|(l, kw)| (l.to_string(), kw.iter().map(|s| s.to_string()).collect()))
                .collect()
        }
    };

    // Feeds.
    println!();
    println!("Available default feeds:");
    for (label, _) in DEFAULT_FEEDS {
        println!("  - {label}");
    }
    let feed_in = read_line(
        "Pick feeds — comma-separated labels, [keep] to leave as-is, or [all] for all defaults: ",
    )?;
    let feeds: Vec<(String, String)> = match feed_in.trim().to_lowercase().as_str() {
        "keep" | "" => existing
            .feeds
            .iter()
            .map(|f| (f.label.clone(), f.url.clone()))
            .collect(),
        "all" => DEFAULT_FEEDS
            .iter()
            .map(|(l, u)| (l.to_string(), u.to_string()))
            .collect(),
        _ => {
            let wanted: Vec<String> = feed_in
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            DEFAULT_FEEDS
                .iter()
                .filter(|(l, _)| wanted.iter().any(|w| w == &l.to_lowercase()))
                .map(|(l, u)| (l.to_string(), u.to_string()))
                .collect()
        }
    };

    let toml = render_news_toml(&feeds, &topics, existing.show_topic_labels);
    std::fs::write(&path, toml)
        .with_context(|| format!("failed to write {}", path.display()))?;
    report.note(&path);
    println!("Wrote {}", path.display());
    Ok(())
}

fn render_news_toml(
    feeds: &[(String, String)],
    topics: &[(String, Vec<String>)],
    show_topic_labels: bool,
) -> String {
    let mut out = String::new();
    out.push_str("poll_interval_secs = 900\n");
    out.push_str("horizontal_scroll_filters = false\n");
    out.push_str(&format!(
        "show_topic_labels = {}\n",
        if show_topic_labels { "true" } else { "false" }
    ));
    if !feeds.is_empty() {
        out.push('\n');
        out.push_str("# RSS / Atom feeds aggregated across the news widget.\n");
        for (label, url) in feeds {
            out.push_str("[[feeds]]\n");
            out.push_str(&format!("label = {}\n", toml_string(label)));
            out.push_str(&format!("url = {}\n", toml_string(url)));
            out.push('\n');
        }
    }
    if !topics.is_empty() {
        out.push_str("# Topics double as filter tabs across the top of the news cell.\n");
        for (label, keywords) in topics {
            out.push_str("[[topics]]\n");
            out.push_str(&format!("label = {}\n", toml_string(label)));
            out.push_str(&format!("keywords = {}\n", toml_string_array(keywords)));
            out.push('\n');
        }
    }
    out
}

// ── Step 3d: Stocks ─────────────────────────────────────────────────────────

fn step_stocks(report: &mut WizardReport, instance: &str) -> Result<()> {
    println!();
    let header = instance_header("Stocks", instance);
    println!("── Step 5: {header} ────────────────────────────────────────");
    let stem = widget_stem("stocks", instance);
    let existing: StocksConfig = config::load_widget_toml(&stem).unwrap_or_default();
    let path = config_dir()?.join(format!("{stem}.toml"));

    println!(
        "Current indices: {}",
        if existing.indices.is_empty() {
            "(none)".to_string()
        } else {
            existing.indices.join(", ")
        }
    );
    println!(
        "Current watchlist: {}",
        if existing.watchlist.is_empty() {
            "(none)".to_string()
        } else {
            existing.watchlist.join(", ")
        }
    );

    if !confirm(&format!("Edit {header}?"), false)? {
        return Ok(());
    }

    println!("Available indices: ^DJI (Dow), ^GSPC (S&P 500), ^IXIC (Nasdaq)");
    let idx_in = read_line(
        "Indices — comma-separated, [keep] to leave as-is, [all] for all three: ",
    )?;
    let indices: Vec<String> = match idx_in.trim().to_lowercase().as_str() {
        "keep" | "" => existing.indices.clone(),
        "all" => vec!["^DJI".into(), "^GSPC".into(), "^IXIC".into()],
        _ => idx_in
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    };

    let watch_in = read_line(
        "Watchlist tickers — comma-separated (e.g. AAPL, MSFT), [keep] to leave as-is: ",
    )?;
    let watchlist: Vec<String> = match watch_in.trim().to_lowercase().as_str() {
        "keep" | "" => existing.watchlist.clone(),
        _ => watch_in
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect(),
    };

    let toml = render_stocks_toml(&indices, &watchlist);
    std::fs::write(&path, toml)
        .with_context(|| format!("failed to write {}", path.display()))?;
    report.note(&path);
    println!("Wrote {}", path.display());
    Ok(())
}

fn render_stocks_toml(indices: &[String], watchlist: &[String]) -> String {
    let mut out = String::new();
    out.push_str("# Yahoo Finance symbols.\n");
    out.push_str(&format!("indices = {}\n", toml_string_array(indices)));
    out.push_str(&format!("watchlist = {}\n", toml_string_array(watchlist)));
    out.push_str("\npoll_interval_secs = 60\n");
    out.push_str("default_display_mode = \"percent\"\n");
    out.push_str("default_period = \"1d\"\n");
    out.push_str("horizontal_scroll_period = false\n");
    out
}

// ── Step 3e: Calendar ───────────────────────────────────────────────────────

fn step_calendar(report: &mut WizardReport, instance: &str) -> Result<()> {
    println!();
    let header = instance_header("Calendar", instance);
    println!("── Step 6: {header} ────────────────────────────────────────");
    let stem = widget_stem("calendar", instance);
    let existing: CalendarConfig =
        config::load_widget_toml(&stem).unwrap_or_default();
    let path = config_dir()?.join(format!("{stem}.toml"));

    if existing.providers.is_empty() {
        println!("Current providers: (none — local-only mode)");
    } else {
        println!("Current providers ({}):", existing.providers.len());
        for (i, p) in existing.providers.iter().enumerate() {
            let ids = if p.calendar_ids.is_empty() {
                "(default)".to_string()
            } else {
                p.calendar_ids.join(", ")
            };
            println!(
                "  {}. {} -> {}",
                i + 1,
                provider_kind_str(p.kind),
                ids
            );
        }
    }

    if !confirm(&format!("Edit {header}?"), false)? {
        return Ok(());
    }

    let mut providers: Vec<ProviderEntry> = existing.providers.clone();

    // Remove flow first (optional).
    if !providers.is_empty() {
        println!();
        println!("Existing providers:");
        for (i, p) in providers.iter().enumerate() {
            println!("  {}. {}", i + 1, provider_kind_str(p.kind));
        }
        let raw = read_line("Remove a provider? Enter index, or 0 to keep all: ")?;
        if let Ok(idx) = raw.parse::<usize>() {
            if idx >= 1 && idx <= providers.len() {
                let removed = providers.remove(idx - 1);
                println!("Removed provider: {}", provider_kind_str(removed.kind));
            } else if idx != 0 {
                println!("Index out of range — no provider removed.");
            }
        }
    }

    // Add flow loop.
    while confirm("Add a calendar provider?", false)? {
        let kind_raw = read_line("Type: google, outlook, caldav, or local? ")?;
        let kind = match kind_raw.trim().to_lowercase().as_str() {
            "google" => ProviderKind::Google,
            "outlook" | "microsoft" => ProviderKind::Outlook,
            "caldav" | "apple" | "icloud" => ProviderKind::Caldav,
            "local" => ProviderKind::Local,
            other => {
                println!("Unknown provider {other:?} — skipping this entry.");
                continue;
            }
        };
        match kind {
            ProviderKind::Google => {
                println!();
                println!("Google Calendar: after this wizard, run `glint --auth google` to grant");
                println!("access. We'll wire up the provider entry now.");
                let ids = read_comma_list(
                    "Calendar IDs (comma-separated, e.g. primary, work@example.com — empty = primary): ",
                )?;
                providers.push(ProviderEntry { kind, calendar_ids: ids });
            }
            ProviderKind::Outlook => {
                println!();
                println!("Outlook / Microsoft 365: after this wizard, ensure");
                println!("~/.config/glint/credentials/microsoft_oauth_client.toml has your");
                println!("Azure app client_id, then run `glint --auth microsoft`.");
                let ids = read_comma_list(
                    "Calendar IDs (comma-separated, empty for primary): ",
                )?;
                providers.push(ProviderEntry { kind, calendar_ids: ids });
            }
            ProviderKind::Caldav => {
                println!();
                println!("CalDAV: we'll prompt for server + username and write");
                println!("~/.config/glint/credentials/caldav.toml. For Apple iCloud,");
                println!("generate an app-specific password at appleid.apple.com.");
                let server = {
                    let raw = read_line("Server URL [https://caldav.icloud.com]: ")?;
                    if raw.is_empty() { "https://caldav.icloud.com".to_string() } else { raw }
                };
                let username = read_line("Username (e.g. you@icloud.com): ")?;
                let cal_urls = read_comma_list(
                    "Calendar URLs (comma-separated, empty to auto-discover): ",
                )?;
                providers.push(ProviderEntry { kind, calendar_ids: cal_urls });
                write_caldav_credentials(&server, &username, report)?;
            }
            ProviderKind::Local => {
                println!("Local: default is local with example events. Adding a local entry.");
                providers.push(ProviderEntry { kind, calendar_ids: Vec::new() });
            }
        }
    }

    let toml = render_calendar_toml(&providers, &existing);
    std::fs::write(&path, toml)
        .with_context(|| format!("failed to write {}", path.display()))?;
    report.note(&path);
    println!("Wrote {}", path.display());
    Ok(())
}

fn provider_kind_str(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Local => "local",
        ProviderKind::Google => "google",
        ProviderKind::Outlook => "outlook",
        ProviderKind::Caldav => "caldav",
    }
}

fn render_calendar_toml(
    providers: &[ProviderEntry],
    existing: &CalendarConfig,
) -> String {
    let mut out = String::new();
    out.push_str("default_view = \"day\"\n");
    out.push_str("poll_interval_secs = 60\n");
    if providers.is_empty() {
        // Fall back to local single-provider mode with the existing seed
        // events, so the calendar widget still has something to show.
        out.push_str("provider = \"local\"\n");
        if !existing.events.is_empty() {
            out.push('\n');
            out.push_str("# Existing example events preserved from your previous config.\n");
            for ev in &existing.events {
                out.push_str(&render_local_event(ev));
            }
        } else {
            out.push('\n');
            out.push_str(
                "# No local events configured. Add [[events]] blocks here with title, start, end.\n",
            );
        }
    } else {
        out.push_str("# Multi-provider mode: each [[providers]] entry is one calendar account.\n");
        for p in providers {
            out.push_str("\n[[providers]]\n");
            out.push_str(&format!("kind = \"{}\"\n", provider_kind_str(p.kind)));
            out.push_str(&format!(
                "calendar_ids = {}\n",
                toml_string_array(&p.calendar_ids)
            ));
        }
    }
    out
}

/// Render one `[[events]]` block from a stored RawEvent. We round-trip through
/// the public fields (title, start, end, etc.) — anything we don't recognize
/// in v1 is dropped.
fn render_local_event(ev: &crate::widgets::calendar::local::RawEvent) -> String {
    let mut out = String::new();
    out.push_str("\n[[events]]\n");
    out.push_str(&format!("title = {}\n", toml_string(&ev.title)));
    out.push_str(&format!("start = {}\n", toml_string(&ev.start)));
    if !ev.end.is_empty() {
        out.push_str(&format!("end = {}\n", toml_string(&ev.end)));
    }
    if ev.all_day {
        out.push_str("all_day = true\n");
    }
    if !ev.calendar.is_empty() && ev.calendar != "default" {
        out.push_str(&format!("calendar = {}\n", toml_string(&ev.calendar)));
    }
    if let Some(loc) = &ev.location {
        out.push_str(&format!("location = {}\n", toml_string(loc)));
    }
    out
}

fn write_caldav_credentials(server: &str, username: &str, report: &mut WizardReport) -> Result<()> {
    let creds_dir = credentials_dir()?;
    let path = creds_dir.join("caldav.toml");
    let mut out = String::new();
    out.push_str("# CalDAV credentials. Replace app_password with the actual\n");
    out.push_str("# app-specific password generated by your provider.\n");
    out.push_str(&format!("server = {}\n", toml_string(server)));
    out.push_str(&format!("username = {}\n", toml_string(username)));
    out.push_str("app_password = \"REPLACE_WITH_APP_SPECIFIC_PASSWORD\"\n");
    std::fs::write(&path, out)
        .with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    println!("Wrote {} — open it and paste your app password.", path.display());
    report.note(&path);
    Ok(())
}

// ── Step 4: LLM API key ─────────────────────────────────────────────────────

#[cfg(feature = "widget-resources")]
fn step_resources(report: &mut WizardReport, instance: &str) -> Result<()> {
    println!();
    let header = instance_header("Resources", instance);
    println!("── Step 7: {header} ─────────────────────────────────────────");
    let stem = widget_stem("resources", instance);
    let existing: ResourcesConfig = config::load_widget_toml(&stem).unwrap_or_default();
    let path = config_dir()?.join(format!("{stem}.toml"));

    println!("Current refresh interval: {}s", existing.poll_interval_secs);
    println!("Current top-N processes : {}", existing.top_n_processes);
    println!(
        "Current sort order      : by {}",
        if existing.sort_by_memory { "memory" } else { "CPU" }
    );

    if !confirm(&format!("Edit {header}?"), false)? {
        return Ok(());
    }

    let interval = loop {
        let raw = read_line(&format!(
            "Refresh interval in seconds (≥1, [keep] for {}): ",
            existing.poll_interval_secs
        ))?;
        if raw.trim().is_empty() || raw.trim().eq_ignore_ascii_case("keep") {
            break existing.poll_interval_secs;
        }
        match raw.trim().parse::<u64>() {
            Ok(n) if n >= 1 => break n,
            _ => println!("Enter a positive integer (or `keep`)."),
        }
    };

    let top_n = loop {
        let raw = read_line(&format!(
            "Number of top processes to show (1-40, [keep] for {}): ",
            existing.top_n_processes
        ))?;
        if raw.trim().is_empty() || raw.trim().eq_ignore_ascii_case("keep") {
            break existing.top_n_processes;
        }
        match raw.trim().parse::<usize>() {
            Ok(n) if (1..=40).contains(&n) => break n,
            _ => println!("Enter a number between 1 and 40 (or `keep`)."),
        }
    };

    let sort_by_memory = confirm("Sort by memory (instead of CPU)?", existing.sort_by_memory)?;

    let toml = render_resources_toml(interval, top_n, sort_by_memory);
    std::fs::write(&path, toml)
        .with_context(|| format!("failed to write {}", path.display()))?;
    report.note(&path);
    println!("Wrote {}", path.display());
    Ok(())
}

#[cfg(feature = "widget-resources")]
fn render_resources_toml(interval_secs: u64, top_n: usize, sort_by_memory: bool) -> String {
    let mut out = String::new();
    out.push_str("# Resources widget — CPU / memory / top-process snapshot.\n");
    out.push_str("# Press `m` while focused to flip CPU/memory sort, `r` to force refresh.\n\n");
    out.push_str(&format!("poll_interval_secs = {interval_secs}\n"));
    out.push_str(&format!("top_n_processes = {top_n}\n"));
    out.push_str(&format!("sort_by_memory = {sort_by_memory}\n"));
    out
}

#[cfg(feature = "widget-gallery")]
fn step_gallery(report: &mut WizardReport, instance: &str) -> Result<()> {
    println!();
    let header = instance_header("Gallery", instance);
    println!("── Step 8: {header} ─────────────────────────────────────────");
    let stem = widget_stem("gallery", instance);
    let existing: GalleryConfig = config::load_widget_toml(&stem).unwrap_or_default();
    let path = config_dir()?.join(format!("{stem}.toml"));

    println!(
        "Current images          : {}",
        if existing.images.is_empty() {
            "(none)".to_string()
        } else {
            format!("{} entries", existing.images.len())
        }
    );
    println!("Current rotation        : {}s (0 = paused)", existing.rotation_secs);

    if !confirm(&format!("Edit {header}?"), false)? {
        return Ok(());
    }

    println!();
    println!("Image source — pick one:");
    println!("  [d] Scan a directory for images (.jpg/.jpeg/.png/.gif/.webp)");
    println!("  [f] Enter a comma-separated list of file paths");
    println!("  [k] Keep the existing list");
    let pick = select_letter(
        "Choice:",
        &[
            ('d', "Directory scan"),
            ('f', "File list"),
            ('k', "Keep existing"),
        ],
    )?;

    let images: Vec<String> = match pick {
        'd' => {
            let dir_raw = read_line("Directory path (~/ expands to $HOME): ")?;
            if dir_raw.is_empty() {
                println!("No directory entered — keeping existing list.");
                existing.images.clone()
            } else {
                let dir = resolve_tilde(&dir_raw);
                match scan_image_directory(&dir) {
                    Ok(found) if found.is_empty() => {
                        println!(
                            "No supported images found in {}. Keeping existing list.",
                            dir.display()
                        );
                        existing.images.clone()
                    }
                    Ok(found) => {
                        println!("Found {} images in {}.", found.len(), dir.display());
                        found
                    }
                    Err(err) => {
                        println!(
                            "Couldn't read {} ({err}). Keeping existing list.",
                            dir.display()
                        );
                        existing.images.clone()
                    }
                }
            }
        }
        'f' => {
            let paths = read_comma_list(
                "File paths — comma-separated (e.g. ~/Pictures/a.jpg, /tmp/b.png): ",
            )?;
            if paths.is_empty() {
                println!("Empty list — keeping existing.");
                existing.images.clone()
            } else {
                paths
            }
        }
        _ => existing.images.clone(),
    };

    let rotation = loop {
        let raw = read_line(&format!(
            "Rotation interval in seconds (0 = paused, [keep] for {}): ",
            existing.rotation_secs
        ))?;
        if raw.trim().is_empty() || raw.trim().eq_ignore_ascii_case("keep") {
            break existing.rotation_secs;
        }
        match raw.trim().parse::<u64>() {
            Ok(n) => break n,
            _ => println!("Enter a non-negative integer (or `keep`)."),
        }
    };

    let toml = render_gallery_toml(&images, rotation);
    std::fs::write(&path, toml)
        .with_context(|| format!("failed to write {}", path.display()))?;
    report.note(&path);
    println!("Wrote {}", path.display());
    Ok(())
}

#[cfg(feature = "widget-gallery")]
fn render_gallery_toml(images: &[String], rotation_secs: u64) -> String {
    let mut out = String::new();
    out.push_str("# Photo slideshow for the Gallery widget.\n");
    out.push_str("#\n");
    out.push_str("# `images` accepts literal file paths and simple globs:\n");
    out.push_str("#   \"~/Pictures/cover.png\"   — one file\n");
    out.push_str("#   \"~/Pictures/*\"           — every image file in the directory\n");
    out.push_str("#   \"~/Downloads/*.jpg\"      — every .jpg in the directory\n");
    out.push_str("#\n");
    out.push_str("# `rotation_secs`        : seconds between slides (0 = start paused).\n");
    out.push_str("# `rescan_interval_secs` : how often glob entries are re-scanned for\n");
    out.push_str("#                          newly added images (0 = disable, min 30s).\n");
    out.push_str("# Press `p` to pause/resume, `n`/`N` to step, ↑/↓ to tune the timer.\n\n");
    out.push_str("images = [\n");
    for img in images {
        out.push_str(&format!("  {},\n", toml_string(img)));
    }
    out.push_str("]\n");
    out.push_str(&format!("rotation_secs = {rotation_secs}\n"));
    out.push_str("rescan_interval_secs = 300\n");
    out
}

/// `~/` → `$HOME`, otherwise pass through. Local to this module so we
/// don't have to leak the gallery widget's helper out.
#[cfg(feature = "widget-gallery")]
fn resolve_tilde(raw: &str) -> std::path::PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(raw)
}

/// Read `dir`, return paths whose extension matches a known image
/// suffix (case-insensitive). One level deep — we don't recurse since
/// that surprises users who only meant to grab the top-level folder.
#[cfg(feature = "widget-gallery")]
fn scan_image_directory(dir: &std::path::Path) -> std::io::Result<Vec<String>> {
    const EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];
    let mut paths: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let matches_ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                let lower = e.to_ascii_lowercase();
                EXTS.iter().any(|x| **x == lower)
            })
            .unwrap_or(false);
        if matches_ext {
            paths.push(path.to_string_lossy().into_owned());
        }
    }
    paths.sort();
    Ok(paths)
}

#[cfg(feature = "widget-email")]
fn step_email(report: &mut WizardReport, instance: &str) -> Result<()> {
    println!();
    let header = instance_header("Email", instance);
    println!("── Step 9: {header} ─────────────────────────────────────────");
    let stem = widget_stem("email", instance);
    let existing: EmailConfig = config::load_widget_toml(&stem).unwrap_or_default();
    let path = config_dir()?.join(format!("{stem}.toml"));

    println!("Current provider        : {}", existing.provider);
    println!("Current latest_days     : {}", existing.latest_days);
    println!("Current refresh_minutes : {}", existing.refresh_minutes);
    println!(
        "Current folders         : {}",
        if existing.folders.is_empty() {
            "(none)".to_string()
        } else {
            existing.folders.join(", ")
        }
    );
    println!("Summarize with LLM      : {}", existing.summarize_with_llm);

    if !confirm(&format!("Edit {header}?"), false)? {
        return Ok(());
    }

    let provider_letter = select_letter(
        "Pick an email provider:",
        &[
            ('o', "Outlook (Microsoft Graph)"),
            ('g', "Gmail"),
        ],
    )?;
    let provider = if provider_letter == 'g' { "gmail" } else { "outlook" };

    let latest_days = loop {
        let raw = read_line(&format!(
            "Days of history to fetch (1-30, [keep] for {}): ",
            existing.latest_days
        ))?;
        if raw.trim().is_empty() || raw.trim().eq_ignore_ascii_case("keep") {
            break existing.latest_days;
        }
        match raw.trim().parse::<u32>() {
            Ok(n) if (1..=30).contains(&n) => break n,
            _ => println!("Enter a number between 1 and 30 (or `keep`)."),
        }
    };

    let refresh_minutes = loop {
        let raw = read_line(&format!(
            "Refresh interval in minutes (≥1, [keep] for {}): ",
            existing.refresh_minutes
        ))?;
        if raw.trim().is_empty() || raw.trim().eq_ignore_ascii_case("keep") {
            break existing.refresh_minutes;
        }
        match raw.trim().parse::<u64>() {
            Ok(n) if n >= 1 => break n,
            _ => println!("Enter a positive integer (or `keep`)."),
        }
    };

    let folders_in = read_line(
        "Folders to monitor — comma-separated (Gmail: INBOX,SENT,…; Outlook: inbox,sentitems,…), [keep] to leave as-is: ",
    )?;
    let folders: Vec<String> = match folders_in.trim().to_lowercase().as_str() {
        "keep" | "" => existing.folders.clone(),
        _ => folders_in
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    };
    let folders = if folders.is_empty() {
        vec!["INBOX".to_string()]
    } else {
        folders
    };

    let summarize = confirm(
        "Summarize messages with the LLM when `s` is pressed?",
        existing.summarize_with_llm,
    )?;

    let toml = render_email_toml(provider, latest_days, refresh_minutes, &folders, summarize);
    std::fs::write(&path, toml)
        .with_context(|| format!("failed to write {}", path.display()))?;
    report.note(&path);
    println!("Wrote {}", path.display());
    println!();
    println!(
        "Note: run `glint --auth {}` after this wizard if you haven't already —",
        if provider == "gmail" { "google" } else { "outlook" }
    );
    println!(
        "      Email needs the additional {} scope.",
        if provider == "gmail" {
            "gmail.readonly"
        } else {
            "Mail.Read"
        }
    );
    Ok(())
}

#[cfg(feature = "widget-email")]
fn render_email_toml(
    provider: &str,
    latest_days: u32,
    refresh_minutes: u64,
    folders: &[String],
    summarize_with_llm: bool,
) -> String {
    let mut out = String::new();
    out.push_str("# Email widget — read-only feed of recent messages.\n");
    out.push_str("# Glint never marks messages read on the server; an `e` press\n");
    out.push_str("# locally suppresses the unread indicator via\n");
    out.push_str("# ~/.config/glint/email_seen_<provider>_<account>.json.\n\n");
    out.push_str(&format!("provider = {}\n", toml_string(provider)));
    out.push_str(&format!("latest_days = {latest_days}\n"));
    out.push_str(&format!("refresh_minutes = {refresh_minutes}\n"));
    out.push_str(&format!("folders = {}\n", toml_string_array(folders)));
    out.push_str(&format!(
        "summarize_with_llm = {}\n",
        if summarize_with_llm { "true" } else { "false" }
    ));
    out
}

fn step_llm_key(report: &mut WizardReport) -> Result<()> {
    println!();
    println!("── Step 10: LLM (Anthropic) API key ─────────────────────────");
    let creds = credentials_dir()?;
    let path = creds.join("anthropic_key.toml");
    let current_set = existing_anthropic_key_set(&path);
    if current_set {
        println!("An Anthropic API key is currently configured at {}", path.display());
    } else {
        println!("No Anthropic API key is configured (path: {})", path.display());
    }

    if !confirm("Configure LLM (Anthropic) API key?", false)? {
        return Ok(());
    }

    let raw = read_line("Paste your Anthropic API key (empty to clear): ")?;
    let key = raw.trim();
    let mut out = String::new();
    out.push_str("# Anthropic API key. Get one at https://console.anthropic.com/.\n");
    if key.is_empty() {
        out.push_str("# api_key = \"\"\n");
    } else {
        out.push_str(&format!("api_key = {}\n", toml_string(key)));
    }
    std::fs::write(&path, out)
        .with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    if key.is_empty() {
        println!("Cleared key — LLM features will fall back to non-LLM mode.");
    } else {
        println!("Wrote {} (chmod 0600).", path.display());
    }
    report.note(&path);
    Ok(())
}

/// Returns true if the credentials file exists and contains a non-placeholder
/// `api_key = "..."` value. Used purely for the prompt — we never read or
/// echo the actual key.
fn existing_anthropic_key_set(path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    for line in contents.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("api_key") {
            let rest = rest.trim_start().trim_start_matches('=').trim();
            let unquoted = rest.trim_matches('"');
            if !unquoted.is_empty() && !unquoted.starts_with("REPLACE_WITH_") {
                return true;
            }
        }
    }
    false
}
