use super::*;

fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|c| match c {
            pincer_core::protocol::Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[test]
fn a_scroll_bar_that_did_not_move_is_no_change_not_success() {
    // KWrite, 300 lines, 10 synthetic wheel steps down (desc-test round 2):
    // the scroll bar stayed at 0.0 of 584.
    let bar = Some((0.0, 0.0, 584.0));
    let movement = scroll_movement(&[bar], &[bar], "down");
    assert_eq!(
        movement,
        ScrollMovement::Still {
            value: 0.0,
            at_end: false
        }
    );
    let result = scroll_outcome_result(
        "down",
        10,
        pincer_contract::ScrollBy::Line,
        10,
        "x11_xsendevent",
        "background",
        movement,
        true,
    );
    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["code"], "no_change");
    assert_eq!(structured["effect"], "no_change");
    let text = text(&result);
    assert!(text.starts_with("Nothing scrolled"), "{text}");
    assert!(text.contains("ignores synthetic wheel events"), "{text}");
    assert!(text.contains("set_value"), "{text}");
    assert!(!text.contains("ticks"), "{text}");
}

#[test]
fn a_moved_scroll_bar_confirms_with_evidence_and_pages_name_their_steps() {
    let movement = scroll_movement(
        &[Some((0.0, 0.0, 584.0))],
        &[Some((18.0, 0.0, 584.0))],
        "down",
    );
    assert_eq!(movement, ScrollMovement::Moved(0.0, 18.0));
    let result = scroll_outcome_result(
        "down",
        2,
        pincer_contract::ScrollBy::Page,
        2 * WHEEL_STEPS_PER_PAGE,
        "x11_xtest_fg",
        "foreground",
        movement,
        false,
    );
    assert_ne!(result.is_error, Some(true));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["effect"], "confirmed");
    assert_eq!(structured["evidence"][0]["kind"], "accessibility_readback");
    let text = text(&result);
    assert!(text.contains("2 pages (6 wheel steps)"), "{text}");
    assert!(text.contains("from 0 to 18"), "{text}");
}

#[test]
fn a_view_at_its_end_and_an_unknown_window_are_said_plainly() {
    let at_bottom = Some((584.0, 0.0, 584.0));
    assert_eq!(
        scroll_movement(&[at_bottom], &[at_bottom], "down"),
        ScrollMovement::Still {
            value: 584.0,
            at_end: true
        }
    );
    // Scrolling up from the bottom is not at the end.
    assert_eq!(
        scroll_movement(&[at_bottom], &[at_bottom], "up"),
        ScrollMovement::Still {
            value: 584.0,
            at_end: false
        }
    );
    assert_eq!(
        scroll_movement(&[], &[], "down"),
        ScrollMovement::Unknown
    );
    assert_eq!(
        scroll_movement(&[None], &[None], "down"),
        ScrollMovement::Unknown
    );
    let unknown = scroll_outcome_result(
        "down",
        3,
        pincer_contract::ScrollBy::Line,
        3,
        "x11_xsendevent",
        "background",
        ScrollMovement::Unknown,
        true,
    );
    assert_ne!(unknown.is_error, Some(true));
    let text = text(&unknown);
    assert!(text.contains("not verified"), "{text}");
    assert!(text.contains("most likely did nothing"), "{text}");
}

#[test]
fn a_zombie_kill_says_the_process_has_exited() {
    assert!(kill_observation_note("zombie").contains("has exited"));
    assert_eq!(kill_observation_note("process_absent"), "");
}
