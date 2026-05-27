// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! WSJ-specific RSS provider. Mostly mirrors `news::provider::RssProvider`
//! but extracts hero-image URLs from `<media:content>` elements (the
//! news widget's `Article` struct doesn't carry these) and tags each
//! article with the feed's topic label directly — no keyword matching.
//! WSJ already groups articles by topic in their RSS structure, so we
//! honor that grouping verbatim.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Strip rudimentary HTML and decode common entities so RSS
/// `<description>` blobs render readably. Thin re-export of the
/// shared [`crate::text::sanitize_html`] so RSS handling stays
/// consistent across widgets.
fn sanitize_summary(raw: &str) -> String {
    crate::text::sanitize_html(raw)
}

/// One WSJ article. Subset of `news::Article` plus the hero image
/// URL pulled from RSS media elements. Serialized into the widget's
/// cache so a fresh launch can paint immediately while the next fetch
/// completes in the background.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsjArticle {
    pub title: String,
    pub url: String,
    pub topic: String,
    pub source: String,
    pub published: DateTime<Utc>,
    pub summary: Option<String>,
    /// First media URL found in `<media:content>` / `<media:thumbnail>`,
    /// if any. WSJ consistently provides one per article on
    /// `images.wsj.net`. Empty for feeds that omit media (rare for WSJ).
    pub hero_image_url: Option<String>,
    pub authors: Vec<String>,
}

/// A single (topic, feed URL) pair the user has activated.
#[derive(Debug, Clone)]
pub struct WsjFeed {
    pub topic: &'static str,
    pub url: &'static str,
}

/// WSJ's curated catalogue. Each entry: (topic, feed URL). Verified
/// reachable as of May 2026 — `feeds.content.dowjones.io` returns the
/// long-form feed (50-100 items) versus `feeds.a.dj.com` which only
/// surfaces the 20 most-recent. We use the longer form.
pub const WSJ_CATALOGUE: &[(&str, &str)] = &[
    (
        "World",
        "https://feeds.content.dowjones.io/public/rss/RSSWorldNews",
    ),
    (
        "US",
        "https://feeds.content.dowjones.io/public/rss/RSSUSNews",
    ),
    (
        "Business",
        "https://feeds.content.dowjones.io/public/rss/WSJcomUSBusiness",
    ),
    (
        "Markets",
        "https://feeds.content.dowjones.io/public/rss/RSSMarketsMain",
    ),
    (
        "Tech",
        "https://feeds.content.dowjones.io/public/rss/RSSWSJD",
    ),
    (
        "Opinion",
        "https://feeds.content.dowjones.io/public/rss/RSSOpinion",
    ),
    (
        "Politics",
        "https://feeds.content.dowjones.io/public/rss/socialpoliticsfeed",
    ),
    (
        "Economy",
        "https://feeds.content.dowjones.io/public/rss/socialeconomyfeed",
    ),
    (
        "Life",
        "https://feeds.content.dowjones.io/public/rss/RSSLifestyle",
    ),
];

pub struct WsjProvider {
    http: reqwest::Client,
    feeds: Vec<WsjFeed>,
}

impl WsjProvider {
    pub fn new(feeds: Vec<WsjFeed>) -> Self {
        Self {
            http: crate::http::shared(),
            feeds,
        }
    }

    /// Fan-out RSS fetch across every activated feed. Per-feed errors
    /// are logged + skipped; surviving articles are deduplicated by
    /// URL and sorted newest-first.
    pub async fn fetch(&self) -> Vec<WsjArticle> {
        let futs = self.feeds.iter().map(|feed| async move {
            match self.fetch_feed(feed).await {
                Ok(chunk) => chunk,
                Err(err) => {
                    tracing::warn!(
                        topic = %feed.topic,
                        url = %feed.url,
                        error = format!("{err:#}"),
                        "wsj feed fetch failed"
                    );
                    Vec::new()
                }
            }
        });
        let chunks = futures::future::join_all(futs).await;
        let mut all: Vec<WsjArticle> = chunks.into_iter().flatten().collect();
        dedup_by_url(&mut all);
        all.sort_by_key(|a| std::cmp::Reverse(a.published));
        all
    }

    async fn fetch_feed(&self, feed: &WsjFeed) -> Result<Vec<WsjArticle>> {
        let bytes = self
            .http
            .get(feed.url)
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
                "application/rss+xml, application/xml;q=0.9, */*;q=0.5",
            )
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

        let source = parsed
            .title
            .as_ref()
            .map(|t| t.content.clone())
            .unwrap_or_else(|| format!("WSJ: {}", feed.topic));
        let mut articles = Vec::with_capacity(parsed.entries.len());
        for entry in parsed.entries {
            if let Some(a) = entry_to_article(entry, feed.topic, &source) {
                articles.push(a);
            }
        }
        Ok(articles)
    }
}

