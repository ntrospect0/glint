//! Half-block pixel-art weather glyphs, extracted from a stock 4×4
//! icon set and faithfully sampled at each glyph's native chunky-pixel
//! grid. Each icon owns its own width, height, and color palette — no
//! shared bounding box, no resampling. Pairs of pixel rows collapse
//! into one terminal row via `▀` with independent fg/bg per cell.
//!
//! Generated from weather-ascii2.png; see the Python pipeline in the
//! commit that introduced this file.
//!
//! Post-processing applied at generation time:
//!  - Transparent cells trapped inside a closed outline are flood-
//!    filled with `INTERIOR_FILL` (the last palette slot in every
//!    icon) so the cloud body reads as solid against dark terminal
//!    backgrounds. Open shapes (sun rays, mist lines) are unaffected.
//!  - Grayish source colors are shifted a touch darker so the new
//!    bright interior fill stays visually distinct from the existing
//!    cloud body / outline shades.
//!
//! Currently-unmapped icons (kept here for the feature surface to grow into):
//! - `MOON`, `MOON_CLOUD` — night versions of clear / partly cloudy. Wire
//!   in once we know the user's sunrise/sunset to flip day/night sprites.
//! - `TORNADO` — useful if we subscribe to a severe-weather feed that
//!   surfaces tornado advisories.
//! - `LIGHTNING_BOLT` — standalone bolt with no cloud. A good fit for a
//!   transient thunder-advisory indicator separate from the in-cloud
//!   `THUNDER` glyph.
//! - `THUNDER_SHOWERS` — heavier alternative to `THUNDER`/`THUNDER_RAIN`
//!   when we want to split storm-intensity buckets.
//! - `SUN_STORM` — sun + dark cloud, for "sun behind heavy weather"
//!   (gusty, stormy daytime).
#![allow(dead_code)]

use ratatui::style::Color;

/// One pixel-art icon. `pixels[r][c]` is `Some(palette_index)` for a lit
/// pixel or `None` for transparent (terminal background shows through).
/// `width` and `height` are in chunky-pixels; the rendered height in
/// terminal rows is `(height + 1) / 2`.
pub struct WeatherIcon {
    pub width: u16,
    pub height: u16,
    pub palette: &'static [Color],
    pub pixels: &'static [&'static [Option<u8>]],
}

pub const CLOUD: WeatherIcon = WeatherIcon {
    width: 19,
    height: 10,
    palette: &[
        Color::Rgb(126, 134, 158),
        Color::Rgb(192, 193, 199),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(0), None, None, None, None, None, None, None],
        &[None, None, None, None, None, Some(0), Some(0), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), Some(0), None, None, None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), None, None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), None, None, None],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), None, None],
        &[Some(0), Some(0), Some(2), Some(2), Some(2), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(2), Some(0), Some(0)],
        &[Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(2), Some(2), Some(2), Some(0)],
        &[Some(0), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0)],
        &[Some(0), Some(0), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0), Some(0)],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None, None],
    ],
};

pub const RAIN: WeatherIcon = WeatherIcon {
    width: 19,
    height: 14,
    palette: &[
        Color::Rgb(126, 133, 158),
        Color::Rgb(192, 194, 199),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(0), None, None, None, None, None, None, None],
        &[None, None, None, None, None, Some(0), Some(0), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), Some(0), None, None, None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), None, None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), None, None, None],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), None, None],
        &[Some(0), Some(0), Some(2), Some(2), Some(2), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(2), Some(0), Some(0)],
        &[Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(2), Some(2), Some(2), Some(0)],
        &[Some(0), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0)],
        &[Some(0), Some(0), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0), Some(0)],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None, None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None],
        &[None, None, None, Some(0), None, None, None, Some(0), None, None, None, Some(0), None, None, None, Some(0), None, None, None],
        &[None, None, None, Some(0), None, Some(0), None, Some(0), None, Some(0), None, Some(0), None, Some(0), None, Some(0), None, None, None],
        &[None, None, None, None, None, Some(0), None, None, None, Some(0), None, None, None, Some(0), None, None, None, None, None],
    ],
};

