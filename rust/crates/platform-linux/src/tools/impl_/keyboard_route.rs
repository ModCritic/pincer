//! How a key or text reaches the app: background key and text routes, grab
//! focus, window-manager chords, terminal input, and the results they report.

use super::*;

pub(super) fn unavailable_webkit_keyboard_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    (!delivery.is_foreground() && is_webkitgtk_embedder(pid) && !crate::wayland::is_inject_mode())
        .then(|| {
            crate::input::delivery::background_unavailable_error(
                crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
            )
        })
}

/// Operator-facing hint attached to a background keyboard refusal on X11: the
/// focus-free route exists (XI2 MPX virtual master keyboard) but needs a
/// writable `/dev/uinput`, which is a one-time host setup step.
const UINPUT_KEYBOARD_HINT: &str = "Background keyboard delivery on X11 needs a virtual master \
    keyboard fed by a uinput device. Grant this process access to /dev/uinput \
    (modprobe uinput; chmod 0666 /dev/uinput, or add the driver's user to the \
    'input' group and restart it) and retry. Xvfb/Xtigervnc cannot hot-add \
    input devices, so the route is unavailable there.";

pub(super) fn with_uinput_hint(mut refusal: ToolResult) -> ToolResult {
    refusal
        .content
        .push(pincer_core::protocol::Content::text(UINPUT_KEYBOARD_HINT));
    if let Some(structured) = refusal.structured_content.as_mut() {
        structured["hint"] = json!(UINPUT_KEYBOARD_HINT);
    }
    refusal
}

/// Honest refusal for a background keyboard action that has no focus-free
/// actuator. Carries the uinput hint whenever the MPX route is missing only
/// because `/dev/uinput` is not writable (the fixable case).
pub(super) fn background_keyboard_refusal(atspi_ok: bool) -> ToolResult {
    let mut refusal = crate::input::delivery::background_unavailable_error(
        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
    );
    let bus = crate::health_report::START_ATSPI_BUS_HINT;
    let hint = format!("AT-SPI is unavailable. {bus} Or use delivery_mode \"foreground\".");
    refusal
        .content
        .extend((!atspi_ok).then(|| pincer_core::protocol::Content::text(hint)));
    if crate::wayland::is_wayland() {
        return refusal;
    }
    // Granting /dev/uinput doesn't help where the route is off for the whole
    // session (XWayland, KDE on Xorg); say that instead.
    if let Some(reason) = crate::input::mpx_route_disabled_reason() {
        let hint = format!("The focus-free keyboard route is {reason}.");
        refusal
            .content
            .push(pincer_core::protocol::Content::text(hint.clone()));
        if let Some(structured) = refusal.structured_content.as_mut() {
            structured["hint"] = json!(hint);
        }
        refusal
    } else if !crate::input::uinput_accessible() {
        with_uinput_hint(refusal)
    } else {
        refusal
    }
}

pub(super) fn atspi_up() -> bool {
    let probe = std::thread::spawn(crate::health_report::probe_a11y_bus);
    crate::wayland::is_wayland() || probe.join().unwrap_or(false)
}

pub(super) fn unavailable_gtk_keyboard_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    (!delivery.is_foreground()
        && is_gtk_process(pid)
        && !crate::wayland::is_inject_mode()
        && !crate::input::real_keyboard_input_available())
    .then(|| background_keyboard_refusal(atspi_up()))
}

/// Outcome of one background/foreground keyboard delivery on X11, funnelled
/// through `spawn_blocking`'s success type so the tool can report the route.
pub(super) enum KeyRoute {
    /// Activated the window and injected via XTest.
    Foreground(crate::input::ForegroundReport),
    /// Real key events through the session's XI2 virtual master keyboard.
    Mpx(crate::input::KeyboardDeliveryReport),
    /// Legacy synthetic XSendEvent (core-only toolkits, Tk, xterm).
    Synthetic,
    /// XSendEvent to a window that is not the active one. Qt 6 drops these
    /// (KWrite with the Test Pad active: background ctrl+End left the caret
    /// at 1:1; with KWrite active it moved to 301:1, 2026-11-11), while Tk
    /// and core X clients take them; carries the target window.
    SyntheticToInactive(u64),
    /// Written to the terminal's pty master.
    Terminal,
}

