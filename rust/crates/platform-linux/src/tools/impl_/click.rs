//! click, and the click pipeline double_click and right_click run through.

use super::*;

// ── click ─────────────────────────────────────────────────────────────────────

pub struct ClickTool {
    pub(super) state: Arc<ToolState>,
}

pub(super) fn capture_admission_error(error: anyhow::Error) -> ToolResult {
    let code = pincer_core::capture_runtime::admission_error_code(&error);
    ToolResult::error(format!("capture-bound click refused: {error}"))
        .with_structured(json!({ "code": code, "effect": "refused" }))
}
static CLICK_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Where a foreground element click may put the real pointer.
enum ForegroundElementPlacement {
    /// The element center lies inside the target window (or a popup of the
    /// same process): XTest-click it there.
    Point { xid: u64, sx: f64, sy: f64 },
    /// The element has no usable on-screen bounds (its menu is closed, its
    /// page is unmapped, its center is outside every window of the process):
    /// only the AT-SPI action can reach it.
    NoPoint { reason: String },
}

impl ClickTool {
    /// X11 foreground element click. Behave like a user when the element is
    /// really on screen: activate the window and XTest-click the element's
    /// center. A degenerate center — the walk answering (0,0) for a menu item
    /// whose menu is closed used to glide the real pointer into the GNOME hot
    /// corner — is never clicked; the observed object's AT-SPI action is fired
    /// instead (`path: "ax_fg"`, a label the core's action record maps to
    /// the AT-SPI transport), with the foreground activation kept, and when
    /// that is not possible either the call is refused
    /// (`element_bounds_unavailable`).
    #[allow(clippy::too_many_arguments)]
    async fn foreground_element_click(
        &self,
        cursor_id: &str,
        pid: u32,
        idx: usize,
        observed: &crate::atspi::snapshot::CachedElement,
        xid_hint: Option<u64>,
        placement: Option<(u64, f64, f64)>,
        button: u8,
        count: usize,
        modifiers: &[String],
    ) -> ToolResult {
        let observed_for_state = observed.clone();
        let decided = tokio::task::spawn_blocking(move || {
            decide_foreground_element_placement(pid, &observed_for_state, xid_hint, placement)
        })
        .await;
        let (xid, decision) = match decided {
            Ok(v) => v,
            Err(e) => return ToolResult::error(format!("Task error: {e}")),
        };
        match decision {
            ForegroundElementPlacement::Point { xid, sx, sy } => {
                // Ask the toolkit who actually owns this screen point before
                // pressing it: a snapshot frame is where the toolkit *said*
                // the element is, and a hidden sibling view (a QFileDialog's
                // off-page QListView mirroring the visible QTreeView) can
                // report a frame nothing is drawn at. See atspi::native::hit_test.
                // Web content is exempt: Chromium's GetAccessibleAtPoint
                // answers with an unrelated `panel` for web elements, so it
                // cannot vouch for (or against) the point.
                // A toolkit that cannot hit-test at all (it names nothing at
                // the point or at its frame's center) is checked with the X11
                // stacking instead, after activation, right before the press.
                let owned_ref = observed
                    .object_ref
                    .clone()
                    .filter(|_| !observed.in_web_content);
                let (sx, sy, redirect_note, point_check) = if let Some(oref) = owned_ref {
                    let (pid_c, oref_c, sx_i, sy_i) =
                        (pid, oref, sx.round() as i32, sy.round() as i32);
                    let ownership = tokio::task::spawn_blocking(move || {
                        crate::atspi::native::hit_test::point_ownership(
                            pid_c, xid, &oref_c, sx_i, sy_i,
                        )
                    })
                    .await
                    .unwrap_or(crate::atspi::native::hit_test::PointOwnership::Unknown);
                    match ownership {
                        crate::atspi::native::hit_test::PointOwnership::Redirect {
                            target,
                            screen_point,
                            ..
                        } => (
                            screen_point.0 as f64,
                            screen_point.1 as f64,
                            Some(format!("redirected the press to {}", target.describe())),
                            PointCheck::Toolkit,
                        ),
                        crate::atspi::native::hit_test::PointOwnership::Owned => {
                            (sx, sy, None, PointCheck::Toolkit)
                        }
                        crate::atspi::native::hit_test::PointOwnership::Unanswerable => {
                            (sx, sy, None, PointCheck::X11Stacking)
                        }
                        crate::atspi::native::hit_test::PointOwnership::Mismatch { owner } => {
                            return ToolResult::error(format!(
                                "click: element [{idx}] (pid {pid}) has a snapshot frame at \
                                 ({sx_i}, {sy_i}), but that screen point is owned by {} rather \
                                 than the observed element; a real pointer click there would hit \
                                 the wrong control. Re-snapshot with get_window_state, or click by \
                                 pixel from a screenshot.",
                                owner.describe()
                            ))
                            .with_structured(json!({
                                "code": "element_bounds_unavailable",
                                "effect": "none",
                                "element_index": idx,
                                "reason": "point_owned_by_another_element",
                                "owner": owner.describe(),
                                "point_check": PointCheck::Toolkit.as_str(),
                            }));
                        }
                        crate::atspi::native::hit_test::PointOwnership::Unknown => {
                            (sx, sy, None, PointCheck::Unchecked)
                        }
                    }
                } else {
                    (sx, sy, None, PointCheck::Unchecked)
                };
                crate::overlay::send_command_for(
                    cursor_id.to_owned(),
                    cursor_overlay::OverlayCommand::PinAbove(xid),
                );
                // A redirected press targets another control: no rect.
                let target_rect = redirect_note
                    .is_none()
                    .then(|| frame_target_rect(observed.bounds, sx, sy))
                    .flatten();
                reveal_pointer_action_target_for(&self.state, cursor_id, sx, sy, true, target_rect)
                    .await;
                // Read before the click: a popup the click closes has no
                // geometry left afterwards.
                let window_point = crate::x11::window_info(xid)
                    .map(|w| (sx.round() as i32 - w.x, sy.round() as i32 - w.y))
                    .unwrap_or((sx.round() as i32, sy.round() as i32));
                let modifier_owned = modifiers.to_vec();
                let result = spawn_blocking_bounded(
                    "foreground element click",
                    foreground_budget(0),
                    move || {
                        let modifier_refs: Vec<&str> =
                            modifier_owned.iter().map(String::as_str).collect();
                        crate::input::with_x11_foreground_opts(
                            xid,
                            crate::input::ForegroundOptions::pointer()
                                .for_pid(pid)
                                .at_point(sx.round() as i32, sy.round() as i32),
                            || {
                                if point_check == PointCheck::X11Stacking {
                                    let cover = crate::atspi::native::hit_test::x11_point_cover(
                                        xid,
                                        pid,
                                        sx.round() as i32,
                                        sy.round() as i32,
                                    );
                                    if !cover.reaches_target() {
                                        return Ok(Some(cover));
                                    }
                                }
                                crate::input::send_click_xtest_desktop_with_modifiers(
                                    sx.round() as i32,
                                    sy.round() as i32,
                                    button,
                                    count,
                                    &modifier_refs,
                                )
                                .map(|()| None)
                            },
                        )
                    },
                )
                .await;
                match result {
                    Ok(Ok((Some(cover), _))) => {
                        x11_point_cover_refusal(idx, pid, xid, (sx, sy), cover)
                    }
                    Ok(Ok((None, report))) => {
                        let confirmed = report.window_change_confirms();
                        let popup_outcome = PopupClickOutcome::from_foreground(&report);
                        let mut structured = foreground_structured(
                            "x11_xtest_fg",
                            report.clone(),
                            serde_json::Map::new(),
                        );
                        let popup_note = popup_outcome
                            .map(|outcome| outcome.apply(&mut structured))
                            .unwrap_or_default();
                        let result = ToolResult::text(format!(
                            "Clicked element {} (pid {pid}) with a real pointer click \
                             (delivery_mode=foreground{}, focus_after={}, point_check={}){}{}{}\
                             {popup_note}",
                            observed.described(idx),
                            report
                                .popup_target
                                .map(|popup| format!(
                                    ", popup window {popup} took the input without activation"
                                ))
                                .unwrap_or_default(),
                            report.focus_after.as_str(),
                            point_check.as_str(),
                            if confirmed {
                                format!("; {}.", report.window_change.as_deref().unwrap_or(""))
                            } else {
                                format!("; not verified: {POINTER_CHECK}.")
                            },
                            redirect_note
                                .as_deref()
                                .map(|n| format!(" ({n})."))
                                .unwrap_or_default(),
                            if point_check == PointCheck::X11Stacking {
                                " The app's toolkit cannot hit-test this window, so X11 \
                                 confirmed the window is on top at the press point instead."
                            } else {
                                ""
                            },
                        ))
                        .with_structured(structured);
                        with_pixel_points(
                            result,
                            Some((sx.round() as i32, sy.round() as i32)),
                            window_point,
                        )
                    }
                    Ok(Err(e)) => input_error_result(e),
                    Err(e) => ToolResult::error(format!("Task error: {e}")),
                }
            }
            ForegroundElementPlacement::NoPoint { reason } => {
                let Some(xid) = xid else {
                    return ToolResult::error(format!(
                        "click: element [{idx}] (pid {pid}) has no usable on-screen bounds \
                         ({reason}) and pid {pid} has no window to activate; no input was sent."
                    ))
                    .with_structured(json!({
                        "code": "element_bounds_unavailable",
                        "effect": "none",
                        "element_index": idx,
                        "reason": reason,
                    }));
                };
                if observed.object_ref.is_none()
                    || button != 1
                    || count != 1
                    || !modifiers.is_empty()
                {
                    return ToolResult::error(format!(
                        "click: element [{idx}] (pid {pid}) has no usable on-screen bounds \
                         ({reason}); a real pointer click cannot reach it and this click \
                         shape (button {button} × {count}{}) has no AT-SPI action form. No \
                         input was sent. Open the containing menu first, re-snapshot with \
                         get_window_state, or click by pixel from a screenshot.",
                        if modifiers.is_empty() { String::new() } else { format!(", modifiers {modifiers:?}") }
                    ))
                    .with_structured(json!({
                        "code": "element_bounds_unavailable",
                        "effect": "none",
                        "element_index": idx,
                        "reason": reason,
                        "suggestion": "open the containing menu first, or re-snapshot with get_window_state and click by x/y",
                    }));
                }
                crate::overlay::send_command_for(
                    cursor_id.to_owned(),
                    cursor_overlay::OverlayCommand::PinAbove(xid),
                );
                let observed_for_ax = observed.clone();
                let result = spawn_blocking_bounded(
                    "foreground element action",
                    foreground_budget(0),
                    move || {
                        crate::input::with_x11_foreground_opts(
                            xid,
                            crate::input::ForegroundOptions::pointer(),
                            || crate::atspi::perform_action_observed(&observed_for_ax),
                        )
                    },
                )
                .await;
                match result {
                    Ok(Ok(((action, suspected_noop, unacknowledged), report))) => {
                        let mut extra = serde_json::Map::new();
                        extra.insert("ax_action".into(), json!(action));
                        extra.insert("bounds_reason".into(), json!(reason));
                        if suspected_noop {
                            extra.insert("effect".into(), json!("suspected_noop"));
                            extra.insert("escalation".into(), non_ax_escalation());
                        } else if unacknowledged {
                            // No doAction reply arrived in time: never let a
                            // coincidental window-change observation promote
                            // this to `confirmed` — the AT-SPI layer itself
                            // never confirmed dispatch, let alone effect.
                            extra.insert("effect".into(), json!("unverifiable"));
                            extra.insert("verified".into(), json!(false));
                            extra.insert(
                                "note".into(),
                                json!(
                                    "no acknowledgement received for the doAction call within \
                                     the AT-SPI timeout; its effect is unknown"
                                ),
                            );
                        }
                        ToolResult::text(format!(
                            "Clicked element {} (pid {pid}) through its AT-SPI action \
                             \"{action}\" with the window activated (delivery_mode=foreground, \
                             path=ax_fg, focus_after={}): the element had no usable \
                             on-screen bounds ({reason}), so no pointer was moved.{}",
                            observed.described(idx),
                            report.focus_after.as_str(),
                            match &report.window_change {
                                Some(change) => format!(" {change}."),
                                None => format!(" Not verified: {POINTER_CHECK}."),
                            }
                        ))
                        .with_structured(foreground_structured("ax_fg", report, extra))
                    }
                    Ok(Err(error)) => {
                        if error.is::<crate::atspi::native::CachedElementGone>() {
                            return ToolResult::error(format!(
                                "click: stale_element_token: observed AT-SPI object [{idx}] is no \
                                 longer present ({error}); re-snapshot with get_window_state"
                            ))
                            .with_structured(json!({
                                "code": "stale_element_token",
                                "effect": "none",
                            }));
                        }
                        if crate::input::foreground::error_code(&error).is_some() {
                            return input_error_result(error);
                        }
                        ToolResult::error(format!(
                            "click: element [{idx}] (pid {pid}) has no usable on-screen bounds \
                             ({reason}) and its AT-SPI action failed ({error:#}); no input was \
                             sent. Open the containing menu first, re-snapshot with \
                             get_window_state, or click by pixel from a screenshot."
                        ))
                        .with_structured(json!({
                            "code": "element_bounds_unavailable",
                            "effect": "none",
                            "element_index": idx,
                            "reason": reason,
                            "ax_error": format!("{error:#}"),
                        }))
                    }
                    Err(e) => ToolResult::error(format!("Task error: {e}")),
                }
            }
        }
    }
}

