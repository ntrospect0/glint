// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Opt-in, non-destructive migration of a pre-profiles flat config into
//! `profiles/default/`.
//!
//! Migration is deliberately **not automatic**. Automatic-on-launch migration
//! is unsafe when an older flat binary and the new profiles binary share one
//! config directory: the new binary relocates the flat config, the old one
//! then sees an "empty" flat dir and re-seeds defaults, and the real config is
//! lost. Instead:
//!
//! - a flat layout is read **in place** for the default profile until you opt
//!   in (see [`crate::config::config_dir`]), and
//! - `glint --migrate-profiles` **copies** the flat per-profile files into
//!   `profiles/default/` and **leaves the originals**, so an older flat binary
//!   keeps working. Remove the root `*.toml` yourself once you've fully
//!   switched.
//!
//! Copy → atomic publish (`rename` staging → `profiles/default/`). Nothing at
//! the root is ever deleted, so the operation is inherently safe to interrupt.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{glint_root, DEFAULT_PROFILE};

const STAGING_DIR: &str = ".default.partial";

/// Root-level files that belong to the GLOBAL layer and stay at the root.
fn is_global_root_file(name: &str) -> bool {
    name == "colorschemes.toml"
}

/// Credential files that belong to the GLOBAL layer (OAuth client
/// registrations); everything else under `credentials/` is per-profile.
fn is_global_client_file(name: &str) -> bool {
    name.ends_with("_oauth_client.toml")
}

/// Copy the flat layout into `profiles/default/`. Returns the published
/// directory and the number of top-level items copied. Non-destructive: the
/// flat originals are left in place.
pub fn migrate_to_profiles() -> Result<(PathBuf, usize)> {
    migrate_to_profiles_at(&glint_root()?)
}

/// Pure core, parameterized on the glint root for testability.
pub(crate) fn migrate_to_profiles_at(root: &Path) -> Result<(PathBuf, usize)> {
    if !root.join("config.toml").exists() {
        anyhow::bail!("no flat config.toml at {} to migrate", root.display());
    }
    let published = root.join("profiles").join(DEFAULT_PROFILE);
    if published.exists() {
        anyhow::bail!("{} already exists — nothing to migrate", published.display());
    }

    let staging = root.join("profiles").join(STAGING_DIR);
    if staging.exists() {
        // Staging only ever holds copies — safe to discard and redo.
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("failed to clear staging {}", staging.display()))?;
    }
    std::fs::create_dir_all(root.join("profiles"))
        .with_context(|| format!("failed to create {}", root.join("profiles").display()))?;

    let count = copy_flat_into(root, &staging)?;

    // Atomic publish: the single commit point.
    std::fs::rename(&staging, &published).with_context(|| {
        format!(
            "failed to publish {} -> {}",
            staging.display(),
            published.display()
        )
    })?;
    // Non-destructive by design: flat originals are LEFT in place.
    Ok((published, count))
}

/// Remove the per-profile flat files from the root, leaving the global-layer
/// files (colorschemes.toml, `*_oauth_client.toml`) and the `profiles/` tree.
/// Returns the number of top-level items removed. Idempotent.
///
/// **Only** call this after the flat config has been copied into
/// `profiles/default/` (they're the same files) and with the user's explicit
/// consent — it removes files an older flat binary would otherwise read.
pub fn remove_flat_originals() -> Result<usize> {
    remove_flat_originals_at(&glint_root()?)
}

pub(crate) fn remove_flat_originals_at(root: &Path) -> Result<usize> {
    let mut removed = 0usize;
    for entry in std::fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        // Keep the profiles tree and the global-layer root file.
        if name == "profiles" || is_global_root_file(&name_s) {
            continue;
        }
        let path = entry.path();
        if name_s == "credentials" {
            // Remove per-profile credential files; keep client registrations.
            if let Ok(rd) = std::fs::read_dir(&path) {
                for e in rd.flatten() {
                    if is_global_client_file(&e.file_name().to_string_lossy()) {
                        continue;
                    }
                    let p = e.path();
                    let ok = if p.is_dir() {
                        std::fs::remove_dir_all(&p)
                    } else {
                        std::fs::remove_file(&p)
                    };
                    if ok.is_ok() {
                        removed += 1;
                    }
                }
            }
        } else {
            let ok = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if ok.is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Copy the per-profile portion of a flat root into `staging`, leaving the
/// global-layer files (colorschemes.toml, `*_oauth_client.toml`) at the root.
/// Returns the number of top-level items copied.
fn copy_flat_into(root: &Path, staging: &Path) -> Result<usize> {
    std::fs::create_dir_all(staging)
        .with_context(|| format!("failed to create staging {}", staging.display()))?;
    let mut copied = 0usize;
    for entry in std::fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if name == "profiles" || is_global_root_file(&name_s) {
            continue;
        }
        let from = entry.path();
        let to = staging.join(&name);
        if name_s == "credentials" {
            copy_credentials_into(&from, &to)?;
        } else if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            copy_file(&from, &to)?;
        }
        copied += 1;
    }
    Ok(copied)
}

