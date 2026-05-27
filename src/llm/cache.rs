use std::{
    hash::{Hash, Hasher},
    num::NonZeroUsize,
    sync::Mutex,
    time::{Duration, Instant},
};

use lru::LruCache;

use super::{LlmRequest, LlmResponse, Role};

/// Cached responses older than this are evicted on read. Summaries the
/// user sees today aren't useful a week later — by then the source
/// article has rotated out of the news feed anyway. Keeps the cache
/// from holding multi-week-old text forever.
const ENTRY_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

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

struct Entry {
    value: LlmResponse,
    inserted_at: Instant,
}

pub struct ResponseCache {
    inner: Mutex<LruCache<CacheKey, Entry>>,
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
        let entry = g.get(&key)?;
        if entry.inserted_at.elapsed() >= ENTRY_TTL {
            g.pop(&key);
            return None;
        }
        Some(entry.value.clone())
    }

    pub fn put(&self, key: CacheKey, value: LlmResponse) {
        let mut g = self.inner.lock().expect("cache poisoned");
        g.put(
            key,
            Entry {
                value,
                inserted_at: Instant::now(),
            },
        );
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

    #[test]
    fn cache_evicts_entries_past_ttl_on_read() {
        // Push an entry whose `inserted_at` is already older than the TTL
        // by reaching past the LRU and mutating directly. Verifies the
        // TTL gate, not the wall clock.
        let cache = ResponseCache::with_capacity(4);
        let key = CacheKey(7);
        let stale = Entry {
            value: LlmResponse { text: "stale".into() },
            inserted_at: Instant::now() - (ENTRY_TTL + Duration::from_secs(1)),
        };
        cache.inner.lock().unwrap().put(key, stale);
        assert!(cache.get(key).is_none(), "stale entry should be evicted");
    }
}