/// Decide whether a foreground element click has a real point to press.
/// Returns the window to activate (when the process has one) and the
/// placement decision. Runs blocking X11 / AT-SPI queries.
fn decide_foreground_element_placement(
    pid: u32,
    observed: &crate::atspi::snapshot::CachedElement,
    xid_hint: Option<u64>,
    placement: Option<(u64, f64, f64)>,
) -> (Option<u64>, ForegroundElementPlacement) {
    let xid = xid_hint
        .or_else(|| placement.map(|(xid, ..)| xid).filter(|xid| *xid != 0))
        .or_else(|| {
            crate::x11::list_windows(Some(pid))
                .into_iter()
                .next()
                .map(|w| w.xid)
        });
    // The object's own state decides whether it is on screen at all: a menu
    // item in a closed menu still answers a (stale or zero) extent.
    let showing = match crate::atspi::element_showing_observed(observed) {
        Ok(showing) => Some(showing),
        Err(error) => {
            tracing::debug!(
                "foreground element [{}] showing-state unavailable: {error:#}",
                observed.key
            );
            None
        }
    };
    if showing == Some(false) {
        return (
            xid,
            ForegroundElementPlacement::NoPoint {
                reason: format!(
                    "the {} is not showing (its menu or page is closed)",
                    observed.role
                ),
            },
        );
    }
    let Some((placed_xid, sx, sy)) = placement.filter(|(xid, ..)| *xid != 0) else {
        return (
            xid,
            ForegroundElementPlacement::NoPoint {
                reason: "no usable on-screen bounds were resolved for it".to_owned(),
            },
        );
    };
    let (px, py) = (sx.round() as i32, sy.round() as i32);
    let inside = |w: &crate::x11::WindowInfo| {
        px >= w.x && py >= w.y && px < w.x + w.width as i32 && py < w.y + w.height as i32
    };
    if crate::x11::window_info(placed_xid).is_some_and(|w| inside(&w)) {
        return (
            xid,
            ForegroundElementPlacement::Point {
                xid: placed_xid,
                sx,
                sy,
            },
        );
    }
    // The center is outside the named window: a popup menu of the same
    // process (an open menu's item) is the one legitimate case.
    if let Some(popup) = crate::input::popup_under_screen_point(px, py) {
        if popup.pid.is_none_or(|p| p == pid) {
            return (
                xid,
                ForegroundElementPlacement::Point {
                    xid: placed_xid,
                    sx,
                    sy,
                },
            );
        }
    }
    (
        xid,
        ForegroundElementPlacement::NoPoint {
            reason: format!(
                "its center ({px}, {py}) lies outside window {placed_xid} and every popup of \
                 pid {pid}"
            ),
        },
    )
}

