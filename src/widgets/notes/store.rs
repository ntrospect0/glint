// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Per-instance note persistence.
//!
//! The directory layout is `<root>/<instance>/<id>.md`, where `root`
//! is whatever [`resolve_root`] decided on at widget mount: the user's
//! configured `notes_dir` if it could be created, falling back to
//! `~/.glint/notes`, then to the legacy `~/.config/glint/notes`.
//!
//! One Markdown file per note. The on-disk layout is intentionally plain:
//! users can `cat` a note, hand-edit it, back the directory up with git,
//! or move notes between machines by copying files. Atomic writes via
//! temp + rename keep partial writes from corrupting an existing note.
//!
//! Filename = `<id>.md`, where `id` is a lexicographically-sortable
//! timestamp-with-counter so listings sort newest-first naturally. The
//! filename is purely an internal handle; the visible note name comes
//! from the first line of the body.
//!
//! Last-edited time is `fs::Metadata::modified()` — we don't carry our
//! own header, which means users who hand-edit the file get a free
//! "last edited" update via the filesystem.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use anyhow::{Context, Result};

/// A single note as held in memory. Bodies are loaded eagerly
/// (notes are tiny and few — typically dozens of KB total per instance).
#[derive(Debug, Clone)]
pub struct Note {
    pub id: String,
    pub body: String,
    /// `fs::Metadata::modified()` from disk — drives the list sort order.
    pub modified: SystemTime,
}

impl Note {
    /// Display name = first line, trimmed. Empty body → "(empty)".
    pub fn display_name(&self) -> &str {
        let line = self.body.lines().next().unwrap_or("").trim();
        if line.is_empty() {
            "(empty)"
        } else {
            line
        }
    }
}

/// Process-local sequence counter so two notes created in the same
/// millisecond still get distinct IDs.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Build a lexicographically-sortable ID: 20-digit zero-padded
/// nanoseconds-since-epoch + a 6-digit zero-padded process-local
/// sequence. Sorts the same as creation order without a centralized
/// clock authority.
pub fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:020}-{seq:06}")
}

/// Which tier of [`resolve_root`]'s fallback chain produced the path.
/// Carried out so the widget can craft a user-visible toast when
/// anything other than the happy path took effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The configured `notes_dir` succeeded (or no override was given
    /// and the built-in `~/.glint/notes` default succeeded — these
    /// share a tier from the widget's perspective).
    Configured,
    /// The configured `notes_dir` was set but couldn't be created;
    /// `~/.glint/notes` worked as a fallback. Carries the rejected
    /// path so the toast can quote it.
    FellBackToDefault { rejected: PathBuf },
    /// Neither the configured override nor `~/.glint/notes` could be
    /// created; we landed on the legacy `~/.config/glint/notes`.
    /// Carries every path that was tried so the toast / log can
    /// surface the full story.
    FellBackToLegacy { rejected: Vec<PathBuf> },
}

/// Resolve the root directory for notes storage and ensure it exists.
///
/// Tries the user's configured override first (if non-empty), then
/// `~/.glint/notes`, then the legacy `~/.config/glint/notes`. Each
/// candidate is `mkdir -p`'d; the first one that survives that step
/// wins. A leading `~/` in the configured path is expanded against
/// `$HOME`.
///
/// Returns an `Err` only if all three candidates fail — which in
/// practice means the home directory is unwritable, a situation glint
/// can't gracefully recover from anyway.
pub fn resolve_root(configured: Option<&str>) -> Result<(PathBuf, Resolution)> {
    let mut rejected: Vec<PathBuf> = Vec::new();

    // ── Tier 1: configured override (if non-empty after trimming). ──
    let configured_path = configured
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(expand_tilde);
    if let Some(path) = configured_path.as_ref() {
        if try_mkdir_p(path) {
            return Ok((path.clone(), Resolution::Configured));
        }
        rejected.push(path.clone());
    }

    // ── Tier 2: ~/.glint/notes. ──
    let glint_home = home_join(".glint").map(|p| p.join("notes"));
    if let Some(path) = glint_home.as_ref() {
        if try_mkdir_p(path) {
            // If the user didn't configure anything, this is the
            // happy path — no fallback occurred.
            return Ok((
                path.clone(),
                if rejected.is_empty() {
                    Resolution::Configured
                } else {
                    Resolution::FellBackToDefault {
                        rejected: rejected.remove(0),
                    }
                },
            ));
        }
        rejected.push(path.clone());
    }

    // ── Tier 3: ~/.config/glint/notes (XDG-aware). ──
    let legacy = crate::config::config_dir().map(|p| p.join("notes"));
    if let Ok(path) = legacy {
        if try_mkdir_p(&path) {
            return Ok((path, Resolution::FellBackToLegacy { rejected }));
        }
        rejected.push(path);
    }

    anyhow::bail!(
        "could not create any notes directory; tried {}",
        rejected
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn try_mkdir_p(path: &Path) -> bool {
    match fs::create_dir_all(path) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "notes: mkdir failed; trying next fallback"
            );
            false
        }
    }
}

