# Profiles — functional & technical spec

Status: **implemented — 0.4.0**
Revised after an adversarial review and dogfooding: migration is
**opt-in and non-destructive** (a flat config is read in place; a stray
run can't wipe it), profile resolution has a hard set-once invariant,
clone is config-only, and OAuth client registrations are global-only.

> Version note: shipped as **0.4.0** — it touches the CLI surface, the
> on-disk layout, a one-time migration, and the setup wizard, so it's a minor
> release rather than a patch.

## Motivation

One person runs glint in several contexts — a focused **work** dashboard, a
stripped-down **travel** view, a **personal** layout — each wanting its own
layout, widget set, theme, and (post multi-account) its own calendar/mail
accounts. Today there is one config tree at `~/.config/glint/`, so switching
contexts means hand-editing or keeping copies.

**Profiles** let you launch a named, fully-configured view:

```sh
glint --profile work        # or: glint -p work
glint                       # the "default" profile
```

A profile is an isolated config tree. Resources that are *libraries* or
*installation identity* — the colorscheme palette and the OAuth app
registrations — are shared globally so you define/register them once.

## Concepts

Two tiers on one hierarchy:

- **Global layer** (`~/.config/glint/`, the root) — app-level resources
  shared across every profile: the colorscheme *definitions* and the OAuth
  *client registrations* (the Azure / Google app, not any account).
- **Per-profile layer** (`~/.config/glint/profiles/<name>/`) — persona data:
  layout, widget configs, the *selected* theme, account tokens, CalDAV +
  IMAP creds, LLM API keys, notes, runtime/wizard state, cache, logs.

```
~/.config/glint/                         ← GLOBAL layer (root)
├── colorschemes.toml                    ← theme library (shared, layerable)
├── credentials/                         ← global, app-level (0700)
│   ├── google_oauth_client.toml         ← app registration (shared; contains client_secret)
│   └── microsoft_oauth_client.toml
├── .profiles-migrated                   ← migration marker
└── profiles/
    ├── default/                         ← PER-PROFILE layer
    │   ├── config.toml                  ← layout + selected theme + globals
    │   ├── clock.toml  stocks.toml  calendar.toml  feeds@<instance>.toml  llm.toml
    │   ├── colorschemes.toml            ← OPTIONAL per-profile scheme overrides/additions
    │   ├── credentials/                 ← per-profile, account-level (0700)
    │   │   ├── google_oauth_token.<account>.toml
    │   │   ├── microsoft_oauth_token.<account>.toml
    │   │   ├── caldav.toml   imap.toml
    │   │   ├── anthropic_key.toml   openai_key.toml     ← LLM keys are per-profile
    │   ├── notes/<instance>/<id>.md
    │   ├── glint.log
    │   ├── .runtime_state.toml
    │   └── .wizard_state.toml
    └── work/  travel/  …                ← same shape as default/
```

Cache lives under `$XDG_CACHE_HOME/glint/profiles/<name>/`.

### Boundary table

| Element | Tier | Notes |
|---|---|---|
| Colorscheme **definitions** (`colorschemes.toml`) | **Global, layerable** | Define `ariel` once; a profile may add/override schemes by name. |
| OAuth **client registrations** (`*_oauth_client.toml`) | **Global only** | One app registration for the install; no per-profile override (rare need; deferred). Read from the root, never per-profile. |
| Layout, widget configs, **selected theme** (`config.toml`) | Per-profile | The dashboard itself. |
| Account **tokens**, CalDAV, **IMAP** creds | Per-profile | Persona identity; composes with multi-account labels. |
| **LLM API keys** (`anthropic_key.toml`, `openai_key.toml`) | Per-profile | Allows work-billed vs personal. |
| Notes, runtime state, wizard state | Per-profile | Persona content/state. |
| Cache, logs | Per-profile | Avoids cross-profile data bleed / interleaved logs. |

### Layered override semantics

Only colorschemes layer:

- **Colorschemes — merge by name.** Load the root `colorschemes.toml`, then
  overlay an optional `profiles/<name>/colorschemes.toml`: schemes union by
  name, profile definitions win on collision. A profile gets the full shared
  library plus any private schemes.

Everything else resolves from exactly one tier — **no fallback**: per-profile
files come from the profile dir only (a missing `work` token must never read
another profile's token); client registrations come from the global dir
only. This single-tier-per-file rule is deliberate: the earlier "client
shadow" design forced fallback logic at every credential call site and was
cut.

## Functional spec

### CLI surface

- `--profile <NAME>` / `-p <NAME>` — select the profile (`-p` is free today).
  Precedence: **`--profile` > `GLINT_PROFILE` env > `"default"`**.
- `--list-profiles` — print profiles under `profiles/`, marking default and
  active, then exit.
- Global to every mode — composes with `--setup`, `--auth`, `--init`,
  `--clear-cache`, and launch. The profile is resolved and set **first**,
  before anything reads or writes config (see *Startup ordering*).
- **Missing profile** on launch/auth: error, don't auto-create —
  `profile 'work' not found. Create it with: glint --profile work --setup`.
- **Name rules:** `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`; reject path separators,
  leading dash/dot. `default` is a normal, always-present, undeletable name.
  **Case-insensitive-filesystem guard:** macOS APFS/HFS+ fold case, so a
  create/rename whose lowercased form collides with an existing profile is
  rejected (don't claim case-sensitivity the FS won't honor).
- **`--config <FILE>`** is **mutually exclusive** with `--profile` and with a
  flat→profiles migration. It means "explicit single-file mode": load that
  `config.toml` and resolve all sibling files from *its* directory, flat.
  Mixing a flat `config.toml` with profile-rooted everything-else (the
  original half-resolving design) is disallowed.

### Default profile & first run

- No `--profile`/env → `default`, which **always exists** and **cannot be
  deleted**.
- First run (no config) seeds `profiles/default/` + the global layer, then
  the wizard for `default` — same UX, one dir deeper.
- `glint --profile X --setup` for non-existent `X` **creates** it (seed
  defaults) and edits it.

### Setup wizard → Profile Manager

- **Bare `glint --setup`** → the **Profile Manager**: lists profiles (default
  marked) and offers **Edit / Create / Clone / Rename / Delete**.
- **`glint --profile X --setup`** → edits `X` directly (creating it first if
  absent).
- **Clone = config-only.** Cloning copies layout/widget/theme config but
  **not** credentials/tokens — the clone re-authorizes per provider. (Copying
  tokens was cut: after a clone both copies share one refresh token, and the
  first refresh rotates it — Azure AD rotates every refresh, Google can too —
  silently logging out the sibling, especially when both profiles run at
  once.) Clone deep-copies `profiles/<src>/` minus its `credentials/`.
- **Rename** (not `default`): dir rename. **Delete** (not `default`):
  recursive remove of `profiles/<name>/` **and** its cache segment, with
  confirmation. Both **refuse if the target profile is running** (a pid
  lockfile in the profile dir, see *Lifecycle safety*).
- Wizard writes land in the **active profile** dir. The one page that writes
  a **global** resource is OAuth **client** capture (`*_oauth_client.toml` →
  root); it surfaces a one-line "this app registration is shared across all
  profiles" so the global effect isn't a surprise. The resulting **token**
  lands in the profile. `.wizard_state.toml` is per-profile.

### Authorize, per profile

`glint --profile work --auth microsoft:exchange` writes the token to
`profiles/work/credentials/microsoft_oauth_token.exchange.toml`, read through
the **global** `microsoft_oauth_client.toml`. Multi-account, nested under a
profile.

### Active-profile indicator

When the active profile ≠ `default`, surface its name in the running TUI
(e.g. a short `⟦work⟧` tag in a status corner) so context is unmistakable.
`default` shows nothing.

### Simultaneous profiles & lifecycle safety

- Running `glint -p work` and `glint -p travel` in two terminals is
  conflict-free for *launched* profiles: each resolves its profile once and
  all mutable per-profile state is isolated.
- **The global layer is shared, so concurrent global writes must be atomic**
  (see *Atomic writes*).
- **Lifecycle vs liveness:** a running profile writes a pid lockfile
  (`profiles/<name>/.lock`) on launch (removed on exit; stale locks detected
  via pid liveness). The Manager **refuses to rename or delete a running
  profile**, and refuses to delete the active one. This closes the
  split-brain where a Manager process deletes the dir out from under a live
  TUI.
- No live profile-switching inside one process — switching means relaunch.

## Migration (opt-in, non-destructive)

> **Why not automatic.** An earlier design migrated on first launch (copy →
> publish → *delete the flat originals*). Dogfooding proved that unsafe: when a
> pre-profiles **flat binary** and the new **profiles binary** share one
> config dir, the profiles binary relocates the flat config, the flat binary
> then sees an "empty" root, treats it as a first run, and **re-seeds
> defaults** over it — destroying the real config (and its tokens). Migration
> is therefore opt-in and never deletes.

**Flat layouts are read in place.** The default profile's `config_dir()`
falls back to the flat root when `profiles/default/` doesn't exist but a root
`config.toml` does (see *Resolution*). So a pre-profiles install keeps working
untouched, and a profiles binary can share the directory with an older flat
binary safely — neither moves anything.

**Opt-in migration — `glint --migrate-profiles`:**

1. Guard: bail if there's no root `config.toml`, or if `profiles/default/`
   already exists (nothing to do).
2. **Copy** the per-profile portion of the flat root into
   `profiles/.default.partial/`:
   - Per-profile files: `config.toml`, all root `*.toml` **except
     `colorschemes.toml`**, `.runtime_state.toml`, `.wizard_state.toml`,
     `glint.log`, `notes/`.
   - **credentials** with a **deny-list**: copy *everything* under
     `credentials/` **except `*_oauth_client.toml`**. `std::fs::copy`
     preserves the 0600 file mode; the new `credentials/` is created 0700.
3. **Leave at the root (global):** `colorschemes.toml`,
   `credentials/*_oauth_client.toml`.
4. **Atomically publish:** `rename("profiles/.default.partial", "profiles/default")`.
5. **Stop.** The flat originals are **left in place** — an older flat binary
   keeps working. The user removes the root `*.toml` themselves once fully
   switched.

**Properties:**

- **Non-destructive.** Nothing at the root is ever deleted, so an interrupted
  run leaves both the flat layout and (at worst) a discardable
  `.default.partial` staging dir; re-running redoes the copy.
- **No cross-binary hazard.** Because the flat files survive and are read in
  place until the user migrates, a stray run of the profiles binary against a
  flat install can't strand or wipe it.
- **Composes with multi-account.** The 0.3.0 legacy-token read fallback is
  copied into `profiles/default/credentials/` and keeps resolving there.
- `--config` mode is unaffected (explicit single-file mode).
- **Notes:** `~/.glint/notes` adoption into the profile is handled at the
  notes resolver (see below), independent of this copy.

## Technical design

### Resolution — chokepoint + set-once invariant

Split `config::config_dir()` (`src/config/mod.rs:99`):

```rust
pub fn glint_root() -> Result<PathBuf>;   // $XDG_CONFIG_HOME/glint | ~/.config/glint  (global layer)
pub fn config_dir() -> Result<PathBuf>;   // glint_root()?/profiles/<active>           (per-profile)
```

`config_dir()` has one **legacy flat-layout fallback**: for the *default*
profile, when `profiles/default/` doesn't exist but a root `config.toml` does,
it returns the **root** — so a pre-profiles flat install is read in place with
no migration (and stays interoperable with an older flat binary). Named
profiles always resolve to `profiles/<name>/`. Migration (opt-in) is what
later creates `profiles/default/`, after which the fallback no longer applies.

Active profile is a **set-once, read-only** process global — there is one
active profile per process and no live switching:

```rust
static ACTIVE_PROFILE: OnceLock<String> = OnceLock::new();

/// Read-only. NEVER initializes the lock (no get_or_init), so an early read
/// can't silently pin "default" and make a later set() a no-op.
pub fn active_profile() -> &'static str { ACTIVE_PROFILE.get().map(String::as_str).unwrap_or("default") }

/// Called exactly once in main(), before any config access. Panics on a
/// second set so an accidental earlier set is loud, not silent.
pub fn set_active_profile(name: String) { ACTIVE_PROFILE.set(name).expect("active profile set twice"); }
```

Invariant: **no `config_dir()` resolution before `set_active_profile`** in a
non-test build. Enforced by ordering (below) and a debug assertion that trips
if `active_profile()` is read while the lock is empty during startup. Tests
that need a non-default value set it explicitly; the config-touching tests
that mutate `XDG_CONFIG_HOME` are already `#[ignore]`d.

This keeps `config_dir()` zero-arg, so its downstream callers
(`config_path`, `load_widget_toml*`, `credentials::dir`,
`runtime_state::state_path`, `wizard::storage::state_path`, the watcher, the
logger, notes) need no signature churn — but each gets a test asserting it
resolves under the active profile (the "no churn" claim is *true* but must be
*verified*, not assumed).

### Startup ordering

`init_tracing()` currently runs at `main.rs:67`, **before** `Cli::parse()`
(`:68`) and already calls `config_dir()`. Reorder:

1. `Cli::parse()` → resolve profile (flag › env › default), validate the
   name.
2. `config::set_active_profile(name)`.
3. Run migration if the flat layout is present (acquire lock, stage, publish).
4. `init_tracing()` → now `profiles/<name>/glint.log`.
5. Dispatch.

### Credentials tiering (no fallback)

```rust
pub fn dir() -> Result<PathBuf>;          // profile creds:  config_dir()?/credentials   (0700)
pub fn global_dir() -> Result<PathBuf>;   // client regs:    glint_root()?/credentials    (0700)
```

- **Client files** (`*_oauth_client.toml`): `global_dir()` **only**.
- **Everything else** (tokens, CalDAV, IMAP, LLM keys): `dir()` **only**.

Because there's no fallback, the audit is simple but **must cover every call
site, not just `credentials::load`/`path`**. The review found sites that
build paths manually and would bypass tiering:

- `registry.rs:316` `needs_credential_capture` → `dir().join(spec.filename)`
  — for a **client** spec this must read `global_dir()`, else every
  non-default profile wrongly reports "client missing." Route client specs
  through `global_dir()`.
- `registry.rs:175` `fetch_imap_folders` → `dir().join("imap.toml")` — IMAP
  is per-profile, so `dir()` is correct; no change, but in scope for the
  audit.

Add `credentials::client_path(filename)` (→ `global_dir()`) and use it
wherever a `*_oauth_client.toml` is read or existence-checked.

### Atomic writes (multi-process global layer)

Per-profile writes have a single writer, but **global files
(`colorschemes.toml`, client regs) can be written by two processes at once**.
The current atomic-write helpers use **fixed** temp names
(`finalize.rs:387` `…toml.wizard.tmp`, `credentials.rs:118` `…toml.tmp`) —
two writers collide. Adopt the cache layer's scheme (`cache/mod.rs:289`:
pid + atomic counter in the temp name) for global writes, and make migration
+ global seeding atomic the same way. Per-profile writes can keep the fixed
names.

### Colorschemes layering

Theme load: parse `glint_root()/colorschemes.toml` → overlay
`config_dir()/colorschemes.toml` if present (insert/override by name) →
resolve the selected theme from the merged map. `init_default_config` seeds
the **root** `colorschemes.toml` (global); a per-profile override is created
only if the user adds one.

### Notes — make profile-aware (fix)

`notes::store::resolve_root` (`store.rs:104`) currently defaults to
`~/.glint/notes` (tier 2) — outside the config tree and **not** profile-aware,
so all profiles share notes today. Change the default to
`config_dir()/notes` (now per-profile). A user-set absolute `notes_dir` stays
honored and is documented as deliberately shared. Migration moves both old
note locations into `profiles/default/notes/` (above).

### Cache scoping + cleanup

`cache::open_default` (`cache/mod.rs:65`) → add the profile segment
(`…/glint/profiles/<active>/`). `--clear-cache` scopes to the active profile.
**Profile delete also removes the profile's cache segment** (else large
payloads orphan forever).

