# glint — Terminal Dashboard

> A fast, extensible terminal dashboard for stocks, calendar, news, and beyond.

## 1. Overview

**glint** is a keyboard-driven terminal application that surfaces real-time information across configurable widgets in a unified dashboard. The initial release ships with three widgets — Stocks, Calendar, and News — but the architecture is designed so that new widgets can be added with minimal friction.

The app targets power users who live in the terminal and want a single-pane view of the data that matters to them without switching to a browser.

## 2. Technology Stack

| Layer | Choice | Rationale |
|---|---|---|
| Language | Rust (2021 edition) | Performance, safety, single-binary distribution |
| TUI framework | Ratatui 0.28+ | Active maintenance, flexible layout, great Canvas/Chart primitives |
| Async runtime | Tokio | Industry standard, required by most HTTP clients |
| HTTP client | reqwest | Async, TLS, cookie support (needed for Yahoo Finance) |
| Serialization | serde / serde_json / toml | De facto Rust standard |
| Config format | TOML | Human-friendly, supports comments, hierarchical; parsed via `toml` crate |
| Date/time | chrono + chrono-tz | Timezone-aware calendar ops |
| NLP dates | two-timer or chrono-english | Relative date parsing ("next tuesday", "tomorrow") |
| Fuzzy matching | strsim | Edit-distance fuzzy match for typo correction in commands |
| LLM client | Anthropic API (reqwest) | Summarization, semantic classification, disambiguation |
| Terminal backend | crossterm | Cross-platform (macOS, Linux, Windows) |

## 3. Architecture

### 3.1 High-Level Component Diagram

```
┌─────────────────────────────────────────────────────┐
│                    glint binary                      │
│                                                      │
│  ┌────────────┐  ┌────────────┐  ┌───────────────┐  │
│  │   Config    │  │  Event     │  │   Renderer    │  │
│  │   Manager   │  │  Loop      │  │   (Ratatui)   │  │
│  └─────┬──────┘  └─────┬──────┘  └───────┬───────┘  │
│        │               │                 │           │
│  ┌─────▼───────────────▼─────────────────▼───────┐  │
│  │              Widget Manager                    │  │
│  │  ┌─────────┐ ┌──────────┐ ┌────────────────┐  │  │
│  │  │ Stocks  │ │ Calendar │ │     News       │  │  │
│  │  │ Widget  │ │ Widget   │ │     Widget     │  │  │
│  │  └────┬────┘ └────┬─────┘ └───────┬────────┘  │  │
│  └───────┼───────────┼───────────────┼───────────┘  │
│          │           │               │               │
│  ┌───────▼───────────▼───────────────▼───────────┐  │
│  │             Data Provider Layer                │  │
│  │  ┌──────────┐ ┌──────────┐ ┌───────────────┐  │  │
│  │  │ Yahoo    │ │ Google   │ │ News Feed     │  │  │
│  │  │ Finance  │ │ Calendar │ │ Aggregator    │  │  │
│  │  └──────────┘ └──────────┘ └───────────────┘  │  │
│  └────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────┘
```

### 3.2 Core Abstractions

#### Widget Trait

Every widget implements a common trait that the Widget Manager calls into:

```rust
pub trait Widget {
    /// Unique identifier (e.g. "stocks", "calendar", "news")
    fn id(&self) -> &str;

    /// Human-readable name for display in headers
    fn display_name(&self) -> &str;

    /// Called on each tick to refresh data (non-blocking)
    async fn update(&mut self, ctx: &AppContext) -> Result<()>;

    /// Render into the provided Ratatui frame area
    fn render(&self, frame: &mut Frame, area: Rect, focused: bool);

    /// Handle a key event when this widget has focus.
    /// Returns Handled / Ignored so the app can fall through to globals.
    fn handle_key(&mut self, key: KeyEvent) -> EventResult;

    /// Handle a command string (from the command bar).
    /// Returns Ok(true) if the command was consumed.
    fn handle_command(&mut self, cmd: &str, args: &[&str]) -> Result<bool>;

    /// Return the widget's current config as JSON Value
    fn config(&self) -> serde_json::Value;

    /// Apply config (called at startup and on live-reload)
    fn apply_config(&mut self, config: serde_json::Value) -> Result<()>;
}
```

#### Data Provider Trait

Data fetching is separated from widget rendering:

```rust
pub trait DataProvider: Send + Sync {
    type Data: Send;

    /// Fetch latest data. Called by the widget's update().
    async fn fetch(&self) -> Result<Self::Data>;

    /// Provider name for logging/config
    fn name(&self) -> &str;
}
```

This allows swapping Yahoo Finance for Alpha Vantage or Polygon.io without touching widget code.

### 3.3 Event Loop

The app runs a standard Ratatui event loop driven by crossterm events plus a tick timer:

1. **Input events** — keypresses dispatched first to the focused widget, then to global handlers (widget switching, command bar, quit).
2. **Tick events** — fire at a configurable interval (default 1s for stocks, 60s for calendar/news). Each widget can specify its own poll cadence.
3. **Resize events** — trigger a full re-layout based on the grid config.

### 3.4 Configurable Grid Layout

The terminal area is divided into a grid defined in config. Each cell references a widget by ID and can span rows/columns.

```toml
[layout]
columns = [60, 40]
rows = [50, 50]

[[layout.cells]]
widget = "stocks"
col = 0
row = 0
col_span = 1
row_span = 2

[[layout.cells]]
widget = "calendar"
col = 1
row = 0

[[layout.cells]]
widget = "news"
col = 1
row = 1
```

Column/row values are **percentage weights** (not absolute sizes), so the grid adapts to terminal dimensions. Widgets not placed in the grid are accessible via tab-cycling but hidden from the default view.

## 4. Widget Specifications

### 4.1 Stocks Widget

#### Display

The stocks widget has two modes:

**List mode (default):**
```
 ╭─ Stocks ──────────────────────────────────────────────────────╮
 │                                                               │
 │   54.23 ┤                                                     │
 │   54.10 ┤        ╭─╮                                         │
 │   53.97 ┤   ╭────╯ ╰──╮        ╭──╮                         │
 │   53.84 ┤───╯         ╰────────╯  ╰──╮                      │
 │   53.71 ┤                              ╰───                  │
 │          └──────────────────────────────────                  │
 │           9:30    10:30   11:30   12:30  1:30                 │
 │                                                               │
 │   DJI     ▲ 38,412.50   +0.42%  │  AAPL   ▲ 198.32  +1.12% │
 │   IXIC    ▼ 16,301.22   -0.18%  │  GOOGL  ▲ 178.91  +0.67% │
 │   SPX     ▲  5,321.88   +0.31%  │  MSFT   ▼ 425.10  -0.23% │
 │                                  │  TSLA   ▲ 241.55  +2.34% │
 │                                  │  NVDA   ▲ 131.20  +3.01% │
 │                                                               │
 │   [%]  $  Mkt Cap          ◀ DJI ▸  ← → to switch ticker    │
 ╰───────────────────────────────────────────────────────────────╯
```

