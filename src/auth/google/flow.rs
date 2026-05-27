use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::auth::loopback;

use super::{store::GoogleToken, OAuthClientConfig, AUTH_URL, SCOPE, TOKEN_URL};

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default = "default_token_type")]
    token_type: String,
    scope: String,
}

fn default_token_type() -> String {
    "Bearer".into()
}

/// Runs the full authorization-code-with-loopback flow:
/// 1. Bind 127.0.0.1:<random>
/// 2. Open the browser to Google's consent page
/// 3. Accept the redirect, parse ?code= and ?state=
/// 4. Exchange the code for an access+refresh token at /token
/// 5. Persist via `GoogleToken::save`
pub async fn run(client: &OAuthClientConfig) -> Result<GoogleToken> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind loopback port for OAuth redirect")?;
    let port = listener
        .local_addr()
        .context("failed to read loopback addr")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let state = loopback::random_state();
    let auth_url = build_auth_url(&client.client_id, &redirect_uri, &state);

    eprintln!("Opening browser to authorize glint…");
    eprintln!("If it doesn't open, paste this URL manually:\n\n  {auth_url}\n");
    if let Err(err) = open::that(&auth_url) {
        tracing::warn!(error = %err, "failed to open browser automatically");
    }

    let (code, returned_state) =
        loopback::accept_redirect(listener, Duration::from_secs(300)).await?;
    if returned_state != state {
        anyhow::bail!("OAuth state mismatch — possible CSRF, aborting");
    }

    let token = exchange_code(client, &redirect_uri, &code).await?;
    let path = token.save()?;
    eprintln!("Saved Google OAuth token to {}", path.display());
    Ok(token)
}

fn build_auth_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "{AUTH_URL}?client_id={cid}&redirect_uri={ru}&response_type=code&scope={sc}&access_type=offline&prompt=consent&state={st}",
        cid = urlencoding::encode(client_id),
        ru = urlencoding::encode(redirect_uri),
        sc = urlencoding::encode(SCOPE),
        st = urlencoding::encode(state),
    )
}

async fn exchange_code(
    client: &OAuthClientConfig,
    redirect_uri: &str,
    code: &str,
) -> Result<GoogleToken> {
    let http = reqwest::Client::new();
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("code", code),
            ("client_id", client.client_id.as_str()),
            ("client_secret", client.client_secret.as_str()),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
        .context("token exchange request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("token exchange failed ({status}): {body}");
    }
    let tr: TokenResponse = resp
        .json()
        .await
        .context("failed to parse token exchange response")?;
    let refresh = tr
        .refresh_token
        .context("Google did not return a refresh_token — re-run auth with prompt=consent")?;
    Ok(GoogleToken {
        access_token: tr.access_token,
        refresh_token: refresh,
        expires_at: Utc::now() + chrono::Duration::seconds(tr.expires_in),
        token_type: tr.token_type,
        scope: tr.scope,
    })
}

/// Exchange a refresh token for a new access token. Reuses the existing refresh
/// token if Google doesn't issue a new one.
pub async fn refresh(client: &OAuthClientConfig, prev: &GoogleToken) -> Result<GoogleToken> {
    let http = reqwest::Client::new();
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("refresh_token", prev.refresh_token.as_str()),
            ("client_id", client.client_id.as_str()),
            ("client_secret", client.client_secret.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .context("refresh request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("token refresh failed ({status}): {body}");
    }
    let tr: TokenResponse = resp
        .json()
        .await
        .context("failed to parse refresh response")?;
    Ok(GoogleToken {
        access_token: tr.access_token,
        refresh_token: tr.refresh_token.unwrap_or_else(|| prev.refresh_token.clone()),
        expires_at: Utc::now() + chrono::Duration::seconds(tr.expires_in),
        token_type: tr.token_type,
        scope: tr.scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_encodes_params() {
        let url = build_auth_url("abc.apps.gusercontent.com", "http://127.0.0.1:54321", "s/t");
        assert!(url.starts_with(AUTH_URL));
        assert!(url.contains("client_id=abc.apps.gusercontent.com"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A54321"));
        assert!(url.contains("calendar.readonly"));
        assert!(url.contains("gmail.readonly"));
        assert!(url.contains("state=s%2Ft"));
        assert!(url.contains("access_type=offline"));
    }
}
