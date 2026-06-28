# Multi-account support — design spec

Status: **implemented in 0.3.0**

User-facing setup lives in INSTRUCTIONS.md → *Multiple calendar
accounts*; this doc is the design rationale.

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
  `src/auth/registry.rs:120`).
- **Source identity collapses to the provider kind.** Events carry
  `source: "outlook"` (`src/widgets/calendar/outlook.rs:221`); the
  color resolver keys on `(source, calendar)`
  (`src/widgets/calendar/colors.rs:63,74`) and the title label joins
  per-kind `&'static str`s (`src/widgets/calendar/wiring.rs:77`). Two
  accounts of one kind collide.

So this is a **platform-layer change** (introduce an account label in
the credential layer) with a thin calendar-facing surface. It composes
with the post-v0.2 `CredentialBackend` trait work: the backend lookup
key becomes `(provider, account, file_kind)` instead of `(provider,
file_kind)`.

Pre-launch note: there is no shipped on-disk state to preserve, so we
can change the credential layout outright. The one concession is a
small read fallback for the default account — `load_account("default")`
reads the legacy unsuffixed `…_oauth_token.toml` when the account-scoped
file is absent — so existing source builds (and the maintainer's own
dev tokens) don't need a manual re-auth. The next refresh writes the new
name, self-migrating.

## Scope (decided)

- **Calendar only.** Multiple same-provider accounts are supported for
  the Calendar widget. The Email widget stays single-account per widget
  instance and is an explicit non-goal here (see *Non-goals*).
- **Wizard stays single-account per provider.** The setup wizard
  manages exactly one Google and one Outlook account (the `"default"`
  account), as it does today. It is *not* extended to express two
  accounts of the same kind.
- **Extra accounts are manual.** A second account is added by
  hand-editing `calendar.toml` (a second `[[providers]]` block with an
  `account` label) and running `glint --auth microsoft:<label>` to mint
  its token (the auth provider is `microsoft`; the calendar `kind` is
  `outlook`).

The catch this scope creates: the wizard and the hand-edited config
share the same `calendar.toml`, and the wizard currently **rebuilds**
the `[[providers]]` list from a deduped set of kinds. Making manual
multi-account survive a wizard re-run is therefore a required piece of
work, not optional — see Phase 4.

## Phased plan

### Phase 1 — Account label in the token layer (platform)

Introduce an account label (default `"default"`) as a first-class
dimension of token storage, for **Microsoft and Google** (both are
calendar providers; Google is also Gmail — see Phase 1 note).

- Token filenames become account-scoped:
  `microsoft_oauth_token.<account>.toml` (default →
  `microsoft_oauth_token.default.toml`). One uniform scheme, no
  per-account-subdir variant.
- The **client config stays a single shared file**
  (`microsoft_oauth_client.toml`). One Azure app registration serves
  many user logins via `tenant = "common"`; only the *token* is
  per-account.
- Add `MicrosoftToken::load_account(label)` / `save_account(label)`.
  **Keep the existing no-arg `load()` / `save()` as thin wrappers that
  pass `"default"`** so non-calendar callers — the Email widget, the
  `fetch_*_folders` post-auth hooks — compile and behave exactly as
  before. This is what keeps Email out of scope.
- Mirror the same shape for `GoogleToken`.

### Phase 2 — Auth CLI account labels

The wizard only ever auths the `"default"` account, but a manual second
account needs a way to obtain its token, so the CLI must understand
labels:

- Parse `provider:account` in `run_auth` (`src/main.rs:218`) — `glint
  --auth microsoft:work`; bare `microsoft` ⇒ `default`. `registry::find`
  still resolves the provider; the label is threaded into the save.
- Prefer threading the label as **data**, not a new trait-method
  parameter: have the provider's `run` flow capture the label (or take
  a small `AuthContext { account }`) so the `AuthFlow` surface and every
  provider arm don't all churn.

### Phase 3 — Calendar config + wiring (the `account` label *is* the source)

Rather than add a parallel `account` dimension everywhere, fold it into
the identity the calendar already has — the provider entry's `source`
label. This keeps color keys a 2-tuple and adds no field to `Event`.

- Add an optional `account` to `ProviderEntry`
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

- The entry's **`source` is the provider kind for the default account
  and `kind/account` for a named one**. So a default Outlook entry still
  has `source = "outlook"` (no config churn for the common case); a
  labelled one has `source = "outlook/work"`. Namespacing by kind keeps
  each account grouped under its provider and means a Google account and
  an Outlook account that share a label (`work`) don't collide. `/` (not
  `:`) is used because `:` already separates source from calendar in
  `calendar_colors` keys.
- `colors.rs` is **unchanged in shape**: keys stay `(source, calendar)`
  / `"source:calendar_id"`, split on the first `:`. `"outlook:primary"`
  still addresses the default account; `"outlook/work:primary"` addresses
  the labelled one. No `Event.account`, no 3-tuple, no key-parser change,
  and the existing source-disambiguation test (`tests.rs`) doesn't grow
  an axis.