pub const FOG: WeatherIcon = WeatherIcon {
    width: 25,
    height: 14,
    palette: &[
        Color::Rgb(125, 134, 158),
        Color::Rgb(193, 194, 198),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(0), None, None, None, None, None, None, None, None, None],
        &[None, None, None, None, None, None, None, None, None, Some(0), Some(0), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), Some(0), None, None, None, None, None, None],
        &[None, None, None, None, None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), None, None, None, None, None],
        &[None, None, None, None, None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), None, None, None, None, None],
        &[None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), None, None, None, None],
        &[None, None, None, None, Some(0), Some(0), Some(2), Some(2), Some(2), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(2), Some(0), Some(0), None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(2), Some(2), Some(2), Some(0), None, None],
        &[None, None, None, None, Some(0), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0), None, None],
        &[None, None, None, None, Some(0), Some(0), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0), Some(0), None, None],
        &[Some(1), Some(1), Some(1), None, None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None, None, None, None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None],
        &[None, None, Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), None, None, Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), None, Some(1), Some(1), Some(1), Some(1), Some(1), Some(1)],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None],
        &[None, None, None, None, None, None, Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), None, None, Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), None, None, None],
    ],
};

pub const THUNDER: WeatherIcon = WeatherIcon {
    width: 19,
    height: 12,
    palette: &[
        Color::Rgb(228, 189, 108),
        Color::Rgb(245, 240, 210),
        Color::Rgb(126, 134, 157),
        Color::Rgb(240, 185, 76),
        Color::Rgb(193, 193, 198),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), None, None, None, None, None],
        &[None, None, None, None, None, None, None, None, None, None, Some(3), Some(3), Some(3), Some(1), None, None, None, None, None],
        &[None, None, None, None, None, None, None, Some(2), Some(2), Some(3), Some(3), Some(3), None, None, None, None, None, None, None],
        &[None, None, None, None, None, Some(2), Some(2), Some(5), Some(5), Some(3), Some(3), Some(3), Some(0), Some(0), Some(2), None, None, None, None],
        &[None, None, None, None, Some(2), Some(4), Some(5), Some(5), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(4), Some(2), None, None, None],
        &[None, None, None, None, Some(2), Some(4), Some(5), Some(5), Some(5), Some(5), Some(3), Some(3), Some(3), Some(5), Some(5), Some(2), None, None, None],
        &[None, Some(2), Some(2), Some(2), Some(2), Some(4), Some(5), Some(5), Some(5), Some(5), Some(3), Some(3), Some(5), Some(5), Some(4), Some(2), Some(2), None, None],
        &[Some(2), Some(2), Some(5), Some(5), Some(5), Some(2), Some(4), Some(5), Some(5), Some(3), Some(3), Some(5), Some(5), Some(5), Some(4), Some(4), Some(5), Some(2), Some(2)],
        &[Some(2), Some(4), Some(5), Some(5), Some(5), Some(5), Some(4), Some(4), Some(4), Some(3), Some(5), Some(5), Some(5), Some(5), Some(4), Some(5), Some(5), Some(5), Some(2)],
        &[Some(2), Some(4), Some(4), Some(5), Some(5), Some(5), Some(5), Some(4), Some(3), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(2)],
        &[Some(2), Some(2), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(2), Some(2)],
        &[None, Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), None, None],
    ],
};

pub const SUN: WeatherIcon = WeatherIcon {
    width: 12,
    height: 12,
    palette: &[
        Color::Rgb(248, 232, 128),
        Color::Rgb(239, 213, 93),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, Some(0), None, None, Some(0), None, None, None, None],
        &[None, Some(0), None, None, None, None, None, None, None, None, Some(0), None],
        &[None, None, None, None, Some(1), Some(1), Some(1), Some(1), None, None, None, None],
        &[None, None, None, Some(1), Some(0), Some(0), Some(0), Some(0), Some(1), None, None, None],
        &[Some(0), None, Some(1), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(1), None, Some(0)],
        &[None, None, Some(1), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(1), None, None],
        &[None, None, Some(1), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(1), None, None],
        &[Some(0), None, Some(1), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(1), None, Some(0)],
        &[None, None, None, Some(1), Some(0), Some(0), Some(0), Some(0), Some(1), None, None, None],
        &[None, None, None, None, Some(1), Some(1), Some(1), Some(1), None, None, None, None],
        &[None, Some(0), None, None, None, None, None, None, None, None, Some(0), None],
        &[None, None, None, None, Some(0), None, None, Some(0), None, None, None, None],
    ],
};

