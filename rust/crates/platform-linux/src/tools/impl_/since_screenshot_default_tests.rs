use super::window_state_captures_screenshot;

#[test]
fn a_full_read_captures_by_default() {
    assert!(window_state_captures_screenshot(None, false, true, false));
}

#[test]
fn a_since_read_skips_the_screenshot_unless_asked() {
    assert!(!window_state_captures_screenshot(None, true, true, false));
    assert!(window_state_captures_screenshot(
        Some(true),
        true,
        true,
        false
    ));
}

#[test]
fn an_explicit_false_always_skips_the_grab() {
    assert!(!window_state_captures_screenshot(
        Some(false),
        false,
        true,
        false
    ));
    assert!(!window_state_captures_screenshot(
        Some(false),
        true,
        true,
        false
    ));
}

/// A read that skips the tree is not a diff, so `since` leaves its default.
#[test]
fn a_screenshot_only_read_still_captures_with_since() {
    assert!(window_state_captures_screenshot(None, true, false, false));
}

#[test]
fn an_out_file_always_captures() {
    assert!(window_state_captures_screenshot(None, true, true, true));
    assert!(window_state_captures_screenshot(
        Some(false),
        true,
        true,
        true
    ));
}

/// A `since` read without a screenshot must not cost the pixel frame of the
/// read before it: pixel tools resolve their scale from the latest snapshot.
#[test]
fn a_tree_only_read_keeps_the_frame_until_the_window_is_resized() {
    use super::{screenshot_scale, ToolState};
    use crate::atspi::snapshot::AtspiSnapshot;

    let state = ToolState::new();
    let args = serde_json::json!({"_session_id": "t"});
    let publish = |scale: Option<f64>, size: (f64, f64)| {
        state.snapshots.publish_sized_for_session(
            42,
            7,
            AtspiSnapshot::from_nodes(&[]),
            Some("t"),
            scale,
            Some(size),
        )
    };

    publish(Some(2.0), (1280.0, 800.0));
    publish(None, (1280.0, 800.0));
    assert_eq!(screenshot_scale(&state, &args, 42, Some(7)).unwrap(), 2.0);

    publish(None, (1024.0, 800.0));
    assert!(screenshot_scale(&state, &args, 42, Some(7)).is_err());

    // get_window_state is what records the size; a plain publish would not.
    let source = include_str!("../impl_.rs");
    let invoke = source
        .split_once("impl Tool for GetWindowStateTool {")
        .unwrap()
        .1
        .split_once("// ── launch_app")
        .unwrap()
        .0;
    assert!(invoke.contains("publish_sized_for_session("));
    assert!(!invoke.contains("snapshots.publish_for_session("));
}
