// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Microsoft Graph (Outlook / Microsoft 365) email provider. Mirrors the
//! calendar Outlook provider's HTTP+OAuth shape — same token refresh path,
//! same Prefer header for UTC normalization.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex as StdMutex},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::auth::microsoft::{flow, store::MicrosoftToken, OAuthClientConfig};

use super::html_strip::html_to_text;
use super::provider::{EmailFolder, EmailMessage, EmailProvider};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

pub struct OutlookEmailProvider {
    http: reqwest::Client,
    client: OAuthClientConfig,
    token: Arc<Mutex<MicrosoftToken>>,
    /// Cached account address — populated lazily on the first successful
    /// `/me` call so the widget can show the user's address in the title.
    account: Arc<StdMutex<Option<String>>>,
    /// Cached folder index: lowercased display name → Graph folder id.
    /// Walked once per session by `ensure_folder_index` on the first
    /// `fetch_recent` call so user-friendly names like "Purchasing" or
    /// "Cloud" (and folders nested under Inbox) resolve to their real
    /// Graph ids — otherwise Graph returns ErrorInvalidIdMalformed.
    folder_index: Arc<StdMutex<Option<HashMap<String, String>>>>,
}

impl OutlookEmailProvider {
    pub fn new(client: OAuthClientConfig, token: MicrosoftToken) -> Result<Self> {
        let http = crate::http::shared();
        Ok(Self {
            http,
            client,
            token: Arc::new(Mutex::new(token)),
            account: Arc::new(StdMutex::new(None)),
            folder_index: Arc::new(StdMutex::new(None)),
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

    /// Hydrate the cached account address from `/me`. Best-effort: a failure
    /// just leaves the cache empty so the widget title shows "(loading…)" on
    /// the next paint.
    async fn ensure_account(&self) {
        {
            let cur = self.account.lock().expect("email account cache poisoned");
            if cur.is_some() {
                return;
            }
        }
        let token = match self.access_token().await {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(error = %err, "outlook /me access_token failed; title stays (loading…)");
                return;
            }
        };
        let url = format!("{GRAPH_BASE}/me?$select=mail,userPrincipalName");
        let resp = match self.http.get(&url).bearer_auth(&token).send().await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, "outlook /me request failed");
                return;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                status = %status,
                body = %body,
                "outlook /me returned non-success — most often a missing User.Read scope; re-authorize via the wizard's Authorize Microsoft step"
            );
            return;
        }
        let parsed: MeResponse = match resp.json().await {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(error = %err, "outlook /me parse failed");
                return;
            }
        };
        let addr = parsed.mail.or(parsed.user_principal_name);
        if let Some(a) = addr {
            *self.account.lock().expect("email account cache poisoned") = Some(a);
        } else {
            tracing::warn!(
                "outlook /me returned without mail or userPrincipalName; \
                 token likely lacks the User.Read scope"
            );
        }
    }

    /// Build (or return the cached) display-name → Graph-id index by
    /// walking the entire mail-folder tree breadth-first. Subfolders
    /// (e.g. `Inbox/Purchasing`) get indexed under their leaf display
    /// name. Collisions on duplicate names are resolved first-wins
    /// (top of tree beats deeper nesting), which is the most predictable
    /// behavior for hand-typed configs.
    async fn ensure_folder_index(&self) -> Result<HashMap<String, String>> {
        if let Some(cached) = self.folder_index.lock().expect("folder index poisoned").clone() {
            return Ok(cached);
        }
        let token = self.access_token().await?;
        let mut index: HashMap<String, String> = HashMap::new();
        // BFS queue. `None` = top-level `/me/mailFolders`; `Some(id)` =
        // `/me/mailFolders/{id}/childFolders`. `includeHiddenFolders=true`
        // surfaces ones the user has hidden in the Outlook UI but still
        // configured here — better to find them than to silently fail.
        let mut queue: VecDeque<Option<String>> = VecDeque::new();
        queue.push_back(None);
        while let Some(parent) = queue.pop_front() {
            let url = match &parent {
                None => format!(
                    "{GRAPH_BASE}/me/mailFolders?$top=100&includeHiddenFolders=true"
                ),
                Some(id) => format!(
                    "{GRAPH_BASE}/me/mailFolders/{}/childFolders?$top=100&includeHiddenFolders=true",
                    urlencoding::encode(id)
                ),
            };
            let resp = self.http.get(&url).bearer_auth(&token).send().await?;
            if !resp.status().is_success() {
                // Don't fail the whole walk for one bad branch — just
                // skip and let other folders still resolve.
                continue;
            }
            let parsed: FoldersResponse = match resp.json().await {
                Ok(p) => p,
                Err(_) => continue,
            };
            for f in parsed.value {
                index
                    .entry(f.display_name.to_ascii_lowercase())
                    .or_insert(f.id.clone());
                if f.child_folder_count.unwrap_or(0) > 0 {
                    queue.push_back(Some(f.id));
                }
            }
        }
        *self.folder_index.lock().expect("folder index poisoned") = Some(index.clone());
        Ok(index)
    }

    /// Resolve a user-supplied folder name to the value glint should
    /// send to Graph. Well-known names (`inbox`, `sentitems`, …) pass
    /// through; everything else is looked up in the folder index by
    /// case-insensitive display name. Returns `None` when the index
    /// has been consulted and the name isn't there — callers surface
    /// that as a clean error to the user instead of a Graph 400.
    async fn resolve_folder(&self, folder: &str) -> Result<Option<String>> {
        let lower = folder.to_ascii_lowercase();
        // Well-known shortcuts that Graph accepts as literal ids.
        const WELL_KNOWN: &[&str] = &[
            "inbox",
            "sentitems",
            "drafts",
            "junkemail",
            "deleteditems",
            "archive",
            "outbox",
            "conversationhistory",
            "scheduled",
            "recoverableitemsdeletions",
            "searchfolders",
        ];
        if WELL_KNOWN.contains(&lower.as_str()) {
            return Ok(Some(lower));
        }
        // If `folder` already looks like a Graph id (long base64-ish
        // blob with mixed case + `=` padding) — pass it through. The
        // index BFS would index it under its display name, but a user
        // who hand-typed an id deserves to have that respected.
        if folder.len() > 60 && folder.contains('=') {
            return Ok(Some(folder.to_string()));
        }
        let index = self.ensure_folder_index().await?;
        Ok(index.get(&lower).cloned())
    }
}