pub const SUN_CLOUD: WeatherIcon = WeatherIcon {
    width: 20,
    height: 15,
    palette: &[
        Color::Rgb(246, 232, 143),
        Color::Rgb(243, 217, 98),
        Color::Rgb(126, 134, 157),
        Color::Rgb(192, 194, 198),
        Color::Rgb(162, 159, 130),
        Color::Rgb(163, 168, 180),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, Some(0), None, None, Some(0), None, None, None],
        &[None, None, None, None, None, None, None, None, None, None, Some(0), None, None, None, None, None, None, None, Some(0), None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, Some(1), Some(1), Some(1), Some(0), None, None, None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, Some(1), Some(0), Some(0), Some(0), Some(1), Some(0), None, None],
        &[None, None, None, None, None, None, None, None, None, Some(0), None, Some(1), Some(0), Some(0), Some(0), Some(0), Some(0), Some(1), Some(0), Some(0)],
        &[None, None, None, None, None, None, None, Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), Some(0), Some(0), Some(0), Some(0), Some(1), Some(0), None],
        &[None, None, None, None, None, Some(2), Some(2), Some(6), Some(6), Some(6), Some(6), Some(3), Some(2), Some(2), Some(4), Some(0), Some(0), Some(1), Some(0), None],
        &[None, None, None, None, Some(2), Some(3), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(5), Some(4), Some(0), Some(1), Some(0), Some(0)],
        &[None, None, None, None, Some(2), Some(3), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(3), Some(4), Some(1), Some(0), None, None],
        &[None, Some(2), Some(2), Some(2), Some(2), Some(3), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(5), Some(2), Some(5), None, None, None],
        &[Some(2), Some(2), Some(6), Some(6), Some(6), Some(2), Some(3), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(3), Some(3), Some(5), Some(2), Some(4), None],
        &[Some(2), Some(3), Some(6), Some(6), Some(6), Some(6), Some(3), Some(3), Some(3), Some(6), Some(6), Some(6), Some(6), Some(6), Some(3), Some(6), Some(6), Some(5), Some(3), None],
        &[Some(2), Some(3), Some(3), Some(6), Some(6), Some(6), Some(6), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(5), Some(3), None],
        &[Some(2), Some(2), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(5), Some(2), Some(3), None],
        &[None, Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(5), None, None, None],
    ],
};

pub const SNOW: WeatherIcon = WeatherIcon {
    width: 19,
    height: 15,
    palette: &[
        Color::Rgb(126, 134, 159),
        Color::Rgb(193, 193, 199),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(0), None, None, None, None, None, None, None],
        &[None, None, None, None, None, Some(0), Some(0), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), Some(0), None, None, None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), None, None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), None, None, None],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), None, None],
        &[Some(0), Some(0), Some(2), Some(2), Some(2), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(2), Some(0), Some(0)],
        &[Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(2), Some(2), Some(2), Some(0)],
        &[Some(0), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0)],
        &[Some(0), Some(0), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0), Some(0)],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None, None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None],
        &[None, None, Some(1), None, None, None, Some(1), None, None, None, Some(1), None, None, None, Some(1), None, None, None, Some(1)],
        &[None, None, None, None, Some(1), None, None, None, Some(1), None, None, None, Some(1), None, None, None, None, None, None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, Some(1), None, None],
        &[None, Some(1), None, None, None, None, Some(1), None, None, None, Some(1), None, None, None, Some(1), None, None, None, Some(1)],
    ],
};

