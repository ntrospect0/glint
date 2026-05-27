# Contributing to glint

Thanks for your interest in glint. Issues, PRs, and design discussion
are all welcome.

## Quick start

```sh
git clone <your-fork-url> glint
cd glint
make test          # full suite, ~460 tests
make build         # debug binary
cargo run -- --setup   # interactive wizard, useful for end-to-end testing
```

Before you open a PR:

- `cargo fmt` — format
- `cargo clippy --features widgets-all` — lint clean (CI gates on this)
- `cargo test --features widgets-all` — every test passes on `main`
  at all times; please keep it that way

For non-trivial changes (new widget, new auth provider, anything that
changes a public TOML field), please open an issue first so we can
agree on shape before you spend time on code. `AGENTS.md` carries the
architecture overview — read it before working on the wizard or the
widget registry.

## Licensing of your contributions

glint is licensed under **GPL v3 or later** (see [LICENSE](LICENSE)).
Two things every contributor needs to understand and accept by
submitting a PR:

### 1. Sign-off (Developer Certificate of Origin)

Every commit in a PR must carry a `Signed-off-by:` line — the
[Developer Certificate of Origin](https://developercertificate.org/),
the same one the Linux kernel uses. It's a per-commit assertion that
you wrote the code (or have the right to submit it under the
project's license).

Add the line automatically with `git commit -s`:

```sh
git commit -s -m "your message"
# trailer becomes:
# Signed-off-by: Your Name <you@example.com>
```

If you forget on one commit, fix it with:

```sh
git commit --amend -s --no-edit
# for multiple commits in your branch:
git rebase --signoff main
```

PRs without sign-off on every commit will be asked to amend before
merge.

### 2. Relicensing grant

**By contributing to glint, you agree that the project's maintainer
(currently the original author) may relicense the project — including
your contributions — under terms different from GPL v3, at the
maintainer's sole discretion.** This includes (but is not limited to)
relaxing to a more permissive open-source license, or offering the
project under additional commercial license terms to specific
licensees.

Reasoning: a single-maintainer project benefits from being able to
adjust its licensing posture as the ecosystem evolves — for example,
adding an Apache 2.0 option for downstream Rust convention, or
offering a commercial license to an enterprise customer who can't
use GPL'd code. Without this grant, every relicensing decision would
require chasing down every past contributor for individual permission.

Your contribution will of course continue to be available under
GPL v3 too — relicensing adds options; it doesn't take any away. You
retain copyright in your contribution; you're granting the project a
relicensing right, not transferring ownership.

If this clause is a dealbreaker for you, please open an issue before
contributing so we can talk through it rather than discover the
disagreement at merge time.

## How features get scoped

glint is pre-launch v0.2 and shipping with deliberate restraint:

- **Widgets are independently optional.** Each widget compiles in
  only when its Cargo feature is enabled. New widgets are purely
  additive — declare a `widget-<name>` feature, implement the
  `Widget` trait under `src/widgets/<name>/`, and append a
  `WidgetDescriptor` to `src/widgets/registry.rs`. No edits to
  `app.rs`, `main.rs`, or the wizard are needed.
- **No design for hypothetical future requirements.** A bug fix
  doesn't need surrounding cleanup; a one-shot operation doesn't
  need a helper. Three similar lines is better than a premature
  abstraction.
- **Comments are minimal and explain WHY, not WHAT.** Well-named
  identifiers do the WHAT.

## Code of conduct

Be kind, be technical. Disagree about implementations; don't disagree
about people. We'll add a formal CoC if growth makes it necessary.

---

Questions? Open an issue, or start a discussion on the repo.
