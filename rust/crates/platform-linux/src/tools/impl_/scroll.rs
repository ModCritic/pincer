//! scroll: AT-SPI scrolling, wheel input and the result of each route.

use super::*;

// ── scroll ────────────────────────────────────────────────────────────────────

pub struct ScrollTool {
    pub(super) state: Arc<ToolState>,
}

fn atspi_scroll_result(progress: crate::atspi::ScrollProgress, foreground: bool) -> ToolResult {
    use pincer_core::action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
    };
    let effect = if !progress.complete && progress.acknowledged > 0 {
        ActionEffect::Partial
    } else {
        ActionEffect::Unverifiable
    };
    let mut record = ActionExecutionRecord::builder(
        effect,
        ActionTransport::LinuxAtSpiAction,
        if foreground {
            RequestedDelivery::Foreground
        } else {
            RequestedDelivery::Background
        },
    )
    .actual_delivery(if !progress.complete {
        ActualDelivery::Unknown
    } else if foreground {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Background
    });
    if progress.acknowledged > 0 {
        record = record.delivered_count(progress.acknowledged);
    }
    let text = if progress.complete {
        format!(
            "AT-SPI acknowledged {} scroll action(s); effect remains unverified.",
            progress.acknowledged
        )
    } else {
        format!("AT-SPI scroll stopped after {} acknowledged action(s): {}. Refresh state before another action.", progress.acknowledged, progress.detail.as_deref().unwrap_or("unknown outcome"))
    };
    let record = record
        .detail(text.clone())
        .build()
        .expect("AT-SPI scroll preserves acknowledged progress");
    let public = serde_json::to_value(record.public_result().unwrap()).unwrap();
    let result = if progress.complete {
        ToolResult::text(text)
    } else {
        ToolResult::error(text)
    };
    result.with_structured(public).with_action_record(record)
}

