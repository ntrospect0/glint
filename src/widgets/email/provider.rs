//! Common types shared by both Email providers (Gmail + Outlook). The widget
//! talks to providers exclusively through this trait so adding a third
//! provider (IMAP, JMAP, …) later is a strictly additive change.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

/// A single normalized email message. Provider-specific bodies and headers
/// are reduced to plain text before reaching the widget; everything renderable
/// is on this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailMessage {
    /// Provider-specific id. Used as the key in the local seen-store and as
    /// the trailing segment of `web_url` (for Gmail).
    pub id: String,
    /// Which folder this message was fetched from. The widget uses this to
    /// group messages under the active folder tab.
    pub folder: String,
    pub from_name: Option<String>,
    pub from_address: String,
    pub subject: String,
    /// Receive time in the user's local zone — providers return UTC; we
    /// normalize at the boundary so the render path can do `%H:%M` /
    /// `%m/%d` formatting without doing TZ math.
    pub received: DateTime<Local>,
    /// Server-side unread state. The widget OR's this with the local
    /// seen-store to decide which messages still warrant the `●` indicator.
    pub server_unread: bool,
    /// Plain-text body. When the source was HTML-only, this is the output of
    /// `html_strip::html_to_text`.
    pub plain_body: String,
    /// Direct URL into the provider's web UI for this message, if available.
    /// Gmail: built from the id. Outlook: comes from Graph's `webLink`. IMAP
    /// (future) will be `None` — there's no canonical web URL for raw IMAP.
    pub web_url: Option<String>,
}

/// One folder / label in the user's mailbox. `id` is what the provider
/// expects on its API; `label` is what we show in the tab bar.
#[derive(Debug, Clone)]
#[allow(dead_code)] // surfaced when folder picker UI lands.
pub struct EmailFolder {
    pub label: String,
    pub id: String,
}

/// Read-only email source. v1 has two implementations: Gmail and Outlook.
#[async_trait]
pub trait EmailProvider: Send + Sync {
    /// List the folders/labels available on the account. Used by `--setup`
    /// and future UI to help the user pick which folders to follow.
    #[allow(dead_code)] // surfaced by the wizard / folder picker later.
    async fn list_folders(&self) -> Result<Vec<EmailFolder>>;

    /// Fetch recent messages from a single folder. `since` is a hard
    /// lower bound on `receivedDateTime`; `max` is the hard upper bound
    /// on returned count (caller passes 100).
    async fn fetch_recent(
        &self,
        folder: &str,
        since: DateTime<Utc>,
        max: usize,
    ) -> Result<Vec<EmailMessage>>;

    /// Static label used as the bracketed source tag in the widget title
    /// (e.g. "gmail", "outlook"). The widget builds its own label from the
    /// configured provider name; this method is for diagnostics / future
    /// auto-detection use cases.
    #[allow(dead_code)]
    fn provider_label(&self) -> &str;

    /// Account address (user's primary email) — fetched lazily on the first
    /// successful refresh and cached. Returns `None` before that round-trip
    /// has resolved, in which case the widget shows "(loading…)".
    /// Callers use the concrete `cached_account()` method on each provider
    /// implementation instead, because returning `&str` from behind a Mutex
    /// isn't safely expressible here.
    #[allow(dead_code)]
    fn account_address(&self) -> Option<&str>;
}
