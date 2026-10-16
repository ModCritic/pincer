//! Opt-in downsizing of `get_desktop_state` screenshots, made transparent to
//! desktop-scope actions on every platform.
//!
//! `get_desktop_state` returns its full-size capture unless the caller passes
//! `max_image_dimension`. When that cap downsizes the PNG, the response
//! reports the uncapped size as `screenshot_original_width/height`, and this
//! module remembers the ratio for the calling session. A later desktop-scope
//! action (`scope:"desktop"` or `coordinate_frame:"desktop"`) from the same
//! session has its `x`/`y` (and drag endpoints) scaled back to the uncapped
//! image before the platform tool interprets them, exactly as if the caller
//! had read them off the full-size capture. `capture_id` actions are left
//! alone: their capture binding already maps the exact published image.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Per-session `(x, y)` factors from capped-image pixels to uncapped pixels.
fn scales() -> &'static Mutex<HashMap<String, (f64, f64)>> {
    static SCALES: OnceLock<Mutex<HashMap<String, (f64, f64)>>> = OnceLock::new();
    SCALES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The runtime session key a dispatched call belongs to.
fn session_key(args: &Value) -> Option<&str> {
    args.get("_session_id").and_then(Value::as_str)
}

/// Record (or clear) the calling session's cap after a successful
/// `get_desktop_state`: the latest capture defines the frame of the pixels
/// the caller reads next.
pub fn record_desktop_state(args: &Value, structured: Option<&Value>) {
    let Some(key) = session_key(args) else {
        return;
    };
    let factor = structured.and_then(|s| {
        let axis = |delivered: &str, original: &str| {
            let delivered = s.get(delivered)?.as_f64()?;
            let original = s.get(original)?.as_f64()?;
            (delivered > 0.0 && original > 0.0).then_some(original / delivered)
        };
        Some((
            axis("screenshot_width", "screenshot_original_width")?,
            axis("screenshot_height", "screenshot_original_height")?,
        ))
    });
    let mut scales = scales().lock().unwrap();
    match factor {
        Some(factor) if factor != (1.0, 1.0) => {
            scales.insert(key.to_owned(), factor);
        }
        _ => {
            scales.remove(key);
        }
    }
}

/// Forget a session's cap (the session ended).
pub fn forget_session(session: &str) {
    scales().lock().unwrap().remove(session);
}

const X_KEYS: [&str; 3] = ["x", "from_x", "to_x"];
const Y_KEYS: [&str; 3] = ["y", "from_y", "to_y"];

/// The `(x, y)` factors from the calling session's capped desktop image back
/// to the uncapped one, for a desktop-scope call. `None` without a recorded
/// cap, for window-scoped pixels, and for `capture_id` actions. A tool whose
/// points are not the top-level keys [`map_desktop_args`] rewrites (the
/// nested path of `move_along_path`) applies these itself.
pub fn desktop_scale_factor(args: &Value) -> Option<(f64, f64)> {
    let desktop = args.get("scope").and_then(Value::as_str) == Some("desktop")
        || args.get("coordinate_frame").and_then(Value::as_str) == Some("desktop");
    if !desktop || args.get("capture_id").is_some() {
        return None;
    }
    let key = session_key(args)?;
    scales().lock().unwrap().get(key).copied()
}

/// Scale a desktop-scope action's coordinates from the session's capped
/// desktop image back to the uncapped one. No-op without a recorded cap, for
/// window-scoped pixels, and for `capture_id` actions.
pub fn map_desktop_args(args: &mut Value) {
    let Some((fx, fy)) = desktop_scale_factor(args) else {
        return;
    };
    for (keys, factor) in [(X_KEYS, fx), (Y_KEYS, fy)] {
        for key in keys {
            if let Some(value) = args.get(key).and_then(Value::as_f64) {
                args[key] = serde_json::json!(value * factor);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_capped_capture_scales_later_desktop_pixels_back() {
        let session = json!({"_session_id": "cap-test-a"});
        record_desktop_state(
            &session,
            Some(&json!({
                "screenshot_width": 960, "screenshot_height": 540,
                "screenshot_original_width": 1920, "screenshot_original_height": 1080
            })),
        );
        let mut click = json!({"_session_id": "cap-test-a", "scope": "desktop", "x": 100, "y": 50});
        map_desktop_args(&mut click);
        assert_eq!(
            (click["x"].as_f64(), click["y"].as_f64()),
            (Some(200.0), Some(100.0))
        );

        let mut drag = json!({"_session_id": "cap-test-a", "coordinate_frame": "desktop",
            "from_x": 10, "from_y": 20, "to_x": 30, "to_y": 40});
        map_desktop_args(&mut drag);
        assert_eq!(drag["to_y"].as_f64(), Some(80.0));

        // Window pixels, capture-bound pixels, and other sessions are untouched.
        let mut window = json!({"_session_id": "cap-test-a", "x": 100, "y": 50});
        map_desktop_args(&mut window);
        assert_eq!(window["x"], 100);
        let mut bound =
            json!({"_session_id": "cap-test-a", "scope": "desktop", "capture_id": "c", "x": 100});
        map_desktop_args(&mut bound);
        assert_eq!(bound["x"], 100);
        let mut other = json!({"_session_id": "cap-test-b", "scope": "desktop", "x": 100});
        map_desktop_args(&mut other);
        assert_eq!(other["x"], 100);
    }

    #[test]
    fn nested_desktop_points_can_read_the_factor() {
        let session = json!({"_session_id": "cap-test-d"});
        record_desktop_state(
            &session,
            Some(&json!({
                "screenshot_width": 1280, "screenshot_height": 720,
                "screenshot_original_width": 2560, "screenshot_original_height": 1440
            })),
        );
        let path = json!({"_session_id": "cap-test-d", "scope": "desktop",
            "points": [[10, 20], [30, 40]]});
        assert_eq!(desktop_scale_factor(&path), Some((2.0, 2.0)));
        // map_desktop_args leaves the nested points to the tool.
        let mut mapped = path.clone();
        map_desktop_args(&mut mapped);
        assert_eq!(mapped, path);
        let window = json!({"_session_id": "cap-test-d", "pid": 1, "window_id": 2});
        assert_eq!(desktop_scale_factor(&window), None);
        let other = json!({"_session_id": "cap-test-e", "scope": "desktop"});
        assert_eq!(desktop_scale_factor(&other), None);
    }

    #[test]
    fn a_full_size_capture_clears_the_cap() {
        let session = json!({"_session_id": "cap-test-c"});
        record_desktop_state(
            &session,
            Some(&json!({
                "screenshot_width": 960, "screenshot_height": 540,
                "screenshot_original_width": 1920, "screenshot_original_height": 1080
            })),
        );
        record_desktop_state(
            &session,
            Some(&json!({"screenshot_width": 1920, "screenshot_height": 1080})),
        );
        let mut click = json!({"_session_id": "cap-test-c", "scope": "desktop", "x": 100, "y": 50});
        map_desktop_args(&mut click);
        assert_eq!(click["x"], 100);
    }
}
