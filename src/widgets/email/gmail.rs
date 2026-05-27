//! Gmail (Google Mail API v1) email provider. Uses the existing Google OAuth
//! token (extended with the `gmail.readonly` scope) to list and read messages.
//!
//! Compared to Graph this API is two-step: `messages.list` returns just ids,
//! then `messages.get?format=full` returns a structured MIME tree we walk to
//! find a `text/plain` part (falling back to `text/html`, which we strip).

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::auth::google::{flow, store::GoogleToken, OAuthClientConfig};

use super::html_strip::html_to_text;
use super::provider::{EmailFolder, EmailMessage, EmailProvider};

const GMAIL_BASE: &str = "https://gmail.googleapis.com/gmail/v1";

pub struct GmailProvider {
    http: reqwest::Client,
    client: OAuthClientConfig,
    token: Arc<Mutex<GoogleToken>>,
    account: Arc<StdMutex<Option<String>>>,
    /// Cached display-name → Gmail label-id index. Built once via
    /// `users.labels.list`. User-created labels have opaque ids like
    /// `Label_4823647829473`, so passing display names straight to the
    /// `labelIds` query parameter would yield zero matches. Keys are
    /// lowercased for case-insensitive lookup.
    label_index: Arc<StdMutex<Option<HashMap<String, String>>>>,
}

impl GmailProvider {
    pub fn new(client: OAuthClientConfig, token: GoogleToken) -> Result<Self> {
        let http = crate::http::shared();
        Ok(Self {
            http,
            client,
            token: Arc::new(Mutex::new(token)),
            account: Arc::new(StdMutex::new(None)),
            label_index: Arc::new(StdMutex::new(None)),
        })
    }

    async fn access_token(&self) -> Result<String> {
        let mut t = self.token.lock().await;
        if t.is_expired(60) {
            let fresh = flow::refresh(&self.client, &t).await?;
            fresh.save()?;
            *t = fresh;
        }
        Ok(t.access_token.clone())
    }

