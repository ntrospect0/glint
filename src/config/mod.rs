pub mod layout;
pub mod types;
pub mod watcher;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use layout::LayoutConfig;
pub use types::Config;

/// Load a per-widget TOML config from `~/.config/glint/<name>.toml`. Returns
/// `T::default()` if the file does not exist.
pub fn load_widget_toml<T>(name: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    let path = config_dir()?.join(format!("{name}.toml"));
    if !path.exists() {
        return Ok(T::default());
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read widget config at {}", path.display()))?;
    let value: T = toml::from_str(&contents)
        .with_context(|| format!("failed to parse widget config at {}", path.display()))?;
    Ok(value)
}

/// Returns `~/.config/glint/` on every platform (overridable with
/// `$XDG_CONFIG_HOME`). The XDG Base Directory layout is what the spec
/// promises, so we use it consistently rather than falling back to
/// `~/Library/Application Support/` on macOS or `%APPDATA%` on Windows.
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("glint"));
        }
    }
    let home = dirs::home_dir().context("could not locate user home directory")?;
    Ok(home.join(".config").join("glint"))
}

/// Returns the path to the main config file (`config.toml`).
pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Load the main config from disk. If the path does not exist, returns the
/// built-in defaults. CLI-supplied `override_path` takes precedence over the
/// XDG default location.
pub fn load(override_path: Option<&Path>) -> Result<Config> {
    let path: PathBuf = match override_path {
        Some(p) => p.to_path_buf(),
        None => config_path()?,
    };

    if !path.exists() {
        tracing::info!(path = %path.display(), "config file not found, using built-in defaults");
        return Ok(Config::default());
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file at {}", path.display()))?;
    let cfg: Config = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file at {}", path.display()))?;
    Ok(cfg)
}

/// Default `config.toml` contents written by `--init`.
pub const DEFAULT_CONFIG_TOML: &str = r#"version = 1

[global]
# Active color scheme — looked up in colorschemes.toml under [schemes.<name>].
# Ships with "default", "nord", and "gruvbox_dark". An unrecognized name falls
# back to glint's built-in palette.
theme = "default"
command_key = ":"
refresh_all_on_focus = true
log_level = "info"

[layout]
columns = [40, 60]
rows = [35, 35, 30]

[[layout.cells]]
widget = "clock"
col = 0
row = 0

[[layout.cells]]
widget = "calendar"
col = 1
row = 0

[[layout.cells]]
widget = "weather"
col = 0
row = 1

[[layout.cells]]
widget = "news"
col = 1
row = 1

[[layout.cells]]
widget = "stocks"
col = 0
row = 2
col_span = 2
"#;

pub const DEFAULT_CLOCK_TOML: &str = r#"# Optional IANA timezone name for the primary clock; defaults to system local time.
# timezone = "America/Vancouver"
show_seconds = false              # show :SS in the big block-digit display
show_seconds_ticker = true        # show a small ticking HH:MM:SS below the big digits
show_date = true
hour_format = "24h"               # "12h" or "24h"

# Additional world clocks rendered when there's vertical room.
[[secondary_timezones]]
label = "New York"
timezone = "America/New_York"

[[secondary_timezones]]
label = "London"
timezone = "Europe/London"

[[secondary_timezones]]
label = "Tokyo"
timezone = "Asia/Tokyo"

[[secondary_timezones]]
label = "Taipei"
timezone = "Asia/Taipei"
"#;

pub const DEFAULT_WEATHER_TOML: &str = r#"# Open-Meteo is free and key-less. Set lat/lon to your city.
# Comment out latitude + longitude (and leave auto_locate = true) to fall back
# to IP-based geolocation via ipapi.co.
label = "Richmond, BC"
latitude = 49.166
longitude = -123.133
units = "metric"                  # "metric" (°C, km/h) or "imperial" (°F, mph)
poll_interval_secs = 600
auto_locate = true                # only consulted when lat/lon are unset
"#;

pub const DEFAULT_NEWS_TOML: &str = r#"# Poll cadence in seconds (floor 60).
poll_interval_secs = 900

# When true, horizontal mouse scroll cycles the filter tabs. Disabled by
# default because trackpad sideways gestures often fire accidentally.
horizontal_scroll_filters = false

# Show the topic categorization (e.g. `[Business,World]`) on each article's
# meta row. Many users prefer the quieter look — flip this off to hide.
show_topic_labels = true

# RSS / Atom feeds to aggregate. `label` is shown in the article row.
# All sources here are free / non-paywall to read (some have paywalls on
# the article pages themselves — your browser session handles auth when
# you hit Enter to open). Add or remove freely.

# ── Tech ─────────────────────────────────────────────────────────────────────
[[feeds]]
label = "Hacker News"
url = "https://hnrss.org/frontpage"

[[feeds]]
label = "Ars Technica"
url = "https://feeds.arstechnica.com/arstechnica/index"

[[feeds]]
label = "The Verge"
url = "https://www.theverge.com/rss/index.xml"

[[feeds]]
label = "Engadget"
url = "https://www.engadget.com/rss.xml"

[[feeds]]
label = "Phoronix"
url = "https://www.phoronix.com/rss.php"

# ── World ────────────────────────────────────────────────────────────────────
[[feeds]]
label = "BBC News"
url = "http://feeds.bbci.co.uk/news/rss.xml"

[[feeds]]
label = "BBC World"
url = "http://feeds.bbci.co.uk/news/world/rss.xml"

[[feeds]]
label = "Guardian World"
url = "https://www.theguardian.com/world/rss"

[[feeds]]
label = "NPR World"
url = "https://feeds.npr.org/1004/rss.xml"

# ── Business / Finance ───────────────────────────────────────────────────────
[[feeds]]
label = "BBC Business"
url = "http://feeds.bbci.co.uk/news/business/rss.xml"

[[feeds]]
label = "Yahoo Finance"
url = "https://finance.yahoo.com/news/rssindex"

[[feeds]]
label = "MarketWatch"
url = "http://feeds.marketwatch.com/marketwatch/topstories/"

[[feeds]]
label = "CNBC Top"
url = "https://www.cnbc.com/id/100003114/device/rss/rss.html"

# WSJ / Barron's headline feeds are public; article bodies are paywalled
# but your browser session opens them when you hit Enter.
# [[feeds]]
# label = "WSJ Markets"
# url = "https://feeds.a.dj.com/rss/RSSMarketsMain.xml"
# [[feeds]]
# label = "Barron's"
# url = "https://feeds.a.dj.com/rss/RSSBarronsMain.xml"

# ── Canada ───────────────────────────────────────────────────────────────────
[[feeds]]
label = "CBC News"
url = "https://www.cbc.ca/webfeed/rss/rss-topstories"

[[feeds]]
label = "CBC Politics"
url = "https://www.cbc.ca/webfeed/rss/rss-politics"

[[feeds]]
label = "CBC Business"
url = "https://www.cbc.ca/webfeed/rss/rss-business"

[[feeds]]
label = "CTV News"
url = "https://www.ctvnews.ca/rss/ctvnews-ca-top-stories-public-rss-1.822009"

# ── Entertainment ────────────────────────────────────────────────────────────
[[feeds]]
label = "Pitchfork"
url = "https://pitchfork.com/rss/news/"

[[feeds]]
label = "Hollywood Reporter"
url = "https://www.hollywoodreporter.com/feed/"

# Topics tag articles whose title/summary contains any keyword (case-insensitive
# substring match) and double as filter tabs across the top of the news cell
# (←/→ to cycle). Add, rename, or remove tabs by editing this list.
[[topics]]
label = "Tech"
keywords = [
  "AI", "OpenAI", "Anthropic", "LLM", "GPU", "developer", "Linux", "Rust",
  "Apple", "Google", "Microsoft", "Meta", "chip", "software", "startup",
  "open source", "GitHub",
]

[[topics]]
label = "Business"
keywords = [
  "CEO", "merger", "acquisition", "IPO", "revenue", "earnings", "quarterly",
  "Wall Street", "market", "Fed", "inflation", "interest rate", "Bitcoin",
  "crypto", "yield", "treasury", "stocks", "bonds", "dividend", "trader",
]

[[topics]]
label = "World"
keywords = [
  "Ukraine", "Russia", "China", "EU", "UN", "climate", "war", "election",
  "summit", "treaty", "Israel", "Gaza", "Iran", "NATO", "global", "Brussels",
  "international",
]

[[topics]]
label = "Canada"
keywords = [
  "Canada", "Canadian", "Ottawa", "Toronto", "Vancouver", "Montreal",
  "Quebec", "Alberta", "B.C.", "Trudeau", "Carney", "CBC", "Bank of Canada",
  "Loonie",
]

[[topics]]
label = "Entertainment"
keywords = [
  "movie", "film", "actor", "actress", "Hollywood", "Netflix", "HBO", "Disney",
  "Oscar", "Grammy", "Emmy", "show", "series", "trailer",
  "album", "song", "single", "artist", "band", "concert", "tour", "music",
  "EP", "soundtrack",
]
"#;

pub const DEFAULT_COLORSCHEMES_TOML: &str = r##"# Color schemes for glint's chrome (borders, titles) and a handful of
# semantic text roles. Pick the active scheme via `[global] theme = "..."` in
# config.toml — or live-switch with `:scheme <name>` from the command bar.
# Per-widget overrides go in a `[colors]` block inside each widget's TOML
# (clock.toml, stocks.toml, …).
#
# StyleSpec format — either a shorthand string ("light_cyan", "#7dd3fc") that
# sets the foreground only, or a table with fg / bg / modifiers:
#
#   border.focused = "light_cyan"
#   border.focused = { fg = "light_cyan", bg = "default", modifiers = ["bold"] }
#
# NOTE: write `border.focused` (unquoted) so TOML sees the dot as a nested
# table separator. Writing `"border.focused"` produces a literal flat key
# that the deserializer can't see — your override would parse without error
# but silently never apply.
#
# Recognized modifiers: bold, dim, italic, underline, slow_blink, rapid_blink,
# reversed, hidden, crossed_out. "default" / "reset" / "none" means "inherit
# from the terminal" — useful for `bg` and `border.unfocused` to let your
# terminal theme show through.
#
# Roles glint reads:
#   border.focused    — widget border when the cell is focused (Tab cycles)
#   border.unfocused  — widget border on inactive cells
#   widget_title      — bold title text rendered in the border
#   text.plain        — default body text (the regular off-white prose)
#   text.brilliant    — emphasized body text (bold/bright)
#   text.dim          — annotation text: bottom hint rows, "all day" labels,
#                       graph axis labels, "(no stats)" placeholders, the
#                       gray separator dividers
#   text.selected     — selected tab, active period toggle, [Today] pill
#   text.focused      — focused entity highlight (cyan company name in stocks,
#                       focused article title in news, local time in clock)
#   text.shortcut     — single highlighted letter in each widget title that
#                       indicates the Shift+<letter> focus shortcut
#                       (e.g. red C in Clock = Shift+C focuses the clock)
#
# Missing roles silently fall back to glint's built-in defaults below, so a
# scheme can override one field and leave the rest alone.

# IMPORTANT: dotted keys (border.focused, text.plain, …) must NOT be quoted.
# In TOML, `"border.focused"` is a single literal key, while `border.focused`
# (unquoted) creates the nested `[border] focused` structure glint expects.
# Quoted keys parse cleanly but never reach the deserializer, so they look
# like silent no-ops at runtime.

# ── Default ─────────────────────────────────────────────────────────────────
# Matches the original glint palette.
[schemes.default]
border.focused   = { fg = "light_cyan",   modifiers = ["bold"] }
border.unfocused = "default"
widget_title     = { modifiers = ["bold"] }
text.plain       = "default"
text.brilliant   = { modifiers = ["bold"] }
text.dim         = { modifiers = ["dim"] }
text.selected    = { fg = "light_yellow", modifiers = ["bold"] }
text.focused     = { fg = "light_cyan",   modifiers = ["bold"] }
text.shortcut    = { fg = "light_red",    modifiers = ["bold"] }

# ── Chalktone ───────────────────────────────────────────────────────────────
# Soft, dusty pastels — chalk on a slate blackboard. Derivation of
# https://github.com/daneofmanythings/chalktone.nvim
[schemes.chalktone]
border.focused   = { fg = "#dabb87", modifiers = ["bold"] }
border.unfocused = "#5a625e"
widget_title     = { fg = "#e6dcc6", modifiers = ["bold"] }
text.plain       = { fg = "#cdc4ad" }
text.brilliant   = { fg = "#e6dcc6", modifiers = ["bold"] }
text.dim         = { fg = "#6f7570" }
text.selected    = { fg = "#c19a9a", modifiers = ["bold"] }
text.focused     = { fg = "#7eafa3", modifiers = ["bold"] }
text.shortcut    = { fg = "#c25450", modifiers = ["bold"] }

# ── Gruvbox ─────────────────────────────────────────────────────────────────
# Warm retro palette, dark medium contrast — derivation of
# https://github.com/ellisonleao/gruvbox.nvim
[schemes.gruvbox]
border.focused   = { fg = "#fabd2f", modifiers = ["bold"] }
border.unfocused = "#3c3836"
widget_title     = { fg = "#fbf1c7", modifiers = ["bold"] }
text.plain       = { fg = "#d5c4a1" }
text.brilliant   = { fg = "#ebdbb2", modifiers = ["bold"] }
text.dim         = { fg = "#7c6f64" }
text.selected    = { fg = "#fe8019", modifiers = ["bold"] }
text.focused     = { fg = "#8ec07c", modifiers = ["bold"] }
text.shortcut    = { fg = "#fb4934", modifiers = ["bold"] }

# ── Gruvbox Dark (legacy) ───────────────────────────────────────────────────
# Kept under its original name so existing configs keep working. Subtly
# different from `gruvbox` above (cooler aqua focus, deeper unfocused border).
[schemes.gruvbox_dark]
border.focused   = { fg = "#fabd2f", modifiers = ["bold"] }
border.unfocused = "#504945"
widget_title     = { fg = "#ebdbb2", modifiers = ["bold"] }
text.plain       = { fg = "#bdae93" }
text.brilliant   = { fg = "#fbf1c7", modifiers = ["bold"] }
text.dim         = { fg = "#665c54" }
text.selected    = { fg = "#fe8019", modifiers = ["bold"] }
text.focused     = { fg = "#83a598", modifiers = ["bold"] }
text.shortcut    = { fg = "#cc241d", modifiers = ["bold"] }

# ── Nord ─────────────────────────────────────────────────────────────────────
# Arctic, north-bluish palette — derivation of
# https://github.com/kunzaatko/nord.nvim (which mirrors the canonical
# https://www.nordtheme.com/ palette).
[schemes.nord]
border.focused   = { fg = "#88c0d0", modifiers = ["bold"] }
border.unfocused = "#3b4252"
widget_title     = { fg = "#eceff4", modifiers = ["bold"] }
text.plain       = { fg = "#d8dee9" }
text.brilliant   = { fg = "#eceff4", modifiers = ["bold"] }
text.dim         = { fg = "#616e88" }
text.selected    = { fg = "#ebcb8b", modifiers = ["bold"] }
text.focused     = { fg = "#88c0d0", modifiers = ["bold"] }
text.shortcut    = { fg = "#bf616a", modifiers = ["bold"] }

# ── Bluloco ─────────────────────────────────────────────────────────────────
# Modern dark with a signature cobalt-blue accent — derivation of
# https://github.com/uloco/bluloco.nvim
[schemes.bluloco]
border.focused   = { fg = "#4090f7", modifiers = ["bold"] }
border.unfocused = "#3e4452"
widget_title     = { fg = "#c8ccd4", modifiers = ["bold"] }
text.plain       = { fg = "#abb2bf" }
text.brilliant   = { fg = "#c8ccd4", modifiers = ["bold"] }
text.dim         = { fg = "#5c6370" }
text.selected    = { fg = "#f9c859", modifiers = ["bold"] }
text.focused     = { fg = "#4090f7", modifiers = ["bold"] }
text.shortcut    = { fg = "#ff6480", modifiers = ["bold"] }

# ── Miasma ──────────────────────────────────────────────────────────────────
# Horror-tinged, earthy decay — derivation of
# https://github.com/xero/miasma.nvim
[schemes.miasma]
border.focused   = { fg = "#b8823b", modifiers = ["bold"] }
border.unfocused = "#3a3a3a"
widget_title     = { fg = "#c9c0a8", modifiers = ["bold"] }
text.plain       = { fg = "#a89880" }
text.brilliant   = { fg = "#c9c0a8", modifiers = ["bold"] }
text.dim         = { fg = "#5c5347" }
text.selected    = { fg = "#c25450", modifiers = ["bold"] }
text.focused     = { fg = "#78824b", modifiers = ["bold"] }
text.shortcut    = { fg = "#a13438", modifiers = ["bold"] }
"##;

pub const DEFAULT_LLM_TOML: &str = r#"# Master switch. If false, every LLM-backed feature falls back to its
# structured-only counterpart (keyword filtering, raw RSS summaries, …).
enabled = true

# ── Provider ─────────────────────────────────────────────────────────────────
[provider]
name = "anthropic"
model = "claude-sonnet-4-6"
api_base = "https://api.anthropic.com"
max_tokens = 512

# ── Budget / cache ───────────────────────────────────────────────────────────
[limits]
max_requests_per_minute = 20
cache_capacity = 1024

# ── Per-feature toggles ──────────────────────────────────────────────────────
# Each defaults to a sensible value; flip off if you want to avoid LLM calls
# for a specific feature.
[features]
news_summarize = true
news_classify = false
stock_disambiguate = false
"#;

pub const DEFAULT_ANTHROPIC_KEY_TEMPLATE: &str = r#"# Anthropic API key. Get one at https://console.anthropic.com/.
# Leave api_key blank or unset to keep LLM features disabled.
api_key = "REPLACE_WITH_YOUR_KEY"
"#;

pub const DEFAULT_MICROSOFT_CLIENT_TEMPLATE: &str = r#"# Microsoft OAuth client config for Outlook calendar access.
#
# One-time setup:
#   1. Go to https://portal.azure.com/ → Microsoft Entra ID → App registrations
#   2. New registration. Name it "glint" (or anything). For "Supported account
#      types" pick "Accounts in any organizational directory and personal
#      Microsoft accounts". Leave Redirect URI blank for now and click Register.
#   3. On the new app's overview page, copy the "Application (client) ID" UUID
#      into client_id below.
#   4. Sidebar → Authentication → "Add a platform" → "Mobile and desktop
#      applications" → check the "http://localhost" loopback option. Save.
#   5. Sidebar → API permissions → "Add a permission" → Microsoft Graph →
#      Delegated permissions → check `Calendars.Read` → Add.
#   6. Back here, save this file, then run:  glint --auth outlook
#
# `tenant` defaults to "common" which accepts both personal and work/school
# accounts. Set it to a specific tenant UUID if your org requires it.
client_id = "REPLACE_WITH_AZURE_APP_CLIENT_ID"
tenant = "common"
"#;

pub const DEFAULT_CALDAV_TEMPLATE: &str = r#"# CalDAV calendar credentials.
#
# Apple iCloud: generate an app-specific password at https://appleid.apple.com
# (Sign-In and Security → App-Specific Passwords). Your Apple ID is the
# `username`; the generated password (looks like `abcd-efgh-ijkl-mnop`) is
# `app_password`. Server URL stays as the iCloud default below.
#
# Other CalDAV servers (Fastmail, Nextcloud, Synology, etc.) use the same
# fields — just point `server` at the provider's CalDAV root.
server = "https://caldav.icloud.com"
username = "your.email@icloud.com"
app_password = "REPLACE_WITH_APP_SPECIFIC_PASSWORD"
"#;

pub const DEFAULT_STOCKS_TOML: &str = r#"# ── Tickers ──────────────────────────────────────────────────────────────────
# Major indices listed at the top of the ticker list. Use Yahoo Finance
# symbols: ^DJI (Dow Jones), ^GSPC (S&P 500), ^IXIC (Nasdaq Composite).
indices = ["^DJI", "^GSPC", "^IXIC"]

# Your watchlist. Add or remove tickers freely.
watchlist = ["AAPL", "MSFT", "GOOGL", "NVDA", "TSLA"]

# ── Refresh ──────────────────────────────────────────────────────────────────
# Poll cadence (seconds, floor 15). Yahoo's chart endpoint refreshes every
# minute or so, so under 60s is overkill.
poll_interval_secs = 60

# ── Display ──────────────────────────────────────────────────────────────────
# Initial display mode for the change column. Cycle while focused with c
# (or pick directly with % / $). One of "percent" or "dollar".
default_display_mode = "percent"

# Initial graph period: "1d", "1w", "1m", "6m", "ytd", "1y", "3y", "5y",
# "10y". When focused, press 1..9 (or click a toggle / ‹›) to switch.
default_period = "1d"

# When true, horizontal mouse scroll cycles the period toggles. Disabled
# by default because trackpad sideways gestures often fire accidentally.
horizontal_scroll_period = false

# ── Jump (open ticker in browser) ────────────────────────────────────────────
# Pressing `j` on a selected ticker opens this URL. `{ticker}` is replaced
# with the URL-encoded symbol. Leave commented out to make `j` a no-op.
#
# Examples:
#   jump_url_template = "https://www.marketwatch.com/investing/stock/{ticker}"
#   jump_url_template = "https://www.google.com/finance/quote/{ticker}"
#   jump_url_template = "https://finance.yahoo.com/quote/{ticker}"
#   jump_url_template = "https://www.barrons.com/market-data/stocks/{ticker}"
# jump_url_template = "https://www.marketwatch.com/investing/stock/{ticker}"
"#;

pub const DEFAULT_CALENDAR_TOML: &str = r#"# Default view: "day", "week", or "month".
default_view = "day"
poll_interval_secs = 60

# Provider: "local" (use the [[events]] block below), "google" (run
# `glint --auth google` first; calendars listed in `calendar_ids`),
# "outlook" (Microsoft 365 / outlook.com — run `glint --auth outlook`
# after filling in microsoft_oauth_client.toml), or "caldav" (Apple
# iCloud / Fastmail / Nextcloud / Synology — fills in caldav.toml).
provider = "local"

# Multi-provider mode: merge events from two or more backends into one
# timeline. When [[providers]] entries exist, they take priority over the
# singular `provider` field above. Cell title shows "google+outlook" etc.
#
# [[providers]]
# kind = "google"
# calendar_ids = ["primary"]
#
# [[providers]]
# kind = "outlook"
# calendar_ids = []          # empty = the account's default calendar
#
# [[providers]]
# kind = "caldav"
# calendar_ids = []          # empty = auto-discover every iCloud calendar

# Google-only: which calendars to fetch. Use "primary" for your main one.
# calendar_ids = ["primary", "team@group.calendar.google.com"]

# CalDAV-only: optional explicit calendar URLs. Leave empty to auto-discover
# every calendar your account has access to.
# [caldav]
# calendars = []

# Example events. Replace these with your own — timed events use RFC3339
# timestamps with a timezone offset; all-day events use bare YYYY-MM-DD.

[[events]]
title = "Team standup"
start = "2026-05-20T09:30:00-07:00"
end = "2026-05-20T10:00:00-07:00"
calendar = "work"
location = "Zoom"

[[events]]
title = "Coffee with Sara"
start = "2026-05-20T15:00:00-07:00"
end = "2026-05-20T16:00:00-07:00"
calendar = "personal"

[[events]]
title = "Project review"
start = "2026-05-21T13:00:00-07:00"
end = "2026-05-21T14:30:00-07:00"
calendar = "work"

[[events]]
title = "Conference"
start = "2026-05-23"
end = "2026-05-24"
all_day = true
calendar = "personal"
"#;

/// Create `~/.config/glint/` and seed the default config files if they do not
/// already exist. Returns the path of the main `config.toml`.
pub fn init_default_config() -> Result<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config directory at {}", dir.display()))?;

    let main = dir.join("config.toml");
    seed(&main, DEFAULT_CONFIG_TOML)?;
    seed(&dir.join("clock.toml"), DEFAULT_CLOCK_TOML)?;
    seed(&dir.join("weather.toml"), DEFAULT_WEATHER_TOML)?;
    seed(&dir.join("calendar.toml"), DEFAULT_CALENDAR_TOML)?;
    seed(&dir.join("news.toml"), DEFAULT_NEWS_TOML)?;
    seed(&dir.join("stocks.toml"), DEFAULT_STOCKS_TOML)?;
    seed(&dir.join("llm.toml"), DEFAULT_LLM_TOML)?;
    seed(&dir.join("colorschemes.toml"), DEFAULT_COLORSCHEMES_TOML)?;

    // Credentials live in their own subdirectory (created with 0700) so they
    // can be locked down with one chmod.
    let credentials = crate::auth::credentials_dir()?;
    seed_credentials(&credentials.join("anthropic_key.toml"), DEFAULT_ANTHROPIC_KEY_TEMPLATE)?;
    seed_credentials(&credentials.join("caldav.toml"), DEFAULT_CALDAV_TEMPLATE)?;
    seed_credentials(
        &credentials.join("microsoft_oauth_client.toml"),
        DEFAULT_MICROSOFT_CLIENT_TEMPLATE,
    )?;
    Ok(main)
}

