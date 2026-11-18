//! How a pointer action reaches the app: the X11 background and foreground
//! pointer routes, the cases that must refuse, and the results they report.

use super::*;

/// Which check vouched for the point a foreground element click pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PointCheck {
    /// The toolkit's own hit test (AT-SPI `GetAccessibleAtPoint`).
    Toolkit,
    /// The toolkit cannot hit-test the window; X11 stacking showed the
    /// window (or its popup) on top at the point after activation.
    X11Stacking,
    /// Neither: web content, or a toolkit that gave no answer.
    Unchecked,
}

impl PointCheck {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Toolkit => "atspi_hit_test",
            Self::X11Stacking => "x11_stacking",
            Self::Unchecked => "unchecked",
        }
    }
}

/// The refusal for an element click whose window's toolkit cannot hit-test
/// and whose press point X11 shows covered after activation. Nothing was
/// pressed.
pub(super) fn x11_point_cover_refusal(
    idx: usize,
    pid: u32,
    xid: u64,
    (sx, sy): (f64, f64),
    cover: crate::atspi::native::hit_test::X11Cover,
) -> ToolResult {
    use crate::atspi::native::hit_test::X11Cover;
    let (sx, sy) = (sx.round() as i32, sy.round() as i32);
    let (what, reason) = match cover {
        X11Cover::Covered {
            window,
            pid: Some(owner),
        } => (
            format!("window {window} of pid {owner} is on top there"),
            "point_covered",
        ),
        X11Cover::Covered { window, pid: None } => {
            (format!("window {window} is on top there"), "point_covered")
        }
        X11Cover::Outside => ("no window is there".to_owned(), "point_outside_window"),
        X11Cover::Unknown | X11Cover::Target | X11Cover::Popup => (
            format!("X11 could not confirm window {xid} is there"),
            "point_unconfirmed",
        ),
    };
    ToolResult::error(format!(
        "click: element [{idx}] (pid {pid}) is at ({sx}, {sy}), and the app's toolkit cannot \
         hit-test its window, so X11 checked the point after activating window {xid}: {what}. \
         Nothing was clicked. Close or move what covers it, or click by pixel from a \
         screenshot."
    ))
    .with_structured(json!({
        "code": "element_bounds_unavailable",
        "effect": "none",
        "element_index": idx,
        "reason": reason,
        "point_check": PointCheck::X11Stacking.as_str(),
    }))
}

/// What became of the open popup (menu, combo list) a click was aimed at.
/// A click on one of its items closes it, so a popup that closed is
/// `window_change` evidence that the click reached it; one still open after
/// the click means the click chose nothing that closes it, which the result
/// says instead of leaving the caller to guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PopupClickOutcome {
    pub(super) window: u64,
    pub(super) closed: bool,
    /// The click ran an item's accessible action instead of pressing it:
    /// Qt runs a menu item that way but leaves its menu open.
    pub(super) ran_action: bool,
}

impl PopupClickOutcome {
    /// The outcome of a foreground transaction that went to a popup.
    pub(super) fn from_foreground(report: &crate::input::ForegroundReport) -> Option<Self> {
        report.popup_target.map(|window| Self {
            window,
            closed: report.popup_closed,
            ran_action: false,
        })
    }

    /// Fold the outcome into a result's structured content (`popup_closed`,
    /// evidence, effect) and return the sentence for its text.
    pub(super) fn apply(&self, v: &mut Value) -> String {
        v["popup_closed"] = json!(self.closed);
        if self.closed {
            if v.get("effect").and_then(Value::as_str) != Some("confirmed") {
                push_evidence(
                    v,
                    json!({
                        "kind": "window_change",
                        "detail": format!(
                            "popup window {} closed after the click", self.window
                        ),
                    }),
                );
            }
            v["effect"] = json!("confirmed");
            v["verified"] = json!(true);
            if let Some(object) = v.as_object_mut() {
                object.remove("warning");
            }
            format!(
                " The popup (window_id {}) closed after the click, so the click reached it.",
                self.window
            )
        } else if self.ran_action {
            format!(
                " The item's accessible action was dispatched, but the popup (window_id {}) \
                 is still open: an accessible action does not close a Qt menu. Close it with \
                 press_key Escape, or click the item with delivery_mode foreground, which \
                 presses it like a user and closes the popup.",
                self.window
            )
        } else {
            format!(
                " The popup (window_id {}) is still open after the click, so the click did \
                 not choose an item that closes it. Check it with get_window_state, or close \
                 it with press_key Escape.",
                self.window
            )
        }
    }
}

