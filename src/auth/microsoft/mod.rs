// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod flow;
pub mod store;

pub use store::OAuthClientConfig;

/// Microsoft Graph scopes. `Calendars.Read` powers the calendar widget;
/// `Mail.Read` powers the email widget (read-only — glint never marks or
/// modifies server-side state). `User.Read` is required for `/me` to return
/// the signed-in account address (without it the email widget's title row
/// stays on "(loading…)"). `offline_access` produces a refresh token.
pub const SCOPE: &str = "Calendars.Read Mail.Read User.Read offline_access";

/// `common` accepts both personal Microsoft accounts (outlook.com /
/// hotmail.com) and work/school accounts. The Azure app registration must
/// allow the matching account types.
pub const AUTH_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
pub const TOKEN_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
