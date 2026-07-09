# Changelog

All notable changes to glint are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions track
the `Cargo.toml` `version` field.

## [0.5.0] — unreleased

**Focus Zoom + responsive widget views.** Press `z` to enlarge the focused
widget into a framed overlay, and every widget now renders a richer view when
it's given a large (zoomed) pane — while normal grid rendering is left exactly
as before.

### Added

- **Focus Zoom** (`z` / `Shift-Z`): enlarge the focused widget to a centered
  ~90% frame over a dimmed backdrop; the home cell shows a placeholder (never a
  second live copy); state is fully preserved on enter/exit; layered `Esc`;
  `Shift-<letter>` / `Tab` / mouse retargeting; configurable `zoom_margin`
  (CSS-style, with fallback); live-reload safe.
- **`ViewTier` framework** (`Compact`/`Standard`/`Expanded`/`Full`) plus shared
  UI helpers (`row_split`, `CardGrid`, `range_bar`) so widgets can adapt to the
  space they are given. Documented for widget authors in
  [`docs/widget-sdk.md`](docs/widget-sdk.md) → *Responsive views & Focus Zoom*.
- **Zoomed (Full-tier) views** — all Full-tier only, unzoomed rendering
  unchanged:
  - **Stocks** — fundamentals panel.
  - **Email** — split list + full-message read pane, with robust HTML→text
    body rendering (via `html2text`): comments (incl. Outlook MSO conditionals),
    `<script>`/`<style>`, and zero-width preheader junk are dropped, the full
    entity set is decoded, and lists/links/blockquotes get readable structure.
  - **Forex** — 52-week range bar (highlight-coloured) in the details column.
  - **Weather** — side-by-side city columns (home pinned, `←/→` scroll,
    prefetch-all-cities), each with a 24h temperature chart (high/low labelled),
    rain-probability bar, and a 7-day forecast.
  - **Clock** — large-digit world-clock grid (bordered cards, day/date +
    day/night glyph), stopwatch lap table, timer burn-down bar.
  - **Resources** — a decorated CPU-history chart (filled braille area with a
    green→amber→red utilisation gradient, a `%` y-axis with gridlines, and a
    time x-axis), taller process list with selection, extra columns.
  - **Feeds** — article panel expanded by default when zoomed.
  - **Calendar** — zoomed Day/Week views place agenda columns above a 3-month
    reference block (prev · current · next) whose days carry event dots
    colour-coded by calendar and are click-to-navigate; a zoomed **wall-calendar
    Month view** with bordered day cells, per-day busyness dots (count scaled to
    the day's events, split across calendars by colour), a bordered month-title
    box, the real current month always accented, clickable days, and arrow-key
    day navigation (`←↑↓→` walk the day, `h`/`l` page months, `j`/`k` scroll the
    agenda). Event data is fetched across the visible months.

### Fixed

- Gallery images render centered (top-aligned) and are never upscaled.
- Weather forecast lines no longer garble on wide panes (cell-width-correct
  layout).
- Mouse input on the dashboard no longer lags: pointer motion/drag reports are
  dropped at the source, inert clicks/scrolls don't force a repaint, and bursts
  of events coalesce into a single redraw — so clicks feel as immediate as
  keyboard navigation, even on the heavy zoomed calendar.

## [0.4.0] — unreleased

**Profiles.** `glint --profile <name>` (or `-p <name>`, or
`GLINT_PROFILE`) runs an isolated config tree, so one machine can hold a
focused **work** dashboard, a stripped-down **travel** view, etc. — each
with its own layout, widgets, theme, and accounts. Without a profile,
glint uses `"default"`.

### Added

- **Per-profile config trees** under `~/.config/glint/profiles/<name>/`:
  layout, widget configs, the selected theme, account tokens, notes,
  runtime/wizard state, cache, and log are all per-profile.
- **Shared global layer** at the glint root: the colorscheme **library**
  (`colorschemes.toml`, with optional per-profile overrides merged by
  name) and the OAuth **client registrations** (`*_oauth_client.toml`) —
  define/register once, use from every profile.
- **Active-profile indicator** in the dashboard status bar — a
  `Profile: <name>` segment shown for any non-default profile so the
  active context is unmistakable (the default profile is unchanged).
- **Profile Manager in the setup wizard.** A bare `glint --setup` opens a
  front page listing your profiles: pick one to configure, or create /
  clone / rename / delete right there (same ops as the CLI, with a delete
  confirmation). `glint --profile X --setup` edits X directly.
- **Profile management CLI:** `--list-profiles`, `--new-profile <name>`
  (`--from <src>` to clone a profile's *config*, credentials excluded),
  `--rename-profile OLD:NEW`, `--delete-profile <name>`. Guards: names
  are validated, case-insensitive collisions rejected (macOS folds
  case), and `default` / the active profile can't be renamed/deleted.

### Changed

- **On-disk layout — flat configs keep working; migration is opt-in.** A
  pre-profiles flat `~/.config/glint/` is read **in place** by the default
  profile (no automatic move), so an older flat binary can share the
  directory safely. Migrate into `profiles/default/` explicitly — via the
  `--setup` migration prompt (recommended), or `glint --migrate-profiles`.
  The wizard prompt migrates *and* removes the now-dead flat duplicates;
  the CLI copies and leaves them (clean up later with
  `glint --cleanup-flat-config`). The shared colorscheme library + client
  registrations always stay at the root.
- **Cache, notes, and logs are now per-profile.** Notes previously
  defaulted to a shared `~/.glint/notes`; the default profile adopts any
  existing one on first run, and other profiles start empty.

## [0.3.0]

The 0.2 release is the structural refactor of glint into a plugin-style
platform: registries for widgets, auth providers, and LLM providers
became the documented seams for community contributions. The bulk of
this entry captures changes since the initial 0.2 plan — the widget
catalogue grew, two more providers landed, and the runtime got tighter
on memory, HTTP, and rendering.

### Added

#### Widgets
- **Multiple accounts of the same provider in Calendar.** A `[[providers]]`
  block can now carry an `account = "<label>"` field, so a work Outlook
  and a personal Outlook (or two Google accounts) coexist in one calendar.
  Tokens are stored per-account (`…_oauth_token.<account>.toml`); authorize
  extra accounts with `glint --auth microsoft:<label>`. A named account's
  `source` is provider-namespaced as `kind/label` (e.g. `outlook/work`),
  so per-calendar colors don't collide — even across providers. The
  setup wizard stays single-account per provider (the default account);
  extra accounts are hand-added to `calendar.toml` and survive wizard
  re-runs untouched. See `docs/multi-account-spec.md`.
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

- **OAuth token filenames are now account-scoped.**
  `google_oauth_token.toml` / `microsoft_oauth_token.toml` became
  `…_oauth_token.default.toml` (the `default` segment is the account
  label). Config files are unaffected — an `account` field on a
  `[[providers]]` block is optional and defaults to `default`. Existing
  source builds keep working via a read fallback: when the
  account-scoped file is absent, the default account reads the legacy
  unsuffixed file, and the next token refresh writes the new name (a
  one-time self-migration). Client-config files (`*_oauth_client.toml`)
  are unchanged.
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
- **Tiered credential storage backends.** Replace today's single
  plaintext-with-0600 path with three pluggable backends behind one
  trait: OS keychain (macOS Keychain / Windows Credential Manager /
  Linux Secret Service via the `keyring` crate), host-bound encryption
  (AES-GCM with key derived from the platform machine-ID via the
  `machine-uid` crate, for headless / SSH-only / homelab where Secret
  Service isn't available), and the current plaintext file as
  universal fallback. Selected via `credentials_backend = "auto" |
  "keychain" | "host-bound" | "plaintext"` in `config.toml`; `auto`
  picks the strongest available tier and logs which one won.
  PR-sized chunks: (1) extract `CredentialBackend` trait + refactor
  existing plaintext path through it, (2) add `keychain` backend +
  Linux-headless probe, (3) add `host-bound` backend with HKDF + a
  per-install salt file, (4) wizard integration with explicit
  backend-pick UI, (5) one-shot migration of existing plaintext files
  on first read. Honest framing in docs — host-bound is
  leak-resistant, not encrypted against local processes running as
  your user.
- **Upgrade `imap` dep when `3.x` stabilises** to clear the
  `imap-proto 0.10.2` future-incompat warning (trailing-semicolon
  macro pattern; rust-lang issue #79813). Informational today, not
  blocking. `imap` 2.x pins `imap-proto` `^0.10` so a transitive
  bump isn't an option without forking.
- **`v0.2.0` cut.** The version field bumps when README + INSTRUCTIONS
  + AGENTS + LICENSE land as a final polish pass and the first GitHub
  release is tagged.