/// How to check a pointer action whose effect Pincer could not confirm. A
/// `since` re-read lists only what changed; a screenshot covers surfaces that
/// are missing from the tree.
pub(super) const POINTER_CHECK: &str =
    "read what changed with get_window_state since:\"latest\", or take a screenshot";

/// Text tail for a foreground pixel click: the confirming window change when
/// there is one, else how to check it.
pub(super) fn foreground_click_suffix(path: &str, report: &crate::input::ForegroundReport) -> String {
    let popup = report
        .popup_target
        .map(|popup| format!(", popup window {popup} took the input without activation"))
        .unwrap_or_default();
    let change = report
        .window_change
        .as_deref()
        .filter(|_| report.window_change_confirms());
    match change {
        Some(change) => format!(
            "(delivery_mode=foreground, path={path}{popup}, focus_after={}); {change}.",
            report.focus_after.as_str()
        ),
        None => format!(
            "(delivery_mode=foreground, path={path}{popup}, focus_after={}); not verified: \
             {POINTER_CHECK}.",
            report.focus_after.as_str()
        ),
    }
}

/// Typed refusal for a pointer tool whose arguments name neither an element
/// nor a complete point. Before this check a call with only a pid was
/// executed at window-local (0, 0) — the top-left corner, i.e. the File menu.
pub(super) fn invalid_pointer_arguments(tool: &str, detail: &str) -> ToolResult {
    let accepted = [
        "pid + element_token from get_window_state",
        "pid/window_id + x + y (window-local pixels of window_id; coordinate_frame:\"desktop\" for screen pixels)",
        "x + y + scope:\"desktop\" (screen pixels, no pid)",
    ];
    ToolResult::error(format!(
        "{tool}: invalid arguments: {detail}. Accepted forms: {}. No input was sent.",
        accepted.join("; ")
    ))
    .with_structured(json!({
        "code": "invalid_arguments",
        "tool": tool,
        "effect": "refused",
        "detail": detail,
        "accepted_forms": accepted,
    }))
}

/// `Some(refusal)` when a pointer tool reached its coordinate path without
/// both `x` and `y` (and, by construction, without an element).
pub(super) fn require_point_args(tool: &str, args: &Value) -> Option<ToolResult> {
    let present = |name: &str| args.get(name).is_some_and(|v| v.is_number());
    match (present("x"), present("y")) {
        (true, true) => None,
        (false, false) => Some(invalid_pointer_arguments(
            tool,
            "no element_token and no x/y were given (a `target_id` is not an \
             element handle)",
        )),
        (true, false) => Some(invalid_pointer_arguments(tool, "x was given without y")),
        (false, true) => Some(invalid_pointer_arguments(tool, "y was given without x")),
    }
}

pub(super) fn parse_mouse_button(name: &str) -> u8 {
    match name {
        "right" => 3,
        "middle" => 2,
        _ => 1,
    }
}

pub(super) fn unavailable_webkit_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    (!delivery.is_foreground()
        && is_webkitgtk_embedder(pid)
        && !crate::wayland::is_inject_mode()
        && !crate::input::real_pointer_input_available())
    .then(|| {
        crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::WebKitSyntheticInput,
        )
    })
}

