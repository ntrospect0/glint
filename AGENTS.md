# glint — Terminal Dashboard

## What This Is

glint is a keyboard-driven terminal dashboard (Rust + Ratatui) that
displays clock, weather, calendar, news, email, stocks, forex (with
crypto), system resources, an image gallery, and a vim-flavoured notes
pad in a configurable grid layout. Cells can be single widgets or
**stacks** that rotate between multiple widgets. Pre-launch v0.2;
shipping under GPL v3-or-later (see `LICENSE` + `CONTRIBUTING.md`).

For user-facing setup, see `README.md` and `INSTRUCTIONS.md`.

## Tech Stack

- **Rust 2021 edition** with Tokio async runtime
- **Ratatui 0.28+** for TUI rendering (crossterm backend)
- **reqwest** for HTTP (Yahoo Finance, Open-Meteo, Google APIs, Microsoft
  Graph, RSS feeds, IMAP via `imap` crate, Anthropic + OpenAI APIs).
  A single process-wide client lives in `src/http.rs::shared()`.
- **serde + toml** for config; **serde_json** for API responses
- **chrono + chrono-tz** for timezone-aware date/time
- **strsim** for fuzzy command matching
- **ratatui-image + image** for the Gallery widget's inline rendering
- **imap + native-tls + mail-parser** for the Email widget's IMAP path
- **readability + url** for the News widget's optional article-body
  extraction (LLM summaries)
- **sysinfo** for the Resources widget's CPU / memory / process info
- **lru + sha2** for in-memory caches keyed by content hash
- Config format: **TOML** (not JSON, not YAML)

## Project Structure

```
src/
├── main.rs                  # Entry point, CLI parsing (clap), runtime setup
├── app.rs                   # App state, focus model, command dispatch, live-reload
├── event.rs                 # Event loop: crossterm input + tick + config watch
├── http.rs                  # Process-wide shared reqwest::Client (`shared()`)
├── clipboard.rs             # OSC-52 clipboard write helper
├── geolocation.rs           # IP / name-based location lookup (weather, clock)
├── runtime_state.rs         # Per-process state persisted to ~/.config/glint/.runtime_state.toml
├── cache/
│   └── mod.rs               # Persistent on-disk cache (JSON + bytes). See Cache Layer.
├── config/
│   ├── mod.rs               # Per-widget TOML loader; XDG paths; --init seeds
│   ├── layout.rs            # Grid layout parsing and resolution
│   ├── types.rs             # Top-level Config struct
│   └── watcher.rs           # `notify`-based config file watcher
├── auth/
│   ├── mod.rs               # credentials_dir helper
│   ├── registry.rs          # AuthProvider registry — self-describing entries
│   │                        #   carry display_name, credentials_spec, post_auth_refresh
│   ├── loopback.rs          # localhost OAuth redirect listener
│   ├── google/              # Google OAuth client + token store
│   └── microsoft/           # Microsoft OAuth client + token store (PKCE)
├── widgets/
│   ├── mod.rs               # Widget trait, WidgetCtx, WidgetManager
│   ├── registry.rs          # WIDGETS table — add a descriptor to register
│   ├── stack.rs             # StackWidget — composite cell holding N child widgets
│   │                        #   with tab-strip rotation via . / ,
│   ├── clock/, weather/, calendar/, news/, stocks/, forex/, email/,
│   │       resources/, gallery/, notes/
│   │   └── mod.rs + helpers — each is a self-contained widget module
│   │      with `pub const KIND` and `pub fn build(&WidgetCtx)`.
├── llm/
│   ├── mod.rs               # LlmProvider trait, LlmProviderDef registry,
│   │                        #   LlmRequest/LlmResponse types
│   ├── anthropic.rs         # AnthropicProvider (Messages API)
│   ├── openai.rs            # OpenAiProvider (Chat Completions API)
│   ├── rate_limiter.rs      # Token-bucket request budget tracking
│   └── cache.rs             # In-memory LRU response cache (L1), TTL-evicted
├── theme/
│   └── mod.rs               # Color scheme loader, per-widget overrides
├── ui/
│   ├── mod.rs               # Top-level renderer, unified title row helper,
│   │                        #   command bar, status bar wiring
│   ├── status_bar.rs        # Bottom bar: theme, command feedback, clock
│   ├── help.rs              # "?" help overlay (sources keybindings from widgets)
│   └── big_digits.rs        # Block-digit renderer for clock
└── wizard/
    ├── mod.rs               # --setup TUI wizard entry
    ├── app.rs               # Wizard event loop + page dispatcher
    ├── descriptor.rs        # WizardField / WizardFieldKind types
    ├── flow.rs              # Page ordering (Welcome → Global → Layout → …)
    ├── hydrate.rs           # Seed wizard state from existing on-disk config
    ├── finalize.rs          # Write final TOMLs at "Complete and Save"
    ├── state.rs             # In-memory wizard state buffer
    ├── storage.rs           # Resume-buffer persistence
    ├── style.rs, toml_merge.rs
    └── pages/               # One renderer per page (welcome, global, layout,
                             #   assign, widget, oauth_setup, assign_stack,
                             #   confirm, preview)
```

