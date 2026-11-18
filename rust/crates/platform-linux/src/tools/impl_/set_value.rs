//! set_value: AT-SPI values, combo boxes and popups, web page fields, and
//! the read-back that confirms them.

use super::*;

// ── set_value ─────────────────────────────────────────────────────────────────

pub struct SetValueTool {
    pub(super) state: Arc<ToolState>,
}
static SV_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetValueTool {
    fn def(&self) -> &ToolDef {
        SV_DEF.get_or_init(|| ToolDef {
            name: "set_value".into(),
            description: "Set an element's value through AT-SPI EditableText or Value, else by \
                typing into the field (see `value`).".into(),
            input_schema: json!({
                "type":"object","required":["value"],"properties":{
                    "session": pincer_core::tool_schema::session_schema(),
                    "pid":{"type":"integer","description":"Target process ID. Optional with element_token (the token carries the pid)."},
                    "window_id":{"type":"integer","description":"Omit when element_token is supplied (the token carries it)."},
                    "element_token": pincer_core::tool_schema::element_token_schema(),
                    "value":{"type":"string","description":"New value. Written through AT-SPI EditableText/Value when the element exposes them; otherwise (GTK2 spin scales, VCL spin buttons) the field is clicked with the session's real pointer, its text selected and replaced through the virtual keyboard and committed with Tab, then read back. A combo box gets the option with this text (exact, else ignoring case), picked from its popup and read back."},
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let value = match args.get("value") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Number(number)) => number.to_string(),
            _ => {
                return ToolResult::error("set_value: 'value' must be a string or a number.")
                    .with_structured(json!({ "code": "invalid_arguments" }))
            }
        };
        let resolved = match self.state.snapshots.resolve(pid as i32, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (idx, xid) = match &resolved {
            pincer_core::element_token::ResolvedElement::Element {
                element_index,
                window_id,
                ..
            } => (*element_index, *window_id),
            pincer_core::element_token::ResolvedElement::None => {
                return ToolResult::error(
                    "set_value requires element_token to address the target element.",
                )
            }
        };
        let value_for_task = value.clone();
        let xid_opt = Some(xid);
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let cursor_id = resolve_cursor_key(&args);
        position_named_session_keyboard_cursor(&self.state, &args, pid, xid, Some(idx), None, true)
            .await;
        // 1. Focus-free accessibility write on the exact snapshot object.
        let ax = spawn_blocking_bounded("set_value", foreground_budget(0), move || {
            crate::atspi::set_value_in(pid, xid, idx, &value_for_task)
        })
        .await;
        let ax_error = match ax {
            Ok(Ok(())) => {
                let (readback, password) = set_value_readback(pid, xid, idx, &value).await;
                if let Some(engine) = web_page_engine(pid, xid, idx).await {
                    return set_value_page_result(idx, &value, readback, password, engine);
                }
                return set_value_result(idx, &value, "ax", readback, password, None);
            }
            Ok(Err(error)) if crate::atspi::native::is_no_value_route(&error) => error,
            Ok(Err(error)) if crate::atspi::native::is_no_such_option(&error) => {
                return no_such_option_result(idx, &error)
            }
            Ok(Err(error)) => return input_error_result(error),
            Err(error) => return ToolResult::error(format!("Task error: {error}")),
        };
        // 2. No accessibility write route. A combo box is opened with a click
        //    and the option picked from its popup (whose items only exist and
        //    act while it is open), then the popup is closed. Anything else is
        //    typed like a user would: click the field (real pointer), select
        //    all, type, commit with Tab.
        let is_combo = crate::atspi::cached_role_in(pid, xid, idx)
            .is_some_and(|role| role.eq_ignore_ascii_case("combo box"));
        let wayland = crate::wayland::wayland_input_enabled();
        // A Qt combo box selects only on a real press of a popup row: the
        // rows' accessible Toggle marks the row in the popup's view without
        // changing the combo box. Both modes press the row, so this route
        // needs no writable /dev/uinput.
        if is_combo && !wayland {
            let qt =
                tokio::task::spawn_blocking(move || crate::atspi::native::is_qt_application(pid))
                    .await
                    .unwrap_or(false);
            if qt {
                return qt_combo_set_value(
                    cursor_id,
                    pid,
                    xid,
                    idx,
                    value,
                    delivery.is_foreground(),
                )
                .await;
            }
        }
        let background_route = !delivery.is_foreground()
            && !wayland
            && crate::input::real_pointer_input_available()
            && crate::input::real_keyboard_input_available();
        let foreground_route = delivery.is_foreground() && !wayland;
        if !background_route && !foreground_route {
            let detail = format!(
                "{ax_error}; the keyboard fallback needs {}",
                if wayland {
                    "an X11 session"
                } else {
                    "the focus-free real-input route (a writable /dev/uinput), or delivery_mode:\"foreground\""
                }
            );
            let mut structured = json!({
                "code": "set_value_unavailable",
                "detail": detail,
                "effect": "none",
            });
            // Only X11 can run the foreground keyboard fallback.
            if !wayland {
                structured["escalation"] = json!({ "recommended": "foreground", "reason": "click the field and type the value with delivery_mode:\"foreground\"" });
            }
            return ToolResult::error(format!(
                "set_value: element [{idx}] has no accessibility write route and no keyboard \
                 fallback is available: {detail}."
            ))
            .with_structured(structured);
        }
        let value_for_keys = value.clone();
        let typed = spawn_blocking_bounded(
            "set_value keyboard fallback",
            foreground_budget(value.len()),
            move || -> anyhow::Result<Option<crate::input::FocusGuardReport>> {
                let (win, lx, ly) = resolve_element_local_coords(pid, idx, xid_opt)?;
                if is_combo {
                    return pick_from_popup(
                        &cursor_id,
                        pid,
                        xid,
                        idx,
                        (win, lx, ly),
                        &value_for_keys,
                        foreground_route,
                    );
                }
                if foreground_route {
                    let (sx, sy) = window_local_to_screen(win, lx, ly)?;
                    crate::input::with_x11_foreground_opts(
                        win,
                        crate::input::ForegroundOptions::pointer(),
                        || {
                            crate::input::send_click_xtest_desktop(
                                sx.round() as i32,
                                sy.round() as i32,
                                1,
                                1,
                            )?;
                            std::thread::sleep(std::time::Duration::from_millis(80));
                            crate::input::send_key_xtest("a", &["ctrl"])?;
                            crate::input::send_type_text_xtest(&value_for_keys)?;
                            crate::input::send_key_xtest("Tab", &[])
                        },
                    )?;
                    return Ok(None);
                }
                let (_, guard) = crate::input::focus_guard::guarded(Some(pid), || {
                    x11_pixel_click_no_focus_steal(&cursor_id, win, lx as i32, ly as i32, 1, 1)?;
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    background_virtual_key(&cursor_id, pid, win, "a", &["ctrl"])?;
                    background_virtual_text(&cursor_id, pid, win, &value_for_keys)?;
                    background_virtual_key(&cursor_id, pid, win, "Tab", &[])?;
                    Ok(())
                })?;
                Ok(guard)
            },
        )
        .await;
        match typed {
            Ok(Ok(guard)) => {
                // Let the toolkit commit on focus-out before reading back.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                let (readback, password) = set_value_readback(pid, xid, idx, &value).await;
                set_value_result(
                    idx,
                    &value,
                    match (is_combo, foreground_route) {
                        (true, true) => "click_select_fg",
                        (true, false) => "click_select_mpx",
                        (false, true) => "click_type_fg",
                        (false, false) => "click_type_mpx",
                    },
                    readback,
                    password,
                    guard.as_ref(),
                )
            }
            Ok(Err(error)) if crate::atspi::native::is_no_such_option(&error) => {
                no_such_option_result(idx, &error)
            }
            Ok(Err(error)) => input_error_result(error),
            Err(error) => ToolResult::error(format!("Task error: {error}")),
        }
    }
}

/// `set_value` named an option the combo box or list does not have. The
/// option labels are the app's text and can each be 4096 characters: the
/// text and `detail` show each escaped, quoted and cut at
/// `SHOWN_NAME_CHARS`; `options` keeps them whole.
pub(super) fn no_such_option_result(idx: usize, error: &anyhow::Error) -> ToolResult {
    let missing = error.downcast_ref::<crate::atspi::native::NoSuchOption>();
    let detail = match missing {
        Some(missing) => {
            let (shown, more) = missing.listed();
            format!(
                "no option {}; options: [{}]{}",
                quote_untrusted(&missing.value, SHOWN_NAME_CHARS),
                shown
                    .iter()
                    .map(|option| quote_untrusted(option, SHOWN_NAME_CHARS))
                    .collect::<Vec<_>>()
                    .join(", "),
                if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                }
            )
        }
        None => {
            let detail = error.to_string();
            detail
                .strip_prefix(crate::atspi::native::NO_SUCH_OPTION)
                .map(|rest| rest.trim_start_matches(':').trim())
                .unwrap_or(&detail)
                .to_owned()
        }
    };
    let mut structured = json!({ "code": "option_not_found", "detail": detail, "effect": "none" });
    if let Some(missing) = missing {
        let (shown, more) = missing.listed();
        structured["options"] = json!(shown);
        if more > 0 {
            structured["options_not_listed"] = json!(more);
        }
    }
    ToolResult::error(format!(
        "set_value: element [{idx}] has {detail}; nothing was changed."
    ))
    .with_structured(structured)
}

/// How long `set_value` waits for an opened combo box popup to list its
/// options.
const POPUP_OPTIONS_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Keyboard-fallback `set_value` on a combo box whose options cannot be
/// selected while it is closed: click it open (the real pointer: XTest in
/// the foreground, the session's MPX pointer in the background), activate
/// the option `value` names from the popup, and press Escape if the popup
/// is still open afterwards (an unknown option, or a toolkit that keeps it
/// open), so no popup is left behind.
fn pick_from_popup(
    cursor_id: &str,
    pid: u32,
    xid: u64,
    idx: usize,
    (win, lx, ly): (u64, f64, f64),
    value: &str,
    foreground: bool,
) -> anyhow::Result<Option<crate::input::FocusGuardReport>> {
    let close_popup = || -> anyhow::Result<()> {
        if !crate::atspi::options_showing_in(pid, xid, idx) {
            return Ok(());
        }
        if foreground {
            crate::input::with_x11_foreground_opts(
                win,
                crate::input::ForegroundOptions::keyboard(),
                || crate::input::send_key_xtest("Escape", &[]),
            )
            .map(|_| ())
        } else {
            background_virtual_key(cursor_id, pid, win, "Escape", &[]).map(|_| ())
        }
    };
    let pick = || -> anyhow::Result<String> {
        let deadline = std::time::Instant::now() + POPUP_OPTIONS_WAIT;
        loop {
            match crate::atspi::activate_option_in(pid, xid, idx, value) {
                Ok(Some(name)) => return Ok(name),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Ok(None) => anyhow::bail!("the combo box popup listed no options"),
                Err(error) => return Err(error),
            }
        }
    };
    let open_and_pick = || -> anyhow::Result<()> {
        let picked = pick();
        // Let the toolkit close the popup on activation before checking.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let closed = close_popup();
        picked?;
        closed
    };
    if foreground {
        let (sx, sy) = window_local_to_screen(win, lx, ly)?;
        crate::input::with_x11_foreground_opts(
            win,
            crate::input::ForegroundOptions::pointer(),
            || crate::input::send_click_xtest_desktop(sx.round() as i32, sy.round() as i32, 1, 1),
        )?;
        open_and_pick()?;
        return Ok(None);
    }
    let (_, guard) = crate::input::focus_guard::guarded(Some(pid), || {
        x11_pixel_click_no_focus_steal(cursor_id, win, lx as i32, ly as i32, 1, 1)?;
        open_and_pick()
    })?;
    Ok(guard)
}

/// Error prefix and result code: another popup of the app (a menu) is open,
/// so a combo box cannot be opened under its grab.
const OTHER_POPUP_OPEN: &str = "other_popup_open";

/// What [`pick_from_qt_popup`] did.
#[derive(Debug)]
pub(super) struct QtComboPick {
    /// The option whose row was pressed, and how its text matched the value
    /// (`exact` or `case-insensitive`).
    pub(super) option: String,
    pub(super) matched: &'static str,
    /// `click_select_fg` (XTest), `click_select_mpx` or
    /// `click_select_xsendevent` (the background press on the row).
    pub(super) path: &'static str,
    pub(super) popup: u64,
    /// The popup closed after the press, as a press on a row closes it.
    pub(super) popup_closed: bool,
    /// What the focus guards of the background open and press saw, when
    /// something moved.
    pub(super) guards: Vec<crate::input::FocusGuardReport>,
}

/// Where to press a popup row: the middle of the part of its rectangle that
/// lies inside the popup (a Qt combo row can be wider than its popup).
/// `None` when less than half the row's height is inside it (a row scrolled
/// out of the popup's view), so a press could land on its neighbor.
pub(super) fn popup_row_point(row: (i32, i32, i32, i32), popup: (i32, i32, u32, u32)) -> Option<(i32, i32)> {
    let (rx, ry, rw, rh) = row;
    let (px, py, pw, ph) = popup;
    let left = rx.max(px);
    let top = ry.max(py);
    let right = (rx + rw).min(px + pw as i32);
    let bottom = (ry + rh).min(py + ph as i32);
    if rw <= 0 || rh <= 0 || right - left < 2 || bottom - top < (rh / 2).max(2) {
        return None;
    }
    Some(((left + right) / 2, (top + bottom) / 2))
}

/// The popup of `pid` that opened after `before` (the pid's popups mapped
/// before the open), waiting up to [`POPUP_OPTIONS_WAIT`]. Qt maps the same
/// popup window again each time, so "new" means not mapped before.
fn wait_for_new_popup(pid: u32, before: &[u64]) -> Option<crate::input::PopupWindow> {
    let deadline = std::time::Instant::now() + POPUP_OPTIONS_WAIT;
    loop {
        if let Some(popup) = crate::input::mapped_popup_windows()
            .into_iter()
            .rev()
            .find(|popup| popup.pid == Some(pid) && !before.contains(&popup.window))
        {
            return Some(popup);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
}

/// The rows of the open popup `popup`, waiting up to [`POPUP_OPTIONS_WAIT`]
/// for Qt to mark its list showing.
fn wait_for_popup_rows(pid: u32, popup: u64) -> Option<crate::atspi::native::PopupItems> {
    let deadline = std::time::Instant::now() + POPUP_OPTIONS_WAIT;
    loop {
        if let Ok(Some(rows)) = crate::atspi::native::qt_popup_items(pid, popup) {
            if !rows.items.is_empty() {
                return Some(rows);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
}

/// Close a Qt combo box's popup that is still open. The combo box's own
/// accessible action toggles its popup (Qt opens or closes it, checked on the
/// probe 2026-11-09), focus-free; an Escape sent after the popup had already
/// closed would reach the dialog under it and reject the dialog.
fn close_qt_combo_popup(combo: &crate::atspi::native::ObjectRef, popup: u64) {
    if crate::input::popup_window_info(popup).is_none() {
        return;
    }
    if let Err(error) = crate::atspi::native::perform_action_ref(combo) {
        tracing::debug!("closing combo box popup {popup} failed: {error:#}");
        return;
    }
    crate::input::foreground::wait_for_popup_close(popup);
}

/// `set_value` on a Qt combo box: open its popup (a real click in the
/// foreground; the combo box's accessible action in the background), find
/// the row named `value` among the popup's rows (exact text first, then
/// ignoring case), and press it (XTest in the foreground; the background
/// pointer route on the popup otherwise), as a user picks an option. A value
/// the combo box does not list is refused before anything opens when Qt
/// lists the rows while closed, and otherwise after the popup is closed
/// again. Blocking.
fn pick_from_qt_popup(
    cursor_id: &str,
    pid: u32,
    xid: u64,
    idx: usize,
    value: &str,
    foreground: bool,
) -> anyhow::Result<QtComboPick> {
    use crate::atspi::native::match_option;
    // Act on the object the snapshot named and nothing else: an index walk of
    // a window that has closed would number another window's elements.
    let combo = crate::atspi::cached_object_ref(pid, xid, idx)
        .ok_or_else(|| anyhow::anyhow!("element [{idx}] is no longer in the snapshot cache"))?;
    if let Err(error) = crate::atspi::native::name_ref(&combo) {
        if error.is::<crate::atspi::native::CachedElementGone>() {
            anyhow::bail!(
                "{STALE_ELEMENT_TOKEN}: combo box element [{idx}] is no longer present \
                 ({error}); nothing was opened. Re-snapshot with get_window_state"
            );
        }
    }
    let listed = crate::atspi::option_names_in(pid, xid, idx);
    if !listed.is_empty() && match_option(&listed, value).is_none() {
        return Err(crate::atspi::no_such_option(value, &listed));
    }
    let pid_popups = || -> Vec<crate::input::PopupWindow> {
        crate::input::mapped_popup_windows()
            .into_iter()
            .filter(|popup| popup.pid == Some(pid))
            .collect()
    };
    let mut guards = Vec::new();
    let open = pid_popups();
    // The combo box's popup may be open already (a click opened it).
    let already = open.iter().rev().find_map(|popup| {
        crate::atspi::native::qt_popup_items(pid, popup.window)
            .ok()
            .flatten()
            .filter(|rows| rows.owner.as_ref() == Some(&combo))
            .map(|rows| (popup.clone(), rows))
    });
    let (popup, rows) = match already {
        Some(found) => found,
        None => {
            if let Some(other) = open.last() {
                anyhow::bail!(
                    "{OTHER_POPUP_OPEN}: {} of pid {pid} is open and is not this combo box's \
                     list; nothing was opened or pressed. Close it first (press_key Escape), \
                     then retry",
                    other.describe()
                );
            }
            let before: Vec<u64> = open.iter().map(|popup| popup.window).collect();
            if foreground {
                let (win, lx, ly) = resolve_element_local_coords(pid, idx, Some(xid))?;
                let (sx, sy) = window_local_to_screen(win, lx, ly)?;
                let (sx, sy) = (sx.round() as i32, sy.round() as i32);
                crate::input::with_x11_foreground_opts(
                    win,
                    crate::input::ForegroundOptions::pointer()
                        .for_pid(pid)
                        .at_point(sx, sy),
                    || crate::input::send_click_xtest_desktop(sx, sy, 1, 1),
                )?;
            } else {
                // Qt's showPopup grabs the pointer and keyboard; the guard
                // puts back a focus or activation the open moved.
                let (_, guard) = crate::input::focus_guard::guarded(Some(pid), || {
                    crate::atspi::native::perform_action_ref(&combo)
                })?;
                guards.extend(guard.filter(|guard| guard.changed));
            }
            let popup = wait_for_new_popup(pid, &before).ok_or_else(|| {
                anyhow::anyhow!(
                    "the combo box's popup did not open within {POPUP_OPTIONS_WAIT:?}; nothing \
                     was selected"
                )
            })?;
            let Some(rows) = wait_for_popup_rows(pid, popup.window) else {
                close_qt_combo_popup(&combo, popup.window);
                anyhow::bail!(
                    "the combo box's {} listed no rows within {POPUP_OPTIONS_WAIT:?}; it was \
                     closed again and nothing was selected",
                    popup.describe()
                );
            };
            if rows.owner.as_ref() != Some(&combo) {
                close_qt_combo_popup(&combo, popup.window);
                anyhow::bail!(
                    "{} opened, but its rows do not belong to combo box element [{idx}]; it \
                     was closed again and nothing was selected",
                    popup.describe()
                );
            }
            (popup, rows)
        }
    };
    let abandon = |error: anyhow::Error| -> anyhow::Error {
        close_qt_combo_popup(&combo, popup.window);
        error
    };
    let names: Vec<String> = rows.items.iter().map(|row| row.name.clone()).collect();
    let Some(found) = match_option(&names, value) else {
        return Err(abandon(crate::atspi::no_such_option(value, &names)));
    };
    let row = &rows.items[found];
    let popup_rect = (popup.x, popup.y, popup.width, popup.height);
    let Some((mut sx, mut sy)) = row.rect.and_then(|rect| popup_row_point(rect, popup_rect)) else {
        return Err(abandon(anyhow::anyhow!(
            "option {:?} is listed but its row is not shown inside {} (scrolled out of \
             view); it was closed again and nothing was selected",
            row.name,
            popup.describe()
        )));
    };
    match crate::atspi::native::hit_test::point_ownership(
        pid,
        popup.window,
        &row.object_ref,
        sx,
        sy,
    ) {
        crate::atspi::native::hit_test::PointOwnership::Mismatch { owner } => {
            return Err(abandon(anyhow::anyhow!(
                "the middle ({sx}, {sy}) of option {:?}'s row is owned by {}; nothing was \
                 pressed and the popup was closed again",
                row.name,
                owner.describe()
            )));
        }
        crate::atspi::native::hit_test::PointOwnership::Redirect { screen_point, .. } => {
            (sx, sy) = screen_point;
        }
        _ => {}
    }
    let (path, popup_closed) = if foreground {
        let ((), report) = crate::input::with_x11_foreground_opts(
            popup.window,
            crate::input::ForegroundOptions::pointer()
                .for_pid(pid)
                .at_point(sx, sy),
            || crate::input::send_click_xtest_desktop(sx, sy, 1, 1),
        )
        .map_err(abandon)?;
        let closed =
            report.popup_closed || crate::input::foreground::wait_for_popup_close(popup.window);
        ("click_select_fg", closed)
    } else {
        let route = x11_pixel_click_no_focus_steal(
            cursor_id,
            popup.window,
            sx - popup.x,
            sy - popup.y,
            1,
            1,
        )
        .map_err(abandon)?;
        let (path, guard) = match route {
            PointerRoute::Mpx(effect) => ("click_select_mpx", effect.focus_guard),
            PointerRoute::Synthetic(guard) => ("click_select_xsendevent", guard),
            _ => ("click_select_xsendevent", None),
        };
        guards.extend(guard.filter(|guard| guard.changed));
        (
            path,
            crate::input::foreground::wait_for_popup_close(popup.window),
        )
    };
    if !popup_closed {
        close_qt_combo_popup(&combo, popup.window);
    }
    Ok(QtComboPick {
        option: row.name.clone(),
        matched: crate::atspi::native::option_match_kind(&row.name, value),
        path,
        popup: popup.window,
        popup_closed,
        guards,
    })
}

/// [`pick_from_qt_popup`] plus the read-back: the combo box's name, which Qt
/// keeps equal to the option it shows.
async fn qt_combo_set_value(
    cursor_id: String,
    pid: u32,
    xid: u64,
    idx: usize,
    value: String,
    foreground: bool,
) -> ToolResult {
    let value_for_task = value.clone();
    let picked = spawn_blocking_bounded("set_value combo box", foreground_budget(0), move || {
        pick_from_qt_popup(&cursor_id, pid, xid, idx, &value_for_task, foreground)
    })
    .await;
    let pick = match picked {
        Ok(Ok(pick)) => pick,
        Ok(Err(error)) if crate::atspi::native::is_no_such_option(&error) => {
            return no_such_option_result(idx, &error)
        }
        Ok(Err(error))
            if [STALE_ELEMENT_TOKEN, OTHER_POPUP_OPEN]
                .iter()
                .any(|code| error.to_string().starts_with(code)) =>
        {
            let text = error.to_string();
            let code = text.split(':').next().unwrap_or_default().to_owned();
            return ToolResult::error(format!("set_value: {text}"))
                .with_structured(json!({ "code": code, "effect": "none" }));
        }
        Ok(Err(error)) => return input_error_result(error),
        Err(error) => return ToolResult::error(format!("Task error: {error}")),
    };
    // Qt changes the current item on the press's release, before the popup
    // closes; a short wait covers a slow event loop.
    let option = pick.option.clone();
    let readback = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            let name = crate::atspi::live_name_in(pid, xid, idx);
            if name.as_deref() == Some(option.as_str()) || std::time::Instant::now() >= deadline {
                return name;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    })
    .await
    .ok()
    .flatten();
    let mut result = qt_combo_pick_result(idx, &value, &pick, readback);
    for guard in &pick.guards {
        result = attach_focus_guard(result, Some(guard));
    }
    result
}

/// The `set_value` result for a Qt combo box: `effect: confirmed` only when
/// the combo box reads back the option whose row was pressed.
pub(super) fn qt_combo_pick_result(
    idx: usize,
    value: &str,
    pick: &QtComboPick,
    readback: Option<String>,
) -> ToolResult {
    let QtComboPick {
        option,
        matched,
        path,
        popup,
        popup_closed,
        ..
    } = pick;
    let verified = readback.as_deref() == Some(option.as_str());
    let mut structured = json!({
        "path": path,
        "verified": verified,
        "effect": if verified { "confirmed" } else if readback.is_some() { "suspected_noop" } else { "unverifiable" },
        "option": option,
        "matched": matched,
        "readback": readback,
        "popup_window": popup,
        "popup_closed": popup_closed,
    });
    // The option label and the read-back are the app's text: escaped and
    // cut in the text, whole in `option` and `readback`.
    let how = if option == value {
        "its text matched exactly".to_owned()
    } else if value.trim() == value {
        format!(
            "{} matched it ignoring case",
            quote_untrusted(value, SHOWN_NAME_CHARS)
        )
    } else {
        format!(
            "{} matched it ignoring case and surrounding space",
            quote_untrusted(value, SHOWN_NAME_CHARS)
        )
    };
    let mut text = format!(
        "Selected option {} of combo box element [{idx}] ({how}; path={path}): pressed \
         its row in popup window {popup}",
        quote_untrusted(option, SHOWN_NAME_CHARS)
    );
    text.push_str(if *popup_closed {
        ", which closed"
    } else {
        ", which stayed open and was closed again"
    });
    match readback.as_deref() {
        Some(seen) if verified => {
            let seen = readback_quoted(seen);
            structured["evidence"] = json!([{
                "kind": "value_readback",
                "detail": format!("combo box element [{idx}] reads back {seen}"),
            }]);
            text.push_str(&format!("; the combo box reads back {seen}."));
        }
        Some(seen) => text.push_str(&format!(
            "; but the combo box reads back {}, so the selection did not commit. Check \
             it with get_window_state.",
            readback_quoted(seen)
        )),
        None => text.push_str(
            "; the combo box could not be read back, so this is not verified. Check it with \
             get_window_state.",
        ),
    }
    ToolResult::text(text).with_structured(structured)
}

/// Numeric-aware equality for the `set_value` read-back ("40" == "40.0").
pub(super) fn values_agree(wanted: &str, observed: &str) -> bool {
    let wanted = wanted.trim();
    let observed = observed.trim();
    if wanted == observed {
        return true;
    }
    match (wanted.parse::<f64>(), observed.parse::<f64>()) {
        (Ok(a), Ok(b)) => (a - b).abs() <= 1e-6 * a.abs().max(1.0),
        _ => false,
    }
}

/// The `set_value` read-back, and whether the element is a password field.
/// The role is only asked for when the read-back is not the value itself,
/// so a matching read-back costs no extra call.
async fn set_value_readback(pid: u32, xid: u64, idx: usize, value: &str) -> (Option<String>, bool) {
    let value = value.to_owned();
    tokio::task::spawn_blocking(move || {
        let readback = crate::atspi::read_value_in(pid, xid, idx);
        let password = readback
            .as_deref()
            .is_some_and(|seen| !values_agree(&value, seen) && masked_length(seen).is_some())
            && crate::atspi::is_password_in(pid, xid, idx);
        (readback, password)
    })
    .await
    .unwrap_or((None, false))
}

/// The engine of the web page element `idx` of `pid`'s window `xid` is part
/// of, or `None` when it is not page content of a Chromium or Gecko app.
async fn web_page_engine(pid: u32, xid: u64, idx: usize) -> Option<crate::web_engine::WebEngine> {
    let in_web_content =
        crate::atspi::snapshot::cached_element(pid, xid, idx).map(|element| element.in_web_content);
    if in_web_content == Some(false) {
        return None;
    }
    let engine = tokio::task::spawn_blocking(move || {
        if is_chromium_embedder(pid) {
            Some(crate::web_engine::WebEngine::Chromium)
        } else if crate::web_engine::is_gecko_process(pid) {
            Some(crate::web_engine::WebEngine::Gecko)
        } else {
            None
        }
    })
    .await
    .ok()
    .flatten();
    web_page_engine_of(in_web_content, engine)
}

/// [`web_page_engine`]'s rule: page content (inside a web document) of a
/// Chromium or Gecko app. An element the snapshot did not record is taken
/// for page content, since an unproven confirmation is the worse mistake.
pub(super) fn web_page_engine_of(
    in_web_content: Option<bool>,
    engine: Option<crate::web_engine::WebEngine>,
) -> Option<crate::web_engine::WebEngine> {
    engine.filter(|_| in_web_content != Some(false))
}

/// `set_value` result for an AT-SPI write to a field of a web page that
/// `engine` renders. The AT-SPI read-back is the accessibility copy of the
/// field's text, not what the page's scripts saw: a page that never gets the
/// `input` event a typed character fires keeps its own copy of the value (a
/// React form submits that one). So a matching read-back is only
/// `unverifiable`, and the result says to check the page.
///
/// A read-back that does not match is the suspected no-op it is anywhere,
/// and says the route that does reach the page. Checked in a KWin X11
/// sandbox against a page that logs every event and keeps a React-style
/// value tracker: Firefox 159 answers SetTextContents, DeleteText and
/// InsertText on a page field with success and leaves the field and the page
/// as they were; Chromium 156 exposes no EditableText on one (set_value
/// types there instead); typed keys reach both pages as trusted keydown,
/// beforeinput and input events with the tracked state equal to the field.
pub(super) fn set_value_page_result(
    idx: usize,
    value: &str,
    readback: Option<String>,
    password: bool,
    engine: crate::web_engine::WebEngine,
) -> ToolResult {
    let mut result = set_value_result(idx, value, "ax", readback.clone(), password, None);
    let effect = result
        .structured_content
        .as_ref()
        .and_then(|structured| structured["effect"].as_str().map(str::to_owned));
    let (product, check, retype) = match engine {
        crate::web_engine::WebEngine::Chromium => (
            "Chromium",
            "with get_browser_state on a Chrome, Edge or Brave page, else in a screenshot",
            "browser_type on a Chrome, Edge or Brave page, else type_text with the field's x,y",
        ),
        crate::web_engine::WebEngine::Gecko => (
            "Gecko (Firefox)",
            "in a screenshot",
            "type_text with the field's x,y",
        ),
    };
    match effect.as_deref() {
        Some("confirmed") => {}
        Some("suspected_noop") => {
            let seen = readback.unwrap_or_default();
            if let Some(structured) = result.structured_content.as_mut() {
                structured["web_engine"] = json!(engine.name());
            }
            result.content = vec![pincer_core::protocol::Content::text(format!(
                "Set value of element [{idx}] to {} (path=ax) on a web page rendered by \
                 {product}; the field still reads back {}: the browser accepted the \
                 accessibility write without applying it (Firefox does this for page fields). \
                 Not changed: type the value instead ({retype}).",
                quote_untrusted(value, SHOWN_NAME_CHARS),
                readback_quoted(&seen)
            ))];
            return result;
        }
        _ => return result,
    }
    let mut structured = json!({
        "path": "ax",
        "verified": false,
        "effect": "unverifiable",
        "web_engine": engine.name(),
    });
    let wrote = if password {
        structured["readback_masked"] = json!(true);
        format!(
            "Set password field element [{idx}] to a value of {} character(s) (path=ax) on a \
             web page rendered by {product}; the field shows that many mask characters",
            value.chars().count()
        )
    } else {
        let seen = readback.unwrap_or_default();
        let wrote = format!(
            "Set value of element [{idx}] to {} (path=ax) on a web page rendered by {product}; \
             the field reads back {}",
            quote_untrusted(value, SHOWN_NAME_CHARS),
            readback_quoted(&seen)
        );
        structured["readback"] = json!(seen);
        wrote
    };
    let text = format!(
        "{wrote}, but that is the accessibility copy of its text: the page's scripts may not \
         have seen the change, and a form can still hold the old value. Not verified: check what \
         the page shows {check}; if it did not take, type the value instead ({retype})."
    );
    ToolResult::text(text).with_structured(structured)
}

/// `set_value` result with an honest verification: `verified` only when the
/// element read back the requested value. A password field (`password`)
/// reads back mask characters, so there only the length is compared.
pub(super) fn set_value_result(
    idx: usize,
    value: &str,
    path: &str,
    readback: Option<String>,
    password: bool,
    guard: Option<&crate::input::FocusGuardReport>,
) -> ToolResult {
    if let Some(masks) = readback
        .as_deref()
        .filter(|seen| password && !values_agree(value, seen))
        .and_then(masked_length)
    {
        return attach_focus_guard(set_value_masked_result(idx, value, path, masks), guard);
    }
    let normalised_path = readback
        .as_deref()
        .is_some_and(|seen| path_normalised_to_basename(value, seen));
    let verified = readback
        .as_deref()
        .is_some_and(|seen| values_agree(value, seen))
        || normalised_path;
    let mut structured = json!({
        "path": path,
        "verified": verified,
        "effect": if verified { "confirmed" } else if readback.is_some() { "suspected_noop" } else { "unverifiable" },
    });
    // The read-back is the app's text (a page script can rewrite a field):
    // escaped and cut in the text, whole in `readback`.
    let mut text = format!(
        "Set value of element [{idx}] to {} (path={path})",
        quote_untrusted(value, SHOWN_NAME_CHARS)
    );
    match readback {
        Some(seen) if normalised_path => {
            text.push_str(&format!(
                "; the field reads back {}: a file chooser navigated to the path's \
                 directory and kept the file name (the write committed).",
                readback_quoted(&seen)
            ));
            structured["readback"] = json!(seen);
        }
        Some(seen) if verified => {
            text.push_str(&format!("; read back {}.", readback_quoted(&seen)))
        }
        Some(seen) => {
            text.push_str(&format!(
                "; but the element reads back {} — the write may not have committed. \
                 Confirm with get_window_state or a screenshot.",
                readback_quoted(&seen)
            ));
            structured["readback"] = json!(seen);
            structured["escalation"] = non_ax_escalation();
        }
        None => {
            structured["readback"] = Value::Null;
            text.push_str("; not verified — confirm with get_window_state.");
        }
    }
    attach_focus_guard(ToolResult::text(text).with_structured(structured), guard)
}

/// `set_value` into a password field whose text reads back as `masks` mask
/// characters. The value is not repeated in the result. The same count as
/// the value's length is evidence the write landed (a masked-length match);
/// any other count only says the field can't be read back, never a no-op.
fn set_value_masked_result(idx: usize, value: &str, path: &str, masks: usize) -> ToolResult {
    let chars = value.chars().count();
    let matched = mask_count_fits(masks, value);
    let mut structured = json!({
        "path": path,
        "verified": matched,
        "effect": if matched { "confirmed" } else { "unverifiable" },
        "readback_masked": true,
        "masked_length": masks,
    });
    let head = format!(
        "Set password field element [{idx}] to a value of {chars} character(s) (path={path})"
    );
    let text = if matched {
        structured["evidence"] = json!([{
            "kind": "value_readback",
            "detail": format!(
                "password field element [{idx}] reads back {masks} mask characters, the \
                 value's length"
            ),
        }]);
        format!(
            "{head}; it reads back {masks} mask characters, the value's length (a masked-length \
             match: a password field shows only its length)."
        )
    } else {
        format!(
            "{head}. The field is masked, so the value can't be read back: it holds {masks} mask \
             characters for a value of {chars}. Not verified."
        )
    };
    ToolResult::text(text).with_structured(structured)
}

/// A Qt/GTK file chooser given an absolute path in its "File name" field
/// moves to that directory and shows only the final component: the readback
/// is the basename, and the write committed.
pub(super) fn path_normalised_to_basename(value: &str, seen: &str) -> bool {
    let value = value.trim();
    let seen = seen.trim();
    value.contains('/')
        && !seen.is_empty()
        && value
            .rsplit('/')
            .next()
            .is_some_and(|base| !base.is_empty() && base == seen)
}