pub const SHOWERS: WeatherIcon = WeatherIcon {
    width: 19,
    height: 14,
    palette: &[
        Color::Rgb(126, 134, 158),
        Color::Rgb(192, 193, 199),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(0), None, None, None, None, None, None, None],
        &[None, None, None, None, None, Some(0), Some(0), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), Some(0), None, None, None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), None, None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), None, None, None],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), None, None],
        &[Some(0), Some(0), Some(2), Some(2), Some(2), Some(0), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(2), Some(0), Some(0)],
        &[Some(0), Some(1), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(2), Some(2), Some(2), Some(0)],
        &[Some(0), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0)],
        &[Some(0), Some(0), Some(1), Some(0), Some(1), Some(0), Some(1), Some(0), Some(1), Some(0), Some(1), Some(0), Some(1), Some(0), Some(1), Some(0), Some(1), Some(0), Some(0)],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None, None],
        &[None, Some(1), Some(2), Some(0), Some(2), Some(0), Some(2), Some(1), Some(2), Some(0), Some(2), Some(1), Some(2), Some(0), Some(2), Some(1), None, None, None],
        &[Some(1), Some(2), Some(0), Some(2), Some(0), Some(2), Some(1), Some(2), Some(0), Some(2), Some(1), Some(2), Some(0), Some(2), Some(1), Some(2), Some(0), None, None],
        &[None, Some(0), Some(2), Some(0), Some(2), Some(1), Some(2), Some(0), Some(2), Some(1), Some(2), Some(0), Some(2), Some(1), Some(2), Some(0), None, None, None],
        &[Some(0), None, Some(0), None, Some(1), None, Some(0), None, Some(1), None, Some(0), None, Some(1), None, Some(0), None, None, None, None],
    ],
};

pub const MOON: WeatherIcon = WeatherIcon {
    width: 19,
    height: 12,
    palette: &[
        Color::Rgb(241, 232, 177),
        Color::Rgb(245, 226, 117),
        Color::Rgb(243, 213, 87),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, Some(0), None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None],
        &[None, None, None, None, None, Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), None, None, None, Some(1), None, None, None, None],
        &[None, None, None, None, Some(1), Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), None, None, None, None, None, None, None],
        &[None, None, None, Some(1), Some(2), Some(1), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), None, None, None, None, None, None],
        &[None, None, None, Some(2), Some(1), Some(1), Some(1), Some(2), Some(2), None, None, None, Some(2), None, None, None, None, None, Some(1)],
        &[None, None, None, Some(2), Some(1), Some(1), Some(1), Some(2), None, None, None, None, None, None, None, None, None, None, None],
        &[Some(1), Some(0), None, Some(2), Some(1), Some(1), Some(1), Some(2), None, None, None, None, None, None, None, None, None, None, None],
        &[None, None, None, Some(2), Some(1), Some(1), Some(1), Some(2), Some(2), None, None, None, Some(2), None, None, None, None, None, None],
        &[None, None, None, Some(1), Some(2), Some(1), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), None, None, None, None, None, None],
        &[None, None, None, None, Some(2), Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(2), None, None, None, None, None, None, None],
        &[None, None, None, None, None, Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), None, None, None, None, Some(2), None, None, None],
        &[None, None, Some(1), None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None],
    ],
};

pub const WET_SNOW: WeatherIcon = WeatherIcon {
    width: 19,
    height: 15,
    palette: &[
        Color::Rgb(126, 134, 158),
        Color::Rgb(193, 193, 198),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(0), None, None, None, None, None, None, None],
        &[None, None, None, None, None, Some(0), Some(0), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), Some(0), None, None, None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), None, None, None],
        &[None, None, None, None, Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), None, None, None],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), None, None],
        &[Some(0), Some(0), Some(2), Some(2), Some(2), Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(2), Some(0), Some(0)],
        &[Some(0), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(2), Some(2), Some(2), Some(0)],
        &[Some(0), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0)],
        &[Some(0), Some(0), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0), Some(0)],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None, None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None],
        &[None, None, Some(1), None, Some(0), None, Some(1), None, Some(0), None, Some(1), None, Some(0), None, Some(1), None, Some(0), None, Some(1)],
        &[None, None, None, None, Some(0), None, None, None, Some(0), None, None, None, Some(0), None, None, None, Some(0), None, None],
        &[None, None, None, None, None, None, Some(0), None, None, None, Some(0), None, None, None, Some(0), None, None, None, None],
        &[None, None, Some(1), None, Some(1), None, Some(0), None, Some(1), None, Some(0), None, Some(1), None, Some(0), None, Some(1), None, None],
    ],
};

