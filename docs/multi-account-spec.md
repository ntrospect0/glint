# Multi-account support — design spec

Status: **draft / not yet implemented**

## Motivation

The Calendar widget can already merge multiple *providers* (Google +
Outlook + CalDAV) and multiple *calendars within one account* (via
`calendar_ids`), but it cannot hold two separate accounts of the *same*
provider — e.g. a work Outlook **and** a personal Outlook.

The blocker is not in the calendar widget. The "one account per
provider" assumption lives in the auth/credentials layer, and the
widgets inherit it:

- **Token storage keys by provider, not account.**
  `MicrosoftToken::load()` reads a fixed `microsoft_oauth_token.toml`
  (`src/auth/microsoft/store.rs:13,61`). Same shape for Google.
- **The auth CLI dispatches on a bare provider name.** `--auth
  microsoft` → `registry::find("microsoft")` → a `run` flow that saves
  to that one fixed file (`src/main.rs:218`,
  `src/auth/registry.rs:120-127`).
- **Event identity collapses to the provider string.** Events carry
  `source: "outlook"` (`src/widgets/calendar/outlook.rs:221`); the
  color resolver keys on `(source, calendar)`
  (`src/widgets/calendar/colors.rs:111`). Two accounts collide.
- **The email widget shares the same token.** `OutlookEmailProvider`
  and the `fetch_outlook_folders` post-auth hook
  (`src/auth/registry.rs:154`) load the same single token. Any
  token-storage change ripples there too.

So this is a **platform-layer change** (introduce account identity)
with a thin calendar-facing surface. It also composes with the
post-v0.2 `CredentialBackend` trait work: the backend lookup key
becomes `(provider, account, file_kind)` instead of `(provider,
file_kind)`.

Pre-launch note: there is no shipped on-disk state to preserve, so we
can change the credential layout outright — no migration shim, no
legacy fallback path.

## Phased plan

### Phase 1 — Account identity in the credential/token layer (platform)

Introduce an account label (default `"default"`) as a first-class
dimension of token storage.

- Token filenames become account-scoped — e.g.
  `credentials/tokens/microsoft.<account>.toml` (or flat:
  `microsoft_oauth_token.<account>.toml`; see open decision #2).
- The **client config stays a single shared file**
  (`microsoft_oauth_client.toml`). One Azure app registration serves
  many user logins via `tenant = "common"`; only the *token* is
  per-account.
- `MicrosoftToken::load(account)` / `save(account)` take the label.
  Mirror the same change for `GoogleToken` so the convention is
  uniform across Calendar + Gmail.

### Phase 2 — Auth CLI

- Parse `provider:account` in `run_auth` — `glint --auth
  microsoft:work` (bare `microsoft` ⇒ `default`). `registry::find`
  still resolves the provider; the account label is threaded into the
  `run` flow, which saves the token under that label.
- The `AuthFlow` signature gains an account parameter (or a small
  `AuthContext` struct).

### Phase 3 — Calendar config + wiring

- Add an optional field to `ProviderEntry`
  (`src/widgets/calendar/config.rs:128`):

  ```toml
  [[providers]]
  kind = "outlook"
  account = "work"        # omitted ⇒ "default"
  calendar_ids = []

  [[providers]]
  kind = "outlook"
  account = "personal"
  ```

- `build_outlook_entry` / `build_google_entry`
  (`src/widgets/calendar/wiring.rs:121,134`) take the account, load
  that account's token, and the provider's refresh path
  (`src/widgets/calendar/outlook.rs:51-59`) saves back to the same
  account file.
- `CompositeProvider` already merges N providers
  (`src/widgets/calendar/wiring.rs:167`), so two Outlook entries "just
  work" once each carries a distinct token.

### Phase 4 — Identity disambiguation (colors + labels)

Two Outlook accounts currently collapse to `source = "outlook"`, so
they share colors and `calendar_colors` keys. Two options (open
decision #3):

- **(a, recommended) Add `account: String` to `Event`** and key the
  color resolver on `(source, account, calendar)`. `calendar_colors`
  TOML keys gain an optional account segment
  (`"outlook:work:primary"`; a 2-part key still implies `default`).
  Cleanest model; touches `Event`, `resolve()`, and the key parser.
  The existing source-disambiguation test
  (`src/widgets/calendar/tests.rs:301`) extends to a third axis.
- **(b) Encode account into `source`** as `"outlook#work"`. Minimal
  ripple — `split_once(':')` on color keys still works and
  `source_label` stays a single string — but it's string-y and leaks
  the separator into config.

### Phase 5 — Email widget convention sweep

Once token storage is account-scoped, `OutlookEmailProvider` /
`GmailProvider` and the `fetch_*_folders` post-auth hooks
(`src/auth/registry.rs:141-178`) must also pass an account. Audit and
update in the same pass so the two widgets stay consistent.

## Open decisions

1. **Scope for the first cut** — Outlook calendar only (Phases 1–4,
   Microsoft token) vs the full uniform model across Google + Outlook
   and both widgets (all phases). The phases are layered, so a narrow
   cut is real; Phase 5 is the convention sweep usually worth doing in
   one go.
2. **On-disk token layout** — per-account subdir
   (`tokens/microsoft.work.toml`) vs flat suffix
   (`microsoft_oauth_token.work.toml`).
3. **Disambiguation model** — `Event.account` field (a) vs `source`
   suffix (b).
4. **Wizard** — recommend punting multi-account out of the setup
   wizard for v1 (its `MultiChoice` is keyed by `kind` and can't
   express two of the same kind). Default account via wizard; extra
   accounts via hand-edited `calendar.toml` + `glint --auth
   outlook:<label>`. Open: acceptable, or should the wizard handle it?

## Affected files (reference)

- `src/auth/microsoft/store.rs` — token load/save, filename constant
- `src/auth/google/store.rs` — mirror for Google
- `src/auth/registry.rs` — `AuthFlow` signature, post-auth hooks
- `src/main.rs` — `run_auth` provider:account parsing
- `src/widgets/calendar/config.rs` — `ProviderEntry.account`
- `src/widgets/calendar/wiring.rs` — per-account token load
- `src/widgets/calendar/outlook.rs` — refresh save path, `Event` source/account
- `src/widgets/calendar/colors.rs` — `(source, account, calendar)` keying
- `src/widgets/calendar/provider.rs` — `Event` struct (if adding `account`)
- `src/widgets/email/*` — convention sweep (Phase 5)