#[async_trait]
impl EmailProvider for OutlookEmailProvider {
    async fn list_folders(&self) -> Result<Vec<EmailFolder>> {
        let token = self.access_token().await?;
        let url = format!("{GRAPH_BASE}/me/mailFolders?$top=50");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Graph mailFolders request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Graph mailFolders returned {status}: {body}");
        }
        let parsed: FoldersResponse = resp
            .json()
            .await
            .context("failed to deserialize Graph mailFolders response")?;
        Ok(parsed
            .value
            .into_iter()
            .map(|f| EmailFolder {
                label: f.display_name.clone(),
                id: f.id,
            })
            .collect())
    }

    async fn fetch_recent(
        &self,
        folder: &str,
        since: DateTime<Utc>,
        max: usize,
    ) -> Result<Vec<EmailMessage>> {
        // Background-populate the account cache on the first fetch.
        self.ensure_account().await;

        // Resolve the user-typed folder name to a Graph id. Well-known
        // names pass through unchanged; everything else (including
        // subfolders like `Inbox/Purchasing`) gets looked up in the
        // cached folder index so we don't send a bare display name to
        // Graph and get a 400 ErrorInvalidIdMalformed.
        let folder_id = match self.resolve_folder(folder).await? {
            Some(id) => id,
            None => {
                anyhow::bail!(
                    "folder {folder:?} not found in this Outlook account — check the spelling \
                     in email.toml (subfolder names are matched case-insensitively against \
                     their leaf display name)"
                );
            }
        };

        let token = self.access_token().await?;
        // $filter on receivedDateTime + $top + $orderby. `$select` keeps the
        // payload small so we don't burn bandwidth on attachments metadata.
        let select = "id,from,subject,receivedDateTime,isRead,webLink,body,bodyPreview";
        let filter = format!("receivedDateTime ge {}", since.to_rfc3339());
        // URL-encode every folder id we route through the path — Graph
        // ids can contain `=` and other URL-unsafe chars. Well-known
        // names survive URL-encoding unchanged anyway (they're all
        // lowercase ASCII), so a single code path here is fine.
        let path = format!(
            "/me/mailFolders/{}/messages",
            urlencoding::encode(&folder_id)
        );
        let url = format!(
            "{GRAPH_BASE}{path}?$filter={f}&$select={s}&$top={top}&$orderby=receivedDateTime desc",
            f = urlencoding::encode(&filter),
            s = urlencoding::encode(select),
            top = max.min(100),
        );

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .header("Prefer", "outlook.body-content-type=\"text\"")
            .send()
            .await
            .context("Graph messages request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Graph messages returned {status}: {body}");
        }
        let parsed: MessagesResponse = resp
            .json()
            .await
            .context("failed to deserialize Graph messages response")?;
        let display_folder = if folder.eq_ignore_ascii_case("inbox") {
            "INBOX".to_string()
        } else {
            folder.to_string()
        };
        Ok(parsed
            .value
            .into_iter()
            .filter_map(|m| m.into_message(&display_folder))
            .collect())
    }

    fn provider_label(&self) -> &str {
        "outlook"
    }

    fn account_address(&self) -> Option<&str> {
        // The cached address lives behind a Mutex, so we can't safely hand
        // out a `&str` tied to `self`. Callers go through `cached_account()`
        // for a cloned snapshot instead. The widget caches its own copy on
        // the EmailState after each fetch (see EmailWidget::spawn_refresh).
        None
    }
}

