//! drag and parallel_mouse_drag.

use super::*;

// ── drag ─────────────────────────────────────────────────────────────────────

/// Typed refusal for a modified drag on native Wayland. The Wayland pointer
/// routes (virtual pointer, libei, and the nested cua-compositor socket) do not
/// hold keyboard modifier state across a pointer gesture, so the drag is
/// refused before any input is dispatched rather than degraded to an
/// unmodified drag. The stable `code` lets callers branch without parsing text.
fn wayland_modified_drag_refusal() -> ToolResult {
    const DETAIL: &str = "the pointer route cannot carry keyboard modifier state";
    ToolResult::error(format!(
        "modified drags are unavailable on native Wayland: {DETAIL}"
    ))
    .with_structured(json!({
        "code": "modified_pointer_unavailable",
        "effect": "refused",
        "verified": false,
        "detail": DETAIL,
    }))
}

pub struct DragTool {
    pub(super) state: Arc<ToolState>,
}
static DRAG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for DragTool {
    fn has_independent_input_lane(&self, args: &Value) -> bool {
        // This opt-in branch below either uses its own leased synthetic seat
        // or refuses. It never falls back to primary-seat input. All ordinary
        // Linux routes retain the common process-wide input coordinator.
        // Socket availability can change between admission and invocation.
        // Native Hyprland window drags remain fail-closed on their own lane.
        args.opt_str("scope").as_deref() != Some("desktop")
            && !crate::input::delivery::DeliveryMode::from_args(args).is_foreground()
            && crate::wayland::wayland_input_enabled()
            && crate::wayland::hyprland::is_session()
    }

    fn def(&self) -> &ToolDef {
        DRAG_DEF.get_or_init(|| ToolDef {
            name: "drag".into(),
            description: "Press, move and release from (from_x, from_y) to (to_x, to_y) in \
                          window screenshot pixels, without stealing focus. Window points need a \
                          get_window_state screenshot of the window earlier in the session \
                          (screenshot_context_missing otherwise). In background, a point covered by \
                          another app's window is refused (target_occluded); one covered by this \
                          app's own dialog goes to the dialog. Background drags need a pointer \
                          backend some apps lack even where clicks work, so a drag can return \
                          background_unavailable; it then needs delivery_mode foreground.".into(),
            input_schema: json!({"type":"object","required":["from_x","from_y","to_x","to_y"],"properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "cursor_id": cursor_id_schema(),
                "pid":{"type":"integer","description":"Target process ID. Required unless scope is \"desktop\"."},
                "window_id":{"type":"integer","description":"Target window XID. Required unless scope is \"desktop\"."},
                "from_x":{"type":"number","description":"Start X in the window's get_window_state screenshot pixels; pass scope:\"desktop\" for get_desktop_state pixels."},
                "from_y":{"type":"number","description":"Drag start Y (same frame as from_x)."},
                "to_x":{"type":"number","description":"Drag end X (same frame as from_x)."},
                "to_y":{"type":"number","description":"Drag end Y (same frame as from_x)."},
                "duration_ms":{"type":"integer","minimum":0,"maximum":10000,"description":"Total drag duration in ms. Default: 500."},
                "steps":{"type":"integer","minimum":1,"maximum":200,"description":"Intermediate motion events. Default: 20."},
                "modifier": pincer_core::tool_schema::modifier_schema(),
                "button": pincer_core::tool_schema::button_schema(),
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with no pid/window_id for get_desktop_state screen coordinates. Default \"window\"."},
                "coordinate_frame": coordinate_frame_schema(),
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        let modifiers = match modifier_arg("drag", "modifier", &args.str_array("modifier")) {
            Ok(modifiers) => modifiers,
            Err(refusal) => return refusal,
        };
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<DragInput>("drag", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let (from_x, from_y) = (input.from_x, input.from_y);
            let (to_x, to_y) = (input.to_x, input.to_y);
            let button = parse_mouse_button(input.button.unwrap_or(ClickButton::Left).as_str());
            let duration_ms = input.duration_ms.unwrap_or(500).min(10_000);
            let steps = input.steps.unwrap_or(20).clamp(1, 200) as usize;
            let modifiers = input.modifier.unwrap_or_default();
            let wayland = crate::wayland::wayland_input_enabled();
            if wayland && !modifiers.is_empty() {
                return wayland_modified_drag_refusal();
            }
            let path = if wayland { "wayland_desktop" } else { "xtest" };
            let space = match desktop_input_space().await {
                Ok(space) => space,
                Err(error) => return ToolResult::error(error.to_string()),
            };
            // The overlay draws in layout coordinates, like the input below.
            let overlay_point = |x: f64, y: f64| {
                let (lx, ly) = space.to_layout(x.round() as i32, y.round() as i32);
                (f64::from(lx), f64::from(ly))
            };
            let (overlay_from, overlay_to) =
                (overlay_point(from_x, from_y), overlay_point(to_x, to_y));
            // Glide to the start before the real pointer jumps there; the
            // tracking below then follows the drag itself.
            reveal_pointer_action_for(
                &self.state,
                &cursor_id,
                overlay_from.0,
                overlay_from.1,
                false,
            )
            .await;
            let result = tokio::task::spawn_blocking(move || {
                if wayland {
                    crate::wayland::drag_desktop(
                        &space,
                        from_x.round() as i32,
                        from_y.round() as i32,
                        to_x.round() as i32,
                        to_y.round() as i32,
                        steps as u32,
                        duration_ms,
                        button,
                    )
                } else {
                    let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    crate::input::send_drag_xtest_desktop_with_modifiers(
                        from_x.round() as i32,
                        from_y.round() as i32,
                        to_x.round() as i32,
                        to_y.round() as i32,
                        button,
                        duration_ms,
                        steps,
                        &modifier_refs,
                    )
                }
            });
            let visual_drag = track_overlay_drag_for(
                cursor_id.clone(),
                overlay_from,
                overlay_to,
                duration_ms,
                steps,
            );
            let (result, ()) = tokio::join!(result, visual_drag);
            if matches!(&result, Ok(Ok(()))) {
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, overlay_to.0, overlay_to.1);
            }
            return match result {
                Ok(Ok(())) => ToolResult::text("Dragged on the desktop.").with_structured(
                    json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
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
        let isolated_background = isolated_hyprland_background(delivery);
        let native_refusal = if !isolated_background {
            if crate::wayland::wayland_input_enabled() && !modifiers.is_empty() {
                return wayland_modified_drag_refusal();
            }
            if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                return refusal;
            }
            if let Some(refusal) = unavailable_webkit_background(pid, delivery) {
                return refusal;
            }
            if let Some(refusal) = unavailable_gtk_pointer_background(pid, delivery) {
                return refusal;
            }
            unavailable_wayland_focused_input_background(delivery, true)
        } else {
            None
        };

        let coerce = |key: &str| -> Option<f64> {
            args.opt_f64(key)
                .or_else(|| args.opt_i64(key).map(|i| i as f64))
        };
        let from_x = match coerce("from_x") {
            Some(v) => v,
            None => {
                return invalid_pointer_arguments(
                    "drag",
                    "from_x is missing (from_x/from_y/to_x/to_y are all required)",
                )
            }
        };
        let from_y = match coerce("from_y") {
            Some(v) => v,
            None => {
                return invalid_pointer_arguments(
                    "drag",
                    "from_y is missing (from_x/from_y/to_x/to_y are all required)",
                )
            }
        };
        let to_x = match coerce("to_x") {
            Some(v) => v,
            None => return ToolResult::error("Missing: to_x"),
        };
        let to_y = match coerce("to_y") {
            Some(v) => v,
            None => return ToolResult::error("Missing: to_y"),
        };

        let duration_ms = args.u64_or("duration_ms", 500);
        let steps = args.u64_or("steps", 20) as usize;
        let button_str = args.str_or("button", "left");
        let button = parse_mouse_button(button_str.as_str());
        let [(from_x, from_y), (to_x, to_y)] = match resolve_window_points(
            &self.state,
            &args,
            "drag",
            (pid, xid),
            [(from_x, from_y), (to_x, to_y)],
            native_refusal,
        )
        .await
        {
            Ok(points) => points,
            Err(refusal) => return refusal,
        };

        if hyprland_foreground(delivery) {
            if button_str != "left" || args.get("modifier").is_some_and(|value| !value.is_null()) {
                return foreground_hyprland_refusal(
                    "foreground drag supports only an unmodified left button",
                );
            }
            if !crate::wayland::hyprland_input::enabled() {
                return foreground_hyprland_refusal(
                    "production Hyprland input plugin is unavailable",
                );
            }
            let owner = named_session_cursor_key(&args);
            let from = crate::wayland::window_local_to_output(
                xid,
                from_x.round() as i32,
                from_y.round() as i32,
            );
            let to = crate::wayland::window_local_to_output(
                xid,
                to_x.round() as i32,
                to_y.round() as i32,
            );
            let (started, acknowledged) = tokio::sync::oneshot::channel();
            let (_cancellation, mut dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_foreground_with_started(
                        owner,
                        pid,
                        xid,
                        crate::wayland::hyprland_input::Action::Drag {
                            from: (from_x, from_y),
                            to: (to_x, to_y),
                            duration_ms,
                        },
                        Some(started),
                        cancellation,
                    )
                }) {
                    Ok(dispatch) => dispatch,
                    Err(_) => {
                        return foreground_hyprland_refusal(
                            "authenticated admitted lifecycle required",
                        )
                    }
                };
            let acknowledged = acknowledged.await.is_ok();
            let result = if acknowledged {
                overlay_snap_to_for(&cursor_id, from.0 as f64, from.1 as f64, None);
                tokio::select! {
                    result = &mut dispatch => result,
                    () = track_overlay_drag_for(cursor_id.clone(), (from.0 as f64, from.1 as f64),
                        (to.0 as f64, to.1 as f64), duration_ms, steps) => dispatch.await,
                }
            } else {
                dispatch.await
            };
            return hyprland_input_result(
                match result {
                    Ok(result) => result,
                    Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                        error.into(),
                        u32::from(acknowledged),
                    )),
                },
                true,
            );
        }

        if isolated_background {
            if button_str != "left" || args.get("modifier").is_some_and(|value| !value.is_null()) {
                return isolated_hyprland_refusal(
                    "isolated drag supports only an unmodified left button",
                );
            }
            let owner = named_session_cursor_key(&args);
            let from = crate::wayland::window_local_to_output(
                xid,
                from_x.round() as i32,
                from_y.round() as i32,
            );
            let to = crate::wayland::window_local_to_output(
                xid,
                to_x.round() as i32,
                to_y.round() as i32,
            );
            let (started, acknowledged) = tokio::sync::oneshot::channel();
            let (_cancellation, mut dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_with_started(
                        owner,
                        pid,
                        xid,
                        crate::wayland::hyprland_input::Action::Drag {
                            from: (from_x, from_y),
                            to: (to_x, to_y),
                            duration_ms,
                        },
                        Some(started),
                        cancellation,
                    )
                }) {
                    Ok(dispatch) => dispatch,
                    Err(refusal) => return refusal,
                };
            // Do not animate a drag that was refused. The compositor starts
            // the overlay clock only after accepting and pressing the button.
            let acknowledged = acknowledged.await.is_ok();
            if acknowledged {
                overlay_snap_to_for(&cursor_id, from.0 as f64, from.1 as f64, None);
                tokio::select! {
                    result = &mut dispatch => {
                        return match result { Ok(result) => isolated_hyprland_result(result), Err(error) => isolated_hyprland_task_error(error, acknowledged) };
                    }
                    () = track_overlay_drag_for(cursor_id.clone(), (from.0 as f64, from.1 as f64),
                        (to.0 as f64, to.1 as f64), duration_ms, steps) => {}
                }
            }
            return match dispatch.await {
                Ok(result) => isolated_hyprland_result(result),
                Err(error) => isolated_hyprland_task_error(error, acknowledged),
            };
        }

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        let wayland_points = crate::wayland::wayland_input_enabled().then(|| {
            (
                crate::wayland::window_local_to_output(
                    xid,
                    from_x.round() as i32,
                    from_y.round() as i32,
                ),
                crate::wayland::window_local_to_output(
                    xid,
                    to_x.round() as i32,
                    to_y.round() as i32,
                ),
            )
        });
        let screen_from = if let Some((from, _)) = wayland_points {
            Some((from.0 as f64, from.1 as f64))
        } else {
            tokio::task::spawn_blocking(move || window_local_to_screen(xid, from_x, from_y))
                .await
                .ok()
                .and_then(|result| result.ok())
        };
        if let Some((sx_from, sy_from)) = screen_from {
            overlay_glide_to_for(&cursor_id, sx_from, sy_from).await;
            self.state
                .cursor_registry
                .update_position(&cursor_id, sx_from, sy_from);
            overlay_snap_to_for(&cursor_id, sx_from, sy_from, None);
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::ClickPulse {
                    x: sx_from,
                    y: sy_from,
                },
            );
        }
        // Native Wayland: emit press + interpolated motion + release as one
        // virtual-pointer (wlroots) or libei (GNOME/KDE) sequence, output-relative
        // coords. Returns early so we don't fall into the X11 XSendEvent loop below.
        if crate::wayland::wayland_input_enabled() {
            let steps_u32 = steps as u32;
            let drag_result = if crate::wayland::is_inject_mode() {
                tokio::task::spawn_blocking(move || {
                    crate::wayland::inject_drag(
                        pid,
                        xid,
                        (from_x, from_y),
                        (to_x, to_y),
                        steps,
                        button as u32,
                    )
                })
            } else {
                let ((fxi, fyi), (txi, tyi)) = wayland_points.unwrap_or((
                    (from_x.round() as i32, from_y.round() as i32),
                    (to_x.round() as i32, to_y.round() as i32),
                ));
                // The exact-target foreground guard click uses.
                tokio::task::spawn_blocking(move || {
                    crate::wayland::with_target_foreground(pid, xid, || {
                        crate::wayland::drag_focused(
                            (fxi, fyi),
                            (txi, tyi),
                            steps_u32,
                            duration_ms,
                            button,
                        )
                    })
                })
            };
            let ((from_output_x, from_output_y), (to_output_x, to_output_y)) = wayland_points
                .unwrap_or((
                    (from_x.round() as i32, from_y.round() as i32),
                    (to_x.round() as i32, to_y.round() as i32),
                ));
            let visual_drag = track_overlay_drag_for(
                cursor_id.clone(),
                (f64::from(from_output_x), f64::from(from_output_y)),
                (f64::from(to_output_x), f64::from(to_output_y)),
                duration_ms,
                steps,
            );
            let (drag_result, ()) = tokio::join!(drag_result, visual_drag);
            if matches!(&drag_result, Ok(Ok(()))) {
                self.state.cursor_registry.update_position(
                    &cursor_id,
                    f64::from(to_output_x),
                    f64::from(to_output_y),
                );
            }
            return match drag_result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "✅ Posted drag ({button_str}) to pid {pid} \
                     from ({from_x:.0}, {from_y:.0}) → ({to_x:.0}, {to_y:.0}) \
                     in {duration_ms}ms / {steps} steps."
                )),
                Ok(Err(e)) => input_error_result(e),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        if delivery.is_foreground() {
            let screen_points = tokio::task::spawn_blocking(move || {
                Ok::<_, anyhow::Error>((
                    window_local_to_screen(xid, from_x, from_y)?,
                    window_local_to_screen(xid, to_x, to_y)?,
                ))
            })
            .await;
            let ((screen_from_x, screen_from_y), (screen_to_x, screen_to_y)) = match screen_points {
                Ok(Ok(points)) => points,
                Ok(Err(e)) => return ToolResult::error(e.to_string()),
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            };
            let drag_result = tokio::task::spawn_blocking(move || {
                let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                // Unobserved: the result below never reports the foreground
                // observation. The agent cursor's drag runs alongside and is
                // awaited on its own.
                crate::input::with_x11_foreground_unobserved(xid, 80, || {
                    crate::input::send_drag_xtest_desktop_with_modifiers(
                        screen_from_x.round() as i32,
                        screen_from_y.round() as i32,
                        screen_to_x.round() as i32,
                        screen_to_y.round() as i32,
                        button,
                        duration_ms,
                        steps,
                        &modifier_refs,
                    )
                })
            });
            let visual_drag = track_overlay_drag_for(
                cursor_id.clone(),
                (screen_from_x, screen_from_y),
                (screen_to_x, screen_to_y),
                duration_ms,
                steps,
            );
            let (drag_result, ()) = tokio::join!(drag_result, visual_drag);
            if matches!(&drag_result, Ok(Ok(()))) {
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, screen_to_x, screen_to_y);
            }
            return match drag_result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Dragged ({button_str}) to pid {pid} from ({from_x:.0}, {from_y:.0}) \
                     to ({to_x:.0}, {to_y:.0}) in {duration_ms}ms / {steps} steps \
                     (delivery_mode=foreground, path=x11_xtest_fg); not verified: {POINTER_CHECK}."
                ))
                .with_structured(json!({
                    "path": "x11_xtest_fg",
                    "verified": false,
                    "delivery_mode": "foreground"
                })),
                Ok(Err(e)) => input_error_result(e),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // Background X11 drag. With a real Xorg server and /dev/uinput the
        // gesture is a held press + interpolated motion + release on the
        // session's MPX virtual master pointer (GTK/VCL/Qt/Chromium drop the
        // synthetic XSendEvent gesture below); the target's screen points are
        // resolved once and the agent cursor tracks the glide.
        if crate::input::real_pointer_input_available() {
            let screen_points = tokio::task::spawn_blocking(move || {
                Ok::<_, anyhow::Error>((
                    window_local_to_screen(xid, from_x, from_y)?,
                    window_local_to_screen(xid, to_x, to_y)?,
                ))
            })
            .await;
            let ((sfx, sfy), (stx, sty)) = match screen_points {
                Ok(Ok(points)) => points,
                Ok(Err(e)) => return ToolResult::error(format!("{e:#}")),
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            };
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::SetPressed(true),
            );
            let cursor_id_for_task = cursor_id.clone();
            let drag_result = tokio::task::spawn_blocking(move || {
                let (outcome, guard) =
                    crate::input::focus_guard::guarded_settled(Some(pid), || {
                        Ok(crate::input::send_virtual_pointer_drag(
                            &cursor_id_for_task,
                            &crate::input::VirtualPointerDrag {
                                target_window: xid,
                                button,
                                path: vec![
                                    (sfx.round() as i32, sfy.round() as i32),
                                    (stx.round() as i32, sty.round() as i32),
                                ],
                                duration_ms,
                                steps,
                            },
                        ))
                    })?;
                outcome.map(|mut effect| {
                    effect.focus_guard = guard;
                    effect
                })
            })
            .await;
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::SetPressed(false),
            );
            if matches!(&drag_result, Ok(Ok(_))) {
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::track_pointer_command(stx, sty),
                );
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, stx, sty);
            }
            return match drag_result {
                Ok(Ok(effect)) => {
                    let route = PointerRoute::Mpx(effect);
                    ToolResult::text(format!(
                        "Dragged ({button_str}) pid {pid} from ({from_x:.0}, {from_y:.0}) to \
                         ({to_x:.0}, {to_y:.0}) in {duration_ms}ms / {steps} steps with a real \
                         held pointer gesture {}",
                        route.text_suffix("background")
                    ))
                    .with_structured(route.structured("background"))
                }
                Ok(Err(e)) => input_error_result(e),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // Legacy target-addressed XSendEvent gesture (headless servers / no
        // uinput). GTK and LibreOffice drop it; Qt takes it (a synthetic drag
        // selected text in KWrite and in a Qt 5.15 text edit while another
        // window was active, 2026-11-11), so Qt is no longer refused here.
        if synthetic_press_is_dropped(pid) {
            return crate::input::delivery::background_unavailable_error(
                crate::input::delivery::BackgroundUnavailable::SyntheticPointerDropped,
            );
        }
        // Qt asks the window manager to activate a window that gets a press
        // (KWrite became the active window after that drag): snapshot the
        // focus before the press and restore it after the release, as the
        // synthetic click does.
        let focus_snapshot =
            tokio::task::spawn_blocking(crate::input::focus_guard::FocusSnapshot::capture)
                .await
                .ok()
                .flatten();
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::SetPressed(true),
        );
        let press_result = tokio::task::spawn_blocking(move || {
            crate::input::send_button_down(
                xid,
                from_x.round() as i32,
                from_y.round() as i32,
                button,
            )
        })
        .await;
        let mut result: anyhow::Result<()> = match press_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(anyhow::anyhow!("Task error: {e}")),
        };

        if result.is_ok() {
            let step_delay_ms = if steps > 1 {
                duration_ms / steps as u64
            } else {
                duration_ms
            };
            for i in 1..=steps {
                let t = i as f64 / steps.max(1) as f64;
                let ix = from_x + (to_x - from_x) * t;
                let iy = from_y + (to_y - from_y) * t;
                let motion_result = tokio::task::spawn_blocking(move || {
                    crate::input::send_motion(
                        xid,
                        ix.round() as i32,
                        iy.round() as i32,
                        Some(button),
                    )
                })
                .await;
                match motion_result {
                    Ok(Ok(())) => {
                        if let Ok(Ok((sx, sy))) =
                            tokio::task::spawn_blocking(move || window_local_to_screen(xid, ix, iy))
                                .await
                        {
                            self.state
                                .cursor_registry
                                .update_position(&cursor_id, sx, sy);
                            crate::overlay::send_command_for(
                                cursor_id.clone(),
                                cursor_overlay::track_pointer_command(sx, sy),
                            );
                        }
                        if step_delay_ms > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(step_delay_ms))
                                .await;
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
        }

        let release_result = tokio::task::spawn_blocking(move || {
            crate::input::send_button_up(xid, to_x.round() as i32, to_y.round() as i32, button)
        })
        .await;
        if result.is_ok() {
            result = match release_result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("Task error: {e}")),
            };
        }
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::SetPressed(false),
        );
        let guard = match focus_snapshot {
            Some(snapshot) => {
                tokio::task::spawn_blocking(move || snapshot.restore_if_changed(Some(pid)))
                    .await
                    .ok()
            }
            None => None,
        };

        if result.is_ok() {
            if let Ok(Ok((sx_to, sy_to))) =
                tokio::task::spawn_blocking(move || window_local_to_screen(xid, to_x, to_y)).await
            {
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::track_pointer_command(sx_to, sy_to),
                );
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, sx_to, sy_to);
            }
        }

        let route = PointerRoute::Synthetic(guard);
        match result {
            Ok(()) => ToolResult::text(format!(
                "Posted drag ({button_str}) to pid {pid} \
                 from ({from_x:.0}, {from_y:.0}) → ({to_x:.0}, {to_y:.0}) \
                 in {duration_ms}ms / {steps} steps {}",
                route.text_suffix("background")
            ))
            .with_structured(route.structured("background")),
            Err(e) => input_error_result(e),
        }
    }
}

