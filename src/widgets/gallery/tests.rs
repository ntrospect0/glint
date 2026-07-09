// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the gallery widget. Split out of `mod.rs` per the repo standard.

use super::*;

#[test]
fn expand_tilde_replaces_leading_tilde() {
    let home = dirs::home_dir().expect("home dir for test");
    assert_eq!(
        expand_tilde("~/Pictures/x.png"),
        home.join("Pictures/x.png")
    );
}

#[test]
fn expand_tilde_passes_through_absolute_paths() {
    assert_eq!(expand_tilde("/tmp/x.png"), PathBuf::from("/tmp/x.png"));
}

#[test]
fn default_rotation_is_ten_seconds() {
    let cfg = GalleryConfig::default();
    assert_eq!(cfg.rotation_secs, 10);
    assert!(cfg.images.is_empty());
}

#[test]
fn zero_rotation_starts_paused() {
    let cfg = GalleryConfig {
        rotation_secs: 0,
        ..GalleryConfig::default()
    };
    let widget = GalleryWidget::with_config(
        "main".to_string(),
        cfg,
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    );
    let st = widget.current.lock().unwrap();
    assert!(st.paused);
    assert!(widget.rotation_interval.is_zero());
}

#[test]
fn non_zero_rotation_starts_running() {
    let cfg = GalleryConfig {
        rotation_secs: 5,
        ..GalleryConfig::default()
    };
    let widget = GalleryWidget::with_config(
        "main".to_string(),
        cfg,
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    );
    let st = widget.current.lock().unwrap();
    assert!(!st.paused);
    assert_eq!(widget.rotation_interval, Duration::from_secs(5));
}

#[test]
fn id_includes_instance_suffix() {
    let main = GalleryWidget::with_config(
        "main".into(),
        GalleryConfig::default(),
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    );
    assert_eq!(main.id(), "gallery");
    let inst = GalleryWidget::with_config(
        "kids".into(),
        GalleryConfig::default(),
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    );
    assert_eq!(inst.id(), "gallery@kids");
    assert_eq!(inst.display_name(), "Gallery (kids)");
}

#[test]
fn shortcut_preferences_default_to_g_a_l_r_y() {
    let w = GalleryWidget::with_config(
        "main".into(),
        GalleryConfig::default(),
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    );
    assert_eq!(w.shortcut_preferences(), &['g', 'a', 'l', 'r', 'y']);
}

#[test]
fn is_image_file_is_case_insensitive() {
    assert!(is_image_file(Path::new("/tmp/a.JPG")));
    assert!(is_image_file(Path::new("/tmp/a.png")));
    assert!(is_image_file(Path::new("/tmp/a.WebP")));
    assert!(!is_image_file(Path::new("/tmp/a.txt")));
    assert!(!is_image_file(Path::new("/tmp/a")));
}

#[test]
fn match_basename_handles_star_suffix_prefix_and_literal() {
    assert!(match_basename("*", "anything.jpg"));
    assert!(match_basename("*.jpg", "cover.jpg"));
    assert!(!match_basename("*.jpg", "cover.png"));
    assert!(match_basename("cover_*", "cover_2024.png"));
    assert!(!match_basename("cover_*", "anything.png"));
    assert!(match_basename("exact.png", "exact.png"));
    assert!(!match_basename("exact.png", "different.png"));
}

#[test]
fn expand_pattern_returns_literal_unchanged() {
    let out = expand_pattern("/some/where/img.png");
    assert_eq!(out, vec![PathBuf::from("/some/where/img.png")]);
}

