// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::credentials;

const CLIENT_FILE: &str = "microsoft_oauth_client.toml";
const TOKEN_FILE: &str = "microsoft_oauth_token.toml";

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
        let path = credentials::path(CLIENT_FILE)?;
        let Some(cfg): Option<OAuthClientConfig> = credentials::load(CLIENT_FILE)? else {
            anyhow::bail!(
                "Microsoft OAuth client config missing at {}",
                path.display()
            );
        };
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
    pub fn load() -> Result<Option<Self>> {
        credentials::load(TOKEN_FILE)
    }

    pub fn save(&self) -> Result<PathBuf> {
        credentials::save(TOKEN_FILE, self)
    }

    /// True when the access token expires within `skew_secs` seconds.
    pub fn is_expired(&self, skew_secs: i64) -> bool {
        let now = Utc::now() + chrono::Duration::seconds(skew_secs);
        self.expires_at <= now
    }
}
