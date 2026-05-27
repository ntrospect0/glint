//! IMAP-backed email provider. Sits alongside [`gmail::GmailProvider`]
//! and [`outlook::OutlookEmailProvider`]; users who don't want to deal
//! with OAuth (or whose provider isn't Google/Microsoft) point glint
//! at any IMAP server with an app-specific password.
//!
//! The `imap` crate is synchronous; we wrap every call in
//! [`tokio::task::spawn_blocking`] so the wider async runtime keeps
//! moving. Each operation opens a fresh connection — IMAP servers
//! routinely close idle connections, and re-establishing for a one-off
//! 100-message fetch every five minutes is well within polite usage.
//!
//! Tested against: Gmail (`imap.gmail.com:993` with an app password),
//! iCloud (`imap.mail.me.com:993`), Fastmail
//! (`imap.fastmail.com:993`). Other providers should work if they
//! speak IMAP4rev1 + STARTTLS or implicit TLS.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use super::html_strip;
use super::provider::{EmailMessage, EmailProvider};

/// Credentials + connection config for a single IMAP account, loaded
/// from `~/.config/glint/credentials/imap.toml` (or
/// `imap_<instance>.toml` for multi-instance email).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImapCredentials {
    /// IMAP server hostname (e.g. `imap.gmail.com`).
    pub host: String,
    /// IMAP port. 993 for implicit TLS (almost always); 143 for plaintext +
    /// STARTTLS (some self-hosted setups).
    #[serde(default = "default_port")]
    pub port: u16,
    /// `true` (default) → connect with implicit TLS on the configured
    /// port. `false` → connect plaintext (mostly useful for local
    /// testing; not recommended over the network).
    #[serde(default = "default_use_tls")]
    pub use_tls: bool,
    /// Account username — typically the full email address.
    pub username: String,
    /// App-specific password. **Not** the account's primary password
    /// — generate one in the provider's security settings (Gmail with
    /// 2FA, iCloud, Fastmail all support this).
    pub app_password: String,
}

fn default_port() -> u16 {
    993
}
fn default_use_tls() -> bool {
    true
}

impl ImapCredentials {
    /// Sensible defaults for the credentials capture page. Users can
    /// edit any field after the seed; we don't enforce server URLs.
    pub fn template_for(preset: &str) -> Option<Self> {
        let (host, port) = match preset {
            "gmail" => ("imap.gmail.com", 993),
            "icloud" => ("imap.mail.me.com", 993),
            "fastmail" => ("imap.fastmail.com", 993),
            "yahoo" => ("imap.mail.yahoo.com", 993),
            "outlook" => ("outlook.office365.com", 993),
            _ => return None,
        };
        Some(Self {
            host: host.into(),
            port,
            use_tls: true,
            username: String::new(),
            app_password: String::new(),
        })
    }
}

/// Concrete IMAP provider used by the Email widget.
pub struct ImapProvider {
    creds: ImapCredentials,
    /// Last successful account address. For IMAP this is just the
    /// configured username — there's no separate `/me` endpoint —
    /// but the indirection keeps the provider interface uniform.
    account: Arc<Mutex<Option<String>>>,
}

impl ImapProvider {
    pub fn new(creds: ImapCredentials) -> Self {
        // The account address is the configured username; surface it
        // immediately so the widget title row paints with no "(loading…)"
        // window. Other providers populate this from /me asynchronously.
        let initial = if creds.username.trim().is_empty() {
            None
        } else {
            Some(creds.username.clone())
        };
        Self {
            creds,
            account: Arc::new(Mutex::new(initial)),
        }
    }

    pub fn cached_account(&self) -> Option<String> {
        self.account
            .lock()
            .expect("imap account cache poisoned")
            .clone()
    }

    /// Mirror of the OAuth providers' seed method. For IMAP this is
    /// largely a no-op — `new` already seeds from `username` — but
    /// the EmailProviderHandle dispatcher calls it uniformly.
    pub fn seed_account_cache(&self, address: &str) {
        let mut guard = self.account.lock().expect("imap account cache poisoned");
        if guard.is_none() {
            *guard = Some(address.to_string());
        }
    }

