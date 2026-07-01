# Glint setup instructions

This file is the long-form companion to glint's interactive `--setup` wizard. The wizard collects the basics inline; the steps below cover the one-time provider setup (Google Cloud, Microsoft Azure, CalDAV) and the moving parts the TUI can't walk you through itself (browser tabs, third-party portals).

If you're stuck, see [Troubleshooting](#troubleshooting) at the bottom or open an issue at https://github.com/ntrospect0/glint/issues.

---

## Why you have to create OAuth credentials yourself

The Calendar and Email widgets use **OAuth 2.0** to read your calendar / mailbox. OAuth requires the application (in this case, glint) to identify itself to Google or Microsoft with a **Client ID** (and, for Google, a **Client Secret**).

For commercial apps (Fantastical, Spark, Thunderbird), the developer goes through Google's / Microsoft's **verification process** — a multi-step review that requires a registered business, a published privacy policy, a brand-verified domain, and (for Gmail-level scopes) an annual security audit costing ~$4,500. Once verified, the developer ships one Client ID for all users.

Glint is open-source, single-developer, and free. Two reasons we can't ship a single shared Client ID:

1. **OAuth secrets in OSS aren't really secret.** Google's terms forbid embedding client secrets in publicly readable code and will revoke clients found there.
2. **All-users-one-app would get flagged.** Even unverified shared clients trip anomaly detection within days; everyone's calendar would go dark at once.

So glint asks you to spin up **your own** OAuth credentials in 5–10 minutes. They're free, never leave your machine, and never re-prompted.

If you'd rather skip OAuth entirely, glint also supports:

- **CalDAV** for calendar (works with iCloud, Fastmail, Nextcloud, Synology, generic CalDAV servers) — uses an app password instead of OAuth. See [CalDAV](#caldav-icloud--fastmail--nextcloud) below.
- **IMAP** for email (Gmail with app-password, iCloud, Fastmail, Yahoo, self-hosted). See [IMAP](#imap-gmail--icloud--fastmail--self-hosted) below.

---

## Google: Calendar + Gmail

You'll create a Google Cloud project, enable the two APIs glint uses, configure an OAuth consent screen, and create a Desktop OAuth client. The credentials get pasted into the wizard's "Authorize Google" page.

### One-time setup

1. **Open https://console.cloud.google.com/** (sign in with the Google account whose calendar / mail you want glint to read).
2. **Create a project.** Top-left project picker → *New Project*. Name it whatever you like (e.g. `glint`). Click *Create*.
3. **Enable APIs.** Left sidebar → *APIs & Services* → *Library*.
   - Search for **"Google Calendar API"** → click → *Enable*.
   - Search for **"Gmail API"** → click → *Enable* (skip if you don't plan to use the email widget).
4. **Configure the OAuth consent screen.** Left sidebar → *APIs & Services* → *OAuth consent screen*.
   - User type: pick **External** (the only choice unless you have a Google Workspace org). Click *Create*.
   - App name: `glint` (or whatever you like).
   - User support email + Developer contact email: your own email.
   - Skip the optional logo / domain fields. *Save and Continue*.
   - Scopes step: *Save and Continue* (we'll request scopes from the OAuth flow itself).
   - **Test users step**: click *+ Add Users*, add **your own email address**, *Save and Continue*. This is what keeps the app usable while it's in "Testing" mode — Google requires test users for unverified apps.
   - *Back to Dashboard*. Leave the publishing status as "Testing" — you don't need to publish anything for personal use.
5. **Create the OAuth client.** Left sidebar → *APIs & Services* → *Credentials* → *+ Create credentials* → *OAuth client ID*.
   - Application type: **Desktop app**.
   - Name: `glint`.
   - Click *Create*.
6. **Copy the credentials.** Google shows you a dialog with the **Client ID** (looks like `1234567-abcdef.apps.googleusercontent.com`) and **Client Secret** (a short random string). Keep this dialog open — you'll paste both into glint's wizard in a moment.

### In the wizard

1. Go to *Configure email* (or *Configure calendar*) → press **Space** on **Authorize Google**.
2. Paste your Client ID and Client Secret into the inline form. Press Tab to move between fields.
3. Press Enter on **[ Save & Authorize ]**. The wizard:
   - Writes `~/.config/glint/credentials/google_oauth_client.toml` with `0600` perms.
   - Opens your browser to Google's consent screen.
   - Listens on a temporary localhost port for the redirect.
4. In the browser, sign in with the same Google account you added as a Test user. You'll see a warning that "Google hasn't verified this app" — that's expected for personal-use clients. Click *Continue* → *Continue* → tick the permissions glint asks for → *Continue*.
5. The browser shows a success page; glint's wizard resumes automatically, the title row shows your email address, and the folder picker loads your Gmail labels.

### What's stored where

- **Client credentials** → `~/.config/glint/credentials/google_oauth_client.toml` (your responsibility to back up if you reinstall).
- **Access + refresh token** → `~/.config/glint/credentials/google_oauth_token.default.toml` (auto-refreshed; safe to delete to force re-auth). The `default` segment is the account label — see [Multiple calendar accounts](#multiple-calendar-accounts-same-provider) to add more.

---

## Microsoft: Outlook calendar + mail

You'll register an Azure app, configure it to accept loopback redirects (so the browser can hand the token back to glint), add the Graph API permissions glint uses, and copy the Application (client) ID into the wizard.

Note: Microsoft uses **PKCE**, so there's no Client Secret to handle — just the Client ID.

### One-time setup

1. **Open https://portal.azure.com/** (sign in with the Microsoft account whose calendar / mail you want glint to read — personal or work/school both work).
2. **Open App registrations.** Search bar → type *Microsoft Entra ID* → click → left sidebar → *App registrations*.
3. **New registration.**
   - Name: `glint`.
   - Supported account types: **Accounts in any organizational directory and personal Microsoft accounts**.
   - Redirect URI: leave blank for now.
   - Click *Register*.
4. **Copy the Application (client) ID** from the new app's overview page. You'll paste it into glint's wizard.
5. **Authentication settings.** Left sidebar → *Authentication* → *Add a platform* → **Mobile and desktop applications**.
   - Tick **http://localhost** under the Custom redirect URIs list (the loopback option).
   - Click *Configure*.
6. **API permissions.** Left sidebar → *API permissions* → *Add a permission* → *Microsoft Graph* → *Delegated permissions*. Tick:
   - `Calendars.Read` — read your calendars (calendar widget).
   - `Mail.Read` — read your mail (email widget).
   - `User.Read` — read your account profile (**required** for the email widget to show your address; without it the title row stays "(loading…)" forever).
   - Click *Add permissions*.

### In the wizard

1. Press **Space** on **Authorize Microsoft (Outlook calendar)** or **Authorize Microsoft (for Outlook)**.
2. Paste the Application (client) ID into the inline form. Tab → Enter on [ Save & Authorize ].
3. Browser opens to login.microsoftonline.com. Sign in, approve the permissions glint asked for. The browser shows a success page; glint resumes.

### What's stored where

- **Client config** → `~/.config/glint/credentials/microsoft_oauth_client.toml`.
- **Access + refresh token** → `~/.config/glint/credentials/microsoft_oauth_token.default.toml`. The `default` segment is the account label — see [Multiple calendar accounts](#multiple-calendar-accounts-same-provider) to add more.

---

## Multiple calendar accounts (same provider)

The Calendar widget can show **two or more accounts of the same provider** at once — e.g. a work Outlook *and* a personal Outlook, or two Google accounts. (Different providers — Google + Outlook + CalDAV — have always merged; this adds *same-provider* accounts.)

The setup wizard manages one **default** account per provider — the account you authorize on its *Authorize* page. Extra accounts are added by hand, in two steps.

### 1. Authorize each extra account

Give the account a label and run the auth flow from a terminal:

```sh
glint --auth microsoft:work       # an extra Outlook account labelled "work"
glint --auth google:personal      # an extra Google account labelled "personal"
```

The provider name is `microsoft` or `google` (the same names the wizard uses — note `microsoft`, not `outlook`); the part after the `:` is your label. A bare `glint --auth microsoft` with no label authorizes the **default** account, exactly as the wizard does.

The browser opens as usual. If you're already signed into another account, choose **"Use another account"** (or use a private window) so you land on the right one. The token is written to `~/.config/glint/credentials/microsoft_oauth_token.<label>.toml` — one file per account. The shared `microsoft_oauth_client.toml` (your app registration) serves all of them, so you don't repeat the Azure / Google Cloud setup.

Keep labels simple — letters, digits, hyphens (e.g. `work`, `team-eu`). Avoid `:` in a label; it collides with the color-key format below.

### 2. Add a provider block per account

Edit `~/.config/glint/calendar.toml` and add one `[[providers]]` block per account, each with an `account = "<label>"` field. Omit `account` (or set it to `"default"`) for the wizard-managed default account:

```toml
# Default Outlook account (managed by the wizard; account omitted)
[[providers]]
kind = "outlook"
calendar_ids = ["primary"]

# Extra Outlook account "work"
[[providers]]
kind = "outlook"
account = "work"
calendar_ids = ["primary"]
```

No restart needed — live config reload picks it up. And the wizard won't clobber these hand-added blocks if you run it again: it preserves every `[[providers]]` block whose source stays ticked.

### Colors per account

Each account gets its own `calendar_colors` key. The **default** account is keyed by its provider kind (`outlook`, `google`); a **named** account is keyed `kind/label`, so accounts never share a color — even a Google and an Outlook account that happen to use the same label:

```toml
[calendar_colors]
"outlook:primary" = "#4097e4"        # default Outlook
"outlook/work:primary" = "#3fb950"   # the "work" account
```

Use `/` between kind and label, not `:` — the first `:` in a color key separates the source from the calendar id.

---

## CalDAV (iCloud / Fastmail / Nextcloud)

CalDAV is the open standard for calendar sync; it bypasses OAuth in favour of an app-specific password. Glint already ships the credentials template — you just fill it in.

### Apple iCloud

1. Go to https://appleid.apple.com and sign in.
2. *Sign-In and Security* → *App-Specific Passwords* → *+ Generate*.
3. Name it `glint` and copy the generated 4-block password (looks like `abcd-efgh-ijkl-mnop`).
4. Edit `~/.config/glint/credentials/caldav.toml`:

   ```toml
   server = "https://caldav.icloud.com"
   username = "your.apple.id@icloud.com"
   app_password = "abcd-efgh-ijkl-mnop"
   ```
5. In the wizard's Calendar page, tick **CalDAV** under *Calendar sources*.

### Fastmail

1. https://www.fastmail.com/settings/security/devicekeys → *New app password* → scope: *CalDAV*.
2. Same `caldav.toml` layout, with `server = "https://caldav.fastmail.com"`.

### Nextcloud, Synology, generic CalDAV

Use your normal username + an app-specific password from the server's UI. Server URL is whatever the server exposes (e.g. `https://nextcloud.example.com/remote.php/dav`).

---

## IMAP (Gmail / iCloud / Fastmail / self-hosted)

IMAP skips OAuth entirely — you provide host, port, username, and an app-specific password and glint connects directly. Works against any IMAP4rev1 server.

### Per-provider hosts + app-password recipes

| Provider | Host | Port | App-password URL |
|---|---|---|---|
| Gmail | `imap.gmail.com` | 993 | https://myaccount.google.com/ → Security → 2-Step Verification → App passwords |
| iCloud | `imap.mail.me.com` | 993 | https://appleid.apple.com → Sign-In and Security → App-Specific Passwords |
| Fastmail | `imap.fastmail.com` | 993 | https://www.fastmail.com/settings/security/devicekeys → *New app password* → scope *IMAP* |
| Yahoo | `imap.mail.yahoo.com` | 993 | https://login.yahoo.com/account/security → Generate app password |
| Outlook / O365 | `outlook.office365.com` | 993 | OAuth recommended — Microsoft is phasing out basic auth for IMAP |
| Self-hosted | whatever your server exposes | usually 993 | depends on the server (Mailcow / Dovecot / etc.) |

(Gmail requires 2-Step Verification to be enabled before you can generate app passwords. iCloud and Fastmail also force app passwords for third-party clients — your account password won't work.)

### In the wizard

1. *Configure email* → Provider → tick **IMAP**.
2. Press **Space** on **Set up IMAP credentials**.
3. Fill in the form: host, port (993 unless you know you need otherwise), username (usually your full email), app password. Press Enter on **[ Save & Authorize ]**.
4. The wizard writes `~/.config/glint/credentials/imap.toml` with 0600 perms, then loads your mailbox folders so the folder picker populates.
5. If the password is wrong, the folder picker stays on its "showing defaults" hint and `~/.config/glint/glint.log` has a `wizard: failed to fetch IMAP folders for picker` warning with the underlying error.

### Manual setup (skipping the wizard)

Drop a file at `~/.config/glint/credentials/imap.toml`:

```toml
host = "imap.gmail.com"
port = 993
use_tls = true
username = "alice@gmail.com"
app_password = "abcd-efgh-ijkl-mnop"
```

Then in `email.toml`:

```toml
provider = "imap"
folders = ["INBOX"]
```

Glint will connect lazily on the first fetch.

---

## LLM provider key (optional, for summaries)

The news + email widgets can summarise expanded items using a
configurable LLM. Glint ships two providers out of the box:
**Anthropic (Claude)** and **OpenAI (GPT)**. You pick one — the
widgets call whichever is active in `llm.toml`.

### Anthropic (Claude)

1. https://console.anthropic.com/ → *Get API Keys* → create a key.
2. Either pick **Anthropic (Claude)** on the wizard's
   *Global → LLM provider* field and paste the key into the
   *Anthropic API key* field below it, or edit
   `~/.config/glint/credentials/anthropic_key.toml`:

   ```toml
   api_key = "sk-ant-..."
   ```

### OpenAI (GPT)

1. https://platform.openai.com/api-keys → *Create new secret key*.
2. Either pick **OpenAI (GPT)** on the wizard's *Global → LLM
   provider* field and paste the key into the *OpenAI API key* field
   below it, or edit `~/.config/glint/credentials/openai_key.toml`:

   ```toml
   api_key = "sk-..."
   ```
3. The default OpenAI model is `gpt-5-mini`. Change it in `llm.toml`
   if you want a different model — the field is sent verbatim to the
   OpenAI Chat Completions API, so any model name your account can
   call (e.g. `gpt-4o-mini`, `gpt-4o`) works.

### Activating LLM features

After the key is on disk:

- `llm.toml` carries `[provider] name = "anthropic"` or `"openai"` —
  the wizard sets this when you pick a provider; you can flip it by
  hand any time.
- `summarize_with_llm = true` in `news.toml` / `email.toml` opts each
  widget into summaries. Both default to `true` once a key is configured.

If no key is configured (or `enabled = false` in `llm.toml`), the
`s summarize` keyboard hint stays hidden in the email widget; the
news widget renders the raw RSS excerpt instead.

---

## Troubleshooting

### Google: "Access blocked: glint has not completed the Google verification process"

Expected for personal-use clients. You're seeing the unverified-app warning. Click *Advanced* → *Go to glint (unsafe)*. This isn't actually unsafe — *you* are the developer in this scenario, and only the test users you added (yourself) can sign in.

### Microsoft: email widget shows "(loading…)" forever

Your token is missing the `User.Read` Graph permission. Re-authorize:

- **In the wizard:** open *Configure email* → Space on *Authorize Microsoft*.
- **Outside the wizard:** `glint --auth microsoft`.

When the browser asks for permissions, make sure "View your basic profile" is part of the consent.

### "The wizard says 'Wrote a template at … press Space again' but I edited the file and pressing Space still complains"

The wizard re-reads the file on each attempt. Double-check:

- File path is `~/.config/glint/credentials/<provider>_oauth_client.toml`.
- Values are quoted: `client_id = "1234-abcdef.apps.googleusercontent.com"`.
- Neither value still starts with `REPLACE_WITH_…`.

### "Folder picker shows '(showing defaults — list refreshes after you authorize)' but never updates"

The post-OAuth fetch is non-blocking and runs synchronously on auth completion. If it didn't populate, check `~/.config/glint/glint.log` for a `wizard: failed to fetch …` warning. The most common cause is a token without the right scope; re-authorize to refresh.

### Calendar / email shows "Last fetch failed: …"

Read the message — it carries the provider's error verbatim. Common causes:

- **Token expired and refresh failed**: re-authorize.
- **API quota**: only matters at very high call volumes; glint's defaults (60s calendar poll, 5min email poll) are well below any free-tier limit.
- **Network**: corporate proxies + loopback OAuth sometimes interact badly. Try from a non-corporate network or set `HTTPS_PROXY` if needed.

### I deleted my token file, now what?

Re-run the wizard's Authorize step or `glint --auth <provider>`. Glint will open a fresh browser flow and write a new token.

### I want to start completely fresh

```bash
rm -rf ~/.config/glint
glint --setup
```

This wipes everything — config, tokens, cache. The wizard seeds fresh defaults from glint's built-in templates.

---

## Profiles

Run glint in several isolated contexts — a focused **work** dashboard, a stripped-down **travel** view — each with its own layout, widgets, theme, and accounts:

```sh
glint --profile work        # or: glint -p work
glint                       # the "default" profile
```

Everything a profile owns — layout, widget configs, the selected theme, account tokens, notes, cache — is isolated under `~/.config/glint/profiles/<name>/`. Two things are **shared** across all profiles, so you define/register them once: the colorscheme **library** (`colorschemes.toml`) and the OAuth **client registrations** (`*_oauth_client.toml` — the Azure/Google *app*, not your account tokens). You can also select a profile with `GLINT_PROFILE=work` instead of the flag.

### Managing profiles

```sh
glint --list-profiles                     # list profiles (marks default + active)
glint --new-profile work                  # create, then: glint --profile work --setup
glint --new-profile staging --from work   # clone work's CONFIG (re-authorize accounts)
glint --rename-profile work:job           # OLD:NEW
glint --delete-profile job                # not "default" or the active profile
```

Cloning copies configuration but **not** credentials — authorize the clone's accounts with `glint --profile <name> --auth <provider>`.

### Upgrading from a pre-profiles install

Your existing flat `~/.config/glint/` **keeps working as-is** — the default profile reads it in place, so nothing moves and nothing is deleted, and glint stays interoperable with an older flat binary sharing the same directory.

Migrating into the `profiles/` layout is **opt-in and non-destructive**. When you're ready:

```sh
glint --migrate-profiles
```

That **copies** your flat config into `profiles/default/` and **leaves the originals in place** (so an older binary still works). The shared colorscheme library + OAuth client registrations stay at the root either way. Once you've fully switched to the profiles-aware binary, delete the leftover root `*.toml` yourself.

---

## What lives where on disk

```
~/.config/glint/                      # GLOBAL layer — shared across profiles
├── colorschemes.toml                 # named [schemes.*] palettes (the library)
├── credentials/                      # 0700-mode
│   ├── google_oauth_client.toml      # OAuth app registrations (shared)
│   └── microsoft_oauth_client.toml
└── profiles/
    ├── default/                      # PER-PROFILE layer (the default profile)
    │   ├── config.toml               # [global] + [layout] + [[layout.cells]]
    │   ├── clock.toml  calendar.toml  news.toml  stocks.toml  forex.toml
    │   ├── weather.toml  gallery.toml  resources.toml  email.toml
    │   ├── notes.toml  llm.toml
    │   ├── colorschemes.toml          # OPTIONAL per-profile scheme overrides
    │   ├── credentials/               # per-profile account secrets (0700)
    │   │   ├── google_oauth_token.<account>.toml
    │   │   ├── microsoft_oauth_token.<account>.toml
    │   │   ├── caldav.toml  imap.toml
    │   │   ├── anthropic_key.toml  openai_key.toml
    │   ├── notes/<instance>/<id>.md   # each note as a plain markdown file
    │   └── .runtime_state.toml  .wizard_state.toml  glint.log
    └── work/  travel/  …             # other profiles, same shape
```

Every `.toml` is plain text — edit in your favourite editor and either restart glint or hit `:reload` from the runtime command bar. The wizard preserves keys it doesn't manage (custom feeds, topic keywords, per-widget color overrides, etc.) across `--setup` re-runs, and preserves other profiles' `[[providers]]` blocks it doesn't own.

---

## Further reading

- `README.md` — install, keybindings, color schemes, multi-instance widgets, widget catalogue, external dependencies.
- `AGENTS.md` — architecture overview for contributors and AI assistants.
- https://github.com/ntrospect0/glint — source, issues, releases.
