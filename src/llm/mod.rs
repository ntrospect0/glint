// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod anthropic;
pub mod cache;
pub mod openai;
pub mod rate_limiter;

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiProvider;

/// Builder function pointer for an LLM provider — takes the user's
/// `provider` config block + the shared `limits` block and produces
/// either a ready provider or `Ok(None)` if the provider is intentionally
/// disabled (missing key, placeholder credentials, etc.).
pub type LlmBuilder =
    fn(&ProviderConfig, LimitsConfig) -> Result<Option<Arc<dyn LlmProvider>>>;

/// One entry in the LLM provider registry. Adding a new provider means
/// appending one [`LlmProviderDef`] to [`PROVIDERS`] — no changes
/// elsewhere in the LLM module are required.
pub struct LlmProviderDef {
    /// Identifier matched against `[provider.name]` in `llm.toml`.
    pub name: &'static str,
    /// Human-readable label used by the wizard's provider picker.
    pub display_name: &'static str,
    /// Credentials filename under `~/.config/glint/credentials/` that
    /// stores this provider's API key.
    pub credentials_filename: &'static str,
    /// URL where the user obtains an API key (shown in the wizard hint
    /// and the seeded credentials-template file header).
    pub key_portal_url: &'static str,
    /// Used when the user's `llm.toml` doesn't override these.
    pub default_model: &'static str,
    pub default_api_base: &'static str,
    pub default_max_tokens: u32,
    /// Constructs the provider given the resolved config. Returning
    /// `Ok(None)` disables the LLM integration for this run.
    pub builder: LlmBuilder,
}

fn build_anthropic(
    config: &ProviderConfig,
    limits: LimitsConfig,
) -> Result<Option<Arc<dyn LlmProvider>>> {
    match anthropic::ApiKey::load() {
        Ok(Some(key)) => {
            let provider = AnthropicProvider::new(
                key,
                config.model.clone(),
                config.api_base.clone(),
                config.max_tokens,
                limits,
            )
            .context("failed to build AnthropicProvider")?;
            Ok(Some(Arc::new(provider)))
        }
        Ok(None) => Ok(None),
        Err(err) => {
            tracing::warn!(error = %err, "anthropic_key.toml unreadable");
            Ok(None)
        }
    }
}

fn build_openai(
    config: &ProviderConfig,
    limits: LimitsConfig,
) -> Result<Option<Arc<dyn LlmProvider>>> {
    match openai::ApiKey::load() {
        Ok(Some(key)) => {
            let provider = OpenAiProvider::new(
                key,
                config.model.clone(),
                config.api_base.clone(),
                config.max_tokens,
                limits,
            )
            .context("failed to build OpenAiProvider")?;
            Ok(Some(Arc::new(provider)))
        }
        Ok(None) => Ok(None),
        Err(err) => {
            tracing::warn!(error = %err, "openai_key.toml unreadable");
            Ok(None)
        }
    }
}

pub const PROVIDERS: &[LlmProviderDef] = &[
    LlmProviderDef {
        name: "anthropic",
        display_name: "Anthropic (Claude)",
        credentials_filename: "anthropic_key.toml",
        key_portal_url: "https://console.anthropic.com/",
        default_model: "claude-sonnet-4-6",
        default_api_base: "https://api.anthropic.com",
        default_max_tokens: 512,
        builder: build_anthropic,
    },
    LlmProviderDef {
        name: "openai",
        display_name: "OpenAI (GPT)",
        credentials_filename: "openai_key.toml",
        key_portal_url: "https://platform.openai.com/api-keys",
        default_model: "gpt-5-mini",
        default_api_base: "https://api.openai.com",
        default_max_tokens: 512,
        builder: build_openai,
    },
];

pub fn find_provider(name: &str) -> Option<&'static LlmProviderDef> {
    PROVIDERS.iter().find(|p| p.name == name)
}

