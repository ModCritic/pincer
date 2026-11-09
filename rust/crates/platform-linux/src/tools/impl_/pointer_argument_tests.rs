use super::*;

#[test]
fn pid_only_pointer_call_is_refused_not_clicked_at_origin() {
    let refusal =
        require_point_args("click", &json!({"target_id": "s1:147", "pid": 1867})).expect("refused");
    assert_eq!(refusal.is_error, Some(true));
    let structured = refusal.structured_content.expect("structured");
    assert_eq!(structured["code"], "invalid_arguments");
    assert_eq!(structured["effect"], "refused");
    assert!(structured["accepted_forms"].as_array().unwrap().len() == 3);
}

#[test]
fn complete_point_passes() {
    assert!(require_point_args(
        "click",
        &json!({"pid": 1, "window_id": 2, "x": 3, "y": 4.5})
    )
    .is_none());
    assert!(require_point_args("click", &json!({"x": 1, "y": null})).is_some());
    assert!(require_point_args("click", &json!({"y": 1})).is_some());
}

#[test]
fn foreground_report_becomes_evidence() {
    let report = crate::input::ForegroundReport {
        already_active: false,
        retried_activation: false,
        confirm_ms: 12,
        focus_after: crate::input::FocusAfter::SamePid,
        window_change: Some("appeared: window 9 \"Brightness-Contrast\"".into()),
        window_change_owned: true,
        popup_target: None,
        popup_closed: false,
    };
    let v = foreground_structured("x11_xtest_fg", report, serde_json::Map::new());
    assert_eq!(v["effect"], "confirmed");
    assert_eq!(v["focus_after"], "same_pid");
    assert_eq!(v["evidence"][0]["kind"], "window_change");
    let quiet = crate::input::ForegroundReport {
        already_active: true,
        retried_activation: false,
        confirm_ms: 0,
        focus_after: crate::input::FocusAfter::Target,
        window_change: None,
        window_change_owned: false,
        popup_target: None,
        popup_closed: false,
    };
    let v = foreground_structured("x11_xtest_fg", quiet, serde_json::Map::new());
    assert_eq!(v["effect"], "unverifiable");
    assert_eq!(v["evidence"][0]["kind"], "native_api_result");
    let closed = crate::input::ForegroundReport {
        already_active: true,
        retried_activation: false,
        confirm_ms: 0,
        focus_after: crate::input::FocusAfter::Unknown,
        window_change: Some("closed: window 3 \"Format Cells\"".into()),
        window_change_owned: true,
        popup_target: None,
        popup_closed: false,
    };
    let v = foreground_structured("x11_xtest_fg", closed, serde_json::Map::new());
    assert_eq!(v["effect"], "confirmed");
    assert_eq!(v["evidence"][0]["kind"], "window_change");
    // A popup that names no pid is listed in the target's window set (a VCL
    // menu carries none), but it may be another client's: once the focus
    // left the target process its change is no confirmation.
    let unattributed = |focus_after| crate::input::ForegroundReport {
        already_active: true,
        retried_activation: false,
        confirm_ms: 0,
        focus_after,
        window_change: Some("appeared: popup window 7 (200x300 at 1,2)".into()),
        window_change_owned: false,
        popup_target: None,
        popup_closed: false,
    };
    let v = foreground_structured(
        "x11_xtest_fg",
        unattributed(crate::input::FocusAfter::Elsewhere),
        serde_json::Map::new(),
    );
    assert_eq!(v["effect"], "suspected_noop");
    let v = foreground_structured(
        "x11_xtest_fg",
        unattributed(crate::input::FocusAfter::Unknown),
        serde_json::Map::new(),
    );
    assert_eq!(v["effect"], "unverifiable");
    assert_eq!(v["evidence"][0]["kind"], "native_api_result");
    let v = foreground_structured(
        "x11_xtest_fg",
        unattributed(crate::input::FocusAfter::SamePid),
        serde_json::Map::new(),
    );
    assert_eq!(v["effect"], "confirmed");
    let lost = crate::input::ForegroundReport {
        already_active: true,
        retried_activation: false,
        confirm_ms: 0,
        focus_after: crate::input::FocusAfter::Elsewhere,
        window_change: None,
        window_change_owned: false,
        popup_target: None,
        popup_closed: false,
    };
    let v = foreground_structured("x11_xtest_fg", lost, serde_json::Map::new());
    assert_eq!(v["effect"], "suspected_noop");
}

