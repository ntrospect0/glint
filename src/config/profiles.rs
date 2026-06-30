// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Profile discovery + name helpers.
//!
//! The mutating lifecycle ops (create / clone / rename / delete) land with
//! the setup-wizard Profile Manager; this module currently provides the
//! read-side primitives the CLI (`--list-profiles`) and those ops will share.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{glint_root, DEFAULT_PROFILE};

/// The directory holding every profile: `<glint_root>/profiles/`.
pub fn profiles_dir() -> Result<PathBuf> {
    Ok(glint_root()?.join("profiles"))
}

/// Profile names present on disk — `default` first, then alphabetical.
/// Dot-prefixed entries (e.g. the `.default.partial` migration staging dir)
/// and non-directories are skipped.
pub fn list() -> Result<Vec<String>> {
    Ok(list_in(&profiles_dir()?))
}

/// Pure core, parameterized on the profiles dir for testability.
pub(crate) fn list_in(profiles_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(profiles_dir) {
        for entry in rd.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            names.push(name);
        }
    }
    // `default` sorts first (false < true), the rest alphabetically.
    names.sort_by(|a, b| (a != DEFAULT_PROFILE, a).cmp(&(b != DEFAULT_PROFILE, b)));
    names
}

/// Case-insensitive collision check against an existing name list. macOS
/// APFS/HFS+ fold case, so `Work` and `work` are the same directory — a
/// create/rename whose lowercased form matches an existing profile must be
/// rejected rather than silently merged.
#[allow(dead_code)] // used by the Profile Manager's create/rename guard (phase 4).
pub fn collides_ignore_case(existing: &[String], name: &str) -> bool {
    let target = name.to_ascii_lowercase();
    existing.iter().any(|n| n.to_ascii_lowercase() == target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("glint-profiles-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn list_puts_default_first_and_skips_hidden_and_files() {
        let dir = temp_dir("list");
        for d in ["work", "default", "alpha"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        fs::create_dir_all(dir.join(".default.partial")).unwrap(); // staging
        fs::write(dir.join("stray.txt"), "x").unwrap(); // not a dir
        assert_eq!(list_in(&dir), vec!["default", "alpha", "work"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn collision_is_case_insensitive() {
        let existing = vec!["work".to_string(), "default".to_string()];
        assert!(collides_ignore_case(&existing, "Work"));
        assert!(collides_ignore_case(&existing, "WORK"));
        assert!(!collides_ignore_case(&existing, "travel"));
    }
}
