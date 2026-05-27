# glint — Terminal Dashboard

## What This Is

glint is a keyboard-driven terminal dashboard (Rust + Ratatui) that displays stocks, calendar, and news in a configurable grid layout. The full specification lives in `docs/glint-spec.md` — read it for widget mockups, UX flows, and detailed config examples.

## Tech Stack

- **Rust 2021 edition** with Tokio async runtime
- **Ratatui 0.28+** for TUI rendering (crossterm backend)
- **reqwest** for HTTP (Yahoo Finance, Google Calendar, RSS feeds, Anthropic API)
- **serde + toml** for config; **serde_json** for API responses
- **chrono + chrono-tz** for timezone-aware date/time
- **strsim** for fuzzy command matching
- Config format: **TOML** (not JSON, not YAML)

## Project Structure

```
src/
├── main.rs                  # Entry point, CLI parsing, runtime setup
├── app.rs                   # App state, focus model, command dispatch, live-reload
├── event.rs                 # Event loop: crossterm input + tick timer + config watch
├── cache/
│   └── mod.rs               # Persistent on-disk cache (JSON + bytes). See Cache Layer.
├── config/
│   ├── mod.rs               # Per-widget TOML loader; XDG paths; --init seeds
│   ├── layout.rs            # Grid layout parsing and resolution
│   ├── types.rs             # Top-level Config struct
│   └── watcher.rs           # `notify`-based config file watcher
├── auth/
│   ├── mod.rs               # credentials_dir helper
│   ├── registry.rs          # AuthProvider registry — --auth <name> looks up here
│   ├── loopback.rs          # localhost OAuth redirect listener
│   ├── google/              # Google OAuth client + token store
│   └── microsoft/           # Microsoft OAuth client + token store
├── widgets/
│   ├── mod.rs               # Widget trait, WidgetCtx, WidgetManager
│   ├── registry.rs          # WIDGETS table — add a descriptor to register
│   ├── clock/, weather/, calendar/, news/, stocks/, email/,
│   │       resources/, gallery/
│   │   └── mod.rs + helpers — each is a self-contained widget module
│   │      with `pub const KIND` and `pub fn build(&WidgetCtx)`.
├── llm/
│   ├── mod.rs               # LlmProvider trait, LlmRequest/LlmResponse types
│   ├── anthropic.rs         # AnthropicProvider (Messages API via reqwest)
│   ├── rate_limiter.rs      # Token-bucket request budget tracking
│   └── cache.rs             # In-memory LRU response cache (L1)
├── theme/
│   └── mod.rs               # Color scheme loader, per-widget overrides
├── geolocation.rs           # IP / name-based location lookup (weather, clock)
├── ui/
│   ├── mod.rs               # Top-level renderer, status + command bar layout
│   ├── command_bar.rs       # ":" command bar with fuzzy suggestions
│   ├── status_bar.rs        # Bottom bar: theme, command feedback
│   ├── help.rs              # "?" help overlay (sources keybindings from widgets)
│   └── big_digits.rs        # Block-digit renderer for clock + calendar
└── wizard/
    └── mod.rs               # --setup / first-run wizard (slated for refactor)
```

## Core Traits

Two traits define the extension points — know these before touching any
widget code:

### Widget (src/widgets/mod.rs)
The runtime contract for everything that lives in a grid cell. The full
trait is bigger than this excerpt (mouse, keybindings, shortcut prefs,
theme reload) — see the source for the full surface.
```rust
pub trait Widget: Send + Sync {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn kind(&self) -> &str;
    fn instance(&self) -> &str { "main" }
    async fn update(&mut self, ctx: &AppContext) -> Result<()>;
    fn render(&self, frame: &mut Frame, area: Rect, focused: bool);
    fn handle_key(&mut self, key: KeyEvent) -> EventResult;
    fn handle_mouse(&mut self, _: MouseEvent, _: Rect) -> EventResult { /* default ignored */ }
    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool>;
    fn config(&self) -> serde_json::Value;
    fn apply_config(&mut self, config: serde_json::Value) -> Result<()>;
    fn keybindings(&self) -> Vec<(&'static str, &'static str)> { vec![] }
    fn set_app_theme(&mut self, _: Arc<Theme>) {}
    fn shortcut_preferences(&self) -> &[char] { &[] }
    fn set_shortcut(&mut self, _: Option<char>) {}
}
```