/// Copy `credentials/`, taking everything **except** the global client
/// registrations (which stay at the root). Keeps 0700 on the new dir.
fn copy_credentials_into(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("failed to create {}", dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o700));
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if is_global_client_file(&name.to_string_lossy()) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            copy_file(&from, &to)?;
        }
    }
    Ok(())
}

fn copy_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // std::fs::copy copies the permission bits on Unix, so 0600 tokens keep
    // their mode.
    std::fs::copy(from, to)
        .with_context(|| format!("failed to copy {} -> {}", from.display(), to.display()))?;
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    if let Ok(meta) = std::fs::metadata(src) {
        let _ = std::fs::set_permissions(dst, meta.permissions());
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            copy_file(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("glint-migrate-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn seed_flat(root: &Path) {
        fs::write(root.join("config.toml"), "version = 1\n").unwrap();
        fs::write(root.join("clock.toml"), "x = 1\n").unwrap();
        fs::write(root.join("colorschemes.toml"), "schemes\n").unwrap(); // GLOBAL
        fs::create_dir_all(root.join("credentials")).unwrap();
        fs::write(root.join("credentials/google_oauth_token.default.toml"), "tok\n").unwrap();
        fs::write(root.join("credentials/caldav.toml"), "dav\n").unwrap();
        fs::write(root.join("credentials/google_oauth_client.toml"), "client\n").unwrap(); // GLOBAL
    }

    #[test]
    fn copies_non_destructively_and_keeps_globals_at_root() {
        let root = temp_root("copy");
        seed_flat(&root);
        let (published, count) = migrate_to_profiles_at(&root).unwrap();
        assert!(count >= 2);

        // Per-profile files are copied down…
        assert!(published.join("config.toml").exists());
        assert!(published.join("clock.toml").exists());
        assert!(published.join("credentials/google_oauth_token.default.toml").exists());
        assert!(published.join("credentials/caldav.toml").exists());

        // …and the flat originals are STILL at the root (non-destructive).
        assert!(root.join("config.toml").exists(), "flat config kept");
        assert!(root.join("clock.toml").exists(), "flat widget kept");
        assert!(root.join("credentials/caldav.toml").exists(), "flat cred kept");

        // Globals stay at the root and are NOT copied into the profile.
        assert!(root.join("colorschemes.toml").exists());
        assert!(root.join("credentials/google_oauth_client.toml").exists());
        assert!(!published.join("colorschemes.toml").exists());
        assert!(!published.join("credentials/google_oauth_client.toml").exists());

        assert!(!root.join("profiles/.default.partial").exists(), "staging consumed");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_when_already_migrated() {
        let root = temp_root("already");
        seed_flat(&root);
        fs::create_dir_all(root.join("profiles/default")).unwrap();
        let err = migrate_to_profiles_at(&root).unwrap_err();
        assert!(err.to_string().contains("already exists"), "got: {err}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_without_flat_config() {
        let root = temp_root("noflat");
        let err = migrate_to_profiles_at(&root).unwrap_err();
        assert!(err.to_string().contains("no flat config"), "got: {err}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn remove_flat_originals_keeps_globals_and_profiles() {
        let root = temp_root("remove");
        seed_flat(&root);
        fs::create_dir_all(root.join("profiles/default")).unwrap();
        let removed = remove_flat_originals_at(&root).unwrap();
        assert!(removed >= 2, "removed {removed}");
        // Per-profile flat files gone.
        assert!(!root.join("config.toml").exists());
        assert!(!root.join("clock.toml").exists());
        assert!(!root.join("credentials/caldav.toml").exists());
        // Globals + the profiles tree kept.
        assert!(root.join("colorschemes.toml").exists());
        assert!(root.join("credentials/google_oauth_client.toml").exists());
        assert!(root.join("profiles/default").exists());
        fs::remove_dir_all(&root).ok();
    }
}
