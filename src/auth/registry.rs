// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Auth provider registry — the single source of truth for OAuth providers
//! and IMAP-style credential-only providers.
//!
//! Adding a provider is one entry in [`PROVIDERS`]. Each entry can carry:
//!
//! - identity (`name`, `display_name`) + the `run` flow,
//! - an on-disk credentials spec (filename, starter template, which keys
//!   must be present for the provider to be usable),
//! - an inline-form schema for the wizard's OAuthSetup page,
//! - an optional post-auth fetch that pre-populates remote pickers
//!   (e.g. mailbox folder lists) so widget pages aren't blocked on the
//!   first render.
//!
//! Widgets declare which providers they depend on via [`AuthRequirement`]
//! on their `WidgetDescriptor`; the wizard reads those to drive prompts,
//! and `--auth <name>` resolves through [`find`].

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

/// Boxed async flow stored behind a function pointer so the registry can
/// hold heterogenous provider flows in a `const`.
pub type AuthFlow = fn() -> Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// Pre-populates a remote-option list after auth completes. Returns
/// `(option_key, options)` where `option_key` matches the
/// `RemoteMultiChoice::source` on a widget descriptor field.
pub type PostAuthRefresh =
    fn() -> Pin<Box<dyn Future<Output = Result<(&'static str, Vec<(String, String)>)>> + Send>>;

pub struct AuthProvider {
    /// Identifier used in `--auth <name>` and in [`AuthRequirement`].
    /// Lowercase ASCII, no spaces.
    pub name: &'static str,

    /// Label rendered by the wizard.
    #[allow(dead_code)] // surfaced by the wizard's auth-prompt step.
    pub display_name: &'static str,

    /// Run the provider's interactive flow. For OAuth providers this
    /// drives the browser handshake; for credential-only providers (e.g.
    /// IMAP) the wizard has already saved credentials so this is a no-op.
    pub run: AuthFlow,

    /// Provider's on-disk credentials, if any. `None` for purely
    /// in-memory providers (no current example).
    pub credentials: Option<&'static CredentialsSpec>,

    /// Optional post-auth fetch — runs once the user has completed `run`
    /// successfully. `None` means nothing extra to refresh.
    pub post_auth_refresh: Option<PostAuthRefresh>,
}

/// Where and how a provider stores its credentials on disk.
pub struct CredentialsSpec {
    /// Filename written under `credentials/` (e.g. `google_oauth_client.toml`).
    pub filename: &'static str,
    /// Starter template written when the file is missing. `None` means
    /// the file is created inline via the wizard's OAuthSetup page (no
    /// useful template to seed — e.g. IMAP).
    pub starter_template: Option<&'static str>,
    /// Keys that must be set (and not still on a `REPLACE_WITH_` placeholder)
    /// for the credentials file to be considered usable.
    pub required_keys: &'static [&'static str],
    /// Inline-form schema for the wizard's OAuthSetup page. `None`
    /// disables inline capture (the user must edit the template file by
    /// hand instead).
    pub setup_schema: Option<&'static SetupSchema>,
}

/// Schema for the wizard's inline credentials-capture form. Drives both
/// the rendered fields and the on-save TOML body.
pub struct SetupSchema {
    /// Short display label used in the form ("Google", "Microsoft", "IMAP").
    pub short_name: &'static str,
    /// URL the user visits to obtain credentials.
    pub portal_url: &'static str,
    /// Multi-line setup hint shown above the form. Newlines preserved.
    pub hint: &'static str,
    /// Fields collected from the user, in render order.
    pub fields: &'static [SetupField],
    /// Extra static lines appended after user-supplied fields
    /// (e.g. Microsoft's `tenant = "common"`).
    pub extra_lines: &'static [&'static str],
}

pub struct SetupField {
    pub key: &'static str,
    pub label: &'static str,
    /// `true` masks input on render (passwords, client secrets).
    pub secret: bool,
}

/// A widget's declared dependency on an OAuth provider.
///
/// `scope_hints` is informational — the actual OAuth scope string is owned
/// by the provider module (e.g. `auth::google::SCOPE`). Hints drive the
/// wizard's "this widget needs access to your mailbox" copy.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // surfaced by the wizard's auth-prompt step.
pub struct AuthRequirement {
    pub provider: &'static str,
    pub scope_hints: &'static [&'static str],
}

fn run_google() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        let client = super::google::OAuthClientConfig::load()?;
        super::google::flow::run(&client).await?;
        println!("Google authorization complete.");
        Ok(())
    })
}

fn run_microsoft() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(async move {
        let client = super::microsoft::OAuthClientConfig::load()?;
        super::microsoft::flow::run(&client).await?;
        println!("Microsoft authorization complete.");
        Ok(())
    })
}

