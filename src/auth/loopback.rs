//! Provider-agnostic helpers for OAuth authorization-code flows that use a
//! loopback redirect URI. Both `auth/google` and `auth/microsoft` build on
//! this — the per-provider modules supply the URL templates, scopes, and
//! token-exchange request bodies; this module handles the small HTTP server
//! that catches the `?code=` redirect plus the boilerplate around it.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// HTML shown to the user once the redirect lands. The browser stays on this
/// tab until they close it.
const SUCCESS_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>glint — authorized</title>
<style>body{font-family:system-ui,sans-serif;max-width:32rem;margin:5rem auto;padding:0 2rem;color:#222}h1{margin:0 0 .5rem}</style>
</head><body>
<h1>glint is now connected.</h1>
<p>You can close this tab and return to your terminal.</p>
</body></html>"#;

/// 16 bytes of entropy is plenty for a CSRF `state` value.
pub fn random_state() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{nanos:032x}-{pid:08x}")
}

/// Accept a single inbound HTTP request on `listener` and parse the `?code=`
/// and `?state=` query parameters from it. Times out after `max_wait` so a
/// stalled or never-completed consent doesn't hang the CLI forever.
pub async fn accept_redirect(
    listener: TcpListener,
    max_wait: Duration,
) -> Result<(String, String)> {
    tokio::time::timeout(max_wait, accept_redirect_inner(listener))
        .await
        .context("timed out waiting for the OAuth redirect to land")?
}

async fn accept_redirect_inner(listener: TcpListener) -> Result<(String, String)> {
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

/// Parses an HTTP request line and pulls out `code` and `state` query params.
/// Surfaces any `?error=` payload as an error so the user sees the provider's
/// reason if they denied or something else went wrong.
pub fn parse_get_query(request: &str) -> Result<(String, String)> {
    let first_line = request.lines().next().context("empty HTTP request")?;
    let path_and_query = first_line
        .split_whitespace()
        .nth(1)
        .context("malformed HTTP request line")?;
    let query = path_and_query.split_once('?').map(|(_, q)| q).unwrap_or("");
    if query.is_empty() {
        if let Some(err) = pick_query_param(path_and_query, "error") {
            anyhow::bail!("OAuth provider returned error: {err}");
        }
        anyhow::bail!("OAuth redirect carried no query string");
    }
    if let Some(err) = pick_query_param(query, "error") {
        anyhow::bail!("OAuth provider returned error: {err}");
    }
    let code = pick_query_param(query, "code")
        .ok_or_else(|| anyhow!("OAuth redirect missing `code` param"))?;
    let state = pick_query_param(query, "state")
        .ok_or_else(|| anyhow!("OAuth redirect missing `state` param"))?;
    Ok((code, state))
}

pub fn pick_query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == name {
            return urlencoding::decode(v).ok().map(|s| s.into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(parse_get_query(req).is_err());
    }

    #[test]
    fn parse_get_query_url_decodes() {
        let req = "GET /?code=a%2Fb&state=x%20y HTTP/1.1\r\n\r\n";
        let (code, state) = parse_get_query(req).unwrap();
        assert_eq!(code, "a/b");
        assert_eq!(state, "x y");
    }
}