#[async_trait]
impl Tool for ClickTool {
    fn def(&self) -> &ToolDef {
        CLICK_DEF.get_or_init(|| ToolDef {
            name: "click".into(),
            description: "Click in a window. Prefer `element_token`: it works on hidden or \
                background windows, and the result names the element's role and label. A token click \
                fires the element's accessible action, so a row with `actions=[]` (a text \
                area, for example) cannot be clicked by token in background; use \
                delivery_mode foreground. Use x,y only for \
                canvas or custom-drawn surfaces missing from the AT-SPI tree.\n\n\
                Pass pid + window_id + x,y, or an element_token (pid optional); a `target` \
                {kind: \"window\", pid, window_id} can stand in for pid + window_id. A point \
                outside the window is refused (point_outside_window); pixel results report \
                window_point (real window pixels, scaled back from the screenshot) and \
                screen_point. In background, a pixel click first fires the accessible action \
                under the point (result `hit: <role> \"<name>\" action=...`, or `selected: ...` \
                for a list or file item), else presses the virtual pointer there, or with no \
                virtual pointer sends a synthetic X press (path x11_xsendevent). Qt takes \
                synthetic presses whether or not its window is active; GTK3/4 and LibreOffice \
                drop them, so there a pixel click with no accessible control under it is \
                refused (background_unavailable). If `hit:` names an element you did not aim \
                at, that element's action ran. The tree can hold invisible items (entries of \
                closed menus), so on a surface with no action of its own (a text area) prefer \
                delivery_mode foreground. When a token \
                or pixel click opens a popup menu Pincer can identify, the result includes \
                `popup: {window_id, bounds}`; pass that window_id to get_window_state to index \
                its items. Without `popup` (the result may instead say a popup holds the \
                keyboard grab), a menu may still be open even though window screenshots never \
                show popup menus; press Escape on the window before more pixel clicks. A stale \
                token fails with an error naming \
                the current snapshot.\n\n\
                `count` 2 or 3 gives a double or triple click; there is no triple_click tool. \
                Native Wayland supports only the left button and refuses `modifier` clicks.".into(),
            input_schema: json!({
                // `pid` is conditionally required (validated in code: needed for
                // window/element clicks, omitted for windowless scope="desktop"),
                // so it is NOT pinned in `required` — matches the click→[] canon
                // in pincer_core::tool_schema.
                "type":"object","required":[],"properties":{
                    "session": pincer_core::tool_schema::session_schema(),
                    "cursor_id": cursor_id_schema(),
                    "pid":{"type":"integer","description":"Process id. Required unless scope is \"desktop\" or element_token is given (the token carries it)."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                    "x":{"type":"number","description":"Pixel X in the window's get_window_state screenshot (0..screenshot_width). Pass scope:\"desktop\" for get_desktop_state pixels."},
                    "y":{"type":"number","description":"Pixel Y in the window's get_window_state screenshot (0..screenshot_height); see x."},
                    "element_token": pincer_core::tool_schema::element_token_schema(),
                    "capture_id":{"type":"string","description":"One-shot id of the get_window_state or get_desktop_state image that x/y come from; x/y are read in that capture."},
                    // Shape matches the shared button_schema() canon (string +
                    // [left,right,middle]); kept inline to carry the Linux/Wayland
                    // back-compat prose the click button-schema test asserts on.
                    "button":{"type":"string","enum":["left","right","middle"],"description":"Mouse button. Default \"left\". Native Wayland supports only left; right/middle return an error."},
                    "count":{"type":"integer","minimum":1,"description":"Clicks in one press train: 1 (default), 2 double click, 3 triple click (selects a line or paragraph)."},
                    "modifier": pincer_core::tool_schema::modifier_schema(),
                    "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Default \"window\". \"desktop\" with x,y and no pid/window_id clicks at get_desktop_state screen coordinates."},
                    "coordinate_frame": coordinate_frame_schema(),
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        self.click(args, ClickKind::Click).await
    }
}

/// What click's pixel delivery reports: the result `path`, the foreground
/// transaction, the pointer route and the focus guard, each when it ran.
type PixelClickDelivery = (
    &'static str,
    Option<crate::input::ForegroundReport>,
    Option<PointerRoute>,
    Option<crate::input::FocusGuardReport>,
);

/// Which pointer tool a click call came from. double_click and right_click
/// run click's implementation with their own button and count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClickKind {
    Click,
    Double,
    Right,
}

impl ClickKind {
    pub(super) fn tool(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Double => "double_click",
            Self::Right => "right_click",
        }
    }

    /// The result's verb.
    pub(super) fn verb(self) -> &'static str {
        match self {
            Self::Click => "Clicked",
            Self::Double => "Double-clicked",
            Self::Right => "Right-clicked",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Double => "double-click",
            Self::Right => "right-click",
        }
    }

    /// double_click and right_click are pointer gestures: they never stand
    /// in an accessible action for the press.
    pub(super) fn is_gesture(self) -> bool {
        self != Self::Click
    }

    /// The `button` and `count` a gesture tool sends.
    pub(super) fn button_and_count(self) -> (&'static str, u64) {
        match self {
            Self::Click => ("left", 1),
            Self::Double => ("left", 2),
            Self::Right => ("right", 1),
        }
    }
}

/// Where a double_click or right_click by element token lands: the element
/// center in its window's real pixels and its screen rectangle for the
/// overlay. Bounded like click's element placement: resolving bounds
/// re-walks the AT-SPI tree, which can stall on a large GTK or VCL window.
async fn gesture_element_point(
    pid: u32,
    idx: usize,
    xid_hint: Option<u64>,
) -> Result<(u64, f64, f64, Option<[f64; 4]>), ToolResult> {
    let located = tokio::time::timeout(
        ELEMENT_AX_BUDGET,
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let (xid, lx, ly) = checked_element_local_coords(pid, idx, xid_hint)?;
            let rect = element_screen_center_and_rect(pid, idx, Some(xid))
                .ok()
                .map(|(_, rect)| rect);
            Ok((xid, lx, ly, rect))
        }),
    )
    .await;
    match located {
        Ok(Ok(Ok(point))) => Ok(point),
        Ok(Ok(Err(e))) => Err(input_error_result(e.context("AT-SPI bounds failed"))),
        Ok(Err(e)) => Err(ToolResult::error(format!("Task error: {e}"))),
        Err(_elapsed) => Err(ToolResult::error(format!(
            "the bounds of element [{idx}] did not resolve within {ELEMENT_AX_BUDGET:?}; no \
             input was sent. Take a screenshot, then retry by pixel (x/y)."
        ))
        .with_structured(json!({ "code": "ax_timeout", "effect": "none" }))),
    }
}

