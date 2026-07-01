// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! One-time migration of the pre-profiles flat config layout into the
//! two-tier profiles layout.
//!
//! Pre-0.3.5 glint stored everything flat under `~/.config/glint/`. Profiles
//! splits that into a **global layer** at the root (the colorscheme library +
//! OAuth client registrations) and a **per-profile layer** under
//! `profiles/<name>/`. An existing flat install is migrated into
//! `profiles/default/` on first launch, with the global files left at the
//! root.
//!
//! ## Crash-safety: copy, publish, then clean up
//!
//! The migration **copies** per-profile files into a staging dir, atomically
//! renames staging → `profiles/default/` (the single commit point), writes a
//! `.profiles-migrated` marker, and only *then* removes the flat originals.
//! Because originals are never destroyed until the published copy exists, a
//! crash at any point is safe: the flat layout is still intact and the next
//! run redoes the copy. A `.migrating` intent marker distinguishes a
//! resumable in-progress migration from a genuinely ambiguous layout.

use std::path::Path;

use anyhow::{Context, Result};

use super::{glint_root, DEFAULT_PROFILE};

const STAGING_DIR: &str = ".default.partial";
const MIGRATED_MARKER: &str = ".profiles-migrated";
const MIGRATING_MARKER: &str = ".migrating";

/// Root-level files that belong to the GLOBAL layer and stay at the root.
fn is_global_root_file(name: &str) -> bool {
    name == "colorschemes.toml"
}

/// Credential files that belong to the GLOBAL layer (OAuth client
/// registrations); everything else under `credentials/` is per-profile.
fn is_global_client_file(name: &str) -> bool {
    name.ends_with("_oauth_client.toml")
}

/// Run the flat→profiles migration and the interim-layout hoist if needed.
pub fn migrate_if_needed() -> Result<()> {
    let root = glint_root()?;
    flat_to_profiles(&root)?;
    hoist_globals(&root)?;
    Ok(())
}

/// Copy-based, crash-safe migration of a flat layout into `profiles/default/`,
/// leaving the global-layer files at the root. Pure core, parameterized on
/// the glint root for testability.
pub(crate) fn flat_to_profiles(root: &Path) -> Result<()> {
    let flat_config = root.join("config.toml");
    let marker = root.join(MIGRATED_MARKER);
    let migrating = root.join(MIGRATING_MARKER);
    let profiles = root.join("profiles");
    let published = profiles.join(DEFAULT_PROFILE);
    let staging = profiles.join(STAGING_DIR);

    let resuming = migrating.exists();
    if !resuming {
        // Fully migrated already, or nothing flat to migrate.
        if marker.exists() || !flat_config.exists() {
            return Ok(());
        }
        // Ambiguity: a populated profiles/default we didn't create, sitting
        // next to a flat config → refuse to guess.
        if published.join("config.toml").exists() {
            anyhow::bail!(
                "ambiguous config: found both a flat {} and {}.\n\
                 Refusing to auto-migrate — move or remove one, then relaunch.",
                flat_config.display(),
                published.join("config.toml").display()
            );
        }
    }

    // Record intent so a crash mid-migration is recognised as resumable
    // (not ambiguous) on the next run.
    std::fs::create_dir_all(&profiles)
        .with_context(|| format!("failed to create {}", profiles.display()))?;
    let _ = std::fs::write(&migrating, "1\n");
    eprintln!("glint: migrating flat config into profiles/{DEFAULT_PROFILE}/ …");

    // Build + publish the profile if it isn't there yet. Copy (never move):
    // the flat originals stay intact until the published copy exists.
    if !published.exists() {
        if staging.exists() {
            // Staging only ever holds copies — safe to discard and redo.
            std::fs::remove_dir_all(&staging)
                .with_context(|| format!("failed to clear staging {}", staging.display()))?;
        }
        copy_flat_into(root, &staging)?;
        std::fs::rename(&staging, &published).with_context(|| {
            format!(
                "failed to publish {} -> {}",
                staging.display(),
                published.display()
            )
        })?;
    }

    // Published copy exists → finalize: mark done, remove the flat originals
    // (globals excepted), clear the in-progress marker.
    let _ = std::fs::write(&marker, "1\n");
    cleanup_flat_originals(root)?;
    let _ = std::fs::remove_file(&migrating);
    eprintln!("glint: migrated flat config into profiles/{DEFAULT_PROFILE}/.");
    Ok(())
}

/// Pull global-layer files that an interim (move-based) build left inside
/// `profiles/default/` back up to the root. Best-effort and idempotent.
pub(crate) fn hoist_globals(root: &Path) -> Result<()> {
    let published = root.join("profiles").join(DEFAULT_PROFILE);
    if !published.exists() {
        return Ok(());
    }
    // colorschemes.toml → root (don't clobber an existing root copy).
    let src = published.join("colorschemes.toml");
    let dst = root.join("colorschemes.toml");
    if src.exists() && !dst.exists() {
        let _ = std::fs::rename(&src, &dst);
    }
    // OAuth client registrations → root/credentials/ (the global creds dir).
    let src_creds = published.join("credentials");
    if let Ok(rd) = std::fs::read_dir(&src_creds) {
        let dst_creds = root.join("credentials");
        for e in rd.flatten() {
            let name = e.file_name();
            if !is_global_client_file(&name.to_string_lossy()) {
                continue;
            }
            let _ = std::fs::create_dir_all(&dst_creds);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &dst_creds,
                    std::fs::Permissions::from_mode(0o700),
                );
            }
            let dst_file = dst_creds.join(&name);
            if !dst_file.exists() {
                let _ = std::fs::rename(e.path(), dst_file);
            }
        }
    }
    Ok(())
}

