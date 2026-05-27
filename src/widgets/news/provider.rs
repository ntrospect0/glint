use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// One normalized item across RSS/Atom/JSON feeds.
#[derive(Debug, Clone)]
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

    #[allow(dead_code)] // surfaced in status bar in later phases.
    fn name(&self) -> &str;
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedConfig {
    pub label: String,
    pub url: String,
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
        let http = reqwest::Client::builder()
            .user_agent(concat!("glint-tui/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(12))
            .build()
            .context("failed to build news HTTP client")?;
        Ok(Self {
            http,
            feeds,
            topics,
        })
    }

    async fn fetch_feed(&self, feed: &FeedConfig) -> Result<Vec<Article>> {
        let bytes = self
            .http
            .get(&feed.url)
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
        let mut all: Vec<Article> = Vec::new();
        for feed in &self.feeds {
            match self.fetch_feed(feed).await {
                Ok(mut chunk) => all.append(&mut chunk),
                Err(err) => {
                    tracing::warn!(feed = %feed.label, error = %err, "news feed fetch failed");
                }
            }
        }
        dedup_by_url(&mut all);
        all.sort_by_key(|a| std::cmp::Reverse(a.published));
        Ok(all)
    }

    fn name(&self) -> &str {
        "rss"
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
    let published = entry
        .published
        .or(entry.updated)
        .unwrap_or_else(Utc::now);
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

/// Strip rudimentary HTML, decode common entities (`&amp;`, `&#8217;`, etc.),
/// and collapse whitespace so RSS `<description>` blobs render readably.
fn sanitize_summary(raw: &str) -> String {
    decode_entities(&strip_tags(raw))
}

fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    let mut prev_was_space = false;
    for ch in raw.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                if !prev_was_space {
                    out.push(' ');
                    prev_was_space = true;
                }
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch.is_whitespace() {
            if !prev_was_space {
                out.push(' ');
                prev_was_space = true;
            }
        } else {
            out.push(ch);
            prev_was_space = false;
        }
    }
    out.trim().to_string()
}

/// Decode HTML entities: numeric (`&#NNNN;`, `&#xHHHH;`) and the common named
/// ones. Unknown entities are left intact so we don't accidentally garble text.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let mut buf = String::new();
        let mut closed = false;
        // Read up to 10 chars looking for the trailing ';'.
        for _ in 0..10 {
            match chars.peek() {
                Some(';') => {
                    chars.next();
                    closed = true;
                    break;
                }
                Some(&nc) if nc.is_ascii_alphanumeric() || nc == '#' => {
                    buf.push(nc);
                    chars.next();
                }
                _ => break,
            }
        }
        if !closed {
            out.push('&');
            out.push_str(&buf);
            continue;
        }
        match lookup_entity(&buf) {
            Some(ch) => out.push(ch),
            None => {
                out.push('&');
                out.push_str(&buf);
                out.push(';');
            }
        }
    }
    out
}

fn lookup_entity(entity: &str) -> Option<char> {
    if let Some(rest) = entity.strip_prefix('#') {
        let (radix, digits) = if let Some(hex) = rest.strip_prefix(['x', 'X']) {
            (16, hex)
        } else {
            (10, rest)
        };
        let n = u32::from_str_radix(digits, radix).ok()?;
        return char::from_u32(n);
    }
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201C}',
        "rdquo" => '\u{201D}',
        _ => return None,
    })
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
