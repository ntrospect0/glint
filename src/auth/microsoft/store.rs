// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth;

/// What we load from `~/.config/glint/credentials/microsoft_oauth_client.toml`.
/// PKCE means there's no client secret to store — just the client_id from
/// the Azure portal app registration.
#[derive(Debug, Clone, Deserialize)]
pub struct OAuthClientConfig {
    pub client_id: String,
    /// Optional tenant override. Defaults to `common` (accepts both personal
    /// and work/school accounts). Override with a specific tenant ID to
    /// restrict to one org.
    #[serde(default = "default_tenant")]
    pub tenant: String,
}

fn default_tenant() -> String {
    "common".into()
}

impl OAuthClientConfig {
    pub fn load() -> Result<Self> {
        let path = auth::credentials_dir()?.join("microsoft_oauth_client.toml");
        if !path.exists() {
            anyhow::bail!(
                "Microsoft OAuth client config missing at {}",
                path.display()
            );
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let cfg: OAuthClientConfig = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if cfg.client_id.is_empty() || cfg.client_id.starts_with("REPLACE_WITH_") {
            anyhow::bail!(
                "{} is still the template — fill in your Azure app client_id",
                path.display()
            );
        }
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MicrosoftToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub token_type: String,
    pub scope: String,
}

impl MicrosoftToken {
    pub fn path() -> Result<PathBuf> {
        Ok(auth::credentials_dir()?.join("microsoft_oauth_token.toml"))
    }

    pub fn load() -> Result<Option<Self>> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let token: MicrosoftToken = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(token))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::path()?;
        let body = toml::to_string_pretty(self)
            .context("failed to serialize Microsoft token")?;
        std::fs::write(&path, body)
            .with_context(|| format!("failed to write {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(0o600),
            );
        }
        Ok(path)
    }

    /// True when the access token expires within `skew_secs` seconds.
    pub fn is_expired(&self, skew_secs: i64) -> bool {
        let now = Utc::now() + chrono::Duration::seconds(skew_secs);
        self.expires_at <= now
    }
}