// We need an external accessor for the cached account from the widget. The
// trait's `account_address` returns `Option<&str>` but our internal cache
// lives behind a Mutex, so we expose a separate snapshot method here.
impl OutlookEmailProvider {
    /// Cloned snapshot of the cached account address. Returns `None` until
    /// the first successful `/me` round-trip lands.
    pub fn cached_account(&self) -> Option<String> {
        self.account
            .lock()
            .expect("email account cache poisoned")
            .clone()
    }

    /// Externally prime the cache from a persisted value. Only seeds
    /// when the cache is empty so a fresh `/me` resolution can still
    /// overwrite a stale value.
    pub fn seed_account_cache(&self, address: &str) {
        let mut guard = self
            .account
            .lock()
            .expect("email account cache poisoned");
        if guard.is_none() {
            *guard = Some(address.to_string());
        }
    }

    /// Public folder-listing entry point used by the wizard to populate
    /// its checkbox picker. Walks just the top-level mail folders
    /// (`/me/mailFolders`) — deep recursion isn't worth the latency for
    /// a one-shot setup picker, and most users only surface top-level
    /// folders (Inbox, Sent Items, …). Returns `(value, label)` pairs
    /// where both halves are the folder's display name (what email.toml
    /// expects).
    pub async fn list_folders_for_picker(
        &self,
    ) -> Result<Vec<(String, String)>> {
        let token = self.access_token().await?;
        let url = format!(
            "{GRAPH_BASE}/me/mailFolders?$top=100&includeHiddenFolders=true"
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("Outlook mailFolders request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Outlook mailFolders returned {status}: {body}");
        }
        let parsed: FoldersResponse = resp
            .json()
            .await
            .context("failed to deserialize Outlook mailFolders")?;
        let priority: &[&str] =
            &["Inbox", "Sent Items", "Drafts", "Archive", "Junk Email", "Deleted Items"];
        let mut folders: Vec<(String, String)> = parsed
            .value
            .into_iter()
            .map(|f| (f.display_name.clone(), f.display_name))
            .collect();
        folders.sort_by(|a, b| {
            let ai = priority
                .iter()
                .position(|p| *p == a.0.as_str())
                .unwrap_or(usize::MAX);
            let bi = priority
                .iter()
                .position(|p| *p == b.0.as_str())
                .unwrap_or(usize::MAX);
            ai.cmp(&bi)
                .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });
        Ok(folders)
    }
}

