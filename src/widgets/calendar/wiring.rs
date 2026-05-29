// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Provider wiring for the calendar widget — turns the configured
//! `[[providers]]` list into a single `Arc<dyn CalendarProvider>`
//! (a CompositeProvider when more than one) plus a short
//! `source_label` for the title row and an optional `auth_hint`
//! when one or more entries failed to authorize.
//!
//! Per-provider HTTP / credential code lives in the sibling files
//! (`local.rs`, `google.rs`, `outlook.rs`, `caldav.rs`); this module
//! is the assembly layer that picks the right backend for each
//! `ProviderEntry` and merges them at the trait boundary.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};

use super::caldav::{CalDavCredentials, CalDavProvider};
use super::config::{CalendarConfig, ProviderEntry, ProviderKind};
use super::google::GoogleCalendarProvider;
use super::local::{LocalCalendarFile, LocalCalendarProvider};
use super::outlook::OutlookCalendarProvider;
use super::provider::{CalendarProvider, Event};

use crate::auth::google::{store::GoogleToken, OAuthClientConfig as GoogleClientConfig};
use crate::auth::microsoft::{store::MicrosoftToken, OAuthClientConfig as MicrosoftClientConfig};

/// Returns `(provider, source_label, auth_hint)`. The provider is either a
/// single backend (Local / Google / Outlook / CalDAV) or a CompositeProvider
/// fanning out to multiple. `source_label` becomes the `[label]` shown in the
/// cell title (`google`, `local`, `google+outlook`, etc.).
pub(super) fn build_provider(
    config: &CalendarConfig,
) -> (Arc<dyn CalendarProvider>, String, Option<String>) {
    let local_file = LocalCalendarFile {
        events: config.events.clone(),
    };
    let local: Arc<dyn CalendarProvider> = match LocalCalendarProvider::from_file(local_file) {
        Ok(p) => Arc::new(p),
        Err(err) => {
            tracing::warn!(error = %err, "failed to parse calendar.toml events, starting empty");
            Arc::new(LocalCalendarProvider::empty())
        }
    };

    // Empty `[[providers]]` means "local only" — bail with the seeded
    // LocalCalendarProvider from above.
    if config.providers.is_empty() {
        return (local, "local".into(), None);
    }
    let entries: Vec<ProviderEntry> = config.providers.clone();

    let mut built: Vec<(Arc<dyn CalendarProvider>, &'static str)> = Vec::new();
    let mut hints: Vec<String> = Vec::new();
    for entry in &entries {
        match build_entry(entry, config) {
            Ok((provider, label)) => built.push((provider, label)),
            Err(hint) => hints.push(hint),
        }
    }

    if built.is_empty() {
        // Every requested provider failed — fall back to local so the widget
        // keeps rendering something useful with the hint banner above.
        let hint = if hints.is_empty() {
            None
        } else {
            Some(hints.join(" · "))
        };
        return (local, "local".into(), hint);
    }

    let labels: Vec<&'static str> = built.iter().map(|(_, l)| *l).collect();
    let source_label = labels.join("+");
    let hint = if hints.is_empty() {
        None
    } else {
        Some(hints.join(" · "))
    };
    let provider: Arc<dyn CalendarProvider> = if built.len() == 1 {
        built.into_iter().next().unwrap().0
    } else {
        Arc::new(CompositeProvider::new(
            built.into_iter().map(|(p, _)| p).collect(),
        ))
    };
    (provider, source_label, hint)
}

/// Build one provider entry. Returns Ok with a static label on success, Err
/// with a human-readable hint string on configuration failure.
fn build_entry(
    entry: &ProviderEntry,
    config: &CalendarConfig,
) -> Result<(Arc<dyn CalendarProvider>, &'static str), String> {
    match entry.kind {
        ProviderKind::Local => {
            let file = LocalCalendarFile {
                events: config.events.clone(),
            };
            let p =
                LocalCalendarProvider::from_file(file).map_err(|e| format!("local events: {e}"))?;
            Ok((Arc::new(p), "local"))
        }
        ProviderKind::Google => build_google_entry(&entry.calendar_ids).map(|p| (p, "google")),
        ProviderKind::Outlook => build_outlook_entry(&entry.calendar_ids).map(|p| (p, "outlook")),
        ProviderKind::Caldav => {
            let urls = if entry.calendar_ids.is_empty() {
                config.caldav.calendars.clone()
            } else {
                entry.calendar_ids.clone()
            };
            build_caldav_entry(urls).map(|p| (p, "caldav"))
        }
    }
}

fn build_outlook_entry(calendar_ids: &[String]) -> Result<Arc<dyn CalendarProvider>, String> {
    let client = MicrosoftClientConfig::load().map_err(|err| {
        tracing::warn!(error = %err, "microsoft_oauth_client.toml missing or invalid");
        "Drop microsoft_oauth_client.toml in ~/.config/glint/credentials/".to_string()
    })?;
    let token = MicrosoftToken::load()
        .map_err(|err| format!("Outlook token unreadable: {err}"))?
        .ok_or_else(|| "Run `glint --auth microsoft` to connect Microsoft Outlook".to_string())?;
    OutlookCalendarProvider::new(client, token, calendar_ids.to_vec())
        .map(|p| Arc::new(p) as Arc<dyn CalendarProvider>)
        .map_err(|err| format!("Outlook init failed: {err}"))
}

fn build_google_entry(calendar_ids: &[String]) -> Result<Arc<dyn CalendarProvider>, String> {
    let client = GoogleClientConfig::load().map_err(|err| {
        tracing::warn!(error = %err, "google_oauth_client.toml missing or invalid");
        "Drop google_oauth_client.toml in ~/.config/glint/credentials/".to_string()
    })?;
    let token = match GoogleToken::load() {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Err("Run `glint --auth google` to connect Google Calendar".into());
        }
        Err(err) => return Err(format!("Google token unreadable: {err}")),
    };
    GoogleCalendarProvider::new(client, token, calendar_ids.to_vec())
        .map(|p| Arc::new(p) as Arc<dyn CalendarProvider>)
        .map_err(|err| format!("Google init failed: {err}"))
}

fn build_caldav_entry(urls: Vec<String>) -> Result<Arc<dyn CalendarProvider>, String> {
    let creds = match CalDavCredentials::load() {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err("Fill in ~/.config/glint/credentials/caldav.toml to connect CalDAV".into());
        }
        Err(err) => return Err(format!("CalDAV credentials unreadable: {err}")),
    };
    CalDavProvider::new(creds, urls)
        .map(|p| Arc::new(p) as Arc<dyn CalendarProvider>)
        .map_err(|err| format!("CalDAV init failed: {err}"))
}

/// Meta-provider that fans `fetch_range` calls out to every wrapped provider
/// in parallel and merges the results. Each child's failures are logged
/// individually; one failing source doesn't block the others.
struct CompositeProvider {
    inner: Vec<Arc<dyn CalendarProvider>>,
}

impl CompositeProvider {
    fn new(inner: Vec<Arc<dyn CalendarProvider>>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl CalendarProvider for CompositeProvider {
    async fn fetch_range(
        &self,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> Result<Vec<Event>> {
        let futs = self.inner.iter().map(|p| p.fetch_range(start, end));
        let results = futures::future::join_all(futs).await;
        let mut all = Vec::new();
        for r in results {
            match r {
                Ok(mut chunk) => all.append(&mut chunk),
                Err(err) => {
                    tracing::warn!(error = %err, "child calendar provider failed");
                }
            }
        }
        all.sort_by_key(|e| e.start);
        Ok(all)
    }
}
