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

// ── mouse_button_down / mouse_drag / mouse_button_up ────────────────────────

/// `coordinate_frame` of mouse_button_down / mouse_button_up. The shared
/// [`coordinate_frame_schema`] also names `scope`, which these tools lack.
fn held_coordinate_frame_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["window", "desktop"],
        "description": "Frame of x/y: \"window\" (default) get_window_state screenshot pixels; \
            \"desktop\" get_desktop_state pixels."
    })
}

/// `delivery_mode` of mouse_drag / mouse_button_up: they follow the mode the
/// button was pressed with, so the field can only confirm it.
fn held_delivery_mode_schema() -> Value {
    pincer_core::tool_schema::delivery_mode_schema_with(
        "Optional: the held button keeps the mode mouse_button_down used, and a different \
         value is refused.",
    )
}

pub struct MouseButtonDownTool {
    pub(super) state: Arc<ToolState>,
}
static MDOWN_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for MouseButtonDownTool {
    fn def(&self) -> &ToolDef {
        MDOWN_DEF.get_or_init(|| ToolDef {
            name: "mouse_button_down".into(),
            description: "Press and hold a mouse button at window pixel (x, y); move it with \
                mouse_drag and release it with mouse_button_up. For one complete stroke, drag \
                is simpler. x,y need a get_window_state screenshot of the window earlier in \
                the session (screenshot_context_missing otherwise). On X11, background delivery \
                sends synthetic events, which GTK, Qt, LibreOffice, Chromium and WebKitGTK \
                windows ignore, so for those it is refused with background_unavailable; pass \
                delivery_mode foreground, which activates the window and holds a real button \
                on the system pointer until mouse_button_up. In background, a point covered by \
                another app's window is refused (target_occluded); foreground raises the window \
                first, so there only a window kept above it refuses. A point outside the window \
                is refused (point_outside_window). Returns a line naming the held button and \
                point."
                .into(),
            input_schema: json!({"type":"object","required":["pid","window_id","x","y"],"properties":{
                "session": pincer_core::tool_schema::session_schema_with(
                    "Takes precedence over cursor_id."
                ),
                "cursor_id": cursor_id_schema(),
                "pid":{"type":"integer","description":"Target process ID."},
                "window_id":{"type":"integer","description":"Window id from list_windows."},
                "x":{"type":"number","description":"Pixel X in the window's latest get_window_state screenshot (0..screenshot_width), or in get_desktop_state pixels with coordinate_frame \"desktop\"."},
                "y":{"type":"number","description":"Pixel Y, in the same frame as x."},
                "coordinate_frame": held_coordinate_frame_schema(),
                "button": pincer_core::tool_schema::button_schema(),
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        if let Some(held) = self
            .state
            .mouse_hold
            .lock()
            .unwrap()
            .get(&cursor_id)
            .cloned()
        {
            return ToolResult::error(format!(
                "Cursor '{cursor_id}' already has a held mouse button. Call mouse_button_up first."
            ))
            .with_structured(mouse_hold_json(&cursor_id, Some(&held)));
        }

        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let xid = match args.opt_u64("window_id") {
            Some(v) => v,
            None => return ToolResult::error("window_id is required on Linux."),
        };
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        // Like drag: the background refusal comes before the screenshot
        // frame is consulted. Native Wayland has no focus-free press either.
        if let Some(refusal) = unavailable_held_button_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, false) {
            return refusal;
        }
        let button_name = args.str_or("button", "left");
        let button = parse_mouse_button(button_name.as_str());
        let (x, y) = match resolve_window_points(
            &self.state,
            &args,
            "mouse_button_down",
            (pid, xid),
            [(args.f64_or("x", 0.0), args.f64_or("y", 0.0))],
            None,
        )
        .await
        {
            Ok([point]) => point,
            Err(refusal) => return refusal,
        };

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        if let Ok(Ok((sx, sy))) =
            tokio::task::spawn_blocking(move || window_local_to_screen(xid, x, y)).await
        {
            overlay_glide_to_for(&cursor_id, sx, sy).await;
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::ClickPulse { x: sx, y: sy },
            );
        }

        let xi = x as i32;
        let yi = y as i32;
        let is_wl = crate::wayland::is_wayland();
        // Native Wayland presses only in the foreground (refused above
        // otherwise), so mouse_drag and mouse_button_up confirm that mode.
        let foreground = delivery.is_foreground();
        let result = if is_wl {
            // The persistent virtual pointer presses inside the exact-target
            // foreground guard click uses, and like the X11 foreground press
            // below, the hold is recorded as the press lands, or the button
            // released at once when the call already gave up.
            let cid = cursor_id.clone();
            let release_cid = cursor_id.clone();
            let (output_x, output_y) = persistent_pointer_coordinates(
                is_wl,
                xid,
                xi,
                yi,
                crate::wayland::window_local_to_output,
            );
            let state = self.state.clone();
            let held_cursor = cursor_id.clone();
            let hold = MouseHoldState {
                pid,
                xid,
                button,
                x,
                y,
                foreground,
            };
            crate::input::held_press::press_and_hold_bounded(
                "foreground mouse_button_down",
                foreground_budget(0),
                move || {
                    crate::wayland::with_target_foreground(pid, xid, || {
                        crate::wayland::persistent_vptr::press(&cid, output_x, output_y, button)
                    })
                },
                // Errors only when nothing is held for the cursor.
                move || {
                    let _ = crate::wayland::persistent_vptr::release(&release_cid, button);
                },
                move || {
                    state.mouse_hold.lock().unwrap().insert(held_cursor, hold);
                },
            )
            .await
        } else if foreground {
            // The press is the core pointer's real button, and the blocking
            // work outlives a call that stops waiting (budget, cancelled
            // request): the hold is recorded as the press lands, or the
            // button released at once when the call already gave up.
            let state = self.state.clone();
            let held_cursor = cursor_id.clone();
            let hold = MouseHoldState {
                pid,
                xid,
                button,
                x,
                y,
                foreground: true,
            };
            crate::input::held_press::press_and_hold_bounded(
                "foreground mouse_button_down",
                foreground_budget(0),
                move || {
                    crate::input::with_x11_foreground_opts(
                        xid,
                        crate::input::ForegroundOptions::from_settle_hint(80)
                            .for_pid(pid)
                            .without_observation(),
                        || {
                            // Checked after the activation raised the window,
                            // so only a window that stays above it (keep
                            // above, another app's popup) refuses the press.
                            let (sx, sy) = window_local_to_screen(xid, x, y)?;
                            let point = (sx.round() as i32, sy.round() as i32);
                            crate::input::refuse_covered_point(xid, point.0, point.1)?;
                            crate::input::send_button_xtest_desktop(Some(point), button, true)
                        },
                    )
                    .map(|((), _report)| ())
                },
                move || crate::input::release_xtest_button_if_down(button),
                move || {
                    state.mouse_hold.lock().unwrap().insert(held_cursor, hold);
                },
            )
            .await
        } else {
            tokio::task::spawn_blocking(move || {
                // The synthetic press is window-addressed and would reach a
                // covered window too; refuse it like the MPX routes do, so a
                // covered point never looks like a press the user can see.
                let (sx, sy) = window_local_to_screen(xid, x, y)?;
                crate::input::refuse_covered_point(xid, sx.round() as i32, sy.round() as i32)?;
                crate::input::send_button_down(xid, xi, yi, button)
            })
            .await
        };
        match result {
            Ok(Ok(())) => {
                let hold = MouseHoldState {
                    pid,
                    xid,
                    button,
                    x,
                    y,
                    foreground,
                };
                self.state
                    .mouse_hold
                    .lock()
                    .unwrap()
                    .insert(cursor_id.clone(), hold.clone());
                if let Ok(Ok((sx, sy))) =
                    tokio::task::spawn_blocking(move || window_local_to_screen(xid, x, y)).await
                {
                    self.state
                        .cursor_registry
                        .update_position(&cursor_id, sx, sy);
                    overlay_snap_to_for(&cursor_id, sx, sy, None);
                    crate::overlay::send_command_for(
                        cursor_id.clone(),
                        cursor_overlay::OverlayCommand::SetPressed(true),
                    );
                }
                with_held_route(
                    ToolResult::text(format!(
                        "✅ Cursor '{cursor_id}' held {} button down at ({x:.1}, {y:.1}){}.",
                        mouse_button_name(button),
                        held_route_note(foreground),
                    ))
                    .with_structured(mouse_hold_json(&cursor_id, Some(&hold))),
                    foreground,
                )
            }
            Ok(Err(e)) => held_input_error(
                e,
                &cursor_id,
                self.state.mouse_hold.lock().unwrap().get(&cursor_id),
                foreground,
            ),
            Err(e) => {
                ToolResult::error(format!("Task error: {e}")).with_structured(mouse_hold_json(
                    &cursor_id,
                    self.state.mouse_hold.lock().unwrap().get(&cursor_id),
                ))
            }
        }
    }
}

pub struct MouseDragTool {
    pub(super) state: Arc<ToolState>,
}
static MDRAG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for MouseDragTool {
    fn def(&self) -> &ToolDef {
        MDRAG_DEF.get_or_init(|| ToolDef {
            name: "mouse_drag".into(),
            description: "Move the button held by mouse_button_down to window pixel (x, y), \
                sending motion events along the way; does not release it. Acts on the held \
                button's window in the delivery mode it was pressed with (a foreground hold \
                moves the system pointer). Returns a line naming the held button and the \
                point it moved to, still held."
                .into(),
            input_schema: json!({"type":"object","required":["x","y"],"properties":{
                "session": pincer_core::tool_schema::session_schema_with(
                    "Takes precedence over cursor_id."
                ),
                "cursor_id": cursor_id_schema(),
                "pid":{"type":"integer","description":"Optional process ID; must match the held button's target."},
                "window_id":{"type":"integer","description":"Optional window id; must match the held button's target."},
                "x":{"type":"number","description":"Pixel X in the held window's latest get_window_state screenshot (0..screenshot_width), whatever coordinate_frame mouse_button_down used."},
                "y":{"type":"number","description":"Pixel Y, in the same frame as x."},
                "duration_ms":{"type":"integer","minimum":0,"maximum":10000,"description":"Total drag duration in ms. Default: 500."},
                "steps":{"type":"integer","minimum":1,"maximum":200,"description":"Intermediate motion events. Default: 20."},
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "delivery_mode": held_delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        let Some(mut hold) = self
            .state
            .mouse_hold
            .lock()
            .unwrap()
            .get(&cursor_id)
            .cloned()
        else {
            return ToolResult::error(format!(
                "No mouse button is currently held for cursor '{cursor_id}'. Call mouse_button_down first."
            ))
            .with_structured(mouse_hold_json(&cursor_id, None));
        };
        if let Some(err) = held_target_mismatch(&args, &cursor_id, &hold) {
            return err;
        }
        if let Some(err) = held_delivery_mismatch(&args, &cursor_id, &hold) {
            return err;
        }

        if let Some(refusal) = require_point_args("mouse_drag", &args) {
            return refusal;
        }
        let (to_x, to_y) = match resolve_window_points(
            &self.state,
            &args,
            "mouse_drag",
            (hold.pid, hold.xid),
            [(args.f64_or("x", 0.0), args.f64_or("y", 0.0))],
            None,
        )
        .await
        {
            Ok([point]) => point,
            Err(refusal) => return refusal,
        };

        let xid = hold.xid;

        let from_x = hold.x;
        let from_y = hold.y;
        let duration_ms = args.u64_or("duration_ms", 500);
        let steps = args.u64_or("steps", 20).max(1) as usize;
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        if let Ok(Ok((sx, sy))) =
            tokio::task::spawn_blocking(move || window_local_to_screen(xid, from_x, from_y)).await
        {
            overlay_glide_to_for(&cursor_id, sx, sy).await;
            self.state
                .cursor_registry
                .update_position(&cursor_id, sx, sy);
            overlay_snap_to_for(&cursor_id, sx, sy, None);
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::SetPressed(true),
            );
        }

        let button = hold.button;
        let foreground = hold.foreground;
        let step_delay_ms = if steps > 1 {
            duration_ms / steps as u64
        } else {
            duration_ms
        };
        let mut result: anyhow::Result<()> = Ok(());
        let mut prev_x = from_x;
        let mut prev_y = from_y;
        let is_wl = crate::wayland::is_wayland();
        // A foreground X11 hold moves the real pointer in screen pixels:
        // resolve the window origin once for the whole motion. Native
        // Wayland moves the persistent virtual pointer in output pixels.
        let origin = if foreground && !is_wl {
            match tokio::task::spawn_blocking(move || window_local_to_screen(xid, 0.0, 0.0)).await {
                Ok(Ok(origin)) => Some(origin),
                Ok(Err(e)) => return held_input_error(e, &cursor_id, Some(&hold), foreground),
                Err(e) => {
                    return ToolResult::error(format!("Task error: {e}"))
                        .with_structured(mouse_hold_json(&cursor_id, Some(&hold)))
                }
            }
        } else {
            None
        };
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let ix = from_x + (to_x - from_x) * t;
            let iy = from_y + (to_y - from_y) * t;
            // Native Wayland: route motion through the persistent virtual-
            // pointer so the held button stays held across the entire drag;
            // X11 moves the real pointer (foreground) or posts synthetic
            // motion (background).
            let cid_inner = cursor_id.clone();
            let move_result = if is_wl {
                let (output_x, output_y) = persistent_pointer_coordinates(
                    is_wl,
                    xid,
                    ix.round() as i32,
                    iy.round() as i32,
                    crate::wayland::window_local_to_output,
                );
                tokio::task::spawn_blocking(move || {
                    crate::wayland::persistent_vptr::move_to(&cid_inner, output_x, output_y)
                })
                .await
            } else if let Some((ox, oy)) = origin {
                tokio::task::spawn_blocking(move || {
                    crate::input::send_move_xtest_desktop(
                        (ox + ix).round() as i32,
                        (oy + iy).round() as i32,
                    )
                })
                .await
            } else {
                tokio::task::spawn_blocking(move || {
                    crate::input::send_motion(
                        xid,
                        ix.round() as i32,
                        iy.round() as i32,
                        Some(button),
                    )
                })
                .await
            };
            match move_result {
                Ok(Ok(())) => {
                    if let Ok(Ok((sx, sy))) =
                        tokio::task::spawn_blocking(move || window_local_to_screen(xid, ix, iy))
                            .await
                    {
                        let heading = if (ix - prev_x).abs() > f64::EPSILON
                            || (iy - prev_y).abs() > f64::EPSILON
                        {
                            Some((iy - prev_y).atan2(ix - prev_x))
                        } else {
                            None
                        };
                        self.state
                            .cursor_registry
                            .update_position(&cursor_id, sx, sy);
                        overlay_move_to_for(&cursor_id, sx, sy, heading);
                    }
                    prev_x = ix;
                    prev_y = iy;
                    if step_delay_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(step_delay_ms)).await;
                    }
                }
                Ok(Err(e)) => {
                    result = Err(e);
                    break;
                }
                Err(e) => {
                    result = Err(anyhow::anyhow!("Task error: {e}"));
                    break;
                }
            }
        }

        match result {
            Ok(()) => {
                hold.x = to_x;
                hold.y = to_y;
                self.state
                    .mouse_hold
                    .lock()
                    .unwrap()
                    .insert(cursor_id.clone(), hold.clone());
                if let Ok(Ok((sx, sy))) =
                    tokio::task::spawn_blocking(move || window_local_to_screen(xid, to_x, to_y))
                        .await
                {
                    self.state
                        .cursor_registry
                        .update_position(&cursor_id, sx, sy);
                    overlay_snap_to_for(
                        &cursor_id,
                        sx,
                        sy,
                        Some((to_y - from_y).atan2(to_x - from_x)),
                    );
                    crate::overlay::send_command_for(
                        cursor_id.clone(),
                        cursor_overlay::OverlayCommand::ClickPulse { x: sx, y: sy },
                    );
                }
                with_held_route(
                    ToolResult::text(format!(
                        "✅ Cursor '{cursor_id}' dragged held {} button to ({to_x:.1}, {to_y:.1}){}.",
                        mouse_button_name(hold.button),
                        held_route_note(foreground),
                    ))
                    .with_structured(mouse_hold_json(&cursor_id, Some(&hold))),
                    foreground,
                )
            }
            Err(e) => held_input_error(e, &cursor_id, Some(&hold), foreground),
        }
    }
}

pub struct MouseButtonUpTool {
    pub(super) state: Arc<ToolState>,
}
static MUP_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for MouseButtonUpTool {
    fn def(&self) -> &ToolDef {
        MUP_DEF.get_or_init(|| ToolDef {
            name: "mouse_button_up".into(),
            description: "Release the button held by mouse_button_down, at window pixel (x, y) \
                or, without x/y, where it is held. Acts on the held button's window in the \
                delivery mode it was pressed with. Returns a line naming the released button \
                and the release point."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "session": pincer_core::tool_schema::session_schema_with(
                    "Takes precedence over cursor_id."
                ),
                "cursor_id": cursor_id_schema(),
                "pid":{"type":"integer","description":"Optional process ID; must match the held button's target."},
                "window_id":{"type":"integer","description":"Optional window id; must match the held button's target."},
                "x":{"type":"number","description":"Optional release X in the held window's latest get_window_state screenshot (0..screenshot_width), or in get_desktop_state pixels with coordinate_frame \"desktop\". Default: where the button is held."},
                "y":{"type":"number","description":"Optional release Y, in the same frame as x."},
                "coordinate_frame": held_coordinate_frame_schema(),
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "delivery_mode": held_delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        let Some(hold) = self
            .state
            .mouse_hold
            .lock()
            .unwrap()
            .get(&cursor_id)
            .cloned()
        else {
            return ToolResult::error(format!(
                "No mouse button is currently held for cursor '{cursor_id}'."
            ))
            .with_structured(mouse_hold_json(&cursor_id, None));
        };
        if let Some(err) = held_target_mismatch(&args, &cursor_id, &hold) {
            return err;
        }
        if let Some(err) = held_delivery_mismatch(&args, &cursor_id, &hold) {
            return err;
        }

        let xid = hold.xid;

        // A window that closed while its button was held (its app quit or
        // crashed) must not keep the button down: on the foreground route it
        // is the core pointer's real button, the user's own.
        if held_window_is_gone(xid).await {
            return release_hold_of_gone_window(&self.state, &cursor_id, &hold).await;
        }

        let (x, y) = match mouse_button_up_point(&self.state, &args, &cursor_id, &hold).await {
            Ok(point) => point,
            Err(_) if held_window_is_gone(xid).await => {
                return release_hold_of_gone_window(&self.state, &cursor_id, &hold).await;
            }
            Err(refusal) => return refusal,
        };

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        if let Ok(Ok((sx, sy))) =
            tokio::task::spawn_blocking(move || window_local_to_screen(xid, x, y)).await
        {
            overlay_glide_to_for(&cursor_id, sx, sy).await;
        }

        let button = hold.button;
        let foreground = hold.foreground;
        let xi = x as i32;
        let yi = y as i32;
        // Native Wayland: release through the persistent virtual-pointer so
        // the same vptr device that emitted the press also emits the release
        // (single logical drag rather than a click pair).
        let result = if crate::wayland::is_wayland() {
            let cid = cursor_id.clone();
            tokio::task::spawn_blocking(move || {
                crate::wayland::persistent_vptr::release(&cid, button)
            })
            .await
        } else if foreground {
            tokio::task::spawn_blocking(move || {
                let (sx, sy) = window_local_to_screen(xid, x, y)?;
                crate::input::send_button_xtest_desktop(
                    Some((sx.round() as i32, sy.round() as i32)),
                    button,
                    false,
                )
            })
            .await
        } else {
            tokio::task::spawn_blocking(move || crate::input::send_button_up(xid, xi, yi, button))
                .await
        };
        match result {
            Ok(Ok(())) => {
                if let Ok(Ok((sx, sy))) =
                    tokio::task::spawn_blocking(move || window_local_to_screen(xid, x, y)).await
                {
                    self.state
                        .cursor_registry
                        .update_position(&cursor_id, sx, sy);
                    overlay_snap_to_for(&cursor_id, sx, sy, None);
                }
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::OverlayCommand::SetPressed(false),
                );
                clear_mouse_hold_after_release(&self.state, &cursor_id);
                let mut cleared = mouse_hold_json(&cursor_id, None);
                cleared["delivery_mode"] = json!(held_delivery_mode(foreground));
                with_held_route(
                    ToolResult::text(format!(
                        "✅ Cursor '{cursor_id}' released held {} button at ({x:.1}, {y:.1}){}.",
                        mouse_button_name(button),
                        held_route_note(foreground),
                    ))
                    .with_structured(cleared),
                    foreground,
                )
            }
            // The window closed between the check above and the release.
            Ok(Err(_)) if held_window_is_gone(xid).await => {
                release_hold_of_gone_window(&self.state, &cursor_id, &hold).await
            }
            Ok(Err(e)) => held_input_error(e, &cursor_id, Some(&hold), foreground),
            Err(e) => ToolResult::error(format!("Task error: {e}"))
                .with_structured(mouse_hold_json(&cursor_id, Some(&hold))),
        }
    }
}