    async fn ensure_account(&self) {
        {
            let cur = self.account.lock().expect("gmail account cache poisoned");
            if cur.is_some() {
                return;
            }
        }
        let token = match self.access_token().await {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(error = %err, "gmail profile access_token failed");
                return;
            }
        };
        let url = format!("{GMAIL_BASE}/users/me/profile");
        let resp = match self.http.get(&url).bearer_auth(&token).send().await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, "gmail profile request failed");
                return;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                status = %status,
                body = %body,
                "gmail /users/me/profile returned non-success"
            );
            return;
        }
        match resp.json::<ProfileResponse>().await {
            Ok(p) => {
                if let Some(addr) = p.email_address {
                    *self.account.lock().expect("gmail account cache poisoned") =
                        Some(addr);
                } else {
                    tracing::warn!("gmail profile returned without emailAddress");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "gmail profile parse failed");
            }
        }
    }

    pub fn cached_account(&self) -> Option<String> {
        self.account
            .lock()
            .expect("gmail account cache poisoned")
            .clone()
    }

    /// Externally prime the cache (e.g. from the widget's persistent
    /// scoped cache). Only seeds when the cache is empty so a fresh
    /// `/me` resolution can still overwrite a stale value.
    pub fn seed_account_cache(&self, address: &str) {
        let mut guard = self.account.lock().expect("gmail account cache poisoned");
        if guard.is_none() {
            *guard = Some(address.to_string());
        }
    }

    /// Public folder-listing entry point used by the wizard to populate
    /// its checkbox picker. Returns `(value, label)` pairs where `value`
    /// is what gets written to email.toml's `folders` array (a Gmail
    /// label name like `INBOX` or `Bills/Utilities`) and `label` is the
    /// display string. System labels come first (in stable order),
    /// then user labels sorted by name.
    pub async fn list_folders_for_picker(&self) -> Result<Vec<(String, String)>> {
        let token = self.access_token().await?;
        let url = format!("{GMAIL_BASE}/users/me/labels");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Gmail labels request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gmail labels returned {status}: {body}");
        }
        let parsed: LabelsResponse = resp
            .json()
            .await
            .context("failed to deserialize Gmail labels")?;
        // System labels first (INBOX, SENT, …); user labels after.
        // Hide `CATEGORY_*` system labels (Gmail-internal tabs that
        // most users don't think of as folders).
        let mut system: Vec<(String, String)> = Vec::new();
        let mut user: Vec<(String, String)> = Vec::new();
        for l in parsed.labels {
            if l.name.starts_with("CATEGORY_") {
                continue;
            }
            let bucket = match l.label_type.as_deref() {
                Some("system") => &mut system,
                _ => &mut user,
            };
            bucket.push((l.name.clone(), l.name));
        }
        // Stable system-label order; alphabetised user labels.
        let priority: &[&str] = &[
            "INBOX", "STARRED", "IMPORTANT", "UNREAD", "SENT", "DRAFT",
            "SPAM", "TRASH",
        ];
        system.sort_by_key(|(name, _)| {
            priority
                .iter()
                .position(|p| *p == name.as_str())
                .unwrap_or(usize::MAX)
        });
        user.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
        system.extend(user);
        Ok(system)
    }

    /// Fetch (or return the cached) lowercased-name → label-id map. Gmail's
    /// `labels.list` returns every label visible to the account in a single
    /// call (no pagination, no hierarchy walk), so this is much simpler than
    /// the Outlook BFS. Nested labels arrive with `/`-separated names
    /// (`Bills/Utilities`) which we index verbatim — users type the full
    /// path in their TOML if they want a nested label.
    async fn ensure_label_index(&self) -> Result<HashMap<String, String>> {
        if let Some(cached) = self
            .label_index
            .lock()
            .expect("gmail label index poisoned")
            .clone()
        {
            return Ok(cached);
        }
        let token = self.access_token().await?;
        let url = format!("{GMAIL_BASE}/users/me/labels");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Gmail labels request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gmail labels returned {status}: {body}");
        }
        let parsed: LabelsResponse = resp
            .json()
            .await
            .context("failed to deserialize Gmail labels")?;
        let mut index: HashMap<String, String> = HashMap::new();
        for l in parsed.labels {
            index.insert(l.name.to_lowercase(), l.id);
        }
        *self.label_index.lock().expect("gmail label index poisoned") = Some(index.clone());
        Ok(index)
    }

    /// Translate a user-typed folder string into a Gmail label id.
    ///
    /// * Gmail system labels (`INBOX`, `SENT`, `DRAFT`, `SPAM`, `TRASH`,
    ///   `IMPORTANT`, `STARRED`, `UNREAD`, etc.) have `id == name`, so they
    ///   pass through unchanged without a network round-trip.
    /// * `Label_<digits>` already looks like a Gmail-issued id — pass through.
    /// * Everything else is matched case-insensitively against the cached
    ///   label index. Returns `Ok(None)` when the name doesn't resolve so
    ///   the caller can surface a clean error message.
    async fn resolve_label(&self, folder: &str) -> Result<Option<String>> {
        const SYSTEM: &[&str] = &[
            "INBOX",
            "SENT",
            "DRAFT",
            "DRAFTS",
            "SPAM",
            "TRASH",
            "IMPORTANT",
            "STARRED",
            "UNREAD",
            "CHAT",
            "CATEGORY_PERSONAL",
            "CATEGORY_SOCIAL",
            "CATEGORY_PROMOTIONS",
            "CATEGORY_UPDATES",
            "CATEGORY_FORUMS",
        ];
        let upper = folder.to_uppercase();
        if SYSTEM.iter().any(|s| *s == upper) {
            return Ok(Some(upper));
        }
        if folder.starts_with("Label_") {
            return Ok(Some(folder.to_string()));
        }
        let index = self.ensure_label_index().await?;
        Ok(index.get(&folder.to_lowercase()).cloned())
    }

    async fn fetch_message_full(&self, token: &str, id: &str) -> Result<RawMessage> {
        let url = format!("{GMAIL_BASE}/users/me/messages/{id}?format=full");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .context("Gmail message fetch failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gmail message returned {status}: {body}");
        }
        resp.json()
            .await
            .context("failed to deserialize Gmail message")
    }
}