    /// Public folder-listing entry point for the wizard's picker.
    /// Issues `LIST "" "*"` against the server. Sub-folders are joined
    /// by the server's hierarchy delimiter (usually `.` or `/`) — we
    /// preserve the raw name so the user can paste it into email.toml
    /// verbatim.
    pub async fn list_folders_for_picker(&self) -> Result<Vec<(String, String)>> {
        let creds = self.creds.clone();
        tokio::task::spawn_blocking(move || list_folders_sync(&creds))
            .await
            .context("imap folder-list task panicked")?
    }
}

fn list_folders_sync(creds: &ImapCredentials) -> Result<Vec<(String, String)>> {
    let mut session = connect_concrete(creds)?;
    let mailboxes = session
        .list(Some(""), Some("*"))
        .context("imap: LIST failed")?;
    let mut out: Vec<(String, String)> = Vec::new();
    for mb in mailboxes.iter() {
        let name = mb.name().to_string();
        // Skip the "\Noselect" placeholder mailboxes (hierarchy nodes
        // that don't hold messages on their own).
        if mb
            .attributes()
            .iter()
            .any(|a| matches!(a, imap::types::NameAttribute::NoSelect))
        {
            continue;
        }
        out.push((name.clone(), name));
    }
    let _ = session.logout();
    // Stable order: priority list first, then alphabetical for the
    // rest. INBOX is special-cased to always lead.
    let priority: &[&str] = &[
        "INBOX", "Sent", "Sent Items", "Drafts", "Archive", "Junk",
        "Spam", "Trash", "Deleted Messages",
    ];
    out.sort_by(|a, b| {
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
    Ok(out)
}

type ConcreteSession = imap::Session<native_tls::TlsStream<std::net::TcpStream>>;

fn connect_concrete(creds: &ImapCredentials) -> Result<ConcreteSession> {
    let tls = native_tls::TlsConnector::builder()
        .build()
        .context("imap: failed to build TLS connector")?;
    let client = imap::connect(
        (creds.host.as_str(), creds.port),
        &creds.host,
        &tls,
    )
    .with_context(|| {
        format!("imap: TLS connect to {}:{} failed", creds.host, creds.port)
    })?;
    client
        .login(&creds.username, &creds.app_password)
        .map_err(|(err, _client)| {
            anyhow!("imap: login failed for {}: {err}", creds.username)
        })
}

fn fetch_recent_sync(
    creds: &ImapCredentials,
    folder: &str,
    since: DateTime<Utc>,
    max: usize,
) -> Result<Vec<EmailMessage>> {
    let mut session = connect_concrete(creds)?;
    let mailbox = session
        .select(folder)
        .with_context(|| format!("imap: SELECT {folder:?} failed"))?;
    if mailbox.exists == 0 {
        let _ = session.logout();
        return Ok(Vec::new());
    }

    // IMAP SEARCH SINCE uses a date string in the form "1-Jan-2026".
    let since_str = since.format("%-d-%b-%Y").to_string();
    let search_query = format!("SINCE {since_str}");
    let uids = session
        .uid_search(&search_query)
        .with_context(|| format!("imap: UID SEARCH {search_query:?} failed"))?;
    if uids.is_empty() {
        let _ = session.logout();
        return Ok(Vec::new());
    }

    // Sort newest first by UID (IMAP UIDs increase monotonically) and
    // take the latest `max`. Fetching the full body for every match in
    // a 30-day window would be wasteful; the widget only needs a small
    // window of recent.
    let mut uids: Vec<u32> = uids.into_iter().collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    uids.truncate(max);
    let uid_set = uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let fetches = session
        .uid_fetch(&uid_set, "(UID FLAGS INTERNALDATE BODY[])")
        .context("imap: UID FETCH failed")?;

    let mut out: Vec<EmailMessage> = Vec::with_capacity(uids.len());
    for fetched in fetches.iter() {
        let Some(body) = fetched.body() else {
            continue;
        };
        let parser = mail_parser::MessageParser::new();
        let Some(msg) = parser.parse(body) else {
            continue;
        };
        let subject = msg.subject().unwrap_or_default().to_string();
        let (from_name, from_address) = msg
            .from()
            .and_then(|h| h.first())
            .map(|addr| {
                (
                    addr.name().map(str::to_string),
                    addr.address().unwrap_or_default().to_string(),
                )
            })
            .unwrap_or_else(|| (None, String::new()));
        let received = match fetched.internal_date() {
            Some(dt) => dt.with_timezone(&Local),
            None => Local::now(),
        };
        // Prefer text/plain; fall back to text/html stripped to plain.
        let plain_body = msg
            .body_text(0)
            .map(|c| c.to_string())
            .or_else(|| {
                msg.body_html(0)
                    .map(|c| html_strip::html_to_text(c.as_ref()))
            })
            .unwrap_or_default();
        let server_unread = !fetched
            .flags()
            .iter()
            .any(|f| matches!(f, imap::types::Flag::Seen));
        out.push(EmailMessage {
            id: format!("imap-{}-{}", folder, fetched.uid.unwrap_or(0)),
            folder: folder.to_string(),
            from_name,
            from_address,
            subject,
            received,
            server_unread,
            plain_body,
            web_url: None,
        });
    }

    // We sorted UIDs newest-first; the server may have returned them
    // in any order. Re-sort by received so the widget renders newest
    // at the top.
    out.sort_by_key(|m| std::cmp::Reverse(m.received));
    let _ = session.logout();
    Ok(out)
}

#[async_trait]
impl EmailProvider for ImapProvider {
    async fn list_folders(&self) -> Result<Vec<super::provider::EmailFolder>> {
        let pairs = self.list_folders_for_picker().await?;
        Ok(pairs
            .into_iter()
            .map(|(id, label)| super::provider::EmailFolder { id, label })
            .collect())
    }

    async fn fetch_recent(
        &self,
        folder: &str,
        since: DateTime<Utc>,
        max: usize,
    ) -> Result<Vec<EmailMessage>> {
        let creds = self.creds.clone();
        let folder = folder.to_string();
        tokio::task::spawn_blocking(move || {
            fetch_recent_sync(&creds, &folder, since, max)
        })
        .await
        .context("imap fetch task panicked")?
    }

    fn provider_label(&self) -> &str {
        "imap"
    }

    fn account_address(&self) -> Option<&str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_for_known_provider_seeds_host_port() {
        let g = ImapCredentials::template_for("gmail").unwrap();
        assert_eq!(g.host, "imap.gmail.com");
        assert_eq!(g.port, 993);
        assert!(g.use_tls);
        assert_eq!(g.username, "");
        assert_eq!(g.app_password, "");

        let i = ImapCredentials::template_for("icloud").unwrap();
        assert_eq!(i.host, "imap.mail.me.com");

        let f = ImapCredentials::template_for("fastmail").unwrap();
        assert_eq!(f.host, "imap.fastmail.com");
    }

    #[test]
    fn template_for_unknown_provider_returns_none() {
        assert!(ImapCredentials::template_for("nopenope").is_none());
    }

    #[test]
    fn new_seeds_account_cache_from_username() {
        let creds = ImapCredentials {
            host: "imap.example.com".into(),
            port: 993,
            use_tls: true,
            username: "alice@example.com".into(),
            app_password: "abcd-efgh".into(),
        };
        let p = ImapProvider::new(creds);
        assert_eq!(p.cached_account().as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn new_with_empty_username_leaves_account_unset() {
        let creds = ImapCredentials {
            host: "imap.example.com".into(),
            port: 993,
            use_tls: true,
            username: "".into(),
            app_password: "abcd-efgh".into(),
        };
        let p = ImapProvider::new(creds);
        assert!(p.cached_account().is_none());
    }
}
