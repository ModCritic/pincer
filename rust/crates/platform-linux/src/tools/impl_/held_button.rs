//! The held-button tools (mouse_button_down, mouse_drag, mouse_button_up) and
//! the hold bookkeeping they share.

use super::*;

#[cfg(test)]
#[tokio::test]
async fn coordinate_less_mouse_button_up_survives_snapshot_replacement() {
    let state = ToolState::new();
    let cursor_id = "held-after-replacement";
    let hold = MouseHoldState {
        pid: 42,
        xid: 7,
        button: 1,
        x: 120.0,
        y: 80.0,
        foreground: false,
    };
    state.snapshots.publish_for_session(
        hold.pid as i32,
        hold.xid,
        crate::atspi::snapshot::AtspiSnapshot::from_nodes(&[]),
        Some("press-owner"),
        Some(2.0),
    );
    state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(cursor_id.to_owned(), hold.clone());

    state.snapshots.publish_for_session(
        hold.pid as i32,
        hold.xid,
        crate::atspi::snapshot::AtspiSnapshot::from_nodes(&[]),
        Some("replacement-owner"),
        Some(1.0),
    );

    let release_args = json!({"_session_id": "press-owner"});
    assert_eq!(
        mouse_button_up_point(&state, &release_args, cursor_id, &hold)
            .await
            .unwrap(),
        (hold.x, hold.y),
        "coordinate-less release must use the stored native hold point"
    );

    let stale_coordinate_args = json!({"_session_id": "press-owner", "x": 60.0, "y": 40.0});
    let refusal = mouse_button_up_point(&state, &stale_coordinate_args, cursor_id, &hold)
        .await
        .expect_err("coordinate-bearing release must still reject a replaced snapshot");
    assert_eq!(
        refusal.structured_content.as_ref().unwrap()["code"],
        "screenshot_context_missing"
    );

    clear_mouse_hold_after_release(&state, cursor_id);
    assert!(state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(cursor_id.to_owned(), hold)
        .is_none());
}

#[cfg(test)]
#[tokio::test]
async fn a_lone_release_coordinate_keeps_the_held_point_on_the_other_axis() {
    let state = ToolState::new();
    let hold = MouseHoldState {
        pid: 42,
        xid: 7,
        button: 1,
        x: 120.0,
        y: 80.0,
        foreground: false,
    };
    state.snapshots.publish_for_session(
        hold.pid as i32,
        hold.xid,
        crate::atspi::snapshot::AtspiSnapshot::from_nodes(&[]),
        Some("owner"),
        Some(2.0),
    );
    let args = json!({"_session_id": "owner", "x": 30.0});
    assert_eq!(
        mouse_button_up_point(&state, &args, "lone", &hold)
            .await
            .unwrap(),
        (60.0, hold.y),
        "the held y is already in window pixels"
    );
    let both = json!({"_session_id": "owner", "x": 30.0, "y": 10.0});
    assert_eq!(
        mouse_button_up_point(&state, &both, "lone", &hold)
            .await
            .unwrap(),
        (60.0, 20.0),
        "a full point maps like the other pointer tools"
    );
}

#[cfg(test)]
#[test]
fn persistent_wayland_pointer_coordinates_include_window_origin() {
    let output = persistent_pointer_coordinates(true, 7, 10, 20, |window_id, x, y| {
        assert_eq!(window_id, 7);
        (x + 100, y + 200)
    });
    assert_eq!(output, (110, 220));

    let x11 = persistent_pointer_coordinates(false, 7, 10, 20, |_, _, _| {
        panic!("X11 coordinates must remain window-local")
    });
    assert_eq!(x11, (10, 20));
}

