// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the news widget. Split out of `mod.rs` per the repo standard.

use super::*;

fn article(url: &str, title: &str, secs_ago: i64) -> Arc<Article> {
    Arc::new(Article {
        title: title.into(),
        url: url.into(),
        source: "TestFeed".into(),
        published: Utc::now() - chrono::Duration::seconds(secs_ago),
        summary: Some("a short summary".into()),
        topics: vec![],
    })
}

fn tagged_article(url: &str, title: &str, topics: &[&str]) -> Arc<Article> {
    Arc::new(Article {
        title: title.into(),
        url: url.into(),
        source: "TestFeed".into(),
        published: Utc::now(),
        summary: Some("a short summary".into()),
        topics: topics.iter().map(|t| t.to_string()).collect(),
    })
}

#[test]
fn salvage_paragraphs_returns_none_for_marker_only_pages_with_no_real_p() {
    // No marker, no <p> tags long enough to clear the salvage floor.
    let html = "<html><body><article><p>short.</p></article></body></html>";
    assert!(salvage_paragraphs(html).is_none());
}

#[test]
fn salvage_paragraphs_handles_bbc_style_marker_blocks() {
    // Mimics BBC News: each paragraph wrapped in its own
    // `data-component="text-block"` sibling, with 3 levels of
    // styled-component divs in between. Marker-anchored strategy
    // should pull all three paragraphs out and skip the headline.
    let html = r#"
        <html><body>
        <div data-component="headline-block"><h1>Title</h1></div>
        <div data-component="text-block" class="x"><div class="y"><div class="z"><p class="q">First paragraph of the article.</p></div></div></div>
        <div data-component="text-block" class="x"><div class="y"><div class="z"><p class="q">Second &amp; deeper paragraph.</p></div></div></div>
        <div data-component="text-block" class="x"><div class="y"><div class="z"><p class="q">Third with <a href="/foo">a link</a> inside.</p></div></div></div>
        </body></html>
    "#;
    let out = salvage_paragraphs(html).expect("should extract");
    assert!(out.contains("First paragraph of the article."));
    assert!(out.contains("Second & deeper paragraph."));
    assert!(out.contains("Third with a link inside."));
    assert!(out.contains("\n\n"));
}

#[test]
fn salvage_paragraphs_falls_back_to_p_tags_without_known_markers() {
    // No publisher-specific markers — just plain <p> tags scattered
    // through a React-shaped DOM. Generic-salvage strategy should
    // catch the editorial content while dropping short UI strings
    // and the run-on footer-menu nav blob.
    let html = r#"
        <html><body>
        <nav><p>Home</p><p>About</p></nav>
        <main>
          <div><div><p>This is a substantial article paragraph with several words and punctuation, so it clears the salvage filters easily.</p></div></div>
          <div><div><p>Another reasonably long paragraph that should be preserved verbatim and joined with the first one for the LLM.</p></div></div>
        </main>
        <footer><p>HomeNewsSportBusinessTechnologyHealthCultureWeatherShopBritBoxBBCInOtherLanguages</p></footer>
        </body></html>
    "#;
    let out = salvage_paragraphs(html).expect("should extract");
    assert!(out.contains("substantial article paragraph"));
    assert!(out.contains("Another reasonably long paragraph"));
    // Short UI <p>s (Home, About) filtered by length floor.
    assert!(!out.contains("Home\n") && !out.starts_with("Home"));
    // Concatenated menu blob filtered by the nav-run heuristic.
    assert!(!out.contains("HomeNewsSportBusiness"));
}

#[test]
fn looks_like_navigation_run_flags_concatenated_menus_but_not_prose() {
    assert!(looks_like_navigation_run(
        "HomeNewsSportBusinessTechnologyHealthCulture"
    ));
    assert!(!looks_like_navigation_run(
        "The president signed the bill on Tuesday, citing record support across both chambers."
    ));
    // Mixed: nav blob with a couple of trailing words. Still flags
    // because the per-space ratio is dominated by the run-on.
    assert!(looks_like_navigation_run(
        "HomeNewsSportBusinessTechnologyHealthCultureArtsTravel News"
    ));
}

#[test]
fn normalize_extracted_text_collapses_blank_runs_and_trims() {
    // Trailing whitespace per line is removed, runs of 3+ newlines
    // collapse to 2, and leading/trailing whitespace on the whole
    // string is trimmed. Per-line leading whitespace is intentionally
    // preserved (Readability sometimes uses it for list indentation).
    let raw = "Para one.   \n\n\n\nPara two.  \n\n\nPara three.\n\n".to_string();
    let out = normalize_extracted_text(raw);
    assert_eq!(out, "Para one.\n\nPara two.\n\nPara three.");
}

