// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::DEFAULT_ACCOUNT;
use crate::credentials;

const CLIENT_FILE: &str = "microsoft_oauth_client.toml";

/// Token filename for `account` — `microsoft_oauth_token.<account>.toml`.
/// The client config stays a single shared file; only the token is
/// per-account.
fn token_file(account: &str) -> String {
    format!("microsoft_oauth_token.{account}.toml")
}

/// Pre-0.3.0 filename for the (single) account's token. Read as a fallback
/// for the default account so source upgrades don't force a re-auth.
const LEGACY_TOKEN_FILE: &str = "microsoft_oauth_token.toml";

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
    /// Load the token for a named account (e.g. `"work"`).
    pub fn load_account(account: &str) -> Result<Option<Self>> {
        if let Some(token) = credentials::load(&token_file(account))? {
            return Ok(Some(token));
        }
        // Legacy fallback: before 0.3.0 the default account's token lived in
        // an unsuffixed file. Read it so a source upgrade doesn't force a
        // re-auth; the next token refresh saves to the account-scoped name.
        if account == DEFAULT_ACCOUNT {
            return credentials::load(LEGACY_TOKEN_FILE);
        }
        Ok(None)
    }

    /// Save the token under a named account.
    pub fn save_account(&self, account: &str) -> Result<PathBuf> {
        credentials::save(&token_file(account), self)
    }

    /// Load the default account's token. Used by callers that aren't
    /// account-aware (the Email widget, post-auth folder pre-fetch).
    pub fn load() -> Result<Option<Self>> {
        Self::load_account(DEFAULT_ACCOUNT)
    }

    /// Save the default account's token.
    pub fn save(&self) -> Result<PathBuf> {
        self.save_account(DEFAULT_ACCOUNT)
    }

    /// True when the access token expires within `skew_secs` seconds.
    pub fn is_expired(&self, skew_secs: i64) -> bool {
        let now = Utc::now() + chrono::Duration::seconds(skew_secs);
        self.expires_at <= now
    }
}