fn seed_credentials(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write credentials template at {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!(path = %path.display(), "wrote credentials template");
    Ok(())
}

fn seed(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        tracing::info!(path = %path.display(), "config file already exists, leaving in place");
        return Ok(());
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write default config to {}", path.display()))?;
    tracing::info!(path = %path.display(), "wrote default config");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG_TOML).expect("default config should parse");
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.layout.cells.len(), 5);
        assert_eq!(cfg.global.command_key, ":");
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let cfg: Config = toml::from_str("").expect("empty config should parse");
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.layout.cells.len(), 5);
    }

    #[test]
    fn default_colorschemes_seed_parses_and_has_default_scheme() {
        let file: crate::theme::ColorSchemesFile =
            toml::from_str(DEFAULT_COLORSCHEMES_TOML).expect("colorschemes seed should parse");
        assert!(
            file.schemes.contains_key("default"),
            "default scheme must exist so the unmodified config.toml resolves"
        );
        for expected in [
            "chalktone",
            "gruvbox",
            "gruvbox_dark",
            "nord",
            "bluloco",
            "miasma",
        ] {
            assert!(
                file.schemes.contains_key(expected),
                "expected scheme {expected:?} in seed"
            );
        }
    }

    #[test]
    fn seeded_schemes_actually_populate_roles_not_just_widget_title() {
        // Catches the quoted-dotted-key bug at the source: if any future
        // edit reverts to `"border.focused"`, this asserts that the
        // override is missing.
        let file: crate::theme::ColorSchemesFile =
            toml::from_str(DEFAULT_COLORSCHEMES_TOML).expect("seed parses");
        for (name, scheme) in &file.schemes {
            assert!(
                scheme.border.focused.is_some(),
                "scheme {name:?} should set border.focused (use unquoted dotted keys)"
            );
            assert!(
                scheme.text.focused.is_some(),
                "scheme {name:?} should set text.focused (use unquoted dotted keys)"
            );
        }
    }

    #[test]
    fn default_widget_seed_files_parse() {
        let _: crate::widgets::clock::ClockConfig =
            toml::from_str(DEFAULT_CLOCK_TOML).expect("clock seed should parse");
        let _: crate::widgets::weather::WeatherConfig =
            toml::from_str(DEFAULT_WEATHER_TOML).expect("weather seed should parse");
        let cal: crate::widgets::calendar::CalendarConfig =
            toml::from_str(DEFAULT_CALENDAR_TOML).expect("calendar seed should parse");
        assert!(!cal.events.is_empty(), "calendar seed should ship example events");
        let news: crate::widgets::news::NewsConfig =
            toml::from_str(DEFAULT_NEWS_TOML).expect("news seed should parse");
        assert!(!news.feeds.is_empty(), "news seed should ship example feeds");
        let llm: crate::llm::LlmConfig =
            toml::from_str(DEFAULT_LLM_TOML).expect("llm seed should parse");
        assert!(llm.enabled);
        assert_eq!(llm.provider.name, "anthropic");
        let stocks: crate::widgets::stocks::StocksConfig =
            toml::from_str(DEFAULT_STOCKS_TOML).expect("stocks seed should parse");
        assert!(!stocks.indices.is_empty());
        assert!(!stocks.watchlist.is_empty());
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let cfg = load(Some(Path::new("/nonexistent/glint/config.toml")))
            .expect("missing file should not error");
        assert_eq!(cfg.version, 1);
    }
}