#[derive(Debug, Deserialize)]
struct MeResponse {
    #[serde(default)]
    mail: Option<String>,
    #[serde(default, rename = "userPrincipalName")]
    user_principal_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FoldersResponse {
    #[serde(default)]
    value: Vec<RawFolder>,
}

#[derive(Debug, Deserialize)]
struct RawFolder {
    id: String,
    #[serde(rename = "displayName")]
    display_name: String,
    /// >0 means this folder has subfolders worth recursing into. Graph
    /// returns this on every `mailFolders` row; the BFS uses it to
    /// avoid issuing wasted `/childFolders` calls on leaves.
    #[serde(rename = "childFolderCount", default)]
    child_folder_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    value: Vec<RawMessage>,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    id: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(rename = "receivedDateTime")]
    received_date_time: Option<String>,
    #[serde(default, rename = "isRead")]
    is_read: bool,
    #[serde(default)]
    from: Option<RawFromWrap>,
    #[serde(default, rename = "webLink")]
    web_link: Option<String>,
    #[serde(default)]
    body: Option<RawBody>,
    #[serde(default, rename = "bodyPreview")]
    body_preview: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFromWrap {
    #[serde(rename = "emailAddress", default)]
    email_address: Option<RawFromAddress>,
}

#[derive(Debug, Deserialize)]
struct RawFromAddress {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawBody {
    #[serde(default, rename = "contentType")]
    content_type: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

impl RawMessage {
    fn into_message(self, folder: &str) -> Option<EmailMessage> {
        let received_str = self.received_date_time?;
        let received = parse_graph_datetime(&received_str)?;
        let (from_name, from_address) = match self.from.and_then(|w| w.email_address) {
            Some(a) => (a.name, a.address.unwrap_or_default()),
            None => (None, String::new()),
        };
        let subject = self.subject.unwrap_or_default();
        let plain_body = match self.body {
            Some(b) => {
                let ctype = b.content_type.unwrap_or_default().to_ascii_lowercase();
                let raw = b.content.unwrap_or_default();
                if ctype == "html" {
                    html_to_text(&raw)
                } else if raw.is_empty() {
                    self.body_preview.clone().unwrap_or_default()
                } else {
                    raw
                }
            }
            None => self.body_preview.clone().unwrap_or_default(),
        };
        Some(EmailMessage {
            id: self.id,
            folder: folder.to_string(),
            from_name,
            from_address,
            subject,
            received,
            server_unread: !self.is_read,
            plain_body,
            web_url: self.web_link,
        })
    }
}

/// Graph receivedDateTime is RFC3339 in UTC (e.g. `2026-05-20T16:30:00Z`).
fn parse_graph_datetime(raw: &str) -> Option<DateTime<Local>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.with_timezone(&Local));
    }
    // Fallback for the fractional-second form Graph sometimes emits.
    let head = raw.split('.').next()?.trim_end_matches('Z');
    let naive = NaiveDateTime::parse_from_str(head, "%Y-%m-%dT%H:%M:%S").ok()?;
    Some(Utc.from_utc_datetime(&naive).with_timezone(&Local))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_raw(html: bool, content: &str) -> RawMessage {
        RawMessage {
            id: "id-1".into(),
            subject: Some("Hi".into()),
            received_date_time: Some("2026-05-20T16:30:00Z".into()),
            is_read: false,
            from: Some(RawFromWrap {
                email_address: Some(RawFromAddress {
                    name: Some("Alice".into()),
                    address: Some("alice@example.com".into()),
                }),
            }),
            web_link: Some("https://outlook.office.com/mail/inbox/id/id-1".into()),
            body: Some(RawBody {
                content_type: Some(if html { "html".into() } else { "text".into() }),
                content: Some(content.into()),
            }),
            body_preview: Some("preview".into()),
        }
    }

    #[test]
    fn into_message_passes_through_plain_text() {
        let raw = make_raw(false, "hello world");
        let m = raw.into_message("INBOX").unwrap();
        assert_eq!(m.plain_body, "hello world");
        assert!(m.server_unread);
        assert_eq!(m.from_name.as_deref(), Some("Alice"));
        assert_eq!(m.from_address, "alice@example.com");
        assert_eq!(m.folder, "INBOX");
    }

    #[test]
    fn into_message_strips_html_body() {
        let raw = make_raw(true, "<p>Hello <b>Bob</b></p>");
        let m = raw.into_message("INBOX").unwrap();
        assert_eq!(m.plain_body, "Hello Bob");
    }
}
