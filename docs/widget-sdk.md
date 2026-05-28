# glint widget SDK

A practical guide for building glint widgets — what the platform gives you, what conventions to follow, and where to look in the codebase for canonical examples.

This document is the living source of truth for **what the platform offers a widget**. When a new platform capability ships, it lands here. When a recurring widget pattern proves itself across multiple widgets and gets extracted, this is where the extracted version is documented.

> **Status:** v0.1 — seeded with the first formalized capability (`PollTracker`). More sections will follow as common patterns lift out of individual widgets.

---

## Table of contents

1. [Quickstart — minimum viable widget](#quickstart)
2. [The `Widget` trait](#the-widget-trait)
3. [Default keybindings convention](#default-keybindings-convention)
4. **Platform capabilities** ← grows over time
    - [Polling (`PollTracker`)](#polling-polltracker)
    - [Text utilities (`glint::text`)](#text-utilities-glinttext)
    - [Compact formatters (`glint::format`)](#compact-formatters-glintformat)
    - [Credentials storage (`glint::credentials`)](#credentials-storage-glintcredentials)
    - [Transient status (`glint::ui::status`)](#transient-status-glintuistatus)
    - [Confirm modal (`glint::ui::modal`)](#confirm-modal-glintuimodal)
5. [Best practices](#best-practices)
6. [Where to look in the codebase](#where-to-look)
7. [Roadmap](#roadmap)

---

## Quickstart

A widget is any type that implements the [`Widget`](../src/widgets/mod.rs) trait and is registered in [`widgets::registry::WIDGETS`](../src/widgets/registry.rs). The smallest possible widget is about 40 lines.

```rust
// src/widgets/hello/mod.rs
use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, text::Span, widgets::Paragraph, Frame};

use super::{AppContext, EventResult, Widget, WidgetCtx};

pub const KIND: &str = "hello";

pub struct HelloWidget {
    id: String,
}

#[async_trait]
impl Widget for HelloWidget {
    fn id(&self) -> &str { &self.id }
    fn display_name(&self) -> &str { "Hello" }
    fn kind(&self) -> &str { KIND }
    async fn update(&mut self, _ctx: &AppContext) -> Result<()> { Ok(()) }
    fn render(&self, frame: &mut Frame, area: Rect, _focused: bool) {
        frame.render_widget(Paragraph::new(Span::raw("hello, world")), area);
    }
    fn handle_key(&mut self, _key: KeyEvent) -> EventResult { EventResult::Ignored }
    fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> { Ok(false) }
    fn config(&self) -> serde_json::Value { serde_json::json!({}) }
    fn apply_config(&mut self, _config: serde_json::Value) -> Result<()> { Ok(()) }
}

pub fn build(ctx: &WidgetCtx) -> Box<dyn Widget> {
    Box::new(HelloWidget { id: format!("hello@{}", ctx.instance) })
}

pub fn wizard_descriptor() -> crate::wizard::descriptor::WizardDescriptor {
    crate::wizard::descriptor::WizardDescriptor::defer_to_toml(KIND)
}
```

Plus three small additions outside the widget module:

1. `widget-hello = []` in `Cargo.toml`'s `[features]` table (and `"widget-hello"` in `widgets-all`).
2. `#[cfg(feature = "widget-hello")] pub mod hello;` in [`src/widgets/mod.rs`](../src/widgets/mod.rs).
3. A `WidgetDescriptor` entry in [`src/widgets/registry.rs`](../src/widgets/registry.rs):
    ```rust
    #[cfg(feature = "widget-hello")]
    WidgetDescriptor {
        kind: super::hello::KIND,
        factory: super::hello::build,
        default_in_first_run: false,
        auth_requirements: &[],
        wizard: super::hello::wizard_descriptor,
    },
    ```

That's the entire surface. The app loop, focus dispatcher, layout engine, theming, and tick loop all "just work."

---

## The `Widget` trait

Full definition in [`src/widgets/mod.rs`](../src/widgets/mod.rs). The methods you care about:

| Method | Required? | Purpose |
|---|---|---|
| `id()`, `display_name()`, `kind()` | yes | Identification surface for the focus dispatcher + title bar. |
| `update(&AppContext)` | yes | Called on every tick (~4× per second). Where you decide whether to refresh data. |
| `render(frame, area, focused)` | yes | Paint into your area. Called when any widget marks itself dirty. |
| `handle_key(key)` | yes | Keyboard input. Return `Handled` to claim, `Ignored` to fall through. |
| `handle_mouse(mouse, area)` | no | Mouse clicks, drags, scroll-wheel. Default ignores. |
| `handle_command(cmd, args)` | yes | `:cmd arg1 arg2` from the command bar. |
| `config()`, `apply_config(json)` | yes | JSON serialization for diagnostics + live-reload from the file watcher. |
| `keybindings()` | no | `(key, description)` pairs surfaced in the `?` help overlay. |
| `set_app_theme(theme)` | no | Live `:scheme <name>` propagation. **Override this if you cache a merged theme.** |
| `take_dirty() -> bool` | no | Opt-in idle-CPU optimization: return `true` only when redraw is actually needed. |
| `poll_snapshot()` | no | Platform-side observability of your polling cadence. See below. |
| `shortcut_preferences()`, `set_shortcut()`, `shortcut()` | no | `Shift+<letter>` focus shortcut wiring. |

The constructor receives a [`WidgetCtx`](../src/widgets/mod.rs):

```rust
pub struct WidgetCtx {
    pub instance: String,                 // "main" or whatever follows "@" in `widget@<instance>`
    pub theme: Arc<Theme>,                // resolved app theme
    pub llm: Option<Arc<dyn LlmProvider>>,// None when llm.toml disabled or no key
    pub cache: ScopedCache,               // already namespaced to (kind, instance)
}
```

Pull what you need; ignore the rest.

---

## Default keybindings convention

A consistent keybinding vocabulary across widgets matters more than any one widget's local cleverness. Users build muscle memory; inconsistency between widgets makes the whole dashboard feel jankier than the sum of its parts. The list below is the platform-recommended baseline. **Adopt these unless you have a deliberate reason not to** — and when you deviate, document why in the widget's `keybindings()` help text.

This is convention, not enforcement. Widgets remain free to bind however they want, but the help overlay (`?`) and the SDK doc all assume widgets follow these defaults.

### Common gestures (recommended across widgets)

| Key | Action | Notes |
|---|---|---|
| `↑` / `↓` / `j` / `k` | Move selection up / down | Vim aliases on letters; arrows for non-vim muscle memory. |
| `←` / `→` / `h` / `l` | Cycle horizontal context (tabs, periods, panes) | Use when the widget has a horizontal axis worth cycling. |
| `Enter` | **Primary in-place action** | Context-dependent: expand a list row (news, feeds), swap selection (forex), open the active note (notes). Never opens an external URL — that's too easy to mis-fire when the user meant "look at this inline." |
| `e` / `Space` | Expand / collapse selected item | Alias for `Enter` in list widgets that have an inline expansion view. Both work. |
| `o` | **Open externally** (browser, file, app) | The dedicated "leave glint" gesture. Used by stocks, forex, news, feeds. Should always be a *single key* away from the user's intent; never on `Enter`. |
| `r` | Force refresh | Bypasses the poll interval. |
| `?` | Help overlay | Global; you don't bind this. |
| `Tab` / `Shift+Tab` | Focus cycle | Global; you don't bind this. |
| `Esc` | Cancel / back out | Closes modals, exits inline edit modes, clears in-flight searches. |
| `q` | Quit (global) | Only fires when no widget consumes it — widgets in text-entry mode (notes) keep `q` as a literal. |
| `Ctrl+C` | Quit (always) | Process interrupt; widgets can't override. |

### List-management gestures

For widgets that let the user add or remove items from a persisted list (stocks watchlist, forex watchlist, notes, feeds topics-at-runtime):

| Key | Action |
|---|---|
| `n` | New (create entity) — note, ticker, currency. Letter form preferred. |
| `d` | Delete (with confirm modal — see [`glint::ui::modal`](#confirm-modal-glintuimodal)). |
| `+` / `-` | Add to / remove from the **current list shape** (e.g. add a `:stock` lookup to the watchlist, remove the selected currency). Punctuation forms preserve muscle memory for widgets that started with them. |

`n`/`d` and `+`/`-` are *not* mutually exclusive. The notes widget binds both pairs to its create/delete actions; either gesture works. New widgets should prefer the letter forms in their help text and keep `+`/`-` available as aliases when there's no risk of muscle-memory churn.

### Per-widget conventions

| Key | Action |
|---|---|
| `s` | Run / cycle a summary (news, feeds). LLM-backed. |
| `y` | Yank selected value to clipboard. |
| `x` | Clear a transient state (a `:lookup` row, a stale message). |
| `c` | Cycle a display mode (stocks: %/$, forex: reset amount). |
| `1`–`9` | Select graph period / numbered tab directly (stocks, forex). |

### Modifiers

| Combo | Meaning |
|---|---|
| `Shift+<letter>` | **Focus shortcut** — reserved for the app's `Shift+<letter>` focus-jump dispatcher. Widgets declare a preference list via `shortcut_preferences()`; do *not* bind these manually in `handle_key`. |
| `Ctrl+<letter>` | Modifier-required actions (e.g. feeds's `Ctrl+S` for summary-length cycle when `Shift+S` was unavailable). Use sparingly; users with non-US keyboards may have surprising `Ctrl+` mappings. |
| `Alt+<letter>` | Avoid. Many terminals consume `Alt` for window-manager actions. |

### Hard-reserved keys

The platform will not honor widget bindings for these. Logged warning if attempted, but the app boots normally:

- `Ctrl+C` — OS-level interrupt, always quits.
- `Ctrl+\` — OS-level SIGQUIT, always quits.

### Soft-reserved keys (warn, but allow)

These are platform-meaningful but a widget *might* legitimately want them in some contexts (e.g. notes editing mode). Bindings produce a startup warning so you know what you're stepping on:

- `:` — Global command-bar trigger. The notes widget legitimately wants `:` as a literal in edit mode; the dispatcher's context-cascade resolves the ambiguity (text-entry widget consumes first; global handler only sees `:` when no widget claims it).
- `?` — Help overlay. As above.

### Why "Enter is in-place, `o` opens externally"

Two reasons:

1. **Risk asymmetry.** Pressing Enter when you meant something else is common; spawning a browser window when you wanted to look at content inline is genuinely disruptive (window focus, scroll position, ad-block context, browser tab spam). The in-place action recovers in one keystroke (`Esc` to collapse); the browser jump doesn't.
2. **Consistency with the rest of the TUI world.** `lynx`, `nnn`, `ranger`, `lazygit`, `mc` all use Enter for "drill in here" and a dedicated key for "open outside." Letting Enter mean "open browser" felt natural in early widgets but quietly broke this contract.

### How to deviate well

If your widget needs a key bound differently from this table, the rules:

1. **Document the deviation in the widget's `keybindings()`** so the help overlay surfaces it. Don't quietly diverge.
2. **Don't deviate on `Ctrl+C` / `Ctrl+\` / `Shift+<letter>`.** Those are global contracts.
3. **If you bind `Enter` to something other than the in-place primary action, you should have a very specific reason.** Forex binds `Enter` to "swap selected to primary" — that IS the in-place primary action (it doesn't navigate away, doesn't open anything external), so it follows the spirit.
4. **Add aliases instead of replacing.** When iterating, keep the old binding as an alias for one or two releases so muscle memory doesn't break out from under users. Notes binds both `n`/`+` and `d`/`-`; either form works.

---

## Platform capabilities

### Polling (`PollTracker`)

**When to use:** your widget periodically pulls data from the network or a slow source (RSS feeds, network quotes, weather, mail, calendar).

**Why a platform helper:** every data-fetching widget was reinventing the same 4-line `last_attempt + Duration` debounce. We extracted it to [`src/polling.rs`](../src/polling.rs) so the cache-age clamp, "is_due" semantics, and forward-looking deadline-scheduler hook live in one place.

#### The struct

```rust
use crate::polling::PollTracker;
use std::time::Duration;

let mut tracker = PollTracker::new(Duration::from_secs(900)); // 15 min

// On first construction: maybe a cache entry survived from last launch.
if let Some(entry) = cache.load::<Vec<Article>>(CACHE_KEY) {
    tracker.seed_from_cache_age(entry.age());  // clamped to interval; won't fire instantly
}

// On every tick:
if tracker.is_due() {
    tracker.mark_attempted();
    self.spawn_refresh();   // kick off your fetch; the tracker advances regardless of success/failure
}

// User pressed `r` to force a refresh:
tracker.mark_dirty();       // next is_due() returns true
```

#### Mounting it on your widget

The tracker holds two fields (`interval`, `last_attempt`) and needs `&mut self` to advance — so most widgets put it inside their `Arc<Mutex<State>>` alongside other mutating state:

```rust
struct MyState {
    items: Vec<Item>,
    inflight: bool,
    poll: crate::polling::PollTracker,
    // ...
}

// In with_config:
let poll_interval = Duration::from_secs(config.poll_interval_secs.max(60));
let mut state = MyState {
    poll: crate::polling::PollTracker::new(poll_interval),
    ..MyState::default()
};

// In is_due:
fn is_due(&self) -> bool {
    let st = self.state.lock().expect("poisoned");
    if st.inflight { return false; }
    st.poll.is_due()
}
```

#### The `poll_snapshot` trait hook (optional)

Implementing [`Widget::poll_snapshot`](../src/widgets/mod.rs) lets the platform see your polling state without taking a reference into your mutex:

```rust
fn poll_snapshot(&self) -> Option<crate::polling::PollSnapshot> {
    Some(self.state.lock().expect("poisoned").poll.snapshot())
}
```

Today this surfaces nothing user-visible — but it's the hook the platform will use for:
- centralized tracing of poll cadence (per-widget `is_due` histograms),
- deadline-aware event-loop scheduling (wake at the earliest next-due across widgets instead of running a blanket 250 ms tick),
- "skip ticks while hidden in a stack" optimization.

Implement it; you get those wins for free when they ship.

#### When *not* to use `PollTracker`

It's a debounce primitive, not a fetcher framework. If your widget:

- Has multi-tier policy (e.g., **email** has a fast retry while account address is being resolved, then switches to the configured mail interval): own **two** trackers and pick between them. See [`src/widgets/email/mod.rs`](../src/widgets/email/mod.rs)'s `account_poll` + `mail_poll`.
- Owns a richer decision tree (e.g., **weather**'s `NextAction { Locate | Fetch | Wait }`): use a tracker *inside* the decision tree's "should I fetch?" branch. See [`src/widgets/weather/mod.rs`](../src/widgets/weather/mod.rs).
- Is push-driven (IMAP IDLE, WebSocket, SSE): you probably don't need a tracker. Implement `update()` however you like and skip `poll_snapshot`.

The trait hook returns `Option<_>` so opting out is the empty default — costs nothing.

#### API surface

| Method | Purpose |
|---|---|
| `PollTracker::new(interval)` | Construct with a refresh interval. First `is_due()` returns `true`. |
| `seed_from_cache_age(age)` | Set `last_attempt` from a cache entry's age. Clamped to interval — stale caches don't push the next fetch into the future. |
| `is_due() -> bool` | True when no attempt yet, or `interval` has elapsed since the last attempt. |
| `has_attempted() -> bool` | True once any fetch has been recorded. Use this for "have I ever tried to load?" decisions like "show 'No items'" vs "show 'Loading…'". |
| `mark_attempted()` | Stamp `Instant::now()` as the latest attempt. Call when you kick off a fetch (success-or-failure). |
| `mark_dirty()` | Force the next `is_due()` to return `true`. Used by `r` key, `:reload`, etc. |
| `interval()`, `set_interval()` | For live config reload via `apply_config`. |
| `next_due_at() -> Option<Instant>` | Monotonic instant of the next scheduled fetch. Reserved for future scheduler. |
| `snapshot() -> PollSnapshot` | Read-only copy for the platform trait hook. |

**Reference example:** [`src/widgets/stocks/mod.rs`](../src/widgets/stocks/mod.rs) — single tracker, straightforward use.

---

### Text utilities (`glint::text`)

**When to use:** any widget rendering free-form text — titles, summaries, message bodies, file paths, RSS descriptions. Use the shared helpers instead of writing your own `chars().count()`-based truncation or wrap; the shared versions are Unicode-width-aware (correct for CJK, emoji, combining marks) and consistent across widgets.

#### API

```rust
use crate::text::{truncate, pad_or_truncate, wrap, sanitize_html};

// Truncate to a max display width, appending `…` when truncated.
// Honors Unicode display widths — a wide char that would overflow
// is dropped, not split mid-codepoint.
let label = truncate("Some long article title", 12);
//                                          ↑ display cells, not chars

// Pad short strings, truncate long ones, to exactly N cells.
let cell = pad_or_truncate("hello", 10);  // "hello     " (10 cells)

// Word-wrap to at most `max_lines` rows of `max_width` cells each.
// When `preserve_paragraphs` is true, `\n` in the input splits
// paragraphs and each wraps independently. When the input doesn't
// fit, the final emitted line ends with `…`.
let lines = wrap("the quick brown fox", 10, 3, false);

// Strip rudimentary HTML + decode named and numeric entities
// (`&amp;`, `&#8217;`, etc.) so RSS `<description>` blobs render.
// Unknown entities pass through verbatim — won't garble plain text.
let text = sanitize_html("<p>Hello &amp; <b>world</b></p>");
//                       ↑ "Hello & world"
```

#### Behavior notes

- **`wrap` is lossless on oversized words.** If a single word exceeds `max_width`, it's mid-broken into multiple chunks across rows rather than silently dropping the tail. Set `max_lines` high enough that you'd rather render a multi-line word than have the wrap eat the rest.
- **`sanitize_html` is intentionally not a full HTML parser.** Use it for short summary snippets where robustness against unknown markup matters more than spec-compliance. For real article-body parsing, reach for `html5ever` directly.
- **All four functions use Unicode display widths.** Char counts and cell widths disagree for emoji (2 cells, 1 char), CJK (2 cells, 1 char), and combining marks (0 cells, 1 char each). Local copies that used `chars().count()` were wrong; the shared module isn't.

#### When *not* to use

- **Path / URL truncation in the middle.** These helpers always trim the tail. If you want `/Users/.../foo.txt`-style middle-elision, build it yourself; the shared `truncate` is wrong shape.
- **Markdown rendering.** `sanitize_html` is not a markdown processor. Widgets that genuinely render markdown (none today) should use a markdown crate.

**Reference example:** [`src/widgets/email/mod.rs`](../src/widgets/email/mod.rs) — uses `truncate`, `pad_or_truncate`, and `wrap_text` (thin wrapper over `wrap` with `preserve_paragraphs = true`).

---

### Compact formatters (`glint::format`)

**When to use:** any widget rendering durations or "how long ago" labels — article publish times, data age, process uptime, last-fetch timestamps. Three variants, each tuned to a different display budget; pick the one that fits.

#### API

```rust
use chrono::{DateTime, Utc};
use crate::format::{relative_time_label, short_duration_label, uptime_label};

// "How long ago" for timestamps. Buckets: `now`, `Nm`, `Nh`, `Nd`,
// `Nw`, then absolute `MMM DD` for older items (so a 60-day-old
// article reads "May 25", not "8w"). Future timestamps clamp to
// `now` for clock-skew tolerance.
let label = relative_time_label(article.published, Utc::now());
//                              ↑ "3h" / "2d" / "May 25"

// Compact single-segment duration: `Ns / Nm / Nh / Nd / Nmo`.
// Use for "data age" meta lines or "last fetch" suffixes — short
// bounded durations where you want one segment, not three.
let age = short_duration_label(seconds_since_last_fetch);
//                             ↑ i64 — negatives clamp to "0s"

// Multi-segment uptime: `Nm` / `Nh Nm` / `Nd Nh Nm`. Reads
// naturally for "process up for 3d 4h 12m".
let up = uptime_label(secs);  // u64
```

#### When *not* to use

- **You need second-precision** ("12.345s"). These all round to nearest unit.
- **You need locale-aware formatting** ("vor 3 Stunden"). All output is English.
- **You need a calendar / clock-time format** ("3:42 PM"). These are *interval* formatters, not point-in-time. Use chrono's `format()` directly.

**Reference examples:**
- `relative_time_label` — [`src/widgets/feeds/mod.rs`](../src/widgets/feeds/mod.rs) and [`src/widgets/news/mod.rs`](../src/widgets/news/mod.rs).
- `short_duration_label` — [`src/widgets/weather/mod.rs`](../src/widgets/weather/mod.rs) (data age in the meta row).
- `uptime_label` — [`src/widgets/resources/mod.rs`](../src/widgets/resources/mod.rs).

---

### Credentials storage (`glint::credentials`)

**When to use:** any time you need to persist a secret to disk — OAuth tokens, API keys, session cookies, paste-captured tokens, anything sensitive enough that "world-readable" would be a real problem.

Use the shared helpers instead of writing the atomic-write + `chmod 0600` dance yourself. Skipping a step here is a security regression and the kind of bug that easily lands silently.

#### API

```rust
use crate::credentials;

// All files live under ~/.config/glint/credentials/. You pass a
// basename; the module resolves the absolute path. The directory
// is created with mode 0700 on first use (idempotent).

// Resolve a basename to its absolute path. Does NOT create the file.
let path = credentials::path("my_widget_token.toml")?;

// Load a TOML-serialised value. Returns Ok(None) if the file is
// absent — caller decides whether that's expected or fatal.
let token: Option<MyToken> = credentials::load("my_widget_token.toml")?;

// Save with atomic write + chmod 0600 (Unix). Temp file is created
// alongside the destination, perms tightened *before* the rename,
// so the final inode is never visible at a wider mode — even if
// glint crashes mid-write.
let path = credentials::save("my_widget_token.toml", &token)?;

// Write a starter template iff the file is missing. Idempotent —
// re-running doesn't clobber the user's filled-in version. Used
// by the wizard's `:setup` flow to drop placeholders the user
// then edits.
let wrote_new = credentials::write_template_if_missing(
    "my_widget_oauth_client.toml",
    "client_id = \"REPLACE_WITH_YOUR_CLIENT_ID\"\n",
)?;
```

#### Behavior notes

- **Atomic + chmod-before-rename**: writes go through `<name>.tmp`, get `chmod 0600`, then rename onto the final path. A crash mid-write leaves either the old file intact or no file — never a half-written secret at default perms.
- **Filename, not path**: the API takes basenames so the credentials-dir convention is enforced. You can't accidentally write a token to the wrong place.
- **`Ok(None)` for missing files**: `load` treats absence as "not yet captured" rather than an error. Surface a user-visible message yourself when that matters.
- **Unix-only perm tightening**: `chmod 0600` is a no-op on non-Unix targets. The atomic-write path still runs.

#### When *not* to use

- **Non-secret config**: widget TOMLs (`stocks.toml`, `news.toml`, etc.) belong in the config dir, not credentials. They're meant to be world-readable; users edit them.
- **System keyring integration**: this module writes plaintext-on-disk (chmod 0600). Future work to back specific files with macOS Keychain / Windows Credential Manager / `libsecret` would land as a `CredentialBackend` trait sitting on top of this module; see the [credential-storage roadmap](https://github.com/ntrospect0/glint/issues) once it's filed.
- **Outside the credentials dir**: if you genuinely need to write a chmod-0600 file somewhere else (logs with secrets?), that's a different problem — talk through the use case in an issue first.

**Reference example:** [`src/auth/google/store.rs`](../src/auth/google/store.rs) — `GoogleToken::{path, load, save}` are thin wrappers over `credentials::{path, load, save}`. Microsoft's token store and the auth wizard's template scaffolding follow the same shape.

---

### Transient status (`glint::ui::status`)

**When to use:** any widget that shows short-lived status messages — "Added AAPL to watchlist", "Save failed", "Copied to clipboard", visual pulse markers like the forex "📋 → ✅" indicator. The pattern: set a value with a TTL, render it while it's live, automatically clear once the TTL elapses.

#### API

```rust
use std::time::Duration;
use crate::ui::status::{TimedFeedback, live_value, drain_if_expired};

const STATUS_TTL: Duration = Duration::from_millis(2500);

struct WidgetState {
    status: Option<TimedFeedback<String>>,
    //                          ↑ generic — works for usize, enums, etc.
}

// Setter: usually wrapped in a small helper so call sites stay tight.
fn set_status(&self, msg: impl Into<String>) {
    let mut st = self.state.lock().expect("poisoned");
    st.status = Some(TimedFeedback::new(msg.into(), STATUS_TTL));
}

// Render-time read: returns Some(&T) while live, auto-clears on expiry.
fn live_status(&self) -> Option<String> {
    let mut st = self.state.lock().expect("poisoned");
    live_value(&mut st.status).cloned()
}

// Tick-path drain: returns `true` exactly when the slot transitioned
// from live to expired, so the widget can mark itself dirty for one
// final redraw that drops the now-stale message.
async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
    let mut st = self.state.lock().expect("poisoned");
    if drain_if_expired(&mut st.status) {
        st.dirty = true;
    }
    Ok(())
}
```

#### Generic over the value type

The most common shape is `Option<TimedFeedback<String>>` for footer messages, but the wrapper is generic. The forex widget uses `Option<TimedFeedback<usize>>` to track the row index whose copy icon should briefly flash "✅" after a clipboard yank — same machinery, different payload.

#### Behavior notes

- **TTL stamps on construction**, not on first read. The expiry clock starts the moment you set the value.
- **`live_value` is `&mut`** so it can auto-clear on expiry. Render paths that just want to *peek* without clearing can call `.is_expired()` on a `&TimedFeedback<T>` directly.
- **`drain_if_expired` is the "redraw now" signal.** Return value is `true` iff the slot was just emptied — wire it through your dirty bit so the chrome reverts on the next frame.

#### When *not* to use

- **Persistent status** (e.g., "OAuth required" while a token is missing). Those aren't transient — they're a function of state, not time.
- **Animations** with multiple frames or per-tick updates. `TimedFeedback` is set-once-and-forget. For pulsing / progress bars, manage the time yourself.

**Reference examples:**
- `Option<TimedFeedback<String>>` for footer messages — [`src/widgets/stocks/mod.rs`](../src/widgets/stocks/mod.rs), [`src/widgets/forex/mod.rs`](../src/widgets/forex/mod.rs), [`src/widgets/feeds/mod.rs`](../src/widgets/feeds/mod.rs).
- `Option<TimedFeedback<usize>>` for "which row just got copied" pulse — [`src/widgets/forex/mod.rs`](../src/widgets/forex/mod.rs) (`copy_feedback`).

---

### Confirm modal (`glint::ui::modal`)

**When to use:** any widget that needs a destructive-action confirmation overlay — "Remove ticker?", "Delete note?", "Drop folder?" The shared helper owns the *rendering* (centred rounded box, theme-aware title bar, target name in bold, action hint at the bottom) and the *key dispatch* (y/Y commits, anything else cancels). Widgets keep their own `Option<T>` state slot so the meaning of "what's being confirmed" stays widget-local.

#### API

```rust
use crate::ui::modal::{render, dispatch_key, ConfirmModal, ConfirmChoice};

// Widget state: an Option<T> slot. Typically Option<String> for
// "the name / id of the thing being confirmed."
struct WidgetState {
    confirm_remove: Option<String>,
    // ...
}

// In your render(), after laying out everything else:
if let Some(target) = self.state.lock().expect("poisoned").confirm_remove.clone() {
    render(
        frame,
        inner,                              // parent area for centring
        &self.theme,
        ConfirmModal {
            title: " Remove ticker? ",      // including leading + trailing spaces
            target: &target,                // shown in bold inside the modal
            hint: None,                     // None → default "[y] confirm · any other key cancels"
            max_width: 48,                  // 48 / 54 — widget's preference
        },
    );
}

// In your handle_key() prelude, when the slot is Some:
if self.state.lock().expect("poisoned").confirm_remove.is_some() {
    match dispatch_key(key) {
        ConfirmChoice::Confirm => self.commit_removal(),  // your action
        ConfirmChoice::Cancel  => self.clear_modal(),     // clears the slot
    }
    return EventResult::Handled;
}
```

#### Behavior notes

- **Theme-aware title bar**: the title's background pulls from `theme.text_selected.fg` so it inherits the active scheme's accent color (e.g. Gruvbox orange, Nord cyan) instead of hardcoded yellow.
- **Fixed 7-row height**: blank · target · blank · hint + borders. Widgets pass `max_width`; height isn't configurable to keep modals visually consistent across the dashboard.
- **No-op on tiny parents**: if `parent.width < 30` or `parent.height < 9` the helper silently returns. Widgets that care about that case should surface a fallback inline (e.g. a status-line message via [`glint::ui::status`](#transient-status-glintuistatus)).
- **State stays widget-local**: the helper doesn't own your `Option<T>`. Set it / clear it from your widget's setter / cancel paths. The helper is just for the modal's presentation + key dispatch.

#### When *not* to use

- **Multi-choice prompts** (Yes / No / Cancel). The y-or-anything-else contract is a 2-state dispatch; if you need 3+ branches, render your own modal or revisit whether a single-key dispatch is appropriate.
- **Inline forms** (paste a cookie / type a path). The body shape here is target-name-as-static-text — no text entry. For input modals, build directly with `Block` + `Paragraph` and your own state machine.
- **Persistent overlays**. This modal expects the user to act and dismiss on the same render cycle. Long-lived dialogs (e.g. "loading…") aren't the right fit.

**Reference example:** [`src/widgets/stocks/mod.rs`](../src/widgets/stocks/mod.rs) — `render_confirm_modal` collapses to a 7-line call into `crate::ui::modal::render` and the key dispatch becomes a 3-line `match` over `ConfirmChoice`. Notes and forex follow the same pattern.

---

## Best practices

These conventions emerged from building 11+ widgets. They aren't enforced — just the patterns that consistently work well.

### Theming

- Pull your border + title-bar styles via [`apply_title_row`](../src/ui/mod.rs). Don't paint your own border with `Block::default()` from scratch.
- Cache a merged `Theme` (app + your widget's `[colors]` overrides) and rebuild it in `set_app_theme(theme)`. Otherwise `:scheme <name>` won't repaint your widget until restart.
- For accent colors (e.g., a scroll-indicator arrow), pull from `self.theme.text_selected` rather than hardcoding `Color::Yellow`. The user's scheme controls the palette.

### Layout

- Reserve a 1-cell right margin so content doesn't touch the panel's right border. Look for `*_RIGHT_BUFFER` constants in `feeds` and `email`.
- When you split horizontally, insert an explicit `Constraint::Length(1)` between sub-panels as a visual gap.
- Adaptive layout: switch between horizontal split (wide) and vertical stack (narrow) based on `area.width`. Threshold around 80 cols has worked for `feeds`.

### Mouse hit-testing

- Capture screen-absolute rects (and per-row hit ranges) into widget state *during render*, then consult them in `handle_mouse`. Avoid re-running layout math on every click. Examples: `feeds`'s `list_rows`, `tab_rects`; `forex`'s `row_hits`.
- Route scroll wheel by cursor position (inside list → navigate, inside details → scroll content) rather than always doing the same thing.

### Confirm modals

- Pattern: `Option<String>` field on state representing "pending confirmation," matched on `y/Y` (commit) vs any-other-key (cancel).
- Render with `Clear` first, then a 5–7 row centered `Block` with `BorderType::Rounded`.
- The title bar background pulls from `self.theme.text_selected.fg.unwrap_or(Color::Yellow)` — never hardcoded.
- Examples: [`notes`](../src/widgets/notes/mod.rs), [`stocks`](../src/widgets/stocks/mod.rs), [`forex`](../src/widgets/forex/mod.rs).

### Status feedback with TTL

- For transient messages ("Added AAPL", "Save failed"), store `Option<(String, Instant)>` on state; check `elapsed() >= 2.5s` in render and clear when expired.
- The footer hint replaces with the status message while it's live, then reverts.

### Caching

- The `ScopedCache` you receive in `WidgetCtx` is already namespaced — no need to prefix keys.
- For images / binary blobs: `store_bytes` / `load_bytes`. Check `entry.stored_at` against your TTL yourself (TTL isn't enforced by the cache).
- For JSON-serializable values: `store` / `load`. `entry.age()` gives wall-clock age.

### Persistence of runtime mutations

- If your widget mutates a list at runtime (e.g., add ticker, remove currency), write back to the widget's TOML via [`config::rewrite_widget_top_level_string_array`](../src/config/mod.rs). Preserves comments and other settings.
- Credentials go in `~/.config/glint/credentials/<widget>_<thing>.toml`, chmod `0600`. See [`auth/google/store.rs`](../src/auth/google/store.rs).

### Don't fight the trait

- `take_dirty()` defaults to `true` (always redraw). Override only if you measure that idle-redraw is actually a problem for your widget.
- `update()` is called per-tick (~250 ms). Don't sleep, don't block — spawn a `tokio::spawn` if you need to do work.
- `render()` should be deterministic given `&self` + `area` + `focused`. Side effects belong in `update()` or `handle_*`.

---

## Where to look

When you want to copy a pattern, these are the canonical references:

| Pattern | Reference widget |
|---|---|
| Simple data list with polling | [`stocks`](../src/widgets/stocks/mod.rs) |
| Two-tier polling policy | [`email`](../src/widgets/email/mod.rs) (`account_poll` + `mail_poll`) |
| Decision-tree fetch logic | [`weather`](../src/widgets/weather/mod.rs) (`NextAction`) |
| RSS feed aggregation | [`news`](../src/widgets/news/mod.rs), [`feeds`](../src/widgets/feeds/mod.rs) |
| LLM summarization w/ length toggle | [`feeds`](../src/widgets/feeds/mod.rs) |
| Inline image rendering | [`gallery`](../src/widgets/gallery/mod.rs), [`feeds`](../src/widgets/feeds/mod.rs) |
| OAuth token storage | [`auth/google/store.rs`](../src/auth/google/store.rs) |
| Confirm-removal modal | [`notes`](../src/widgets/notes/mod.rs), [`stocks`](../src/widgets/stocks/mod.rs) |
| Mouse hit-testing with wrapped rows | [`feeds`](../src/widgets/feeds/mod.rs) |
| Adaptive horizontal/vertical layout | [`feeds`](../src/widgets/feeds/mod.rs) |
| Editable inline cell | [`forex`](../src/widgets/forex/mod.rs) (`editing_amount`) |
| Stack composition (multi-widget tab strip) | [`stack`](../src/widgets/stack.rs) — platform layer |
| Wizard descriptor (declarative setup) | every widget's `wizard_descriptor()` fn |

---

## Roadmap

The current extraction sprint is complete — the high-confidence cluster candidates surfaced in audits 1–2 have all landed (see § Platform capabilities above). The remaining items are tracked but waiting on either pain signal or design clarity:

1. **`ScopedCache::load_within<T>(key, ttl)`** — convenience wrapper over `load + entry.age() < ttl`. Manually computed today in `feeds`, `gallery`, and planned `news` cache flows. ~40 LOC saved across 3 sites; easy lift when one of those widgets needs more cache work.
2. **Spawn-refresh skeleton** — every polling widget has the same `lock → mark inflight → spawn → fetch → lock → write data` shape. Theoretical biggest win (~200 LOC) but the highest abstraction risk — the variance across result types and state shapes makes a clean generic non-obvious. **Don't reach for this** until a new widget shows up demanding a "common shape" we don't already have.
3. **Selection trait (`SelectableList`)** — `move_selection(delta)` + clamp, currently duplicated across 5 widgets. The variance (scroll-reset behavior, primary-row skip in forex, list-then-transient layering in stocks) means a single trait either becomes too prescriptive or too loose to pay for itself. **Likely doesn't ship.**
4. **`apply_config` macro** — every widget's `apply_config` does the same deserialize → clone(theme, cache, …) → `*self = Self::with_config(...)` dance. ~100 LOC could collapse, but macros hurt readability and stack-trace clarity. **Likely doesn't ship.**

Future lifts should be driven by *new* widget pain, not by scanning the existing tree for more things to consolidate. Per the [convergence-signal rule](#convergence-signal), the bar is 3+ widgets with mostly-identical shape and clear variance points — none of the remaining items hit that bar cleanly without active pressure.

### Convergence signal

Per the [Memory note "Convention sweep"](../) — the bar for lifting something to the platform is that it exists in **3+ widgets with mostly-identical shape and clear variance points**. The remaining items above hit that bar; everything else surfaced during audits failed it (too few call sites, or too much variance to abstract cleanly). Future widgets should drive the next round of lifts — don't go looking.

---

## Contributing

When you add or extract a platform capability:

1. Land the code under `src/<capability>.rs` (or `src/ui/<capability>.rs` for UI helpers).
2. Add a "Platform capabilities" section to this doc using the same template Polling uses.
3. Update the canonical reference widget that uses it.
4. Drop a line under "Roadmap" → remove your row.

This document is the social contract between the platform and widget authors. Keep it honest: if a "capability" is really just convention, say so; if a pattern keeps getting reinvented, extract it and write it up here.