/// `s` on a non-All filter tab used to read `st.articles[selected]`,
/// but `selected` indexes into the *filtered* view — so the URL we
/// pinned, the body we fetched, and the summary we requested were
/// all for some other article (the Nth in the full list). The
/// visible row never updated and `s` looked dead. Verify that
/// after `s` fires on the Tech tab, `summary_view` carries the
/// URL of the *visible filtered* article, not the underlying
/// full-list article at the same index.
#[tokio::test]
async fn toggle_summary_uses_filtered_article_not_full_list_index() {
    use std::sync::atomic::AtomicUsize;

    // Fake LLM — must exist for `toggle_summary_view` to proceed
    // past the `self.llm.is_none()` bail. It won't actually be
    // called in this test because we only check sync state.
    struct UnusedLlm {
        _calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl LlmProvider for UnusedLlm {
        async fn complete(&self, _request: LlmRequest) -> Result<crate::llm::LlmResponse> {
            unreachable!("LLM should not be called during this test")
        }
    }
    let cfg = NewsConfig {
        topics: vec![provider::Topic {
            label: "Tech".into(),
            keywords: vec!["AI".into()],
        }],
        ..NewsConfig::default()
    };
    let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm {
        _calls: Arc::new(AtomicUsize::new(0)),
    });
    let mut w = NewsWidget::with_config_and_llm(
        "main".into(),
        cfg,
        Some(llm),
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    );
    {
        let mut st = w.state.lock().unwrap();
        st.articles = vec![
            tagged_article("https://full-0", "Non-tech 0", &[]),
            tagged_article("https://full-1-tech", "Tech 1", &["Tech"]),
            tagged_article("https://full-2", "Non-tech 2", &[]),
            tagged_article("https://full-3-tech", "Tech 3", &["Tech"]),
            tagged_article("https://full-4", "Non-tech 4", &[]),
        ];
        // Tech tab → filtered list is [full-1-tech, full-3-tech].
        // selected = 0 → filtered first item = full-1-tech.
        // The OLD bug would read st.articles[0] = full-0 (non-tech).
        st.active_filter_idx = 1;
        st.selected = 0;
    }
    w.toggle_summary_view();
    let st = w.state.lock().unwrap();
    assert!(
        st.summary_view.contains_key("https://full-1-tech"),
        "summary_view should be keyed by the filtered article's URL, got: {:?}",
        st.summary_view.keys().collect::<Vec<_>>()
    );
    assert!(
        !st.summary_view.contains_key("https://full-0"),
        "must NOT mutate the underlying full-list article at the same index"
    );
}

#[test]
fn move_selection_respects_active_filter_bounds() {
    let cfg = NewsConfig {
        topics: vec![provider::Topic {
            label: "Tech".into(),
            keywords: vec!["AI".into()],
        }],
        ..NewsConfig::default()
    };
    let mut w = NewsWidget::with_config(cfg);
    {
        let mut st = w.state.lock().unwrap();
        st.articles = vec![
            tagged_article("https://a", "Non-tech A", &[]),
            tagged_article("https://b", "Tech B", &["Tech"]),
            tagged_article("https://c", "Non-tech C", &[]),
            tagged_article("https://d", "Tech D", &["Tech"]),
            tagged_article("https://e", "Non-tech E", &[]),
        ];
        st.active_filter_idx = 1; // Tech (filter_tabs[1])
    }
    // Filter shows 2 articles (B, D). move_selection should clamp to 0..=1.
    w.move_selection(99);
    assert_eq!(w.state.lock().unwrap().selected, 1);
    w.move_selection(-99);
    assert_eq!(w.state.lock().unwrap().selected, 0);
}

#[test]
fn move_selection_clamps_to_bounds() {
    let mut w = NewsWidget::with_config(NewsConfig::default());
    {
        let mut st = w.state.lock().unwrap();
        st.articles = vec![
            article("https://a", "A", 0),
            article("https://b", "B", 0),
            article("https://c", "C", 0),
        ];
    }
    w.move_selection(-5);
    assert_eq!(w.state.lock().unwrap().selected, 0);
    w.move_selection(99);
    assert_eq!(w.state.lock().unwrap().selected, 2);
}