pub const TORNADO: WeatherIcon = WeatherIcon {
    width: 15,
    height: 17,
    palette: &[
        Color::Rgb(91, 100, 118),
        Color::Rgb(191, 192, 198),
        Color::Rgb(142, 153, 178),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None, None, None, None],
        &[None, None, None, Some(0), Some(0), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0), Some(0), None, None],
        &[None, None, Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0), None],
        &[None, None, Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0), Some(0)],
        &[None, None, Some(2), Some(2), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(0), Some(0), Some(2), Some(0)],
        &[None, None, None, Some(2), Some(2), Some(2), Some(2), Some(0), Some(2), Some(0), Some(0), Some(2), Some(2), Some(2), Some(0)],
        &[None, None, None, Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(1), Some(2), Some(1), Some(2), Some(0), None],
        &[None, None, None, Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), None],
        &[None, None, Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(1), Some(1), Some(2), Some(2), Some(0), None, None],
        &[None, Some(1), Some(1), Some(1), Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), None, None, None],
        &[None, Some(1), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), Some(0), None, None, None, None],
        &[Some(1), Some(1), Some(1), Some(1), Some(2), Some(1), Some(1), Some(2), Some(0), None, None, None, None, None, None],
        &[Some(1), Some(1), Some(2), Some(2), Some(2), Some(2), Some(0), Some(0), None, None, None, None, None, None, None],
        &[None, Some(1), Some(1), Some(2), Some(2), Some(0), None, None, None, None, None, None, None, None, None],
        &[None, None, Some(1), Some(2), Some(2), Some(0), None, None, None, None, None, None, None, None, None],
        &[None, None, None, Some(1), Some(1), Some(2), Some(0), None, None, None, None, None, None, None, None],
        &[None, None, None, None, None, Some(1), Some(2), Some(0), None, None, None, None, None, None, None],
    ],
};

pub const MOON_CLOUD: WeatherIcon = WeatherIcon {
    width: 20,
    height: 20,
    palette: &[
        Color::Rgb(246, 227, 118),
        Color::Rgb(242, 213, 88),
        Color::Rgb(126, 134, 155),
        Color::Rgb(193, 193, 198),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0)],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, Some(0), Some(1), Some(0), Some(0), Some(0), Some(0), Some(0)],
        &[None, None, None, None, None, None, None, None, None, None, None, None, Some(1), Some(1), Some(0), Some(0), Some(0), Some(1), Some(1), Some(1)],
        &[None, None, None, None, None, None, None, None, None, None, None, None, Some(1), Some(0), Some(0), Some(0), Some(1), Some(1), None, None],
        &[None, None, None, None, None, None, None, Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), Some(0), Some(1), None, None, None],
        &[None, None, None, None, None, Some(2), Some(2), Some(4), Some(4), Some(4), Some(4), Some(3), Some(2), Some(2), Some(2), Some(0), Some(1), None, None, None],
        &[None, None, None, None, Some(2), Some(3), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(3), Some(2), Some(1), Some(1), None, None],
        &[None, None, None, None, Some(2), Some(3), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(2), Some(0), Some(1), Some(1), Some(1)],
        &[None, Some(2), Some(2), Some(2), Some(2), Some(3), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(3), Some(2), Some(2), Some(0), Some(0), Some(0)],
        &[Some(2), Some(2), Some(4), Some(4), Some(4), Some(2), Some(3), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(3), Some(3), Some(4), Some(2), Some(2), Some(1)],
        &[Some(2), Some(3), Some(4), Some(4), Some(4), Some(4), Some(3), Some(3), Some(3), Some(4), Some(4), Some(4), Some(4), Some(4), Some(3), Some(4), Some(4), Some(4), Some(2), None],
        &[Some(2), Some(3), Some(3), Some(4), Some(4), Some(4), Some(4), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(2), None],
        &[Some(2), Some(2), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(2), Some(2), None],
        &[None, Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), None, None, None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None],
        &[None, None, Some(3), None, None, None, Some(3), None, None, None, Some(3), None, None, None, Some(3), None, None, None, Some(3), None],
        &[None, None, None, None, Some(3), None, None, None, Some(3), None, None, None, Some(3), None, None, None, None, None, None, None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, Some(3), None, None, None],
        &[None, Some(3), None, None, None, None, Some(3), None, None, None, Some(3), None, None, None, Some(3), None, None, None, Some(3), None],
        &[None, None, None, None, Some(3), None, None, None, Some(3), None, None, None, Some(3), None, None, None, None, None, None, None],
    ],
};