## Core Traits and Registries

Five extension points define the architecture — know these before
touching anything material.

### Widget (src/widgets/mod.rs)
The runtime contract for everything that lives in a grid cell. The
trait is broader than this excerpt (mouse, keybindings, shortcut prefs,
theme reload, composite-child plumbing for stacks) — see the source
for the full surface.

```rust
pub trait Widget: Send + Sync {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn kind(&self) -> &str;
    fn instance(&self) -> &str { "main" }
    async fn update(&mut self, ctx: &AppContext) -> Result<()>;
    fn render(&self, frame: &mut Frame, area: Rect, focused: bool);
    fn handle_key(&mut self, key: KeyEvent) -> EventResult;
    fn handle_mouse(&mut self, _: MouseEvent, _: Rect) -> EventResult { /* ignored */ }
    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool>;
    fn config(&self) -> serde_json::Value;
    fn apply_config(&mut self, config: serde_json::Value) -> Result<()>;
    fn keybindings(&self) -> Vec<(&'static str, &'static str)> { vec![] }
    fn set_app_theme(&mut self, _: Arc<Theme>) {}
    fn shortcut_preferences(&self) -> &[char] { &[] }
    fn set_shortcut(&mut self, _: Option<char>) {}
    fn shortcut(&self) -> Option<char> { None }
    fn title_metadata(&self) -> Option<String> { None }
    fn composite_children(&self) -> Vec<String> { vec![] }
    // ... (more composite hooks used by stack.rs)
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

### LlmProvider + LlmProviderDef registry (src/llm/mod.rs)
The provider trait is the runtime boundary; `LlmProviderDef` is the
registry entry that lets the wizard surface a provider picker and the
config layer dispatch builds correctly.

```rust
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse>;
}

pub struct LlmProviderDef {
    pub name: &'static str,                    // matched against llm.toml [provider].name
    pub display_name: &'static str,            // wizard picker label
    pub credentials_filename: &'static str,    // under credentials/
    pub key_portal_url: &'static str,
    pub default_model: &'static str,
    pub default_api_base: &'static str,
    pub default_max_tokens: u32,
    pub builder: LlmBuilder,                   // fn(&ProviderConfig, LimitsConfig) -> Result<Option<Arc<dyn LlmProvider>>>
}

