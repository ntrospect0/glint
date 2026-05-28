// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Hero image fetch + 24-hour byte cache + protocol-state builder.
//!
//! Design note: we cache two things separately:
//!
//! * The *decoded image* (`Arc<DynamicImage>`) per URL — source of
//!   truth, lives until the widget is dropped.
//! * The *encoded protocol* (`StatefulProtocol`) per URL — expensive
//!   to build (resize + base64), so we keep it across same-article
//!   redraws (which fire on every 250 ms tick + every input event).
//!
//! Navigation between articles tracks `last_url`: when the URL
//! changes, we drop the cached protocol for the *new* URL and
//! rebuild from the cached image. That force-rebuild is what
//! sidesteps the iTerm2 stale-frame bug we saw earlier without
//! re-encoding on every redraw — same-URL renders reuse the cached
//! protocol and consume effectively zero CPU.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

/// Browser-shaped User-Agent for image fetches. Several news-site
/// CDNs (notably `images.wsj.net`) 403 the default reqwest UA, so
/// we present as a regular Firefox build with the ntrospect0/glint
/// URL in the comment so any operator who inspects logs sees who
/// we are.
const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; glint-tui/",
    env!("CARGO_PKG_VERSION"),
    "; +https://github.com/ntrospect0/glint) Gecko/20100101 Firefox/120.0",
);

use anyhow::{Context, Result};
use image::{imageops::FilterType, DynamicImage};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;

use crate::cache::ScopedCache;

/// Hero images render in a small detail panel — anything larger than
/// ~640 px on the long side is wasted memory, since ratatui_image's
/// resize protocol downscales to terminal-cell resolution anyway.
/// We downsample on decode so the `Arc<DynamicImage>` we hold per URL
/// is bounded regardless of what the source CDN served.
const MAX_IMAGE_DIM: u32 = 640;

/// Cap on URLs simultaneously held in the in-memory image + protocol
/// caches. Mainstream news sources rarely surface more than a dozen
/// "currently of interest" articles at once; bounding at 10 keeps the
/// widget's resident footprint constant regardless of how many
/// articles the user navigates through in a session.
const MAX_CACHED_IMAGES: usize = 10;

/// Aspect-correct downscale when either side exceeds `MAX_IMAGE_DIM`.
/// `Triangle` (bilinear) matches the gallery widget's choice — visibly
/// indistinguishable from Lanczos3 at terminal-cell resolution and
/// 3-5× faster.
fn downscale(img: DynamicImage) -> DynamicImage {
    if img.width().max(img.height()) <= MAX_IMAGE_DIM {
        return img;
    }
    img.resize(MAX_IMAGE_DIM, MAX_IMAGE_DIM, FilterType::Triangle)
}

/// How long a cached image stays valid. Past this, the next
/// `ensure` call re-fetches it. News sites rarely swap hero images
/// on a published article, so 24h is plenty.
pub const IMAGE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Cache-key namespace for image bytes. Each entry is keyed by
/// `image-<short-sha256-of-url>` so URLs with query strings round-trip
/// to filesystem-safe filenames.
const IMAGE_CACHE_PREFIX: &str = "image-";

fn cache_key(url: &str) -> String {
    crate::cache::short_hash_key(IMAGE_CACHE_PREFIX, url)
}

/// Status of a hero-image entry in the in-memory state.
pub enum HeroState {
    /// Not yet attempted (or invalidated). Render shows a placeholder.
    Idle,
    /// A fetch is in flight for this URL.
    Fetching,
    /// Decoded image ready for re-encoding into a fresh protocol on
    /// every render. `Arc` so cheap to clone for the encode call.
    Ready(Arc<DynamicImage>),
    /// Last attempt failed. Render shows a "couldn't load" placeholder.
    Failed,
}

/// Owns the image picker + in-memory decoded-image cache. Per widget
/// instance.
pub struct HeroImageStore {
    picker: Option<Picker>,
    cache: ScopedCache,
    state: Mutex<HashMap<String, Arc<Mutex<HeroState>>>>,
    /// Encoded-protocol cache, one entry per article URL. Built
    /// lazily by `build_protocol` and reused across same-URL
    /// renders so we only pay the resize+encode cost once per
    /// navigation (not once per 250 ms tick).
    protocols: Mutex<HashMap<String, Arc<Mutex<Box<dyn StatefulProtocol>>>>>,
    /// URL of the last article we built a protocol for. When the
    /// caller asks for a different URL, we evict that URL's cache
    /// entry before rebuilding so the new protocol's escape bytes
    /// are freshly encoded — sidesteps the iTerm2 stale-frame bug
    /// without the per-frame CPU cost of always rebuilding.
    last_url: Mutex<Option<String>>,
    /// LRU access order across URLs that have been *requested* (via
    /// `ensure`). Front = least-recently requested. Capped at
    /// `MAX_CACHED_IMAGES`; eviction drops both the decoded image
    /// and any cached protocol for the evicted URL. Plain `slot()`
    /// reads from render are status queries, not access events —
    /// they deliberately don't touch this order.
    lru: Mutex<VecDeque<String>>,
}

