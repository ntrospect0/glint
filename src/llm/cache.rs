use std::{
    hash::{Hash, Hasher},
    num::NonZeroUsize,
    sync::Mutex,
};

use lru::LruCache;

use super::{LlmRequest, LlmResponse, Role};

/// Hash-based key. Avoids holding the full prompt in the cache key; we only
/// store a stable digest so memory stays bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(u64);

impl CacheKey {
    pub fn of(request: &LlmRequest) -> Self {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        request.model.as_deref().unwrap_or("").hash(&mut h);
        request.system.as_deref().unwrap_or("").hash(&mut h);
        request.max_tokens.hash(&mut h);
        for m in &request.messages {
            match m.role {
                Role::User => 0u8.hash(&mut h),
                Role::Assistant => 1u8.hash(&mut h),
            }
            m.content.hash(&mut h);
        }
        CacheKey(h.finish())
    }
}

pub struct ResponseCache {
    inner: Mutex<LruCache<CacheKey, LlmResponse>>,
}

impl ResponseCache {
    pub fn with_capacity(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    pub fn get(&self, key: CacheKey) -> Option<LlmResponse> {
        let mut g = self.inner.lock().expect("cache poisoned");
        g.get(&key).cloned()
    }

    pub fn put(&self, key: CacheKey, value: LlmResponse) {
        let mut g = self.inner.lock().expect("cache poisoned");
        g.put(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmMessage, LlmRequest, Role};

    fn req(model: &str, prompt: &str) -> LlmRequest {
        LlmRequest {
            model: Some(model.into()),
            system: None,
            messages: vec![LlmMessage {
                role: Role::User,
                content: prompt.into(),
            }],
            max_tokens: 200,
            cache_system: false,
        }
    }

    #[test]
    fn identical_requests_produce_identical_keys() {
        let a = CacheKey::of(&req("sonnet", "hi"));
        let b = CacheKey::of(&req("sonnet", "hi"));
        assert_eq!(a, b);
    }

    #[test]
    fn model_or_prompt_changes_invalidate_key() {
        let base = CacheKey::of(&req("sonnet", "hi"));
        assert_ne!(base, CacheKey::of(&req("haiku", "hi")));
        assert_ne!(base, CacheKey::of(&req("sonnet", "bye")));
    }

    #[test]
    fn cache_get_returns_the_put_value() {
        let cache = ResponseCache::with_capacity(8);
        let key = CacheKey(42);
        let val = LlmResponse { text: "cached".into() };
        cache.put(key, val.clone());
        let got = cache.get(key).unwrap();
        assert_eq!(got.text, "cached");
    }
}
