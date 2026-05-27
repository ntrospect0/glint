pub mod anthropic;
pub mod cache;
pub mod rate_limiter;

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

pub use anthropic::AnthropicProvider;

/// A `LlmProvider` is the boundary between glint widgets and any LLM. Provider
/// chooses how to map `LlmRequest` onto its native API; widgets stay generic.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    #[allow(dead_code)] // surfaced in status bar / settings in a later phase.
    fn name(&self) -> &str;

    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse>;

    #[allow(dead_code)] // wired into status bar in a later phase.
    async fn health_check(&self) -> Result<bool>;
}

/// Role on a single chat message — matches the Anthropic Messages convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    #[allow(dead_code)] // used by multi-turn flows in later phases.
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
    #[allow(dead_code)] // surfaced in status bar in a later phase.
    pub input_tokens: u32,
    #[allow(dead_code)] // surfaced in status bar in a later phase.
    pub output_tokens: u32,
}

/// User-configurable LLM options (loaded from `~/.config/glint/llm.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    #[serde(default)]
    pub provider: ProviderConfig,

    #[serde(default)]
    pub limits: LimitsConfig,

    #[serde(default)]
    pub features: FeaturesConfig,
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
            features: FeaturesConfig::default(),
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
    "anthropic".into()
}
fn default_model() -> String {
    "claude-sonnet-4-6".into()
}
fn default_api_base() -> String {
    "https://api.anthropic.com".into()
}
fn default_max_tokens() -> u32 {
    512
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
    1024
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_requests_per_minute: default_rpm(),
            cache_capacity: default_cache_capacity(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeaturesConfig {
    #[serde(default = "default_true")]
    pub news_summarize: bool,
    #[allow(dead_code)] // wired in when LLM topic classification lands.
    #[serde(default)]
    pub news_classify: bool,
    #[allow(dead_code)] // wired in when stocks widget gets real data.
    #[serde(default)]
    pub stock_disambiguate: bool,
}

fn default_true() -> bool {
    true
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            news_summarize: true,
            news_classify: false,
            stock_disambiguate: false,
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
    match config.provider.name.as_str() {
        "anthropic" => match anthropic::ApiKey::load() {
            Ok(Some(key)) => {
                let provider = AnthropicProvider::new(
                    key,
                    config.provider.model.clone(),
                    config.provider.api_base.clone(),
                    config.provider.max_tokens,
                    config.limits.clone(),
                )
                .context("failed to build AnthropicProvider")?;
                Ok(Some(Arc::new(provider)))
            }
            Ok(None) => Ok(None),
            Err(err) => {
                tracing::warn!(error = %err, "anthropic_key.toml unreadable");
                Ok(None)
            }
        },
        other => {
            tracing::warn!(provider = %other, "unknown LLM provider, disabling");
            Ok(None)
        }
    }
}
