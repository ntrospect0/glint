use anyhow::{Context, Result};
// rustc's `unused_imports` lint mis-fires on proc-macro attribute imports
// when there's exactly one use site in this file. The compile fails without
// the import, so suppress the false-positive locally.
#[allow(unused_imports)]
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::auth;

use super::{cache::CacheKey, cache::ResponseCache, rate_limiter::RateLimiter, LimitsConfig};
use super::{LlmProvider, LlmRequest, LlmResponse, Role};

const CHAT_PATH: &str = "/v1/chat/completions";

/// Thin wrapper that keeps the secret away from `Debug`/`Display` so it
/// doesn't leak into logs.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    #[cfg(test)]
    pub fn new(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Loads the key from `~/.config/glint/credentials/openai_key.toml`.
    /// Returns `Ok(None)` when the file is absent or carries only the template
    /// placeholder, so callers can disable LLM features transparently.
    pub fn load() -> Result<Option<Self>> {
        let path = auth::credentials_dir()?.join("openai_key.toml");
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let parsed: OpenAiKeyFile = toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let key = parsed.api_key.unwrap_or_default();
        let key = key.trim();
        if key.is_empty() || key.starts_with("REPLACE_WITH_") {
            return Ok(None);
        }
        Ok(Some(ApiKey(key.to_string())))
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiKeyFile {
    #[serde(default)]
    api_key: Option<String>,
}

pub struct OpenAiProvider {
    client: reqwest::Client,
    key: ApiKey,
    default_model: String,
    api_base: String,
    default_max_tokens: u32,
    cache: ResponseCache,
    limiter: RateLimiter,
}

impl OpenAiProvider {
    pub fn new(
        key: ApiKey,
        default_model: String,
        api_base: String,
        default_max_tokens: u32,
        limits: LimitsConfig,
    ) -> Result<Self> {
        let client = crate::http::shared();
        Ok(Self {
            client,
            key,
            default_model,
            api_base,
            default_max_tokens,
            cache: ResponseCache::with_capacity(limits.cache_capacity),
            limiter: RateLimiter::new(limits.max_requests_per_minute),
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse> {
        let key = CacheKey::of(&request);
        if let Some(cached) = self.cache.get(key) {
            return Ok(cached);
        }
        if !self.limiter.try_acquire() {
            anyhow::bail!("LLM rate limit exceeded — try again in a moment");
        }
        let body = build_request_body(&request, &self.default_model, self.default_max_tokens);
        let url = format!("{}{CHAT_PATH}", self.api_base.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(self.key.as_str())
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("openai chat completions request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("openai returned {status}: {body}");
        }
        let parsed: ChatResponse = resp
            .json()
            .await
            .context("failed to deserialize openai response")?;
        let text = parsed
            .choices
            .into_iter()
            .filter_map(|c| c.message.and_then(|m| m.content))
            .collect::<Vec<_>>()
            .join("\n");
        let out = LlmResponse { text };
        self.cache.put(key, out.clone());
        Ok(out)
    }
}

#[derive(Debug, Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    /// gpt-5 / o-series reasoning models accept only `max_completion_tokens`;
    /// gpt-4o / gpt-4o-mini accept either. We send the newer name so the
    /// same code path works across the whole catalogue.
    max_completion_tokens: u32,
    messages: Vec<MessageBody<'a>>,
}

#[derive(Debug, Serialize)]
struct MessageBody<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    message: Option<ChatMessage>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
}

fn build_request_body<'a>(
    request: &'a LlmRequest,
    default_model: &'a str,
    default_max_tokens: u32,
) -> RequestBody<'a> {
    let model = request.model.as_deref().unwrap_or(default_model);
    let max_completion_tokens = if request.max_tokens == 0 {
        default_max_tokens
    } else {
        request.max_tokens
    };
    let mut messages: Vec<MessageBody<'a>> = Vec::with_capacity(request.messages.len() + 1);
    if let Some(sys) = request.system.as_deref() {
        // OpenAI carries the system prompt as the first message rather
        // than a separate top-level field. `cache_system` is an Anthropic
        // hint — silently ignored here.
        messages.push(MessageBody { role: "system", content: sys });
    }
    for m in &request.messages {
        messages.push(MessageBody {
            role: match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            },
            content: &m.content,
        });
    }
    RequestBody {
        model,
        max_completion_tokens,
        messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmMessage, LlmRequest, Role};

    fn req(prompt: &str) -> LlmRequest {
        LlmRequest {
            model: Some("test-model".into()),
            system: Some("you are a tester".into()),
            messages: vec![LlmMessage {
                role: Role::User,
                content: prompt.into(),
            }],
            max_tokens: 100,
            cache_system: true,
        }
    }

    #[test]
    fn request_body_uses_request_model_over_default() {
        let r = req("hi");
        let body = build_request_body(&r, "fallback-model", 999);
        assert_eq!(body.model, "test-model");
        assert_eq!(body.max_completion_tokens, 100);
        assert_eq!(body.messages.len(), 2);
        assert_eq!(body.messages[0].role, "system");
        assert_eq!(body.messages[0].content, "you are a tester");
        assert_eq!(body.messages[1].role, "user");
        assert_eq!(body.messages[1].content, "hi");
    }

    #[test]
    fn request_body_falls_back_to_defaults_when_zero() {
        let r = LlmRequest {
            model: None,
            system: None,
            messages: vec![LlmMessage {
                role: Role::User,
                content: "hi".into(),
            }],
            max_tokens: 0,
            cache_system: false,
        };
        let body = build_request_body(&r, "fallback-model", 512);
        assert_eq!(body.model, "fallback-model");
        assert_eq!(body.max_completion_tokens, 512);
        assert_eq!(body.messages.len(), 1, "no system prompt → no system message");
        assert_eq!(body.messages[0].role, "user");
    }

    #[test]
    fn api_key_debug_does_not_leak_secret() {
        let k = ApiKey::new("sk-supersecret".into());
        assert_eq!(format!("{k:?}"), "ApiKey(<redacted>)");
    }
}