#[test]
fn atspi_scroll_outcomes_preserve_uncertainty_and_acknowledged_count() {
    use pincer_core::action_record::ActionEffect;
    for (complete, acknowledged, effect) in [
        (false, 0, ActionEffect::Unverifiable),
        (false, 1, ActionEffect::Partial),
        (true, 2, ActionEffect::Unverifiable),
    ] {
        let result = atspi_scroll_result(
            crate::atspi::ScrollProgress {
                acknowledged,
                complete,
                detail: None,
            },
            false,
        );
        let record = result.action_record.unwrap();
        assert_eq!(record.effect, effect);
        let public = serde_json::to_value(record.public_result().unwrap()).unwrap();
        assert_eq!(
            public["delivery"]["mode"],
            if complete { "background" } else { "unknown" }
        );
        assert_eq!(
            public["delivery"]["delivered_count"],
            if acknowledged == 0 {
                Value::Null
            } else {
                json!(acknowledged)
            }
        );
    }
}
static SCROLL_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for ScrollTool {
    fn def(&self) -> &ToolDef {
        SCROLL_DEF.get_or_init(|| ToolDef {
            name: "scroll".into(),
            description: "Scroll a window without stealing focus: wheel steps over the \
                element_token or x,y point, else the window's center, sent by the session's \
                virtual pointer where uinput is available, else as synthetic X events (path \
                x11_xsendevent). direction required; amount (default 3) counts wheel steps, or with \
                by:\"page\" pages of 3 wheel steps each, not screenfuls; the app decides how far a \
                step moves. On X11 the result compares the window's scroll bars from \
                the last get_window_state (a narrow read must include them, e.g. query:\"scroll \
                bar\"): an error with code no_change when nothing moved, unverifiable when no \
                scroll bar is known. Qt ignores synthetic wheel events: use delivery_mode \
                foreground, or set_value on the scroll bar row with a value in the bar's own \
                units (the number its row shows, not a fraction). Where no focus-free pointer route \
                exists (a GTK3 canvas, for example) background delivery is refused with \
                background_unavailable: retry with delivery_mode foreground. Window x,y need a \
                get_window_state screenshot of the window earlier in the session \
                (screenshot_context_missing otherwise). modifier holds keys during the wheel \
                steps (ctrl+wheel zooms in most editors, canvases and browsers); X11 only. A \
                modified scroll is not checked against the scroll bars, since it usually zooms \
                instead of scrolling: its result is unverifiable.".into(),
            input_schema: json!({
                // `pid` is conditionally required (validated in code), so only
                // `direction` is pinned — matches the scroll→["direction"] canon
                // in pincer_core::tool_schema.
                "type":"object","required":["direction"],"properties":{
                    "session": pincer_core::tool_schema::session_schema(),
                    "cursor_id": cursor_id_schema(),
                    "pid":{"type":"integer","description":"Process id. Required unless scope is \"desktop\" or element_token is given (the token carries it)."},
                    "direction":{"type":"string","enum":["up","down","left","right"],"description":"Scroll direction."},
                    "by":{"type":"string","enum":["line","page"],"description":"line (default): one wheel step per amount; page: 3 wheel steps per amount."},
                    "amount":{"type":"integer","minimum":1,"maximum":50,"description":"Wheel steps, or pages with by:\"page\". Default: 3."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                    "element_token": pincer_core::tool_schema::element_token_schema(),
                    "modifier": pincer_core::tool_schema::modifier_schema(),
                    "x":{"type":"number","description":"Window-local screenshot-pixel X of the scroll target. Pass with y and without element_token."},
                    "y":{"type":"number","description":"Window-local screenshot-pixel Y of the scroll target. Pass with x and without element_token."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with x,y and no pid/window_id for get_desktop_state screen coordinates. Default \"window\"."},
                    "coordinate_frame": coordinate_frame_schema(),
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: false, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        // Keys held around the wheel steps (ctrl+wheel to zoom). Only the X11
        // XTest and XSendEvent routes can carry them.
        let modifiers = match modifier_arg("scroll", "modifier", &args.str_array("modifier")) {
            Ok(modifiers) => modifiers,
            Err(refusal) => return refusal,
        };
        if !modifiers.is_empty() && crate::wayland::wayland_input_enabled() {
            return ToolResult::error(
                "scroll with modifier is unavailable on native Wayland: the wheel route \
                 cannot carry keyboard modifier state. Use press_key or hotkey for the \
                 app's keyboard zoom instead.",
            );
        }
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<ScrollInput>("scroll", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let direction = input.direction.as_str().to_owned();
            let (x, y) = (input.x, input.y);
            let x = x.round() as i32;
            let y = y.round() as i32;
            let amount = input.amount.unwrap_or(3).clamp(1, 50) as usize;
            let display = direction.clone();
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland { "wayland_desktop" } else { "xtest" };
            let space = match desktop_input_space().await {
                Ok(space) => space,
                Err(error) => return ToolResult::error(error.to_string()),
            };
            if named_session_cursor_key(&args).is_some() {
                let (overlay_x, overlay_y) = space.to_layout(x, y);
                reveal_pointer_action_for(
                    &self.state,
                    &cursor_id,
                    f64::from(overlay_x),
                    f64::from(overlay_y),
                    false,
                )
                .await;
            }
            let modifiers_for_task = modifiers.clone();
            let result = tokio::task::spawn_blocking(move || {
                if wayland {
                    crate::wayland::scroll_desktop(&space, x, y, &direction, amount as u32)
                } else if modifiers_for_task.is_empty() {
                    crate::input::send_scroll_xtest_desktop(x, y, &direction, amount)
                } else {
                    let refs: Vec<&str> = modifiers_for_task.iter().map(String::as_str).collect();
                    crate::input::send_click_xtest_desktop_with_modifiers(
                        x,
                        y,
                        wheel_button(&direction),
                        amount,
                        &refs,
                    )
                }
            })
            .await;
            let held = held_note(&modifiers);
            return match result {
                Ok(Ok(())) => {
                    ToolResult::text(format!("Scrolled desktop {display} × {amount}{held}."))
                        .with_structured(
                            json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                        )
                }
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let mut pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let direction = match args.require_str("direction") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let amount = args.u64_or("amount", 3).clamp(1, 50) as usize;
        let by = match serde_json::from_value::<pincer_contract::ScrollBy>(
            args.get("by").cloned().unwrap_or(json!("line")),
        ) {
            Ok(by) => by,
            Err(error) => return ToolResult::error(format!("Invalid scroll by: {error}")),
        };
        // The Linux scroll implementation doesn't pre-focus the element (X11
        // scroll buttons go to the window root), but the token still needs to
        // be validated so a stale token surfaces an error instead of silently
        // no-op'ing.
        let resolved = match self.state.snapshots.resolve(pid as i32, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let xid_opt: Option<u64> = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            pincer_core::element_token::ResolvedElement::None => args.opt_u64("window_id"),
        };

        // Resolve XID: use window_id if given, else first window for pid.
        let xid = match xid_opt {
            Some(x) => x,
            None => {
                let windows = tokio::task::spawn_blocking(move || {
                    crate::x11::list_windows(if pid == 0 { None } else { Some(pid) })
                })
                .await
                .unwrap_or_default();
                // pid omitted: the keys go to the active window, like a
                // physical keyboard would, and the action adopts its pid.
                let chosen = if pid == 0 {
                    let active = crate::x11::active_window();
                    windows
                        .iter()
                        .find(|w| Some(w.xid) == active && w.pid.is_some())
                        .or_else(|| windows.iter().find(|w| w.is_on_screen && w.pid.is_some()))
                } else {
                    // The pid's active window, else its largest on-screen
                    // toplevel: LibreOffice and GIMP own hidden/utility
                    // toplevels that a plain `first()` could pick, and an
                    // unmapped window cannot take the virtual keyboard focus.
                    let active = crate::x11::active_window();
                    windows
                        .iter()
                        .find(|w| Some(w.xid) == active)
                        .or_else(|| {
                            windows
                                .iter()
                                .filter(|w| w.is_on_screen)
                                .max_by_key(|w| u64::from(w.width) * u64::from(w.height))
                        })
                        .or_else(|| windows.first())
                };
                match chosen {
                    Some(w) => {
                        if pid == 0 {
                            pid = w.pid.unwrap_or(0);
                        }
                        w.xid
                    }
                    None => {
                        return ToolResult::error(format!(
                            "No windows found for pid {pid}. Provide window_id."
                        ))
                    }
                }
            }
        };

        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let isolated_background = isolated_hyprland_background(delivery);
        let pixel_target = match (
            args.get("x").and_then(|value| value.as_f64()),
            args.get("y").and_then(|value| value.as_f64()),
        ) {
            (Some(x), Some(y)) => {
                // Desktop-frame pixels (from get_desktop_state) map into the
                // named window. Screenshot pixels use the latest screenshot's
                // frame, like click and drag, after an unsupported native
                // background delivery refuses.
                let native_refusal = (!isolated_background && !desktop_frame_requested(&args))
                    .then(|| unavailable_wayland_focused_input_background(delivery, true))
                    .flatten();
                match resolve_window_points(
                    &self.state,
                    &args,
                    "scroll",
                    (pid, xid),
                    [(x, y)],
                    native_refusal,
                )
                .await
                {
                    Ok([point]) => Some(point),
                    Err(refusal) => return refusal,
                }
            }
            (None, None) => None,
            (Some(_), None) => return invalid_pointer_arguments("scroll", "x was given without y"),
            (None, Some(_)) => return invalid_pointer_arguments("scroll", "y was given without x"),
        };
        let resolved_element_index = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { element_index, .. } => {
                Some(*element_index)
            }
            pincer_core::element_token::ResolvedElement::None => None,
        };
        if pixel_target.is_some() && resolved_element_index.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to scroll, not both.",
            );
        }

        if named_session_cursor_key(&args).is_some() {
            let visual_target = tokio::task::spawn_blocking(move || {
                explicit_keyboard_cursor_target(pid, xid, resolved_element_index, pixel_target)
                    .or_else(|| keyboard_window_center(xid))
            })
            .await
            .ok()
            .flatten();
            if let Some((sx, sy)) = visual_target {
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(xid),
                );
                reveal_pointer_action_for(&self.state, &cursor_id, sx, sy, false).await;
            }
        }

        if !isolated_background {
            if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                return refusal;
            }
        }
        if !modifiers.is_empty()
            && (isolated_background
                || hyprland_foreground(delivery)
                || crate::wayland::is_inject_mode())
        {
            return ToolResult::error(
                "scroll with modifier needs X11 input: this route cannot carry keyboard \
                 modifier state. Use press_key or hotkey for the app's keyboard zoom instead.",
            );
        }
        // An AT-SPI scroll moves the view and cannot hold keys, so a modified
        // scroll goes to the wheel over the element instead.
        if let (pincer_core::element_token::ResolvedElement::Element { element_index, .. }, true) =
            (&resolved, modifiers.is_empty())
        {
            let idx = *element_index;
            // WebKitGTK acknowledges the AT-SPI scroll action without moving
            // the DOM scroller. On native Wayland, use a real compositor wheel
            // event at the resolved element instead of reporting a silent
            // success. Other AX targets keep their semantic route before any
            // coordinate-based compositor fallback.
            if !(crate::wayland::wayland_input_enabled() && is_webkitgtk_embedder(pid)) {
                let direction_for_ax = direction.clone();
                let ax_result = tokio::task::spawn_blocking(move || {
                    crate::atspi::scroll_element(pid, xid, idx, &direction_for_ax, amount, by)
                })
                .await;
                match ax_result {
                    Ok(Ok(progress)) => {
                        return atspi_scroll_result(progress, delivery.is_foreground())
                    }
                    Err(error) => {
                        return atspi_scroll_result(
                            crate::atspi::ScrollProgress {
                                acknowledged: 0,
                                complete: false,
                                detail: Some(error.to_string()),
                            },
                            delivery.is_foreground(),
                        )
                    }
                    Ok(Err(_)) => {}
                }
            }
        }
        if hyprland_foreground(delivery) {
            if xid_opt.is_none() {
                return foreground_hyprland_refusal(
                    "an exact window_id or window-bound element token is required",
                );
            }
            let point = if let Some(index) = resolved_element_index {
                match tokio::task::spawn_blocking(move || {
                    resolve_element_local_coords(pid, index, Some(xid))
                })
                .await
                {
                    Ok(Ok((_, x, y))) => Some((x, y)),
                    _ => {
                        return foreground_hyprland_refusal(
                            "could not resolve exact element coordinates",
                        )
                    }
                }
            } else {
                pixel_target
            };
            // `by: "page"` is the documented 3-detent approximation, not an
            // exact viewport; large requests become several wheel packets.
            let actions =
                match crate::wayland::hyprland_input::scroll_actions(point, &direction, amount, by)
                {
                    Ok(actions) => actions,
                    Err(error) => return foreground_hyprland_refusal(error.to_string()),
                };
            return foreground_hyprland_scroll(&args, pid, xid, actions).await;
        }
        if isolated_background {
            if xid_opt.is_none() {
                return isolated_hyprland_refusal(
                    "an exact window_id or window-bound element token is required",
                );
            }
            let owner = named_session_cursor_key(&args);
            let (_cancellation, dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    let point = if let Some(idx) = resolved_element_index {
                        let (_, x, y) = resolve_element_local_coords(pid, idx, Some(xid))?;
                        Some((x, y))
                    } else {
                        pixel_target
                    };
                    let actions = crate::wayland::hyprland_input::scroll_actions(
                        point, &direction, amount, by,
                    )?;
                    crate::wayland::hyprland_input::execute_scroll(
                        owner,
                        pid,
                        xid,
                        actions,
                        cancellation,
                    )
                }) {
                    Ok(dispatch) => dispatch,
                    Err(refusal) => return refusal,
                };
            return match dispatch.await {
                Ok(result) => isolated_hyprland_result(result),
                Err(error) => isolated_hyprland_task_error(error, false),
            };
        }

        if crate::wayland::is_inject_mode() {
            let Some((x, y)) = pixel_target else {
                return crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                );
            };
            let direction_for_inject = direction.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::inject_scroll(pid, xid, x, y, &direction_for_inject, amount as u32)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Scrolled {direction} {amount} ticks (focus-free via cua-compositor)."
                ))
                .with_structured(json!({
                    "verified": false,
                    "delivery_mode": if delivery.is_foreground() { "foreground" } else { "background" },
                    "route": "cua_compositor_inject"
                })),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        if crate::wayland::wayland_input_enabled() {
            if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, false) {
                return refusal;
            }
            let direction_for_wayland = direction.clone();
            let local_point = match (pixel_target, &resolved) {
                (Some(point), _) => Some(point),
                (
                    None,
                    pincer_core::element_token::ResolvedElement::Element { element_index, .. },
                ) => {
                    let idx = *element_index;
                    match tokio::task::spawn_blocking(move || {
                        resolve_element_local_coords(pid, idx, Some(xid))
                    })
                    .await
                    {
                        Ok(Ok((_resolved_xid, x, y))) => Some((x, y)),
                        Ok(Err(error)) => {
                            return ToolResult::error(format!(
                                "Could not resolve scroll element [{idx}] coordinates: {error}"
                            ))
                        }
                        Err(error) => {
                            return ToolResult::error(format!(
                                "Scroll element coordinate task failed: {error}"
                            ))
                        }
                    }
                }
                (None, pincer_core::element_token::ResolvedElement::None) => None,
            };
            let output_point = local_point.map(|(x, y)| {
                crate::wayland::window_local_to_output(xid, x.round() as i32, y.round() as i32)
            });
            // The exact-target foreground guard click uses.
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::with_target_foreground(pid, xid, || {
                    crate::wayland::scroll_focused(
                        output_point,
                        &direction_for_wayland,
                        amount as u32,
                    )
                })
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Scrolled {direction} {amount} ticks (delivery_mode=foreground)."
                ))
                .with_structured(json!({ "verified": false, "delivery_mode": "foreground" })),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        if let Some(refusal) = unavailable_webkit_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_gtk_pointer_background(pid, delivery) {
            return refusal;
        }

        // An element-addressed scroll must land over the element. The old
        // fallback used (0, 0) in the window, which can report success while
        // Chromium/GTK routes the wheel to the toplevel instead of the
        // requested scroll region.
        let element_point = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { element_index, .. } => {
                let idx = *element_index;
                match tokio::task::spawn_blocking(move || {
                    resolve_element_local_coords(pid, idx, Some(xid))
                })
                .await
                {
                    Ok(Ok((_resolved_xid, local_x, local_y))) => {
                        let screen = window_local_to_screen(xid, local_x, local_y).ok();
                        Some(((local_x, local_y), screen))
                    }
                    Ok(Err(e)) => {
                        return ToolResult::error(format!(
                            "Could not resolve scroll element [{idx}] coordinates: {e}"
                        ))
                    }
                    Err(e) => {
                        return ToolResult::error(format!(
                            "Scroll element coordinate task failed: {e}"
                        ))
                    }
                }
            }
            pincer_core::element_token::ResolvedElement::None => pixel_target.map(|local| {
                let screen = window_local_to_screen(xid, local.0, local.1).ok();
                (local, screen)
            }),
        };

        // X11 scroll buttons: 4=up, 5=down, 6=left, 7=right
        let button = wheel_button(&direction);
        // X11 has no page step: a page is WHEEL_STEPS_PER_PAGE wheel steps,
        // the approximation the Hyprland route documents.
        let steps = match by {
            pincer_contract::ScrollBy::Page => amount * WHEEL_STEPS_PER_PAGE,
            _ => amount,
        };
        // The window's scroll bars as the last snapshot indexed them, read
        // before and after: the only way to tell a scroll from wheel events
        // the toolkit ignored.
        // A modified wheel usually zooms: the scroll bars say nothing about it.
        let before = if crate::wayland::wayland_input_enabled() || !modifiers.is_empty() {
            Vec::new()
        } else {
            tokio::task::spawn_blocking(move || crate::atspi::scroll_bar_ranges(pid, xid))
                .await
                .unwrap_or_default()
        };
        let cursor_id_for_task = cursor_id.clone();
        let modifiers_for_task = modifiers.clone();
        // Native Wayland returned above: from here on the route is X11.
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<&'static str> {
            let modifier_refs: Vec<&str> = modifiers_for_task.iter().map(String::as_str).collect();
            // foreground: activate the window, then scroll, then restore — for
            // surfaces that only route wheel events to the active window.
            let x11_scroll = || -> anyhow::Result<&'static str> {
                // X11: synthetic Button4-7 XSendEvents are dropped by XInput2
                // toolkits (GTK never scrolls, Qt ignores core wheel buttons
                // once XInput 2.1 is up). On a real Xorg host, drive a real
                // wheel detent through the MPX uinput pointer over the window's
                // center — libinput turns it into the XI2 smooth-scroll GTK reads —
                // without stealing focus. Falls back to the legacy XSendEvent
                // Button4-7 path on Xvfb / unsupported servers.
                // Button → axis/sign: 4=up(+v) 5=down(-v) 6=left(-h) 7=right(+h).
                // The virtual pointer has no keyboard: a modified scroll goes
                // as XSendEvent wheel buttons with the modifier state set.
                if modifier_refs.is_empty() && crate::input::real_pointer_input_available() {
                    let pointer_point = element_point
                        .and_then(|(_, screen)| screen)
                        .map(|(x, y)| (x as i32, y as i32))
                        .or_else(|| window_screen_center(xid).ok());
                    if let Some((cx, cy)) = pointer_point {
                        let horizontal = matches!(button, 6 | 7);
                        let ticks = match button {
                            4 | 7 => steps as i32,
                            _ => -(steps as i32), // 5 (down) and 6 (left)
                        };
                        match crate::input::send_virtual_pointer_scroll(
                            &cursor_id_for_task,
                            &crate::input::VirtualPointerScroll {
                                x: cx,
                                y: cy,
                                horizontal,
                                ticks,
                            },
                        ) {
                            Ok(()) => return Ok(crate::input::MPX_POINTER_PATH),
                            Err(error) if crate::input::is_uinput_unavailable(&error) => {
                                return Err(error)
                            }
                            Err(e) => tracing::warn!("MPX scroll fell back to XSendEvent: {e}"),
                        }
                    }
                }
                // Over the element or point, else the window's center (the
                // window's top-left corner is usually its menu bar).
                let (local_x, local_y) = element_point
                    .map(|(local, _)| local)
                    .or_else(|| {
                        window_screen_center(xid).ok().and_then(|(cx, cy)| {
                            desktop_to_window_local(xid, f64::from(cx), f64::from(cy)).ok()
                        })
                    })
                    .unwrap_or((0.0, 0.0));
                crate::input::send_click_with_modifiers(
                    xid,
                    local_x as i32,
                    local_y as i32,
                    steps,
                    button,
                    &modifier_refs,
                )
                .map(|()| "x11_xsendevent")
            };
            if delivery.is_foreground() {
                let point = element_point
                    .and_then(|(_, screen)| screen)
                    .or_else(|| {
                        window_screen_center(xid)
                            .ok()
                            .map(|(x, y)| (x as f64, y as f64))
                    })
                    .ok_or_else(|| anyhow::anyhow!("could not resolve foreground scroll point"))?;
                crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_click_xtest_desktop_with_modifiers(
                        point.0 as i32,
                        point.1 as i32,
                        button,
                        steps,
                        &modifier_refs,
                    )
                })
                .map(|()| "x11_xtest_fg")
            } else {
                x11_scroll()
            }
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        match result {
            Ok(Ok(path)) if !modifiers.is_empty() => modified_scroll_result(
                &direction,
                steps,
                &modifiers,
                path,
                mode_label,
                path == "x11_xsendevent" && synthetic_pointer_is_dropped(pid),
            ),
            Ok(Ok(path)) => {
                let after = if before.is_empty() {
                    Vec::new()
                } else {
                    // Let the toolkit apply the events before reading back.
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    tokio::task::spawn_blocking(move || crate::atspi::scroll_bar_ranges(pid, xid))
                        .await
                        .unwrap_or_default()
                };
                let synthetic_wheel_ignored =
                    path == "x11_xsendevent" && synthetic_pointer_is_dropped(pid);
                scroll_outcome_result(
                    &direction,
                    amount,
                    by,
                    steps,
                    path,
                    mode_label,
                    scroll_movement(&before, &after, &direction),
                    synthetic_wheel_ignored,
                )
            }
            Ok(Err(e)) => input_error_result(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// X11 wheel button for a scroll direction: 4 up, 5 down, 6 left, 7 right.
fn wheel_button(direction: &str) -> u8 {
    match direction {
        "up" => 4,
        "left" => 6,
        "right" => 7,
        _ => 5,
    }
}

/// " with ctrl+shift held" for a result line, or nothing.
fn held_note(modifiers: &[String]) -> String {
    if modifiers.is_empty() {
        String::new()
    } else {
        format!(" with {} held", modifiers.join("+"))
    }
}

#[cfg(test)]
#[test]
fn modified_scroll_reports_unverifiable_with_the_held_keys() {
    let modifiers = vec!["ctrl".to_owned()];
    let result = modified_scroll_result("up", 4, &modifiers, "x11_xtest_fg", "foreground", false);
    assert!(!result.is_error.unwrap_or(false));
    let text = format!("{:?}", result.content);
    assert!(text.contains("Sent 4 wheel steps up with ctrl held"), "{text}");
    let structured = result.structured_content.expect("structured result");
    assert_eq!(structured["effect"], "unverifiable");
    assert_eq!(structured["verified"], false);
    assert_eq!(structured["modifier"], json!(["ctrl"]));
    assert_eq!(structured["path"], "x11_xtest_fg");

    let ignored = modified_scroll_result("down", 1, &modifiers, "x11_xsendevent", "background", true);
    let text = format!("{:?}", ignored.content);
    assert!(text.contains("1 wheel step down"), "{text}");
    assert!(text.contains("retry with delivery_mode"), "{text}");
}

#[cfg(test)]
#[test]
fn wheel_buttons_match_x11_numbering() {
    assert_eq!(
        ["up", "down", "left", "right"].map(wheel_button),
        [4, 5, 6, 7]
    );
    assert_eq!(held_note(&[]), "");
    assert_eq!(held_note(&["ctrl".to_owned(), "shift".to_owned()]), " with ctrl+shift held");
}

/// Result of a wheel scroll sent with keys held. Ctrl+wheel zooms in most
/// apps, so the scroll bars are no evidence either way: report it sent and
/// unverified, and name the toolkits that drop the synthetic route.
fn modified_scroll_result(
    direction: &str,
    steps: usize,
    modifiers: &[String],
    path: &str,
    mode_label: &str,
    synthetic_wheel_ignored: bool,
) -> ToolResult {
    let what = format!("{steps} wheel step{}", if steps == 1 { "" } else { "s" });
    let mut text = format!(
        "Sent {what} {direction}{} (delivery_mode={mode_label}, path={path}); not verified: \
         a modified wheel usually zooms or does something other than scroll, so check the \
         window with get_window_state.",
        held_note(modifiers)
    );
    if synthetic_wheel_ignored {
        text.push_str(
            " This toolkit (Qt, GTK or LibreOffice) ignores synthetic wheel events, so this \
             most likely did nothing: retry with delivery_mode:\"foreground\".",
        );
    }
    ToolResult::text(text).with_structured(json!({
        "verified": false,
        "effect": "unverifiable",
        "delivery_mode": mode_label,
        "path": path,
        "modifier": modifiers,
    }))
}

/// Wheel steps for one `by: "page"` step on X11 (Hyprland's documented
/// approximation).
pub(super) const WHEEL_STEPS_PER_PAGE: usize = 3;

/// What a wheel scroll did to the window's scroll bars.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum ScrollMovement {
    /// No scroll bar of the window was readable: nothing to compare.
    Unknown,
    /// A scroll bar moved from the first value to the second.
    Moved(f64, f64),
    /// No scroll bar moved. `at_end`: one already sat at the end the wheel
    /// pushes toward, so there was nowhere to go.
    Still { value: f64, at_end: bool },
}

pub(super) fn scroll_movement(
    before: &[Option<(f64, f64, f64)>],
    after: &[Option<(f64, f64, f64)>],
    direction: &str,
) -> ScrollMovement {
    let pairs: Vec<_> = before
        .iter()
        .zip(after)
        .filter_map(|(b, a)| Some(((*b)?, (*a)?)))
        .collect();
    let Some(first) = pairs.first() else {
        return ScrollMovement::Unknown;
    };
    if let Some((b, a)) = pairs.iter().find(|(b, a)| (b.0 - a.0).abs() > f64::EPSILON) {
        return ScrollMovement::Moved(b.0, a.0);
    }
    let toward_max = matches!(direction, "down" | "right");
    let at_end = pairs.iter().any(|((current, minimum, maximum), _)| {
        if toward_max {
            current >= maximum
        } else {
            current <= minimum
        }
    });
    ScrollMovement::Still {
        value: (first.0).0,
        at_end,
    }
}

/// The scroll result, told from what the scroll bars did. A wheel scroll
/// that Qt ignored used to read "Scrolled down 10 ticks" while nothing
/// moved (desc-test rounds 2 and 3).
#[allow(clippy::too_many_arguments)]
pub(super) fn scroll_outcome_result(
    direction: &str,
    amount: usize,
    by: pincer_contract::ScrollBy,
    steps: usize,
    path: &str,
    mode_label: &str,
    movement: ScrollMovement,
    synthetic_wheel_ignored: bool,
) -> ToolResult {
    // "3 pages" read as three screenfuls (round 2 of the first-call test:
    // 9 wheel steps moved KWrite 27 lines, about half its view), so the
    // result counts wheel steps and says what a page was.
    let what = format!("{steps} wheel step{}", if steps == 1 { "" } else { "s" });
    let page_note = match by {
        pincer_contract::ScrollBy::Page if amount == 1 => {
            format!(" The page was {WHEEL_STEPS_PER_PAGE} wheel steps, not a screenful.")
        }
        pincer_contract::ScrollBy::Page => format!(
            " Each of the {amount} pages was {WHEEL_STEPS_PER_PAGE} wheel steps, not a \
             screenful."
        ),
        _ => String::new(),
    };
    let route = format!("delivery_mode={mode_label}, path={path}");
    match movement {
        ScrollMovement::Moved(from, to) => ToolResult::text(format!(
            "Scrolled {direction} {what} ({route}); the scroll bar moved from {from} to \
             {to}.{page_note}"
        ))
        .with_structured(json!({
            "verified": true,
            "effect": "confirmed",
            "delivery_mode": mode_label,
            "path": path,
            "scroll_bar": {"before": from, "after": to},
            "evidence": [{
                "kind": "accessibility_readback",
                "detail": format!("scroll bar value {from} -> {to}"),
            }],
        })),
        ScrollMovement::Still {
            value,
            at_end: true,
        } => ToolResult::text(format!(
            "Sent {what} {direction} ({route}), but the view is already at its end: the \
             scroll bar stays at {value}.{page_note}"
        ))
        .with_structured(json!({
            "verified": true,
            "effect": "suspected_noop",
            "delivery_mode": mode_label,
            "path": path,
            "scroll_bar": {"before": value, "after": value},
        })),
        ScrollMovement::Still {
            value,
            at_end: false,
        } => {
            let why = if synthetic_wheel_ignored {
                "this toolkit (Qt, GTK or LibreOffice) ignores synthetic wheel events"
            } else {
                "the application ignored the wheel events"
            };
            ToolResult::error(format!(
                "Nothing scrolled: sent {what} {direction} ({route}), but the window's scroll \
                 bar stayed at {value}; {why}. In background, set the scroll position with \
                 set_value on the `scroll bar` row's element_token, in the bar's own units (the \
                 number its row's value shows, not a fraction), or press_key pagedown / pageup \
                 on the window (Qt takes keys only while its window is active); otherwise retry \
                 with delivery_mode:\"foreground\".\
                 {page_note}"
            ))
            .with_structured(json!({
                "code": "no_change",
                "verified": true,
                "effect": "no_change",
                "delivery_mode": mode_label,
                "path": path,
                "scroll_bar": {"before": value, "after": value},
            }))
        }
        ScrollMovement::Unknown => {
            let mut text = format!(
                "Sent {what} {direction} ({route}); not verified: no scroll bar of this \
                 window is known, so {POINTER_CHECK}.{page_note}"
            );
            if synthetic_wheel_ignored {
                text.push_str(
                    " This toolkit (Qt, GTK or LibreOffice) ignores synthetic wheel events, so \
                     this most likely did nothing: retry with delivery_mode:\"foreground\", or \
                     read the bar with query:\"scroll bar\" and set_value its row in the bar's \
                     own units.",
                );
            }
            ToolResult::text(text).with_structured(json!({
                "verified": false,
                "effect": "unverifiable",
                "delivery_mode": mode_label,
                "path": path,
            }))
        }
    }
}

// `ScreenshotTool` and `ScreenshotCompatTool` removed in PR #1692 — see the
// matching note in platform-windows/src/tools/impl_.rs. `get_window_state` is
// the canonical screenshot path (it returns one by default); the
// underlying capture machinery (XGetImage / `import` shell-out / etc.)
// stays reachable through GetWindowStateTool.