#[test]
fn jump_to_supports_top_and_bottom() {
    let mut w = NewsWidget::with_config(NewsConfig::default());
    {
        let mut st = w.state.lock().unwrap();
        st.articles = vec![
            article("https://a", "A", 0),
            article("https://b", "B", 0),
            article("https://c", "C", 0),
        ];
        st.selected = 1;
    }
    w.jump_to(0);
    assert_eq!(w.state.lock().unwrap().selected, 0);
    w.jump_to(usize::MAX);
    assert_eq!(w.state.lock().unwrap().selected, 2);
}

#[test]
fn age_label_buckets() {
    // Delegates to `format::relative_time_label`, which buckets
    // sub-minute as "now" (minute-resolution suffices for news
    // article timestamps — see SDK doc § Formatting).
    let now = Utc::now();
    assert_eq!(age_label(now, now - chrono::Duration::seconds(30)), "now");
    assert_eq!(age_label(now, now - chrono::Duration::seconds(120)), "2m");
    assert_eq!(age_label(now, now - chrono::Duration::seconds(7200)), "2h");
    assert_eq!(
        age_label(now, now - chrono::Duration::seconds(86400 * 3)),
        "3d"
    );
}

#[test]
fn empty_feeds_is_visible_in_state() {
    let w = NewsWidget::with_config(NewsConfig::default());
    assert!(!w.feeds_configured);
}

#[test]
fn is_insufficient_reply_recognizes_canonical_phrasings() {
    assert!(is_insufficient_reply("Insufficient content to summarize."));
    assert!(is_insufficient_reply("insufficient content to summarize"));
    assert!(is_insufficient_reply(
        "  INSUFFICIENT CONTENT TO SUMMARIZE.  "
    ));
    assert!(is_insufficient_reply(
        "Insufficient information to summarize this article."
    ));
    assert!(!is_insufficient_reply("Apple announced…"));
}

#[test]
fn wrap_text_greedy_fills_within_width() {
    let out = wrap_text("the quick brown fox jumps over the lazy dog", 12, 5);
    // Expected greedy wrap: "the quick", "brown fox", "jumps over", "the lazy dog"
    assert_eq!(
        out,
        vec!["the quick", "brown fox", "jumps over", "the lazy dog"]
    );
}

#[test]
fn wrap_text_caps_at_max_lines_and_ellipsizes() {
    let out = wrap_text("one two three four five six seven eight nine ten", 4, 3);
    assert_eq!(out.len(), 3);
    let last = out.last().unwrap();
    assert!(
        last.ends_with('…'),
        "last line should end in ellipsis: {last:?}"
    );
}

#[test]
fn wrap_text_breaks_oversized_single_words_across_lines() {
    // The shared `text::wrap` is lossless on oversized words: it
    // mid-breaks rather than truncating with `…`, so the reader
    // can see the whole word across lines. A previous local
    // implementation truncated; the converged behaviour is
    // strictly more informative.
    let out = wrap_text("supercalifragilistic", 10, 3);
    assert!(out.len() >= 2);
    for line in &out {
        assert!(line.chars().count() <= 10);
    }
    // The full word survives (concatenation reproduces it).
    let joined: String = out.iter().flat_map(|l| l.chars()).collect();
    assert!(joined.contains("supercalifragilistic"));
}

/// Yahoo Finance + some Atom feeds ship items without a
/// `<description>` element, so `article.summary` is `None`. Without
/// a placeholder, `wrap_text("", …)` returns an empty Vec and the
/// expanded row count drops to zero — pressing `e` looks broken.
/// Surface the `s` hint when an LLM is configured so the user has
/// a path to actual content.
#[test]
fn expanded_summary_lines_shows_llm_hint_when_excerpt_empty() {
    let article = Article {
        title: "no-desc".into(),
        url: "https://example/no-desc".into(),
        source: "TestFeed".into(),
        published: Utc::now(),
        summary: None,
        topics: vec![],
    };
    let state = Arc::new(Mutex::new(NewsState::default()));
    let lines = expanded_summary_lines(&article, &state, 80, true);
    assert_eq!(lines.len(), 1, "must render exactly one placeholder line");
    assert!(
        lines[0].contains("press s"),
        "should point at the `s` action: {lines:?}"
    );
}