pub const PROVIDERS: &[LlmProviderDef] = &[ /* anthropic, openai */ ];
```

Add a new LLM provider by appending one entry to `PROVIDERS`.

### AuthProvider registry (src/auth/registry.rs)
Self-describing OAuth + credential providers. Entries carry their
credentials spec (filename, required-keys placeholder check, starter
template), an inline-form setup schema for the wizard, and an optional
post-auth refresh callback that populates downstream pickers (e.g.
Gmail labels, Outlook folders, IMAP folders).

```rust
pub struct AuthProvider {
    pub name: &'static str,
    pub display_name: &'static str,
    pub run: AuthFlow,
    pub credentials: Option<&'static CredentialsSpec>,
    pub post_auth_refresh: Option<PostAuthRefresh>,
}
```

`--auth <name>` looks up by `name`. Widgets declare an `AuthRequirement`
on their `WidgetDescriptor` so the wizard knows what to prompt for.

### WidgetDescriptor + registry (src/widgets/registry.rs)
The single registration point for widget kinds:

```rust
pub struct WidgetDescriptor {
    pub kind: &'static str,
    pub factory: WidgetFactory,                // &WidgetCtx -> Box<dyn Widget>
    pub default_in_first_run: bool,
    pub auth_requirements: &'static [AuthRequirement],
    pub wizard: fn() -> WizardDescriptor,      // declarative wizard schema
}

pub const WIDGETS: &[WidgetDescriptor] = &[ /* ... 10 entries ... */ ];
```

## Adding a New Widget

1. Create `src/widgets/<name>/mod.rs` implementing the `Widget` trait.
2. Export at module level: `pub const KIND: &str = "<name>";`,
   `pub fn build(ctx: &WidgetCtx) -> Box<dyn Widget>`, and
   `pub fn wizard_descriptor() -> WizardDescriptor`.
3. Add a `widget-<name>` feature in `Cargo.toml` and a
   `#[cfg(feature = "widget-<name>")] pub mod <name>;` line in
   `src/widgets/mod.rs`. Add to the `widgets-all` umbrella.
4. Append a `WidgetDescriptor` to `WIDGETS` in
   `src/widgets/registry.rs`.

That's it. No edits to `app.rs`, `main.rs`, or the wizard driver are
needed. The wizard reads each widget's `wizard_descriptor()` for its
setup-form fields; auth prompts come from the descriptor's
`auth_requirements`; the cache scope is granted automatically by the
factory contract.

If your widget fetches remote data, use `ctx.cache` (see **Cache
Layer** below) and `crate::http::shared()` for the HTTP client unless
you need bespoke session state (Yahoo's cookie jar, CalDAV's basic-auth
headers — those construct their own `reqwest::Client`).