pub struct ParallelMouseDragTool {
    pub(super) state: Arc<ToolState>,
}
static PMDRAG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// cua-compositor path for parallel_mouse_drag: build window-local drag paths
/// and run them as concurrent multi-cursor injections over the control socket.
/// Coordinates stay window-local (the compositor maps them per app_id), so no
/// X11 geometry/MPX is needed — the X11 path's hard blocker on Wayland.
async fn parallel_drag_inject(state: &ToolState, args: &Value) -> ToolResult {
    let Some(items) = args.get("drags").and_then(|v| v.as_array()) else {
        return ToolResult::error("drags[] is required.");
    };
    if items.len() < 2 {
        return ToolResult::error("parallel_mouse_drag requires at least two drag items.");
    }
    let mut drags = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let Some(xid) = item.get("window_id").and_then(|v| v.as_u64()) else {
            return ToolResult::error("each drag item requires window_id.");
        };
        let local: Vec<(f64, f64)> = if let Some(pts) = item.get("path").and_then(|v| v.as_array())
        {
            pts.iter()
                .filter_map(|p| {
                    let a = p.as_array()?;
                    Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
                })
                .collect()
        } else {
            let g = |k: &str| item.get(k).and_then(|v| v.as_f64());
            match (g("from_x"), g("from_y"), g("to_x"), g("to_y")) {
                (Some(fx), Some(fy), Some(tx), Some(ty)) => vec![(fx, fy), (tx, ty)],
                _ => {
                    return ToolResult::error(
                        "each drag item requires path[] or from_x/from_y/to_x/to_y.",
                    )
                }
            }
        };
        if local.len() < 2 {
            return ToolResult::error("drag path needs at least 2 points.");
        }
        let local = match parallel_drag_scale(state, args, xid) {
            Ok(scale) => scale_points(local, scale),
            Err(refusal) => return refusal,
        };
        let steps = item
            .get("steps")
            .and_then(|v| v.as_u64())
            .unwrap_or(60)
            .clamp(1, 300) as usize;
        let x_button = parse_mouse_button(
            item.get("button")
                .and_then(|v| v.as_str())
                .unwrap_or("left"),
        ) as u32;
        let app = match tokio::task::spawn_blocking(move || {
            crate::wayland::inject_target_for_window(xid)
        })
        .await
        {
            Ok(Ok(target)) => target,
            Ok(Err(error)) => return ToolResult::error(error.to_string()),
            Err(error) => return ToolResult::error(format!("Task error: {error}")),
        };
        drags.push(crate::wayland::InjectDrag {
            app_id: app,
            idx: i,
            x_button,
            path: local,
            steps,
        });
    }
    let n = drags.len();
    match tokio::task::spawn_blocking(move || crate::wayland::inject_parallel_drags(&drags)).await {
        Ok(Ok(())) => ToolResult::text(format!(
            "Ran {n} concurrent drags (multi-cursor via cua-compositor)."
        )),
        Ok(Err(e)) => ToolResult::error(e.to_string()),
        Err(e) => ToolResult::error(format!("Task error: {e}")),
    }
}