/// Background key delivery that honours a keyboard grab held by the target's
/// own popup (a Qt combo list / completer, a GTK or VCL menu): the X server
/// drops core key events from any other master keyboard to a client that
/// holds an active keyboard grab, so the virtual master keyboard route would
/// be silently lost; the core keyboard (XTest) reaches the grab holder, which
/// is the target itself, without any focus change.
pub(super) fn background_virtual_key(
    cursor_id: &str,
    pid: u32,
    xid: u64,
    key: &str,
    modifiers: &[&str],
) -> anyhow::Result<crate::input::KeyboardDeliveryReport> {
    if let Some(popup) = crate::input::popup_of_pid(pid) {
        return crate::input::send_keys_under_popup_grab(pid, popup, || {
            crate::input::send_key_xtest(key, modifiers)
        });
    }
    crate::input::send_virtual_keyboard_key(cursor_id, xid, key, modifiers)
}

/// Text counterpart of [`background_virtual_key`].
pub(super) fn background_virtual_text(
    cursor_id: &str,
    pid: u32,
    xid: u64,
    text: &str,
) -> anyhow::Result<crate::input::KeyboardDeliveryReport> {
    if let Some(popup) = crate::input::popup_of_pid(pid) {
        return crate::input::send_keys_under_popup_grab(pid, popup, || {
            crate::input::send_type_text_xtest(text)
        });
    }
    crate::input::send_virtual_keyboard_text(cursor_id, xid, text)
}

pub(super) fn is_popup_keyboard_grab(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::input::PopupKeyboardGrab>()
        .is_some()
}

/// Deliver a background key/chord: prefer the focus-free real-input route
/// (MPX virtual master keyboard) when the host supports it, otherwise the
/// legacy synthetic XSendEvent. A uinput permission failure is surfaced as is
/// (the tool turns it into the refusal + hint); any other MPX failure falls
/// back to XSendEvent for the toolkits that still accept it, and is reported
/// as an error for the ones known to drop it.
pub(super) fn background_key_route(
    cursor_id: &str,
    pid: u32,
    xid: u64,
    px_target: Option<(i32, i32)>,
    key: &str,
    modifiers: &[&str],
    element_index: Option<usize>,
) -> anyhow::Result<KeyRoute> {
    if crate::input::real_keyboard_input_available() {
        // The element GrabFocus and the delivery run under one focus guard:
        // GTK can raise/activate the toplevel for either, and the guard
        // restores the user's focus and reports it (see `focus_guard`).
        let delivered = crate::input::focus_guard::guarded(Some(pid), || {
            grab_focus_for_background(pid, xid, element_index)?;
            background_virtual_key(cursor_id, pid, xid, key, modifiers)
        });
        match delivered {
            Ok((mut report, guard)) => {
                report.focus_guard = guard;
                return Ok(KeyRoute::Mpx(report));
            }
            Err(error) if crate::input::is_uinput_unavailable(&error) => return Err(error),
            Err(error) if is_popup_keyboard_grab(&error) => return Err(error),
            Err(error)
                if is_grab_focus_failure(&error)
                    || is_gtk_process(pid)
                    || synthetic_pointer_is_dropped(pid) =>
            {
                return Err(error.context("virtual master keyboard delivery failed"));
            }
            Err(error) => tracing::warn!("MPX keyboard fell back to XSendEvent: {error}"),
        }
    }
    if let Some(index) = element_index {
        if !crate::atspi::focus_element(pid, xid, index)? {
            anyhow::bail!("AT-SPI Component.GrabFocus returned false for element {index}; {GRAB_FOCUS_RETRY}");
        }
    }
    match px_target {
        Some((x, y)) => crate::input::send_key_at(xid, x, y, key, modifiers),
        None => crate::input::send_key(xid, key, modifiers),
    }?;
    // A pixel focus click (px_target) can have activated the window, so the
    // check runs after delivery.
    Ok(
        if crate::x11::active_window().is_some_and(|active| active != xid) {
            KeyRoute::SyntheticToInactive(xid)
        } else {
            KeyRoute::Synthetic
        },
    )
}

const GRAB_FOCUS_FAILURE_PREFIX: &str = "grab_focus_failed";

/// Recovery advice for a refused GrabFocus. KWrite's editor answers false even
/// while it holds the focus (round 4 of the first-call test), and a key or
/// text sent to the window still reaches its focused widget.
pub(super) const GRAB_FOCUS_RETRY: &str = "the app would not move its focus to that element; to send \
    the input to the window's focused widget instead, call again with pid and window_id in \
    place of element_token";

