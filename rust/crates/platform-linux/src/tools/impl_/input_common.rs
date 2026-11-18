//! Plumbing the input tools share: time budgets for blocking input work, and
//! the error, evidence and foreground-check results they report.

use super::*;

/// Upper bound for one foreground input transaction: focus confirmation plus
/// the injection itself. Typing sleeps per character, so scale with `chars`.
pub(super) fn foreground_budget(chars: usize) -> std::time::Duration {
    std::time::Duration::from_millis(12_000 + (chars as u64) * 40)
}

/// `spawn_blocking` with a hard deadline. A wedged X server (or a client
/// holding a server grab) must surface as a structured `foreground_timeout`
/// error instead of a tool call that never returns. The blocking task is not
/// cancellable; it is left to finish (or leak) on the blocking pool.
pub(super) async fn spawn_blocking_bounded<T: Send + 'static>(
    label: &'static str,
    budget: std::time::Duration,
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<anyhow::Result<T>, tokio::task::JoinError> {
    match tokio::time::timeout(budget, tokio::task::spawn_blocking(f)).await {
        Ok(joined) => joined,
        Err(_elapsed) => Ok(Err(anyhow::anyhow!(
            "{}: {label} did not complete within {budget:?}; the X server or the target \
             application may be wedged. Take a screenshot before retrying",
            crate::input::foreground::CODE_TIMEOUT
        ))),
    }
}

/// Map an input-path error to a tool error, attaching the structured
/// foreground `code` (`foreground_unavailable` / `foreground_timeout`) when the
/// message carries one so the caller can branch instead of parsing prose.
pub(super) fn input_error_result(e: anyhow::Error) -> ToolResult {
    let text = e.to_string();
    if crate::input::is_uinput_unavailable(&e) {
        // The focus-free real-input route exists but this process cannot open
        // /dev/uinput: an honest refusal the operator can act on, not a bare
        // I/O error.
        let mut refusal = with_uinput_hint(crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
        ));
        if let Some(structured) = refusal.structured_content.as_mut() {
            structured["cause"] = json!(crate::input::UINPUT_UNAVAILABLE_CODE);
            structured["detail"] = json!(text);
        }
        return refusal;
    }
    match crate::input::foreground::error_code(&e) {
        Some(code) => ToolResult::error(text.clone()).with_structured(json!({
            "code": code,
            "detail": text,
            "effect": "none",
            "suggestion": if code == crate::input::foreground::CODE_TIMEOUT {
                "Take a screenshot (get_desktop_state) to confirm the desktop is responsive, then retry once."
            } else {
                "Call bring_to_front on the window (or its modal dialog), confirm with get_window_state, then retry."
            },
        })),
        None => linux_input_error(e),
    }
}

/// Structured payload for a completed foreground transaction.
pub(super) fn foreground_structured(
    path: &str,
    report: crate::input::ForegroundReport,
    extra: serde_json::Map<String, Value>,
) -> Value {
    let mut v = json!({
        "path": path,
        "verified": false,
        "delivery_mode": "foreground",
        "effect": "unverifiable",
    });
    apply_foreground_report(&mut v, &report);
    for (k, val) in extra {
        v[k] = val;
    }
    v
}

/// Fold a foreground transaction's post-check into a result: the report
/// itself, `focus_after` at the top level, and an `evidence` item so the
/// observation survives the public-record reduction. A window of the target
/// process that appeared or closed is `window_change` evidence and an
/// `effect: confirmed` wherever the focus sits afterwards (the OK button of a
/// dialog destroys the very window that held the focus, and closing the last
/// window of a process leaves the focus to the window manager); a change
/// among unattributed popups only counts while the focus stayed in the
/// process (`ForegroundReport::window_change_confirms`). Otherwise, focus that
/// left the process is a suspected no-op and anything else stays
/// unverifiable with a `native_api_result` item.
pub(super) fn apply_foreground_report(v: &mut Value, report: &crate::input::ForegroundReport) {
    v["foreground"] = report.to_json();
    v["focus_after"] = json!(report.focus_after.as_str());
    let confirming_change = report
        .window_change
        .as_ref()
        .filter(|_| report.window_change_confirms());
    let evidence = match confirming_change {
        Some(change) => {
            v["effect"] = json!("confirmed");
            v["verified"] = json!(true);
            json!({
                "kind": "window_change",
                "detail": format!("focus_after={}; {change}", report.focus_after.as_str()),
            })
        }
        None if report.focus_after == crate::input::FocusAfter::Elsewhere => {
            v["effect"] = json!("suspected_noop");
            v["warning"] = json!(
                "input focus left the target process before the post-check; the input \
                 may have reached another window. Verify with a screenshot."
            );
            return;
        }
        None => json!({
            "kind": "native_api_result",
            "detail": format!(
                "real input delivered to the activated window (focus_after={}); {}",
                report.focus_after.as_str(),
                match &report.window_change {
                    Some(change) => format!("{change}, not attributed to the target process"),
                    None => "no window change observed".to_owned(),
                }
            ),
        }),
    };
    let mut items = v
        .get("evidence")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items.push(evidence);
    v["evidence"] = json!(items);
}