- `source_label` in `wiring.rs:35,77` becomes an owned `String`
  (today it's `&'static str`); the title row then reads e.g.
  `outlook+work` instead of two identical `outlook`s — a readability
  win.
- `build_outlook_entry` / `build_google_entry` (`wiring.rs:121,134`)
  take the account, load that account's token, and the refresh path
  (`outlook.rs:55`) saves back to the same account file.
- `CompositeProvider` already merges N providers (`wiring.rs:167`), so
  two Outlook entries "just work" once each carries a distinct token and
  source.

### Phase 4 — Make the wizard round-trip preserve manual accounts (required)

The wizard's `calendar.toml` round-trip in
`src/widgets/calendar/config.rs` is hard-keyed by canonical kind and
will destroy a hand-added second account on the next save:

- `load_calendar_from_toml` (`config.rs:285`) dedupes providers to a
  `Vec<String>` of kinds — the toggle UI only needs "is this kind
  present", so **this stays as-is**.
- `existing_provider_blocks_by_kind` (`config.rs:354`) stores one block
  per kind in a `HashMap<kind, block>` (second same-kind entry
  overwrites) and re-emits only `kind` + `calendar_ids` (drops
  `account`). **Must change** to preserve *every* block of a kind, with
  full content.
- `render_calendar_toml` (`config.rs:335`) strips all `[[providers]]`
  and rebuilds one block per selected kind. **Must change** to a
  preserve-don't-rebuild model.

Target behaviour: the wizard toggle still works per kind, but on save it
**preserves every existing `[[providers]]` block whose kind is still
ticked, verbatim** (including `account` and any other keys), drops
blocks whose kind was unticked, and appends a single default block only
for a newly-ticked kind that has none. Net effect: toggling Google off
removes *all* Google blocks; leaving Outlook ticked keeps *both* the
default and `work` blocks untouched. Add a round-trip test that a
two-account file survives a wizard save unchanged.

## What the setup wizard does / doesn't do

For the record, so the boundary is explicit:

- The calendar `sources` field is a `MultiChoice`
  (`config.rs:194`) over `["google","outlook","caldav","local"]`, and
  `WizardValue::MultiChoice` is a `Vec<String>` deduped by value
  (`src/wizard/descriptor.rs:221`) — it *cannot* represent
  `["google","google"]`, and we are not changing that.
- The OAuth setup page (`src/wizard/pages/oauth_setup.rs`) is
  provider-keyed: `PageAction::RunAuth(provider)` has no account label,
  and `run_oauth_for_provider` (`src/wizard/app.rs:364`) shells to
  `(provider.run)()`. The wizard authenticates only the `"default"`
  account of each provider.
- The Email widget's `provider` field is a single `Choice`
  (`src/widgets/email/mod.rs:1645`) — one account per widget instance,
  unchanged.

So: **the wizard can populate exactly one Google and one Outlook
account.** Additional calendar accounts are config-plus-CLI, and Phase 4
is what makes those edits durable against the wizard.

## Non-goals

- **Email multi-account.** Out of scope. Phase 1's no-arg token
  wrappers keep the Email widget and the `fetch_*_folders` hooks
  byte-for-byte on the `"default"` account. If Email multi-account is
  wanted later it's a separate convention sweep over `src/widgets/email/`
  plus the post-auth hooks (`src/auth/registry.rs:141`).
- **Wizard-driven multi-account.** Out of scope (see above). The
  `MultiChoice` field is keyed by kind by design.

## Open decisions

1. **`account` default semantics.** Token files default to the literal
   label `"default"` (`…_token.default.toml`), while an entry's *source*
   defaults to the *kind* string (`"outlook"`) so existing single-account
   configs and color keys are untouched. Confirm this split (storage key
   vs. display/color identity) reads clearly, or unify on one default.
2. **Auth threading shape** — capture the label in the `run` closure vs.
   a small `AuthContext { account }` struct. Lean `AuthContext` if more
   than the label ends up needed.

## Affected files (reference)

- `src/auth/microsoft/store.rs` — `load_account`/`save_account`,
  account-scoped filename, no-arg default wrappers
- `src/auth/google/store.rs` — mirror for Google
- `src/main.rs:218` — `run_auth` `provider:account` parsing
- `src/auth/registry.rs` — auth `run` flow threads the label (no-arg
  hooks unchanged)
- `src/widgets/calendar/config.rs` — `ProviderEntry.account`; **Phase 4**
  round-trip preservation (`existing_provider_blocks_by_kind`,
  `render_calendar_toml`)
- `src/widgets/calendar/wiring.rs` — per-account token load; owned
  `String` `source_label`
- `src/widgets/calendar/outlook.rs` / `google.rs` — `source` = account
  label; refresh saves to the account's token
- `src/widgets/calendar/colors.rs` — unchanged in shape; `(source,
  calendar)` now carries the account via `source`