### LlmProvider (src/llm/mod.rs)
Boundary between widgets and any LLM. `AnthropicProvider` is the only
concrete impl today; new providers slot in here.
```rust
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse>;
}
```

### WidgetCtx (src/widgets/mod.rs)
Construction-time bundle every widget factory receives:
```rust
pub struct WidgetCtx {
    pub instance: String,                       // "main" or the @-suffix
    pub theme: Arc<Theme>,
    pub llm: Option<Arc<dyn LlmProvider>>,      // None when LLM disabled / unconfigured
    pub cache: ScopedCache,                     // already scoped to (kind, instance)
}
```

## Adding a New Widget

1. Create `src/widgets/<name>/mod.rs` implementing the `Widget` trait.
2. Export at module level: `pub const KIND: &str = "<name>";` and
   `pub fn build(ctx: &WidgetCtx) -> Box<dyn Widget>` — the factory the
   registry will call.
3. Add a `widget-<name>` feature in `Cargo.toml` and a
   `#[cfg(feature = "widget-<name>")] pub mod <name>;` line in
   `src/widgets/mod.rs`.
4. Append a `WidgetDescriptor` to `WIDGETS` in `src/widgets/registry.rs`.

That's it. No edits to `app.rs`, `main.rs`, `widgets/mod.rs` beyond the
module declaration, or any other widget. The wizard step is still
centralised today (see TODO in `src/wizard/mod.rs`); a follow-up refactor
will let widget descriptors carry their own setup steps.

If your widget fetches remote data, use `ctx.cache` (see **Cache Layer**
above). If it needs OAuth, declare an `AuthRequirement` on the descriptor
and register the provider in `src/auth/registry.rs`. If it talks to an
LLM, accept the optional `ctx.llm` and gate via a per-widget TOML flag.

## Config System

All config lives at `~/.config/glint/`:

```
config.toml        — layout grid, global settings, theme overrides
stocks.toml        — watchlist tickers, indices, poll intervals
calendar.toml      — Google Calendar IDs, view mode, time format
news.toml          — RSS sources, topic keywords, priority order
llm.toml           — Anthropic model, per-feature toggles, rate limits
credentials/       — OAuth tokens (0600 perms), API keys
```

Config is layered: built-in defaults → user TOML → CLI flags → env vars.
Files are watched via `notify` crate; changes trigger live reload via `apply_config()`.

## LLM Integration

Follow the **structured-first, LLM-fallback** principle:

| Input type | Fast path (always try first) | LLM path (only if needed) |
|---|---|---|
| Company name → ticker | Yahoo Finance search API | Disambiguate if 2+ results (Haiku) |
| News relevance | Keyword substring match | Batch semantic classification (Sonnet) |
| Article summary | RSS description field | On-demand summarization (Sonnet) |
| Date parsing | two-timer / chrono-english crate | Not needed |
| Typo correction | strsim edit-distance | Not needed |
| City name (future) | Geocoding API | Disambiguate if 2+ results |

Every LLM feature MUST have a non-LLM fallback. No widget should break if the API key is missing or the service is down.

Per-feature model overrides are in `llm.toml` — use Haiku for simple classification, Sonnet for quality summarization.

## Cache Layer

Widgets that fetch remote data should persist results so the dashboard paints
prior values on the first frame and refreshes in the background. The platform
provides `src/cache/mod.rs` with two parallel APIs — JSON for structured
payloads and bytes for opaque blobs:

```rust
// JSON values (any T: Serialize + DeserializeOwned)
ctx.cache.load::<T>(key) -> Option<CacheEntry<T>>
ctx.cache.store(key, &value) -> Result<()>

// Raw bytes (images, attachments)
ctx.cache.load_bytes(key) -> Option<BytesEntry>
ctx.cache.store_bytes(key, &bytes) -> Result<()>

// Maintenance
ctx.cache.invalidate(key) -> Result<()>   // clears both .json and .bin variants
```

