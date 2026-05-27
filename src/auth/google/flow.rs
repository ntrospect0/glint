use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::{store::GoogleToken, OAuthClientConfig, AUTH_URL, SCOPE, TOKEN_URL};

const SUCCESS_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>glint — authorized</title>
<style>body{font-family:system-ui,sans-serif;max-width:32rem;margin:5rem auto;padding:0 2rem;color:#222}h1{margin:0 0 .5rem}</style>
</head><body>
<h1>glint is now connected to Google Calendar.</h1>
<p>You can close this tab and return to your terminal.</p>
</body></html>"#;

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

    let state = random_state();
    let auth_url = build_auth_url(&client.client_id, &redirect_uri, &state);

    eprintln!("Opening browser to authorize glint…");
    eprintln!("If it doesn't open, paste this URL manually:\n\n  {auth_url}\n");
    if let Err(err) = open::that(&auth_url) {
        tracing::warn!(error = %err, "failed to open browser automatically");
    }

    let (code, returned_state) = tokio::time::timeout(
        Duration::from_secs(300),
        accept_redirect(listener),
    )
    .await
    .context("timed out waiting for Google to redirect to the loopback server")??;

    if returned_state != state {
        anyhow::bail!("OAuth state mismatch — possible CSRF, aborting");
    }

    let token = exchange_code(client, &redirect_uri, &code).await?;
    let path = token.save()?;
    eprintln!("Saved Google OAuth token to {}", path.display());
    Ok(token)
}

fn random_state() -> String {
    // 16 bytes of entropy is plenty for CSRF state.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{nanos:032x}-{pid:08x}")
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

/// Accept a single inbound HTTP request on `listener`, return `(code, state)`.
async fn accept_redirect(listener: TcpListener) -> Result<(String, String)> {
    let (mut socket, _peer) = listener.accept().await.context("accept failed")?;

    let mut buf = vec![0u8; 8192];
    let mut total = 0usize;
    loop {
        let n = socket
            .read(&mut buf[total..])
            .await
            .context("read from loopback socket failed")?;
        if n == 0 {
            break;
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if total == buf.len() {
            anyhow::bail!("oversized OAuth redirect request");
        }
    }
    let request = std::str::from_utf8(&buf[..total]).context("non-UTF8 OAuth redirect")?;
    let (code, state) = parse_get_query(request)?;

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        SUCCESS_HTML.len(),
        SUCCESS_HTML
    );
    socket
        .write_all(response.as_bytes())
        .await
        .context("failed to write OAuth success page")?;
    let _ = socket.shutdown().await;
    Ok((code, state))
}

fn parse_get_query(request: &str) -> Result<(String, String)> {
    // First line: "GET /?code=...&state=... HTTP/1.1"
    let first_line = request.lines().next().context("empty HTTP request")?;
    let path_and_query = first_line
        .split_whitespace()
        .nth(1)
        .context("malformed HTTP request line")?;
    let query = path_and_query
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("");
    if query.is_empty() {
        if let Some(err) = pick_query_param(path_and_query, "error") {
            anyhow::bail!("Google returned error: {err}");
        }
        anyhow::bail!("OAuth redirect carried no query string");
    }
    if let Some(err) = pick_query_param(query, "error") {
        anyhow::bail!("Google returned error: {err}");
    }
    let code = pick_query_param(query, "code")
        .ok_or_else(|| anyhow!("OAuth redirect missing `code` param"))?;
    let state = pick_query_param(query, "state")
        .ok_or_else(|| anyhow!("OAuth redirect missing `state` param"))?;
    Ok((code, state))
}

fn pick_query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == name {
            return urlencoding::decode(v).ok().map(|s| s.into_owned());
        }
    }
    None
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
        assert!(url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcalendar.readonly"));
        assert!(url.contains("state=s%2Ft"));
        assert!(url.contains("access_type=offline"));
    }

    #[test]
    fn parse_get_query_extracts_code_and_state() {
        let req = "GET /?state=s1&code=abc HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let (code, state) = parse_get_query(req).unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "s1");
    }

    #[test]
    fn parse_get_query_surfaces_error_param() {
        let req = "GET /?error=access_denied HTTP/1.1\r\n\r\n";
        let err = parse_get_query(req).unwrap_err();
        assert!(err.to_string().contains("access_denied"));
    }

    #[test]
    fn parse_get_query_url_decodes() {
        let req = "GET /?code=a%2Fb&state=x%20y HTTP/1.1\r\n\r\n";
        let (code, state) = parse_get_query(req).unwrap();
        assert_eq!(code, "a/b");
        assert_eq!(state, "x y");
    }
}
