// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod google;
pub mod loopback;
pub mod microsoft;
pub mod registry;

/// Account label used when none is given. Tokens for this account live in
/// the `…_oauth_token.default.toml` files; CLI `--auth google` (no
/// `:account`) and the setup wizard always operate on it.
pub const DEFAULT_ACCOUNT: &str = "default";