impl HeroImageStore {
    pub fn new(cache: ScopedCache) -> Self {
        let mut picker = Picker::from_termios().unwrap_or_else(|_| {
            // Headless / test path: pick a generic cell size and the
            // Halfblocks protocol so we still render *something* in
            // environments where the termios query fails.
            Picker::new((10, 20))
        });
        // Upgrade the picker's protocol from the default Halfblocks
        // to the best graphics protocol the user's terminal can
        // actually paint. Prefer the iTerm2 inline-image protocol
        // (what `imgcat` uses) — works on iTerm.app, WezTerm, mintty,
        // VS Code, Tabby, Hyper. Halfblocks (pixelated ASCII) is the
        // honest fallback when no graphics-capable terminal is
        // detected; the widget still renders, just blockier.
        picker.protocol_type = detect_protocol();
        Self {
            picker: Some(picker),
            cache,
            state: Mutex::new(HashMap::new()),
            protocols: Mutex::new(HashMap::new()),
            last_url: Mutex::new(None),
            lru: Mutex::new(VecDeque::with_capacity(MAX_CACHED_IMAGES + 1)),
        }
    }

    /// Move `url` to the back of the LRU queue (most-recently
    /// requested) and, if the queue is now over capacity, return the
    /// URL that should be evicted from both the decoded-image and
    /// protocol maps. Caller is responsible for the actual map
    /// removal so eviction can happen outside the LRU lock.
    fn touch_lru(&self, url: &str) -> Option<String> {
        let mut order = self.lru.lock().expect("hero lru poisoned");
        order.retain(|k| k != url);
        order.push_back(url.to_string());
        if order.len() > MAX_CACHED_IMAGES {
            order.pop_front()
        } else {
            None
        }
    }

