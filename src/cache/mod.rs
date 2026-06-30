// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Platform-level persistent cache.
//!
//! Widgets store fetched data here so the dashboard can paint cached results
//! on the first frame and refresh in the background. Each widget gets a
//! [`ScopedCache`] via [`WidgetCtx::cache`](crate::widgets::WidgetCtx), already
//! namespaced to its `(kind, instance)`.
//!
//! ## Usage
//!
//! ```ignore
//! // In Widget construction:
//! let seed: Option<CacheEntry<Articles>> = ctx.cache.load("articles");
//! let initial = seed.map(|e| (e.value, e.stored_at));
//!
//! // After a successful fetch:
//! let _ = ctx.cache.store("articles", &articles);
//! ```
//!
//! Cache failures are intentionally non-fatal: [`load`](ScopedCache::load)
//! returns `None` on parse / IO error, and [`store`](ScopedCache::store)
//! errors should be logged and ignored.
//!
//! ## Storage layout
//!
//! Files live under `$XDG_CACHE_HOME/glint/` (typically `~/.cache/glint/`):
//!
//! ```text
//! ~/.cache/glint/<kind>/<instance>/<key>.json
//! ```
//!
//! Each file is `{ "stored_at": <RFC3339>, "value": <T> }`. Writes go to a
//! sibling temp file and rename, so an interrupted process can't corrupt an
//! existing entry. Concurrency is handled by atomic rename — multiple writers
//! resolve last-write-wins without an in-process lock.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

/// Monotonic counter for atomic-write tmp suffixes. `SystemTime` nanos alone
/// can collide between threads that sample the clock at the same tick;
/// an in-process counter guarantees uniqueness.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Root cache rooted at `~/.cache/glint/`. Cloneable cheaply (it's just a path).
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Open the default cache under `$XDG_CACHE_HOME/glint/` (falling back to
    /// `~/.cache/glint/`). The directory is created lazily on first write.
    pub fn open_default() -> Result<Self> {
        Ok(Self {
            root: default_dir()?,
        })
    }

    /// Open a cache at an explicit root. Used by tests and the home-dir
    /// fallback path in `App::new`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Scope to one widget instance. Cheap — clones two `PathBuf`s.
    pub fn scoped(&self, kind: &str, instance: &str) -> ScopedCache {
        ScopedCache {
            dir: self.root.join(sanitize(kind)).join(sanitize(instance)),
        }
    }

    /// Remove every cached entry across all widgets.
    pub fn clear_all(&self) -> Result<()> {
        remove_dir_if_present(&self.root)
    }

    /// Remove every cached entry for one widget kind (all instances).
    pub fn clear_widget(&self, kind: &str) -> Result<()> {
        remove_dir_if_present(&self.root.join(sanitize(kind)))
    }

    /// Remove every cached entry for one widget instance.
    pub fn clear_instance(&self, kind: &str, instance: &str) -> Result<()> {
        remove_dir_if_present(&self.root.join(sanitize(kind)).join(sanitize(instance)))
    }

    /// Walk the cache root and remove files older than `max_age`. Best-effort
    /// — individual failures log and are skipped rather than bubbled up so a
    /// single unreadable file doesn't abort the sweep. Empty leaf directories
    /// left after deletion are pruned too. Returns the number of files
    /// removed.
    pub fn sweep_older_than(&self, max_age: Duration) -> usize {
        if !self.root.exists() {
            return 0;
        }
        let cutoff = match SystemTime::now().checked_sub(max_age) {
            Some(t) => t,
            None => return 0,
        };
        let mut removed = 0;
        sweep_dir(&self.root, cutoff, &mut removed);
        removed
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Recursive helper for `sweep_older_than`. Removes any regular file whose
/// modified-time is older than `cutoff`; then removes the directory itself
/// if it became empty.
fn sweep_dir(dir: &Path, cutoff: SystemTime, removed: &mut usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            sweep_dir(&path, cutoff, removed);
            // After recursing, drop the directory if it emptied out.
            let _ = fs::remove_dir(&path);
        } else if meta.is_file() {
            let stale = meta.modified().map(|m| m < cutoff).unwrap_or(false);
            if stale {
                match fs::remove_file(&path) {
                    Ok(()) => *removed += 1,
                    Err(err) => tracing::debug!(
                        path = %path.display(),
                        error = %err,
                        "cache sweep: remove failed"
                    ),
                }
            }
        }
    }
}

