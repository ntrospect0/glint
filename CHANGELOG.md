# Changelog

All notable changes to glint are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions track
the `Cargo.toml` `version` field.

## [0.2.0] — unreleased

The 0.2 release is the structural refactor of glint into a plugin-style
platform: registries for widgets, auth providers, and LLM providers
became the documented seams for community contributions. The bulk of
this entry captures changes since the initial 0.2 plan — the widget
catalogue grew, two more providers landed, and the runtime got tighter
on memory, HTTP, and rendering.

### Added

#### Widgets
- **Forex widget** (`widget-forex`). Watchlist of fiat pairs against
  a configurable primary, intraday + multi-year graphs, period toggle
  shared with Stocks, swap-primary with `s` or Enter (`:fx <code>`
  from the command bar). Pre-launch: also supports **crypto** via a
  separate `crypto_watchlist` config field. Renders Currencies + Crypto
  as visually separated sections.
- **Notes widget** (`widget-notes`, internally `notes`; was briefly
  named `sticky` during development). Vim-flavoured multi-note pad:
  normal mode for view/navigation (no cursor), insert mode for
  editing (blinking cursor). Per-note undo / redo (100-entry cap,
  preserved across note switches), `Ctrl-A` / `Ctrl-E` line jumps,
  `Ctrl-U` delete-line, `Ctrl-Z` / `Ctrl-Shift-Z` undo/redo, mouse
  click to position cursor in insert mode. Per-note `.md` files under
  `~/.config/glint/notes/<instance>/` — users can `cat`, hand-edit,
  back up, or git-track. Multi-instance via `notes@<instance>`.
- **Email widget** (`widget-email`). Unified inbox preview across
  Gmail (OAuth), Outlook (OAuth), and IMAP (app password) — adding
  IMAP brought every standards-compliant mailbox (iCloud, Fastmail,
  Yahoo, self-hosted) into scope without OAuth. Optional per-message
  LLM summaries via `s`; falls back to first N chars of plain body.
- **Stack widget**. Composite cell holding 2+ child widgets with a
  tab-strip header; `.` / `,` rotate the active widget; mouse click on
  a tab switches directly. `Shift+<letter>` focus dispatcher walks
  into stacks. Wizard surfaces a new `AssignStack` sub-page for
  multi-widget cells. Layout cell uses `widgets = [...]` instead of
  `widget = "..."` for stacks. Stack delegates keys to the active
  child first, so widgets that consume text input (Notes in insert
  mode) aren't interrupted by stack-level chord interpretation.

#### Providers + Auth
- **OpenAI LLM provider** alongside Anthropic. `LlmProviderDef`
  registry replaces a hardcoded match arm — adding a third provider
  is one struct literal. Wizard's Global page now picks the active
  provider and writes the matching `credentials/<provider>_key.toml`.
  Default OpenAI model is `gpt-5-mini`; freely overridable in
  `llm.toml` since the field is sent verbatim to the Chat
  Completions API.
- **`AuthProvider` registry consolidation.** Provider knowledge that
  was scattered across four files (wizard credential capture,
  OAuth-template seeding, post-auth folder fetch, OAuth setup form
  schema) folds into a single self-describing registry entry per
  provider. IMAP joined the registry instead of living as a special
  case.

#### Forex / Stocks
- **Crypto support** in Forex. `provider::CRYPTO_CODES` registry maps
  symbols to Yahoo's hyphenated `BTC-USD` URL convention; mixed
  fiat/crypto pairs work via the new USD-pivot path.
- **USD-pivot for every Forex cross pair.** `R(a, b) = R(a, USD) /
  R(b, USD)` with each leg fetched in Yahoo's natural direction
  (`xUSD=X` for fiat, `x-USD` for crypto). Eliminates the sparse /
  404 cases on direct crypto-to-non-USD pairs like `BTC-EUR`.
- **`USD-CRYPTO` direction inversion.** Yahoo only lists `BTC-USD`,
  never `USD-BTC`; the direct fetch path now detects the inverted
  case, fetches `BTC-USD`, and reciprocates the rate so USD-as-primary
  + crypto-as-alternate works.
- **Swap-to-crypto seeds amount = 1.** Buying-power preservation
  makes no sense for crypto primaries; you always want "what is 1 BTC
  worth in X" as the immediate read.
- **Configured primary stays anchored across multi-hop swaps.** The
  user's home-base currency now stays at the top of the Currencies
  section regardless of how many primary-swaps they walk through.
- **Disk cache only persists when live primary matches configured.**
  Earlier closing-on-non-USD-primary seeded the next launch with the
  wrong-direction symbol set and every row rendered blank until the
  first fresh fetch returned.

#### Platform
- **Setup wizard redesign**. Full TUI wizard (was: stdio-only) with
  layout preset picker, per-widget setup pages driven by each widget's
  `WizardDescriptor`, inline OAuth credentials capture, post-auth
  remote-options fetch (Gmail labels, Outlook folders, IMAP folders
  pre-populated into the picker), resume buffer, and per-page Enter
  / [Save & Next] semantics. Re-runs with `glint --setup` are safe;
  the wizard preserves keys it doesn't manage.