/// Escalation hint for a non-AX / suspected-no-op surface — the cross-platform
/// `escalation` field mirrored from macOS. The recommended next rung depends on
/// the session type: X11 can pixel-target a specific window in the background,
/// so the deliberate move is an element px action — click by pixel (x,y) off the
/// screenshot already in this response (`px`). Native Wayland CANNOT
/// background-target an unfocused window (libei injects to the compositor's input
/// focus — see `input::delivery`), and the pixel path itself routes through the
/// same coordinate-free AT-SPI actuation that just no-op'd, so the move there is
/// to bring the window to the foreground first (`foreground`).
pub(super) fn non_ax_escalation() -> Value {
    if crate::wayland::is_wayland() {
        json!({
            "recommended": "foreground",
            "reason": "non-AX surface on Wayland: a specific unfocused window can't be \
                       pixel-targeted in the background (libei injects to the compositor's \
                       focus). bring_to_front, then click by pixel off the screenshot \
                       with delivery_mode:\"foreground\"."
        })
    } else {
        json!({
            "recommended": "px",
            "reason": "non-AX surface — act by pixel (x,y) off the screenshot \
                       in this response (an element px action)."
        })
    }
}

/// Append an `evidence[]` item to a structured payload (the public action
/// contract keeps `evidence`, while flat diagnostic fields are reduced away).
pub(super) fn push_evidence(structured: &mut Value, item: Value) {
    match structured.get_mut("evidence").and_then(Value::as_array_mut) {
        Some(items) => items.push(item),
        None => structured["evidence"] = json!([item]),
    }
}

/// Error prefix and result code for an element token whose object or window
/// is gone.
pub(super) const STALE_ELEMENT_TOKEN: &str = "stale_element_token";