    /// Return the per-URL state slot. Inserts an `Idle` entry on
    /// first access so the caller can synchronously check status
    /// without a separate "is tracked" probe.
    pub fn slot(&self, url: &str) -> Arc<Mutex<HeroState>> {
        let mut map = self.state.lock().expect("hero image state poisoned");
        map.entry(url.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(HeroState::Idle)))
            .clone()
    }

    /// Kick off a fetch for `url` if it isn't already ready / in
    /// flight. The async work happens on a tokio task; the widget's
    /// next tick sees the updated state. Cached bytes (younger than
    /// `IMAGE_TTL`) are picked up synchronously before going to the
    /// network.
    pub fn ensure(&self, url: &str) {
        // Mark this URL as the most-recently-requested and evict the
        // least-recently-requested one if we're now over the cap. Done
        // before any decode work so the new URL never displaces itself.
        if let Some(evicted) = self.touch_lru(url) {
            if evicted != url {
                self.state
                    .lock()
                    .expect("hero image state poisoned")
                    .remove(&evicted);
                self.protocols
                    .lock()
                    .expect("hero protocols poisoned")
                    .remove(&evicted);
            }
        }

        let slot = self.slot(url);
        {
            let st = slot.lock().expect("hero image state poisoned");
            if !matches!(*st, HeroState::Idle | HeroState::Failed) {
                return;
            }
        }

        // Synchronous cache check first — avoids a redundant fetch
        // when the user re-expands an article they viewed recently.
        if let Some(bytes_entry) = self.cache.load_bytes(&cache_key(url)) {
            let age = bytes_entry
                .stored_at
                .signed_duration_since(chrono::Utc::now());
            let elapsed = age.abs().to_std().unwrap_or(Duration::ZERO);
            if elapsed < IMAGE_TTL {
                match image::load_from_memory(&bytes_entry.value) {
                    Ok(img) => {
                        *slot.lock().expect("hero image state poisoned") =
                            HeroState::Ready(Arc::new(downscale(img)));
                        return;
                    }
                    Err(err) => {
                        tracing::debug!(
                            url = %url,
                            error = %err,
                            "hero image cache decode failed; refetching"
                        );
                    }
                }
            }
        }

        // Mark fetching and fire off the network task.
        *slot.lock().expect("hero image state poisoned") = HeroState::Fetching;
        let url_owned = url.to_string();
        let cache = self.cache.clone();
        let slot_clone = slot.clone();
        tokio::spawn(async move {
            let result = download_bytes(&url_owned).await;
            let bytes = match result {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(url = %url_owned, error = %err, "feeds: hero image fetch failed");
                    *slot_clone.lock().expect("hero image state poisoned") = HeroState::Failed;
                    return;
                }
            };
            if let Err(err) = cache.store_bytes(&cache_key(&url_owned), &bytes) {
                tracing::debug!(url = %url_owned, error = %err, "feeds: hero image cache store failed");
            }
            match image::load_from_memory(&bytes) {
                Ok(img) => {
                    *slot_clone.lock().expect("hero image state poisoned") =
                        HeroState::Ready(Arc::new(downscale(img)));
                }
                Err(err) => {
                    tracing::warn!(url = %url_owned, error = %err, "feeds: hero image decode failed");
                    *slot_clone.lock().expect("hero image state poisoned") = HeroState::Failed;
                }
            }
        });
    }

    /// Get a StatefulProtocol for `url`, reusing the cached entry
    /// when this is the same article as the last call. When the
    /// URL changed, evicts that URL's cached protocol (if any) and
    /// builds a fresh one from the decoded image. Returns `None`
    /// when the image slot isn't `Ready` yet.
    ///
    /// The returned `Arc<Mutex<…>>` is owned by the cache; the
    /// caller locks it for the duration of one render call.
    pub fn build_protocol(&self, url: &str) -> Option<Arc<Mutex<Box<dyn StatefulProtocol>>>> {
        // Track URL transitions. When the caller asks for a
        // different URL than the previous render, evict any
        // existing entry for the new URL so we encode it fresh —
        // that's what fixes the backward-navigation stale-frame
        // bug. Same-URL repeats fall through to the cache hit
        // below and consume zero CPU.
        let url_changed = {
            let mut last = self.last_url.lock().expect("hero last_url poisoned");
            let changed = last.as_deref() != Some(url);
            *last = Some(url.to_string());
            changed
        };
        if url_changed {
            self.protocols
                .lock()
                .expect("hero protocols poisoned")
                .remove(url);
        }
        // Cache hit?
        if let Some(p) = self
            .protocols
            .lock()
            .expect("hero protocols poisoned")
            .get(url)
            .cloned()
        {
            return Some(p);
        }
        // Miss → build fresh from the decoded image.
        let picker = self.picker.as_ref()?.clone();
        let slot = self.slot(url);
        let st = slot.lock().expect("hero image state poisoned");
        let HeroState::Ready(img) = &*st else {
            return None;
        };
        let img_clone = (**img).clone();
        drop(st);
        let proto: Box<dyn StatefulProtocol> = picker.clone().new_resize_protocol(img_clone);
        let arc = Arc::new(Mutex::new(proto));
        self.protocols
            .lock()
            .expect("hero protocols poisoned")
            .insert(url.to_string(), arc.clone());
        Some(arc)
    }
}

async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let client = crate::http::shared();
    let resp = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?
        .error_for_status()
        .with_context(|| format!("{url} returned non-2xx"))?;
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading {url} bytes failed"))?;
    Ok(bytes.to_vec())
}

/// Pick the best inline-image protocol based on the current terminal.
/// Mirrors the env-var detection ratatui-image's `guess_protocol` uses
/// internally, but without writing escape sequences to stdout — we'd
/// rather not race with the active TUI render.
fn detect_protocol() -> ProtocolType {
    if let Ok(term) = std::env::var("TERM") {
        if term.contains("kitty") {
            return ProtocolType::Kitty;
        }
        if term == "mlterm" || term == "yaft-256color" {
            return ProtocolType::Sixel;
        }
    }
    if let Ok(tp) = std::env::var("TERM_PROGRAM") {
        if tp.contains("iTerm")
            || tp.contains("WezTerm")
            || tp.contains("mintty")
            || tp.contains("vscode")
            || tp.contains("Tabby")
            || tp.contains("Hyper")
        {
            return ProtocolType::Iterm2;
        }
    }
    if std::env::var("WEZTERM_EXECUTABLE").is_ok()
        || std::env::var("ITERM_SESSION_ID").is_ok()
        || std::env::var("LC_TERMINAL")
            .map(|v| v.contains("iTerm"))
            .unwrap_or(false)
    {
        return ProtocolType::Iterm2;
    }
    ProtocolType::Halfblocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_stable_and_filesystem_safe() {
        let k1 = cache_key("https://images.wsj.net/im-12345");
        let k2 = cache_key("https://images.wsj.net/im-12345");
        assert_eq!(k1, k2, "same URL → same key");
        assert!(k1.starts_with(IMAGE_CACHE_PREFIX));
        assert!(k1
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn cache_key_differs_per_url() {
        assert_ne!(
            cache_key("https://images.wsj.net/im-12345"),
            cache_key("https://images.wsj.net/im-67890")
        );
    }
}