fn entry_to_article(entry: feed_rs::model::Entry, topic: &str, source: &str) -> Option<WsjArticle> {
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
    // Hero image: walk every MediaObject's content list and grab the
    // first URL we find. WSJ puts one `<media:content>` per item and
    // it's always the article hero on `images.wsj.net`.
    let hero_image_url = entry
        .media
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|c| c.url.as_ref().map(|u| u.to_string()));
    let authors = entry
        .authors
        .into_iter()
        .filter_map(|a| {
            let n = a.name.trim();
            if n.is_empty() {
                None
            } else {
                Some(n.to_string())
            }
        })
        .collect();
    Some(WsjArticle {
        title,
        url,
        topic: topic.to_string(),
        source: source.to_string(),
        published,
        summary,
        hero_image_url,
        authors,
    })
}

fn dedup_by_url(articles: &mut Vec<WsjArticle>) {
    // WSJ syndicates the same article across multiple topic feeds
    // (e.g. an AI piece lands in both World and Tech) sometimes with
    // tracking query strings differing per feed. Strip query + fragment
    // before comparing so the same article doesn't show up twice with
    // different `?mod=` suffixes.
    let mut seen = std::collections::HashSet::new();
    articles.retain(|a| seen.insert(normalize_url(&a.url)));
}

/// Identity for dedup purposes: scheme + host + path, lower-cased.
/// Drops query strings and fragments — those are commonly tracking
/// params that differ across feeds for the same article.
fn normalize_url(raw: &str) -> String {
    let no_frag = raw.split('#').next().unwrap_or(raw);
    let no_query = no_frag.split('?').next().unwrap_or(no_frag);
    // Lowercase scheme + host for case-insensitive dedup; preserve
    // path case since some CMSes are path-case-sensitive.
    match no_query.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            format!(
                "{}://{}/{}",
                scheme.to_ascii_lowercase(),
                host.to_ascii_lowercase(),
                path
            )
        }
        None => no_query.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_has_no_duplicate_topic_labels() {
        let mut seen = std::collections::HashSet::new();
        for (label, _) in WSJ_CATALOGUE {
            assert!(seen.insert(*label), "duplicate topic label: {label}");
        }
    }

    #[test]
    fn dedup_drops_repeats_by_url() {
        let mut v = vec![
            WsjArticle {
                title: "a".into(),
                url: "https://x".into(),
                topic: "World".into(),
                source: "WSJ".into(),
                published: Utc::now(),
                summary: None,
                hero_image_url: None,
                authors: vec![],
            },
            WsjArticle {
                title: "b".into(),
                url: "https://x".into(),
                topic: "Business".into(),
                source: "WSJ".into(),
                published: Utc::now(),
                summary: None,
                hero_image_url: None,
                authors: vec![],
            },
        ];
        dedup_by_url(&mut v);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].title, "a", "first occurrence wins");
    }

    #[test]
    fn dedup_treats_syndicated_articles_as_one() {
        // WSJ adds `?mod=hp_lead_pos1` / `?mod=djemRSS` etc. for the
        // same article across different feeds. Without query-string
        // stripping the article would render twice in the list.
        let url_world = "https://www.wsj.com/world/pope-leo-ai-c5e1af6c?mod=hp_lead_pos1";
        let url_tech = "https://www.wsj.com/world/pope-leo-ai-c5e1af6c?mod=djemRSS";
        let mut v = vec![
            WsjArticle {
                title: "Pope".into(),
                url: url_world.into(),
                topic: "World".into(),
                source: "WSJ World".into(),
                published: Utc::now(),
                summary: None,
                hero_image_url: None,
                authors: vec![],
            },
            WsjArticle {
                title: "Pope".into(),
                url: url_tech.into(),
                topic: "Tech".into(),
                source: "WSJ Tech".into(),
                published: Utc::now(),
                summary: None,
                hero_image_url: None,
                authors: vec![],
            },
        ];
        dedup_by_url(&mut v);
        assert_eq!(v.len(), 1, "syndicated article should appear once");
    }

    #[test]
    fn normalize_url_strips_query_and_fragment() {
        assert_eq!(
            normalize_url("https://www.wsj.com/a/b?x=1#section"),
            "https://www.wsj.com/a/b"
        );
        assert_eq!(
            normalize_url("https://WWW.WSJ.COM/A/B?x=1"),
            "https://www.wsj.com/A/B",
            "scheme + host lowercased, path case preserved"
        );
    }
}