/// Same situation with no LLM configured: still show a placeholder,
/// but don't promise an AI summary that can't be delivered. Redirect
/// to Enter (browser open) as the only meaningful action.
#[test]
fn expanded_summary_lines_shows_browser_hint_when_no_llm() {
    let article = Article {
        title: "no-desc".into(),
        url: "https://example/no-desc".into(),
        source: "TestFeed".into(),
        published: Utc::now(),
        summary: Some("   \n  \t ".into()), // whitespace-only also counts as empty
        topics: vec![],
    };
    let state = Arc::new(Mutex::new(NewsState::default()));
    let lines = expanded_summary_lines(&article, &state, 80, false);
    assert_eq!(lines.len(), 1);
    // Hint points at the `o` action — the platform convention
    // for "open externally" (was Enter before; Enter is now the
    // in-place expand binding).
    assert!(
        lines[0].contains("`o`"),
        "should point at the `o` action: {lines:?}"
    );
}

/// After `s` runs the body fetch + LLM and both come up empty
/// (Yahoo Finance: no Readability/salvage match + no RSS excerpt),
/// `summary_state = Failed`. The render path used to fall back to
/// `raw_lines()`, which produced the SAME placeholder shown before
/// `s` was pressed — indistinguishable from "never tried." Now
/// the empty-raw + Failed combination renders a distinct
/// couldn't-extract message so the user knows the action ran.
#[test]
fn expanded_summary_lines_failed_with_empty_raw_shows_couldnt_extract() {
    let article = Article {
        title: "no-desc".into(),
        url: "https://example/empty".into(),
        source: "TestFeed".into(),
        published: Utc::now(),
        summary: None,
        topics: vec![],
    };
    let state = Arc::new(Mutex::new(NewsState::default()));
    {
        let mut st = state.lock().unwrap();
        st.summaries
            .insert(article.url.clone(), SummaryState::Failed);
        st.summary_view.insert(article.url.clone(), true);
    }
    let lines = expanded_summary_lines(&article, &state, 80, true);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("Couldn't extract"),
        "should show couldn't-extract message: {lines:?}"
    );
}

/// Articles with a real excerpt still surface that excerpt on
/// Failed — losing it would regress vs. the pre-LLM behavior.
#[test]
fn expanded_summary_lines_failed_with_real_raw_falls_back_to_excerpt() {
    let article = Article {
        title: "with-desc".into(),
        url: "https://example/with-desc".into(),
        source: "TestFeed".into(),
        published: Utc::now(),
        summary: Some("Real article excerpt content goes here.".into()),
        topics: vec![],
    };
    let state = Arc::new(Mutex::new(NewsState::default()));
    {
        let mut st = state.lock().unwrap();
        st.summaries
            .insert(article.url.clone(), SummaryState::Failed);
        st.summary_view.insert(article.url.clone(), true);
    }
    let lines = expanded_summary_lines(&article, &state, 80, true);
    assert!(!lines.is_empty());
    assert!(
        lines[0].starts_with("Real"),
        "should still show raw excerpt when present: {lines:?}"
    );
}

