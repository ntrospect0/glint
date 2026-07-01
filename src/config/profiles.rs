// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Profile discovery + name helpers.
//!
//! The mutating lifecycle ops (create / clone / rename / delete) land with
//! the setup-wizard Profile Manager; this module currently provides the
//! read-side primitives the CLI (`--list-profiles`) and those ops will share.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{
    active_profile, glint_root, seed_global_layer, seed_profile_dir, validate_profile_name,
    DEFAULT_PROFILE,
};

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
pub fn collides_ignore_case(existing: &[String], name: &str) -> bool {
    let target = name.to_ascii_lowercase();
    existing.iter().any(|n| n.to_ascii_lowercase() == target)
}

/// Create a new profile named `name`. With `from`, clones an existing
/// profile's **config only** (not its credentials — the clone re-authorizes;
/// cloned tokens would break on refresh-token rotation). Without `from`, seeds
/// defaults. Ensures the shared global layer exists either way. Rejects
/// invalid names and case-insensitive collisions.
pub fn create(name: &str, from: Option<&str>) -> Result<()> {
    validate_profile_name(name)?;
    let existing = list()?;
    if collides_ignore_case(&existing, name) {
        anyhow::bail!("a profile named like {name:?} already exists");
    }
    seed_global_layer()?;
    let dest = profiles_dir()?.join(name);
    match from {
        Some(src) => {
            if !existing.iter().any(|n| n == src) {
                anyhow::bail!("source profile {src:?} not found");
            }
            clone_config_only(&profiles_dir()?.join(src), &dest)?;
        }
        None => seed_profile_dir(&dest)?,
    }
    Ok(())
}

/// Rename a profile. The default profile cannot be renamed.
pub fn rename(old: &str, new: &str) -> Result<()> {
    if old == DEFAULT_PROFILE {
        anyhow::bail!("the default profile cannot be renamed");
    }
    validate_profile_name(new)?;
    let existing = list()?;
    if !existing.iter().any(|n| n == old) {
        anyhow::bail!("profile {old:?} not found");
    }
    if collides_ignore_case(&existing, new) {
        anyhow::bail!("a profile named like {new:?} already exists");
    }
    let base = profiles_dir()?;
    std::fs::rename(base.join(old), base.join(new))
        .with_context(|| format!("failed to rename profile {old:?} -> {new:?}"))?;
    Ok(())
}

/// Delete a profile and its cache segment. The default profile and the
/// currently-active profile cannot be deleted.
pub fn delete(name: &str) -> Result<()> {
    if name == DEFAULT_PROFILE {
        anyhow::bail!("the default profile cannot be deleted");
    }
    if name == active_profile() {
        anyhow::bail!("cannot delete the active profile {name:?}");
    }
    let dir = profiles_dir()?.join(name);
    if !dir.exists() {
        anyhow::bail!("profile {name:?} not found");
    }
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("failed to delete profile dir {}", dir.display()))?;
    let _ = crate::cache::remove_profile_cache(name);
    Ok(())
}

/// Copy a profile's config into a new profile dir, **excluding** its
/// top-level `credentials/` (the clone re-authorizes), then create a fresh
/// empty `credentials/` at 0700.
fn clone_config_only(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        anyhow::bail!("destination {} already exists", dest.display());
    }
    copy_tree_excluding(src, dest, "credentials")?;
    let creds = dest.join("credentials");
    std::fs::create_dir_all(&creds)
        .with_context(|| format!("failed to create {}", creds.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Recursively copy `src` → `dest`, skipping a top-level entry named
/// `exclude`. `std::fs::copy` preserves file modes on Unix.
fn copy_tree_excluding(src: &Path, dest: &Path, exclude: &str) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name() == exclude {
            continue;
        }
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
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

    #[test]
    fn clone_copies_config_but_not_credentials() {
        let base = temp_dir("clone");
        let src = base.join("src");
        fs::create_dir_all(src.join("credentials")).unwrap();
        fs::create_dir_all(src.join("notes/scratch")).unwrap();
        fs::write(src.join("config.toml"), "cfg").unwrap();
        fs::write(src.join("calendar.toml"), "cal").unwrap();
        fs::write(src.join("notes/scratch/1.md"), "note").unwrap();
        fs::write(src.join("credentials/token.toml"), "tok").unwrap();

        let dest = base.join("dest");
        clone_config_only(&src, &dest).unwrap();

        assert!(dest.join("config.toml").exists(), "config copied");
        assert!(dest.join("calendar.toml").exists(), "widget config copied");
        assert!(dest.join("notes/scratch/1.md").exists(), "notes copied recursively");
        assert!(dest.join("credentials").is_dir(), "empty creds dir created");
        assert!(
            !dest.join("credentials/token.toml").exists(),
            "credentials NOT cloned"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn default_profile_is_protected() {
        // These bail on the guard before touching the filesystem.
        assert!(rename("default", "anything").is_err());
        assert!(delete("default").is_err());
    }
}