/// Expand a leading `~/` (or bare `~`) against `$HOME`. Anything else
/// is returned unchanged. We only handle the common leading-tilde case
/// — `~user/...` is intentionally not supported.
fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn home_join(suffix: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(suffix))
}

/// Resolve the on-disk directory for one instance's notes inside an
/// already-resolved root. Created lazily on first write.
pub fn notes_dir(root: &Path, instance: &str) -> PathBuf {
    root.join(sanitize_instance(instance))
}

fn sanitize_instance(instance: &str) -> String {
    // Defense in depth against `..` and path separators. Matches the
    // ScopedCache sanitiser's intent.
    instance
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// Load every `<id>.md` from the instance's notes directory. Missing
/// directory → empty vec (no error). Per-file read failures are logged
/// and skipped so one corrupt note doesn't hide the rest.
pub fn load_all(root: &Path, instance: &str) -> Vec<Note> {
    let dir = notes_dir(root, instance);
    if !dir.exists() {
        return Vec::new();
    }
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(path = %dir.display(), error = %err, "notes: read_dir failed");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let body = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "notes: read failed; skipping"
                );
                continue;
            }
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(Note { id, body, modified });
    }
    // Newest first.
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// Atomic write of one note. Creates the instance directory on demand.
/// Updates the in-memory note's `modified` to the new mtime so callers
/// can re-sort without an extra stat.
pub fn save(root: &Path, instance: &str, note: &mut Note) -> Result<()> {
    let dir = notes_dir(root, instance);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.md", note.id));
    let tmp = dir.join(format!("{}.md.tmp", note.id));
    fs::write(&tmp, &note.body).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    note.modified = fs::metadata(&path)
        .and_then(|m| m.modified())
        .unwrap_or_else(|_| SystemTime::now());
    Ok(())
}