/// IMAP credentials are written to disk by the wizard's OAuthSetup page
/// itself; there is no browser handshake. The `run` callback exists only
/// to satisfy the shared dispatch path.
fn run_imap() -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
    Box::pin(async move { Ok(()) })
}

fn fetch_gmail_folders(
) -> Pin<Box<dyn Future<Output = Result<(&'static str, Vec<(String, String)>)>> + Send>> {
    Box::pin(async move {
        let client = super::google::OAuthClientConfig::load()?;
        let token = super::google::store::GoogleToken::load()?
            .ok_or_else(|| anyhow::anyhow!("no Google token on disk yet"))?;
        let provider = crate::widgets::email::gmail::GmailProvider::new(client, token)?;
        let opts = provider.list_folders_for_picker().await?;
        Ok(("email_folders", opts))
    })
}

fn fetch_outlook_folders(
) -> Pin<Box<dyn Future<Output = Result<(&'static str, Vec<(String, String)>)>> + Send>> {
    Box::pin(async move {
        let client = super::microsoft::OAuthClientConfig::load()?;
        let token = super::microsoft::store::MicrosoftToken::load()?
            .ok_or_else(|| anyhow::anyhow!("no Microsoft token on disk yet"))?;
        let provider = crate::widgets::email::outlook::OutlookEmailProvider::new(client, token)?;
        let opts = provider.list_folders_for_picker().await?;
        Ok(("email_folders", opts))
    })
}

fn fetch_imap_folders(
) -> Pin<Box<dyn Future<Output = Result<(&'static str, Vec<(String, String)>)>> + Send>> {
    Box::pin(async move {
        let dir = super::credentials_dir()?;
        let path = dir.join("imap.toml");
        let text = std::fs::read_to_string(&path)?;
        let creds: crate::widgets::email::imap::ImapCredentials = toml::from_str(&text)?;
        let provider = crate::widgets::email::imap::ImapProvider::new(creds);
        let opts = provider.list_folders_for_picker().await?;
        Ok(("email_folders", opts))
    })
}

const GOOGLE_SETUP: SetupSchema = SetupSchema {
    short_name: "Google",
    portal_url: "https://console.cloud.google.com/",
    hint: "Quick steps (full walkthrough in INSTRUCTIONS.md → Google):\n\
           \x20  1. Create a Google Cloud project at the URL above.\n\
           \x20  2. APIs & Services → Library → enable \"Google Calendar API\" + \"Gmail API\".\n\
           \x20  3. APIs & Services → OAuth consent screen → External → add yourself as a Test user.\n\
           \x20  4. APIs & Services → Credentials → Create OAuth client ID → Application type: Desktop app.\n\
           \x20  5. Copy the Client ID + Client Secret it shows you into the fields below.",
    fields: &[
        SetupField { key: "client_id", label: "Client ID", secret: false },
        SetupField { key: "client_secret", label: "Client Secret", secret: true },
    ],
    extra_lines: &[],
};

const MICROSOFT_SETUP: SetupSchema = SetupSchema {
    short_name: "Microsoft",
    portal_url: "https://portal.azure.com/",
    hint: "Quick steps (full walkthrough in INSTRUCTIONS.md → Microsoft):\n\
           \x20  1. portal.azure.com → Microsoft Entra ID → App registrations → New registration.\n\
           \x20  2. Supported account types: personal + work/school. Register.\n\
           \x20  3. Authentication → Add a platform → Mobile and desktop applications → tick http://localhost.\n\
           \x20  4. API permissions → Microsoft Graph → Delegated → add Calendars.Read, Mail.Read, User.Read.\n\
           \x20  5. Copy the Application (client) ID from the app's overview page into the field below.",
    fields: &[
        SetupField { key: "client_id", label: "Application (client) ID", secret: false },
    ],
    extra_lines: &["tenant = \"common\""],
};

const IMAP_SETUP: SetupSchema = SetupSchema {
    short_name: "IMAP",
    portal_url: "INSTRUCTIONS.md → IMAP for per-provider host/port presets",
    hint: "App-password recipes (full table in INSTRUCTIONS.md → IMAP):\n\
           \x20  • Gmail:     host imap.gmail.com / port 993. App password: myaccount.google.com → Security → 2-Step → App passwords.\n\
           \x20  • iCloud:    host imap.mail.me.com / port 993. App password: appleid.apple.com → Sign-In and Security → App-Specific Passwords.\n\
           \x20  • Fastmail:  host imap.fastmail.com / port 993. App password: fastmail.com → Settings → Privacy & Security → New app password (scope IMAP).\n\
           \x20  • Yahoo:     host imap.mail.yahoo.com / port 993. Generate an app password under Account Security.\n\
           \x20  • Self-host: whatever your server exposes; port 993 implicit TLS works in 95% of cases.",
    fields: &[
        SetupField { key: "host", label: "IMAP host", secret: false },
        SetupField { key: "port", label: "IMAP port (993 for TLS)", secret: false },
        SetupField { key: "username", label: "Username (usually full email)", secret: false },
        SetupField { key: "app_password", label: "App password", secret: true },
    ],
    extra_lines: &["use_tls = true"],
};