#[cfg(test)]
#[test]
fn session_end_releases_only_its_hold_before_discarding_state() {
    let state = ToolState::new();
    let session_id = "ending-session";
    let hold = MouseHoldState {
        pid: 42,
        xid: 7,
        button: 1,
        x: 10.0,
        y: 20.0,
        foreground: false,
    };
    let other_hold = MouseHoldState {
        pid: 43,
        xid: 8,
        button: 3,
        x: 30.0,
        y: 40.0,
        foreground: false,
    };
    {
        let mut holds = state.mouse_hold.lock().unwrap();
        holds.insert(session_id.to_owned(), hold.clone());
        holds.insert("other-session".to_owned(), other_hold.clone());
    }

    let mut released = Vec::new();
    release_mouse_hold_for_session(&state, session_id, |cursor_id, held| {
        assert!(state.mouse_hold.lock().unwrap().contains_key(cursor_id));
        released.push((cursor_id.to_owned(), held.button));
        Ok(())
    })
    .unwrap();

    assert_eq!(released, vec![(session_id.to_owned(), hold.button)]);
    let mut holds = state.mouse_hold.lock().unwrap();
    assert!(!holds.contains_key(session_id));
    assert_eq!(
        holds.get("other-session").unwrap().button,
        other_hold.button
    );
    assert!(holds.insert(session_id.to_owned(), hold).is_none());
}

#[cfg(test)]
#[test]
fn failed_session_end_release_remains_retryable_and_bounded() {
    let state = ToolState::new();
    let session_id = "failed-release-session";
    let hold = MouseHoldState {
        pid: 42,
        xid: 7,
        button: 1,
        x: 10.0,
        y: 20.0,
        foreground: false,
    };
    state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(session_id.to_owned(), hold.clone());

    let mut attempts = 0;
    let failure = release_mouse_hold_for_session(&state, session_id, |_, _| {
        attempts += 1;
        anyhow::bail!("compositor disconnected")
    })
    .expect_err("a failed compositor release must fail session cleanup");
    assert!(failure.contains("compositor disconnected"));
    assert_eq!(
        attempts, 1,
        "one cleanup pass must make one bounded attempt"
    );
    assert_eq!(
        state
            .mouse_hold
            .lock()
            .unwrap()
            .get(session_id)
            .unwrap()
            .button,
        hold.button,
        "failed cleanup must retain the hold for the next session-end retry"
    );

    release_mouse_hold_for_session(&state, session_id, |cursor_id, held| {
        attempts += 1;
        assert_eq!(cursor_id, session_id);
        assert_eq!(held.button, hold.button);
        Ok(())
    })
    .unwrap();
    assert_eq!(attempts, 2);
    assert!(state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(session_id.to_owned(), hold)
        .is_none());
}

pub(super) fn mouse_button_name(button: u8) -> &'static str {
    match button {
        3 => "right",
        2 => "middle",
        _ => "left",
    }
}

pub(super) fn mouse_hold_json(cursor_id: &str, hold: Option<&MouseHoldState>) -> Value {
    match hold {
        Some(hold) => json!({
            "cursor_id": cursor_id,
            "held": true,
            "pid": hold.pid,
            "window_id": hold.xid,
            "button": mouse_button_name(hold.button),
            "x": hold.x,
            "y": hold.y,
            "delivery_mode": held_delivery_mode(hold.foreground),
        }),
        None => json!({
            "cursor_id": cursor_id,
            "held": false,
            "pid": Value::Null,
            "window_id": Value::Null,
            "button": Value::Null,
            "x": Value::Null,
            "y": Value::Null,
        }),
    }
}

pub(super) fn held_target_mismatch(
    args: &Value,
    cursor_id: &str,
    hold: &MouseHoldState,
) -> Option<ToolResult> {
    match args.opt_u32("pid") {
        Ok(Some(pid)) if pid != hold.pid => {
            return Some(
                ToolResult::error(format!(
                    "Cursor '{cursor_id}' is holding a button for pid {}, not pid {pid}.",
                    hold.pid
                ))
                .with_structured(mouse_hold_json(cursor_id, Some(hold))),
            );
        }
        Err(err) => return Some(err.with_structured(mouse_hold_json(cursor_id, Some(hold)))),
        _ => {}
    }

    match args.opt_u64("window_id") {
        Some(xid) if xid != hold.xid => Some(
            ToolResult::error(format!(
                "Cursor '{cursor_id}' is holding a button for window_id {}, not {xid}.",
                hold.xid
            ))
            .with_structured(mouse_hold_json(cursor_id, Some(hold))),
        ),
        _ => None,
    }
}

pub(super) fn held_delivery_mode(foreground: bool) -> &'static str {
    if foreground {
        "foreground"
    } else {
        "background"
    }
}