#[async_trait]
impl EmailProvider for GmailProvider {
    async fn list_folders(&self) -> Result<Vec<EmailFolder>> {
        let token = self.access_token().await?;
        let url = format!("{GMAIL_BASE}/users/me/labels");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Gmail labels request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gmail labels returned {status}: {body}");
        }
        let parsed: LabelsResponse = resp
            .json()
            .await
            .context("failed to deserialize Gmail labels")?;
        // System labels we surface by default + every user-created label.
        let surface = ["INBOX", "SENT", "DRAFT", "SPAM", "TRASH", "IMPORTANT", "STARRED"];
        let mut out: Vec<EmailFolder> = Vec::new();
        for l in parsed.labels {
            let is_system = l.label_type.as_deref() == Some("system");
            let keep = if is_system {
                surface.iter().any(|s| s == &l.id)
            } else {
                true
            };
            if keep {
                out.push(EmailFolder {
                    label: l.name.clone(),
                    id: l.id,
                });
            }
        }
        Ok(out)
    }

    async fn fetch_recent(
        &self,
        folder: &str,
        since: DateTime<Utc>,
        max: usize,
    ) -> Result<Vec<EmailMessage>> {
        self.ensure_account().await;
        let token = self.access_token().await?;

        // Translate the user-typed folder name into a Gmail label id.
        // System labels (INBOX, SENT, …) pass through unchanged because
        // their id equals their name; user-created labels go through the
        // cached `labels.list` index so we don't ship the display name
        // ("Finances") to an endpoint that only accepts the opaque id
        // ("Label_4823647829473") and silently returns zero matches.
        let label_id = match self.resolve_label(folder).await? {
            Some(id) => id,
            None => {
                anyhow::bail!(
                    "label {folder:?} not found in this Gmail account — check the spelling \
                     in email.toml (nested labels need their full path, e.g. \"Bills/Utilities\")"
                );
            }
        };

        // newer_than:Nd filter — we approximate `since` as a day count to
        // play nicely with Gmail's search syntax (more efficient than
        // running a full filter on receivedDateTime).
        let now = Utc::now();
        let days = ((now - since).num_days().max(0) + 1) as u32;
        let query = format!("newer_than:{days}d");

        let list_url = format!(
            "{GMAIL_BASE}/users/me/messages?maxResults={top}&labelIds={lid}&q={q}",
            top = max.min(100),
            lid = urlencoding::encode(&label_id),
            q = urlencoding::encode(&query),
        );
        let resp = self
            .http
            .get(&list_url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Gmail messages.list failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gmail messages.list returned {status}: {body}");
        }
        let listed: ListResponse = resp
            .json()
            .await
            .context("failed to deserialize Gmail messages.list")?;

        let mut out: Vec<EmailMessage> = Vec::new();
        // Walk the id list serially. Parallelism would help but the per-batch
        // ceiling (100 by config) is small enough that sequential reqwest
        // calls stay well under a 10s budget and we don't have to think about
        // backoff. We can revisit with a futures::stream::buffer_unordered if
        // this proves too slow.
        for stub in listed.messages.unwrap_or_default() {
            let raw = match self.fetch_message_full(&token, &stub.id).await {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(id = %stub.id, error = %err, "gmail message fetch failed, skipping");
                    continue;
                }
            };
            if let Some(m) = raw.into_message(folder, &since) {
                out.push(m);
            }
        }
        Ok(out)
    }

    fn provider_label(&self) -> &str {
        "gmail"
    }

    fn account_address(&self) -> Option<&str> {
        // Same shape as Outlook: callers go through `cached_account()` for a
        // cloned snapshot. Direct & is non-trivial through a Mutex.
        None
    }
}

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ProfileResponse {
    #[serde(default, rename = "emailAddress")]
    email_address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LabelsResponse {
    #[serde(default)]
    labels: Vec<RawLabel>,
}

#[derive(Debug, Deserialize)]
struct RawLabel {
    id: String,
    name: String,
    #[serde(default, rename = "type")]
    label_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    messages: Option<Vec<MessageStub>>,
}

#[derive(Debug, Deserialize)]
struct MessageStub {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    id: String,
    #[serde(default, rename = "labelIds")]
    label_ids: Vec<String>,
    #[serde(default, rename = "internalDate")]
    internal_date: Option<String>,
    #[serde(default)]
    payload: Option<RawPayload>,
}

#[derive(Debug, Deserialize, Clone)]
struct RawPayload {
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(default)]
    headers: Vec<RawHeader>,
    #[serde(default)]
    body: Option<RawBody>,
    #[serde(default)]
    parts: Vec<RawPayload>,
}

#[derive(Debug, Deserialize, Clone)]
struct RawHeader {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize, Clone)]
struct RawBody {
    #[serde(default)]
    data: Option<String>,
}

impl RawMessage {
    fn into_message(self, folder: &str, since: &DateTime<Utc>) -> Option<EmailMessage> {
        let payload = self.payload?;
        let received = parse_internal_date(self.internal_date.as_deref())?;
        if received.with_timezone(&Utc) < *since {
            return None;
        }
        let subject = header(&payload.headers, "Subject").unwrap_or_default();
        let from_raw = header(&payload.headers, "From").unwrap_or_default();
        let (from_name, from_address) = split_from(&from_raw);

        // UNREAD = server still considers the message unread.
        let server_unread = self.label_ids.iter().any(|l| l == "UNREAD");

        let plain_body = extract_body(&payload).unwrap_or_default();

        // Build a web URL into Gmail. Account index defaults to 0 — same
        // assumption Gmail's own "view in Gmail" buttons use when the user
        // is single-signed-in.
        let web_url = Some(format!("https://mail.google.com/mail/u/0/#inbox/{}", self.id));

        Some(EmailMessage {
            id: self.id,
            folder: folder.to_string(),
            from_name,
            from_address,
            subject,
            received,
            server_unread,
            plain_body,
            web_url,
        })
    }
}