/// After a pointer press: name the grid cell that holds the widget focus now
/// (LibreOffice reports "D2"), so a one-row miss shows in the click result
/// instead of after six typed values. Read from the focus-event log only,
/// so it costs nothing when the press did not land on a grid.
pub(super) async fn attach_focused_cell(result: ToolResult, pid: u32) -> ToolResult {
    if result.is_error == Some(true) {
        return result;
    }
    let focused = tokio::task::spawn_blocking(move || crate::atspi::focused_control(pid))
        .await
        .ok()
        .flatten();
    let Some((role, name)) = focused.filter(|(role, name)| {
        let role = role.to_ascii_lowercase();
        (role.contains("cell") || role == "table cell") && !name.trim().is_empty()
    }) else {
        return result;
    };
    with_focused_cell(result, &role, &name)
}

/// [`attach_focused_cell`]'s additions: `focus: cell D2` in the text, the
/// raw name as `focused_cell`, and an evidence item. The cell's name and
/// role come from the app (a web grid names its own cells), so the text
/// shows the name as `untrusted_token` does (bare when one plain word,
/// else escaped and quoted) and the evidence escapes the role
/// (`table cell`) without quotes and quotes the name.
pub(super) fn with_focused_cell(mut result: ToolResult, role: &str, name: &str) -> ToolResult {
    let shown = untrusted_token(name, SHOWN_NAME_CHARS);
    result
        .content
        .push(pincer_core::protocol::Content::text(format!(
            "focus: cell {shown}"
        )));
    let mut structured = result
        .structured_content
        .take()
        .unwrap_or_else(|| json!({}));
    structured["focused_cell"] = json!(name);
    push_evidence(
        &mut structured,
        json!({
            "kind": "native_api_result",
            "detail": format!(
                "the accessible holding the focus after the press is {} {}",
                pincer_core::untrusted_text::inline_clipped(role, SHOWN_NAME_CHARS),
                quote_untrusted(name, SHOWN_NAME_CHARS)
            ),
        }),
    );
    result.structured_content = Some(structured);
    result
}

pub(super) fn unavailable_gtk_pointer_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    use crate::input::delivery::BackgroundUnavailable;
    (!delivery.is_foreground()
        && is_gtk_process(pid)
        && !crate::wayland::is_inject_mode()
        && !crate::input::real_pointer_input_available())
    .then(|| {
        // On X11 the only route left is synthetic XSendEvent, which GTK
        // drops; Wayland input reaches only the focused window.
        crate::input::delivery::background_unavailable_error(
            if crate::wayland::wayland_input_enabled() {
                BackgroundUnavailable::FocusedInputOnly
            } else {
                BackgroundUnavailable::SyntheticPointerDropped
            },
        )
    })
}

/// Background refusal for the held-button tools on X11. They have no MPX
/// route of their own: in background they post window-addressed XSendEvent
/// press, motion and release events, which Chromium, WebKitGTK and the
/// toolkits of [`synthetic_pointer_is_dropped`] (GTK3/4, VCL, Qt) drop, so
/// the call refuses as `drag`'s synthetic fallback does instead of reporting
/// a press the app never saw. Native Wayland keeps its persistent virtual
/// pointer and is not refused here.
pub(super) fn unavailable_held_button_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    use crate::input::delivery::BackgroundUnavailable;
    if delivery.is_foreground() || crate::wayland::is_wayland() {
        return None;
    }
    let reason = if is_chromium_embedder(pid) {
        BackgroundUnavailable::ChromiumInput
    } else if is_webkitgtk_embedder(pid) {
        BackgroundUnavailable::WebKitSyntheticInput
    } else if synthetic_pointer_is_dropped(pid) {
        BackgroundUnavailable::FocusedInputOnly
    } else {
        return None;
    };
    Some(crate::input::delivery::background_unavailable_error(reason))
}

/// Chromium's X11 renderer drops synthetic input sent to an occluded,
/// unfocused toplevel. Returning success here would be a silent loss, so all
/// input tools expose the same typed refusal and leave foreground activation
/// as the explicit escalation.
pub(super) fn unavailable_chromium_background(
    pid: u32,
    delivery: crate::input::delivery::DeliveryMode,
) -> Option<ToolResult> {
    if delivery.is_foreground() || !is_chromium_embedder(pid) {
        return None;
    }
    // Chromium is an XInput2 client: real (non-synthetic) events from the MPX
    // virtual master pointer/keyboard reach its renderer like the private
    // compositor's do, so only the synthetic-XSendEvent-only host must refuse.
    let focus_free_real_input =
        crate::wayland::is_inject_mode() || crate::input::real_pointer_input_available();
    if chromium_background_must_refuse(delivery.is_foreground(), focus_free_real_input, true) {
        Some(crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::ChromiumInput,
        ))
    } else {
        None
    }
}

