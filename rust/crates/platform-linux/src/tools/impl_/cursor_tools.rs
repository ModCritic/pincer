//! The pointer and agent cursor tools: get_cursor_position, move_cursor,
//! move_along_path, set_agent_cursor_enabled, set_agent_cursor_motion,
//! set_agent_cursor_theme and get_agent_cursor_state.

use super::*;

// ── get_cursor_position ───────────────────────────────────────────────────────

pub struct GetCursorPositionTool;
static GCP_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetCursorPositionTool {
    fn def(&self) -> &ToolDef {
        GCP_DEF.get_or_init(|| ToolDef {
            name: "get_cursor_position".into(),
            description:
                "Return the pointer position in desktop pixels (origin top-left). On X11 it \
                 reads the X server's pointer (`source: \"x11\"`). In XWayland mode (the \
                 default on a Wayland session) that is the Xwayland pointer, which follows the \
                 real pointer only while it is over an X11 window. With the native Wayland \
                 backend, Hyprland reports the real pointer (`source: \"compositor\"`); other \
                 compositors give only the last position this process moved the pointer to \
                 (`source: \"synthetic\"`)."
                    .into(),
            input_schema: json!({"type":"object","properties":{
                "session": pincer_core::tool_schema::session_schema()
            },"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        if let Err(result) =
            parse_typed_input::<GetCursorPositionInput>("get_cursor_position", args)
        {
            return result;
        }
        // Hyprland reports the real pointer over IPC; return it in the
        // desktop-frame coordinates that get_desktop_state and desktop actions use.
        if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
            let position = tokio::task::spawn_blocking(|| -> anyhow::Result<(i32, i32)> {
                let (x, y) = crate::wayland::hyprland::cursor_position()?;
                Ok(crate::wayland::hyprland::desktop_frame()?
                    .from_layout(x.round() as i32, y.round() as i32))
            })
            .await;
            // The synthetic registry below holds layout coordinates, not the
            // desktop frame, so a failed compositor query is an error here.
            return match position {
                Ok(Ok((x, y))) => {
                    ToolResult::text(format!("✅ Cursor at ({x}, {y}) in the desktop frame"))
                        .with_structured(json!({ "x": x, "y": y, "source": "compositor" }))
                }
                Ok(Err(e)) => ToolResult::error(format!(
                    "Could not read the Hyprland pointer position: {e:#}"
                )),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }
        // Native Wayland: there's no protocol for clients to query the real
        // global cursor position. Fall back to the synthetic registry that
        // records every `motion_absolute` this process emits.
        if crate::wayland::is_wayland() {
            return match crate::wayland::last_synth_cursor_pos() {
                Some((x, y)) => ToolResult::text(
                    format!("✅ Cursor at ({x}, {y}) (synthetic — last move_cursor in this process)")
                ).with_structured(json!({
                    "x": x, "y": y, "source": "synthetic"
                })),
                None => ToolResult::text(
                    "Cursor position unknown on Wayland — no move_cursor has been issued in this process yet.".to_string()
                ).with_structured(json!({ "source": "synthetic", "available": false })),
            };
        }
        let result = tokio::task::spawn_blocking(|| {
            use x11rb::connection::Connection;
            use x11rb::protocol::xproto::ConnectionExt as _;
            let (conn, screen_num) = crate::x11::query_connection()?;
            let root = conn.setup().roots[screen_num].root;
            let reply = conn.query_pointer(root)?.reply()?;
            Ok::<(i32, i32), anyhow::Error>((reply.root_x as i32, reply.root_y as i32))
        })
        .await;
        match result {
            // Text format matches Swift `GetCursorPositionTool` 1:1.
            Ok(Ok((x, y))) => ToolResult::text(format!("✅ Cursor at ({x}, {y})"))
                .with_structured(json!({ "x": x, "y": y, "source": "x11" })),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── move_cursor ───────────────────────────────────────────────────────────────

pub struct MoveCursorTool {
    pub(super) state: Arc<ToolState>,
}

static MCURSOR_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CursorControlScope {
    Agent,
    Desktop,
}

pub(super) fn cursor_control_scope(args: &Value) -> CursorControlScope {
    match args.get("scope").and_then(Value::as_str) {
        Some("desktop") => CursorControlScope::Desktop,
        _ => CursorControlScope::Agent,
    }
}

#[async_trait]
impl Tool for MoveCursorTool {
    fn def(&self) -> &ToolDef {
        MCURSOR_DEF.get_or_init(|| ToolDef {
            name: "move_cursor".into(),
            description: "Move the synthetic agent cursor without changing the user's pointer. Only an explicit scope=desktop request moves the real OS pointer in get_desktop_state coordinates.".into(),
            input_schema: json!({"type":"object","required":["x","y"],"properties":{
                "x":{"type":"number","description":"Destination X in screen pixels (list_windows bounds, full-size get_desktop_state pixels) in both scopes. Unlike click, drag and the other window pointer tools, move_cursor never takes window screenshot pixels, and a window `target` does not convert x,y."},
                "y":{"type":"number","description":"Destination Y, in the same space as x."},
                "session": pincer_core::tool_schema::session_schema(),
                "cursor_id":{"type":"string","description":"Cursor instance to move when no `session` is given; with `session`, the session label names the cursor. Default 'default'."},
                "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"What moves: \"window\" (default) only the agent cursor overlay, leaving the user's pointer alone; \"desktop\" the real OS pointer. A `target` of kind window or desktop selects the same scope."}
            },"additionalProperties":false}),
            read_only: false, destructive: false, idempotent: true, open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        if cursor_control_scope(&args) == CursorControlScope::Desktop {
            let input = match parse_typed_projection::<MoveCursorInput>("move_cursor", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let (x, y) = (input.x, input.y);
            let xi = x.round() as i32;
            let yi = y.round() as i32;
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland {
                "wayland_desktop"
            } else {
                "xtest_desktop"
            };
            let space = match desktop_input_space().await {
                Ok(space) => space,
                Err(error) => return ToolResult::error(error.to_string()),
            };
            // Glide the agent cursor to where the real pointer is about to
            // jump. The overlay draws in layout coordinates, like the input.
            let (overlay_x, overlay_y) = space.to_layout(xi, yi);
            reveal_pointer_action_for(
                &self.state,
                &resolve_cursor_key(&args),
                f64::from(overlay_x),
                f64::from(overlay_y),
                false,
            )
            .await;
            let result = if wayland {
                tokio::task::spawn_blocking(move || {
                    crate::wayland::move_cursor_desktop(&space, xi, yi)
                })
                .await
            } else {
                tokio::task::spawn_blocking(move || crate::input::send_move_xtest_desktop(xi, yi))
                    .await
            };
            return match result {
                Ok(Ok(())) => {
                    // On X11 the server reports the pointer XTest just moved;
                    // Wayland gives clients no such query.
                    let read_back = if crate::wayland::is_wayland() {
                        None
                    } else {
                        tokio::task::spawn_blocking(current_pointer_position)
                            .await
                            .ok()
                            .flatten()
                            .map(|(rx, ry)| (rx.round() as i32, ry.round() as i32))
                    };
                    let mut structured = json!({
                        "scope":"desktop","path":path,"x":xi,"y":yi,"effect":"unverifiable"
                    });
                    let text = match read_back {
                        Some((rx, ry)) if (rx, ry) == (xi, yi) => {
                            structured["effect"] = json!("confirmed");
                            structured["evidence"] = json!([{
                                "kind": "value_readback",
                                "detail": format!("the X server reports the pointer at ({rx}, {ry})"),
                            }]);
                            format!("Moved the real desktop pointer to ({xi}, {yi}); it reads back there.")
                        }
                        Some((rx, ry)) => {
                            structured["read_back"] = json!({"x": rx, "y": ry});
                            format!(
                                "Sent the real desktop pointer to ({xi}, {yi}), but it reads back \
                                 at ({rx}, {ry}); a point off the screen stops at its edge."
                            )
                        }
                        None => format!("Moved the real desktop pointer to ({xi}, {yi})."),
                    };
                    ToolResult::text(text).with_structured(structured)
                }
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let x = args.f64_or("x", 0.0);
        let y = args.f64_or("y", 0.0);
        let cursor_id = resolve_cursor_key(&args);
        // End pointing upper-left (45°) — matches Swift's
        // `AgentCursor.animateAndWait(endAngleDegrees: 45)` convention so the
        // overlay arrow settles to the natural macOS-style pose.
        // Use the acknowledged animation path so a first-ever move seeds and
        // displays the session cursor just as reliably as a coordinate click.
        reveal_pointer_action_for(&self.state, &cursor_id, x, y, false).await;
        ToolResult::text(format!(
            "Agent cursor '{cursor_id}' moved to ({x:.1}, {y:.1}); the user pointer was unchanged."
        ))
    }
}

// ── move_along_path ───────────────────────────────────────────────────────────

/// Glides the real pointer along a sampled path (see `crate::input::path`).
/// On X11, delivery is XTest, the route `move_cursor` scope=desktop and
/// foreground drags use; the session's MPX virtual pointer is never used,
/// because the point of this tool is the visible system pointer. On Wayland
/// it is the route of `move_cursor` scope=desktop (the wlroots virtual
/// pointer, else libei), for desktop scope only. Both share the timing of
/// `crate::input::run_pointer_path`.
pub struct MoveAlongPathTool {
    pub(super) state: Arc<ToolState>,
}

static MOVE_ALONG_PATH_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn path_point_schema(description: &str) -> Value {
    json!({
        "type": "array",
        "items": {"type": "number"},
        "minItems": 2,
        "maxItems": 2,
        "description": description
    })
}

fn move_along_path_schema() -> Value {
    use crate::input::path::{
        MAX_DURATION_MS, MAX_POINTS, MAX_SAMPLES, MAX_SEGMENTS, MAX_SPACING_PX, MIN_SPACING_PX,
    };
    let point = |description: &str| path_point_schema(description);
    let segment = |properties: Value, required: Value, description: &str| {
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
            "description": description
        })
    };
    json!({"type":"object","properties":{
        "session": pincer_core::tool_schema::session_schema(),
        "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"\"window\" (default): window_id's get_window_state screenshot pixels; the window is raised first. \"desktop\": desktop pixels, no pid/window_id: get_desktop_state pixels, or list_windows bounds plus window-local offsets (the source when get_desktop_state is unavailable, as in XWayland mode)."},
        "pid":{"type":"integer","description":"Target process ID. Required for window scope."},
        "window_id":{"type":"integer","description":"Target window ID. Required for window scope."},
        "start": point("First point [x, y] of `segments`."),
        "segments":{"type":"array","minItems":1,"maxItems":MAX_SEGMENTS,"description":"Pieces after `start`, each with one key: line, quad, cubic, or arc (from the current point around center, which sets the radius).","items":{
            "type":"object",
            "minProperties":1,
            "maxProperties":1,
            "additionalProperties":false,
            "properties":{
                "line": segment(json!({"to": point("End point.")}), json!(["to"]), "Straight line to `to`."),
                "quad": segment(json!({"control": point("Control point."), "to": point("End point.")}), json!(["control","to"]), "Quadratic Bezier curve."),
                "cubic": segment(json!({"control1": point("First control point."), "control2": point("Second control point."), "to": point("End point.")}), json!(["control1","control2","to"]), "Cubic Bezier curve."),
                "arc": segment(json!({"center": point("Center of the arc."), "sweep_deg": {"type":"number","description":"Degrees to turn; positive is clockwise on screen."}}), json!(["center","sweep_deg"]), "Circular arc around `center`.")
            }
        }},
        "points":{"type":"array","minItems":2,"maxItems":MAX_POINTS,"items": point("Point [x, y]."),"description":"Polyline [[x, y], ...], instead of segments."},
        "smooth":{"type":"boolean","description":"With `points`: a Catmull-Rom curve through the points instead of straight spans. Default false."},
        "parametric":{"type":"object","required":["x","y","t_from","t_to"],"additionalProperties":false,"description":"x(t), y(t) for t from t_from to t_to (circles, loops, spirals). Supports + - * / ^, sin, cos, tan, sqrt, abs, exp, ln, pi, e.","properties":{
            "x":{"type":"string","description":"x(t), e.g. \"1280+300*cos(t)\"."},
            "y":{"type":"string","description":"y(t), e.g. \"720+300*sin(t)\"."},
            "t_from":{"type":"number","description":"First value of t."},
            "t_to":{"type":"number","description":"Last value of t."}
        }},
        // The shared `button` shape (left/right/middle); leaving it out is the
        // hover-only glide, and "none" is accepted as the same thing.
        "button":{"type":"string","enum":["left","right","middle"],"description":"Button held from the first point to the last. Omit to only move."},
        "click_at_end":{"type":"string","enum":["left","right","middle","double"],"description":"Click at the last point after the glide. Only without `button`."},
        "duration_ms":{"type":"integer","minimum":0,"maximum":MAX_DURATION_MS,"default":600,"description":"Glide time in ms. Default 600."},
        "easing":{"type":"string","enum":["linear","ease_in","ease_out","ease_in_out"],"default":"ease_in_out","description":"Speed profile; linear is even. Default ease_in_out."},
        "spacing_px":{"type":"number","minimum":MIN_SPACING_PX,"maximum":MAX_SPACING_PX,"default":4,"description":format!("Max distance between samples, in path pixels. Default 4; at most {MAX_SAMPLES} samples.")}
    },"additionalProperties":false})
}

fn move_along_path_refusal(detail: impl Into<String>) -> ToolResult {
    // Parser messages arrive with their own full stop.
    let detail = detail.into().trim_end_matches('.').to_owned();
    ToolResult::error(format!("move_along_path: {detail}. No input was sent.")).with_structured(
        json!({
            "code": "invalid_arguments",
            "tool": "move_along_path",
            "effect": "refused",
            "detail": detail,
        }),
    )
}

fn round_tenth(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[async_trait]
impl Tool for MoveAlongPathTool {
    fn def(&self) -> &ToolDef {
        MOVE_ALONG_PATH_DEF.get_or_init(|| ToolDef {
            name: "move_along_path".into(),
            description: "Move the real pointer along a path (segments, a polyline, or a \
                parametric x(t), y(t)), optionally holding a button to draw or drag, or clicking \
                at the end. With the native Wayland backend (PINCER_ENABLE_WAYLAND=1), only scope \
                \"desktop\" without pid/window_id works; XWayland mode (the default on a Wayland \
                session) takes both scopes, for X11 windows."
                .into(),
            input_schema: move_along_path_schema(),
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use crate::input::path;
        let wayland = crate::wayland::wayland_input_enabled();
        let scope = args.opt_str("scope");
        if let Some(other) = scope
            .as_deref()
            .filter(|scope| !matches!(*scope, "window" | "desktop"))
        {
            return move_along_path_refusal(format!(
                "scope must be \"window\" or \"desktop\", not \"{other}\""
            ));
        }
        // Dispatch rewrites scope:"desktop" plus pid/window_id into
        // coordinate_frame:"desktop": desktop pixels, delivered to that window.
        let desktop_pixels = scope.as_deref() == Some("desktop") || desktop_frame_requested(&args);
        let target = match (args.opt_u64("pid"), args.opt_u64("window_id")) {
            (None, None) => None,
            (Some(pid), Some(xid)) => match u32::try_from(pid) {
                Ok(pid) => Some((pid, xid)),
                Err(_) => return move_along_path_refusal("pid is out of range"),
            },
            _ => return move_along_path_refusal("pid and window_id go together"),
        };
        if wayland && target.is_some() {
            // Window pixels need the window's screen origin and a verified
            // way to raise it, which Wayland compositors do not reliably
            // give, so only desktop pixels are accepted.
            return ToolResult::error(
                "move_along_path on Wayland only takes scope \"desktop\" without pid or \
                 window_id, in get_desktop_state pixels. No input was sent.",
            )
            .with_structured(json!({
                "code": "unsupported_platform",
                "tool": "move_along_path",
                "effect": "refused",
            }));
        }
        if target.is_none() && !desktop_pixels {
            return move_along_path_refusal(
                "window scope needs pid and window_id (window-local screenshot pixels); \
                 pass scope \"desktop\" without them for desktop pixels (get_desktop_state \
                 pixels, or list_windows bounds plus window-local offsets)",
            );
        }
        let request = match path::PathRequest::from_args(&args) {
            Ok(request) => request,
            Err(error) => return move_along_path_refusal(error.to_string()),
        };
        let sampled = {
            let request = request.clone();
            match tokio::task::spawn_blocking(move || path::sample(&request)).await {
                Ok(Ok(sampled)) => sampled,
                Ok(Err(error)) => return move_along_path_refusal(error.to_string()),
                Err(error) => return ToolResult::error(format!("Task error: {error}")),
            }
        };

        // Map the samples to screen pixels: one affine map for the whole
        // path, so arcs and parametric curves keep their shape.
        let (scale, origin) = if let Some((pid, xid)) = target {
            match tokio::task::spawn_blocking(move || window_target_matches(pid, xid, None)).await {
                Ok(true) => {}
                Ok(false) => {
                    return move_along_path_refusal(format!(
                        "window_id {xid} does not belong to pid {pid}"
                    ))
                }
                Err(error) => return ToolResult::error(format!("Task error: {error}")),
            }
            let scale = if desktop_pixels {
                let (fx, fy) = pincer_core::desktop_capture_scale::desktop_scale_factor(&args)
                    .unwrap_or((1.0, 1.0));
                (fx, fy)
            } else {
                match screenshot_scale(&self.state, &args, pid, Some(xid)) {
                    Ok(scale) => (scale, scale),
                    Err(refusal) => return refusal,
                }
            };
            let origin = if desktop_pixels {
                (0.0, 0.0)
            } else {
                match tokio::task::spawn_blocking(move || window_local_to_screen(xid, 0.0, 0.0))
                    .await
                {
                    Ok(Ok(origin)) => origin,
                    Ok(Err(error)) => return ToolResult::error(format!("{error:#}")),
                    Err(error) => return ToolResult::error(format!("Task error: {error}")),
                }
            };
            (scale, origin)
        } else {
            (
                pincer_core::desktop_capture_scale::desktop_scale_factor(&args)
                    .unwrap_or((1.0, 1.0)),
                (0.0, 0.0),
            )
        };
        let screen: Vec<(f64, f64)> = sampled
            .points
            .iter()
            .map(|(x, y)| (origin.0 + x * scale.0, origin.1 + y * scale.1))
            .collect();
        // On Wayland the backend checks the samples against its own pointer
        // area, before it sends any input.
        let mut screen_width = None;
        if !wayland {
            let (width, height) = match tokio::task::spawn_blocking(x11_screen_size).await {
                Ok(Ok(size)) => size,
                Ok(Err(error)) => return ToolResult::error(error.to_string()),
                Err(error) => return ToolResult::error(format!("Task error: {error}")),
            };
            if let Err(error) = path::check_on_screen(&screen, width, height) {
                return move_along_path_refusal(error.to_string());
            }
            screen_width = Some(width);
        }

        let points: Vec<(i32, i32)> = screen
            .iter()
            .map(|(x, y)| (x.round() as i32, y.round() as i32))
            .collect();
        let space = match desktop_input_space().await {
            Ok(space) => space,
            Err(error) => return ToolResult::error(error.to_string()),
        };
        // The agent cursor follows the real pointer in the overlay's layout
        // coordinates (the same as screen pixels on X11).
        let overlay_points: Vec<(f64, f64)> = points
            .iter()
            .map(|&(x, y)| {
                let (lx, ly) = space.to_layout(x, y);
                (f64::from(lx), f64::from(ly))
            })
            .collect();
        let cursor_id = resolve_cursor_key(&args);
        if let Some((_, xid)) = target {
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::PinAbove(xid),
            );
        }
        // Glide to the start before the real pointer jumps there.
        reveal_pointer_action_for(
            &self.state,
            &cursor_id,
            overlay_points[0].0,
            overlay_points[0].1,
            false,
        )
        .await;

        let (started_tx, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let gesture = crate::input::PointerPathGesture {
            points,
            offsets_ms: sampled.offsets_ms.clone(),
            button: request.button,
            click_at_end: request.click_at_end.map(path::EndClick::x_button_and_count),
            started: Some(started_tx),
        };
        let budget = std::time::Duration::from_millis(request.duration_ms + 15_000);
        let run = async move {
            match target {
                // Like move_cursor and drag on Wayland: libei can wait for
                // portal consent, which the X11 budget does not allow for.
                None if wayland => {
                    tokio::task::spawn_blocking(move || {
                        crate::wayland::pointer_path_desktop(&space, &gesture)
                    })
                    .await
                }
                None => {
                    spawn_blocking_bounded("move_along_path", budget, move || {
                        crate::input::send_pointer_path_xtest(&gesture)
                    })
                    .await
                }
                Some((pid, xid)) => {
                    spawn_blocking_bounded("move_along_path", budget, move || {
                        // The report is not used (the result is always
                        // "unverifiable"), so skip the post-path observation:
                        // with no window change it polled for up to 800 ms.
                        crate::input::with_x11_foreground_opts(
                            xid,
                            crate::input::ForegroundOptions::pointer()
                                .for_pid(pid)
                                .without_observation(),
                            || crate::input::send_pointer_path_xtest(&gesture),
                        )
                        .map(|(elapsed, _report)| elapsed)
                    })
                    .await
                }
            }
        };
        let follow = follow_overlay_path_with(
            |command| crate::overlay::send_command_for(cursor_id.clone(), command),
            &overlay_points,
            &sampled.offsets_ms,
            request.button.is_some(),
            eased_jump_px(screen_width),
            started_rx,
        );
        let result = if crate::overlay::renderer_configured() {
            tokio::pin!(run);
            tokio::pin!(follow);
            tokio::select! {
                // The route returns once its last sample is sent, which can be
                // a frame before the follow's own clock reaches the end: let
                // the follow finish so the agent cursor's glide is drawn in
                // full. A route that fails mid-path ends the follow with it.
                result = &mut run => {
                    if matches!(&result, Ok(Ok(_))) {
                        follow.await;
                    }
                    result
                }
                () = &mut follow => run.await,
            }
        } else {
            run.await
        };
        if matches!(&result, Ok(Ok(_))) {
            let end = *overlay_points.last().expect("sampled path is non-empty");
            self.state
                .cursor_registry
                .update_position(&cursor_id, end.0, end.1);
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::track_pointer_command(end.0, end.1),
            );
            if request.click_at_end.is_some() {
                emit_cursor_hook(&cursor_id, end.0, end.1, true);
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::OverlayCommand::ClickPulse { x: end.0, y: end.1 },
                );
            }
        }
        let elapsed = match result {
            Ok(Ok(elapsed)) => elapsed,
            Ok(Err(error)) => {
                if let Some(refused) = error.downcast_ref::<crate::wayland::PointerPathRefused>() {
                    return move_along_path_refusal(refused.to_string());
                }
                return input_error_result(error);
            }
            Err(error) => return ToolResult::error(format!("Task error: {error}")),
        };

        let first = sampled.points[0];
        let last = *sampled.points.last().expect("sampled path is non-empty");
        let duration_ms = elapsed.as_millis() as u64;
        let stats = json!({
            "segments": sampled.segment_count,
            "samples": sampled.points.len(),
            "length_px": round_tenth(sampled.length_px),
            "duration_ms": duration_ms,
            "start": [round_tenth(first.0), round_tenth(first.1)],
            "end": [round_tenth(last.0), round_tenth(last.1)],
        });
        let held = match request.button {
            Some(1) => " holding the left button",
            Some(2) => " holding the middle button",
            Some(3) => " holding the right button",
            _ => "",
        };
        let click = match request.click_at_end {
            Some(path::EndClick::Left) => ", then left-clicked",
            Some(path::EndClick::Right) => ", then right-clicked",
            Some(path::EndClick::Middle) => ", then middle-clicked",
            Some(path::EndClick::Double) => ", then double-clicked",
            None => "",
        };
        let windowed = target.is_some();
        // One line with every stat, floats to one decimal as JSON writes
        // them, so the text result does not repeat `stats` as JSON.
        ToolResult::text(format!(
            "Moved the real pointer along the path{held} in {duration_ms} ms{click}: \
             {:.1} px from ({:.1}, {:.1}) to ({:.1}, {:.1}) in the coordinates passed, {} \
             segment(s), {} samples.",
            round_tenth(sampled.length_px),
            round_tenth(first.0),
            round_tenth(first.1),
            round_tenth(last.0),
            round_tenth(last.1),
            sampled.segment_count,
            sampled.points.len(),
        ))
        .with_structured(json!({
            "scope": if windowed { "window" } else { "desktop" },
            "path": if windowed {
                "x11_xtest_fg"
            } else if wayland {
                "wayland_desktop"
            } else {
                "xtest_desktop"
            },
            "delivery_mode": if windowed { "foreground" } else { "not_applicable" },
            "effect": "unverifiable",
            "stats": stats,
        }))
    }
}

// ── set_agent_cursor_enabled ──────────────────────────────────────────────────

/// The `renderer` value and a text suffix for an agent-cursor reply. Without a
/// renderer (XWayland mode) the setters still store the value, but nothing on
/// screen changes, and the reply has to say so.
fn cursor_renderer_report() -> (Value, String) {
    match crate::overlay::active_renderer() {
        Some(renderer) => (json!(renderer), String::new()),
        None => (
            Value::Null,
            format!(
                " Nothing changes on screen: {}.",
                crate::overlay::missing_renderer_reason()
            ),
        ),
    }
}

pub struct SetAgentCursorEnabledV2Tool {
    pub(super) state: Arc<ToolState>,
}

static CURSOR_ENABLED_V2_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetAgentCursorEnabledV2Tool {
    fn def(&self) -> &ToolDef {
        CURSOR_ENABLED_V2_DEF.get_or_init(|| canonical_cursor_def("set_agent_cursor_enabled"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let enabled = match args.get("enabled").and_then(Value::as_bool) {
            Some(value) => value,
            None => return ToolResult::error("Missing required boolean field `enabled`."),
        };
        let session = resolve_cursor_key(&args);
        self.state.cursor_registry.set_enabled(&session, enabled);
        crate::overlay::send_command_for(
            session.clone(),
            cursor_overlay::OverlayCommand::SetEnabled(enabled),
        );
        let (renderer, note) = cursor_renderer_report();
        // Pointer and keyboard actions turn the cursor back on (see
        // `overlay_glide_to_target_for`), so the reply says how long a hide
        // lasts; round 2's Opus tester expected it to last until
        // `enabled:true`.
        ToolResult::text(format!(
            "Agent cursor for session '{session}' {}.{note}",
            if enabled {
                "enabled"
            } else {
                "disabled until the session's next desktop pointer or keyboard action, which \
                 shows it again"
            }
        ))
        .with_structured(json!({"session":session,"enabled":enabled,"renderer":renderer}))
    }
}

pub struct SetAgentCursorMotionV2Tool;

static CURSOR_MOTION_V2_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn cursor_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_i64().map(|integer| integer as f64))
    })
}

#[async_trait]
impl Tool for SetAgentCursorMotionV2Tool {
    fn def(&self) -> &ToolDef {
        CURSOR_MOTION_V2_DEF.get_or_init(|| canonical_cursor_def("set_agent_cursor_motion"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let session = resolve_cursor_key(&args);
        let current = crate::overlay::current_motion_for(&session);
        let motion = current.with_overrides(
            cursor_number(args.get("start_handle")),
            cursor_number(args.get("end_handle")),
            cursor_number(args.get("arc_size")),
            cursor_number(args.get("arc_flow")),
            cursor_number(args.get("spring")),
            cursor_number(args.get("glide_duration_ms")),
            cursor_number(args.get("dwell_after_click_ms")),
            cursor_number(args.get("idle_hide_ms")),
            None,
            cursor_number(args.get("turn_radius")),
        );
        let motion = match motion.with_style_args(&args) {
            Ok(motion) => motion,
            Err(message) => return ToolResult::error(message),
        };
        crate::overlay::send_command_for(
            session.clone(),
            cursor_overlay::OverlayCommand::SetMotion(motion.clone()),
        );
        let (renderer, note) = cursor_renderer_report();
        ToolResult::text(format!(
            "Agent cursor motion updated for session '{session}': style {}, timing {}.{note}\n{}",
            motion.style.as_str(),
            motion.timing.as_str(),
            motion.effects_summary(&current),
        ))
        .with_structured(
            json!({"session":session,"motion":motion.output_json(),"renderer":renderer}),
        )
    }
}

pub struct SetAgentCursorThemeTool {
    pub(super) state: Arc<ToolState>,
}

static CURSOR_THEME_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn cursor_reduced_motion(value: Option<&str>) -> cursor_overlay::ReducedMotion {
    match value {
        Some("on") => cursor_overlay::ReducedMotion::On,
        Some("off") => cursor_overlay::ReducedMotion::Off,
        _ => cursor_overlay::ReducedMotion::Auto,
    }
}

#[async_trait]
impl Tool for SetAgentCursorThemeTool {
    fn def(&self) -> &ToolDef {
        CURSOR_THEME_DEF.get_or_init(|| canonical_cursor_def("set_agent_cursor_theme"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let Some(theme_id) = args.get("theme_id").and_then(Value::as_str) else {
            return ToolResult::error("Missing required string field `theme_id`.");
        };
        let session = resolve_cursor_key(&args);
        let resolved_theme = match cursor_overlay::resolve_theme_selection(theme_id) {
            Ok(theme) => theme,
            Err(error) => {
                return ToolResult::error(format!(
                    "Cursor theme '{theme_id}' cannot be selected: {error:#}. Installed theme \
                     ids: {}.",
                    installed_cursor_themes().join(", ")
                ));
            }
        };
        let (version, profile) = resolved_theme
            .as_deref()
            .map(|theme| (theme.version.as_str(), theme.profile.as_str()))
            .unwrap_or((
                cursor_overlay::DEFAULT_THEME_VERSION,
                cursor_overlay::THEME_PROFILE,
            ));
        let reduced_motion =
            cursor_reduced_motion(args.get("reduced_motion").and_then(Value::as_str));
        self.state
            .cursor_registry
            .update_config(&session, |config| {
                config.theme_id = theme_id.to_owned();
                config.reduced_motion = reduced_motion;
            });
        crate::overlay::send_command_for(
            session.clone(),
            cursor_overlay::OverlayCommand::SetTheme {
                theme_id: theme_id.to_owned(),
                reduced_motion,
            },
        );
        let (renderer, note) = cursor_renderer_report();
        ToolResult::text(format!(
            "Agent cursor theme for session '{session}' set to '{theme_id}'.{note}"
        ))
        .with_structured(json!({"session":session,"theme":{
            "id":theme_id,
            "version":version,
            "profile":profile,
            "reduced_motion":reduced_motion,
            "fallback":null
        },"renderer":renderer}))
    }
}

pub struct GetAgentCursorStateV2Tool {
    pub(super) state: Arc<ToolState>,
}

static CURSOR_STATE_V2_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetAgentCursorStateV2Tool {
    fn def(&self) -> &ToolDef {
        CURSOR_STATE_V2_DEF.get_or_init(|| canonical_cursor_def("get_agent_cursor_state"))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let session = resolve_cursor_key(&args);
        let enabled = crate::overlay::is_enabled_for(&session);
        let motion = crate::overlay::current_motion_for(&session);
        let (theme_id, version, profile, fallback, visual) =
            crate::overlay::current_theme_state_for(&session).unwrap_or_else(|| {
                (
                    cursor_overlay::DEFAULT_THEME_ID.into(),
                    cursor_overlay::DEFAULT_THEME_VERSION.into(),
                    cursor_overlay::THEME_PROFILE.into(),
                    None,
                    cursor_overlay::CursorVisualState::default(),
                )
            });
        let modifiers: Vec<&str> = [
            visual.delivery.map(|value| value.as_str()),
            visual.target.map(|value| value.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect();
        // The last point this session's cursor was placed at, `null` until
        // it first moves (same registry source as macOS).
        let position = self
            .state
            .cursor_registry
            .get(&session)
            .and_then(|cursor| cursor.x.zip(cursor.y))
            .map(|(x, y)| json!({"x": x, "y": y}));
        // Without a renderer nothing applies the setters' commands, so the
        // motion and theme below are the defaults, not what was last set.
        let (renderer, note) = cursor_renderer_report();
        let installed_themes = installed_cursor_themes();
        let drawn_at = match &position {
            Some(point) => format!(
                "last drawn at ({}, {}) in desktop pixels",
                point["x"], point["y"]
            ),
            None => "not drawn yet (position null)".to_owned(),
        };
        // The effects line says which effects are pinned even when none is:
        // the text renderer leaves an empty `motion.pinned_effects` out.
        let text = format!(
            "Agent cursor state for session '{session}'.{note}\n\
             enabled={enabled}: {}; get_session's cursor_visible says whether it is drawn right \
             now (it hides after motion.idle_hide_ms idle).\n\
             Position: {drawn_at}.\n\
             Installed themes (ids for set_agent_cursor_theme): {}.\n\
             {}",
            if enabled {
                "it may be drawn"
            } else {
                "hidden by set_agent_cursor_enabled until the session's next desktop pointer or \
                 keyboard action"
            },
            installed_themes.join(", "),
            motion.effects_summary(&motion),
        );
        ToolResult::text(text).with_structured(json!({
            "session":session,
            "renderer":renderer,
            "enabled":enabled,
            "position":position,
            "installed_themes":installed_themes,
            "theme":{
                "id":theme_id,
                "version":version,
                "profile":profile,
                "reduced_motion":visual.reduced_motion,
                "fallback":fallback
            },
            "visual_state":{
                "requested_action":visual.requested_action,
                "resolved_action":visual.resolved_action,
                "modifiers":modifiers,
                "phase":visual.phase(),
                "frame":visual.frame(),
                "preempted_count":visual.preempted_count
            },
            "motion":motion.output_json()
        }))
    }
}

fn canonical_cursor_def(name: &str) -> ToolDef {
    let contract = pincer_contract::tool_contract(name)
        .unwrap_or_else(|| panic!("missing canonical cursor contract for {name}"));
    ToolDef::from_contract(&contract)
}

/// The ids set_agent_cursor_theme accepts: the built-in default plus the
/// theme store's files, as `pincer cursor-theme list` reads them. An
/// unreadable store still leaves the built-in.
fn installed_cursor_themes() -> Vec<String> {
    cursor_overlay::list_installed_themes()
        .unwrap_or_else(|_| vec![cursor_overlay::DEFAULT_THEME_ID.to_owned()])
}