/// Why parallel_mouse_drag cannot run on this host, or `None` when it can:
/// the cua-compositor socket runs it on Wayland; elsewhere it needs the MPX
/// real-pointer route (`real_pointer_unavailable`, only probed on X11).
pub(super) fn parallel_drag_unavailable_reason(
    inject_mode: bool,
    wayland: bool,
    real_pointer_unavailable: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if inject_mode {
        return None;
    }
    if wayland {
        return Some("native Wayland without the cua-compositor inject socket".to_owned());
    }
    real_pointer_unavailable()
}

/// Window pixels per caller pixel for one parallel_mouse_drag item: 1 with
/// `native_pixels` (or a trusted caller's native window pixels), else the
/// scale of this session's latest get_window_state screenshot of the window
/// (refused without one, as click is).
pub(super) fn parallel_drag_scale(state: &ToolState, args: &Value, xid: u64) -> Result<f64, ToolResult> {
    if native_pixels_requested(args)
        || args.get(pincer_core::snapshot_store::NATIVE_WINDOW_PIXELS_ARG) == Some(&json!(true))
    {
        return Ok(1.0);
    }
    state
        .snapshots
        .screenshot_context_for_zoom(None, xid, args.get("_session_id").and_then(Value::as_str))
        .map(|(_, context)| context.scale)
}