/// double_click and right_click: click's implementation with their button
/// and count. Their own background refusals (Chromium, WebKitGTK, GTK
/// without a real pointer, native Wayland focused input) still run first,
/// and Hyprland foreground keeps click's exact-target route, as before.
pub(super) async fn gesture_click(state: &Arc<ToolState>, args: Value, kind: ClickKind) -> ToolResult {
    let click = ClickTool {
        state: state.clone(),
    };
    let mut args = args;
    let (button, count) = kind.button_and_count();
    args["button"] = json!(button);
    args["count"] = json!(count);
    if windowless_desktop_scope(&args) {
        // Dispatch refuses an explicit background delivery for click's
        // desktop scope; that check is keyed to the tool name.
        if let Err(refusal) = pincer_core::action_target::enforce_delivery_target("click", &args)
        {
            return refusal;
        }
        return click.click(args, kind).await;
    }
    let pid = match args.require_u32("pid") {
        Ok(v) => v,
        Err(e) => return e,
    };
    let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
    if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
        return refusal;
    }
    if let Some(refusal) = unavailable_webkit_background(pid, delivery) {
        return refusal;
    }
    if let Some(refusal) = unavailable_gtk_pointer_background(pid, delivery) {
        return refusal;
    }
    if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, true) {
        return refusal;
    }
    if hyprland_foreground(delivery) {
        return click.click(args, ClickKind::Click).await;
    }
    click.click(args, kind).await
}

impl ClickTool {
    /// click's implementation, shared by double_click and right_click
    /// (`kind`), which pass their own button and count in `args`.
    async fn click(&self, args: Value, kind: ClickKind) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        let modifiers = match modifier_arg(kind.tool(), "modifier", &args.str_array("modifier")) {
            Ok(modifiers) => modifiers,
            Err(refusal) => return refusal,
        };

        // ── Window-less screen-absolute branch (desktop target) ───────────────
        // x,y with NO pid and NO window_id → TRUE SCREEN pixels. Foreground,
        // desktop-scope click (the Linux peer of the Windows WindowFromPoint /
        // macOS global-HID path). The core registry has already enforced the
        // session policy; this branch validates the explicit action form.
        let has_pid = args.get("pid").map(|v| !v.is_null()).unwrap_or(false);
        let has_window_id = args.get("window_id").map(|v| !v.is_null()).unwrap_or(false);
        let has_xy = args.get("x").map(|v| v.is_number()).unwrap_or(false)
            && args.get("y").map(|v| v.is_number()).unwrap_or(false);
        if args.get("capture_id").is_some() && !has_xy {
            return ToolResult::error("click.capture_id requires both x and y coordinates.")
                .with_structured(json!({ "code": "invalid_arguments" }));
        }
        if has_xy && !has_pid && !has_window_id {
            if args.get("scope").and_then(Value::as_str) != Some("desktop") {
                return ToolResult::error(format!(
                    "{}: x,y given with no pid/window_id requires scope=\"desktop\"; \
                     use get_desktop_state to read true screen pixels first.",
                    kind.tool()
                ))
                .with_structured(json!({
                    "code": "desktop_coordinate_scope_required",
                    "suggestion": "pass scope=desktop",
                }));
            }
            let input = match parse_legacy_click_input(&args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let button = parse_mouse_button(input.button.unwrap_or(ClickButton::Left).as_str());
            let n = input.count.unwrap_or(1) as usize;
            if n == 0 {
                return ToolResult::error("click.count must be at least 1.")
                    .with_structured(json!({ "code": "invalid_arguments" }));
            }
            let space = match desktop_input_space().await {
                Ok(space) => space,
                Err(error) => {
                    return ToolResult::error(format!("desktop-scope click failed: {error}"))
                }
            };
            let (action_x, action_y) = if let Some(capture_id) = args.opt_str("capture_id") {
                match crate::capture_action_frame::admit_desktop_click(
                    &self.state.capture_service,
                    &args,
                    &capture_id,
                    input.x,
                    input.y,
                ) {
                    Ok(point) => point,
                    Err(error) => return capture_admission_error(error),
                }
            } else {
                (input.x, input.y)
            };
            let sx = action_x.round() as i32;
            let sy = action_y.round() as i32;
            // Glide the agent-cursor overlay to the click point first (the macOS
            // / Windows desktop paths already do this). Without it the overlay
            // sits idle elsewhere while only the real pointer warps, so a viewer
            // sees the cursor "click somewhere else."
            // The overlay draws in layout coordinates, like the input below.
            let (overlay_x, overlay_y) = space.to_layout(sx, sy);
            reveal_pointer_action_for(
                &self.state,
                &cursor_id,
                f64::from(overlay_x),
                f64::from(overlay_y),
                true,
            )
            .await;
            let r = tokio::task::spawn_blocking(move || {
                if crate::wayland::wayland_input_enabled() {
                    if !modifiers.is_empty() {
                        anyhow::bail!(
                            "modified desktop clicks are unavailable on native Wayland: \
                             the virtual-pointer route cannot carry keyboard modifier state"
                        );
                    }
                    crate::wayland::click_desktop(&space, sx, sy, n as u32, button)
                } else {
                    let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    crate::input::send_click_xtest_desktop_with_modifiers(
                        sx,
                        sy,
                        button,
                        n,
                        &modifier_refs,
                    )
                }
            })
            .await;
            return match r {
                // Screen-absolute click — never driver-verifiable (no
                // read-back); the caller confirms via screenshot.
                Ok(Ok(())) => ToolResult::text(format!(
                    "✅ Sent screen-absolute {} at ({sx},{sy}) (desktop scope).",
                    kind.noun()
                ))
                .with_structured(
                    json!({ "path": if crate::wayland::wayland_input_enabled() { "wayland_desktop" } else { "xtest_desktop" }, "verified": false, "effect": "unverifiable" }),
                ),
                Ok(Err(e)) => ToolResult::error(format!("desktop-scope click failed: {e}")),
                Err(e) => ToolResult::error(format!("task error: {e}")),
            };
        }

        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let count = args.u64_or("count", 1) as usize;
        let isolated_background = isolated_hyprland_background(delivery);
        // Surface 5: reject unknown buttons so a typo can't silently fall through
        // to a left-click. Empty string keeps back-compat with old clients.
        let button_str_raw = args.str_or("button", "left").to_lowercase();
        if !matches!(button_str_raw.as_str(), "" | "left" | "right" | "middle") {
            return ToolResult::error(format!(
                "{}: unknown button \"{button_str_raw}\"; expected one of left, right, middle.",
                kind.tool()
            ));
        }
        let button_str = if button_str_raw.is_empty() {
            "left"
        } else {
            button_str_raw.as_str()
        };
        let button = parse_mouse_button(button_str);

        // Surface 6: resolve `element_token` to its element and window
        // before any pixel path.
        let capture_id_arg = args.opt_str("capture_id");
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self.state.snapshots.resolve(pid as i32, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let elem_idx_resolved: Option<usize> = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { element_index, .. } => {
                Some(*element_index)
            }
            pincer_core::element_token::ResolvedElement::None => None,
        };
        let window_id_resolved: Option<u64> = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            pincer_core::element_token::ResolvedElement::None => window_id_arg,
        };

