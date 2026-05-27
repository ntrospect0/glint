//! File-system watcher that notifies the main loop when any
//! `~/.config/glint/*.toml` file changes, so widgets can hot-reload their
//! config without an app restart.
//!
//! We watch the directory rather than individual files because editors often
//! rename-on-save (vim, neovim with default settings) — the original file
//! handle becomes invalid mid-write. Directory-level events catch those.

use std::path::PathBuf;

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// Spawn a notify watcher on `~/.config/glint/`. Emits one `PathBuf` per
/// (likely) file mutation through the returned channel. Caller is responsible
/// for filtering by extension and tolerating spurious / duplicate events
/// (editors often fire several per save).
pub fn spawn(dir: PathBuf) -> Result<mpsc::Receiver<PathBuf>> {
    let (tx, rx) = mpsc::channel::<PathBuf>(32);

    // notify needs a blocking-thread closure. tokio's runtime is multi-thread
    // by default, so blocking_send is safe to call from this thread.
    let mut watcher = notify::recommended_watcher(
        move |res: notify::Result<Event>| match res {
            Ok(event) => {
                if matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    for path in event.paths {
                        // Drop the result; if the receiver is gone the app is
                        // exiting and there's nothing to do.
                        let _ = tx.blocking_send(path);
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "config watcher error"),
        },
    )
    .context("failed to create config watcher")?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", dir.display()))?;

    // The watcher must outlive the receiver — we leak it into a detached
    // tokio task that just holds the handle alive. When the receiver is
    // dropped, the task winds down via the channel-closed branch.
    tokio::spawn(async move {
        let _keep_alive = watcher;
        // Park the task forever; the only thing it does is keep `watcher`
        // alive so its background thread continues delivering events.
        std::future::pending::<()>().await;
    });

    Ok(rx)
}
