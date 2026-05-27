//! Auth provider registry — the single source of truth for OAuth providers.
//!
//! Add a provider by appending an [`AuthProvider`] to [`PROVIDERS`]. Widgets
//! declare which providers they use via [`AuthRequirement`] on their
//! `WidgetDescriptor`; the wizard reads those to drive auth prompts, and
//! `--auth <name>` resolves through `find`.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

/// Boxed async flow stored behind a function pointer so the registry can
/// hold heterogenous provider flows in a `const`.
pub type AuthFlow = fn() -> Pin<Box<dyn Future<Output = Result<()>> + Send>>;

pub struct AuthProvider {
    /// Identifier used in `--auth <name>` and in [`AuthRequirement`].
    /// Lowercase ASCII, no spaces.
    pub name: &'static str,

    /// Label rendered by the wizard.
    #[allow(dead_code)] // surfaced by the wizard's auth-prompt step.
    pub display_name: &'static str,

    pub run: AuthFlow,
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

pub const PROVIDERS: &[AuthProvider] = &[
    AuthProvider {
        name: "google",
        display_name: "Google (Calendar + Gmail)",
        run: run_google,
    },
    AuthProvider {
        name: "microsoft",
        display_name: "Microsoft (Outlook + Mail)",
        run: run_microsoft,
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
        assert!(find("not-a-real-provider").is_none());
    }
}