/// AT-SPI `Component.GrabFocus` on the addressed element, for the background
/// keyboard routes. Failures carry a prefix so the caller never mistakes them
/// for a delivery failure worth a synthetic-event fallback.
fn grab_focus_for_background(
    pid: u32,
    xid: u64,
    element_index: Option<usize>,
) -> anyhow::Result<()> {
    let Some(index) = element_index else {
        return Ok(());
    };
    match crate::atspi::focus_element(pid, xid, index) {
        Ok(true) => Ok(()),
        Ok(false) => anyhow::bail!(
            "{GRAB_FOCUS_FAILURE_PREFIX}: AT-SPI Component.GrabFocus returned false for element {index}; {GRAB_FOCUS_RETRY}"
        ),
        Err(error) => Err(error.context(format!(
            "{GRAB_FOCUS_FAILURE_PREFIX}: AT-SPI Component.GrabFocus for element {index}"
        ))),
    }
}

fn is_grab_focus_failure(error: &anyhow::Error) -> bool {
    error.to_string().starts_with(GRAB_FOCUS_FAILURE_PREFIX)
}

/// Tool result for a completed keyboard route (`press_key` / `hotkey`). The
/// text is written after the effect is decided, so it never says "not
/// verified" next to an `effect: confirmed` line.
pub(super) fn key_route_result(action: &str, route: KeyRoute, mode_label: &str) -> ToolResult {
    match route {
        KeyRoute::Foreground(report) => ToolResult::text(format!(
            "{action} as a real key event {}",
            foreground_key_tail(&report)
        ))
        .with_structured(foreground_structured(
            "key_events_fg",
            report,
            serde_json::Map::new(),
        )),
        KeyRoute::Mpx(report) => {
            let mut structured = report.to_json();
            structured["verified"] = json!(false);
            structured["delivery_mode"] = json!(mode_label);
            structured["effect"] = json!("unverifiable");
            let mut text = format!(
                "{action} as real key events {} (delivery_mode={mode_label})",
                report.route_phrase()
            );
            let closed_target = report
                .focus_guard
                .as_ref()
                .is_some_and(|guard| guard.closed_window.is_some());
            let opened_window = report
                .focus_guard
                .as_ref()
                .and_then(|guard| guard.opened_window())
                .filter(|_| report.delivery_confirmed);
            if closed_target && report.delivery_confirmed {
                // The key closed the window that held the focus (Escape /
                // Return on a dialog): the lost virtual focus is the effect,
                // and the window change is evidence the contract publishes.
                structured["effect"] = json!("confirmed");
                structured["verified"] = json!(true);
                text.push_str(
                    "; the window that held the focus closed after the key, which confirms it.",
                );
            } else if let Some(window) = opened_window {
                // The key mapped a new top-level of the target (F4 -> Position
                // and Size, ctrl+1 -> Format Cells): the window change is the
                // effect; name it and the call that targets it.
                structured["effect"] = json!("confirmed");
                structured["verified"] = json!(true);
                structured["window_opened"] = json!({
                    "window_id": window.window,
                    "title": window.title,
                    "focused": window.focused,
                });
                if let Some(owner) = window.owner_pid {
                    structured["window_opened"]["pid"] = json!(owner);
                }
                text.push_str(&format!(
                    "; the key opened window {}, which confirms it. Call {} to index it and act there.",
                    window.describe(),
                    window.read_call().unwrap_or_else(|| format!(
                        "get_window_state(pid, window_id={})",
                        window.window
                    ))
                ));
            } else if !report.virtual_focus_held || !report.delivery_confirmed {
                structured["effect"] = json!("suspected_noop");
                text.push_str(
                    ". Warning: the target lost the virtual keyboard's focus or the server \
                     did not confirm the last key; the keys may not have landed.",
                );
            } else {
                text.push_str(&format!("; {KEY_EFFECT_UNVERIFIED}."));
            }
            text.push_str(&report.delivery_notes());
            ToolResult::text(text).with_structured(structured)
        }
        KeyRoute::Synthetic => ToolResult::text(format!(
            "{action} (delivery_mode={mode_label}); {KEY_EFFECT_UNVERIFIED}."
        ))
        .with_structured(json!({ "verified": false, "delivery_mode": mode_label })),
        KeyRoute::SyntheticToInactive(window) => ToolResult::text(format!(
            "{action} as synthetic X key events (delivery_mode={mode_label}) to window {window}, \
             which is not the active window. Qt apps ignore these keys while their window is \
             not the active one (GTK apps are refused before sending), so if nothing changed, \
             retry with delivery_mode foreground."
        ))
        .with_structured(json!({
            "verified": false,
            "delivery_mode": mode_label,
            "target_window_active": false,
        })),
        KeyRoute::Terminal => ToolResult::text(format!("{action} (delivery_mode={mode_label})."))
            .with_structured(json!({ "verified": false, "delivery_mode": mode_label })),
    }
}

