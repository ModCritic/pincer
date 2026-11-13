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

fn foreground_report(
    focus_after: crate::input::FocusAfter,
    window_change: Option<&str>,
) -> crate::input::ForegroundReport {
    crate::input::ForegroundReport {
        already_active: false,
        retried_activation: false,
        confirm_ms: 5,
        focus_after,
        window_change: window_change.map(str::to_owned),
        window_change_owned: true,
        popup_target: None,
        popup_closed: false,
    }
}

#[test]
fn a_foreground_key_result_never_says_not_verified_next_to_confirmed() {
    // press_key escape closed the Test Pad's menu (round 1): the text said
    // "not verified" while the effect line said confirmed.
    let closed = key_route_result(
        "Pressed key 'escape'",
        KeyRoute::Foreground(foreground_report(
            crate::input::FocusAfter::Target,
            Some("closed: popup window 16777259"),
        )),
        "foreground",
    );
    let structured = closed.structured_content.clone().unwrap();
    assert_eq!(structured["effect"], "confirmed");
    let said = text(&closed);
    assert!(!said.contains("not verified"), "{said}");
    assert!(
        said.contains("confirmed by a window change: closed: popup window 16777259"),
        "{said}"
    );

    let unchanged = key_route_result(
        "Pressed key 'end'",
        KeyRoute::Foreground(foreground_report(crate::input::FocusAfter::Target, None)),
        "foreground",
    );
    assert_eq!(
        unchanged.structured_content.clone().unwrap()["effect"],
        "unverifiable"
    );
    assert!(
        text(&unchanged).contains("not verified"),
        "{}",
        text(&unchanged)
    );

    let elsewhere = key_route_result(
        "Pressed key 'end'",
        KeyRoute::Foreground(foreground_report(crate::input::FocusAfter::Elsewhere, None)),
        "foreground",
    );
    assert_eq!(
        elsewhere.structured_content.clone().unwrap()["effect"],
        "suspected_noop"
    );
    let said = text(&elsewhere);
    assert!(said.contains("left the target process"), "{said}");
    assert!(!said.contains('\u{2014}'), "{said}");
}

#[test]
fn synthetic_keys_to_an_inactive_window_say_qt_drops_them() {
    // KWrite with the Test Pad active: background ctrl+End did nothing.
    let result = key_route_result(
        "Pressed ctrl+end on pid 200",
        KeyRoute::SyntheticToInactive(16777223),
        "background",
    );
    let said = text(&result);
    assert!(said.contains("not the active window"), "{said}");
    assert!(said.contains("delivery_mode foreground"), "{said}");
    assert_eq!(
        result.structured_content.unwrap()["target_window_active"],
        false
    );
}

#[test]
fn invoke_menu_says_what_became_of_the_menu_it_opened() {
    use crate::atspi::native::MenuCleanup;
    let closed = invoke_menu_result(
        "delivery_mode=background, focus untouched",
        &MenuCleanup::Closed {
            menu: "Edit".into(),
        },
        1,
        2,
        "background",
    );
    let said = text(&closed);
    assert!(
        said.contains("then closed the Edit menu it had opened to reach the item"),
        "{said}"
    );
    assert_eq!(closed.structured_content.unwrap()["menu_closed"], "Edit");

    let open = invoke_menu_result(
        "delivery_mode=background, focus untouched",
        &MenuCleanup::StillOpen {
            menu: "Edit".into(),
        },
        1,
        2,
        "background",
    );
    let said = text(&open);
    assert!(
        said.contains("The Edit menu Pincer opened to reach the item is still open"),
        "{said}"
    );
    assert!(said.contains("press_key escape on window 2"), "{said}");
    assert_eq!(open.structured_content.unwrap()["menu_left_open"], "Edit");

    let none = invoke_menu_result("r", &MenuCleanup::NoneOpen, 1, 2, "background");
    assert_eq!(
        text(&none),
        "Resolved the live native menu path and dispatched its final accessibility action \
         (r); verify the command's semantic effect from fresh state."
    );
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
    assert_eq!(scroll_movement(&[], &[], "down"), ScrollMovement::Unknown);
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
