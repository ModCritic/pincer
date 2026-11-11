use super::{megapixel_long_edge_cap, window_screenshot_size, WINDOW_SCREENSHOT_MAX_PIXELS};

#[test]
fn a_window_shot_above_the_api_threshold_is_capped_below_it() {
    let edge = megapixel_long_edge_cap(1568, 861, WINDOW_SCREENSHOT_MAX_PIXELS).expect("capped");
    let scale = edge as f64 / 1568.0;
    let (w, h) = (
        (1568.0 * scale).round() as u64,
        (861.0 * scale).round() as u64,
    );
    assert!(w * h <= WINDOW_SCREENSHOT_MAX_PIXELS, "{w}x{h}");
    assert!((1440..1568).contains(&edge), "{edge}");
}

#[test]
fn a_shot_within_the_threshold_is_left_alone() {
    assert_eq!(
        megapixel_long_edge_cap(1280, 800, WINDOW_SCREENSHOT_MAX_PIXELS),
        None
    );
    assert_eq!(
        megapixel_long_edge_cap(0, 800, WINDOW_SCREENSHOT_MAX_PIXELS),
        None
    );
}

/// The sizes the two-pass resize (fit to max_image_dimension, then fit to
/// the megapixel cap) delivered before it became one pass (89124df, measured
/// 2026-11-11). The one-pass size must not move, or pixel clicks would.
#[test]
fn window_screenshot_sizes_match_the_two_pass_sizes() {
    for ((w, h), expected) in [
        ((1600, 1000), (1356, 848)),
        ((1920, 1050), (1449, 793)),
        ((2560, 1400), (1449, 793)),
        ((1200, 1000), (1174, 978)),
        ((1280, 800), (1280, 800)),
    ] {
        assert_eq!(
            window_screenshot_size(w, h, 1568, false),
            expected,
            "{w}x{h}"
        );
    }
}

#[test]
fn an_explicit_limit_skips_the_megapixel_cap() {
    assert_eq!(window_screenshot_size(1600, 1000, 0, true), (1600, 1000));
    assert_eq!(window_screenshot_size(1600, 1000, 1568, true), (1568, 980));
    // Configured 0 (no limit) still gets the cap.
    assert_eq!(window_screenshot_size(1600, 1000, 0, false), (1356, 848));
}