/// Copy the per-profile portion of a flat root into `staging`, leaving the
/// global-layer files (colorschemes.toml, `*_oauth_client.toml`) untouched.
fn copy_flat_into(root: &Path, staging: &Path) -> Result<()> {
    std::fs::create_dir_all(staging)
        .with_context(|| format!("failed to create staging {}", staging.display()))?;
    for entry in std::fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if name == "profiles"
            || name_s == MIGRATED_MARKER
            || name_s == MIGRATING_MARKER
            || is_global_root_file(&name_s)
        {
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
    }
    Ok(())
}

/// Copy `credentials/`, moving everything **except** the global client
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

/// Remove the flat originals from the root after a successful publish,
/// leaving the global-layer files in place. Idempotent.
fn cleanup_flat_originals(root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if name == "profiles"
            || name_s == MIGRATED_MARKER
            || name_s == MIGRATING_MARKER
            || is_global_root_file(&name_s)
        {
            continue;
        }
        let path = entry.path();
        if name_s == "credentials" {
            // Remove the per-profile credential files; keep client regs + dir.
            if let Ok(rd) = std::fs::read_dir(&path) {
                for e in rd.flatten() {
                    if is_global_client_file(&e.file_name().to_string_lossy()) {
                        continue;
                    }
                    let p = e.path();
                    let _ = if p.is_dir() {
                        std::fs::remove_dir_all(&p)
                    } else {
                        std::fs::remove_file(&p)
                    };
                }
            }
        } else if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
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
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("glint-migrate-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    // A flat root: per-profile files + global files.
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
    fn partitions_flat_tree_globals_stay_at_root() {
        let root = temp_root("partition");
        seed_flat(&root);
        flat_to_profiles(&root).unwrap();

        let def = root.join("profiles/default");
        // Per-profile files copied down…
        assert!(def.join("config.toml").exists());
        assert!(def.join("clock.toml").exists());
        assert!(def.join("credentials/google_oauth_token.default.toml").exists());
        assert!(def.join("credentials/caldav.toml").exists());
        // …and removed from the root.
        assert!(!root.join("config.toml").exists());
        assert!(!root.join("clock.toml").exists());
        assert!(!root.join("credentials/caldav.toml").exists());

        // Globals STAY at the root, and are NOT copied into the profile.
        assert!(root.join("colorschemes.toml").exists());
        assert!(root.join("credentials/google_oauth_client.toml").exists());
        assert!(!def.join("colorschemes.toml").exists());
        assert!(!def.join("credentials/google_oauth_client.toml").exists());

        assert!(root.join(MIGRATED_MARKER).exists());
        assert!(!root.join(MIGRATING_MARKER).exists());
        assert!(!root.join("profiles/.default.partial").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn is_idempotent() {
        let root = temp_root("idem");
        seed_flat(&root);
        flat_to_profiles(&root).unwrap();
        flat_to_profiles(&root).unwrap(); // second run: marker present → no-op
        assert!(root.join("profiles/default/config.toml").exists());
        assert!(root.join("colorschemes.toml").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ambiguous_layout_is_refused() {
        let root = temp_root("ambig");
        fs::write(root.join("config.toml"), "flat\n").unwrap();
        fs::create_dir_all(root.join("profiles/default")).unwrap();
        fs::write(root.join("profiles/default/config.toml"), "already\n").unwrap();
        let err = flat_to_profiles(&root).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "got: {err}");
        // The flat config is untouched (copy-based never destroys it).
        assert_eq!(fs::read_to_string(root.join("config.toml")).unwrap(), "flat\n");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn resumes_after_publish_without_marker() {
        // Simulate a crash after publish but before the marker/cleanup: the
        // profile is published, the .migrating marker is present, originals
        // still at root. A re-run must finalize, not bail as ambiguous.
        let root = temp_root("resume");
        seed_flat(&root);
        fs::create_dir_all(root.join("profiles/default")).unwrap();
        fs::write(root.join("profiles/default/config.toml"), "version = 1\n").unwrap();
        fs::write(root.join(MIGRATING_MARKER), "1\n").unwrap();

        flat_to_profiles(&root).unwrap();
        assert!(root.join(MIGRATED_MARKER).exists());
        assert!(!root.join(MIGRATING_MARKER).exists());
        assert!(!root.join("config.toml").exists(), "originals cleaned up");
        assert!(root.join("colorschemes.toml").exists(), "global kept");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hoist_pulls_interim_globals_back_to_root() {
        // A tree an interim move-based build produced: globals inside the
        // profile. hoist_globals moves them back to the root.
        let root = temp_root("hoist");
        let def = root.join("profiles/default");
        fs::create_dir_all(def.join("credentials")).unwrap();
        fs::write(def.join("colorschemes.toml"), "schemes\n").unwrap();
        fs::write(def.join("credentials/microsoft_oauth_client.toml"), "client\n").unwrap();
        fs::write(def.join("credentials/microsoft_oauth_token.default.toml"), "tok\n").unwrap();

        hoist_globals(&root).unwrap();
        assert!(root.join("colorschemes.toml").exists(), "colorschemes hoisted");
        assert!(!def.join("colorschemes.toml").exists());
        assert!(
            root.join("credentials/microsoft_oauth_client.toml").exists(),
            "client reg hoisted"
        );
        // The token stays in the profile.
        assert!(def.join("credentials/microsoft_oauth_token.default.toml").exists());
        fs::remove_dir_all(&root).ok();
    }
}
