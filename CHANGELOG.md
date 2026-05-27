# Changelog

All notable changes to glint are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions track
the `Cargo.toml` `version` field.

## [0.2.0] — unreleased

This release is a structural refactor of the dashboard into a plugin-style
platform. The widget surface, cache layer, and OAuth registry are now the
documented seams for community contributions; adding a new widget no longer
requires editing core files (the wizard refactor is pending — see below).

### Added

- **Widget registry** (`src/widgets/registry.rs`). Every widget exports
  `pub const KIND` and `pub fn build(&WidgetCtx) -> Box<dyn Widget>`; a
  single `WidgetDescriptor` entry plugs it into the dashboard.
- **`WidgetCtx`** bundles construction-time dependencies (theme, optional
  LLM provider, scoped cache). New shared deps land here without breaking
  every widget's constructor.
- **Auth provider registry** (`src/auth/registry.rs`). `--auth <name>`
  looks up by name; widgets declare `AuthRequirement` on their descriptor.
- **Persistent cache** (`src/cache/mod.rs`). JSON + bytes APIs under
  `~/.cache/glint/<kind>/<instance>/<key>.{json,bin}`; atomic writes; the
  five fetching widgets (news, stocks, calendar, email, weather) seed on
  construction and refresh in background, so the first frame paints last
  session's data instead of an empty grid.
- **LLM summary persistence**. News + email cache their LLM-generated
  summaries keyed by sha256(url) / sha256(message-id), so a repeat
  summarisation across restarts costs zero API calls.
- **Gallery thumb cache**. Downscaled JPEGs persist under
  `~/.cache/glint/gallery/<instance>/`; mtime-based invalidation against
  the source file. Phone-camera startup costs drop from ~2-3s of decode
  work to a few hundred ms after the first run.
- **Gallery glob expansion + periodic rescan**. `images` entries accept
  literal paths, `<dir>/*`, and `<dir>/*.ext`. A background loader
  re-walks glob patterns every `rescan_interval_secs` (default 300s, min
  30s, 0 disables) and reconciles the slide list — new images appear in
  rotation without restart.
- **Per-widget cargo features** (`widget-clock`, ..., `widgets-all`).
  Heavy widget dependencies (Google OAuth, Microsoft Graph,
  `ratatui-image`) only compile when their feature is enabled. Default
  builds include all eight stock widgets; downstream packagers can ship
  trimmed binaries via `--no-default-features`.
- **`--clear-cache [<target>]`** CLI flag with `[y/N]` confirmation. Pass
  a widget kind (`news`) or `kind@instance` (`news@home`) to scope the
  clear. `--clear-cache-forced` skips the prompt.

### Changed

- **LLM toggles moved into per-widget TOMLs.** `summarize_with_llm` lives
  in `news.toml` / `email.toml` instead of a central `llm.toml`
  `[features]` block. The LLM layer is now widget-agnostic.
- **`AGENTS.md` replaces `CLAUDE.md`** as the contributor onboarding doc
  — vendor-neutral naming that other agent toolchains recognise.
- **Default config layout reflects the registry.** `app.rs::register_widget`
  collapsed from a 9-arm match into a single `registry::build_for` call.
- **`Event::Resize` no longer carries dimensions.** Ratatui recomputes
  layout on next draw; the new size doesn't need to ride the event.

### Removed

- **Single-provider calendar config form.** `provider = "..."` and the
  top-level `calendar_ids` field are gone; use `[[providers]]` blocks
  (the docs in `DEFAULT_CALENDAR_TOML` already showed this as the
  recommended form). No back-compat is retained — glint is pre-release.
- **Stocks `display_mode` and clock `tz` / integer-form `hour_format`
  aliases**. The canonical names (`default_display_mode`, `timezone`,
  string `"24h"`) are the only accepted forms now.
- **`DataProvider` trait** (`src/providers/`). Implemented but never
  dispatched through; widgets call their concrete providers directly.
- **Dead trait methods**: `LlmProvider::{name, health_check}`,
  `CalendarProvider::name`, `NewsProvider::name`, `LlmResponse::input_tokens`,
  `LlmResponse::output_tokens`. Phase-referring `#[allow(dead_code)]`
  annotations across the tree are gone.
- **`--auth outlook`** alias for `--auth microsoft`. Use the registry name.

### Deferred

- **Wizard refactor.** `src/wizard/mod.rs` is still the only file that
  knows widget kinds by name; declarative per-widget setup steps land in
  a follow-up.
