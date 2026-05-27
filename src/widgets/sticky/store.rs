//! Per-instance note persistence under `~/.config/glint/notes/<instance>/`.
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
    fs,
    io,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use anyhow::{Context, Result};

/// A single sticky note as held in memory. Bodies are loaded eagerly
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

/// Resolve the on-disk directory for one instance's notes. Created
/// lazily on first write.
pub fn notes_dir(instance: &str) -> Result<PathBuf> {
    let root = crate::config::config_dir()?.join("notes");
    let safe_instance = sanitize_instance(instance);
    Ok(root.join(safe_instance))
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
pub fn load_all(instance: &str) -> Vec<Note> {
    let dir = match notes_dir(instance) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "sticky: notes dir unresolvable");
            return Vec::new();
        }
    };
    if !dir.exists() {
        return Vec::new();
    }
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(path = %dir.display(), error = %err, "sticky: read_dir failed");
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
                    "sticky: read failed; skipping"
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
pub fn save(instance: &str, note: &mut Note) -> Result<()> {
    let dir = notes_dir(instance)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.md", note.id));
    let tmp = dir.join(format!("{}.md.tmp", note.id));
    fs::write(&tmp, &note.body)
        .with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    note.modified = fs::metadata(&path)
        .and_then(|m| m.modified())
        .unwrap_or_else(|_| SystemTime::now());
    Ok(())
}

/// Remove a note from disk. Returns `Ok(())` if the file is already gone.
pub fn delete(instance: &str, id: &str) -> Result<()> {
    let dir = notes_dir(instance)?;
    let path = dir.join(format!("{id}.md"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

#[allow(dead_code)] // surfaced when the wizard adds a "notes dir" hint.
pub fn root_dir() -> Result<PathBuf> {
    Ok(crate::config::config_dir()?.join("notes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home() -> tempdir::PathGuard {
        let dir = std::env::temp_dir().join(format!(
            "glint-sticky-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        tempdir::PathGuard(dir)
    }

    // Tiny RAII helper so each test cleans up after itself even on
    // panic. Standalone to avoid pulling in a `tempfile` crate dep just
    // for these notes-store tests.
    mod tempdir {
        use std::path::PathBuf;
        pub struct PathGuard(pub PathBuf);
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

    /// Parallel tests share XDG_CONFIG_HOME via the process env, so we
    /// also need unique instance names to isolate their notes dirs.
    fn unique_instance(label: &str) -> String {
        format!(
            "{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )
        // Sanitize for our own rules so the assertion-target path
        // matches what notes_dir() resolves to.
        .replace(|c: char| !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-'), "_")
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn save_then_load_round_trips_body_and_modified() {
        let _g = tmp_home();
        let inst = unique_instance("roundtrip");
        let mut n = Note {
            id: new_id(),
            body: "alpha\nbeta".into(),
            modified: SystemTime::UNIX_EPOCH,
        };
        save(&inst, &mut n).expect("save");
        assert!(n.modified > SystemTime::UNIX_EPOCH, "save must stamp mtime");

        let loaded = load_all(&inst);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].body, "alpha\nbeta");
        assert_eq!(loaded[0].id, n.id);
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn load_all_sorts_newest_first() {
        let _g = tmp_home();
        let inst = unique_instance("sort");
        let mut older = Note {
            id: new_id(),
            body: "old".into(),
            modified: SystemTime::UNIX_EPOCH,
        };
        save(&inst, &mut older).expect("save older");
        // Bump filesystem mtime resolution by a deliberate gap so the
        // second save sorts strictly newer.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut newer = Note {
            id: new_id(),
            body: "new".into(),
            modified: SystemTime::UNIX_EPOCH,
        };
        save(&inst, &mut newer).expect("save newer");

        let loaded = load_all(&inst);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].body, "new", "newest first");
        assert_eq!(loaded[1].body, "old");
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn delete_removes_the_file_and_is_idempotent() {
        let _g = tmp_home();
        let inst = unique_instance("delete");
        let mut n = Note {
            id: new_id(),
            body: "doomed".into(),
            modified: SystemTime::now(),
        };
        save(&inst, &mut n).expect("save");
        delete(&inst, &n.id).expect("delete");
        assert!(load_all(&inst).is_empty());
        // Deleting again must not error.
        delete(&inst, &n.id).expect("idempotent delete");
    }

    #[test]
    fn sanitize_instance_strips_path_traversal() {
        assert_eq!(sanitize_instance("work"), "work");
        // `.` is allowed (legitimate in `foo.bar`); `/` becomes `_`.
        // ".." stays as ".." since dots are allowed, but the path
        // separator is the actual escape vector and gets sanitized.
        assert_eq!(sanitize_instance("../escape"), ".._escape");
        assert_eq!(sanitize_instance("a/b"), "a_b");
        assert_eq!(sanitize_instance("..\\evil"), ".._evil");
    }
}