/// Remove a note from disk. Returns `Ok(())` if the file is already gone.
pub fn delete(root: &Path, instance: &str, id: &str) -> Result<()> {
    let dir = notes_dir(root, instance);
    let path = dir.join(format!("{id}.md"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home() -> tempdir::PathGuard {
        let dir = std::env::temp_dir().join(format!(
            "glint-notes-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        tempdir::PathGuard(dir)
    }

    // Tiny RAII helper so each test cleans up after itself even on
    // panic. Standalone to avoid pulling in a `tempfile` crate dep just
    // for these notes-store tests.
    mod tempdir {
        use std::path::PathBuf;
        pub struct PathGuard(pub PathBuf);
        impl AsRef<std::path::Path> for PathGuard {
            fn as_ref(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for PathGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn new_id_is_monotonic_within_process() {
        let a = new_id();
        let b = new_id();
        assert!(b > a, "{b} should sort after {a}");
    }

    #[test]
    fn display_name_uses_first_line_or_empty_marker() {
        let n = Note {
            id: "x".into(),
            body: "hello world\nsecond line".into(),
            modified: SystemTime::now(),
        };
        assert_eq!(n.display_name(), "hello world");

        let n = Note {
            id: "x".into(),
            body: String::new(),
            modified: SystemTime::now(),
        };
        assert_eq!(n.display_name(), "(empty)");

        let n = Note {
            id: "x".into(),
            body: "   \n".into(),
            modified: SystemTime::now(),
        };
        assert_eq!(n.display_name(), "(empty)");
    }

    fn unique_instance(label: &str) -> String {
        format!(
            "{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )
        .replace(
            |c: char| !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-'),
            "_",
        )
    }

    #[test]
    fn save_then_load_round_trips_body_and_modified() {
        let g = tmp_home();
        let inst = unique_instance("roundtrip");
        let mut n = Note {
            id: new_id(),
            body: "alpha\nbeta".into(),
            modified: SystemTime::UNIX_EPOCH,
        };
        save(g.as_ref(), &inst, &mut n).expect("save");
        assert!(n.modified > SystemTime::UNIX_EPOCH, "save must stamp mtime");

        let loaded = load_all(g.as_ref(), &inst);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body, "alpha\nbeta");
        assert_eq!(loaded[0].id, n.id);
    }

    #[test]
    fn load_all_sorts_newest_first() {
        let g = tmp_home();
        let inst = unique_instance("sort");
        let mut older = Note {
            id: new_id(),
            body: "old".into(),
            modified: SystemTime::UNIX_EPOCH,
        };
        save(g.as_ref(), &inst, &mut older).expect("save older");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut newer = Note {
            id: new_id(),
            body: "new".into(),
            modified: SystemTime::UNIX_EPOCH,
        };
        save(g.as_ref(), &inst, &mut newer).expect("save newer");

        let loaded = load_all(g.as_ref(), &inst);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].body, "new", "newest first");
        assert_eq!(loaded[1].body, "old");
    }

    #[test]
    fn delete_removes_the_file_and_is_idempotent() {
        let g = tmp_home();
        let inst = unique_instance("delete");
        let mut n = Note {
            id: new_id(),
            body: "doomed".into(),
            modified: SystemTime::now(),
        };
        save(g.as_ref(), &inst, &mut n).expect("save");
        delete(g.as_ref(), &inst, &n.id).expect("delete");
        assert!(load_all(g.as_ref(), &inst).is_empty());
        delete(g.as_ref(), &inst, &n.id).expect("idempotent delete");
    }

    #[test]
    fn sanitize_instance_strips_path_traversal() {
        assert_eq!(sanitize_instance("work"), "work");
        assert_eq!(sanitize_instance("../escape"), ".._escape");
        assert_eq!(sanitize_instance("a/b"), "a_b");
        assert_eq!(sanitize_instance("..\\evil"), ".._evil");
    }

    #[test]
    fn resolve_root_uses_configured_when_creatable() {
        let g = tmp_home();
        let target = g.0.join("custom-notes");
        let configured = target.to_string_lossy().to_string();
        let (root, res) = resolve_root(Some(&configured)).expect("resolve");
        assert_eq!(root, target);
        assert!(matches!(res, Resolution::Configured));
        assert!(target.exists(), "mkdir -p should have run");
    }

    #[test]
    fn resolve_root_falls_back_when_configured_is_unwritable() {
        // A path under an existing regular file can't be `mkdir -p`'d —
        // exercises the fallback without needing root or chmod.
        let g = tmp_home();
        let blocker = g.0.join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();
        let bad = blocker.join("sub").to_string_lossy().to_string();
        let (root, res) = resolve_root(Some(&bad)).expect("resolve");
        // Either tier 2 (~/.glint/notes) or tier 3 (~/.config/glint/notes)
        // should have caught it — both signal a fallback.
        assert_ne!(root, PathBuf::from(&bad));
        assert!(
            matches!(
                res,
                Resolution::FellBackToDefault { .. } | Resolution::FellBackToLegacy { .. }
            ),
            "expected fallback resolution, got {res:?}"
        );
    }

    #[test]
    fn expand_tilde_handles_common_forms() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));
        // Non-tilde paths pass through unchanged.
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde("relative/path"), PathBuf::from("relative/path"));
        // Bare ~name (no slash) is not expanded.
        assert_eq!(expand_tilde("~alice/foo"), PathBuf::from("~alice/foo"));
    }
}