- **Top area:** ASCII-art intraday price graph for the currently selected ticker (drawn using Ratatui's `Canvas` or braille-dot characters for higher resolution).
- **Bottom-left:** Major indices (DJI, IXIC/NASDAQ Composite, SPX) with colored day change.
- **Bottom-right:** User-configured watchlist tickers with colored day change.
- **Footer:** Display mode indicator and navigation hint.

**Detail mode** (activated by `s <ticker>` command):
```
 ╭─ AAPL — Apple Inc. ──────────────────────────────────────────╮
 │                                                               │
 │   199.10 ┤                                           ╭──     │
 │   198.50 ┤              ╭───╮       ╭──╮  ╭─────────╯       │
 │   197.90 ┤    ╭─────────╯   ╰──────╯  ╰──╯                  │
 │   197.30 ┤────╯                                               │
 │   196.70 ┤                                                    │
 │           └──────────────────────────────────────────          │
 │            9:30     10:30    11:30    12:30    1:30            │
 │                                                               │
 │   Price:    198.32      Day Change:  +$2.20 (+1.12%)         │
 │   Open:     196.12      Prev Close:  196.12                  │
 │   Day H/L:  199.15 / 195.80                                  │
 │   Week H/L: 201.30 / 193.45                                  │
 │   52w H/L:  220.80 / 164.08                                  │
 │   Volume:   48.2M       Avg Volume:  52.1M                   │
 │   Mkt Cap:  3.02T       P/E:         31.2                    │
 │   Yield:    0.52%       EPS:         6.35                    │
 │                                                               │
 │   Press ESC to return to list view                            │
 ╰───────────────────────────────────────────────────────────────╯
```

#### Display Cycling

The ticker list supports three display modes, cycled with dedicated keys:

| Key | Mode | Example display |
|-----|------|----------------|
| `1` or `%` | % change (default) | `AAPL ▲ +1.12%` |
| `2` or `$` | Price change | `AAPL ▲ +$2.20` |
| `3` or `m` | Market cap | `AAPL 3.02T` |

#### Color Scheme

- **Green** (`Color::Green`): price/% increase from previous close
- **Red** (`Color::Red`): price/% decrease from previous close
- **Gray** (`Color::DarkGray`): unchanged / market closed

#### Commands

| Command | Action |
|---------|--------|
| `s <ticker or name>` | Zoom to detail view for that stock |
| `ESC` | Return from detail to list view |
| `←` / `→` | Cycle through tickers for the graph display |
| `↑` / `↓` | Scroll the ticker list |
| `1` / `2` / `3` | Cycle display mode (%, $, mkt cap) |

#### Data Source

- **Default provider:** Yahoo Finance v8 API (unofficial REST endpoints)
- Endpoint for quotes: `https://query1.finance.yahoo.com/v8/finance/chart/{symbol}`
- Endpoint for search/lookup: `https://query1.finance.yahoo.com/v1/finance/search?q={query}`
- **Poll interval:** configurable, default 15 seconds during market hours, 5 minutes outside
- **Rate limiting:** built-in request throttle to avoid Yahoo's IP-based rate limits

#### Config (`~/.config/glint/stocks.toml`)

```toml
watchlist = ["AAPL", "GOOGL", "MSFT", "TSLA", "NVDA"]
indices = ["^DJI", "^IXIC", "^GSPC"]
default_display_mode = "percent"   # "percent" | "price" | "marketcap"
poll_interval_market_secs = 15
poll_interval_off_hours_secs = 300
graph_style = "braille"            # "braille" | "box_drawing"
market_timezone = "America/New_York"
```

---

### 4.2 Calendar Widget

#### Display

Three view modes: **day**, **week**, and **month**.

**Day view (default):**
```
 ╭─ Calendar ─── Wed, May 20 2026 ──────────────────────────────╮
 │                                                               │
 │   TODAY                                                       │
 │                                                               │
 │   09:00  ┃ ██ Team Standup                        (30 min)   │
 │   09:30  ┃                                                    │
 │   10:00  ┃ ██ Design Review — Project Alpha       (60 min)   │
 │   11:00  ┃                                                    │
 │   11:30  ┃ ██ 1:1 with Sarah                      (30 min)   │
 │   12:00  ┃ ░░ Lunch                                           │
 │   13:00  ┃                                                    │
 │   14:00  ┃ ██ Sprint Planning                     (90 min)   │
 │   15:30  ┃                                                    │
 │                                                               │
 │   ▼ 2 more events below                                      │
 │                                                               │
 │   ← prev day    → next day    [d]ay [w]eek [m]onth           │
 ╰───────────────────────────────────────────────────────────────╯
```

**Week view:**
```
 ╭─ Calendar ─── Week of May 18, 2026 ──────────────────────────╮
 │                                                               │
 │   Mon 18   Tue 19  *Wed 20*  Thu 21   Fri 22   Sat   Sun    │
 │    ·  ·      ·  ·    ●  ●     ·  ·     ·         ·          │
 │                                                               │
 │   ── Wednesday, May 20 (Today) ──                             │
 │                                                               │
 │   09:00  Team Standup                             (30 min)   │
 │   10:00  Design Review — Project Alpha            (60 min)   │
 │   11:30  1:1 with Sarah                           (30 min)   │
 │   14:00  Sprint Planning                          (90 min)   │
 │   16:00  Code Review Session                      (45 min)   │
 │                                                               │
 │   ▼ 1 more event                                             │
 │                                                               │
 │   ← → navigate days   ▲ ▼ scroll events                      │
 ╰───────────────────────────────────────────────────────────────╯
```

- Days of the week shown across the top.
- **Today** is highlighted with `*asterisks*` or bold.
- **Dots** (●) under each day indicate event density (e.g., `●` = has events, `·` = no events, `●●` = busy day).
- Selecting a day with `←`/`→` shows that day's events below.

**Month view:**
```
 ╭─ Calendar ─── May 2026 ──────────────────────────────────────╮
 │                                                               │
 │     Mon   Tue   Wed   Thu   Fri   Sat   Sun                  │
 │                                 1     2     3                 │
 │      4     5     6     7     8     9    10                    │
 │     11    12    13    14    15    16    17                     │
 │     18    19   [20]   21    22    23    24                     │
 │     25    26    27    28    29    30    31                     │
 │                                                               │
 │     ● = has events     [20] = today                           │
 │                                                               │
 │   Days with events: 4● 5● 6● 7  8● 9  10                    │
 │                                                               │
 │   ← → prev/next month   Enter = jump to day view             │
 ╰───────────────────────────────────────────────────────────────╯
```

- Standard month grid with today highlighted in brackets `[20]`.
- Days with events get a dot indicator.
- Pressing `Enter` on a day switches to day view for that date.

#### Scrolling Events

When more events exist than can fit in the visible area, `▼ N more events below` / `▲ N more events above` indicators appear. Arrow keys `↑`/`↓` scroll through them.

#### Commands

| Command | Action |
|---------|--------|
| `cal <date>` | Jump to specified date. Accepts: `yyyy-mm-dd`, `mm/dd`, `mm-dd`, `today`, `tomorrow` |
| `d` | Switch to day view |
| `w` | Switch to week view |
| `m` | Switch to month view |
| `t` | Jump to today |
| `←` / `→` | Navigate prev/next day (day/week view) or prev/next month (month view) |
| `↑` / `↓` | Scroll events list |
| `Enter` | In month view, jump to day view for selected date |

#### Data Source

- **Google Calendar API** via OAuth 2.0 with read-only scope (`calendar.readonly`)
- Uses the Events:list endpoint with `timeMin`/`timeMax` for the visible range
- OAuth flow: on first run, opens browser for consent, stores refresh token locally
- **Poll interval:** configurable, default 60 seconds

#### Config (`~/.config/glint/calendar.toml`)

```toml
provider = "google"
calendar_ids = ["primary", "work@group.calendar.google.com"]
default_view = "day"        # "day" | "week" | "month"
poll_interval_secs = 60
time_format = "24h"         # "24h" | "12h"
week_start = "monday"       # "monday" | "sunday"
max_visible_events = 8

[working_hours]
start = "08:00"
end = "18:00"

# Colors assigned to each calendar for merged view
[calendar_colors]
primary = "blue"
"work@group.calendar.google.com" = "green"
```

---

### 4.3 News Widget

#### Display

```
 ╭─ News ─── Top Stories ────────────────────────────────────────╮
 │                                                               │
 │  1. AI Chip Export Controls Tighten as US-China             │
 │     Tensions Escalate                                        │
 │     Reuters · 23 min ago                          [Tech|Gov] │
 │                                                               │
 │  ▶ 2. Federal Reserve Signals Pause on Rate Cuts             │
 │     Through Q3 2026                                          │
 │     AP News · 1 hr ago                            [Finance]  │
 │   ┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄  │
 │   The Federal Reserve indicated in today's FOMC minutes      │
 │   that interest rates will remain at current levels through  │
 │   the third quarter, citing persistent inflation in the      │
 │   services sector...                                         │
 │   → https://apnews.com/article/fed-rates-2026-q3            │
 │   ┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄  │
 │                                                               │
 │  3. SpaceX Starship Completes First Orbital Refueling        │
 │     Test Successfully                                        │
 │     BBC · 2 hrs ago                               [Science]  │
 │                                                               │
 │  4. European Parliament Passes Comprehensive AI              │
 │     Liability Framework                                      │
 │     The Guardian · 3 hrs ago                     [Tech|Gov]  │
 │                                                               │
 │   ▼ 12 more headlines                                        │
 │                                                               │
 │   ↑↓ navigate   Enter = expand/collapse   o = open in browser│
 ╰───────────────────────────────────────────────────────────────╯
```

- Headlines listed in reverse chronological order.
- Each headline shows: title, source, time ago, and topic tags.
- **Expanding** a headline (Enter or `→`) reveals an AI-generated summary (2–4 sentences) and a link to the original article.
- The `o` key opens the article URL in the user's default browser.

#### Summary Generation

Article summaries can be generated in two ways (configurable):

1. **Pre-fetched excerpts** — pull the article's meta description or first paragraph from the RSS/API response. Fast, no external dependency.
2. **AI-generated** — send the article URL/content to a local or remote LLM for summarization. Higher quality but requires an LLM endpoint.

The initial implementation ships with option 1. Option 2 is a future extension point, configured via:

```toml
summary_mode = "excerpt"   # "excerpt" | "llm_local" | "llm_openai" | "llm_anthropic" (future)
```

Future values: `"llm_local"`, `"llm_openai"`, `"llm_anthropic"`.

#### Data Sources

News is aggregated from RSS feeds and/or news APIs. The initial set of sources:

| Source | Type | URL / Endpoint |
|--------|------|----------------|
| Reuters | RSS | `https://www.reutersagency.com/feed/` |
| AP News | RSS | `https://rsshub.app/apnews/topics/{topic}` |
| BBC News | RSS | `https://feeds.bbci.co.uk/news/rss.xml` |
| Hacker News | API | `https://hacker-news.firebaseio.com/v0/` |
| NewsAPI.org | API | `https://newsapi.org/v2/top-headlines` (free tier, API key) |

Sources are configurable; users can add/remove RSS feeds or API endpoints.

#### Topic Filtering

Headlines are filtered and ranked by a configurable priority list of topics:

```toml
topic_priority = ["Tech", "Finance", "Science"]
max_headlines = 25

[[topics]]
name = "Tech"
keywords = ["AI", "software", "startup", "chip", "semiconductor"]

[[topics]]
name = "Finance"
keywords = ["Fed", "interest rate", "earnings", "stock", "IPO"]

[[topics]]
name = "Science"
keywords = ["space", "climate", "research", "physics"]
```

Articles matching higher-priority topics appear first. Articles matching no configured topic are deprioritized but still shown at the bottom.

#### Commands

| Command | Action |
|---------|--------|
| `↑` / `↓` | Navigate headlines |
| `Enter` or `→` | Expand/collapse headline summary |
| `o` | Open article in default browser |
| `r` | Refresh news feed |
| `1`–`9` | Jump to headline by number |

#### Config (`~/.config/glint/news.toml`)

```toml
topic_priority = ["Tech", "Finance"]
summary_mode = "excerpt"
poll_interval_secs = 300
max_headlines = 25
max_summary_length = 280

[[sources]]
name = "Reuters"
type = "rss"
url = "https://www.reutersagency.com/feed/"

[[sources]]
name = "Hacker News"
type = "api"
url = "https://hacker-news.firebaseio.com/v0/"

[[sources]]
name = "BBC"
type = "rss"
url = "https://feeds.bbci.co.uk/news/rss.xml"

[[topics]]
name = "Tech"
keywords = ["AI", "software", "startup"]

[[topics]]
name = "Finance"
keywords = ["Fed", "interest rate", "earnings"]
```

---

## 5. Global UI & Interaction

### 5.1 Command Bar

A command bar at the bottom of the screen, activated by pressing `:` (vim-style) or `/`:

```
 ┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄
 : s AAPL                                                       
```

Commands are routed first to the focused widget, then to global handlers. This allows widget-specific commands (like `s <ticker>`) to coexist with global ones (like `:quit`).

**Global commands:**

| Command | Action |
|---------|--------|
| `:quit` / `:q` | Exit glint |
| `:reload` | Reload config from disk |
| `:layout <preset>` | Switch to a named layout preset |
| `Tab` | Cycle focus to next widget |
| `Shift+Tab` | Cycle focus to previous widget |
| `?` | Show help overlay with keybindings |

### 5.2 Focus Model

One widget has focus at a time, indicated by a highlighted border. Focused widgets receive key events first. `Tab` cycles focus in the order widgets appear in the grid config (left-to-right, top-to-bottom).

### 5.3 Status Bar

A thin status bar at the very bottom shows:

```
 glint v0.1.0 │ ● stocks (15s) │ ● calendar (60s) │ ● news (5m) │ Last: 14:32:05
```

Shows connection status per data provider and last successful fetch time.

---

## 6. Configuration System

### 6.1 File Location

Configuration lives at `~/.config/glint/` following the XDG Base Directory Specification:

```
~/.config/glint/
├── config.toml          # Main config (layout, global settings)
├── stocks.toml          # Stocks widget config
├── calendar.toml        # Calendar widget config  
├── news.toml            # News widget config
├── llm.toml             # LLM provider config (model, preferences)
└── credentials/         # OAuth tokens, API keys (gitignored)
    ├── google_oauth.json # OAuth tokens remain JSON (standard format)
    ├── anthropic_key.toml
    └── newsapi_key.toml
```

### 6.2 Config Layering

Configuration is resolved in this order (later overrides earlier):

1. **Built-in defaults** — compiled into the binary
2. **User config** — `~/.config/glint/*.toml`
3. **CLI flags** — `--layout`, `--poll-interval`, etc.
4. **Environment variables** — `GLINT_STOCKS_POLL=10` etc.

### 6.3 Main Config (`config.toml`)

```toml
version = 1

[global]
theme = "default"
command_key = ":"
refresh_all_on_focus = true
log_level = "info"
log_file = "~/.config/glint/glint.log"

# Default layout
[layout]
columns = [60, 40]
rows = [50, 50]

[[layout.cells]]
widget = "stocks"
col = 0
row = 0
col_span = 1
row_span = 2

[[layout.cells]]
widget = "calendar"
col = 1
row = 0

[[layout.cells]]
widget = "news"
col = 1
row = 1

# Named layout presets (switch with :layout <name>)
[layout.presets.stocks-focus]
columns = [100]
rows = [100]
cells = [{ widget = "stocks", col = 0, row = 0 }]

[layout.presets.two-up]
columns = [50, 50]
rows = [100]
cells = [
  { widget = "stocks", col = 0, row = 0 },
  { widget = "news", col = 1, row = 0 },
]
```

### 6.4 Live Reload

glint watches config files via `notify` (Rust file-watcher crate). When a file changes, the app re-reads it and calls `apply_config()` on the affected widget without restarting.

---

## 7. LLM Integration

### 7.1 Overview & Philosophy

glint uses LLM capabilities selectively, following a **structured-first, LLM-fallback** principle: every task that can be handled by an API lookup, library, or pattern match should be. The LLM is invoked only where it provides a qualitative improvement that simpler methods cannot match — primarily semantic understanding, summarization, and disambiguation.

All LLM calls are **non-blocking and failure-tolerant**. If the LLM is unavailable or the API key is not configured, glint degrades gracefully: news falls back to keyword ranking and RSS excerpts, stock lookups present all API candidates without disambiguation, and so on. No widget should hard-depend on LLM availability.

### 7.2 LLM Configuration (`~/.config/glint/llm.toml`)

```toml
# Enable/disable LLM features globally
enabled = true

# Provider configuration — Anthropic is the initial (and currently only) provider
[provider]
name = "anthropic"
model = "claude-sonnet-4-5-20250514"     # any Anthropic model identifier
api_base = "https://api.anthropic.com"   # override for proxies or compatible endpoints
max_tokens = 1024                        # default max response tokens

# Rate limiting to control costs
[limits]
max_requests_per_minute = 30
max_tokens_per_hour = 50000
budget_warning_threshold = 0.80          # warn at 80% of hourly budget

# Per-feature toggles — each can be disabled independently
[features]
news_ranking = true          # semantic topic classification for news headlines
news_summaries = true        # AI-generated article summaries
stock_disambiguation = true  # resolve ambiguous company name → ticker matches
calendar_search = false      # natural language calendar queries (future)
nl_commands = false           # natural language command interpretation (v2)
```

**API key storage** (`~/.config/glint/credentials/anthropic_key.toml`):

```toml
api_key = "sk-ant-..."
```

File permissions are set to `0600` on creation. The API key can alternatively be provided via the `ANTHROPIC_API_KEY` environment variable, which takes precedence over the file.

### 7.3 LLM Client Abstraction

The LLM integration is abstracted behind a trait, allowing future providers (OpenAI, local models via Ollama, etc.) to be added:

```rust
pub trait LlmProvider: Send + Sync {
    /// Provider name for config/logging
    fn name(&self) -> &str;

    /// Send a prompt and receive a text completion
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse>;

    /// Check if the provider is configured and reachable
    async fn health_check(&self) -> Result<bool>;
}

pub struct LlmRequest {
    pub system: Option<String>,
    pub messages: Vec<LlmMessage>,
    pub max_tokens: u32,
    pub temperature: f32,
}

pub struct LlmResponse {
    pub text: String,
    pub usage: TokenUsage,
}
```

The initial implementation ships with `AnthropicProvider` using the Anthropic Messages API via `reqwest`.

### 7.4 Integration Points by Widget

#### Stocks — Disambiguation

**Trigger:** The `s <query>` command first calls Yahoo Finance's search endpoint. If the search returns **exactly one match**, that ticker is used directly — no LLM involved. If it returns **2 or more matches**, the LLM is called to disambiguate.

**Flow:**
```
User types: s Apple
  → Yahoo Finance search("Apple")
  → Returns: [AAPL (Apple Inc), APLE (Apple Hospitality REIT), AGFY (Agrify Corp)]
  → LLM prompt: "The user searched for 'Apple' in a stock dashboard.
     Candidates: AAPL (Apple Inc, $3T market cap), APLE (Apple Hospitality REIT, $3.8B),
     AGFY (Agrify Corp, $2M). Which ticker did the user most likely mean?
     Respond with only the ticker symbol."
  → LLM returns: "AAPL"
  → Display AAPL detail view
```

**Fallback:** If LLM is unavailable, show the candidate list and let the user select with arrow keys + Enter.

**Latency budget:** The disambiguation call should use `claude-haiku-4-5-20251001` (fastest, cheapest) since it's a simple classification task. Configurable via:

```toml
[features.stock_disambiguation]
model_override = "claude-haiku-4-5-20251001"   # use fastest model for this
```

#### News — Two-Pass Relevance Ranking

**Pass 1 (keyword filter):** Each headline is scored against configured topic keywords using simple substring/regex matching. Headlines matching at least one keyword in any topic are tagged as candidates. Headlines matching no keywords are placed in an "unclassified" bucket.

**Pass 2 (LLM classification):** The candidate headlines plus the unclassified bucket are sent to the LLM in a single batch call for semantic topic classification and relevance scoring.

**Prompt structure:**
```
System: You are a news classifier for a terminal dashboard. Classify each
headline into the provided topics and assign a relevance score (0-100).
Respond as JSON.

Topics:
- Tech: AI, software, startups, semiconductors, cloud computing
- Finance: Federal Reserve, interest rates, earnings, IPOs, markets
- Science: Space, climate, research, physics, medicine

Headlines:
1. "TSMC Plans $65B Arizona Fab Expansion"
2. "Federal Reserve Signals Rate Pause Through Q3"
3. "New Species of Deep-Sea Fish Discovered Near Mariana Trench"
4. "Local Restaurant Wins Best Pizza Award"
```

**Response format (expected):**
```json
[
  {"index": 1, "topic": "Tech", "relevance": 92},
  {"index": 2, "topic": "Finance", "relevance": 95},
  {"index": 3, "topic": "Science", "relevance": 78},
  {"index": 4, "topic": null, "relevance": 15}
]
```

Headlines are then sorted by: topic priority order first, relevance score second. Articles classified as `null` or below a configurable relevance threshold (default: 30) are pushed to the bottom.

**Fallback:** If LLM is unavailable, Pass 1 keyword scores are used directly. Unclassified articles are shown at the bottom sorted by recency.

#### News — Article Summaries

When a user expands a headline, the summary is generated on demand (not pre-fetched for all articles):

**Flow:**
```
User presses Enter on headline
  → Check summary cache (in-memory LRU, persists for session)
  → Cache miss: fetch article content (RSS description + link)
  → LLM prompt: "Summarize this news article in 2-3 sentences for a terminal
     dashboard. Be factual and concise. Article: [RSS description or fetched excerpt]"
  → Cache and display the summary
```

**Fallback:** If LLM is unavailable or the feature is disabled, display the RSS `<description>` field or "No summary available."

**Model choice:** Summaries use the model configured in `llm.toml`. Since summarization benefits from quality, the default model (`claude-sonnet-4-5-20250514`) is appropriate here, but users on a budget can switch to Haiku.

#### Calendar — Semantic Search (Future, v2)

Not implemented in v1. The design reserves a slot for natural language calendar queries like "when is my next meeting with Sarah?" that would search across event titles, descriptions, and attendee names using LLM-powered semantic matching.

#### Command Bar — Natural Language Commands (Future, v2)

Not implemented in v1. The design reserves a slot for interpreting free-form commands like "show me how tech stocks are doing today" and routing them to the appropriate widget action. In v1, unrecognized commands display an error with suggestions based on fuzzy string matching (`strsim` crate).

### 7.5 Cost & Performance Controls

LLM calls have real cost implications. glint includes several guardrails:

| Control | Description |
|---------|-------------|
| Per-feature toggles | Each LLM feature can be independently enabled/disabled |
| Model override per feature | Use cheaper models (Haiku) for simple tasks, better models for summaries |
| Rate limiter | Configurable max requests/minute and tokens/hour |
| Budget warning | Log warning when approaching hourly token budget |
| Response caching | In-memory LRU cache for summaries and classifications; avoids re-classifying the same headlines |
| Batch calls | News classification sends all headlines in one API call, not one per headline |
| Graceful degradation | Every LLM feature has a non-LLM fallback path |

### 7.6 Input Resolution Strategy (Summary)

This table summarizes how each type of user input is resolved across all widgets:

| Input | Widget | Step 1 (fast) | Step 2 (LLM, if needed) |
|-------|--------|---------------|------------------------|
| Ticker/company name | Stocks | Yahoo Finance search API | Disambiguate if 2+ results |
| Date expression | Calendar | `two-timer` / `chrono-english` crate | — (not needed) |
| City name | Weather (future) | Geocoding API (OpenWeatherMap/Nominatim) | Disambiguate if 2+ results |
| News topic relevance | News | Keyword substring match | Semantic classification + scoring |
| Article summary | News | RSS `<description>` field | On-demand LLM summarization |
| Typo correction | Global | `strsim` edit-distance fuzzy match | — (not needed) |
| Free-form command | Global (v2) | — | LLM intent parsing (future) |

---

## 8. Extensibility

### 7.1 Adding a New Widget

To add a widget (e.g., "weather"), a developer:

1. Creates `src/widgets/weather.rs` implementing the `Widget` trait
2. Creates a corresponding `DataProvider` impl for the weather API
3. Registers it in the widget registry (`src/widgets/mod.rs`)
4. Adds a default config section
5. Users can place it in their grid layout by ID

No changes to the core event loop, renderer, or config system are needed.

### 7.2 Future Widget Ideas

| Widget | Description |
|--------|-------------|
| Weather | Local forecast, hourly/daily, ASCII art conditions |
| System | CPU, memory, disk, network utilization |
| Todo | Task list synced with Todoist / Things / local file |
| Git | Recent commits, PR status, CI pipeline for configured repos |
| Crypto | Cryptocurrency prices (reuse stocks architecture) |
| Pomodoro | Focus timer with session tracking |
| Email | Unread count + recent subjects from IMAP/Gmail |

### 7.3 Plugin System (Future)

For v2, a plugin system could allow distributing widgets as separate crates or dynamic libraries. The current trait-based architecture is designed to make this transition straightforward. Potential approaches: WASM plugins, dynamic loading via `libloading`, or a separate process model with IPC.

---

## 9. Design Decisions (Resolved)

The following decisions were evaluated and locked in during the design phase.

### D1: Graph Rendering — Braille Dots

Use **braille characters** (U+2800–U+28FF) for stock chart rendering. Provides ~160×100 dot resolution in a typical terminal. A `graph_style` config option allows falling back to `"box_drawing"` for terminals without Unicode support.

### D2: Authentication Storage — Plain File

Store Google Calendar OAuth tokens as plain JSON in `~/.config/glint/credentials/` with **0600 file permissions**, matching the convention used by `gcloud`, `gh`, and similar CLI tools. OS keychain integration is a future enhancement.

### D3: Yahoo Finance Reliability — Cache + Retry

Mitigate Yahoo Finance's lack of SLA with:
- Exponential backoff on failed requests (max 5 retries, capped at 60s delay)
- Local response cache — serve stale data when fetch fails
- Status bar shows last-successful-fetch timestamp and a `⚠ stale` indicator
- The `DataProvider` trait makes it straightforward to swap to Alpha Vantage or Polygon.io later without touching widget code

No secondary provider ships in v1.

### D4: News Summaries — RSS Excerpts

Ship with RSS `<description>` / meta-description extraction for article summaries. Full-article fetching and LLM-generated summaries are deferred to a future release behind the `summary_mode` config key.

### D5: Offline Mode — Cached Data with Stale Indicator

When the network is unavailable, each widget continues displaying its last-fetched data. A visual `⚠ stale` marker appears on the widget border and the status bar shows how old the data is. Background retries happen silently at the normal poll interval.

### D6: Keybindings — Sensible Defaults Only in v1

v1 ships with vim-inspired defaults (`:` for command bar, `hjkl`/arrow navigation, `Tab` for focus cycling). Keybinding customization via a `keybindings.toml` file is planned for v1.1.

### D7: Color Theme — ANSI Semantic Colors

Use ANSI semantic color names (`Red`, `Green`, `Blue`, `Yellow`, etc.) which inherit from the user's terminal color scheme. This ensures glint looks native in any theme (Dracula, Solarized, Catppuccin, etc.). An optional `[theme]` section in `config.toml` allows overriding individual colors:

```toml
[theme]
positive = "green"
negative = "red"
accent = "cyan"
muted = "dark_gray"
```

### D8: Multi-Calendar — Merged Timeline with Color-Coding

All configured calendars are merged into a single chronological timeline. Each calendar is assigned a color in `calendar.toml` via the `[calendar_colors]` table. Calendars can be enabled/disabled in config; a runtime toggle command is planned for v1.1.

### D9: Command Routing — Focused Widget Wins + Startup Warning

The focused widget gets first priority for command dispatch. If a command is not consumed, it falls through to the global handler.

**Conflict detection:** At startup, glint scans all registered widgets' command prefixes. If two or more widgets register the same prefix (e.g., both `stocks` and a future `crypto` widget register `s`), a warning is printed to stderr:

```
⚠ glint: command prefix "s" registered by both [stocks, crypto].
  The focused widget will take priority. Use "stocks:s" or "crypto:s" to disambiguate.
```

Namespaced commands (`widget_id:command`) are always available as a disambiguation fallback.

---

## 10. Build & Distribution

| Method | Notes |
|--------|-------|
| `cargo install glint-tui` | From crates.io |
| Homebrew | `brew install glint` via a tap |
| Pre-built binaries | GitHub Releases for macOS (arm64, x86), Linux (x86_64, aarch64) |
| Nix | Flake for NixOS users |
| AUR | For Arch Linux |

First-run experience: `glint --init` creates `~/.config/glint/` with default config files and walks the user through Google Calendar OAuth setup interactively.

---

## 11. Development Phases

### Phase 1 — Foundation (MVP)
- Core event loop, config system, grid layout
- Stocks widget with list mode, ASCII graph, Yahoo Finance provider
- Basic keybindings and command bar

### Phase 2 — Calendar + Polish
- Calendar widget with day/week/month views
- Google Calendar OAuth integration
- Status bar, focus model, live config reload

### Phase 3 — News + Release
- News widget with RSS aggregation and topic filtering
- Excerpt-based summaries
- First-run setup (`glint --init`)
- Homebrew tap, pre-built binaries

### Phase 4 — LLM Integration
- LLM client abstraction (`LlmProvider` trait) + `AnthropicProvider` implementation
- `llm.toml` config with per-feature toggles, model overrides, rate limiting
- News: two-pass relevance ranking (keyword pre-filter → LLM classification)
- News: on-demand AI-generated article summaries with LRU caching
- Stocks: LLM-powered disambiguation for ambiguous company name lookups
- Graceful degradation paths for all LLM features when unavailable

### Phase 5 — Polish & Enhancements
- Stock detail mode with full statistics
- Keybinding customization (`keybindings.toml`)
- Additional calendar providers (Outlook, CalDAV)
- Natural language command bar interpretation (v2, off by default)
- Semantic calendar search ("when is my next meeting with Sarah?")
- Plugin system exploration
- Additional LLM providers (OpenAI, Ollama for local models)
