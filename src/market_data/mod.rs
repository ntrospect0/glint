// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Provider-agnostic market-data primitives plus the Yahoo Finance
//! adapter. Houses the [`Period`] timeframe enum (shared by every
//! widget that plots a time-window-selectable price series) and the
//! per-period cache-key helper. Provider-specific HTTP / wire types
//! live in submodules — today only [`yahoo`]; future providers slot
//! in as siblings (`polygon.rs`, `finnhub.rs`, etc.) without touching
//! `period.rs` or `cache.rs`.

pub mod cache;
pub mod period;
pub mod yahoo;

pub use cache::quotes_cache_key;
pub use period::Period;