const GOOGLE_CREDENTIALS: CredentialsSpec = CredentialsSpec {
    filename: "google_oauth_client.toml",
    starter_template: Some(crate::config::DEFAULT_GOOGLE_CLIENT_TEMPLATE),
    required_keys: &["client_id", "client_secret"],
    setup_schema: Some(&GOOGLE_SETUP),
};

const MICROSOFT_CREDENTIALS: CredentialsSpec = CredentialsSpec {
    filename: "microsoft_oauth_client.toml",
    starter_template: Some(crate::config::DEFAULT_MICROSOFT_CLIENT_TEMPLATE),
    required_keys: &["client_id"],
    setup_schema: Some(&MICROSOFT_SETUP),
};

const IMAP_CREDENTIALS: CredentialsSpec = CredentialsSpec {
    filename: "imap.toml",
    starter_template: None,
    required_keys: &["host", "username", "app_password"],
    setup_schema: Some(&IMAP_SETUP),
};

pub const PROVIDERS: &[AuthProvider] = &[
    AuthProvider {
        name: "google",
        display_name: "Google (Calendar + Gmail)",
        run: run_google,
        credentials: Some(&GOOGLE_CREDENTIALS),
        post_auth_refresh: Some(fetch_gmail_folders),
    },
    AuthProvider {
        name: "microsoft",
        display_name: "Microsoft (Outlook + Mail)",
        run: run_microsoft,
        credentials: Some(&MICROSOFT_CREDENTIALS),
        post_auth_refresh: Some(fetch_outlook_folders),
    },
    AuthProvider {
        name: "imap",
        display_name: "IMAP (email via any IMAP server)",
        run: run_imap,
        credentials: Some(&IMAP_CREDENTIALS),
        post_auth_refresh: Some(fetch_imap_folders),
    },
];

pub fn find(name: &str) -> Option<&'static AuthProvider> {
    PROVIDERS.iter().find(|p| p.name == name)
}

/// Comma-separated list of registered provider names for CLI error messages.
pub fn names_csv() -> String {
    PROVIDERS
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `true` when the provider's credentials file is missing OR any
/// [`CredentialsSpec::required_keys`] is empty / still placeholder.
/// Returns `false` for providers without a credentials spec (nothing to
/// capture).
pub fn needs_credential_capture(provider_name: &str) -> bool {
    let Some(provider) = find(provider_name) else {
        return false;
    };
    let Some(spec) = provider.credentials else {
        return false;
    };
    let Ok(dir) = super::credentials_dir() else {
        return true;
    };
    let path = dir.join(spec.filename);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return true;
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
        return true;
    };
    spec.required_keys
        .iter()
        .any(|key| match doc.get(*key).and_then(|v| v.as_str()) {
            None => true,
            Some(s) => s.trim().is_empty() || s.starts_with("REPLACE_WITH_"),
        })
}

/// Write the provider's starter credentials template to `credentials/`
/// if the file doesn't already exist. Bails (with a "fill in credentials
/// then retry" message) when a fresh template gets written, so the wizard
/// surfaces a clear prompt instead of `OAuthClientConfig::load`'s raw
/// missing-file error. Providers without a `starter_template` (e.g.
/// IMAP, captured inline) are no-ops.
pub fn ensure_credentials_template(provider_name: &str) -> Result<()> {
    let Some(provider) = find(provider_name) else {
        return Ok(());
    };
    let Some(spec) = provider.credentials else {
        return Ok(());
    };
    let Some(contents) = spec.starter_template else {
        return Ok(());
    };
    if crate::credentials::write_template_if_missing(spec.filename, contents)? {
        let path = crate::credentials::path(spec.filename)?;
        anyhow::bail!(
            "Wrote a template at {}. Open it, paste in your OAuth credentials, save, then press Space again to authorize.",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn provider_names_are_unique() {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for p in PROVIDERS {
            assert!(!p.name.is_empty());
            assert!(
                seen.insert(p.name),
                "duplicate auth provider name: {}",
                p.name
            );
        }
    }

    #[test]
    fn find_resolves_registered_providers() {
        assert!(find("google").is_some());
        assert!(find("microsoft").is_some());
        assert!(find("imap").is_some());
        assert!(find("not-a-real-provider").is_none());
    }

    #[test]
    fn every_provider_with_credentials_lists_required_keys() {
        for p in PROVIDERS {
            let Some(spec) = p.credentials else { continue };
            assert!(
                !spec.required_keys.is_empty(),
                "{} has credentials but no required_keys",
                p.name
            );
            assert!(
                !spec.filename.is_empty(),
                "{} credentials.filename is empty",
                p.name
            );
        }
    }
}
