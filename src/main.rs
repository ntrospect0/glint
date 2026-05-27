// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Parser;

use crate::widgets::parse_widget_ref;

mod app;
mod auth;
mod cache;
mod clipboard;
mod config;
mod event;
mod geolocation;
mod http;
mod llm;
mod runtime_state;
mod theme;
mod ui;
mod widgets;
mod wizard;

/// glint — terminal dashboard for stocks, calendar, news, and beyond.
#[derive(Parser, Debug)]
#[command(name = "glint", version, about, long_about = None)]
struct Cli {
    /// Create ~/.config/glint/ and seed default config files, then exit.
    #[arg(long)]
    init: bool,

    /// Launch the interactive setup wizard (plain stdin/stdout — no TUI), then exit.
    #[arg(long)]
    setup: bool,

    /// Run an authentication flow for the given provider, then exit.
    /// Registered names: see `auth::registry::PROVIDERS`. Unknown names
    /// print the available list as an error.
    #[arg(long, value_name = "PROVIDER")]
    auth: Option<String>,

    /// Clear cached data before launching. With no value, wipes
    /// `$XDG_CACHE_HOME/glint/` entirely. Pass a widget kind (`news`) or
    /// `kind@instance` (`news@home`) to scope the clear. Prompts for [y/N]
    /// confirmation; use --clear-cache-forced to skip the prompt.
    #[arg(long, value_name = "TARGET", num_args = 0..=1, default_missing_value = "*")]
    clear_cache: Option<String>,

    /// Like --clear-cache but skips the confirmation prompt.
    #[arg(long, value_name = "TARGET", num_args = 0..=1, default_missing_value = "*")]
    clear_cache_forced: Option<String>,

    /// Path to a config file (overrides the default XDG location).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        if cli.init {
            let path = config::init_default_config()?;
            println!("Initialized config at {}", path.display());
            return Ok(());
        }
        // --clear-cache / --clear-cache-forced fire before the rest of startup
        // so a confirmed clear flows straight into the dashboard with a fresh
        // slate. Conflicting flags are flagged early.
        if cli.clear_cache.is_some() && cli.clear_cache_forced.is_some() {
            return Err(anyhow!(
                "pass either --clear-cache or --clear-cache-forced, not both"
            ));
        }
        if let Some(target) = cli.clear_cache.as_deref() {
            run_clear_cache(target, true)?;
        }
        if let Some(target) = cli.clear_cache_forced.as_deref() {
            run_clear_cache(target, false)?;
        }
        if cli.setup {
            // The wizard is fully synchronous — it does plain stdin/stdout
            // text prompts and never touches the tokio runtime. The runtime
            // already exists at this point; we just don't use it here.
            //
            // Seed any missing default config files first. `init_default_config`
            // is idempotent: existing files are left untouched, so it's safe
            // to call here even when the user is just re-running the wizard.
            // Without this, fresh installs hit the theme picker with no
            // colorschemes.toml on disk and the scheme list is empty.
            config::init_default_config()?;
            return wizard::run();
        }
        if let Some(target) = cli.auth.as_deref() {
            return run_auth(target).await;
        }

        // First-run UX: drop into the setup wizard before opening the TUI.
        // `--config <path>` opts out — the user explicitly named a file.
        if cli.config.is_none() && !looks_initialized() {
            eprintln!("No config detected at ~/.config/glint/config.toml — launching the setup wizard.");
            eprintln!("(You can re-run `glint --setup` later to make changes.)");
            eprintln!();
            config::init_default_config()?;
            wizard::run()?;
            eprintln!();
            eprintln!("Launching glint…");
        }

        app::run(cli.config).await
    })
}

/// True when `~/.config/glint/config.toml` exists. Path-resolution failures
/// (no home dir, etc.) report `true` so an unusual environment doesn't
/// block launch on a wizard prompt.
fn looks_initialized() -> bool {
    config::config_path()
        .map(|p| p.exists())
        .unwrap_or(true)
}

/// Resolve a `--clear-cache <target>` argument and apply it to the on-disk
/// cache. `target` is `"*"` (the sentinel for "no value passed"), a widget
/// kind (`news`), or `kind@instance` (`news@home`). When `confirm` is true,
/// the user must answer `y` at a [y/N] prompt; otherwise no prompt is shown.
/// A declined prompt is non-fatal — startup continues with the cache intact.
fn run_clear_cache(target: &str, confirm: bool) -> Result<()> {
    let cache = cache::Cache::open_default().context("failed to open cache directory")?;
    let action = ClearAction::parse(target);

    if confirm && !prompt_yes_no(&action.confirm_message())? {
        println!("Cache clear cancelled — continuing without changes.");
        return Ok(());
    }

    match &action {
        ClearAction::All => cache.clear_all()?,
        ClearAction::Kind(kind) => cache.clear_widget(kind)?,
        ClearAction::Instance { kind, instance } => cache.clear_instance(kind, instance)?,
    }
    println!("{}", action.success_message(cache.root()));
    Ok(())
}

enum ClearAction {
    All,
    Kind(String),
    Instance { kind: String, instance: String },
}

impl ClearAction {
    fn parse(target: &str) -> Self {
        if target == "*" || target.is_empty() {
            return Self::All;
        }
        let (kind, instance) = parse_widget_ref(target);
        if instance == "main" && !target.contains('@') {
            Self::Kind(kind)
        } else {
            Self::Instance { kind, instance }
        }
    }

    fn confirm_message(&self) -> String {
        match self {
            Self::All => "Clear ALL cached glint data?".to_string(),
            Self::Kind(kind) => format!("Clear all cached data for widget {kind:?}?"),
            Self::Instance { kind, instance } => {
                format!("Clear cached data for {kind}@{instance}?")
            }
        }
    }

    fn success_message(&self, root: &std::path::Path) -> String {
        match self {
            Self::All => format!("Cleared cache at {}", root.display()),
            Self::Kind(kind) => format!("Cleared cache for widget {kind:?}"),
            Self::Instance { kind, instance } => {
                format!("Cleared cache for {kind}@{instance}")
            }
        }
    }
}

/// Single-line `[y/N]` prompt on stdin. Empty / N / n / EOF all return false.
fn prompt_yes_no(question: &str) -> Result<bool> {
    print!("{question} [y/N]: ");
    io::stdout().flush().ok();
    let stdin = io::stdin();
    let mut buf = String::new();
    let n = stdin
        .lock()
        .read_line(&mut buf)
        .context("failed to read confirmation from stdin")?;
    if n == 0 {
        return Ok(false);
    }
    Ok(matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

async fn run_auth(target: &str) -> Result<()> {
    match auth::registry::find(target) {
        Some(provider) => (provider.run)().await,
        None => Err(anyhow!(
            "unknown auth provider {target:?}. Known providers: {}",
            auth::registry::names_csv()
        )),
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));

    // Route logs to ~/.config/glint/glint.log — alt-screen mode would corrupt
    // the dashboard the moment a widget logged to stderr. `tail -f` it when
    // debugging.
    let Ok(dir) = config::config_dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("glint.log");
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .try_init();
}