/// First registered provider — supplies the built-in defaults when the
/// user has no `llm.toml` (or omits the `[provider]` block).
fn default_provider() -> &'static LlmProviderDef {
    PROVIDERS
        .first()
        .expect("at least one LLM provider must be registered")
}

/// A `LlmProvider` is the boundary between glint widgets and any LLM. Provider
/// chooses how to map `LlmRequest` onto its native API; widgets stay generic.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse>;
}

/// Role on a single chat message — matches the Anthropic Messages convention.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct LlmMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    /// `None` means "use the provider's default model".
    pub model: Option<String>,
    pub system: Option<String>,
    pub messages: Vec<LlmMessage>,
    pub max_tokens: u32,
    /// Hint to the provider that the system prompt should be eligible for
    /// prompt caching (Anthropic only — others may ignore).
    pub cache_system: bool,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: String,
}

/// User-configurable LLM options (loaded from `~/.config/glint/llm.toml`).
///
/// Per-feature on/off toggles live in each LLM-aware widget's own TOML
/// (e.g. `summarize_with_llm = true` in news.toml), keeping this layer
/// widget-agnostic.
#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    #[serde(default)]
    pub provider: ProviderConfig,

    #[serde(default)]
    pub limits: LimitsConfig,
}

fn default_enabled() -> bool {
    true
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            provider: ProviderConfig::default(),
            limits: LimitsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_provider_name")]
    pub name: String,

    #[serde(default = "default_model")]
    pub model: String,

    #[serde(default = "default_api_base")]
    pub api_base: String,

    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_provider_name() -> String {
    default_provider().name.to_string()
}
fn default_model() -> String {
    default_provider().default_model.to_string()
}
fn default_api_base() -> String {
    default_provider().default_api_base.to_string()
}
fn default_max_tokens() -> u32 {
    default_provider().default_max_tokens
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: default_provider_name(),
            model: default_model(),
            api_base: default_api_base(),
            max_tokens: default_max_tokens(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_rpm")]
    pub max_requests_per_minute: u32,

    #[serde(default = "default_cache_capacity")]
    pub cache_capacity: usize,
}

fn default_rpm() -> u32 {
    20
}
fn default_cache_capacity() -> usize {
    128
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_requests_per_minute: default_rpm(),
            cache_capacity: default_cache_capacity(),
        }
    }
}

/// Try to construct the configured provider. Returns `Ok(None)` if the LLM
/// integration is intentionally disabled (no key, `enabled = false`, etc.) so
/// callers can transparently fall back to non-LLM paths.
pub fn build_provider(config: &LlmConfig) -> Result<Option<Arc<dyn LlmProvider>>> {
    if !config.enabled {
        return Ok(None);
    }
    match find_provider(&config.provider.name) {
        Some(def) => (def.builder)(&config.provider, config.limits.clone()),
        None => {
            tracing::warn!(
                provider = %config.provider.name,
                known = %PROVIDERS.iter().map(|p| p.name).collect::<Vec<_>>().join(", "),
                "unknown LLM provider, disabling"
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn registered_provider_names_are_unique() {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for p in PROVIDERS {
            assert!(!p.name.is_empty(), "empty provider name");
            assert!(seen.insert(p.name), "duplicate LLM provider: {}", p.name);
        }
    }

    #[test]
    fn defaults_track_first_registered_provider() {
        let first = default_provider();
        assert_eq!(default_provider_name(), first.name);
        assert_eq!(default_model(), first.default_model);
        assert_eq!(default_api_base(), first.default_api_base);
        assert_eq!(default_max_tokens(), first.default_max_tokens);
    }

    #[test]
    fn build_provider_disables_when_unknown_name() {
        let mut cfg = LlmConfig::default();
        cfg.provider.name = "not-a-real-provider".into();
        let result = build_provider(&cfg).expect("unknown providers fall through, not error");
        assert!(result.is_none());
    }
}