- **Unified title bar**. Every widget gets a `▶ … ◀` focus chevron
  pair, the shortcut letter painted into the title text, and a
  right-aligned metadata suffix (Weather's location, Email's account,
  News's article count). Width-aware metadata hides on narrow panes.
  Theme split: `widget_title.focused` / `widget_title.unfocused` +
  `metadata.focused` / `metadata.unfocused`, with helpers for the
  three-tier brightness (dim / plain / brilliant).
- **Shared HTTP client** (`src/http.rs`). Eleven call sites
  (LLM providers, calendar, email, news, weather, geolocation, OAuth
  flows) now use a single process-wide `reqwest::Client` instead of
  each constructing their own. Saves TLS pool memory and lets
  keepalive work across widgets. Stocks/Forex (Yahoo cookie jar) and
  CalDAV (per-request Basic auth) keep bespoke clients.
- **Cache disk sweep**. `Cache::sweep_older_than(30d)` runs at app
  startup to drop orphan cache files left by removed widgets / renamed
  instances.
- **`+ new note · - delete` footer** in the Notes list pane.
- **Brightness signal in Notes**: body text dims when content isn't
  focused; brilliant in insert mode; plain in normal mode. Divider
  between list + content uses the same three-tier scheme.

### Changed

- **Renamed widget kind `sticky` → `notes`** (display name was
  already "Notes"). Module path, Cargo feature, type names, config
  filenames, and layout-cell values all moved. No back-compat
  aliases — pre-launch.
- **Project relicensed to GPL v3-or-later** (was MIT). `LICENSE`
  carries the canonical FSF text; every `.rs` file under `src/` has an
  SPDX header; `CONTRIBUTING.md` documents the DCO sign-off
  requirement and the contributor relicensing grant.
- **LLM toggles moved into per-widget TOMLs.** `summarize_with_llm`
  lives in `news.toml` / `email.toml` instead of a central `llm.toml`
  `[features]` block. The LLM layer is now widget-agnostic.
- **`AGENTS.md` replaces `CLAUDE.md`** as the contributor onboarding
  doc — vendor-neutral naming that other agent toolchains recognise.
- **Default config layout reflects the registry.** Widget registration
  is a single `WidgetDescriptor` entry instead of edits in `app.rs`.
- **News widget fetches article body for richer summaries.** When
  `s` requests an LLM summary, glint can fetch the article page and
  feed its extracted body to the LLM instead of just the RSS excerpt.
  Per-feed `fetch_body = true/false` override; widget-wide
  `fetch_body_for_summary` default. Uses `readability` for extraction.
- **Stocks + Forex series downsampled** to 240 evenly-spaced points
  before they hit memory and disk cache. 10Y daily traces compress
  10× with no perceptible chart-quality loss at TUI resolutions.
- **News in-memory summaries pruned on refresh**. Summaries for
  articles that rotated out of the feed get dropped from memory; the
  summary text remains on disk so re-encounters reload transparently.
- **Email in-memory body length capped** at 4 KB. The expanded view
  caps at 5 lines and the full-message read happens via `o` opening
  the user's mail client — no need to keep multi-KB HTML-stripped
  bodies resident for every message.
- **LLM cache trimmed**. Default capacity 1024 → 128 entries; entries
  now expire 7 days after insertion.
- **List column in Forex is dynamic-width**: targets 28 cells, capped
  at 35% of pane width. Below 60 cells the list auto-hides.

### Removed

- **Single-provider calendar config form.** `provider = "..."` and the
  top-level `calendar_ids` field are gone; use `[[providers]]` blocks.
- **Stocks `display_mode`, clock `tz` / integer-form `hour_format`
  aliases**. The canonical names (`default_display_mode`, `timezone`,
  string `"24h"`) are the only accepted forms.
- **`DataProvider` trait** (`src/providers/`). Implemented but never
  dispatched through; widgets call their concrete providers directly.
- **`--auth outlook`** alias for `--auth microsoft`. Use the registry name.
- **`docs/` folder.** `docs/glint-spec.md` + `docs/stack-spec.md` were
  scratchpads from the initial design phase that drifted from the
  implementation. Their content is captured (and kept accurate) in
  `README.md` and `AGENTS.md`.
- **`dd` in Notes normal mode**. Normal mode is now view-only (no
  cursor); line editing happens in insert mode with `Ctrl-U`.

### Fixed

- **Stack pane swallowing typed `.` and `,`**. Previously, typing
  text containing periods or commas into a widget inside a stack
  rotated the stack on every such character. The stack now offers
  the key to its active child first; rotation only fires when the
  child returns `Ignored`.
- **Forex `USD-CRYPTO` symbol 404s**. See "USD-CRYPTO direction
  inversion" above.
- **Forex configured primary disappearing after a second swap**.
  Multi-hop swaps now anchor `config.primary` to position 0 of its
  category every time.
- **Forex blank rows on relaunch after non-USD primary**. Disk cache
  only writes when live primary matches configured.
- **Memory bloat**. Bounded every accumulating in-memory store
  (News summaries, Email bodies, Stocks/Forex series, Gallery
  thumbnails, LLM cache), consolidated 11 HTTP clients into one
  shared instance, added a 30-day cache sweep on startup. Steady-state
  RSS dropped by roughly a third on the reference dashboard.

### Deferred

- **Custom-page wizard escape hatch** for third-party widgets that
  need a multi-step setup the declarative `WizardField` schema can't
  express.
- **Dynamic widget registry** for out-of-tree widget crates (inventory
  / linkme-style runtime registration). Add when a concrete community
  widget needs it.
- **`v0.2.0` cut.** The version field bumps when README + INSTRUCTIONS
  + AGENTS + LICENSE land as a final polish pass and the first GitHub
  release is tagged.