#[test]
fn expand_pattern_globs_image_files_in_directory() {
    // Write three files into a fresh dir: two images + one non-image.
    // The non-image must be filtered out.
    use image::{Rgb, RgbImage};
    let dir = std::env::temp_dir().join(format!(
        "glint-gallery-glob-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let png_a = dir.join("a.png");
    let png_b = dir.join("b.png");
    let txt = dir.join("readme.txt");
    RgbImage::from_pixel(8, 8, Rgb([0, 0, 0]))
        .save(&png_a)
        .unwrap();
    RgbImage::from_pixel(8, 8, Rgb([0, 0, 0]))
        .save(&png_b)
        .unwrap();
    std::fs::write(&txt, b"hi").unwrap();

    let mut out = expand_pattern(&format!("{}/*", dir.display()));
    out.sort();
    assert_eq!(out, vec![png_a.clone(), png_b.clone()]);

    // Extension-typed glob: only one match.
    let out2 = expand_pattern(&format!("{}/*.png", dir.display()));
    assert_eq!(out2.len(), 2);

    let out3 = expand_pattern(&format!("{}/*.jpg", dir.display()));
    assert!(out3.is_empty());

    // Cleanup.
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn normalize_images_entry_auto_globs_a_real_directory() {
    // Bare directory → trailing `/*` appended so the entry behaves
    // as "every image in this directory" without the user typing it.
    let dir = std::env::temp_dir().join(format!(
        "glint-gallery-normalize-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let raw = dir.display().to_string();
    assert_eq!(normalize_images_entry(&raw), format!("{raw}/*"));
    // Trailing slash is honoured (no double-slash).
    let trailing = format!("{raw}/");
    assert_eq!(normalize_images_entry(&trailing), format!("{raw}/*"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn normalize_images_entry_leaves_files_and_globs_untouched() {
    // Already-glob entries pass through verbatim.
    assert_eq!(
        normalize_images_entry("~/Pictures/*.jpg"),
        "~/Pictures/*.jpg"
    );
    assert_eq!(normalize_images_entry("/abs/dir/*"), "/abs/dir/*");
    // Non-existent path: fall through to literal — the existing
    // loader logs the failure rather than silently treating it as
    // an empty glob.
    assert_eq!(
        normalize_images_entry("/this/path/does/not/exist"),
        "/this/path/does/not/exist"
    );
}

#[test]
fn expand_pattern_handles_missing_dir() {
    let out = expand_pattern("/this/directory/does/not/exist/*");
    assert!(out.is_empty());
}

#[test]
fn expand_all_patterns_dedups_across_entries() {
    let dir = std::env::temp_dir().join(format!(
        "glint-gallery-dedup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("a.png");
    image::RgbImage::from_pixel(8, 8, image::Rgb([0, 0, 0]))
        .save(&p)
        .unwrap();

    let patterns = vec![
        p.to_string_lossy().into_owned(),
        format!("{}/*", dir.display()),
    ];
    let out = expand_all_patterns(&patterns);
    assert_eq!(
        out,
        vec![p.clone()],
        "same file matched twice should appear once"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_thumb_round_trips_through_cache() {
    // Write a tiny PNG to a temp file, load it through load_thumb,
    // then load again and confirm the second call doesn't re-touch the
    // source file (i.e. removing the source between calls still works).
    use image::{Rgb, RgbImage};
    let mut tmp = std::env::temp_dir();
    tmp.push(format!(
        "glint-gallery-test-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let img = RgbImage::from_pixel(64, 32, Rgb([200, 100, 50]));
    img.save(&tmp).expect("write source image");

    let cache = ScopedCache::ephemeral();
    let first = load_thumb(&cache, &tmp).expect("first decode");
    assert_eq!((first.width(), first.height()), (64, 32));

    // Cache should now hold a thumb. Delete the source; a second
    // load_thumb call must still succeed via the cached bytes.
    std::fs::remove_file(&tmp).expect("remove source");
    let second = load_thumb(&cache, &tmp).expect("second decode (cached)");
    assert_eq!((second.width(), second.height()), (64, 32));
}

#[test]
fn image_smaller_than_pane_keeps_natural_size_and_centers() {
    // 50×50 px image at 10×10 cells = 5×5 cells, in a big 100×40 pane.
    // Must NOT upscale: draw rect stays 5×5, horizontally centered,
    // top-aligned. gap = 100 - 5 = 95 (odd) → shrink to 4, gap 96, x=48.
    let area = Rect::new(0, 0, 100, 40);
    let out = centered_image_rect(area, (50, 50), (10, 10));
    assert_eq!(out.height, 5, "natural height preserved (no upscale)");
    assert!(out.width <= 5, "natural width preserved (no upscale)");
    assert_eq!(out.y, 0, "top-aligned");
    let left = out.x - area.x;
    let right = area.width - out.width - left;
    assert_eq!(left, right, "horizontally centered: {out:?} in {area:?}");
}

#[test]
fn image_exactly_fitting_is_centered_top_aligned() {
    // 300×200 px at 10×10 cells = 30×20 cells; pane 30×20 → fits exactly.
    let area = Rect::new(0, 0, 30, 20);
    let out = centered_image_rect(area, (300, 200), (10, 10));
    assert_eq!(out, Rect::new(0, 0, 30, 20));
}

#[test]
fn portrait_image_exceeding_height_scales_down_and_centers() {
    // 800×1600 px at 10×10 cells = 80×160 cells; pane 30×20.
    // scale = min(30/80, 20/160, 1) = 0.125 → 10×20. Centered: x=10.
    let area = Rect::new(0, 0, 30, 20);
    let out = centered_image_rect(area, (800, 1600), (10, 10));
    assert_eq!(out.width, 10);
    assert_eq!(out.height, 20);
    assert_eq!(out.x, 10, "horizontally centered");
    assert_eq!(out.y, 0, "top-aligned");
}

#[test]
fn wide_image_exceeding_width_fills_width_and_top_aligns() {
    // 1600×800 px at 10×10 cells = 160×80 cells; pane 30×20.
    // scale = min(30/160, 20/80, 1) = 0.1875 → 30×15. Fills width, top.
    let area = Rect::new(0, 0, 30, 20);
    let out = centered_image_rect(area, (1600, 800), (10, 10));
    assert_eq!(out.width, 30, "scaled to full pane width");
    assert_eq!(out.height, 15);
    assert_eq!(out.x, 0);
    assert_eq!(out.y, 0, "top-aligned, does not fill height");
}

#[test]
fn centered_image_handles_zero_area_gracefully() {
    let zero = Rect::new(5, 7, 0, 0);
    assert_eq!(centered_image_rect(zero, (100, 100), (10, 10)), zero);
}

#[test]
fn odd_gap_shrinks_one_cell_for_symmetry() {
    // 150×100 px at 10×10 cells = 15×10 cells; fits in 50×20 pane at
    // natural size. gap = 50 - 15 = 35 (odd) → shrink width to 14 so
    // gap = 36 (even) → symmetric centering.
    let area = Rect::new(0, 0, 50, 20);
    let out = centered_image_rect(area, (150, 100), (10, 10));
    let left = out.x - area.x;
    let right = area.width - out.width - left;
    assert_eq!(left, right, "odd-gap shrink must restore symmetry");
    assert_eq!(out.width, 14, "should have shrunk by 1 cell");
    assert_eq!(out.height, 10, "natural height preserved");
}

#[test]
fn downscale_below_cap_returns_input_dimensions() {
    // 800×600 with cap 1600 should be unchanged.
    let img = DynamicImage::new_rgba8(800, 600);
    let out = downscale_to_max_dim(img, 1600);
    assert_eq!((out.width(), out.height()), (800, 600));
}

#[test]
fn downscale_above_cap_shrinks_aspect_correct() {
    // 4000×3000 (4:3) capped at 1600 → 1600×1200.
    let img = DynamicImage::new_rgba8(4000, 3000);
    let out = downscale_to_max_dim(img, 1600);
    assert_eq!((out.width(), out.height()), (1600, 1200));
}

#[test]
fn missing_image_does_not_panic_construction() {
    // Path that definitely doesn't exist. Under on-demand loading
    // the path still gets a slot — the background loader marks it
    // `failed: true` on its first decode attempt and the render
    // path shows "(image unavailable)". Constructor must not
    // panic; that's the contract the test guards.
    let cfg = GalleryConfig {
        images: vec!["/tmp/glint-gallery-does-not-exist-12345.png".to_string()],
        rotation_secs: 0,
        ..GalleryConfig::default()
    };
    let widget = GalleryWidget::with_config(
        "main".into(),
        cfg,
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    );
    assert_eq!(widget.slide_count(), 1, "literal path always gets a slot");
}
