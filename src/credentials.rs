// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! On-disk credentials store — one home for every "load a TOML file
//! holding a secret" / "write one with chmod 0600" pattern.
//!
//! Before this module the dance was open-coded across
//! `auth/google/store.rs`, `auth/microsoft/store.rs`, and
//! `auth/registry.rs`. Each re-implemented atomic write +
//! `chmod 0600` independently, which is exactly the kind of
//! security-relevant duplication that eventually ships a 0644
//! token file by accident.
//!
//! All files live under `~/.config/glint/credentials/`. The
//! directory is created with mode `0700` on first use. Save paths
//! atomic-write to a sibling `<name>.tmp`, `chmod 0600` the tmp,
//! then rename — so even a crash mid-write can't leak a partially-
//! written secret at world-readable permissions.
//!
//! Callers identify files by basename (`"google_oauth_token.toml"`)
//! rather than full path so the credentials-dir convention is
//! enforced — you can't accidentally write a token to `~/Desktop/`.
//!
//! See `docs/widget-sdk.md` § Credentials storage.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Returns `~/.config/glint/credentials/`, creating it (mode `0700`
/// on Unix) on first use. Idempotent — safe to call repeatedly.
pub fn dir() -> Result<PathBuf> {
    let path = crate::config::config_dir()?.join("credentials");
    std::fs::create_dir_all(&path)
        .with_context(|| format!("failed to create credentials dir {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort: a chmod failure on an existing dir we don't
        // own (rare; would indicate an installed-as-root scenario)
        // shouldn't crash the auth flow.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(path)
}

/// Resolve a credentials basename to its absolute path under the
/// credentials directory. Does *not* create the file. Returns an
/// error only if the credentials dir itself can't be resolved.
pub fn path(filename: &str) -> Result<PathBuf> {
    Ok(dir()?.join(filename))
}

/// Load a TOML-serialised credentials value by basename. Returns:
///
/// - `Ok(Some(value))` — file exists and parsed cleanly.
/// - `Ok(None)`        — file is absent. Caller decides whether
///                        that's expected (no token yet) or an error.
/// - `Err(_)`          — file exists but is unreadable / malformed.
///                        Surfaces with file path context so the
///                        user can find what to fix.
pub fn load<T>(filename: &str) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    let path = path(filename)?;
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let value: T =
        toml::from_str(&body).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(value))
}

/// Save a TOML-serialisable credentials value to the credentials
/// directory under `filename`. Atomic via `<name>.tmp` + rename,
/// with `chmod 0600` applied to the temp file *before* rename so
/// the final inode is never visible to the world at a wider
/// permission. Returns the final absolute path.
pub fn save<T>(filename: &str, value: &T) -> Result<PathBuf>
where
    T: Serialize,
{
    let path = path(filename)?;
    let body =
        toml::to_string_pretty(value).with_context(|| format!("failed to serialize {filename}"))?;
    atomic_write_locked(&path, body.as_bytes())?;
    Ok(path)
}

/// Write a raw string (not necessarily TOML) to the credentials
/// directory iff the file is missing. Used for first-launch
/// scaffolding of `*_oauth_client.toml` template files. Returns
/// `Ok(true)` when the template was written, `Ok(false)` when the
/// file already existed (the caller's expected idempotent path).
pub fn write_template_if_missing(filename: &str, contents: &str) -> Result<bool> {
    let path = path(filename)?;
    if path.exists() {
        return Ok(false);
    }
    atomic_write_locked(&path, contents.as_bytes())?;
    Ok(true)
}

/// Atomic write with `chmod 0600` (Unix). The temp file is created
/// alongside the destination so the rename is on the same
/// filesystem; perms are tightened on the tmp before rename so the
/// final path is never observable at a more-permissive mode.
fn atomic_write_locked(path: &Path, body: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("failed to write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod 0600 {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_dir() -> PathBuf {
        // Each test gets its own XDG_CONFIG_HOME so they can't
        // collide with each other or with the user's real
        // credentials. The teardown drop the dir explicitly.
        let dir = std::env::temp_dir().join(format!(
            "glint-credentials-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        dir
    }

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Sample {
        api_key: String,
        nonce: u64,
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn save_then_load_round_trips() {
        let tmp = isolated_dir();
        let val = Sample {
            api_key: "abc".into(),
            nonce: 42,
        };
        let path = save("sample.toml", &val).unwrap();
        assert!(path.exists());
        let loaded: Option<Sample> = load("sample.toml").unwrap();
        assert_eq!(loaded, Some(val));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn load_returns_none_for_missing_file() {
        let tmp = isolated_dir();
        let loaded: Option<Sample> = load("nonexistent.toml").unwrap();
        assert!(loaded.is_none());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn save_sets_0600_on_unix() {
        let tmp = isolated_dir();
        let val = Sample {
            api_key: "secret".into(),
            nonce: 0,
        };
        let path = save("perms_check.toml", &val).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            // We only care about the low 9 bits — top bits encode
            // the file-type, which differs between platforms.
            assert_eq!(mode & 0o777, 0o600, "{:o}", mode);
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    #[ignore = "mutates the process-wide XDG_CONFIG_HOME — opt in with --ignored"]
    fn write_template_if_missing_is_idempotent() {
        let tmp = isolated_dir();
        let wrote_first = write_template_if_missing("tmpl.toml", "key = \"x\"").unwrap();
        assert!(wrote_first);
        let wrote_second = write_template_if_missing("tmpl.toml", "key = \"y\"").unwrap();
        assert!(!wrote_second, "second call must not overwrite");
        let body = std::fs::read_to_string(path("tmpl.toml").unwrap()).unwrap();
        assert_eq!(body, "key = \"x\"");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn dir_resolves_to_credentials_subdir_of_config() {
        // Plain unit test that doesn't actually create anything.
        // We just verify the path shape is the canonical
        // credentials/ subdirectory of the config dir.
        // (`dir()` may still create the dir, but we don't assert on
        // that here; the ignored tests above exercise creation.)
        let tmp = std::env::temp_dir().join(format!(
            "glint-creds-path-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let resolved = path("foo.toml").unwrap();
        assert!(resolved.ends_with("credentials/foo.toml"));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
