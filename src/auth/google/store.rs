// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::DEFAULT_ACCOUNT;
use crate::credentials;

/// Filename for the user-supplied OAuth client credentials.
pub const CLIENT_FILE: &str = "google_oauth_client.toml";

/// Token filename for `account` — `google_oauth_token.<account>.toml`. The
/// client config stays a single shared file; only the token is per-account.
fn token_file(account: &str) -> String {
    format!("google_oauth_token.{account}.toml")
}

/// Pre-0.3.0 filename for the (single) account's token. Read as a fallback
/// for the default account so source upgrades don't force a re-auth.
const LEGACY_TOKEN_FILE: &str = "google_oauth_token.toml";

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthClientConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl OAuthClientConfig {
    pub fn load() -> Result<Self> {
        let path = credentials::path(CLIENT_FILE)?;
        let Some(cfg): Option<OAuthClientConfig> = credentials::load(CLIENT_FILE)? else {
            anyhow::bail!(
                "missing {}\n\nCreate a Google Cloud OAuth desktop client and save:\n\n  client_id = \"...\"\n  client_secret = \"...\"\n",
                path.display()
            );
        };
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

    /// True if the access token is within `slack` seconds of expiring (or
    /// already past).
    pub fn is_expired(&self, slack_secs: i64) -> bool {
        let now = Utc::now();
        let cutoff = self.expires_at - chrono::Duration::seconds(slack_secs);
        now >= cutoff
    }
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
