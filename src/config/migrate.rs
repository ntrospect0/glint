// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! One-time migration of the pre-profiles flat config layout into
//! `profiles/default/`.
//!
//! Pre-0.3.5 glint stored everything flat under `~/.config/glint/`. Profiles
//! moves per-profile state under `profiles/<name>/`, so an existing flat
//! install is migrated into `profiles/default/` on first launch.
//!
//! **Staged note:** this moves the *entire* flat tree into `profiles/default/`,
//! including the colorscheme library and OAuth client registrations. Hoisting
//! those shared resources back up to the global root (with the read-side
//! resolution) is a later phase; until then every file is per-profile, which
//! keeps this step self-consistent — all reads resolve under
//! `config_dir()` = `profiles/default/`.
//!
//! Crash-safety: **stage-and-publish**. Files move into
//! `profiles/.default.partial/`; a single atomic `rename` to
//! `profiles/default/` is the commit point, so there is no observable
//! half-migrated `profiles/default/`.

use std::path::Path;

use anyhow::{Context, Result};

use super::{glint_root, DEFAULT_PROFILE};

const STAGING_DIR: &str = ".default.partial";
const MIGRATED_MARKER: &str = ".profiles-migrated";

/// Migrate the flat layout under the glint root if one is present. No-op
/// when already migrated (or never flat).
pub fn migrate_if_needed() -> Result<()> {
    migrate_root(&glint_root()?)
}

/// Pure core, parameterized on the glint root for testability.
pub(crate) fn migrate_root(root: &Path) -> Result<()> {
    let flat_config = root.join("config.toml");
    // Trigger on a flat layout — a `config.toml` at the root. Deliberately
    // NOT "profiles/ is absent": a stray `profiles/` dir must never mask a
    // real flat config.
    if !flat_config.exists() {
        return Ok(());
    }

    let profiles = root.join("profiles");
    let published = profiles.join(DEFAULT_PROFILE);

    // Ambiguity guard: both a flat config AND an already-populated
    // profiles/default → refuse to pick one.
    if published.join("config.toml").exists() {
        anyhow::bail!(
            "ambiguous config: found both a flat {} and {}.\n\
             Refusing to auto-migrate — move or remove one, then relaunch.",
            flat_config.display(),
            published.join("config.toml").display()
        );
    }

    // Discard a stale staging dir from a prior interrupted run (never
    // published, so safe to drop and redo).
    let staging = profiles.join(STAGING_DIR);
    if staging.exists() {
        std::fs::remove_dir_all(&staging).with_context(|| {
            format!("failed to clear stale staging dir {}", staging.display())
        })?;
    }
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create staging dir {}", staging.display()))?;

    // Move every root entry into staging except the `profiles/` tree itself.
    eprintln!("glint: migrating flat config into profiles/{DEFAULT_PROFILE}/ …");
    let mut moved = 0usize;
    for entry in std::fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry?;
        if entry.file_name() == "profiles" {
            continue;
        }
        let from = entry.path();
        let to = staging.join(entry.file_name());
        std::fs::rename(&from, &to)
            .with_context(|| format!("failed to move {} -> {}", from.display(), to.display()))?;
        moved += 1;
    }

    // Atomic publish: the single commit point.
    std::fs::rename(&staging, &published).with_context(|| {
        format!(
            "failed to publish {} -> {}",
            staging.display(),
            published.display()
        )
    })?;

    let _ = std::fs::write(root.join(MIGRATED_MARKER), "1\n");
    eprintln!("glint: migrated {moved} item(s) into profiles/{DEFAULT_PROFILE}/.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    // A throwaway dir under the system temp, unique per test, no env mutation.
    fn temp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("glint-migrate-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn migrates_flat_tree_into_profiles_default() {
        let root = temp_root("basic");
        fs::write(root.join("config.toml"), "version = 1\n").unwrap();
        fs::write(root.join("clock.toml"), "x = 1\n").unwrap();
        fs::create_dir_all(root.join("credentials")).unwrap();
        fs::write(root.join("credentials/caldav.toml"), "creds\n").unwrap();

        migrate_root(&root).unwrap();

        let def = root.join("profiles/default");
        assert!(def.join("config.toml").exists(), "config.toml moved");
        assert!(def.join("clock.toml").exists(), "widget toml moved");
        assert!(
            def.join("credentials/caldav.toml").exists(),
            "credentials moved"
        );
        assert!(!root.join("config.toml").exists(), "flat config removed");
        assert!(root.join(MIGRATED_MARKER).exists(), "marker written");
        assert!(
            !root.join("profiles/.default.partial").exists(),
            "staging consumed"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn is_idempotent_and_noop_without_flat_config() {
        let root = temp_root("idem");
        fs::write(root.join("config.toml"), "version = 1\n").unwrap();
        migrate_root(&root).unwrap();
        // Second run: no flat config.toml at root anymore → no-op, no error.
        migrate_root(&root).unwrap();
        assert!(root.join("profiles/default/config.toml").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ambiguous_layout_is_refused() {
        let root = temp_root("ambig");
        fs::write(root.join("config.toml"), "flat\n").unwrap();
        fs::create_dir_all(root.join("profiles/default")).unwrap();
        fs::write(root.join("profiles/default/config.toml"), "already\n").unwrap();
        let err = migrate_root(&root).unwrap_err();
        assert!(
            err.to_string().contains("ambiguous"),
            "expected ambiguity error, got: {err}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stale_staging_is_discarded_and_redone() {
        let root = temp_root("stale");
        fs::write(root.join("config.toml"), "version = 1\n").unwrap();
        // Simulate a crashed prior run: leftover staging dir, never published.
        fs::create_dir_all(root.join("profiles/.default.partial/garbage")).unwrap();
        migrate_root(&root).unwrap();
        assert!(root.join("profiles/default/config.toml").exists());
        assert!(!root.join("profiles/.default.partial").exists());
        fs::remove_dir_all(&root).ok();
    }
}
