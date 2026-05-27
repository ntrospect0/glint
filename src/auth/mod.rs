// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod google;
pub mod loopback;
pub mod microsoft;
pub mod registry;

use std::path::PathBuf;

use anyhow::Result;

/// Backwards-compatible alias for [`crate::credentials::dir`]. New
/// code should call `crate::credentials::dir()` directly; this
/// stays because every auth submodule and a handful of callers
/// outside the auth tree imported `auth::credentials_dir`.
pub fn credentials_dir() -> Result<PathBuf> {
    crate::credentials::dir()
}