pub(super) fn scale_points(points: Vec<(f64, f64)>, scale: f64) -> Vec<(f64, f64)> {
    points
        .into_iter()
        .map(|(x, y)| (x * scale, y * scale))
        .collect()
}

/// parallel_mouse_drag without its uinput device. Unlike the other pointer
/// tools it has no foreground route, so the advice is to draw the strokes
/// one at a time with drag.
fn parallel_uinput_refusal(reason: &str) -> ToolResult {
    let suggestion = "Draw the strokes one at a time with drag (with \
         delivery_mode:\"foreground\" if drag refuses background delivery); \
         parallel_mouse_drag has no foreground fallback.";
    ToolResult::error(format!(
        "parallel_mouse_drag needs a Linux uinput device for its virtual pointers, which is \
         unavailable here: {reason}. No input was sent. {suggestion}"
    ))
    .with_structured(json!({
        "code": crate::input::UINPUT_UNAVAILABLE_CODE,
        "effect": "refused",
        "reason": reason,
        "suggestion": suggestion,
    }))
}

#[async_trait]
impl Tool for ParallelMouseDragTool {
    fn def(&self) -> &ToolDef {
        PMDRAG_DEF.get_or_init(|| ToolDef {
            name: "parallel_mouse_drag".into(),
            description: "Run two or more mouse drags at the same time, each on its own virtual \
                pointer (an X11 MPX master pointer backed by a Linux uinput device): each item \
                presses once, moves along its whole path and releases once. Offered only on \
                hosts where that device works; if it is refused anyway (uinput_unavailable), \
                there is no foreground fallback: draw the strokes one at a time with drag. Each \
                item names its window by window_id alone (no pid) and needs its own session, \
                different from the other items'. Coordinates are pixels of that window's latest \
                get_window_state screenshot in this session (refused without one), mapped to \
                the window for you; native_pixels:true takes window pixels instead (screenshot \
                pixels divided by frame_scale) and needs no screenshot. A path is a straight segment (from_x/from_y to \
                to_x/to_y), waypoints (path), or a function fn = y(x) sampled over [x_from, x_to] \
                (fn:\"300+120*sin(x/40)\" is a sine wave; + - * / ^, sin, cos, tan, sqrt, abs, \
                exp, ln, pi, e). A start point covered by another app's window is refused \
                (target_occluded).".into(),
            input_schema: json!({"type":"object","required":["drags"],"properties":{
                "drags":{"type":"array","minItems":2,"description":"Two or more drags to run at the same time, each on its own virtual pointer.","items":{"type":"object","required":["session","window_id"],"properties":{
                    "session":{"type":"string","description":"Cursor id for this item, different from every other item's: it keys the item's virtual pointer and agent cursor."},
                    "window_id":{"type":"integer","description":"Window id from list_windows; the window is found by this id alone."},
                    "path":{"type":"array","items":{"type":"array","items":{"type":"number"}},"description":"Waypoints [[x,y],...] (at least 2) in screenshot pixels; pressed once, moved through, released once. Takes precedence over fn and from/to."},
                    "fn":{"type":"string","description":"Expression y(x) in screenshot pixels, sampled over [x_from, x_to]. Instead of from_x/to_x."},
                    "x_from":{"type":"number","description":"Domain start (screenshot x) when `fn` is used."},
                    "x_to":{"type":"number","description":"Domain end (screenshot x) when `fn` is used."},
                    "samples":{"type":"integer","minimum":2,"maximum":400,"description":"Waypoints sampled along `fn`. Default: 80."},
                    "from_x":{"type":"number","description":"Straight-segment start X (screenshot pixels)."},
                    "from_y":{"type":"number","description":"Straight-segment start Y (screenshot pixels)."},
                    "to_x":{"type":"number","description":"Straight-segment end X (screenshot pixels)."},
                    "to_y":{"type":"number","description":"Straight-segment end Y (screenshot pixels)."},
                    "button": pincer_core::tool_schema::button_schema(),
                    "duration_ms":{"type":"integer","minimum":0,"maximum":10000,"description":"Default: 1500 for fn paths, 500 for straight."},
                    "steps":{"type":"integer","minimum":1,"maximum":300,"description":"Motion sub-steps along the whole path. Default: scaled to path length."}
                },"additionalProperties":false}},
                "native_pixels": native_pixels_schema("true: every item's coordinates are window pixels (get_window_state screenshot pixels divided by frame_scale) and no screenshot is needed. Default false.")
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        // Nested cua-compositor: run the drags as concurrent multi-cursor
        // injections (window-local, no X11 MPX/geometry needed).
        if crate::wayland::is_inject_mode() {
            return parallel_drag_inject(&self.state, &args).await;
        }
        // Native Wayland without the inject socket: MPX/XI2 + uinput master
        // pointers don't exist on Wayland. Surface a typed error instead of
        // silently calling the X11 path that's guaranteed to fail.
        if crate::wayland::is_wayland() {
            return ToolResult::error(
                "parallel_mouse_drag requires the cua-compositor inject socket on Wayland \
                 (set CUA_INJECT_SOCKET to the cua-compositor control socket), \
                 or run the target under X11.",
            );
        }
        // Refuse before any device is created: the MPX route has no
        // foreground fallback, so the advice names drag instead of the
        // delivery_mode the shared uinput refusal suggests.
        match tokio::task::spawn_blocking(crate::input::real_pointer_unavailable_reason).await {
            Ok(None) => {}
            Ok(Some(reason)) => return parallel_uinput_refusal(&reason),
            Err(e) => return ToolResult::error(format!("Task error: {e}")),
        }

        let Some(items) = args.get("drags").and_then(|v| v.as_array()) else {
            return ToolResult::error("drags[] is required.");
        };
        if items.len() < 2 {
            return ToolResult::error("parallel_mouse_drag requires at least two drag items.");
        }

        let mut drags = Vec::with_capacity(items.len());
        let mut sessions = std::collections::HashSet::new();
        for item in items {
            let Some(session) = item.get("session").and_then(|v| v.as_str()) else {
                return ToolResult::error("each drag item requires session.");
            };
            // The session keys the item's virtual pointer: two items on one
            // pointer would press and move the same device.
            if !sessions.insert(session) {
                return ToolResult::error(format!(
                    "parallel_mouse_drag: two drag items share session \"{session}\"; give each \
                     item its own session, which keys its virtual pointer. No input was sent."
                ))
                .with_structured(json!({
                    "code": "invalid_arguments",
                    "effect": "refused",
                    "session": session,
                }));
            }
            let Some(xid) = item.get("window_id").and_then(|v| v.as_u64()) else {
                return ToolResult::error("each drag item requires window_id.");
            };

            // Build the window-local waypoint path from one of: an explicit
            // `path` of [x,y] points, a function `fn` (y = f(x) sampled over
            // [x_from, x_to]), or a straight from→to segment.
            let is_fn = item.get("fn").and_then(|v| v.as_str()).is_some();
            let local: Vec<(f64, f64)> = if let Some(pts) =
                item.get("path").and_then(|v| v.as_array())
            {
                let mut out = Vec::with_capacity(pts.len());
                for p in pts {
                    let a = p.as_array();
                    let (Some(px), Some(py)) = (
                        a.and_then(|a| a.first()).and_then(|v| v.as_f64()),
                        a.and_then(|a| a.get(1)).and_then(|v| v.as_f64()),
                    ) else {
                        return ToolResult::error("each `path` entry must be [x, y].");
                    };
                    out.push((px, py));
                }
                if out.len() < 2 {
                    return ToolResult::error("`path` needs at least 2 points.");
                }
                out
            } else if let Some(expr_str) = item.get("fn").and_then(|v| v.as_str()) {
                let Some(x_from) = item.get("x_from").and_then(|v| v.as_f64()) else {
                    return ToolResult::error("`fn` requires x_from.");
                };
                let Some(x_to) = item.get("x_to").and_then(|v| v.as_f64()) else {
                    return ToolResult::error("`fn` requires x_to.");
                };
                let samples = item
                    .get("samples")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(80)
                    .clamp(2, 400);
                match crate::input::sample_function(expr_str, x_from, x_to, samples) {
                    Ok(pts) => pts,
                    Err(e) => return ToolResult::error(e.to_string()),
                }
            } else {
                let coerce = |k: &str| item.get(k).and_then(|v| v.as_f64());
                match (coerce("from_x"), coerce("from_y"), coerce("to_x"), coerce("to_y")) {
                    (Some(fx), Some(fy), Some(tx), Some(ty)) => vec![(fx, fy), (tx, ty)],
                    _ => return ToolResult::error("each drag item requires either `fn`+x_from+x_to, or from_x/from_y/to_x/to_y."),
                }
            };
            // Screenshot pixels to window pixels.
            let local = match parallel_drag_scale(&self.state, &args, xid) {
                Ok(scale) => scale_points(local, scale),
                Err(refusal) => return refusal,
            };

            let button = parse_mouse_button(
                item.get("button")
                    .and_then(|v| v.as_str())
                    .unwrap_or("left"),
            );
            let duration_ms = item
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(if is_fn { 1500 } else { 500 });

            // One translate gives the window origin; the path is a pure offset.
            let origin =
                match tokio::task::spawn_blocking(move || window_local_to_screen(xid, 0.0, 0.0))
                    .await
                {
                    Ok(Ok(o)) => o,
                    Ok(Err(e)) => return ToolResult::error(e.to_string()),
                    Err(e) => return ToolResult::error(format!("Task error: {e}")),
                };
            let path: Vec<(i32, i32)> = local
                .iter()
                .map(|(lx, ly)| {
                    (
                        (origin.0 + lx).round() as i32,
                        (origin.1 + ly).round() as i32,
                    )
                })
                .collect();

            // Default sub-step count scaled to path length (smooth glide),
            // overridable via `steps`.
            let total_len: f64 = path
                .windows(2)
                .map(|w| {
                    (((w[1].0 - w[0].0) as f64).powi(2) + ((w[1].1 - w[0].1) as f64).powi(2)).sqrt()
                })
                .sum();
            let steps = item
                .get("steps")
                .and_then(|v| v.as_u64())
                .map(|s| (s as usize).clamp(1, 300))
                .unwrap_or_else(|| ((total_len / 3.0).round() as usize).clamp(24, 300));

            let start = path[0];
            self.state
                .cursor_registry
                .update_position(session, start.0 as f64, start.1 as f64);
            crate::overlay::send_command_for(
                session.to_owned(),
                cursor_overlay::OverlayCommand::PinAbove(xid),
            );

            drags.push((
                session.to_owned(),
                crate::input::VirtualPointerDrag {
                    target_window: xid,
                    button,
                    path,
                    duration_ms,
                    steps,
                },
            ));
        }

        // Glide every cursor to its drag's start together, so none jumps
        // there, then show them pressed for the drags.
        futures_util::future::join_all(drags.iter().map(|(session, drag)| {
            overlay_glide_to_for(session, drag.path[0].0 as f64, drag.path[0].1 as f64)
        }))
        .await;
        for (session, drag) in &drags {
            overlay_snap_to_for(session, drag.path[0].0 as f64, drag.path[0].1 as f64, None);
            crate::overlay::send_command_for(
                session.to_owned(),
                cursor_overlay::OverlayCommand::SetPressed(true),
            );
        }

        let drags_for_task = drags.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::input::send_parallel_virtual_pointer_drags(&drags_for_task)
        })
        .await;
        match result {
            Ok(Ok(())) => {
                for (session, drag) in &drags {
                    let n = drag.path.len();
                    let end = drag.path[n - 1];
                    let prev = drag.path[n.saturating_sub(2)];
                    self.state
                        .cursor_registry
                        .update_position(session, end.0 as f64, end.1 as f64);
                    overlay_snap_to_for(
                        session,
                        end.0 as f64,
                        end.1 as f64,
                        Some(((end.1 - prev.1) as f64).atan2((end.0 - prev.0) as f64)),
                    );
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::SetPressed(false),
                    );
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::ClickPulse {
                            x: end.0 as f64,
                            y: end.1 as f64,
                        },
                    );
                }
                ToolResult::text(format!(
                    "✅ Ran {} MPX drag gesture(s) concurrently.",
                    drags.len()
                ))
                .with_structured(json!({"count": drags.len()}))
            }
            Ok(Err(e)) => {
                for (session, _) in &drags {
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::SetPressed(false),
                    );
                }
                if crate::input::is_uinput_unavailable(&e) {
                    return parallel_uinput_refusal(&format!("{e:#}"));
                }
                linux_input_error(e)
            }
            Err(e) => {
                for (session, _) in &drags {
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::SetPressed(false),
                    );
                }
                ToolResult::error(format!("Task error: {e}"))
            }
        }
    }
}