pub(super) fn chromium_background_must_refuse(
    foreground: bool,
    focus_free_inject_mode: bool,
    chromium: bool,
) -> bool {
    chromium && !foreground && !focus_free_inject_mode
}

/// How a background pointer action was delivered on X11, with the cheap
/// post-checks the real-pointer route can make.
#[derive(Debug)]
pub(super) enum PointerRoute {
    /// Real button events from the session's MPX virtual master pointer.
    Mpx(crate::input::PointerEffect),
    /// A background pixel click resolved to an accessible under the point and
    /// fired its action (or selected it through its container).
    Atspi(crate::atspi::AtPointHit),
    /// Target-addressed synthetic `XSendEvent` (core-only toolkits), with
    /// the focus guard's report when the press ran under one.
    Synthetic(Option<crate::input::FocusGuardReport>),
    /// Activated the window first and used XTest (delivery_mode=foreground).
    Foreground,
    /// [`Self::Foreground`] after which these popups (a context menu) of the
    /// target opened.
    ForegroundPopups(Vec<crate::input::PopupWindow>),
}

impl PointerRoute {
    /// Result `path`. The Wayland routes report their own labels and build
    /// no `PointerRoute`.
    pub(super) fn path(&self) -> Option<&'static str> {
        match self {
            Self::Mpx(_) => Some(crate::input::MPX_POINTER_PATH),
            Self::Atspi(_) => Some("x11_atspi"),
            Self::Synthetic(_) => Some("x11_xsendevent"),
            Self::Foreground | Self::ForegroundPopups(_) => Some("x11_xtest_fg"),
        }
    }

    /// The popups the action opened, newest last.
    fn popups(&self) -> &[crate::input::PopupWindow] {
        match self {
            Self::Mpx(effect) => &effect.popups,
            Self::ForegroundPopups(popups) => popups,
            _ => &[],
        }
    }

    /// Structured fields shared by click / double_click / right_click / drag:
    /// `path`, `verified`, `effect`, and for the MPX route the post-checks
    /// (`focus_unchanged`, `region_diff_pct`). `verified:true` + `effect:
    /// "confirmed"` only for an AT-SPI selection read-back; a region change
    /// is recorded but never confirms.
    pub(super) fn structured(&self, mode_label: &str) -> Value {
        let mut v = json!({
            "verified": false,
            "effect": "unverifiable",
            "delivery_mode": mode_label,
        });
        if let Some(path) = self.path() {
            v["path"] = json!(path);
        }
        if let Self::Atspi(hit) = self {
            v["hit"] = json!({
                "role": hit.role,
                "name": hit.name,
                "action": hit.action,
                "path": hit.path,
            });
            if let Some(selected) = &hit.selected {
                v["selected"] = json!(selected);
            }
            // The read-back of the container's selection state is real
            // accessibility evidence; a fired action is only a native call
            // whose effect the caller still confirms.
            let evidence = if hit.selection_verified {
                v["verified"] = json!(true);
                v["effect"] = json!("confirmed");
                json!({
                    "kind": "accessibility_readback",
                    "detail": hit.describe(),
                })
            } else {
                json!({
                    "kind": "native_api_result",
                    "detail": hit.describe(),
                })
            };
            v["evidence"] = json!([evidence]);
        }
        if let Self::Synthetic(Some(guard)) = self {
            for (key, value) in guard.to_json().as_object().into_iter().flatten() {
                v[key] = value.clone();
            }
            if let Some(item) = guard.evidence_item() {
                push_evidence(&mut v, item);
            }
        }
        if let Self::ForegroundPopups(popups) = self {
            // Same fields and evidence as the MPX route's popups: a popup
            // that appeared is a window change, which confirms the action.
            v["popups_appeared"] = json!(popups.len());
            if let Some(popup) = popups.last() {
                v["popup"] = popup.to_json();
            }
            v["popups"] = json!(popups.iter().map(|p| p.to_json()).collect::<Vec<_>>());
            v["evidence"] = json!([{
                "kind": "window_change",
                "detail": format!(
                    "{} appeared after the x11_xtest_fg press; get_window_state(pid, \
                     window_id=<popup window_id>) lists its items",
                    popup_list(popups)
                ),
            }]);
            v["verified"] = json!(true);
            v["effect"] = json!("confirmed");
        }
        if let Self::Mpx(effect) = self {
            v["focus_unchanged"] = json!(effect.focus_unchanged);
            v["screen_point"] = json!([effect.x, effect.y]);
            if let Some((wx, wy)) = effect.window_point {
                v["window_point"] = json!([wx, wy]);
            }
            if let Some(pct) = effect.region_diff_pct {
                v["region_diff_pct"] = json!((pct * 100.0).round() / 100.0);
            }
            v["popups_appeared"] = json!(effect.popups_appeared);
            if let Some(popup) = effect.popups.last() {
                v["popup"] = popup.to_json();
            }
            if !effect.popups.is_empty() {
                v["popups"] = json!(effect
                    .popups
                    .iter()
                    .map(|p| p.to_json())
                    .collect::<Vec<_>>());
            }
            // Evidence the action record can publish. A popup (menu, popover,
            // combo list) appearing is a window change the public contract
            // accepts for `effect: confirmed`; a screen-region change is
            // recorded but, per the contract, cannot promote the effect on
            // its own — the text still reports it.
            let mut evidence = Vec::new();
            if effect.popups_appeared > 0 {
                evidence.push(json!({
                    "kind": "window_change",
                    "detail": format!(
                        "{} appeared after the {} at ({}, {}); get_window_state(pid, window_id=<popup window_id>) lists its items",
                        popup_list(&effect.popups),
                        self.path().unwrap_or("pointer"),
                        effect.x,
                        effect.y
                    ),
                }));
            }
            if let Some(pct) = effect
                .region_diff_pct
                .filter(|pct| *pct >= crate::input::PointerEffect::LANDED_THRESHOLD_PCT)
            {
                evidence.push(json!({
                    "kind": "screenshot_comparison",
                    "detail": format!("{pct:.2}% of the screen region around ({}, {}) changed", effect.x, effect.y),
                }));
            }
            // The background focus guard ran around the press train: report
            // what the application moved and whether it was restored. An own
            // window it opened is a window change the contract publishes.
            if let Some(guard) = &effect.focus_guard {
                for (key, value) in guard.to_json().as_object().into_iter().flatten() {
                    v[key] = value.clone();
                }
                evidence.extend(guard.evidence_item());
            }
            if let Some(foreign) = &effect.foreign_window {
                v["foreign_window"] = foreign.to_json();
                if foreign.changed() {
                    evidence.push(json!({
                        "kind": "window_change",
                        "detail": foreign.describe(),
                    }));
                }
            }
            if let Some(cover) = &effect.retargeted_to {
                v["retargeted_to"] = json!({
                    "window_id": cover.window,
                    "title": cover.title,
                });
                evidence.push(json!({
                    "kind": "window_change",
                    "detail": format!(
                        "the point was under the application's own window {} {}; the press went to it",
                        cover.window,
                        quoted_title(&cover.title)
                    ),
                }));
            }
            if !evidence.is_empty() {
                v["evidence"] = json!(evidence);
            }
            if effect.landed() || effect.foreign_window.as_ref().is_some_and(|f| f.changed()) {
                v["verified"] = json!(true);
                v["effect"] = json!("confirmed");
            }
        }
        v
    }

    /// Human-readable tail for the result text.
    pub(super) fn text_suffix(&self, mode_label: &str) -> String {
        let mut text = self.text_suffix_inner(mode_label);
        if let Some(popup) = self.popups().last() {
            text.push_str(&format!(
                " {} opened (popup: window_id={}, bounds x={} y={} {}x{}); call \
                 get_window_state(pid={}, window_id={}) to index its items and click them \
                 by element_token, or click(window_id={}, x, y) with popup-local pixels.",
                popup.describe(),
                popup.window,
                popup.x,
                popup.y,
                popup.width,
                popup.height,
                popup
                    .pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "<pid>".to_owned()),
                popup.window,
                popup.window
            ));
        }
        if let Self::Mpx(effect) = self {
            if let Some(cover) = &effect.retargeted_to {
                text.push_str(&format!(
                    " The point was under this application's own window {} {} \
                     (retargeted_to), so the press went to that window.",
                    cover.window,
                    quoted_title(&cover.title)
                ));
            }
            if let Some(foreign) = &effect.foreign_window {
                text.push(' ');
                text.push_str(&foreign.describe());
            }
            if let Some(guard) = &effect.focus_guard {
                text.push_str(&guard.summary_with_popup(effect.popups.is_empty()));
            }
        }
        if let Self::Synthetic(Some(guard)) = self {
            text.push_str(&guard.summary());
        }
        text
    }

    fn text_suffix_inner(&self, mode_label: &str) -> String {
        match self {
            Self::Atspi(hit) if hit.selected.is_some() => format!(
                "(delivery_mode={mode_label}, path=x11_atspi); {}{}",
                hit.describe(),
                if hit.selection_verified {
                    "; the item is selected.".to_owned()
                } else {
                    format!("; the selection is not verified: {POINTER_CHECK}.")
                }
            ),
            Self::Atspi(hit) => format!(
                "(delivery_mode={mode_label}, path=x11_atspi); {}; the AT-SPI action was \
                 dispatched; not verified: {POINTER_CHECK}.",
                hit.describe()
            ),
            Self::Mpx(effect) if effect.landed() => format!(
                "(delivery_mode={mode_label}, path={}, focus {}); {}the screen around the \
                 point changed ({:.1}% of the region), so the action landed.",
                crate::input::MPX_POINTER_PATH,
                if effect.focus_unchanged {
                    "untouched"
                } else {
                    "restored"
                },
                if effect.popups_appeared > 0 {
                    format!("{} popup window(s) appeared and ", effect.popups_appeared)
                } else {
                    String::new()
                },
                effect.region_diff_pct.unwrap_or(0.0)
            ),
            Self::Mpx(effect) => format!(
                "(delivery_mode={mode_label}, path={}, focus {}); real button events were \
                 delivered but the screen around the point did not visibly change \
                 ({}); not verified: {POINTER_CHECK}.",
                crate::input::MPX_POINTER_PATH,
                if effect.focus_unchanged {
                    "untouched"
                } else {
                    "restored"
                },
                effect
                    .region_diff_pct
                    .map(|pct| format!("{pct:.2}% of the region"))
                    .unwrap_or_else(|| "region capture unavailable".to_owned())
            ),
            // The popup note that text_suffix appends is the verification.
            Self::ForegroundPopups(_) => {
                format!("(delivery_mode={mode_label}, path=x11_xtest_fg);")
            }
            _ => format!(
                "(delivery_mode={mode_label}{}); not verified: {POINTER_CHECK}.",
                self.path()
                    .map(|p| format!(", path={p}"))
                    .unwrap_or_default()
            ),
        }
    }
}