### Watcher

`config::watcher::spawn` watches `config_dir()` `NonRecursive`, so it
auto-targets the profile dir; a `profiles/` sibling causes no noise. Global
colorscheme edits at the root are **not** live-watched in v1 (relaunch to
apply); optionally also watch `glint_root()/colorschemes.toml` later.

### Profile management ops (`config::profiles`, new)

- **list** → dirs under `profiles/`.
- **create(name, from: Option<&str>)** → validate name (incl. case-fold
  collision); if `from`, deep-copy `profiles/<from>/` **minus `credentials/`**
  → `profiles/<name>/`; else seed defaults. Create `credentials/` 0700.
- **rename(old, new)** → guard `old != "default"`, not running, no case
  collision; dir rename.
- **delete(name)** → guard `name != "default"`, not running, not active;
  remove `profiles/<name>/` + its cache segment; confirm.

### Affected files (reference)

- `src/main.rs` — `Cli` (`--profile`/`-p`, `--list-profiles`), profile
  resolution + `set_active_profile` before `init_tracing`, dispatch,
  `--config`/`--profile` mutual exclusion.
- `src/config/mod.rs` — `glint_root`, profile-aware `config_dir`,
  `ACTIVE_PROFILE` (read-only + panic-on-reset), split `init_default_config`
  (global seed vs profile seed), migration entry.
