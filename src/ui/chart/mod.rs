// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Time-series chart presentation primitives shared by widgets that
//! plot a price action (stocks, forex, ...). Provider-agnostic — these
//! routines render numbers, not market data — so a widget that pulled
//! its series from somewhere other than Yahoo (or wasn't financial at
//! all) could still compose against this layer.

pub mod annotations;
pub mod axes;
pub mod braille;
pub(crate) mod range_bar;
pub(crate) use range_bar::range_bar_line;