/// What a key result tells the caller to check when nothing confirmed the
/// key. A `since` re-read lists only what changed (text, caret, title), the
/// cheap check round 2 of the first-call test used instead of a screenshot.
pub(super) const KEY_EFFECT_UNVERIFIED: &str =
    "not verified: read what changed with get_window_state since:\"latest\", or check it \
     with verify_state";

/// Text tail for a foreground key delivery, in step with the effect
/// [`apply_foreground_report`] decides: the window change that confirms it,
/// the focus that left the target (a suspected no-op), or a request to check.
pub(super) fn foreground_key_tail(report: &crate::input::ForegroundReport) -> String {
    let focus = report.focus_after.as_str();
    let change = report
        .window_change
        .as_deref()
        .filter(|_| report.window_change_confirms());
    match change {
        Some(change) => format!(
            "(delivery_mode=foreground, focus_after={focus}); confirmed by a window change: {change}."
        ),
        None if report.focus_after == crate::input::FocusAfter::Elsewhere => format!(
            "(delivery_mode=foreground, focus_after={focus}); the input focus left the target \
             process before the check, so the input may have reached another window: check \
             with a screenshot."
        ),
        None => format!("(delivery_mode=foreground, focus_after={focus}); {KEY_EFFECT_UNVERIFIED}."),
    }
}

/// A chord the window manager, not the application, acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WmChord {
    /// Alt+F4: close the target window (`_NET_CLOSE_WINDOW`).
    CloseWindow,
    /// Super+*, Alt+Tab, Ctrl+Alt+*, Alt+F7/F8/F10/space/Escape: a WM binding
    /// with no window-addressed equivalent.
    Unavailable,
}

/// mutter's key bindings are passive grabs on the core keyboard. A chord from
/// the session's virtual master keyboard (background delivery) reaches the
/// client window, never the WM, so `alt+F4` / `super+a` / `alt+tab` silently
/// do nothing. Recognise them so the tool can act (close the window through
/// EWMH) or refuse with a typed reason instead of reporting `unverifiable`.
pub(super) fn wm_chord_kind(key: &str, modifiers: &[String]) -> Option<WmChord> {
    use crate::input::modifier::Modifier;
    let has = |wanted: Modifier| {
        modifiers
            .iter()
            .any(|name| Modifier::parse(name) == Some(wanted))
    };
    let alt = has(Modifier::Alt);
    let ctrl = has(Modifier::Ctrl);
    let shift = has(Modifier::Shift);
    let super_ = has(Modifier::Super);
    let key_lower = key.trim().to_ascii_lowercase();
    if super_ {
        return Some(WmChord::Unavailable);
    }
    if alt && ctrl {
        return Some(WmChord::Unavailable);
    }
    if alt && !ctrl && key_lower == "f4" {
        return Some(WmChord::CloseWindow);
    }
    if alt
        && !ctrl
        && !shift
        && matches!(
            key_lower.as_str(),
            "tab" | "f5" | "f7" | "f8" | "f10" | "space" | "escape" | "esc"
        )
    {
        return Some(WmChord::Unavailable);
    }
    if alt && shift && key_lower == "tab" {
        return Some(WmChord::Unavailable);
    }
    None
}

