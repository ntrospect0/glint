// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Local "seen via glint" persistence. Glint never writes read-state back to
//! the server (Gmail / Graph), so we maintain a tiny on-disk set so messages
//! the user has expanded inside the dashboard stop showing the `●` indicator
//! even if they remain unread on the provider.
//!
//! One file per (provider, account) pair, e.g.
//! `~/.config/glint/email_seen_outlook_alice_at_example.com.json`.
//! Contents: `{ "seen": ["id_1", "id_2"], "last_pruned": "<iso>" }`.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDisk {
    #[serde(default)]
    seen: Vec<String>,
    #[serde(default)]
    last_pruned: Option<DateTime<Utc>>,
}

pub struct SeenStore {
    path: PathBuf,
    seen: HashSet<String>,
    last_pruned: Option<DateTime<Utc>>,
}

impl SeenStore {
    /// Open the seen-store for the given provider+account, creating an empty
    /// one if no file exists yet. A failing parse is logged and treated as
    /// "no entries"; we never want a corrupt cache file to block the widget.
    pub fn load(provider: &str, account: &str) -> Result<Self> {
        let dir = config_dir()?;
        // Ensure the parent dir exists — on a fresh install the glint config
        // dir might not have been seeded yet, and we'd otherwise fail to
        // write the seen file on the first `e` press.
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!(
            "email_seen_{}_{}.json",
            provider,
            sanitize_account(account)
        ));
        Self::load_at_path(path)
    }

    /// Test hook: load from a caller-supplied path so unit tests can be
    /// isolated from XDG_CONFIG_HOME (which is process-global and races
    /// across parallel tests).
    fn load_at_path(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let (seen, last_pruned) = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str::<OnDisk>(&text) {
                    Ok(d) => (d.seen.into_iter().collect(), d.last_pruned),
                    Err(err) => {
                        tracing::warn!(error = %err, path = %path.display(), "seen-store parse failed, starting fresh");
                        (HashSet::new(), None)
                    }
                },
                Err(err) => {
                    tracing::warn!(error = %err, path = %path.display(), "seen-store read failed, starting fresh");
                    (HashSet::new(), None)
                }
            }
        } else {
            (HashSet::new(), None)
        };
        Ok(Self {
            path,
            seen,
            last_pruned,
        })
    }

    pub fn contains(&self, id: &str) -> bool {
        self.seen.contains(id)
    }

    /// Mark `id` as seen and immediately persist. Persistence failure is
    /// surfaced but the in-memory set still has the update so the current
    /// session reflects the change.
    pub fn mark_seen(&mut self, id: &str) -> Result<()> {
        if !self.seen.insert(id.to_string()) {
            return Ok(());
        }
        self.persist()
    }

    /// Drop ids known to be older than `days` worth of *seen state* — we
    /// don't know per-id timestamps, so the heuristic is: if the set grows
    /// unbounded and it's been more than `days` since the last prune,
    /// truncate it to half. Cheap, effective, never wrong in a way that
    /// affects correctness (the worst case is showing an unread badge for
    /// a message the user already opened in glint — server-unread state
    /// still drives the badge in that case, so the user will see it as
    /// "unread again" and re-trigger seen on the next `e`).
    #[allow(dead_code)] // called opportunistically from widget construction.
    pub fn prune_older_than_days(&mut self, days: u32) -> Result<()> {
        let now = Utc::now();
        let should_prune = match self.last_pruned {
            None => true,
            Some(t) => (now - t).num_days() as u32 >= days,
        };
        if !should_prune {
            return Ok(());
        }
        // Don't churn small caches.
        if self.seen.len() > 2048 {
            let take = self.seen.len() / 2;
            let kept: HashSet<String> = self.seen.iter().take(take).cloned().collect();
            self.seen = kept;
        }
        self.last_pruned = Some(now);
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let disk = OnDisk {
            seen: self.seen.iter().cloned().collect(),
            last_pruned: self.last_pruned,
        };
        let text = serde_json::to_string(&disk).context("seen-store serialize failed")?;
        std::fs::write(&self.path, text)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        Ok(())
    }
}

/// Lowercase + replace `@` with `_at_` and any other non-alphanumeric with
/// `_` so the filename is always portable. We don't care about preserving
/// the original address — the user never sees this filename, only the file
/// itself in `~/.config/glint/`.
fn sanitize_account(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 4);
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower == '@' {
            out.push_str("_at_");
        } else if lower.is_ascii_alphanumeric() || lower == '.' || lower == '-' {
            out.push(lower);
        } else {
            out.push('_');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Allocate an isolated test path. Uses the system temp dir + a counter
    /// so parallel tests never collide. No process-global state involved.
    fn temp_path(tag: &str) -> PathBuf {
        let nano = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "glint-seen-{tag}-{}-{nano}.json",
            std::process::id()
        ))
    }

    #[test]
    fn sanitize_replaces_at_and_specials() {
        assert_eq!(sanitize_account("Alice@Example.com"), "alice_at_example.com");
        assert_eq!(sanitize_account("foo+bar@baz.io"), "foo_bar_at_baz.io");
        assert_eq!(sanitize_account("a/b\\c"), "a_b_c");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let path = temp_path("roundtrip");
        let _cleanup = scopeguard_remove(path.clone());

        let mut s = SeenStore::load_at_path(path.clone()).unwrap();
        assert!(!s.contains("msg-1"));
        s.mark_seen("msg-1").unwrap();
        s.mark_seen("msg-2").unwrap();
        // mark_seen on the same id is idempotent.
        s.mark_seen("msg-1").unwrap();

        let s2 = SeenStore::load_at_path(path).unwrap();
        assert!(s2.contains("msg-1"));
        assert!(s2.contains("msg-2"));
        assert!(!s2.contains("msg-3"));
    }

    #[test]
    fn missing_file_returns_empty_store() {
        let path = temp_path("missing");
        let _cleanup = scopeguard_remove(path.clone());
        let s = SeenStore::load_at_path(path).unwrap();
        assert!(!s.contains("anything"));
    }

    /// Tiny RAII helper — drops the test file when the guard goes out of
    /// scope, so the system temp dir doesn't accumulate cruft.
    struct RemoveOnDrop(PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn scopeguard_remove(path: PathBuf) -> RemoveOnDrop {
        RemoveOnDrop(path)
    }
}