- `src/config/migrate.rs` (new) — stage-and-publish migration + lock +
  ambiguity guard.
- `src/config/profiles.rs` (new) — list/create/clone(config-only)/rename/delete + liveness lock.
- `src/credentials.rs` — `global_dir`, `client_path`, pid+counter temp for
  global writes.
- `src/auth/registry.rs` — `needs_credential_capture` routes client specs to
  `global_dir`.
- `src/widgets/notes/store.rs` — profile-aware default root.
- theme/colorschemes loader — layered merge.
- `src/cache/mod.rs` — profile cache segment + delete cleanup hook.
- `src/wizard/app.rs`, `src/wizard/pages/profiles.rs` (new),
  `src/wizard/finalize.rs` — Profile Manager, "client reg is global" notice,
  global writes atomic.
- `INSTRUCTIONS.md`, `README.md`, `CHANGELOG.md` — docs + migration note.

## Non-goals (v1)

- Live profile switching inside a running process (relaunch).
- Per-profile OAuth **client** registrations (global-only; revisitable).
- Cloning credentials/tokens (config-only clone; re-auth per provider).
- A shared/global cache across profiles.
- Promoting LLM keys to global (left per-profile).
- Per-profile macOS `.app` launchers (the existing launcher could pass
  `--profile`; out of scope).

