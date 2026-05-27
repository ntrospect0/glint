// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::credentials_dir;

/// Filename for the user-supplied OAuth client credentials.
pub const CLIENT_FILE: &str = "google_oauth_client.toml";

/// Filename for the persisted access/refresh token pair.
pub const TOKEN_FILE: &str = "google_oauth_token.toml";

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthClientConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl OAuthClientConfig {
    pub fn load() -> Result<Self> {
        let path = credentials_dir()?.join(CLIENT_FILE);
        if !path.exists() {
            anyhow::bail!(
                "missing {}\n\nCreate a Google Cloud OAuth desktop client and save:\n\n  client_id = \"...\"\n  client_secret = \"...\"\n",
                path.display()
            );
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let cfg: OAuthClientConfig = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if cfg.client_id.is_empty()
            || cfg.client_id.starts_with("REPLACE_WITH_")
            || cfg.client_secret.is_empty()
            || cfg.client_secret.starts_with("REPLACE_WITH_")
        {
            anyhow::bail!(
                "{} is still the template — fill in client_id + client_secret from your Google Cloud OAuth client.",
                path.display()
            );
        }
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GoogleToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    pub scope: String,
}

fn default_token_type() -> String {
    "Bearer".into()
}

impl GoogleToken {
    pub fn path() -> Result<PathBuf> {
        Ok(credentials_dir()?.join(TOKEN_FILE))
    }

    pub fn load() -> Result<Option<Self>> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let token: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(token))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::path()?;
        write_secret(&path, &toml::to_string_pretty(self).context("token serialize failed")?)?;
        Ok(path)
    }

    /// True if the access token is within `slack` seconds of expiring (or
    /// already past).
    pub fn is_expired(&self, slack_secs: i64) -> bool {
        let now = Utc::now();
        let cutoff = self.expires_at - chrono::Duration::seconds(slack_secs);
        now >= cutoff
    }
}

/// Write secret-bearing TOML to `path` with 0600 perms (Unix only).
pub fn write_secret(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips_through_toml() {
        let t = GoogleToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: Utc::now(),
            token_type: "Bearer".into(),
            scope: SCOPE_FOR_TEST.into(),
        };
        let s = toml::to_string_pretty(&t).unwrap();
        let parsed: GoogleToken = toml::from_str(&s).unwrap();
        assert_eq!(parsed.access_token, t.access_token);
        assert_eq!(parsed.refresh_token, t.refresh_token);
        assert_eq!(parsed.scope, t.scope);
    }

    #[test]
    fn is_expired_respects_slack() {
        let t = GoogleToken {
            access_token: String::new(),
            refresh_token: String::new(),
            expires_at: Utc::now() + chrono::Duration::seconds(30),
            token_type: "Bearer".into(),
            scope: String::new(),
        };
        assert!(!t.is_expired(10));
        assert!(t.is_expired(60));
    }

    const SCOPE_FOR_TEST: &str = "https://www.googleapis.com/auth/calendar.readonly";
}