fn header(headers: &[RawHeader], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.clone())
}

/// Parse a `From:` header into (name, address). Handles:
///   "Alice Smith <alice@example.com>" → (Some("Alice Smith"), "alice@example.com")
///   "alice@example.com"                → (None, "alice@example.com")
///   ""                                 → (None, "")
pub(crate) fn split_from(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (None, String::new());
    }
    if let Some(start) = trimmed.rfind('<') {
        if let Some(end) = trimmed[start..].find('>') {
            let addr = &trimmed[start + 1..start + end];
            let name = trimmed[..start].trim().trim_matches('"');
            let name = if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            };
            return (name, addr.to_string());
        }
    }
    (None, trimmed.to_string())
}

/// Pull a `text/plain` part out of a Gmail MIME payload. Falls back to
/// `text/html` (with strip) if no plain part exists.
fn extract_body(payload: &RawPayload) -> Option<String> {
    if let Some(plain) = walk_for_mime(payload, "text/plain") {
        return Some(plain);
    }
    if let Some(html) = walk_for_mime(payload, "text/html") {
        return Some(html_to_text(&html));
    }
    None
}

fn walk_for_mime(payload: &RawPayload, want: &str) -> Option<String> {
    let mime = payload.mime_type.as_deref().unwrap_or("").to_ascii_lowercase();
    if mime == want {
        if let Some(b) = &payload.body {
            if let Some(data) = &b.data {
                return decode_base64_url(data).ok();
            }
        }
    }
    for child in &payload.parts {
        if let Some(found) = walk_for_mime(child, want) {
            return Some(found);
        }
    }
    None
}

/// Gmail uses base64url (RFC 4648 §5) without padding. Hand-rolled to avoid
/// adding the `base64` crate just for this one call.
fn decode_base64_url(input: &str) -> anyhow::Result<String> {
    // Build a lookup table once per call. With Gmail's typical 4-20KB
    // bodies, the constant cost is negligible compared to the network.
    fn decode_char(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = input.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=').collect();
    let mut out: Vec<u8> = Vec::with_capacity(cleaned.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in cleaned {
        let v = decode_char(c).ok_or_else(|| anyhow::anyhow!("invalid base64url char: {c}"))?;
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(String::from_utf8_lossy(&out).to_string())
}

fn parse_internal_date(raw: Option<&str>) -> Option<DateTime<Local>> {
    let s = raw?;
    let millis: i64 = s.parse().ok()?;
    let secs = millis / 1000;
    let nanos = (millis % 1000) * 1_000_000;
    Some(
        Utc.timestamp_opt(secs, nanos as u32)
            .single()?
            .with_timezone(&Local),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_from_with_display_name() {
        let (name, addr) = split_from("Alice Smith <alice@example.com>");
        assert_eq!(name.as_deref(), Some("Alice Smith"));
        assert_eq!(addr, "alice@example.com");
    }

    #[test]
    fn split_from_bare_address() {
        let (name, addr) = split_from("alice@example.com");
        assert_eq!(name, None);
        assert_eq!(addr, "alice@example.com");
    }

    #[test]
    fn split_from_quoted_name() {
        let (name, addr) = split_from("\"Bob the Builder\" <bob@example.com>");
        assert_eq!(name.as_deref(), Some("Bob the Builder"));
        assert_eq!(addr, "bob@example.com");
    }

    #[test]
    fn decode_base64_url_simple() {
        // "Hello" → "SGVsbG8" in base64url (no padding)
        assert_eq!(decode_base64_url("SGVsbG8").unwrap(), "Hello");
    }

    #[test]
    fn decode_base64_url_ignores_whitespace() {
        // Gmail line-wraps base64 payloads — make sure linebreaks survive.
        let wrapped = "SGVsbG8g\nV29ybGQ=";
        assert_eq!(decode_base64_url(wrapped).unwrap(), "Hello World");
    }

    #[test]
    fn extract_body_prefers_text_plain() {
        let p = RawPayload {
            mime_type: Some("multipart/alternative".into()),
            headers: vec![],
            body: None,
            parts: vec![
                RawPayload {
                    mime_type: Some("text/html".into()),
                    headers: vec![],
                    body: Some(RawBody {
                        data: Some("PHA-aHRtbDwvcD4".into()), // <p>html</p>
                    }),
                    parts: vec![],
                },
                RawPayload {
                    mime_type: Some("text/plain".into()),
                    headers: vec![],
                    body: Some(RawBody {
                        data: Some("cGxhaW4gYm9keQ".into()), // "plain body"
                    }),
                    parts: vec![],
                },
            ],
        };
        assert_eq!(extract_body(&p).as_deref(), Some("plain body"));
    }
}
