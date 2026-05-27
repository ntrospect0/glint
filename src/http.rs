// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Shared HTTP client. Building a [`reqwest::Client`] allocates a fresh
//! TLS session pool — separate clients can't reuse keepalive sockets or
//! cached TLS sessions across widgets. A single process-wide client folds
//! those costs into one pool and lets connection reuse work end-to-end.
//!
//! ## When to share, when to bespoke
//!
//! Use [`shared`] for plain JSON over HTTPS with a glint user-agent —
//! news, weather, calendar (Google + Outlook), email (Gmail + Outlook),
//! LLM providers, geolocation, OAuth flows.
//!
//! Keep a bespoke client for callers needing client-scoped state the
//! shared instance can't carry:
//! - **Cookie store** (`cookie_store(true)`): Yahoo's stocks / forex
//!   endpoints set CSRF cookies on the chart API that the next request
//!   must echo back. A shared cookie store would bleed those cookies
//!   into unrelated widgets.
//! - **Default headers**: CalDAV uses HTTP Basic on every request via
//!   `default_headers(Authorization: …)` — that header would leak into
//!   every other caller if applied on the shared client.

use std::sync::OnceLock;
use std::time::Duration;

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Process-wide reqwest client. Lazily constructed on first call; later
/// calls return cheap clones (Client is internally `Arc`).
///
/// Carries a 30-second timeout — generous enough for slow LLM
/// completions (Anthropic + OpenAI both can take >20s under load), tight
/// enough that a wedged TCP connection won't hang a widget refresh
/// forever. Callers needing a shorter bound (geolocation, weather) apply
/// it per-request via `RequestBuilder::timeout`.
pub fn shared() -> reqwest::Client {
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(concat!("glint-tui/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client should build with default features")
        })
        .clone()
}