`ctx.cache` is already scoped to the widget's `(kind, instance)`. Pick any
flat key namespace per widget (`articles`, `quotes-1d`, `messages`,
`thumb-<hash>`). Files land at
`~/.cache/glint/<kind>/<instance>/<key>.{json,bin}`. Writes are atomic
(temp + rename); a crash mid-write can't corrupt an existing entry.

### Fetch-payload pattern (reference: `news/mod.rs`)

1. In the constructor, `cache.load::<T>(key)` and seed the in-memory state.
2. Translate `entry.age()` into a synthetic `Instant` so the existing
   `last_attempt` poll-interval gate suppresses redundant refetches:
   `last_attempt = Some(Instant::now() - entry.age().min(poll_interval))`.
3. After a successful fetch, `cache.store(key, &payload)`. Failures log and
   continue — caching is best-effort.

### LLM-derivation pattern (reference: `news/mod.rs`, `email/mod.rs`)

Cache per-record derivations (article summaries, message summaries) when
they're content-stable. Key by a SHA-256 prefix of the record's identity
(article URL, message ID). Only persist successful outcomes — flaky network
calls shouldn't poison the cache.

```rust
fn summary_cache_key(id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(id.as_bytes());
    let mut k = String::from("summary-");
    for b in &digest[..8] {
        use std::fmt::Write;
        let _ = write!(k, "{b:02x}");
    }
    k
}
```

Whether to cache an LLM response is a per-widget call: stable derivations
(an article summary, an email summary) are good candidates; time-sensitive
or query-shaped answers ("what's the price of AAPL right now?") are not.

### Bytes-payload pattern (reference: `gallery/mod.rs`)

For files that take longer to decode than to read (resized images,
attachment renders), cache the heavy output keyed by source-path hash and
invalidate against the source file's mtime:

```rust
let key = thumb_cache_key(path);  // sha256(path)[..8]
if let Some(entry) = cache.load_bytes(&key) {
    let stored: SystemTime = entry.stored_at.into();
    let src_mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
    // Map_or(true, ...) — when the source mtime is unreadable (file moved,
    // disconnected drive), the cache is the best signal we have. Serve it.
    if src_mtime.map_or(true, |m| stored >= m) {
        return Ok(load_from_cached_bytes(&entry.value));
    }
}
// cache miss / stale → decode source, store result
cache.store_bytes(&key, &encoded)?;
```

`BytesEntry.stored_at` comes from file mtime (one less write than embedding
a timestamp). The pattern degrades gracefully: if a previously indexed
source vanishes between runs, the cache continues serving the last good
version until the user clears it.

## Key Architectural Decisions

- **Graph rendering**: braille characters (U+2800–U+28FF) by default, `graph_style = "box_drawing"` fallback
- **Colors**: ANSI semantic colors (Red, Green, etc.) — inherits from terminal theme. Optional `[theme]` override in config.toml
- **Cache**: persistent JSON under `~/.cache/glint/`; widgets seed on construction, refresh in background, persist on success. See **Cache Layer** above.
- **Command routing**: focused widget gets priority; startup warning on prefix conflicts; `widget_id:command` for disambiguation
- **Auth storage**: plain files with 0600 perms in credentials/ dir (like gcloud, gh)
- **Calendar**: merged multi-calendar timeline with per-calendar color coding

## Commands

```
cargo build                  # Build
cargo run                    # Run with default config
cargo run -- --init          # First-run setup (creates ~/.config/glint/)
cargo test                   # Run tests
cargo clippy                 # Lint
cargo fmt                    # Format
```

## Conventions

- Use `Result<T>` with `anyhow` for error handling (not custom error types in v1)
- All async code runs on Tokio; never block the event loop
- Widget rendering is synchronous (Ratatui requirement) — data fetching is async
- Prefer `tracing` over `println!` / `eprintln!` for logging
- TOML config structs derive `serde::Deserialize` with `#[serde(default)]` for optional fields
- Test data providers with mock HTTP responses (use `wiremock` or similar)