/// The focus-free real-pointer route existed but the press did not go
/// through, on a toolkit that would silently drop the synthetic fallback.
#[derive(Debug)]
pub(super) struct BackgroundPointerFailed {
    pub(super) reason: String,
}

impl std::fmt::Display for BackgroundPointerFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "background pointer delivery failed: the virtual master pointer route did not \
             go through ({}) and this toolkit drops synthetic pointer events, so no click \
             was delivered",
            self.reason
        )
    }
}
impl std::error::Error for BackgroundPointerFailed {}

/// `popup window A "x" (..), popup window B (..)` for evidence text.
fn popup_list(popups: &[crate::input::PopupWindow]) -> String {
    if popups.is_empty() {
        return "a popup window".to_owned();
    }
    popups
        .iter()
        .map(crate::input::PopupWindow::describe)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every pixel action reports the point it resolved, success or refusal, so
/// a coordinate-frame mistake is visible: `window_point` is what the caller
/// passed (window-local pixels after zoom / resize translation),
/// `screen_point` is where that lands on the screen.
pub(super) fn with_pixel_points(
    mut result: ToolResult,
    screen: Option<(i32, i32)>,
    window: (i32, i32),
) -> ToolResult {
    let mut structured = result
        .structured_content
        .take()
        .unwrap_or_else(|| json!({}));
    structured["window_point"] = json!([window.0, window.1]);
    let mut note = format!(" [window_point=({}, {})", window.0, window.1);
    if let Some((sx, sy)) = screen {
        structured["screen_point"] = json!([sx, sy]);
        note.push_str(&format!(" screen_point=({sx}, {sy})"));
    }
    note.push(']');
    result.structured_content = Some(structured);
    if let Some(pincer_core::protocol::Content::Text { text, .. }) = result.content.first_mut() {
        text.push_str(&note);
    } else {
        result
            .content
            .push(pincer_core::protocol::Content::text(note.trim().to_owned()));
    }
    result
}

/// X11 no-focus-steal pixel click. On a real Xorg host the MPX uinput pointer
/// lands a *true* button event on XInput2 toolkits (GTK3/4, VCL, Qt,
/// Chromium) that silently drop synthetic `XSendEvent` pointers, so right /
/// middle / double clicks actually register. It refuses (typed
/// [`crate::input::TargetOccluded`]) when another toplevel covers the point.
/// On Xvfb / Xtigervnc / unsupported servers (`real_pointer_input_available()`
/// returns false) it falls back to the legacy `XSendEvent` path so headless
/// tests and core-only toolkits keep working; when the MPX attempt itself
/// fails, the fallback is only taken for toolkits that accept synthetic
/// events, otherwise a typed [`BackgroundPointerFailed`] is returned. `lx`,`ly`
/// are window-local; screen-absolute coords for the warp are derived here.
/// Blocking; call inside spawn_blocking.
pub(super) fn x11_pixel_click_no_focus_steal(
    cursor_id: &str,
    xid: u64,
    lx: i32,
    ly: i32,
    button: u8,
    count: usize,
) -> anyhow::Result<PointerRoute> {
    if crate::input::real_pointer_input_available() {
        if let Ok((sx, sy)) = window_local_to_screen(xid, lx as f64, ly as f64) {
            // The press train has its own save/restore fast path; the focus
            // guard around it catches what the application does afterwards
            // (a menu grab, a dialog it maps) and reports it.
            let target_pid = crate::x11::window_pid(xid);
            let (outcome, guard) = crate::input::focus_guard::guarded_settled(target_pid, || {
                Ok(crate::input::send_virtual_pointer_click(
                    cursor_id,
                    &crate::input::VirtualPointerClick {
                        target_window: xid,
                        x: sx.round() as i32,
                        y: sy.round() as i32,
                        button,
                        count,
                    },
                ))
            })?;
            match outcome {
                Ok(mut effect) => {
                    effect.focus_guard = guard;
                    effect.window_point = Some((lx, ly));
                    return Ok(PointerRoute::Mpx(effect));
                }
                Err(error) if crate::input::is_uinput_unavailable(&error) => return Err(error),
                Err(error)
                    if error
                        .downcast_ref::<crate::input::TargetOccluded>()
                        .is_some()
                        || error
                            .downcast_ref::<crate::input::PointOutsideWindow>()
                            .is_some() =>
                {
                    return Err(error)
                }
                // The synthetic fallback is a silent no-op on GTK/VCL/Qt:
                // report the real-input failure instead of a click that
                // changed nothing.
                Err(error)
                    if crate::x11::window_pid(xid).is_some_and(synthetic_press_is_dropped) =>
                {
                    tracing::warn!("MPX click failed on a toolkit that drops synthetic pointer events: {error:#}");
                    return Err(BackgroundPointerFailed {
                        reason: format!("{error:#}"),
                    }
                    .into());
                }
                Err(e) => tracing::warn!("MPX click fell back to XSendEvent: {e:#}"),
            }
        }
    }
    // Qt asks the window manager to activate a window that gets a press, and
    // a press can open a menu: run it under the focus guard, which restores
    // the focus and names the popup.
    let target_pid = crate::x11::window_pid(xid);
    let ((), guard) = crate::input::focus_guard::guarded(target_pid, || {
        crate::input::send_click(xid, lx, ly, count, button)
    })?;
    Ok(PointerRoute::Synthetic(guard))
}

/// AT-SPI roles whose `activate`/`press` action does not move the widget
/// focus into them, so a following keystroke would land elsewhere; a real
/// pointer click at their center is what a user does.
pub(super) fn element_needs_real_click(role: &str) -> bool {
    crate::atspi::is_focus_taking_role(role)
}

/// Menubar entries and the items of an open menu.
pub(super) fn element_is_menu_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "menu" | "menu item" | "check menu item" | "radio menu item"
    )
}