/// Background delivery of a WM-level chord: close the window for Alt+F4,
/// refuse the rest (`wm_chord_unavailable`). `None` for ordinary chords.
pub(super) async fn wm_chord_background(
    pid: u32,
    xid: u64,
    key: &str,
    modifiers: &[String],
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    if delivery.is_foreground()
        || crate::wayland::wayland_input_enabled()
        || crate::wayland::is_inject_mode()
    {
        return None;
    }
    let chord = wm_chord_kind(key, modifiers)?;
    let display = format!("{}+{}", modifiers.join("+"), key);
    match chord {
        WmChord::CloseWindow => Some(close_window_background(pid, xid, &display).await),
        WmChord::Unavailable => {
            let hint = format!(
                "{display} is a window-manager binding (mutter's passive grab on the core \
                 keyboard); a virtual-keyboard chord in background mode reaches the window, \
                 not the WM, so it would do nothing. Use the driver instead: launch_app / \
                 list_windows / bring_to_front to switch windows, hotkey alt+F4 (or \
                 kill_app) to close one, or retry with delivery_mode:\"foreground\"."
            );
            Some(
                ToolResult::error(format!("Refused {display} on pid {pid}: {hint}"))
                    .with_structured(json!({
                        "code": "wm_chord_unavailable",
                        "effect": "refused",
                        "verified": false,
                        "delivery_mode": "background",
                        "chord": display,
                        "hint": hint,
                    })),
            )
        }
    }
}

/// Alt+F4 in background mode: ask the WM to close the window
/// (`_NET_CLOSE_WINDOW` -> `WM_DELETE_WINDOW`) and watch it go.
async fn close_window_background(pid: u32, xid: u64, display: &str) -> ToolResult {
    let before: Vec<crate::x11::WindowInfo> =
        tokio::task::spawn_blocking(move || crate::x11::list_windows(Some(pid)))
            .await
            .unwrap_or_default();
    let title = before
        .iter()
        .find(|w| w.xid == xid)
        .map(|w| w.title.clone())
        .unwrap_or_default();
    let sent = tokio::task::spawn_blocking(move || crate::x11::close_window(xid, pid)).await;
    match sent {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            return ToolResult::error(format!(
                "{display} on pid {pid}: could not request the window manager to close \
                 window {xid}: {error:#}"
            ))
            .with_structured(json!({
                "code": "close_window_failed",
                "effect": "none",
                "detail": error.to_string(),
            }))
        }
        Err(error) => return ToolResult::error(format!("Task error: {error}")),
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    let mut closed = false;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let viewable =
            tokio::task::spawn_blocking(move || crate::x11::window_is_viewable(xid)).await;
        if matches!(viewable, Ok(false)) {
            closed = true;
            break;
        }
    }
    let after: Vec<crate::x11::WindowInfo> =
        tokio::task::spawn_blocking(move || crate::x11::list_windows(Some(pid)))
            .await
            .unwrap_or_default();
    let opened = after
        .iter()
        .find(|w| w.is_on_screen && !before.iter().any(|old| old.xid == w.xid));
    close_window_result(pid, xid, display, &title, closed, opened)
}

/// close_window_background's result once the request went out: `closed`
/// when the window stopped being viewable, `opened` the first new window of
/// the pid (a save or confirm dialog). The titles are app text, escaped and
/// quoted (`quoted_title`); the structured `opened_window` keeps the raw one.
pub(super) fn close_window_result(
    pid: u32,
    xid: u64,
    display: &str,
    title: &str,
    closed: bool,
    opened: Option<&crate::x11::WindowInfo>,
) -> ToolResult {
    let quoted = if title.is_empty() {
        String::new()
    } else {
        format!(" {}", quoted_title(title))
    };
    // The close request is an XSendEvent client message to the root (the
    // transport the action contract knows as x11_xsendevent).
    let mut structured = json!({
        "path": "x11_xsendevent",
        "mechanism": "_NET_CLOSE_WINDOW",
        "delivery_mode": "background",
        "chord": display,
        "target_window": xid,
        "window_closed": closed,
    });
    if closed {
        structured["verified"] = json!(true);
        structured["effect"] = json!("confirmed");
        structured["closed_window"] = json!(xid);
        structured["evidence"] = json!([{
            "kind": "window_change",
            "detail": format!("window {xid}{quoted} of pid {pid} closed after _NET_CLOSE_WINDOW"),
        }]);
        let mut text = format!(
            "Pressed {display} on pid {pid} as a window-manager close request \
             (_NET_CLOSE_WINDOW -> WM_DELETE_WINDOW; a virtual-keyboard Alt+F4 never reaches \
             the window manager): window {xid}{quoted} closed."
        );
        if let Some(w) = opened {
            text.push_str(&format!(
                " The application then opened window {} {}.",
                w.xid,
                quoted_title(&w.title)
            ));
            structured["opened_window"] = json!({ "window_id": w.xid, "title": w.title });
        }
        ToolResult::text(text).with_structured(structured)
    } else {
        structured["verified"] = json!(false);
        structured["effect"] = json!("unverifiable");
        let mut text = format!(
            "Pressed {display} on pid {pid} as a window-manager close request \
             (_NET_CLOSE_WINDOW -> WM_DELETE_WINDOW): window {xid}{quoted} is still mapped \
             after 2 s."
        );
        if let Some(w) = opened {
            text.push_str(&format!(
                " The application opened window {} {} instead (a save / confirm \
                 dialog?); act on it with get_window_state / click.",
                w.xid,
                quoted_title(&w.title)
            ));
            structured["opened_window"] = json!({ "window_id": w.xid, "title": w.title });
            structured["evidence"] = json!([{
                "kind": "window_change",
                "detail": format!(
                    "the application opened window {} {} after the close request",
                    w.xid,
                    quoted_title(&w.title)
                ),
            }]);
        } else {
            text.push_str(" Confirm with list_windows; kill_app force-closes the application.");
        }
        ToolResult::text(text).with_structured(structured)
    }
}

