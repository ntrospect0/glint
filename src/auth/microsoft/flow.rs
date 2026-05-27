use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

use crate::auth::loopback;

use super::{store::MicrosoftToken, OAuthClientConfig, AUTH_URL, SCOPE, TOKEN_URL};

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default = "default_token_type")]
    token_type: String,
    #[serde(default)]
    scope: String,
}

fn default_token_type() -> String {
    "Bearer".into()
}

/// Run the PKCE-flavored OAuth authorization-code flow against Microsoft's
/// identity platform. Mirrors the Google flow except:
///   - tenant goes in the URL path (default `common`)
///   - response uses PKCE: code_challenge + code_verifier in place of a
///     client_secret. Azure desktop apps don't require a secret.
pub async fn run(client: &OAuthClientConfig) -> Result<MicrosoftToken> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind loopback port for OAuth redirect")?;
    let port = listener
        .local_addr()
        .context("failed to read loopback addr")?
        .port();
    // Microsoft validates redirect URIs by exact string match against the
    // app registration, and the "Mobile and desktop applications" platform
    // option whitelists `http://localhost` — *not* `http://127.0.0.1`. The
    // browser will resolve `localhost` to 127.0.0.1 anyway, so our listener
    // still receives the callback.
    let redirect_uri = format!("http://localhost:{port}");

    let state = loopback::random_state();
    let (verifier, challenge) = pkce_pair()?;

    let auth_url = build_auth_url(client, &redirect_uri, &state, &challenge);

    eprintln!("Opening browser to authorize glint with Microsoft…");
    eprintln!(
        "If it doesn't open, paste this URL manually:\n\n  {auth_url}\n"
    );
    if let Err(err) = open::that(&auth_url) {
        tracing::warn!(error = %err, "failed to open browser automatically");
    }

    let (code, returned_state) =
        loopback::accept_redirect(listener, Duration::from_secs(300)).await?;
    if returned_state != state {
        anyhow::bail!("OAuth state mismatch — possible CSRF, aborting");
    }

    let token = exchange_code(client, &redirect_uri, &code, &verifier).await?;
    let path = token.save()?;
    eprintln!("Saved Microsoft OAuth token to {}", path.display());
    Ok(token)
}

fn build_auth_url(
    client: &OAuthClientConfig,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> String {
    // Tenant goes inside AUTH_URL via substitution. AUTH_URL hard-codes
    // `common` but the user can override via the `tenant` config field.
    let url = AUTH_URL.replace("common", &client.tenant);
    format!(
        "{url}?client_id={cid}&response_type=code&redirect_uri={ru}&response_mode=query&scope={sc}&state={st}&code_challenge={cc}&code_challenge_method=S256",
        cid = urlencoding::encode(&client.client_id),
        ru = urlencoding::encode(redirect_uri),
        sc = urlencoding::encode(SCOPE),
        st = urlencoding::encode(state),
        cc = urlencoding::encode(code_challenge),
    )
}

async fn exchange_code(
    client: &OAuthClientConfig,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<MicrosoftToken> {
    let http = crate::http::shared();
    let url = TOKEN_URL.replace("common", &client.tenant);
    let resp = http
        .post(&url)
        .form(&[
            ("client_id", client.client_id.as_str()),
            ("scope", SCOPE),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("Microsoft token exchange request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Microsoft token exchange failed ({status}): {body}");
    }
    let tr: TokenResponse = resp
        .json()
        .await
        .context("failed to parse Microsoft token response")?;
    let refresh = tr
        .refresh_token
        .context("Microsoft did not return a refresh_token — check that `offline_access` is in your scope")?;
    Ok(MicrosoftToken {
        access_token: tr.access_token,
        refresh_token: refresh,
        expires_at: Utc::now() + chrono::Duration::seconds(tr.expires_in),
        token_type: tr.token_type,
        scope: tr.scope,
    })
}

/// Use a refresh token to get a fresh access token. Microsoft may rotate the
/// refresh token too; if a new one is returned we use it, otherwise we keep
/// the existing one.
pub async fn refresh(client: &OAuthClientConfig, prev: &MicrosoftToken) -> Result<MicrosoftToken> {
    let http = crate::http::shared();
    let url = TOKEN_URL.replace("common", &client.tenant);
    let resp = http
        .post(&url)
        .form(&[
            ("client_id", client.client_id.as_str()),
            ("scope", SCOPE),
            ("refresh_token", prev.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .context("Microsoft refresh request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Microsoft token refresh failed ({status}): {body}");
    }
    let tr: TokenResponse = resp
        .json()
        .await
        .context("failed to parse Microsoft refresh response")?;
    Ok(MicrosoftToken {
        access_token: tr.access_token,
        refresh_token: tr.refresh_token.unwrap_or_else(|| prev.refresh_token.clone()),
        expires_at: Utc::now() + chrono::Duration::seconds(tr.expires_in),
        token_type: tr.token_type,
        scope: if tr.scope.is_empty() {
            prev.scope.clone()
        } else {
            tr.scope
        },
    })
}

/// Generate a PKCE (verifier, challenge) pair per RFC 7636 §4. Verifier is
/// 32 bytes of CSPRNG output, base64url-encoded; challenge is SHA-256 of the
/// verifier, base64url-encoded.
fn pkce_pair() -> Result<(String, String)> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| anyhow::anyhow!("getrandom failed for PKCE verifier: {err}"))?;
    let verifier = base64_url_encode(&bytes);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = base64_url_encode(&hasher.finalize());
    Ok((verifier, challenge))
}

/// Base64url-encode (RFC 4648 §5) without padding. ~30 lines vs. pulling in
/// the `base64` crate just for this one call.
fn base64_url_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity((input.len() * 4).div_ceil(3));
    let chunks = input.chunks_exact(3);
    let rem = chunks.remainder();
    for chunk in chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        output.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        output.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        output.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        output.push(ALPHABET[(n & 0x3F) as usize] as char);
    }
    match rem.len() {
        0 => {}
        1 => {
            let n = u32::from(rem[0]) << 16;
            output.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            output.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            output.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            output.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            output.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        }
        _ => unreachable!(),
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_and_challenge_are_unique_and_url_safe() {
        let (v1, c1) = pkce_pair().unwrap();
        let (v2, _c2) = pkce_pair().unwrap();
        assert_ne!(v1, v2);
        assert!(!c1.is_empty());
        for ch in v1.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
                "verifier contains invalid char: {ch:?}"
            );
        }
        for ch in c1.chars() {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
                "challenge contains invalid char: {ch:?}"
            );
        }
    }

    #[test]
    fn base64_url_encode_matches_known_vectors() {
        // RFC 4648 §10 test vectors (base64url, no padding).
        assert_eq!(base64_url_encode(b""), "");
        assert_eq!(base64_url_encode(b"f"), "Zg");
        assert_eq!(base64_url_encode(b"fo"), "Zm8");
        assert_eq!(base64_url_encode(b"foo"), "Zm9v");
        assert_eq!(base64_url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64_url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(base64_url_encode(b"foobar"), "Zm9vYmFy");
    }
}