/// Typed tool error for a Linux input failure: uinput permission, an occluded
/// target, or a real-pointer delivery that did not go through. Everything
/// else keeps its full error chain in the text.
pub(super) fn linux_input_error(error: anyhow::Error) -> ToolResult {
    if crate::input::is_uinput_unavailable(&error) {
        return ToolResult::error(error.to_string()).with_structured(json!({
            "code": crate::input::UINPUT_UNAVAILABLE_CODE,
        }));
    }
    if let Some(occluded) = error.downcast_ref::<crate::input::TargetOccluded>() {
        let hint = format!(
            "The point is under window {}{}{}, which belongs to a different application, so \
             a pointer press there would hit that window instead of the target; \
             no input was sent. Either act on that window (pass its window_id{} and click / \
             press keys there, or dismiss it), click the target by element_token (an AT-SPI \
             action that needs no pointer), or pick a point of the target that is not \
             covered (list_windows gives bounds and z_index).",
            occluded.covering_window,
            if occluded.covering_title.is_empty() {
                String::new()
            } else {
                // App text inside Pincer's advice: escaped, so it cannot
                // close the quote or spell a retry instruction.
                format!(" {}", quoted_title(&occluded.covering_title))
            },
            occluded
                .covering_pid
                .map(|pid| format!(" of pid {pid}"))
                .unwrap_or_default(),
            occluded
                .covering_pid
                .map(|pid| format!(" with pid {pid}"))
                .unwrap_or_default(),
        );
        return ToolResult::error(format!("{occluded}. {hint}")).with_structured(json!({
            "code": "target_occluded",
            "effect": "refused",
            "path": crate::input::MPX_POINTER_PATH,
            "target_window": occluded.target_window,
            "covering_window": occluded.covering_window,
            "covering_title": occluded.covering_title,
            "covering_pid": occluded.covering_pid,
            "screen_point": [occluded.x, occluded.y],
            "hint": hint,
        }));
    }
    if let Some(grab) = error.downcast_ref::<crate::input::PopupKeyboardGrab>() {
        let hint = format!(
            "Keys for pid {} cannot be delivered while its {} is open: the popup's keyboard \
             grab drops keys from the virtual keyboard, and the core keyboard would go to the \
             focused application instead. Dismiss the popup first (click outside it, or \
             click an item in it with pid/window_id={}), or click the popup's item by \
             element_token after get_window_state(pid={}, window_id={}); then retry.",
            grab.pid,
            grab.popup.describe(),
            grab.popup.window,
            grab.pid,
            grab.popup.window
        );
        return ToolResult::error(format!("{grab}. {hint}")).with_structured(json!({
            "code": "popup_keyboard_grab",
            "effect": "refused",
            "verified": false,
            "popup": grab.popup.to_json(),
            "focus_owner_pid": grab.focus_owner,
            "hint": hint,
        }));
    }
    if let Some(outside) = error.downcast_ref::<crate::input::PointOutsideWindow>() {
        let (bx, by, bw, bh) = outside.bounds;
        let hint = format!(
            "x/y are window-local pixels of window {} (0..{bw} x 0..{bh}, its top-left is at \
             screen ({bx}, {by})). Pass window-local coordinates from get_window_state's \
             screenshot, or screen pixels with coordinate_frame:\"desktop\".",
            outside.target_window
        );
        return ToolResult::error(format!("{outside}. {hint}")).with_structured(json!({
            "code": "point_outside_window",
            "effect": "refused",
            "path": crate::input::MPX_POINTER_PATH,
            "target_window": outside.target_window,
            "screen_point": [outside.screen_x, outside.screen_y],
            "window_point": [outside.window_x, outside.window_y],
            "window_bounds": { "x": bx, "y": by, "width": bw, "height": bh },
            "hint": hint,
        }));
    }
    if let Some(closed) = error.downcast_ref::<crate::atspi::native::WindowClosed>() {
        return ToolResult::error(closed.to_string()).with_structured(json!({
            "code": STALE_ELEMENT_TOKEN,
            "effect": "none",
            "window_id": closed.xid,
        }));
    }
    if let Some(unusable) = error.downcast_ref::<ElementBoundsUnusable>() {
        let hint = "Re-snapshot with get_window_state and click by pixel (x/y) from the \
             screenshot, or pick a child element that reports a frame.";
        return ToolResult::error(format!("{unusable}. {hint}")).with_structured(json!({
            "code": "element_bounds_unavailable",
            "effect": "none",
            "element_index": unusable.idx,
            "hint": hint,
        }));
    }
    if let Some(failed) = error.downcast_ref::<BackgroundPointerFailed>() {
        let hint = "Click by element_token (AT-SPI action) or retry with \
             delivery_mode:\"foreground\".";
        return ToolResult::error(format!("{failed}. {hint}")).with_structured(json!({
            "code": "background_pointer_failed",
            "effect": "none",
            "path": crate::input::MPX_POINTER_PATH,
            "reason": failed.reason,
            "hint": hint,
        }));
    }
    ToolResult::error(format!("{error:#}"))
}

#[cfg(test)]
#[test]
fn uinput_failure_has_a_stable_structured_code() {
    let result = linux_input_error(crate::input::uinput_unavailable("synthetic failure"));
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("code"))
            .and_then(Value::as_str),
        Some(crate::input::UINPUT_UNAVAILABLE_CODE)
    );
}

/// Merge a background focus-guard report into a finished tool result: the
/// structured `focus_changed` / `focus_restored` / `grab_held_by` fields plus
/// one sentence of text when anything moved.
pub(super) fn attach_focus_guard(
    result: ToolResult,
    report: Option<&crate::input::FocusGuardReport>,
) -> ToolResult {
    attach_focus_guard_opts(result, report, true)
}

/// [`attach_focus_guard`] for a result that already names the open popup
/// (`describe_popup: false` leaves the guard's popup sentence out).
pub(super) fn attach_focus_guard_opts(
    mut result: ToolResult,
    report: Option<&crate::input::FocusGuardReport>,
    describe_popup: bool,
) -> ToolResult {
    let Some(report) = report else {
        return result;
    };
    let summary = report.summary_with_popup(describe_popup);
    if !summary.is_empty() {
        result.content.push(pincer_core::protocol::Content::text(
            summary.trim().to_owned(),
        ));
    }
    let mut structured = result
        .structured_content
        .take()
        .unwrap_or_else(|| json!({}));
    for (key, value) in report.to_json().as_object().into_iter().flatten() {
        structured[key] = value.clone();
    }
    if let Some(item) = report.evidence_item() {
        push_evidence(&mut structured, item);
    }
    result.structured_content = Some(structured);
    result
}

pub(super) fn unavailable_wayland_focused_input_background(
    delivery: crate::input::delivery::DeliveryMode,
    focus_free_inject_supported: bool,
) -> Option<ToolResult> {
    (crate::wayland::wayland_input_enabled()
        && !(focus_free_inject_supported && crate::wayland::is_inject_mode())
        && !delivery.is_foreground())
    .then(|| {
        crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
        )
    })
}