/// mouse_drag / mouse_button_up follow the mode the button was pressed with;
/// a `delivery_mode` naming the other one is refused rather than ignored.
pub(super) fn held_delivery_mismatch(
    args: &Value,
    cursor_id: &str,
    hold: &MouseHoldState,
) -> Option<ToolResult> {
    let requested = args.get("delivery_mode").and_then(Value::as_str)?;
    let held = held_delivery_mode(hold.foreground);
    (requested != held).then(|| {
        let mut structured = mouse_hold_json(cursor_id, Some(hold));
        structured["code"] = json!("delivery_mode_mismatch");
        ToolResult::error(format!(
            "Cursor '{cursor_id}' holds its button with delivery_mode {held}, and mouse_drag \
             and mouse_button_up continue in that mode. Omit delivery_mode, or release with \
             mouse_button_up and press again with delivery_mode:\"{requested}\"."
        ))
        .with_structured(structured)
    })
}

/// `(delivery_mode=..., path=...)` for a held-button result on X11; empty on
/// native Wayland, whose persistent virtual pointer has no X11 path label.
pub(super) fn held_route_note(foreground: bool) -> String {
    if crate::wayland::is_wayland() {
        String::new()
    } else if foreground {
        " (delivery_mode=foreground, path=x11_xtest_fg)".to_owned()
    } else {
        " (delivery_mode=background, path=x11_xsendevent)".to_owned()
    }
}

/// A held-button result with its `path` added on X11.
pub(super) fn with_held_route(mut result: ToolResult, foreground: bool) -> ToolResult {
    if !crate::wayland::is_wayland() {
        if let Some(structured) = result.structured_content.as_mut() {
            structured["path"] = json!(if foreground {
                "x11_xtest_fg"
            } else {
                "x11_xsendevent"
            });
        }
    }
    result
}

/// An input error from a held-button call, typed like the other pointer
/// tools' (target_occluded, point_outside_window, foreground_unavailable),
/// with the cursor's held-button state added and `path` naming this route
/// (the shared occlusion refusal names the MPX one).
pub(super) fn held_input_error(
    error: anyhow::Error,
    cursor_id: &str,
    hold: Option<&MouseHoldState>,
    foreground: bool,
) -> ToolResult {
    let mut result = with_held_route(input_error_result(error), foreground);
    let held = mouse_hold_json(cursor_id, hold);
    let structured = result.structured_content.get_or_insert_with(|| json!({}));
    for (key, value) in held.as_object().into_iter().flatten() {
        if structured.get(key).is_none() {
            structured[key] = value.clone();
        }
    }
    result
}

/// The release point of mouse_button_up, in window pixels. Without x/y it is
/// where the button is held, read from no frame, so a release still works
/// after the window's snapshot was replaced. With them, the shared
/// [`resolve_window_points`] maps the point from the frame the call names
/// (window screenshot, zoom image or desktop), as for the other pointer
/// tools. A lone x or y keeps the held point's other coordinate, which is
/// stored in window pixels already.
pub(super) async fn mouse_button_up_point(
    state: &ToolState,
    args: &Value,
    cursor_id: &str,
    hold: &MouseHoldState,
) -> Result<(f64, f64), ToolResult> {
    let (x, y) = (args.opt_f64("x"), args.opt_f64("y"));
    if x.is_none() && y.is_none() {
        return Ok((hold.x, hold.y));
    }
    let [(mapped_x, mapped_y)] = resolve_window_points(
        state,
        args,
        "mouse_button_up",
        (hold.pid, hold.xid),
        [(x.unwrap_or(0.0), y.unwrap_or(0.0))],
        None,
    )
    .await
    .map_err(|mut refusal| {
        // The desktop-frame refusals come without structured content; they
        // keep naming the held button, as this tool's own mapping did.
        if refusal.structured_content.is_none() {
            refusal.structured_content = Some(mouse_hold_json(cursor_id, Some(hold)));
        }
        refusal
    })?;
    Ok((
        if x.is_some() { mapped_x } else { hold.x },
        if y.is_some() { mapped_y } else { hold.y },
    ))
}

/// Whether the X server says the held window `xid` no longer exists. Always
/// `false` on native Wayland, whose persistent virtual pointer releases
/// without a window, and when the server cannot be asked.
pub(super) async fn held_window_is_gone(xid: u64) -> bool {
    if crate::wayland::is_wayland() {
        return false;
    }
    matches!(
        tokio::task::spawn_blocking(move || crate::x11::window_presence(xid)).await,
        Ok(Some(crate::x11::WindowPresence::Gone))
    )
}