fn popup_report(popup_closed: bool) -> crate::input::ForegroundReport {
    crate::input::ForegroundReport {
        already_active: false,
        retried_activation: false,
        confirm_ms: 3,
        focus_after: crate::input::FocusAfter::SamePid,
        window_change: popup_closed
            .then(|| "closed: popup window 90177563 \"PincerQtProbe\" (126x290 at 785,340)".into()),
        window_change_owned: popup_closed,
        popup_target: Some(90177563),
        popup_closed,
    }
}

/// A foreground click on a combo popup (bug 1 in the tested app): the popup took
/// the input without an activation, closed, and the result says so instead
/// of asking for a screenshot.
#[test]
fn a_foreground_click_that_closed_its_popup_is_confirmed() {
    let report = popup_report(true);
    let suffix = foreground_click_suffix("x11_xtest_fg", &report);
    assert!(
        suffix.contains("popup window 90177563 took the input without activation"),
        "{suffix}"
    );
    assert!(suffix.contains("closed: popup window 90177563"), "{suffix}");
    assert!(!suffix.contains("not verified"), "{suffix}");
    let mut v = foreground_structured("x11_xtest_fg", report.clone(), serde_json::Map::new());
    let outcome = PopupClickOutcome::from_foreground(&report).expect("popup target");
    let note = outcome.apply(&mut v);
    assert!(note.contains("closed after the click"), "{note}");
    assert_eq!(v["effect"], "confirmed");
    assert_eq!(v["popup_closed"], true);
    assert_eq!(v["foreground"]["activated"], false);
    // The window change already carries the evidence; it is not doubled.
    assert_eq!(v["evidence"].as_array().map(Vec::len), Some(1));
}

/// A popup that stayed open after the click is reported as open, never as
/// success, and the result names the way to close it.
#[test]
fn a_popup_still_open_after_the_click_is_not_success() {
    let report = popup_report(false);
    let suffix = foreground_click_suffix("x11_xtest_fg", &report);
    assert!(suffix.contains("not verified"), "{suffix}");
    let mut v = foreground_structured("x11_xtest_fg", report.clone(), serde_json::Map::new());
    let note = PopupClickOutcome::from_foreground(&report)
        .expect("popup target")
        .apply(&mut v);
    assert!(note.contains("still open after the click"), "{note}");
    assert!(note.contains("press_key Escape"), "{note}");
    assert_eq!(v["effect"], "unverifiable");
    assert_eq!(v["popup_closed"], false);
}

/// A background click on a popup (synthetic route, which Qt accepts on its
/// popups) is confirmed by the popup closing.
#[test]
fn a_background_click_that_closed_its_popup_is_confirmed() {
    let mut v = json!({
        "path": "x11_xsendevent",
        "verified": false,
        "effect": "unverifiable",
        "delivery_mode": "background",
    });
    let note = PopupClickOutcome {
        window: 90177563,
        closed: true,
        ran_action: false,
    }
    .apply(&mut v);
    assert!(note.contains("window_id 90177563"), "{note}");
    assert_eq!(v["effect"], "confirmed");
    assert_eq!(v["verified"], true);
    assert_eq!(v["evidence"][0]["kind"], "window_change");
    assert!(
        v["evidence"][0]["detail"]
            .as_str()
            .is_some_and(|d| d.contains("popup window 90177563 closed")),
        "{v}"
    );
}

/// An ordinary foreground click keeps its activation wording.
#[test]
fn a_foreground_window_click_does_not_mention_a_popup() {
    let report = crate::input::ForegroundReport {
        popup_target: None,
        popup_closed: false,
        window_change: None,
        window_change_owned: false,
        ..popup_report(false)
    };
    let suffix = foreground_click_suffix("x11_xtest_fg", &report);
    assert!(!suffix.contains("popup"), "{suffix}");
    assert!(PopupClickOutcome::from_foreground(&report).is_none());
}

/// Qt runs a menu item's accessible action without closing its menu: the
/// result says the action was dispatched and the menu is still open, not
/// that the click missed.
#[test]
fn an_accessible_action_that_left_its_menu_open_says_so() {
    let mut v = json!({"path": "ax", "verified": false, "effect": "unverifiable"});
    let note = PopupClickOutcome {
        window: 90177603,
        closed: false,
        ran_action: true,
    }
    .apply(&mut v);
    assert!(note.contains("accessible action was dispatched"), "{note}");
    assert!(note.contains("still open"), "{note}");
    assert!(note.contains("delivery_mode foreground"), "{note}");
    assert_eq!(v["effect"], "unverifiable");
    assert_eq!(v["popup_closed"], false);
}