        if capture_id_arg.is_some() && elem_idx_resolved.is_some() {
            return ToolResult::error(
                "click.capture_id is only valid with x/y coordinates, not an element target.",
            )
            .with_structured(json!({ "code": "invalid_arguments" }));
        }

        // double_click and right_click are pointer gestures: an element token
        // only says where they land, never which accessible action to run.
        let gesture_element = match elem_idx_resolved {
            Some(idx) if kind.is_gesture() => {
                match gesture_element_point(pid, idx, window_id_resolved).await {
                    Ok((xid, x, y, rect)) => Some((idx, xid, x, y, rect)),
                    Err(refusal) => return refusal,
                }
            }
            _ => None,
        };
        if let (Some(idx), false) = (elem_idx_resolved, kind.is_gesture()) {
            // The observed element: its proven AT-SPI identity and the frame
            // the snapshot recorded. Every route below acts on that identity
            // (or its cached frame); the live ordinal `idx` is only an
            // address within the snapshot, never used to retarget.
            let observed = match resolved {
                pincer_core::element_token::ResolvedElement::Element { element, .. } => element,
                pincer_core::element_token::ResolvedElement::None => {
                    unreachable!("indexed click resolved without an element")
                }
            };
            let xid_hint = window_id_resolved;
            // Resolve the element's screen center + its window FIRST, so the
            // agent cursor glides to the target *before* the click fires —
            // matching the coordinate path below and the macOS/Windows backends.
            // Previously perform_action ran inside this spawn_blocking, so the
            // app updated before the cursor visibly arrived.
            // Bounded: resolving bounds re-walks the AT-SPI tree, which on a
            // large GTK/VCL tree can stall for minutes; a missing placement
            // degrades to the AX action (background) or a refusal (foreground).
            let placement = tokio::time::timeout(
                ELEMENT_AX_BUDGET,
                tokio::task::spawn_blocking(move || -> anyhow::Result<(u64, f64, f64)> {
                    let (cx, cy) = element_screen_center(pid, idx, xid_hint)?;
                    let xid = xid_hint
                        .or_else(|| {
                            crate::x11::list_windows(Some(pid))
                                .into_iter()
                                .next()
                                .map(|w| w.xid)
                        })
                        .unwrap_or(0);
                    Ok((xid, cx, cy))
                }),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(Result::ok);
            // X11 foreground element click: behave like a user — activate the
            // window and XTest-click the element's screen center. No AT-SPI
            // action, no second tree walk. Element bounds come from the
            // (bounded) placement above.
            if delivery.is_foreground()
                && !crate::wayland::wayland_input_enabled()
                && !hyprland_foreground(delivery)
            {
                return self
                    .foreground_element_click(
                        &cursor_id, pid, idx, &observed, xid_hint, placement, button, count,
                        &modifiers,
                    )
                    .await;
            }
            if let Some((xid, sx, sy)) = placement {
                if xid != 0 {
                    crate::overlay::send_command_for(
                        cursor_id.clone(),
                        cursor_overlay::OverlayCommand::PinAbove(xid),
                    );
                }
                reveal_pointer_action_target_for(
                    &self.state,
                    &cursor_id,
                    sx,
                    sy,
                    true,
                    frame_target_rect(observed.bounds, sx, sy),
                )
                .await;
            }

            // Chromium can execute a genuine AT-SPI action without focus. Try
            // that route before applying its background synthetic-input gate.
            // Plain Hyprland foreground clicks also retain semantic activation
            // for offscreen controls; other pointer gestures need raw input.
            let foreground_hyprland = hyprland_foreground(delivery);
            if foreground_hyprland && window_id_resolved.is_none() {
                return foreground_hyprland_refusal(
                    "an exact window_id or window-bound element token is required",
                );
            }
            // An AT-SPI action can open a menu or press a button, but it cannot
            // give an entry / spin button / slider the widget focus a following
            // type_text needs (GIMP spin scales, VS Code settings inputs). With
            // the MPX real pointer available, click those like a user would.
            let needs_real_click = element_needs_real_click(&observed.role);
            // A menu / menu item's `doAction` is declined or silently ignored
            // by LibreOffice VCL (and opens GTK menus without the pointer
            // grab a following item click expects): with a real pointer, press
            // it like a user would and let the popup be listed. That holds
            // for an entry that is on screen (SHOWING: a menubar entry, an
            // item of an open GTK3 / VCL popup). A menu item that is not
            // showing has no point to press: gail (GTK2, GIMP) never marks
            // menu items SHOWING, open or closed, and an item of a closed
            // menu is still reached through its AT-SPI `click` action (the
            // same rule the foreground route applies), so those keep the
            // action route.
            let real_pointer = !delivery.is_foreground()
                && !crate::wayland::wayland_input_enabled()
                && crate::input::real_pointer_input_available();
            let menu_entry_on_screen = real_pointer && element_is_menu_role(&observed.role) && {
                // A bounded D-Bus round-trip: off the runtime thread.
                let observed_for_state = observed.clone();
                let showing = tokio::task::spawn_blocking(move || {
                    crate::atspi::element_showing_observed(&observed_for_state)
                })
                .await;
                !matches!(showing, Ok(Ok(false)))
            };
            let real_click_role = real_pointer && (needs_real_click || menu_entry_on_screen);
            // Without a real pointer, an editable / table cell cannot take a
            // background click that gives it focus: refuse instead of firing
            // an AT-SPI action whose effect is not a click.
            if !delivery.is_foreground()
                && !crate::wayland::is_wayland()
                && needs_real_click
                && !crate::input::real_pointer_input_available()
            {
                if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                    return refusal;
                }
                return crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                );
            }
            if !real_click_role
                && element_click_prefers_ax(
                    foreground_hyprland,
                    button,
                    count,
                    !modifiers.is_empty(),
                )
            {
                let observed_for_ax = observed.clone();
                let guard_pid = (!delivery.is_foreground()).then_some(pid);
                // A token from a popup's own snapshot (a menu-bar menu read
                // with get_window_state): whether the popup closed afterwards
                // tells the caller if the item really ran its click.
                let token_popup = match xid_hint.filter(|_| !delivery.is_foreground()) {
                    Some(xid) if !crate::wayland::is_wayland() => {
                        tokio::task::spawn_blocking(move || crate::input::popup_window_info(xid))
                            .await
                            .ok()
                            .flatten()
                            .map(|popup| popup.window)
                    }
                    _ => None,
                };
                let ax_result = match tokio::time::timeout(
                    ELEMENT_AX_BUDGET,
                    tokio::task::spawn_blocking(move || {
                        // Background: an AT-SPI action can open a menu (GTK/VCL
                        // grab the keyboard and activate the toplevel) or map a
                        // dialog mutter then focuses. Snapshot, act, restore.
                        match guard_pid {
                            Some(pid) => crate::input::focus_guard::guarded(Some(pid), || {
                                crate::atspi::perform_action_observed(&observed_for_ax)
                            }),
                            None => crate::atspi::perform_action_observed(&observed_for_ax)
                                .map(|value| (value, None)),
                        }
                    }),
                )
                .await
                {
                    Ok(joined) => joined,
                    Err(_elapsed) => {
                        return ToolResult::error(format!(
                            "click: the AT-SPI action for element [{idx}] did not complete                              within {ELEMENT_AX_BUDGET:?}; its effect is unknown. Take a                              screenshot, then retry by pixel (x/y) or with                              delivery_mode:\"foreground\"."
                        ))
                        .with_structured(json!({
                            "code": "ax_timeout",
                            "effect": "unknown",
                            "escalation": non_ax_escalation(),
                        }));
                    }
                };
                if let Ok(Ok(((_action, suspected_noop, unacknowledged), guard))) = ax_result {
                    let mut structured = json!({
                        "path": "ax",
                        "verified": false,
                        "effect": if suspected_noop { "suspected_noop" } else { "unverifiable" },
                    });
                    if suspected_noop {
                        structured["escalation"] = non_ax_escalation();
                    } else if unacknowledged {
                        structured["note"] = json!(
                            "no acknowledgement received for the doAction call within the \
                             AT-SPI timeout; its effect is unknown"
                        );
                    }
                    let mut text = format!("Clicked element {} (pid {pid}).", observed.described(idx));
                    if let Some(window) = token_popup {
                        let closed = tokio::task::spawn_blocking(move || {
                            crate::input::foreground::wait_for_popup_close(window)
                        })
                        .await
                        .unwrap_or(false);
                        text.push_str(
                            &PopupClickOutcome {
                                window,
                                closed,
                                ran_action: true,
                            }
                            .apply(&mut structured),
                        );
                    }
                    return attach_focus_guard(
                        ToolResult::text(text).with_structured(structured),
                        guard.as_ref(),
                    );
                }
                // The observed object is gone: the snapshot is stale. Refuse
                // rather than retarget whatever now lives at that index.
                if let Ok(Err(error)) = &ax_result {
                    if error.is::<crate::atspi::native::CachedElementGone>() {
                        return ToolResult::error(format!(
                            "AT-SPI element click failed: stale_element_token: observed \
                             AT-SPI object [{idx}] is no longer present ({error}); \
                             re-snapshot with get_window_state"
                        ))
                        .with_structured(json!({
                            "code": "stale_element_token",
                            "effect": "none",
                        }));
                    }
                }
                // AT-SPI errors can arrive after dispatch. Do not introduce a
                // primary-seat replay when its semantic outcome is uncertain.
                if foreground_hyprland {
                    return foreground_ax_click_unknown(pid, idx);
                }
            }
            if foreground_hyprland {
                if !modifiers.is_empty() {
                    return foreground_hyprland_refusal("modified clicks are unsupported");
                }
                let Some(xid) = window_id_resolved else {
                    return foreground_hyprland_refusal(
                        "an exact window_id or window-bound element token is required",
                    );
                };
                let point = tokio::task::spawn_blocking(move || {
                    resolve_element_local_coords(pid, idx, Some(xid))
                })
                .await;
                let (x, y) = match point {
                    Ok(Ok((_, x, y))) => (x, y),
                    _ => {
                        return foreground_hyprland_refusal(
                            "could not resolve exact element coordinates",
                        )
                    }
                };
                return foreground_hyprland_action(
                    &args,
                    pid,
                    xid,
                    crate::wayland::hyprland_input::Action::Click {
                        x,
                        y,
                        button: u32::from(button),
                        count,
                    },
                )
                .await;
            }
            if isolated_background {
                if !modifiers.is_empty() {
                    return isolated_hyprland_refusal(
                        "modified element clicks are unavailable on native Wayland: \
                         isolated input does not carry pointer modifier state",
                    );
                }
                let Some(exact_xid) = window_id_resolved else {
                    return isolated_hyprland_refusal(
                        "an exact window_id or window-bound element token is required",
                    );
                };
                let owner = named_session_cursor_key(&args);
                let (_cancellation, dispatch) =
                    match spawn_isolated_hyprland(&args, move |cancellation| {
                        let (_, x, y) = resolve_element_local_coords(pid, idx, Some(exact_xid))?;
                        crate::wayland::hyprland_input::execute(
                            owner,
                            pid,
                            exact_xid,
                            crate::wayland::hyprland_input::Action::Click {
                                x,
                                y,
                                button: u32::from(button),
                                count,
                            },
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
            if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                return refusal;
            }

            // The AX route was unavailable. Fall back to a real MPX pointer
            // click at the element (no focus steal), then to a target-addressed
            // X11 event for toolkits that accept it.
            let cursor_id_for_fallback = cursor_id.clone();
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<PointerRoute> {
                let (xid2, lx, ly) = checked_element_local_coords(pid, idx, xid_hint)?;
                let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                if crate::wayland::wayland_input_enabled() && !modifier_refs.is_empty() {
                    anyhow::bail!(
                        "modified element clicks are unavailable on native Wayland: \
                         the pointer route cannot carry keyboard modifier state"
                    );
                }
                // An explicit X11 foreground request needs the real XTest path
                // even for a plain click. Some native widgets (notably GTK
                // selectable rows) expose bounds but no AT-SPI Action and
                // ignore a targeted XSendEvent; limiting XTest to modified
                // clicks made those rows addressable but not selectable.
                if delivery.is_foreground() && !crate::wayland::wayland_input_enabled() {
                    let (_, sx, sy) = placement
                        .ok_or_else(|| anyhow::anyhow!("element screen bounds unavailable"))?;
                    crate::input::with_x11_foreground(xid2, 80, || {
                        crate::input::send_click_xtest_desktop_with_modifiers(
                            sx.round() as i32,
                            sy.round() as i32,
                            button,
                            count,
                            &modifier_refs,
                        )
                    })
                    .map(|()| PointerRoute::Foreground)
                } else if modifier_refs.is_empty() && !crate::wayland::wayland_input_enabled() {
                    x11_pixel_click_no_focus_steal(
                        &cursor_id_for_fallback,
                        xid2,
                        lx as i32,
                        ly as i32,
                        button,
                        count,
                    )
                } else {
                    crate::input::send_click_with_modifiers(
                        xid2,
                        lx as i32,
                        ly as i32,
                        count,
                        button,
                        &modifier_refs,
                    )
                    .map(|()| PointerRoute::Synthetic(None))
                }
            })
            .await;
            let mode_label = if delivery.is_foreground() {
                "foreground"
            } else {
                "background"
            };
            return match result {
                // The MPX route reports its cheap post-checks (screen region
                // around the element, focus untouched); the other routes stay
                // unverifiable and the caller confirms via screenshot.
                Ok(Ok(route)) => {
                    attach_focused_cell(
                        ToolResult::text(format!(
                            "Clicked element {} (pid {pid}) with a real pointer click {}",
                            observed.described(idx),
                            route.text_suffix(mode_label)
                        ))
                        .with_structured(route.structured(mode_label)),
                        pid,
                    )
                    .await
                }
                Ok(Err(e)) => input_error_result(e.context("AT-SPI element click failed")),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        if !isolated_background {
            if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                return refusal;
            }
        }

        // One point in the window's real pixels: a gesture's element center,
        // or x,y in the frame the call names.
        let (xid, x, y, target_rect) = match gesture_element {
            Some((_, xid, x, y, rect)) => (xid, x, y, rect),
            None => {
                if let Some(refusal) = require_point_args(kind.tool(), &args) {
                    return refusal;
                }
                let Some(xid) = args.opt_u64("window_id") else {
                    return ToolResult::error(
                        "Provide either element_token or pid + window_id + x/y.",
                    );
                };
                let native_refusal = (!isolated_background)
                    .then(|| unavailable_wayland_focused_input_background(delivery, true))
                    .flatten();
                let point = (args.f64_or("x", 0.0), args.f64_or("y", 0.0));
                match resolve_window_points(
                    &self.state,
                    &args,
                    kind.tool(),
                    (pid, xid),
                    [point],
                    native_refusal,
                )
                .await
                {
                    Ok([(x, y)]) => (xid, x, y, None),
                    Err(refusal) => return refusal,
                }
            }
        };

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        // Resolve the screen point the cursor glides to. Tool coordinates are
        // always window-local screenshot pixels; native Wayland translates
        // through compositor/AT-SPI geometry while X11 uses XTranslateCoordinates.
        let wayland_output_point = if crate::wayland::wayland_input_enabled() {
            Some(crate::wayland::window_local_to_output(
                xid,
                x.round() as i32,
                y.round() as i32,
            ))
        } else {
            None
        };
        let glide_target = if let Some((sx, sy)) = wayland_output_point {
            Some((sx as f64, sy as f64))
        } else {
            tokio::task::spawn_blocking(move || window_local_to_screen(xid, x, y))
                .await
                .ok()
                .and_then(|r| r.ok())
        };
        if let Some((sx, sy)) = glide_target {
            reveal_pointer_action_target_for(&self.state, &cursor_id, sx, sy, true, target_rect)
                .await;
        }

        let (xi, yi) = (x as i32, y as i32);
        let pixel_screen_point =
            glide_target.map(|(sx, sy)| (sx.round() as i32, sy.round() as i32));
        let (output_x, output_y) = wayland_output_point.unwrap_or((xi, yi));
        if hyprland_foreground(delivery) {
            if !modifiers.is_empty() {
                return foreground_hyprland_refusal("modified clicks are unsupported");
            }
            if window_id_resolved.is_none() {
                return foreground_hyprland_refusal("an exact window_id is required");
            }
            return foreground_hyprland_action(
                &args,
                pid,
                xid,
                crate::wayland::hyprland_input::Action::Click {
                    x,
                    y,
                    button: u32::from(button),
                    count,
                },
            )
            .await;
        }
        if isolated_background {
            if !modifiers.is_empty() {
                return isolated_hyprland_refusal(
                    "modified clicks are unsupported by isolated input",
                );
            }
            if button == 1 && count == 1 {
                let semantic = tokio::task::spawn_blocking(move || {
                    crate::atspi::perform_action_at_screen_point(pid, xid, output_x, output_y)
                })
                .await;
                if matches!(semantic, Ok(Ok(Some(_)))) {
                    return ToolResult::text("Dispatched AT-SPI click.").with_structured(json!({
                        "path": "wayland_atspi", "verified": false, "effect": "unverifiable"
                    }));
                }
            }
            return isolated_hyprland_action(
                &args,
                pid,
                xid,
                crate::wayland::hyprland_input::Action::Click {
                    x,
                    y,
                    button: u32::from(button),
                    count,
                },
            )
            .await;
        }
        // A background click on an open popup (the call get_window_state
        // advises for a Qt combo list whose items have no accessibles) is
        // checked afterwards by whether the popup closed. Foreground clicks
        // learn the same from their own transaction.
        let background_popup =
            if !delivery.is_foreground() && !crate::wayland::wayland_input_enabled() {
                tokio::task::spawn_blocking(move || crate::input::popup_window_info(xid))
                    .await
                    .ok()
                    .flatten()
                    .map(|popup| popup.window)
            } else {
                None
            };
        let cursor_id_for_task = cursor_id.clone();
        let modifiers_for_task = modifiers.clone();
        // delivery_mode: background (default) = no-focus-steal injection;
        // foreground = activate the target window (EWMH) first, then inject,
        // then restore prior active. Mirrors macOS/Windows.
        let fg_budget = foreground_budget(0);
        let result = spawn_blocking_bounded("foreground click", fg_budget, move || -> anyhow::Result<PixelClickDelivery> {
            if crate::wayland::wayland_input_enabled() {
                if !modifiers_for_task.is_empty() {
                    anyhow::bail!(
                        "modified coordinate clicks are unavailable on native Wayland: \
                         the pointer route cannot carry keyboard modifier state"
                    );
                }
                // Vision/pixel click on native Wayland. Mutter drops synthetic
                // virtual-pointer events (the `wayland::click` warp doesn't land),
                // so for a plain left single click resolve the screen pixel to the
                // covering accessible element and fire its action by
                // `element_index` — the coordinate-free path already verified
                // working. (x,y) are screen coords here, matching the frames in
                // `get_window_state`. Miss → fall through to the injection paths.
                if !delivery.is_foreground() && button == 1 && count == 1 {
                    if let Ok(Some(_)) =
                        crate::atspi::perform_action_at_screen_point(pid, xid, output_x, output_y)
                    {
                        return Ok(("wayland_atspi", None, None, None));
                    }
                }
                if crate::wayland::is_inject_mode() {
                    crate::wayland::inject_click(pid, xid, x, y, count as u32, button)?;
                    return Ok(("wayland_cua_compositor", None, None, None));
                }
                if !delivery.is_foreground() {
                    return Ok(("background_unavailable", None, None, None));
                }
                // Native Wayland: focus+raise the target toplevel
                // (foreign-toplevel `activate`), then drive `count` virtual-pointer
                // button events. Wayland injection routes to the compositor focus.
                crate::wayland::with_target_foreground(pid, xid, || {
                    crate::wayland::click_focused(output_x, output_y, count as u32, button)
                })?;
                return Ok(("wayland_activate", None, None, None));
            }
            // X11 injection. Tiered no-focus-steal delivery (background):
            //   1. Plain left single-click → AT-SPI doAction at that point.
            //   2. Right / middle / double click → real MPX uinput pointer + XI2
            //      shield grab (real Xorg only; skipped on Xvfb/Wayland).
            //   3. Fallback → synthetic XSendEvent.
            // Foreground skips the AT-SPI shortcut and does a real activated pixel
            // click (the agent's escalation when background didn't land).
            let inject = |fg: bool| -> anyhow::Result<(&'static str, Option<PointerRoute>, Option<crate::input::FocusGuardReport>)> {
                if !fg && button == 1 && count == 1 && modifiers_for_task.is_empty() {
                    // The accessible action under the point may open a menu or
                    // a dialog that takes the focus: guard and restore. With a
                    // real pointer available, an entry / spin button / cell
                    // under the point is left to the MPX click below (its
                    // `doAction` would not focus it for a following type_text).
                    let real_click_for_focus_roles =
                        crate::input::real_pointer_input_available();
                    let (hit, guard) = crate::input::focus_guard::guarded(Some(pid), || {
                        Ok(crate::atspi::perform_action_at_point_in(
                            pid,
                            xid,
                            xi,
                            yi,
                            real_click_for_focus_roles,
                        )
                        .ok()
                        .flatten())
                    })?;
                    if let Some(hit) = hit {
                        return Ok(("x11_atspi", Some(PointerRoute::Atspi(hit)), guard));
                    }
                }
                if !fg
                    && !crate::input::real_pointer_input_available()
                    && synthetic_press_is_dropped(pid)
                {
                    // No accessible actuator covers the point and the only
                    // route left is a synthetic XSendEvent that this toolkit
                    // (GTK3/4 XInput2, LibreOffice VCL) discards. Say so
                    // instead of reporting a click that changed nothing.
                    return Ok(("background_unavailable_pointer", None, None));
                }
                if fg {
                    // Foreground: the window is already activated. Deliver a REAL
                    // XTest warp+button click. Synthetic XSendEvent button events
                    // are dropped by GTK/Qt/Chromium/Firefox, and the MPX uinput
                    // path needs /dev/uinput, which headless X servers
                    // (Xvfb/Xtigervnc) lack — so neither focuses the clicked
                    // widget. XTest is accepted as real input and gives the
                    // widget keyboard focus, so a following type lands.
                    if let Ok((sx, sy)) = window_local_to_screen(xid, xi as f64, yi as f64) {
                        let modifier_refs: Vec<&str> =
                            modifiers_for_task.iter().map(String::as_str).collect();
                        crate::input::send_click_xtest_desktop_with_modifiers(
                            sx.round() as i32,
                            sy.round() as i32,
                            button,
                            count,
                            &modifier_refs,
                        )?;
                        return Ok(("x11_xtest_fg", None, None));
                    }
                }
                if modifiers_for_task.is_empty() {
                    // The MPX press train runs under its own (settled) focus
                    // guard inside the helper; its report rides on the route.
                    let route = x11_pixel_click_no_focus_steal(
                        &cursor_id_for_task,
                        xid,
                        xi,
                        yi,
                        button,
                        count,
                    )?;
                    return Ok((route.path().unwrap_or("x11_pixel"), Some(route), None));
                } else {
                    let modifier_refs: Vec<&str> =
                        modifiers_for_task.iter().map(String::as_str).collect();
                    x11_pixel_click_no_focus_steal_modifiers(
                        xid,
                        xi,
                        yi,
                        button,
                        count,
                        &modifier_refs,
                    )?;
                }
                Ok((if fg { "x11_pixel_fg" } else { "x11_pixel" }, None, None))
            };
            if delivery.is_foreground() {
                let mut opts = crate::input::ForegroundOptions::pointer().for_pid(pid);
                if let Some((sx, sy)) = pixel_screen_point {
                    opts = opts.at_point(sx, sy);
                }
                // A right press opens a context menu: name it, as the
                // background MPX route does.
                let popups_before: Option<Vec<u64>> = (button == 3).then(|| {
                    crate::input::mapped_popup_windows()
                        .iter()
                        .map(|popup| popup.window)
                        .collect()
                });
                crate::input::with_x11_foreground_opts(xid, opts, || inject(true))
                .map(|((path, route, guard), report)| {
                    let route = match (route, popups_before) {
                        (None, Some(before)) => {
                            let popups = wait_for_new_popups(pid, &before, CONTEXT_MENU_WAIT);
                            (!popups.is_empty()).then_some(PointerRoute::ForegroundPopups(popups))
                        }
                        (route, _) => route,
                    };
                    (path, Some(report), route, guard)
                })
            } else {
                inject(false).map(|(path, route, guard)| (path, None, route, guard))
            }
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        let outcome = match result {
            Ok(Ok(("background_unavailable", _, _, _))) => {
                crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                )
            }
            Ok(Ok(("background_unavailable_pointer", _, _, _))) => {
                let mut refusal = crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::SyntheticPointerDropped,
                );
                let hint = "No accessible control covers this point either, so a background \
                     pixel click here would change nothing. Click by element_token from \
                     get_window_state (AT-SPI action), or retry with \
                     delivery_mode='foreground'.";
                refusal
                    .content
                    .push(pincer_core::protocol::Content::text(hint));
                if let Some(structured) = refusal.structured_content.as_mut() {
                    structured["hint"] = json!(hint);
                    structured["path"] = json!("background_unavailable_pointer");
                }
                refusal
            }
            // A pixel/coordinate click is never driver-verifiable (no read-back) —
            // verified:false, effect:"unverifiable"; the caller confirms via
            // screenshot. path reports the rung taken; a foreground click also
            // reports the activation/focus transaction it confirmed first.
            Ok(Ok((path, report, route, guard))) => {
                let mut structured = match &route {
                    Some(route) => route.structured(mode_label),
                    None => json!({
                        "path": path,
                        "verified": false,
                        "effect": "unverifiable",
                        "delivery_mode": mode_label,
                    }),
                };
                if let Some(report) = &report {
                    apply_foreground_report(&mut structured, report);
                }
                let mut suffix = match (&route, &report) {
                    (Some(route), _) => route.text_suffix(mode_label),
                    (None, Some(report)) => foreground_click_suffix(path, report),
                    (None, None) => format!(
                        "(delivery_mode={mode_label}, path={path}); not verified — confirm \
                         with a screenshot."
                    ),
                };
                let popup_outcome = match (&report, background_popup) {
                    (Some(report), _) => PopupClickOutcome::from_foreground(report),
                    (None, Some(window)) => {
                        let closed = tokio::task::spawn_blocking(move || {
                            crate::input::foreground::wait_for_popup_close(window)
                        })
                        .await
                        .unwrap_or(false);
                        Some(PopupClickOutcome {
                            window,
                            closed,
                            ran_action: matches!(route, Some(PointerRoute::Atspi(_))),
                        })
                    }
                    (None, None) => None,
                };
                if let Some(outcome) = popup_outcome {
                    if outcome.closed {
                        // The closed popup is the verification.
                        suffix = suffix.replace(&format!("; not verified: {POINTER_CHECK}."), ".");
                    }
                    suffix.push_str(&outcome.apply(&mut structured));
                }
                let what = match (gesture_element, kind) {
                    (Some((idx, ..)), _) => format!("{} element [{idx}]", kind.verb()),
                    (None, ClickKind::Click) => format!("Clicked at ({x:.1}, {y:.1}) × {count}"),
                    (None, _) => format!("{} at ({x:.1}, {y:.1})", kind.verb()),
                };
                attach_focus_guard(
                    ToolResult::text(format!("{what} {suffix}")).with_structured(structured),
                    guard.as_ref(),
                )
            }
            Ok(Err(e)) => input_error_result(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        };
        let outcome = attach_focused_cell(outcome, pid).await;
        with_pixel_points(outcome, pixel_screen_point, (xi, yi))
    }
}

/// `scope:"desktop"` with no pid and no window_id: x,y are get_desktop_state
/// screen pixels and no window is named (dispatch has already rewritten the
/// form that names a window into `coordinate_frame:"desktop"`).
pub(super) fn windowless_desktop_scope(args: &Value) -> bool {
    args.get("scope").and_then(Value::as_str) == Some("desktop")
        && args.get("pid").is_none_or(Value::is_null)
        && args.get("window_id").is_none_or(Value::is_null)
}

/// How long a foreground right-click waits for the context menu it opened to
/// map: GTK and Qt map one within about 100 ms, and the foreground
/// transaction's own window-change poll allows the same 800 ms.
const CONTEXT_MENU_WAIT: std::time::Duration = std::time::Duration::from_millis(800);

/// The popups of `pid` mapped since `before` (popup window ids mapped
/// earlier), waiting up to `wait` for the first. A popup that names no pid
/// (a VCL menu) counts; Pincer's own agent cursor overlay does not.
fn wait_for_new_popups(
    pid: u32,
    before: &[u64],
    wait: std::time::Duration,
) -> Vec<crate::input::PopupWindow> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let popups: Vec<_> = crate::input::mapped_popup_windows()
            .into_iter()
            .filter(|popup| !before.contains(&popup.window))
            .filter(|popup| match popup.pid {
                Some(owner) => owner == pid,
                None => !popup.title.starts_with("Pincer."),
            })
            .collect();
        if !popups.is_empty() || std::time::Instant::now() >= deadline {
            return popups;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
}