pub(super) fn element_click_prefers_ax(
    hyprland_foreground: bool,
    button: u8,
    count: usize,
    has_modifiers: bool,
) -> bool {
    !has_modifiers && (!hyprland_foreground || (button == 1 && count == 1))
}

pub(super) fn foreground_ax_click_unknown(pid: u32, idx: usize) -> ToolResult {
    use pincer_core::action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
    };
    let record = ActionExecutionRecord::builder(
        ActionEffect::Unverifiable,
        ActionTransport::LinuxAtSpiAction,
        RequestedDelivery::Foreground,
    )
    .actual_delivery(ActualDelivery::Unknown)
    .build()
    .expect("uncertain semantic activation has no delivered count or replay");
    let public = serde_json::to_value(record.public_result().unwrap()).unwrap();
    ToolResult::error(format!(
        "click: AT-SPI activation outcome is unknown for element [{idx}] (pid {pid}); refresh state before another action."
    ))
    .with_structured(public)
    .with_action_record(record)
}

#[cfg(test)]
#[test]
fn foreground_ax_click_error_does_not_claim_delivery_or_request_replay() {
    let result = foreground_ax_click_unknown(123, 4);
    assert_eq!(result.is_error, Some(true));
    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(wire["isError"], true);
    let public = &wire["structuredContent"];
    assert_eq!(
        *public,
        serde_json::to_value(result.action_record.unwrap().public_result().unwrap()).unwrap()
    );
    assert_eq!(public["effect"], "unverifiable");
    assert_eq!(public["route"], "accessibility");
    assert_eq!(public["delivery"]["mode"], "unknown");
    assert!(public.get("escalation").is_none());
    assert!(public["delivery"].get("delivered_count").is_none());
}

#[cfg(test)]
#[test]
fn hyprland_foreground_semantic_click_preserves_pointer_gestures() {
    assert!(element_click_prefers_ax(true, 1, 1, false));
    for (button, count) in [(2, 1), (3, 1), (1, 0), (1, 2), (1, 3), (2, 2), (3, 3)] {
        assert!(!element_click_prefers_ax(true, button, count, false));
    }
    // Preserve the existing AT-SPI eligibility for all other delivery routes.
    for button in [1, 2, 3] {
        for count in [0, 1, 2, 3] {
            assert!(element_click_prefers_ax(false, button, count, false));
            assert!(!element_click_prefers_ax(false, button, count, true));
            assert!(!element_click_prefers_ax(true, button, count, true));
        }
    }
}

pub(super) fn x11_pixel_click_no_focus_steal_modifiers(
    xid: u64,
    lx: i32,
    ly: i32,
    button: u8,
    count: usize,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    crate::input::send_click_with_modifiers(xid, lx, ly, count, button, modifiers)
}