## Open decisions

1. **Live-reload of global colorscheme edits** — watch
   `glint_root()/colorschemes.toml` too, or require relaunch (current lean:
   relaunch).
2. **Cache path shape** — `…/glint/profiles/<name>/` vs `…/glint-<name>/`.
4. **Leftover flat files after migration** *(resolved)* — the CLI
   `--migrate-profiles` stays copy-only (safe for scripting), and the flat
   duplicates are removed only by an **explicit, consented** step: the
   `--setup` migration prompt (which migrates *and* removes the duplicates) or
   the `--cleanup-flat-config` CLI. Both run the tested
   `remove_flat_originals`, which only ever removes files that already exist in
   `profiles/default/`, keeping the global layer + `profiles/` tree. Never
   auto-deletes (that caused the 2026-06-30 data-loss incident).

## Phased plan

1. **Resolution + migration (platform).** `glint_root`/`config_dir` split,
   set-once `ACTIVE_PROFILE`, startup-ordering fix, stage-and-publish
   migration with lock + ambiguity guard. Default works end to end; existing
   installs migrate atomically. Tests assert every downstream resolver lands
   under the active profile.
2. **CLI + operational scoping.** `--profile`/`-p`, `GLINT_PROFILE`,
   `--list-profiles`, missing-profile errors, `--config` exclusivity,
   per-profile cache + log, notes profile-awareness.
3. **Global layer.** `global_dir`/`client_path` (client global-only) +
   call-site audit (incl. `needs_credential_capture`); colorschemes layered
   merge; atomic global writes; split seeding.
4. **Profile Manager.** Wizard front menu + create/clone(config-only)/rename/
   delete/edit with liveness guards and the "client reg is global" notice.
5. **Polish + docs.** Active-profile indicator, INSTRUCTIONS/README/CHANGELOG,
   migration note.
