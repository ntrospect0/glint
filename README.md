# glint

A fast, keyboard-driven terminal dashboard for stocks, calendar, weather,
news, and more. Written in Rust with [ratatui](https://ratatui.rs).

![glint widgets at a glance: clock, calendar, weather, news, stocks](docs/glint-spec.md)

---

## Features

- **Multi-widget layout** — grid of panes, mix and match
  (Clock · Calendar · Weather · News · Stocks · Resources · Gallery), each
  with its own TOML config.
- **Multi-instance** — run the same widget kind in several panes (e.g. two
  Stocks panes for different watchlists, two Clocks for home + office).
- **Theming** — bundled color schemes (default · chalktone · gruvbox ·
  tokyonight · rosepine · nord · bluloco · onedark · miasma); switch
  live with `:scheme nord`.
- **Per-widget focus shortcuts** — `Shift+C` / `Shift+W` / `Shift+N` / …
  jump straight to that widget. The shortcut letter is painted in the
  title.
- **Live config reload** — edit any widget TOML and the dashboard picks
  up the change without restart.
- **Inline images (Gallery)** — iTerm2, Kitty graphics, Sixel, or unicode
  half-blocks fallback. Pre-decoded on a background thread so startup
  isn't blocked.
- **Setup wizard** — `glint --setup` walks you through layout + widget
  configs interactively. First run launches it automatically.

---

## Install

### From source (recommended for now)

Requires a recent Rust toolchain (1.81+). Install via
[`rustup`](https://rustup.rs/) if you don't have one.

```sh
git clone <your-fork-url> glint
cd glint

# Per-user install (no sudo, installs to ~/.local/bin):
make install PREFIX=~/.local

# Or system-wide (typically needs sudo):
sudo make install
```

If `~/.local/bin` isn't on your `$PATH`, add this to `~/.zshrc` or
`~/.bashrc`:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Verify:

```sh
glint --version
```

### Other Makefile targets

| target | what it does |
|---|---|
| `make` / `make release` | release build at `target/release/glint` |
| `make build` | debug build (faster compile, slower runtime) |
| `make install` | release build + copy to `$(PREFIX)/bin/glint` |
| `make uninstall` | remove `$(PREFIX)/bin/glint` |
| `make test` | run the test suite |
| `make clean` | `cargo clean` |

### Updating

```sh
git pull
make install PREFIX=~/.local   # or sudo make install
```

---

## Quickstart

Launch with no config and glint drops you into the setup wizard
automatically:

```sh
glint
# → "No config detected … launching the setup wizard."
```

The wizard walks you through:

1. **Layout** — 1 to 8 panes with 1–3 recommended layouts each.
2. **Widget assignment** — pick which widget kind goes in each pane.
   Same kind can occupy multiple panes when you give each instance a
   distinct name (e.g. `home`, `office`).
3. **Per-widget configs** — timezone, location, RSS feeds, watchlist
   tickers, calendar providers, system-info refresh interval, gallery
   image paths.
4. **LLM key** (optional) — Anthropic API key for news summaries and
   stock disambiguation.

Re-run the wizard anytime with `glint --setup`. Each section has an
**Edit / Skip** gate; skipping leaves that TOML untouched (preserving
any hand-edited comments).

---

## Configuration

All config lives under `~/.config/glint/`:

| file | what it controls |
|---|---|
| `config.toml` | active color scheme, grid layout, widget cell placements |
| `colorschemes.toml` | named theme palettes (`default`, `nord`, `gruvbox`, …) |
| `clock.toml` | primary timezone, world clocks, big-digit gradient |
| `weather.toml` | location, units, IP geolocation fallback |
| `news.toml` | RSS feeds, topic filters, show-categorization toggle |
| `stocks.toml` | watchlist, indices, default period, jump URL |
| `calendar.toml` | Google / Outlook / CalDAV / Local providers |
| `resources.toml` | refresh interval, top-N processes, sort key |
| `gallery.toml` | image paths, rotation cadence |
| `llm.toml` | per-feature LLM toggles |
| `credentials/` | OAuth tokens + API keys (0600 perms) |

Most fields have sensible defaults; you only have to set the ones you
care about. Edit any file by hand or re-run the wizard.

### Multi-instance widgets

Cells in `config.toml` can reference widgets as `kind@instance`:

```toml
[[layout.cells]]
widget = "stocks@watchlist1"
col = 0
row = 2

[[layout.cells]]
widget = "stocks@watchlist2"
col = 1
row = 2
```

The first one reads `stocks.toml` (the implicit "main" instance), the
others read `stocks@watchlist1.toml` and `stocks@watchlist2.toml`.

---

## Keybindings

### Global

| key | action |
|---|---|
| `Tab` / `Shift+Tab` | cycle focused widget |
| `Shift+<letter>` | jump focus directly (red letter in title) |
| `click cell` | focus that widget |
| `:` | open the command bar |
| `:scheme <name>` | switch color scheme (persisted to `config.toml`) |
| `:news <terms>` | filter news by keyword |
| `:weather <city>` | retarget the weather widget |
| `:time <city>` | retarget the clock widget |
| `:stock <symbol>` | jump-lookup a ticker |
| `?` | toggle help overlay (scrollable) |
| `q` / `Ctrl+C` | quit |

### Common per-widget keys

| widget | keys |
|---|---|
| **Stocks** | `↑/↓` select ticker · `←/→` cycle graph period · `c` % ↔ $ · `Enter` open in browser · `1-9` jump period |
| **Calendar** | `d` / `w` / `m` day/week/month · `←/→` navigate · `t` today · `g` cycle digit gradient |
| **Weather** | `:weather <city>` retarget · `x` revert to default |
| **Clock** | `:time <city>` retarget · `x` revert to local · `g` cycle gradient |
| **News** | `↑/↓` select · `←/→` filter tabs · `e` expand · `Enter` open · `x` clear `:news` search |
| **Resources** | `m` toggle sort (CPU ↔ memory) · `r` force refresh |
| **Gallery** | `p` pause/resume · `n`/`N` step · `↑/↓` rotation interval ±1s |

Hit `?` while running for the full overlay with scheme list and current
shortcut assignments.

---

## Color schemes

Switch live:

```
:scheme nord
:scheme gruvbox
:scheme miasma
```

The choice persists to `[global] theme` in `config.toml`. Add your own
scheme by editing `~/.config/glint/colorschemes.toml`:

```toml
[schemes.my_scheme]
border.focused   = { fg = "#88c0d0", modifiers = ["bold"] }
border.unfocused = "#3b4252"
widget_title     = { fg = "#eceff4", modifiers = ["bold"] }
text.plain       = { fg = "#d8dee9" }
text.brilliant   = { fg = "#eceff4", modifiers = ["bold"] }
text.dim         = { fg = "#616e88" }
text.selected    = { fg = "#ebcb8b", modifiers = ["bold"] }
text.focused     = { fg = "#88c0d0", modifiers = ["bold"] }
text.shortcut    = { fg = "#bf616a", modifiers = ["bold"] }
```

Then `:scheme my_scheme`.

**Important**: dotted keys must be unquoted (`border.focused`, not
`"border.focused"`). Quoted dotted keys are literal flat keys and
silently fail to deserialize into the nested struct.

---

## Calendar providers

Out of the box `calendar.toml` shows the bundled example events. To
hook into real calendars:

- **Google**: `glint --auth google` opens a browser to grant access.
  List calendar IDs in `[[providers]]` under `kind = "google"`.
- **Outlook / Microsoft 365**: register an Azure app, write the client
  ID into `credentials/microsoft_oauth_client.toml`, run
  `glint --auth outlook`.
- **CalDAV (Apple iCloud, Fastmail, Nextcloud, …)**: generate an
  app-specific password, fill `credentials/caldav.toml`, set
  `kind = "caldav"`.

The setup wizard walks you through each of these with copy-pasteable
instructions.

---

## Troubleshooting

- **`glint` not found after install** — make sure `$(PREFIX)/bin` is on
  your `$PATH`. The Makefile prints the right export line at the end of
  `make install`.
- **Gallery shows chunky pixelated images** — your terminal doesn't
  support iTerm2 / Kitty / Sixel inline protocols, so glint fell back
  to unicode half-blocks. Switch to iTerm2 (macOS), WezTerm, Kitty, or
  enable sixel mode in your terminal.
- **Logs**: anything that goes wrong at runtime is written to
  `~/.config/glint/glint.log` — the TUI's alternate-screen mode means
  stderr/stdout would corrupt the display, so warnings/errors land in
  the log instead. `tail -f ~/.config/glint/glint.log` while debugging.
- **Reset to defaults**: delete (or move aside) the files in
  `~/.config/glint/` and re-run `glint --setup`.

---

## Development

```sh
cargo run                  # debug build, dashboard mode
cargo run -- --setup       # wizard
cargo test --quiet         # full test suite (~190 tests)
cargo clippy               # lints
cargo fmt                  # format
```

See `docs/glint-spec.md` for the original design spec and `AGENTS.md`
for the architecture overview targeted at contributors and AI assistants.

---

## License

MIT.