/// `press_key` accepts `"alt+F4"` / `"ctrl+shift+t"` style keys: everything
/// before the last `+` is a modifier. A bare `"+"` stays the plus key.
pub(super) fn split_key_combo(key: &str) -> (Vec<String>, String) {
    if key.len() <= 1 || !key.contains('+') {
        return (Vec::new(), key.to_owned());
    }
    let mut parts: Vec<&str> = key.split('+').collect();
    let last = parts.pop().unwrap_or("");
    let last = if last.is_empty() { "+" } else { last };
    let modifiers: Vec<String> = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    if !modifiers.is_empty() && modifiers.iter().all(|m| is_modifier(m)) {
        (modifiers, last.to_owned())
    } else {
        (Vec::new(), key.to_owned())
    }
}

#[cfg(test)]
#[test]
fn background_keyboard_refusal_names_missing_atspi_bus() {
    let plain = crate::input::delivery::background_unavailable_error(
        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
    );
    let text = |refusal: &ToolResult| -> String {
        refusal
            .content
            .iter()
            .filter_map(|content| match content {
                pincer_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    };
    let (missing, reachable) = (
        background_keyboard_refusal(false),
        background_keyboard_refusal(true),
    );
    for needle in [
        "AT-SPI is unavailable",
        "at-spi-bus-launcher",
        "delivery_mode \"foreground\"",
    ] {
        assert!(
            text(&missing).contains(needle),
            "{needle} missing from {}",
            text(&missing)
        );
    }
    assert!(!text(&reachable).contains("AT-SPI is unavailable"));
    for refusal in [missing, reachable] {
        let structured = refusal.structured_content.unwrap();
        let expected = plain.structured_content.as_ref().unwrap();
        assert_eq!(structured["code"], expected["code"]);
        assert_eq!(structured["escalation"], expected["escalation"]);
    }
}

pub(super) fn is_terminal_process(pid: u32) -> bool {
    // Canonical list lives in `crate::terminal::TERMINAL_PROCESS_NAMES`
    // — keeps the additive contract centralised. Adding a new terminal
    // here means appending one string in `crate::terminal`.
    match crate::proc_fs::ProcessFacts::program_name(pid).as_deref() {
        Some(name) => crate::terminal::is_terminal_process_name(name),
        None => false,
    }
}

fn terminal_descendant_ttys(pid: u32) -> Vec<PathBuf> {
    let mut descendants = crate::proc_fs::ProcessFacts::new().descendants(pid, usize::MAX);
    descendants.sort_unstable();

    let mut ttys = Vec::new();
    for child in descendants {
        let tty = match fs::read_link(format!("/proc/{child}/fd/0")) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if tty.starts_with("/dev/pts/") {
            ttys.push(tty);
        }
    }
    ttys
}

fn terminal_tty_for_window(pid: u32, xid: u64) -> Option<PathBuf> {
    if !is_terminal_process(pid) {
        return None;
    }
    let mut windows = crate::x11::list_windows(Some(pid));
    windows.sort_by_key(|w| w.xid);
    let window_index = windows.iter().position(|w| w.xid == xid)?;
    let ttys = terminal_descendant_ttys(pid);
    ttys.get(window_index).cloned()
}

/// True when `key` names the Enter key in the shared X keysym vocabulary
/// (`key_name_to_keysym`). The terminal pty short-circuit below applies to
/// every spelling of that physical key — `enter`, `return`, any case — so an
/// agent following the documented key names cannot silently lose the keypress
/// on a terminal window (terminals discard synthetic XSendEvent keys).
pub(super) fn is_enter_key(key: &str) -> bool {
    crate::input::key_name_to_keysym(key).ok() == Some(0xFF0D)
}

/// Type into a terminal window without touching X focus. Resolves the window's
/// pty, then borrows the emulator's master fd and writes to it (see
/// `crate::tty`). Returns `Ok(false)` when the target isn't a terminal we can
/// reach this way so the caller falls back to the generic XSendEvent path.
pub(super) fn inject_terminal_input(pid: u32, xid: u64, text: &str) -> anyhow::Result<bool> {
    let Some(tty) = terminal_tty_for_window(pid, xid) else {
        return Ok(false);
    };
    // tty is `/dev/pts/<N>`; the emulator (pid) holds the master for the same N.
    let Some(ptn) = tty
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return Ok(false);
    };
    crate::tty::inject_via_master(pid, ptn, text)
}

// ── px-focus helper (keyboard family) ───────────────────────────────────────

/// px-focus for the keyboard family (type_text / press_key / hotkey): pixel-click
/// at (x,y) to establish real renderer focus before a keystroke — the *element px
/// action* form of a keyboard tool. Reuses ClickTool's exact coordinate
/// translation + delivery_mode so it lands on the same pixel a px-click would.
/// `Ok(())` on success; `Err(ToolResult)` short-circuits the caller.
///
/// Retains the parent's authenticated session admission for the nested click.
/// `point` is the window-local pixel to click for focus.
pub(super) async fn focus_by_pixel(
    state: &Arc<ToolState>,
    pid: u32,
    window_id: Option<u64>,
    (x, y): (f64, f64),
    foreground: bool,
    parent_args: &Value,
    from_zoom: bool,
) -> Result<(), ToolResult> {
    let mut click_args = json!({
        "pid": pid, "x": x, "y": y,
        "delivery_mode": if foreground { "foreground" } else { "background" },
    });
    if let Some(wid) = window_id {
        click_args["window_id"] = json!(wid);
    }
    for field in [
        "session",
        "_session_id",
        "_transport_session_id",
        "cursor_id",
        "scope",
        "coordinate_frame",
    ] {
        if let Some(value) = parent_args.get(field) {
            click_args[field] = value.clone();
        }
    }
    if from_zoom {
        click_args["from_zoom"] = json!(true);
    }
    let focus = ClickTool {
        state: state.clone(),
    }
    .invoke(click_args)
    .await;
    if focus.is_error == Some(true) {
        return Err(focus);
    }
    // Brief settle so the renderer registers focus before the keystrokes.
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    Ok(())
}

/// Establish widget-local focus inside a nested-compositor target without
/// changing the compositor's focused toplevel. AX uses Component.GrabFocus;
/// PX sends a private per-surface left click at the requested local point.
pub(super) async fn focus_nested_inject_target(
    pid: u32,
    window_id: u64,
    element_index: Option<usize>,
    pixel: Option<(f64, f64)>,
) -> Result<(), ToolResult> {
    if let Some(index) = element_index {
        return match tokio::task::spawn_blocking(move || {
            crate::atspi::focus_element(pid, window_id, index)
        })
        .await
        {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err(ToolResult::error(format!(
                "AT-SPI Component.GrabFocus returned false for element {index}; {GRAB_FOCUS_RETRY}"
            ))),
            Ok(Err(error)) => Err(ToolResult::error(error.to_string())),
            Err(error) => Err(ToolResult::error(format!("Task error: {error}"))),
        };
    }
    if let Some((x, y)) = pixel {
        return match tokio::task::spawn_blocking(move || {
            crate::wayland::inject_click(pid, window_id, x, y, 1, 1)
        })
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(ToolResult::error(error.to_string())),
            Err(error) => Err(ToolResult::error(format!("Task error: {error}"))),
        };
    }
    Ok(())
}
