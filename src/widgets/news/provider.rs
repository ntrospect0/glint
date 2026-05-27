// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One normalized item across RSS/Atom/JSON feeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Article {
    pub title: String,
    pub url: String,
    pub source: String,
    pub published: DateTime<Utc>,
    pub summary: Option<String>,
    /// Topic labels this article matched. Empty when no topic config matches.
    pub topics: Vec<String>,
}

#[async_trait]
pub trait NewsProvider: Send + Sync {
    async fn fetch(&self) -> Result<Vec<Article>>;
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedConfig {
    pub label: String,
    pub url: String,
    /// Per-feed override of `[news] fetch_body_for_summary` in
    /// `news.toml`. When `Some(true)`, pressing `s` on an article from
    /// this feed HTTP-fetches the article page and extracts the
    /// readable body before handing it to the LLM. When `Some(false)`,
    /// the LLM only ever sees the RSS excerpt for this feed (useful
    /// for feeds whose `<description>` is already the full article,
    /// like Phoronix or HN, or for paywalled sources where the body
    /// fetch would just return the paywall page). `None` falls back
    /// to the widget-wide default.
    #[serde(default)]
    pub fetch_body: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Topic {
    pub label: String,
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// Returns the topic labels whose keywords match anywhere in `title` or
/// `summary` (case-insensitive substring).
pub fn match_topics(title: &str, summary: Option<&str>, topics: &[Topic]) -> Vec<String> {
    if topics.is_empty() {
        return Vec::new();
    }
    let haystack_lower = {
        let mut s = title.to_lowercase();
        if let Some(sm) = summary {
            s.push(' ');
            s.push_str(&sm.to_lowercase());
        }
        s
    };
    let mut out = Vec::new();
    for topic in topics {
        for keyword in &topic.keywords {
            if keyword.is_empty() {
                continue;
            }
            if haystack_lower.contains(&keyword.to_lowercase()) {
                out.push(topic.label.clone());
                break;
            }
        }
    }
    out
}

pub struct RssProvider {
    http: reqwest::Client,
    feeds: Vec<FeedConfig>,
    topics: Vec<Topic>,
}

impl RssProvider {
    pub fn new(feeds: Vec<FeedConfig>, topics: Vec<Topic>) -> Result<Self> {
        let http = crate::http::shared();
        Ok(Self {
            http,
            feeds,
            topics,
        })
    }

    async fn fetch_feed(&self, feed: &FeedConfig) -> Result<Vec<Article>> {
        // Per-request browser-shaped headers. Cloudflare's lighter bot-
        // detection modes look at the full request shape, not just the
        // User-Agent — sending only UA + Accept passes some sites
        // (Bloomberg, Reuters) but fails others that require the
        // `Sec-Fetch-*` and Upgrade-Insecure-Requests headers a real
        // browser sends on top-level navigations. We mirror what
        // Firefox sends for a feed click.
        //
        // Note: this can't defeat *TLS fingerprinting* (Cloudflare's
        // "Bot Fight Mode" with JA3/JA4). Sites that detect Rust's
        // rustls handshake signature reject the request before any
        // header is read. The reliable workaround there is to remove
        // the feed and use an alternative source.
        let bytes = self
            .http
            .get(&feed.url)
            .header(
                reqwest::header::USER_AGENT,
                concat!(
                    "Mozilla/5.0 (compatible; glint-tui/",
                    env!("CARGO_PKG_VERSION"),
                    "; +https://github.com/ntrospect0/glint) Gecko/20100101 Firefox/120.0",
                ),
            )
            .header(
                reqwest::header::ACCEPT,
                "application/rss+xml, application/atom+xml, application/xml;q=0.9, \
                 text/xml;q=0.8, application/json;q=0.7, */*;q=0.5",
            )
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            // `gzip`/`br`/`deflate` are negotiated by reqwest's compression
            // features (enabled in Cargo.toml). Without explicit
            // Accept-Encoding some servers assume "no compression support
            // == bot" and 4xx the request.
            .header(reqwest::header::ACCEPT_ENCODING, "gzip, br, deflate")
            // Real browsers always send these on top-level navigations.
            // Cloudflare's Bot Management feature flags requests missing
            // any of them as automation candidates.
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Sec-Fetch-User", "?1")
            .header(reqwest::header::UPGRADE_INSECURE_REQUESTS, "1")
            .send()
            .await
            .with_context(|| format!("GET {} failed", feed.url))?
            .error_for_status()
            .with_context(|| format!("{} returned non-2xx", feed.url))?
            .bytes()
            .await
            .with_context(|| format!("reading {} body failed", feed.url))?;
        let parsed = feed_rs::parser::parse(bytes.as_ref())
            .with_context(|| format!("parsing {} failed", feed.url))?;

        let mut articles = Vec::with_capacity(parsed.entries.len());
        for entry in parsed.entries {
            let Some(article) = entry_to_article(entry, feed, &self.topics) else {
                continue;
            };
            articles.push(article);
        }
        Ok(articles)
    }
}

#[async_trait]
impl NewsProvider for RssProvider {
    async fn fetch(&self) -> Result<Vec<Article>> {
        // Fan out one HTTP request per feed in parallel — sequential fetching
        // over a dozen+ feeds was visibly slow on refresh. Per-feed errors
        // are logged and the rest of the batch still lands.
        let futs = self.feeds.iter().map(|feed| async move {
            match self.fetch_feed(feed).await {
                Ok(chunk) => chunk,
                Err(err) => {
                    // `{:#}` prints the full anyhow Error chain — without
                    // it the inner cause (TLS handshake failure, DNS NXDOMAIN,
                    // connection reset, etc.) is hidden behind the top-level
                    // `with_context` message. That's exactly the info you
                    // need to tell "this feed is bot-blocked" apart from
                    // "this feed's host went down."
                    tracing::warn!(
                        feed = %feed.label,
                        url = %feed.url,
                        error = format!("{err:#}"),
                        "news feed fetch failed"
                    );
                    Vec::new()
                }
            }
        });
        let chunks = futures::future::join_all(futs).await;
        let mut all: Vec<Article> = chunks.into_iter().flatten().collect();
        dedup_by_url(&mut all);
        all.sort_by_key(|a| std::cmp::Reverse(a.published));
        Ok(all)
    }
}

fn entry_to_article(
    entry: feed_rs::model::Entry,
    feed: &FeedConfig,
    topics: &[Topic],
) -> Option<Article> {
    let title = entry.title.map(|t| t.content).unwrap_or_default();
    if title.is_empty() {
        return None;
    }
    let url = entry
        .links
        .iter()
        .find(|l| !l.href.is_empty())
        .map(|l| l.href.clone())?;
    let published = entry.published.or(entry.updated).unwrap_or_else(Utc::now);
    let summary = entry
        .summary
        .map(|s| s.content)
        .filter(|s| !s.trim().is_empty())
        .map(|s| sanitize_summary(&s));
    let matched = match_topics(&title, summary.as_deref(), topics);
    Some(Article {
        title,
        url,
        source: feed.label.clone(),
        published,
        summary,
        topics: matched,
    })
}

/// Strip rudimentary HTML and decode common entities so RSS
/// `<description>` blobs render readably. Thin re-export over the
/// canonical [`crate::text::sanitize_html`] so the body-extraction
/// path in `mod.rs` keeps the same name it's always used.
pub(super) fn sanitize_summary(raw: &str) -> String {
    crate::text::sanitize_html(raw)
}

fn dedup_by_url(articles: &mut Vec<Article>) {
    let mut seen = std::collections::HashSet::new();
    articles.retain(|a| seen.insert(a.url.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_matching_is_case_insensitive_and_substring() {
        let topics = vec![Topic {
            label: "Tech".into(),
            keywords: vec!["OpenAI".into(), "rust".into()],
        }];
        let m = match_topics("OPENAI raises…", None, &topics);
        assert_eq!(m, vec!["Tech".to_string()]);
        let m2 = match_topics("Why we love Rust at Foo", None, &topics);
        assert_eq!(m2, vec!["Tech".to_string()]);
    }

    #[test]
    fn topic_matching_returns_each_matched_label_once() {
        let topics = vec![
            Topic {
                label: "Tech".into(),
                keywords: vec!["AI".into(), "Linux".into()],
            },
            Topic {
                label: "Finance".into(),
                keywords: vec!["Fed".into()],
            },
        ];
        let m = match_topics("Linux foundation announces AI initiative", None, &topics);
        // Linux *and* AI both match Tech — should only appear once.
        assert_eq!(m, vec!["Tech".to_string()]);
    }

    #[test]
    fn topic_matching_returns_empty_when_no_topics_configured() {
        let topics: Vec<Topic> = Vec::new();
        let m = match_topics("anything", Some("anything"), &topics);
        assert!(m.is_empty());
    }

    #[test]
    fn sanitize_summary_strips_simple_html_and_collapses_whitespace() {
        let raw = "<p>Hello,  <b>world</b>!\nNext\tline.</p>";
        assert_eq!(sanitize_summary(raw), "Hello, world ! Next line.");
    }

    #[test]
    fn sanitize_summary_decodes_numeric_and_named_entities() {
        let raw = "Trump &#8217;s plan &amp; the &#8220;deal&#8221; &mdash; today";
        let expected = "Trump \u{2019}s plan & the \u{201C}deal\u{201D} \u{2014} today";
        assert_eq!(sanitize_summary(raw), expected);
    }

    #[test]
    fn sanitize_summary_handles_hex_entities() {
        assert_eq!(sanitize_summary("AT&#x26;T"), "AT&T");
    }

    #[test]
    fn sanitize_summary_leaves_unknown_entities_intact() {
        assert_eq!(sanitize_summary("foo &bogus; bar"), "foo &bogus; bar");
    }

    #[test]
    fn sanitize_summary_handles_unterminated_amp() {
        assert_eq!(sanitize_summary("rock & roll"), "rock & roll");
    }

    #[test]
    fn dedup_keeps_first_occurrence() {
        let mut v = vec![
            Article {
                title: "a".into(),
                url: "https://x".into(),
                source: "f1".into(),
                published: Utc::now(),
                summary: None,
                topics: vec![],
            },
            Article {
                title: "b".into(),
                url: "https://x".into(),
                source: "f2".into(),
                published: Utc::now(),
                summary: None,
                topics: vec![],
            },
        ];
        dedup_by_url(&mut v);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].source, "f1");
    }
}