pub const LIGHTNING_BOLT: WeatherIcon = WeatherIcon {
    width: 6,
    height: 10,
    palette: &[
        Color::Rgb(229, 192, 116),
        Color::Rgb(239, 185, 80),
        Color::Rgb(248, 236, 200),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, Some(0), Some(0), Some(0), Some(0)],
        &[None, None, Some(1), Some(1), Some(1), Some(2)],
        &[None, Some(0), Some(1), Some(1), None, None],
        &[None, Some(1), Some(1), Some(1), Some(1), Some(0)],
        &[Some(0), Some(1), Some(1), Some(1), Some(1), Some(1)],
        &[None, None, Some(1), Some(1), Some(1), None],
        &[None, None, Some(1), Some(1), None, None],
        &[None, Some(1), Some(1), None, None, None],
        &[None, Some(1), None, None, None, None],
        &[Some(0), None, None, None, None, None],
    ],
};

pub const THUNDER_SHOWERS: WeatherIcon = WeatherIcon {
    width: 19,
    height: 14,
    palette: &[
        Color::Rgb(90, 100, 116),
        Color::Rgb(162, 160, 161),
        Color::Rgb(137, 137, 141),
        Color::Rgb(241, 196, 72),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(0), None, None, None, None, None, None, None],
        &[None, None, None, None, None, Some(0), Some(0), Some(1), Some(1), Some(1), Some(1), Some(2), Some(0), Some(0), Some(0), None, None, None, None],
        &[None, None, None, None, Some(0), Some(2), Some(1), Some(1), Some(1), Some(3), Some(3), Some(3), Some(3), Some(1), Some(2), Some(0), None, None, None],
        &[None, None, None, None, Some(0), Some(2), Some(1), Some(1), Some(1), Some(3), Some(3), Some(3), Some(1), Some(1), Some(1), Some(0), None, None, None],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(2), Some(1), Some(1), Some(3), Some(3), Some(3), Some(1), Some(1), Some(1), Some(2), Some(0), Some(0), None, None],
        &[Some(0), Some(0), Some(1), Some(1), Some(1), Some(0), Some(2), Some(2), Some(3), Some(3), Some(3), Some(3), Some(3), Some(1), Some(2), Some(2), Some(1), Some(0), Some(0)],
        &[Some(0), Some(2), Some(1), Some(1), Some(1), Some(2), Some(2), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(2), Some(2), Some(1), Some(1), Some(1), Some(0)],
        &[Some(0), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(3), Some(3), Some(3), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0)],
        &[Some(0), Some(0), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(3), Some(3), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(0), Some(0)],
        &[None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(3), Some(3), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None, None],
        &[None, Some(2), Some(4), Some(0), Some(4), Some(0), Some(4), Some(2), Some(3), Some(0), Some(4), Some(2), Some(4), Some(0), Some(4), Some(2), None, None, None],
        &[Some(2), Some(4), Some(0), Some(4), Some(0), Some(4), Some(2), Some(3), Some(0), Some(4), Some(2), Some(4), Some(0), Some(4), Some(2), Some(4), Some(0), None, None],
        &[None, Some(0), Some(4), Some(0), Some(4), Some(2), Some(4), Some(0), Some(4), Some(2), Some(4), Some(0), Some(4), Some(2), Some(4), Some(0), None, None, None],
        &[Some(0), None, Some(0), None, Some(2), None, Some(0), None, Some(2), None, Some(0), None, Some(2), None, Some(0), None, None, None, None],
    ],
};

pub const SUN_STORM: WeatherIcon = WeatherIcon {
    width: 22,
    height: 14,
    palette: &[
        Color::Rgb(245, 226, 121),
        Color::Rgb(244, 213, 85),
        Color::Rgb(89, 100, 117),
        Color::Rgb(162, 162, 163),
        Color::Rgb(136, 138, 143),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None, None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, Some(0), Some(1), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, Some(0), Some(1), Some(0), Some(0), Some(0), Some(1), Some(1), Some(1), Some(1), Some(0)],
        &[None, None, None, None, None, None, None, None, None, None, None, None, Some(1), Some(0), Some(0), Some(0), Some(1), Some(1), None, None, None, Some(1)],
        &[None, None, None, None, None, None, None, Some(2), Some(2), Some(2), Some(2), Some(2), Some(1), Some(0), Some(0), Some(0), Some(1), None, None, None, None, None],
        &[None, None, None, None, None, Some(2), Some(2), Some(3), Some(3), Some(3), Some(3), Some(4), Some(2), Some(2), Some(2), Some(0), Some(1), None, None, None, None, None],
        &[None, None, None, None, Some(2), Some(4), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(4), Some(2), Some(1), Some(1), None, None, None, Some(1)],
        &[None, None, None, None, Some(2), Some(4), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(2), Some(0), Some(1), Some(1), Some(1), Some(1), Some(1)],
        &[None, Some(2), Some(2), Some(2), Some(2), Some(4), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(4), Some(2), Some(2), Some(0), Some(0), Some(0), Some(1), None],
        &[Some(2), Some(2), Some(3), Some(3), Some(3), Some(2), Some(4), Some(4), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(4), Some(4), Some(3), Some(2), Some(2), Some(1), None, None],
        &[Some(2), Some(4), Some(3), Some(3), Some(3), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(3), Some(3), Some(3), Some(2), None, None, None],
        &[Some(2), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(2), None, None, None],
        &[Some(2), Some(2), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(4), Some(2), Some(2), None, None, None],
        &[None, Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), Some(2), None, None, None, None, None],
    ],
};

