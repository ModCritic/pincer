//! double_click and right_click run click's implementation.

use super::*;

#[test]
fn gesture_kinds_send_their_button_and_count() {
    assert_eq!(ClickKind::Double.button_and_count(), ("left", 2));
    assert_eq!(ClickKind::Right.button_and_count(), ("right", 1));
    assert_eq!(ClickKind::Double.verb(), "Double-clicked");
    assert_eq!(ClickKind::Right.verb(), "Right-clicked");
    assert_eq!(ClickKind::Right.tool(), "right_click");
    assert!(ClickKind::Double.is_gesture() && ClickKind::Right.is_gesture());
    assert!(!ClickKind::Click.is_gesture());
}

/// No second pointer pipeline: both tools hand the call to click's.
#[test]
fn double_and_right_click_delegate_to_click() {
    let source = include_str!("click_gestures.rs");
    for (tool, kind) in [
        ("impl Tool for DoubleClickTool {", "ClickKind::Double"),
        ("impl Tool for RightClickTool {", "ClickKind::Right"),
    ] {
        let invoke = source
            .rsplit_once(tool)
            .unwrap()
            .1
            .split_once("async fn invoke")
            .unwrap()
            .1
            .split_once("\n    }\n")
            .unwrap()
            .0;
        assert!(
            invoke.contains(&format!("gesture_click(&self.state, args, {kind})")),
            "{tool}: {invoke}"
        );
        assert!(!invoke.contains("spawn_blocking"), "{tool}");
    }
}

fn test_pid() -> u32 {
    std::process::id()
}

/// right_click used to ignore `modifier` on X11; it now goes through
/// click's parsing, which names a bad modifier. Foreground delivery keeps
/// the host's background refusals out of the way; nothing is sent.
#[tokio::test]
async fn right_click_reads_its_modifier_argument() {
    let tool = RightClickTool {
        state: ToolState::new(),
    };
    let result = tool
        .invoke(json!({
            "pid": test_pid(), "window_id": 99, "x": 5, "y": 6,
            "modifier": ["hyper"], "delivery_mode": "foreground",
            "_session_id": "delegation",
        }))
        .await;
    let structured = result.structured_content.expect("structured refusal");
    assert_eq!(structured["code"], "invalid_arguments", "{structured}");
    assert_eq!(structured["tool"], "right_click");
}

/// x,y go through click's window-point resolver: without a screenshot of
/// the window there is no frame, and nothing is sent.
#[tokio::test]
async fn double_click_pixels_need_a_screenshot_frame() {
    let tool = DoubleClickTool {
        state: ToolState::new(),
    };
    let result = tool
        .invoke(json!({
            "pid": test_pid(), "window_id": 99, "x": 5, "y": 6,
            "delivery_mode": "foreground", "_session_id": "delegation",
        }))
        .await;
    let structured = result.structured_content.expect("structured refusal");
    assert_eq!(structured["code"], "screenshot_context_missing", "{structured}");
    // A missing point names double_click, not click.
    let missing = DoubleClickTool {
        state: ToolState::new(),
    }
    .invoke(json!({"pid": test_pid(), "window_id": 99, "delivery_mode": "foreground"}))
    .await;
    assert!(
        format!("{:?}", missing.content).contains("double_click"),
        "{:?}",
        missing.content
    );
}