/// Empty content must NOT hit the LLM. The body-fetch chain
/// passes the RSS excerpt as fallback after extraction failure;
/// when both are empty, spawning the LLM produces a visible
/// "Summarizing…" flicker and burns a request to get back
/// "Insufficient content to summarize." Short-circuit to Failed
/// instead — the render path picks up the distinct empty-Failed
/// rendering.
#[tokio::test]
async fn spawn_summary_llm_task_skips_llm_when_content_empty() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PanickingLlm {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl LlmProvider for PanickingLlm {
        async fn complete(&self, _request: LlmRequest) -> Result<crate::llm::LlmResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            panic!("LLM should not be called for empty content");
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let llm: Arc<dyn LlmProvider> = Arc::new(PanickingLlm {
        calls: calls.clone(),
    });
    let state = Arc::new(Mutex::new(NewsState::default()));
    let cache = ScopedCache::ephemeral();
    let url = "https://example/empty".to_string();
    spawn_summary_llm_task(
        llm,
        state.clone(),
        cache,
        "title".into(),
        url.clone(),
        "   \n\t  ".into(), // whitespace-only also counts as empty
    );
    // Yield once so any (unexpected) tokio task gets a chance to run.
    tokio::task::yield_now().await;
    let st = state.lock().unwrap();
    assert!(
        matches!(st.summaries.get(&url), Some(SummaryState::Failed)),
        "must mark Failed up-front, got: {:?}",
        st.summaries.get(&url)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "LLM must not be called for empty content"
    );
}

/// Articles with a real excerpt still render the wrapped text —
/// the empty-summary placeholder mustn't hijack the normal path.
#[test]
fn expanded_summary_lines_renders_excerpt_normally_when_present() {
    let article = Article {
        title: "with-desc".into(),
        url: "https://example/with-desc".into(),
        source: "TestFeed".into(),
        published: Utc::now(),
        summary: Some("This is a real article excerpt that should wrap.".into()),
        topics: vec![],
    };
    let state = Arc::new(Mutex::new(NewsState::default()));
    let lines = expanded_summary_lines(&article, &state, 80, true);
    assert!(!lines.is_empty());
    assert!(
        lines[0].starts_with("This is"),
        "should show the real excerpt, not a placeholder: {lines:?}"
    );
}

#[test]
fn expand_key_toggles_expanded_state() {
    let mut w = NewsWidget::with_config(NewsConfig::default());
    {
        let mut st = w.state.lock().unwrap();
        st.articles = vec![article("https://a", "A", 0)];
    }
    let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
    assert_eq!(w.handle_key(key), EventResult::Handled);
    assert!(w.state.lock().unwrap().expanded);
    assert_eq!(w.handle_key(key), EventResult::Handled);
    assert!(!w.state.lock().unwrap().expanded);
}

#[test]
fn cycle_filter_wraps_and_resets_selection() {
    let cfg = NewsConfig {
        topics: vec![
            provider::Topic {
                label: "Tech".into(),
                keywords: vec!["AI".into()],
            },
            provider::Topic {
                label: "Finance".into(),
                keywords: vec!["Fed".into()],
            },
        ],
        ..NewsConfig::default()
    };
    let mut w = NewsWidget::with_config(cfg);
    assert_eq!(w.filter_tabs, vec!["All", "Tech", "Finance"]);
    // Seed selection so we can verify the cycle resets it.
    {
        let mut st = w.state.lock().unwrap();
        st.articles = vec![article("https://a", "x", 0); 5];
        st.selected = 3;
    }
    w.cycle_filter(true);
    assert_eq!(w.active_filter_label(), "Tech");
    assert_eq!(w.state.lock().unwrap().selected, 0);
    w.cycle_filter(true);
    assert_eq!(w.active_filter_label(), "Finance");
    w.cycle_filter(true);
    assert_eq!(w.active_filter_label(), "All");
    w.cycle_filter(false);
    assert_eq!(w.active_filter_label(), "Finance");
}

#[test]
fn cycle_filter_no_op_with_no_topics() {
    let mut w = NewsWidget::with_config(NewsConfig::default());
    assert_eq!(w.filter_tabs, vec!["All"]);
    w.cycle_filter(true);
    assert_eq!(w.active_filter_label(), "All");
}

#[test]
fn tab_index_at_maps_columns_to_tabs() {
    let cfg = NewsConfig {
        topics: vec![
            provider::Topic {
                label: "Tech".into(),
                keywords: vec![],
            },
            provider::Topic {
                label: "World".into(),
                keywords: vec![],
            },
        ],
        ..NewsConfig::default()
    };
    let w = NewsWidget::with_config(cfg);
    // tabs render as: " [All] [Tech] [World]" starting at x=0
    //                  012345678901234567890123
    //                  [All] at 1..6, [Tech] at 7..13, [World] at 14..21
    let tab_area = Rect::new(0, 0, 40, 1);
    assert_eq!(w.tab_index_at(2, tab_area), Some(0));
    assert_eq!(w.tab_index_at(8, tab_area), Some(1));
    assert_eq!(w.tab_index_at(15, tab_area), Some(2));
    // click past the last tab → None
    assert_eq!(w.tab_index_at(30, tab_area), None);
}

#[test]
fn article_index_at_maps_rows_in_compact_mode() {
    let w = NewsWidget::with_config(NewsConfig::default());
    {
        let mut st = w.state.lock().unwrap();
        st.articles = vec![
            article("https://a", "A", 0),
            article("https://b", "B", 0),
            article("https://c", "C", 0),
        ];
    }
    let articles = w.filtered_articles();
    let list_area = Rect::new(0, 5, 60, 10);
    // Each article = 2 rows starting at y=5: A=[5,6], B=[7,8], C=[9,10]
    assert_eq!(w.article_index_at(5, list_area, &articles), Some(0));
    assert_eq!(w.article_index_at(6, list_area, &articles), Some(0));
    assert_eq!(w.article_index_at(7, list_area, &articles), Some(1));
    assert_eq!(w.article_index_at(10, list_area, &articles), Some(2));
    assert_eq!(w.article_index_at(99, list_area, &articles), None);
}

#[test]
fn expand_is_no_op_when_no_articles() {
    let mut w = NewsWidget::with_config(NewsConfig::default());
    let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
    w.handle_key(key);
    assert!(!w.state.lock().unwrap().expanded);
}