pub const THUNDER_RAIN: WeatherIcon = WeatherIcon {
    width: 18,
    height: 16,
    palette: &[
        Color::Rgb(227, 189, 103),
        Color::Rgb(241, 185, 76),
        Color::Rgb(241, 228, 194),
        Color::Rgb(126, 134, 158),
        Color::Rgb(154, 157, 174),
        Color::Rgb(173, 176, 187),
        Color::Rgb(193, 194, 198),
        Color::Rgb(242, 245, 250),
    ],
    pixels: &[
        &[None, None, None, None, None, None, None, None, None, Some(0), Some(0), Some(0), Some(0), None, None, None, None, None],
        &[None, None, None, None, None, None, None, None, None, Some(1), Some(1), Some(1), Some(2), None, None, None, None, None],
        &[None, None, None, None, None, None, Some(3), Some(3), Some(0), Some(1), Some(1), None, None, None, None, None, None, None],
        &[None, None, None, None, Some(4), Some(3), Some(7), Some(7), Some(1), Some(1), Some(1), Some(0), Some(0), Some(3), None, None, None, None],
        &[None, None, None, Some(4), Some(5), Some(7), Some(7), Some(0), Some(1), Some(1), Some(1), Some(1), Some(1), Some(6), Some(3), None, None, None],
        &[None, None, None, Some(4), Some(5), Some(7), Some(7), Some(7), Some(7), Some(1), Some(1), Some(1), Some(7), Some(7), Some(3), None, None, None],
        &[Some(6), Some(3), Some(3), Some(3), Some(5), Some(7), Some(7), Some(7), Some(7), Some(0), Some(1), Some(7), Some(7), Some(6), Some(3), Some(3), None, None],
        &[Some(3), Some(5), Some(7), Some(7), Some(4), Some(5), Some(7), Some(7), Some(0), Some(1), Some(7), Some(7), Some(7), Some(6), Some(6), Some(7), Some(3), Some(3)],
        &[Some(4), Some(6), Some(7), Some(7), Some(7), Some(6), Some(6), Some(6), Some(1), Some(7), Some(7), Some(7), Some(7), Some(6), Some(7), Some(7), Some(7), Some(3)],
        &[Some(4), Some(6), Some(7), Some(7), Some(7), Some(7), Some(6), Some(0), Some(2), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(3)],
        &[Some(3), Some(4), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(6), Some(3), Some(3)],
        &[Some(6), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), Some(3), None, None],
        &[None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None],
        &[None, None, Some(5), Some(5), None, None, Some(3), None, None, None, Some(3), None, None, None, Some(3), None, None, None],
        &[None, None, Some(5), Some(5), Some(4), Some(6), Some(3), None, Some(3), None, Some(3), None, Some(3), None, Some(3), None, None, None],
        &[None, None, None, None, Some(4), Some(6), None, None, Some(3), None, None, None, Some(3), None, None, None, None, None],
    ],
};

/// Widest icon (chunky-pixel columns). Layout reserves this much width.
pub const MAX_WIDTH: u16 = 25;
/// Tallest icon in pixel rows. Render uses `(MAX_HEIGHT_PX + 1) / 2` char rows.
pub const MAX_HEIGHT_PX: u16 = 20;
pub const MAX_HEIGHT_CHARS: u16 = (MAX_HEIGHT_PX + 1) / 2;