fn remove_dir_if_present(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_dir_all(path)
        .with_context(|| format!("failed to clear cache at {}", path.display()))?;
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Build a stable, short cache key from a free-form identifier. Used
/// by widgets that key per-record derivations (article summaries,
/// message summaries, gallery thumbnails) — `prefix` gives the kind
/// a human-readable namespace; the SHA-256 prefix gives a
/// filesystem-safe, collision-resistant suffix without holding the
/// full id in the file name.
pub fn short_hash_key(prefix: &str, identity: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(identity.as_bytes());
    let mut key = String::with_capacity(prefix.len() + 16);
    key.push_str(prefix);
    for b in &digest[..8] {
        use std::fmt::Write;
        let _ = write!(key, "{b:02x}");
    }
    key
}

/// A `Cache` view rooted at one widget's `(kind, instance)` directory. Keys
/// are flat strings; widgets pick whatever scheme fits their data (`articles`,
/// `chart-AAPL-1d`, `messages-INBOX`, …).
#[derive(Debug, Clone)]
pub struct ScopedCache {
    dir: PathBuf,
}

impl ScopedCache {
    /// A throwaway scope under a freshly-generated subdir of the system temp
    /// dir. Used by widget unit tests so they don't pollute the user's real
    /// cache and don't collide with each other across parallel runs.
    pub fn ephemeral() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "glint-ephemeral-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        Self { dir }
    }

    /// Read and deserialise a cached entry. `None` on miss, parse error, or
    /// IO error — corruption auto-degrades to a refetch.
    pub fn load<T: DeserializeOwned>(&self, key: &str) -> Option<CacheEntry<T>> {
        let path = self.path_for(key);
        let contents = fs::read_to_string(&path).ok()?;
        let stored: StoredEntry<T> = match serde_json::from_str(&contents) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    file = %path.display(),
                    error = %err,
                    "cache entry unreadable; will refetch"
                );
                return None;
            }
        };
        Some(CacheEntry {
            value: stored.value,
            stored_at: stored.stored_at,
        })
    }

    /// Serialise + write a value with the current wall-clock timestamp.
    /// Atomic via temp-file + rename.
    pub fn store<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let payload = StoredEntry {
            stored_at: Utc::now(),
            value,
        };
        let serialised = serde_json::to_vec(&payload).context("cache serialise failed")?;
        self.atomic_write(&self.path_for(key), &serialised, key)
    }

    /// Read a raw byte blob (e.g. a downscaled image thumbnail). `None` on
    /// miss or IO error. The entry's `stored_at` is taken from the file's
    /// mtime — enough resolution for "is the cache newer than the source?"
    /// invalidation patterns without doubling every write.
    pub fn load_bytes(&self, key: &str) -> Option<BytesEntry> {
        let path = self.bytes_path_for(key);
        let value = fs::read(&path).ok()?;
        let stored_at = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(Utc::now);
        Some(BytesEntry { value, stored_at })
    }

    /// Atomically write a raw byte blob. Companion to [`load_bytes`].
    pub fn store_bytes(&self, key: &str, value: &[u8]) -> Result<()> {
        self.atomic_write(&self.bytes_path_for(key), value, key)
    }

    /// Remove a single key. Missing keys are not an error. Removes both the
    /// JSON and bytes variant if either exists, so widgets that switch
    /// representations don't leave stale files behind.
    #[allow(dead_code)] // platform primitive; widgets call this on schema changes / user-driven resets.
    pub fn invalidate(&self, key: &str) -> Result<()> {
        remove_if_present(&self.path_for(key))?;
        remove_if_present(&self.bytes_path_for(key))?;
        Ok(())
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.json", sanitize(key)))
    }

    fn bytes_path_for(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.bin", sanitize(key)))
    }

    /// Shared write path for both JSON and bytes entries. pid disambiguates
    /// across processes; an in-process atomic counter disambiguates across
    /// threads (wall-clock nanos can collide). Atomic rename resolves the
    /// last-write-wins on a shared destination.
    fn atomic_write(&self, final_path: &Path, bytes: &[u8], key: &str) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create cache dir {}", self.dir.display()))?;
        let tmp_path = self.dir.join(format!(
            ".{}.{}.{}.tmp",
            sanitize(key),
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        {
            let mut f = fs::File::create(&tmp_path)
                .with_context(|| format!("failed to open {}", tmp_path.display()))?;
            f.write_all(bytes)
                .with_context(|| format!("failed to write {}", tmp_path.display()))?;
            f.sync_all().ok();
        }
        if let Err(err) = fs::rename(&tmp_path, final_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(err).with_context(|| {
                format!(
                    "failed to rename {} → {}",
                    tmp_path.display(),
                    final_path.display()
                )
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CacheEntry<T> {
    pub value: T,
    pub stored_at: DateTime<Utc>,
}

/// Result of [`ScopedCache::load_bytes`]. Mirrors [`CacheEntry`] but for
/// opaque blobs (image thumbs, attachments, …). `stored_at` is the file's
/// mtime, so widgets can compare it directly against a source file's mtime
/// for invalidation.
#[derive(Debug, Clone)]
pub struct BytesEntry {
    pub value: Vec<u8>,
    pub stored_at: DateTime<Utc>,
}

impl<T> CacheEntry<T> {
    /// Wall-clock age. Returns `Duration::ZERO` if `stored_at` is in the
    /// future (clock skew across machines or after a system clock change).
    pub fn age(&self) -> Duration {
        let stored: SystemTime = self.stored_at.into();
        SystemTime::now()
            .duration_since(stored)
            .unwrap_or(Duration::ZERO)
    }

    /// True when the entry was stored within the last `ttl`. Widgets that
    /// poll on an internal `last_attempt: Instant` typically don't need this
    /// (they synthesise the Instant from `age()` at construction); widgets
    /// that read directly from the cache on every render do.
    #[allow(dead_code)]
    pub fn is_within(&self, ttl: Duration) -> bool {
        self.age() <= ttl
    }
}

#[derive(Serialize, Deserialize)]
struct StoredEntry<T> {
    stored_at: DateTime<Utc>,
    #[serde(bound(serialize = "T: Serialize", deserialize = "T: DeserializeOwned"))]
    value: T,
}

fn default_dir() -> Result<PathBuf> {
    let base = match std::env::var("XDG_CACHE_HOME") {
        Ok(xdg) if !xdg.is_empty() => PathBuf::from(xdg).join("glint"),
        _ => dirs::home_dir()
            .context("could not locate user home directory")?
            .join(".cache")
            .join("glint"),
    };
    // Scope the cache to the active profile so fetched payloads don't bleed
    // across profiles; `--clear-cache` then scopes to the active profile too.
    Ok(base
        .join("profiles")
        .join(crate::config::active_profile()))
}

/// Replace path-unfriendly characters in a kind / instance / key segment so
/// `news@home` and other user-supplied strings can't escape the cache dir or
/// produce surprising filesystem names. Conservative allowlist — any char
/// outside `[A-Za-z0-9._-]` becomes `_`.
fn sanitize(segment: &str) -> String {
    segment
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::thread;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Sample {
        title: String,
        count: u32,
    }

    fn tmpdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("glint-cache-test-{}", std::process::id()));
        p.push(format!("{:?}", std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn round_trip_persists_value_and_timestamp() {
        let cache = Cache::at(tmpdir());
        let scoped = cache.scoped("news", "main");
        let s = Sample {
            title: "hello".into(),
            count: 3,
        };
        scoped.store("articles", &s).unwrap();
        let got: CacheEntry<Sample> = scoped.load("articles").unwrap();
        assert_eq!(got.value, s);
        // Timestamp set to roughly now (sanity check — within a minute).
        assert!(got.age() < Duration::from_secs(60));
    }

    #[test]
    fn miss_returns_none() {
        let cache = Cache::at(tmpdir());
        let scoped = cache.scoped("news", "main");
        let got: Option<CacheEntry<Sample>> = scoped.load("nope");
        assert!(got.is_none());
    }

    #[test]
    fn corrupt_file_load_returns_none() {
        let dir = tmpdir();
        let cache = Cache::at(&dir);
        let scoped = cache.scoped("news", "main");
        // Write garbage at the path the scope would use.
        std::fs::create_dir_all(dir.join("news").join("main")).unwrap();
        std::fs::write(
            dir.join("news").join("main").join("articles.json"),
            b"{not json",
        )
        .unwrap();
        let got: Option<CacheEntry<Sample>> = scoped.load("articles");
        assert!(got.is_none());
    }

    #[test]
    fn store_overwrites_in_place() {
        let cache = Cache::at(tmpdir());
        let scoped = cache.scoped("stocks", "main");
        scoped
            .store(
                "quote",
                &Sample {
                    title: "v1".into(),
                    count: 1,
                },
            )
            .unwrap();
        scoped
            .store(
                "quote",
                &Sample {
                    title: "v2".into(),
                    count: 2,
                },
            )
            .unwrap();
        let got: CacheEntry<Sample> = scoped.load("quote").unwrap();
        assert_eq!(got.value.title, "v2");
    }

    #[test]
    fn invalidate_removes_key() {
        let cache = Cache::at(tmpdir());
        let scoped = cache.scoped("weather", "main");
        scoped
            .store(
                "current",
                &Sample {
                    title: "x".into(),
                    count: 0,
                },
            )
            .unwrap();
        scoped.invalidate("current").unwrap();
        assert!(scoped.load::<Sample>("current").is_none());
        // Idempotent.
        scoped.invalidate("current").unwrap();
    }

    #[test]
    fn instance_scoping_isolates_keys() {
        let cache = Cache::at(tmpdir());
        let a = cache.scoped("clock", "home");
        let b = cache.scoped("clock", "office");
        a.store(
            "snapshot",
            &Sample {
                title: "home".into(),
                count: 1,
            },
        )
        .unwrap();
        b.store(
            "snapshot",
            &Sample {
                title: "office".into(),
                count: 2,
            },
        )
        .unwrap();
        assert_eq!(a.load::<Sample>("snapshot").unwrap().value.title, "home");
        assert_eq!(b.load::<Sample>("snapshot").unwrap().value.title, "office");
    }

    #[test]
    fn clear_widget_wipes_only_that_kind() {
        let dir = tmpdir();
        let cache = Cache::at(&dir);
        cache
            .scoped("news", "main")
            .store(
                "a",
                &Sample {
                    title: "1".into(),
                    count: 1,
                },
            )
            .unwrap();
        cache
            .scoped("stocks", "main")
            .store(
                "b",
                &Sample {
                    title: "2".into(),
                    count: 2,
                },
            )
            .unwrap();
        cache.clear_widget("news").unwrap();
        assert!(cache.scoped("news", "main").load::<Sample>("a").is_none());
        assert!(cache.scoped("stocks", "main").load::<Sample>("b").is_some());
    }

    #[test]
    fn clear_instance_wipes_only_that_pair() {
        let dir = tmpdir();
        let cache = Cache::at(&dir);
        cache
            .scoped("clock", "home")
            .store(
                "a",
                &Sample {
                    title: "1".into(),
                    count: 1,
                },
            )
            .unwrap();
        cache
            .scoped("clock", "office")
            .store(
                "a",
                &Sample {
                    title: "2".into(),
                    count: 2,
                },
            )
            .unwrap();
        cache.clear_instance("clock", "home").unwrap();
        assert!(cache.scoped("clock", "home").load::<Sample>("a").is_none());
        assert!(cache
            .scoped("clock", "office")
            .load::<Sample>("a")
            .is_some());
    }

    #[test]
    fn clear_methods_are_idempotent_on_missing_paths() {
        let cache = Cache::at(tmpdir());
        cache.clear_all().unwrap();
        cache.clear_widget("never-existed").unwrap();
        cache.clear_instance("never", "existed").unwrap();
    }

    #[test]
    fn clear_all_wipes_every_widget() {
        let dir = tmpdir();
        let cache = Cache::at(&dir);
        cache
            .scoped("news", "main")
            .store(
                "a",
                &Sample {
                    title: "1".into(),
                    count: 1,
                },
            )
            .unwrap();
        cache
            .scoped("stocks", "main")
            .store(
                "b",
                &Sample {
                    title: "2".into(),
                    count: 2,
                },
            )
            .unwrap();
        cache.clear_all().unwrap();
        assert!(cache.scoped("news", "main").load::<Sample>("a").is_none());
        assert!(cache.scoped("stocks", "main").load::<Sample>("b").is_none());
    }

    #[test]
    fn unfriendly_chars_in_segments_get_sanitized() {
        let dir = tmpdir();
        let cache = Cache::at(&dir);
        let scoped = cache.scoped("news@home", "../../etc");
        scoped
            .store(
                "a/b",
                &Sample {
                    title: "x".into(),
                    count: 1,
                },
            )
            .unwrap();
        // Must round-trip even though every segment had unfriendly chars.
        let got: CacheEntry<Sample> = scoped.load("a/b").unwrap();
        assert_eq!(got.value.title, "x");
        // And the resulting path must actually be inside the cache root.
        let entries: Vec<_> = walkdir(&dir).collect();
        for p in &entries {
            assert!(p.starts_with(&dir), "leaked outside cache root: {p:?}");
        }
    }

    fn walkdir(root: &Path) -> impl Iterator<Item = PathBuf> {
        let mut stack = vec![root.to_path_buf()];
        let mut out = Vec::new();
        while let Some(p) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                for e in rd.flatten() {
                    let ep = e.path();
                    if ep.is_dir() {
                        stack.push(ep.clone());
                    }
                    out.push(ep);
                }
            }
        }
        out.into_iter()
    }

    #[test]
    fn bytes_round_trip_persists_payload() {
        let cache = Cache::at(tmpdir());
        let scoped = cache.scoped("gallery", "main");
        let payload: &[u8] = b"\xff\xd8\xff\xe0not really a jpeg but bytes are bytes";
        scoped.store_bytes("thumb-abcd1234", payload).unwrap();
        let entry = scoped.load_bytes("thumb-abcd1234").unwrap();
        assert_eq!(entry.value, payload);
    }

    #[test]
    fn bytes_miss_returns_none() {
        let cache = Cache::at(tmpdir());
        let scoped = cache.scoped("gallery", "main");
        assert!(scoped.load_bytes("nope").is_none());
    }

    #[test]
    fn bytes_and_json_keys_coexist() {
        let cache = Cache::at(tmpdir());
        let scoped = cache.scoped("gallery", "main");
        scoped
            .store(
                "meta",
                &Sample {
                    title: "x".into(),
                    count: 1,
                },
            )
            .unwrap();
        scoped.store_bytes("meta", b"raw").unwrap();
        // Both round-trip independently.
        assert_eq!(scoped.load::<Sample>("meta").unwrap().value.title, "x");
        assert_eq!(scoped.load_bytes("meta").unwrap().value, b"raw");
    }

    #[test]
    fn invalidate_removes_both_variants() {
        let cache = Cache::at(tmpdir());
        let scoped = cache.scoped("gallery", "main");
        scoped
            .store(
                "meta",
                &Sample {
                    title: "x".into(),
                    count: 1,
                },
            )
            .unwrap();
        scoped.store_bytes("meta", b"raw").unwrap();
        scoped.invalidate("meta").unwrap();
        assert!(scoped.load::<Sample>("meta").is_none());
        assert!(scoped.load_bytes("meta").is_none());
    }

    #[test]
    fn is_within_compares_against_age() {
        let entry = CacheEntry {
            value: 42_u32,
            stored_at: Utc::now() - chrono::Duration::seconds(10),
        };
        assert!(entry.is_within(Duration::from_secs(30)));
        assert!(!entry.is_within(Duration::from_secs(5)));
    }

    /// Sanity: concurrent stores on the same key resolve last-write-wins
    /// without corruption (no partial file ever survives).
    #[test]
    fn concurrent_writes_dont_corrupt() {
        let dir = tmpdir();
        let cache = Cache::at(&dir);
        let scoped = cache.scoped("test", "main");
        let mut handles = Vec::new();
        for i in 0..16 {
            let s = scoped.clone();
            handles.push(thread::spawn(move || {
                s.store(
                    "k",
                    &Sample {
                        title: format!("v{i}"),
                        count: i,
                    },
                )
                .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Whatever value ended up there must at least be parsable.
        let got: Option<CacheEntry<Sample>> = scoped.load("k");
        assert!(got.is_some());
    }

    #[test]
    fn sweep_removes_files_older_than_max_age_and_keeps_fresh_ones() {
        use std::fs::File;
        use std::time::Duration;

        let dir = tmpdir();
        let cache = Cache::at(&dir);
        let scoped = cache.scoped("news", "main");

        // Two entries: one we'll backdate, one we leave fresh.
        scoped
            .store(
                "stale",
                &Sample {
                    title: "old".into(),
                    count: 1,
                },
            )
            .unwrap();
        scoped
            .store(
                "fresh",
                &Sample {
                    title: "new".into(),
                    count: 2,
                },
            )
            .unwrap();

        // Backdate the stale file by hand so we don't have to sleep.
        let stale_path = scoped.path_for("stale");
        let backdated = SystemTime::now() - Duration::from_secs(60 * 24 * 60 * 60);
        let f = File::options().write(true).open(&stale_path).unwrap();
        f.set_modified(backdated).unwrap();
        drop(f);

        let removed = cache.sweep_older_than(Duration::from_secs(30 * 24 * 60 * 60));
        assert_eq!(removed, 1, "expected exactly the stale file to go");
        assert!(!stale_path.exists(), "stale file should be removed");
        assert!(
            scoped.path_for("fresh").exists(),
            "fresh file should survive"
        );
    }

    #[test]
    fn sweep_is_a_noop_when_cache_root_is_missing() {
        let dir = tmpdir().join("never-created");
        let cache = Cache::at(&dir);
        assert_eq!(cache.sweep_older_than(Duration::from_secs(1)), 0);
    }
}