If it needs OAuth, declare an `AuthRequirement` on the descriptor and
register the provider in `src/auth/registry.rs` (or extend an existing
entry's `post_auth_refresh` if you need to populate a picker).

If it talks to an LLM, accept the optional `ctx.llm` and gate on a
per-widget TOML flag (`summarize_with_llm = true`).

## Config System

All config lives at `~/.config/glint/`:

```
config.toml           — [global], [layout] grid, [[layout.cells]] placements
colorschemes.toml     — named [schemes.*] palettes
clock.toml            — primary tz, world clocks
weather.toml          — lat/lon, units, forecast days
calendar.toml         — providers, calendar_ids, [[events]]
news.toml             — [[feeds]], [[topics]], summarize_with_llm
stocks.toml           — indices, watchlist, default period
forex.toml            — primary, watchlist, crypto_watchlist
email.toml            — provider, folders, summarize_with_llm
resources.toml        — poll cadence, top-N processes
gallery.toml          — image globs, rotation interval, rescan interval
notes.toml            — per-instance shortcut + colour overrides
llm.toml              — [provider] name (anthropic / openai), model, [limits]
notes/<instance>/     — one .md per note; mtime sorts the list
credentials/          — OAuth tokens, API keys, IMAP/CalDAV passwords (0600)
```

Cells in `config.toml` reference widgets as `kind` (single) or
`widgets = [kind1, kind2, …]` (stack). The `kind@instance` shorthand
selects a non-default config file (`stocks@watchlist1.toml`,
`clock@home.toml`, etc.).

Config is layered: built-in defaults → user TOML → CLI flags → env vars.
Files are watched via `notify`; changes trigger live reload via
`apply_config()`. The user can also force reload with `:reload`.

## LLM Integration

Two providers ship today (Anthropic + OpenAI). Add more via the
`LlmProviderDef` registry. The active provider is set by
`llm.toml`'s `[provider].name`; widgets call through the trait without
knowing which concrete provider is active.

Follow the **structured-first, LLM-fallback** principle:

| Input type | Fast path (always try first) | LLM path (only if needed) |
|---|---|---|
| Company name → ticker | Yahoo Finance search API | Disambiguate if 2+ results |
| News relevance | Keyword substring match | (Reserved for batch classification) |
| News article body | RSS `<description>` field | Page-extract via `readability` then summarise |
| Email summary | First N chars of plain_body | On-demand `s`-key summary |
| Date parsing | chrono / chrono-english | Not needed |
| Typo correction | strsim edit-distance | Not needed |

Every LLM feature MUST have a non-LLM fallback. No widget should break
if the API key is missing or the service is down.

LLM responses are cached in two tiers: an in-memory LRU (128 entries,
7-day TTL) in `src/llm/cache.rs`, and a per-widget on-disk cache keyed
by `sha256(record-id)` for content-stable derivations.

## Cache Layer

Widgets that fetch remote data should persist results so the dashboard
paints prior values on the first frame and refreshes in the background.
`src/cache/mod.rs` provides two parallel APIs — JSON for structured
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

`ctx.cache` is already scoped to the widget's `(kind, instance)`. Pick
any flat key namespace per widget (`articles`, `quotes-1d`, `messages`,
`thumb-<hash>`). Files land at
`~/.cache/glint/<kind>/<instance>/<key>.{json,bin}`. Writes are atomic
(temp + rename); a crash mid-write can't corrupt an existing entry.

On startup the app runs `Cache::sweep_older_than(30d)` to drop orphan
cache files left by removed widgets or renamed instances.

### Fetch-payload pattern (reference: `news/mod.rs`)

1. In the constructor, `cache.load::<T>(key)` and seed the in-memory state.
2. Translate `entry.age()` into a synthetic `Instant` so the existing
   `last_attempt` poll-interval gate suppresses redundant refetches:
   `last_attempt = Some(Instant::now() - entry.age().min(poll_interval))`.
3. After a successful fetch, `cache.store(key, &payload)`. Failures log
   and continue — caching is best-effort.

### LLM-derivation pattern (reference: `news/mod.rs`, `email/mod.rs`)

Cache per-record derivations (article summaries, message summaries)
when they're content-stable. Key by a SHA-256 prefix of the record's
identity (article URL, message ID). Only persist successful outcomes —
flaky network calls shouldn't poison the cache.

### Bytes-payload pattern (reference: `gallery/mod.rs`)

For files that take longer to decode than to read (resized images,
attachment renders), cache the heavy output keyed by source-path hash
and invalidate against the source file's mtime.

## Memory + HTTP Discipline

Two cross-cutting concerns the platform takes care of so individual
widgets don't reinvent them.

### Shared HTTP client (src/http.rs)

`crate::http::shared()` returns a process-wide `reqwest::Client` (glint
UA, 30s timeout, no cookies). Eleven call sites use it — news, weather,
calendar (Google + Outlook), email (Gmail + Outlook), LLM providers,
geolocation, OAuth flows. Per-request timeout overrides via
`RequestBuilder::timeout` where a shorter bound matters.

Bespoke clients deliberately kept for callers needing client-scoped state:
- **stocks + forex** (Yahoo): browser-shaped UA + cookie store (CSRF
  cookie required on chart endpoints).
- **calendar/caldav**: per-request Basic auth header pushed via
  `default_headers` — would leak onto unrelated requests if shared.

### Bounded in-memory state

Every widget that accumulates data has a defined upper bound:
- News: drops summaries for articles that rotated out of the feed.
- Email: caps in-memory body length at 4 KB (full message read via `o`
  opens the user's mail client).
- Stocks + Forex: downsample multi-year daily series to 240 points
  before they hit memory + disk cache.
- Gallery: caps to 60 slides; the loader truncates when more match.
- LLM cache: 128 entries with 7-day TTL.

When you add a new widget, do the same — don't ship an unbounded
collector.

## Key Architectural Decisions

- **Graph rendering**: braille characters (U+2800–U+28FF) for the
  intraday + multi-period traces. Box-drawing fallback available.
- **Colours**: ANSI semantic colours (Red, Green, etc.) for default
  text inherit from the terminal theme. Each colour scheme in
  `colorschemes.toml` overrides those defaults; per-widget `[colors]`
  blocks further override per pane.
- **Cache**: persistent JSON + bytes under `~/.cache/glint/`; widgets
  seed on construction, refresh in background, persist on success.
- **Command routing**: focused widget gets priority; `:cmd` falls
  through to widgets in registration order. `widget_id:command` for
  explicit targeting.
- **Auth storage**: plain files with 0600 perms in `credentials/`
  (mirroring `gcloud`, `gh`, etc.). A post-v0.2 refactor moves this
  behind a `CredentialBackend` trait with three tiers — OS keychain
  (`keyring` crate), host-bound AES-GCM (key from `machine-uid`), and
  the current plaintext file as fallback — selected via
  `credentials_backend` in `config.toml`. See `CHANGELOG.md` →
  Deferred for the work breakdown. Pick:
  - **Keychain**: default on macOS, Windows, Linux desktop with a
    running Secret Service daemon.
  - **Host-bound**: default on Linux headless / SSH-only / homelab
    where Secret Service is unavailable. Leak-resistant (file is
    useless if copied off-host) but does NOT defend against local
    processes that can read the machine-ID.
  - **Plaintext**: universal fallback. Add Windows ACL handling
    before relying on it as the documented default there — `chmod
    0600` is a no-op on NTFS.
- **Calendar**: merged multi-calendar timeline with per-calendar colour
  coding.
- **Notes**: one `.md` file per note under
  `~/.config/glint/notes/<instance>/`; user can hand-edit, back up via
  git, or move between machines.
- **Forex**: USD-pivot for all cross pairs (`R(a, b) = R(a, USD) / R(b, USD)`).
  Handles fiat-fiat, fiat-crypto, crypto-fiat, crypto-crypto uniformly.

## Commands

```sh
cargo build --features widgets-all          # debug build, all widgets
cargo run                                    # debug binary with default config
cargo run -- --init                          # seed ~/.config/glint/
cargo run -- --setup                         # interactive wizard
cargo run -- --auth <provider>               # OAuth / credential flow
cargo test --features widgets-all            # full suite (~465 tests)
cargo clippy --features widgets-all          # lint
cargo fmt                                    # format
make install PREFIX=~/.local                 # build + copy to ~/.local/bin
```

## Conventions

- Use `Result<T>` with `anyhow` for error handling (no custom error
  types in v0.x).
- All async code runs on Tokio; never block the event loop. Use
  `tokio::task::spawn_blocking` for sync APIs like the `imap` crate.
- Widget rendering is synchronous (Ratatui requirement) — data
  fetching is async and writes results into shared state.
- Prefer `tracing` over `println!` / `eprintln!` — alt-screen mode
  would corrupt the TUI; tracing writes to `~/.config/glint/glint.log`.
- TOML config structs derive `serde::Deserialize` with `#[serde(default)]`
  for optional fields.
- Test data providers with mock HTTP responses (`wiremock` is in
  dev-deps).
- Every `.rs` file under `src/` carries an SPDX header
  (`// SPDX-License-Identifier: GPL-3.0-or-later`). The script in the
  prelaunch commit history is idempotent — re-run after adding a new
  file if your editor doesn't auto-insert.
- Comments default to **none**. Add one only when the WHY is non-obvious
  (hidden constraint, subtle invariant, workaround). Don't restate
  what well-named code already says.