/// mouse_button_up for a hold whose window is gone. The foreground hold is
/// the core pointer's real XTest button: it is released where the pointer is
/// now. The background hold's synthetic press went to that window alone, so
/// no button is down anywhere. Either way the hold is cleared and the result
/// says the window was gone. (X11 has no MPX route for held buttons, and
/// native Wayland never comes here.)
pub(super) async fn release_hold_of_gone_window(
    state: &ToolState,
    cursor_id: &str,
    hold: &MouseHoldState,
) -> ToolResult {
    let (xid, button, foreground) = (hold.xid, hold.button, hold.foreground);
    let released_at = if foreground {
        match tokio::task::spawn_blocking(move || {
            crate::input::release_button_xtest_at_pointer(button)
        })
        .await
        {
            Ok(Ok(point)) => Some(point),
            Ok(Err(e)) => return held_input_error(e, cursor_id, Some(hold), foreground),
            Err(e) => {
                return ToolResult::error(format!("Task error: {e}"))
                    .with_structured(mouse_hold_json(cursor_id, Some(hold)))
            }
        }
    } else {
        None
    };
    crate::overlay::send_command_for(
        cursor_id.to_owned(),
        cursor_overlay::OverlayCommand::SetPressed(false),
    );
    clear_mouse_hold_after_release(state, cursor_id);
    let mut cleared = mouse_hold_json(cursor_id, None);
    cleared["delivery_mode"] = json!(held_delivery_mode(foreground));
    cleared["window_gone"] = json!(true);
    let name = mouse_button_name(button);
    let text = match released_at {
        Some((sx, sy)) => {
            cleared["released_at_screen"] = json!({ "x": sx, "y": sy });
            format!(
                "✅ Cursor '{cursor_id}' released held {name} button where the pointer is \
                 (screen {sx}, {sy}): window {xid} is gone, so the release could not be aimed \
                 into it{}.",
                held_route_note(foreground)
            )
        }
        None => format!(
            "✅ Cursor '{cursor_id}' cleared its held {name} button: window {xid} is gone, and \
             the background press went to that window alone, so no button was left down{}.",
            held_route_note(foreground)
        ),
    };
    with_held_route(ToolResult::text(text).with_structured(cleared), foreground)
}

pub(super) fn clear_mouse_hold_after_release(state: &ToolState, cursor_id: &str) {
    state.mouse_hold.lock().unwrap().remove(cursor_id);
}

pub(super) fn persistent_pointer_coordinates(
    is_wayland: bool,
    window_id: u64,
    x: i32,
    y: i32,
    window_local_to_output: impl FnOnce(u64, i32, i32) -> (i32, i32),
) -> (i32, i32) {
    if is_wayland {
        window_local_to_output(window_id, x, y)
    } else {
        (x, y)
    }
}

pub(super) fn release_mouse_hold_for_session<Release>(
    state: &ToolState,
    session_id: &str,
    mut release: Release,
) -> Result<(), String>
where
    Release: FnMut(&str, &MouseHoldState) -> anyhow::Result<()>,
{
    let hold = state.mouse_hold.lock().unwrap().get(session_id).cloned();
    let Some(hold) = hold else {
        return Ok(());
    };

    release(session_id, &hold).map_err(|error| error.to_string())?;
    state.mouse_hold.lock().unwrap().remove(session_id);
    Ok(())
}

#[cfg(test)]
#[test]
fn held_button_moves_only_in_its_own_delivery_mode() {
    let hold = MouseHoldState {
        pid: 1,
        xid: 2,
        button: 1,
        x: 0.0,
        y: 0.0,
        foreground: true,
    };
    assert!(held_delivery_mismatch(&json!({}), "c", &hold).is_none());
    assert!(held_delivery_mismatch(&json!({"delivery_mode": "foreground"}), "c", &hold).is_none());
    let refused =
        held_delivery_mismatch(&json!({"delivery_mode": "background"}), "c", &hold).unwrap();
    let structured = refused.structured_content.unwrap();
    assert_eq!(structured["code"], "delivery_mode_mismatch");
    assert_eq!(structured["delivery_mode"], "foreground");
    assert_eq!(structured["held"], true);
}
