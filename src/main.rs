use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};

mod app;
mod auth;
mod config;
mod event;
mod geolocation;
mod llm;
mod providers;
mod theme;
mod ui;
mod widgets;

/// glint — terminal dashboard for stocks, calendar, news, and beyond.
#[derive(Parser, Debug)]
#[command(name = "glint", version, about, long_about = None)]
struct Cli {
    /// Create ~/.config/glint/ and seed default config files, then exit.
    #[arg(long)]
    init: bool,

    /// Run an authentication flow for the given provider, then exit.
    #[arg(long, value_enum, value_name = "PROVIDER")]
    auth: Option<AuthTarget>,

    /// Path to a config file (overrides the default XDG location).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AuthTarget {
    Google,
    Outlook,
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
        if let Some(target) = cli.auth {
            return run_auth(target).await;
        }
        app::run(cli.config).await
    })
}

async fn run_auth(target: AuthTarget) -> Result<()> {
    match target {
        AuthTarget::Google => {
            let client = auth::google::OAuthClientConfig::load()?;
            auth::google::flow::run(&client).await?;
            println!("Google Calendar authorization complete.");
            Ok(())
        }
        AuthTarget::Outlook => {
            let client = auth::microsoft::OAuthClientConfig::load()?;
            auth::microsoft::flow::run(&client).await?;
            println!("Microsoft Outlook authorization complete.");
            Ok(())
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));

    // The TUI runs in alternate-screen mode, so writing tracing to stderr or
    // stdout corrupts the dashboard the moment any widget logs a warning.
    // Route logs to ~/.config/glint/glint.log instead — tail it when debugging.
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
