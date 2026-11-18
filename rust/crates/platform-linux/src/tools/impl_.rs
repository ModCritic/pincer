//! Real Linux tool implementations (compiled only on Linux).

use async_trait::async_trait;
use pincer_contract::{
    ClickButton, DragInput, GetCursorPositionInput, GetDesktopStateInput, GetScreenSizeInput,
    HotkeyInput, InvokeMenuInput, MoveCursorInput, PressKeyInput, ScrollInput, TypeTextInput,
};
use pincer_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef, ToolRegistry},
    tool_args::{parse_legacy_click_input, parse_typed_input, parse_typed_projection, ArgsExt},
    window_target::{PidOnlyWindowTargetGuard, WindowTargetCandidate, WindowTargetCandidates},
};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::atspi::Snapshots;
use crate::input::{
    quote_untrusted, quoted_title, untrusted_token, SHOWN_COMMAND_CHARS, SHOWN_NAME_CHARS,
};
use cursor_overlay::CursorRegistry;

mod click;
mod click_gestures;
mod coords;
mod cursor_reveal;
mod discovery;
mod drag;
mod held_button;
mod hyprland_route;
mod input_common;
mod keyboard_route;
mod launch;
mod pointer_route;
mod scroll;
mod state;
mod text;
mod toolkit;
mod window_ops;
mod window_state;
mod window_target;

use self::click::*;
use self::click_gestures::*;
use self::coords::*;
use self::cursor_reveal::*;
use self::discovery::*;
use self::drag::*;
use self::held_button::*;
use self::hyprland_route::*;
use self::input_common::*;
use self::keyboard_route::*;
use self::launch::*;
use self::pointer_route::*;
use self::scroll::*;
use self::state::*;
use self::text::*;
use self::toolkit::*;
use self::window_ops::*;
use self::window_state::*;
use self::window_target::*;

#[cfg(test)]
mod pid_window_resolver_tests;

use pincer_core::snapshot_store::ZoomContext;

#[cfg(test)]
mod region_frame_tests;

#[cfg(test)]
mod list_windows_tests;

#[cfg(test)]
mod untrusted_text_tests;

#[cfg(test)]
mod megapixel_cap_tests;

#[cfg(test)]
mod get_window_state_actions_tests;

mod appimage;
mod launch_child;
use launch_child::{LaunchExit, LaunchedChild};

#[cfg(test)]
mod launch_app_tests;
#[cfg(test)]
mod launch_scope_tests;

#[cfg(test)]
mod launch_resolution_tests;

#[cfg(test)]
mod pointer_argument_tests;

#[cfg(test)]
mod held_button_live_tests;

#[cfg(test)]
mod type_text_readback_tests;

#[cfg(test)]
mod click_delegation_tests;

// ── type_text ─────────────────────────────────────────────────────────────────

pub struct TypeTextTool {
    state: Arc<ToolState>,
}
static TYPE_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for TypeTextTool {
    fn def(&self) -> &ToolDef {
        TYPE_DEF.get_or_init(|| ToolDef {
            name: "type_text".into(),
            description: "Type text at the caret of a window's field. With element_token (a \
                text row from get_window_state) the text goes in through AT-SPI editable text \
                in the background, Qt and GTK alike, and is read back. Without it the \
                background route guesses the field (the app's focused or only editable one \
                through AT-SPI, or a terminal's pty) or sends real key events from a virtual \
                keyboard where the host has one (X11 with a writable /dev/uinput, not KDE \
                Plasma). When it cannot, it refuses with background_unavailable (KWrite on KDE \
                Plasma X11, for one; XWayland and Wayland have no such keyboard either): pass \
                element_token, or delivery_mode \"foreground\" to type real keys into the \
                activated window. set_value replaces a field's whole value.".into(),
            input_schema: json!({
                "type":"object","required":["text"],"properties":{
                    "session": pincer_core::tool_schema::session_schema(),
                    "pid":{"type":"integer","description":"Target process ID. Omit with scope \"desktop\" to type into the focused application."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                    "text":{"type":"string","description":"Text to type."},
                    "element_token": pincer_core::tool_schema::element_token_schema(),
                    "x":{"type":"number","description":"Pixel X of the field to type into (the element px action form). Pass x,y (no element_token) and the tool pixel-clicks there to establish real renderer focus, then types. Use for Chromium/Electron inputs the AX path can't reach. Window-local pixels of the target window's own get_window_state screenshot by default (same convention as click); for get_desktop_state pixels pass scope:\"desktop\" (or coordinate_frame:\"desktop\"); a bare pid+x/y is otherwise misinterpreted as window-local and can focus the wrong widget."},
                    "y":{"type":"number","description":"Pixel Y of the field (see x)."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with no pid to type into the focused application. Default \"window\"."},
                    "coordinate_frame": coordinate_frame_schema(),
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        if (args.opt_str("scope").as_deref() == Some("desktop")
            || coordinate_frame_is_desktop(&args))
            && args.get("pid").is_none()
        {
            let input = match parse_typed_projection::<TypeTextInput>("type_text", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let text = pincer_core::text_sanitize::strip_trailing_agent_protocol_tags(&input.text)
                .into_owned();
            position_named_session_keyboard_cursor(&self.state, &args, 0, 0, None, None, false)
                .await;
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland { "wayland_focused" } else { "xtest" };
            let result = tokio::task::spawn_blocking(move || {
                if wayland {
                    crate::wayland::type_text_focused(&text)
                } else {
                    crate::input::send_type_text_xtest(&text)
                }
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text("Typed text into the focused desktop application.")
                    .with_structured(
                        json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                    ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let mut pid = args.u64_or("pid", 0) as u32;
        let text_raw = match args.require_str("text") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Strip trailing agent-protocol closing tags — see
        // pincer_core::text_sanitize docs for rationale.
        let text =
            pincer_core::text_sanitize::strip_trailing_agent_protocol_tags(&text_raw).into_owned();
        let resolved = match self.state.snapshots.resolve(pid as i32, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (resolved_elem_idx, resolved_window_id) = match &resolved {
            pincer_core::element_token::ResolvedElement::Element {
                element_index,
                window_id,
                ..
            } => (Some(*element_index), Some(*window_id)),
            pincer_core::element_token::ResolvedElement::None => (None, None),
        };
        let xid_opt = resolved_window_id.or(args.opt_u64("window_id"));

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
        if isolated_hyprland_background(delivery)
            && resolved_elem_idx.is_none()
            && args.get("x").is_none()
            && args.get("y").is_none()
        {
            if xid_opt.is_none() {
                return isolated_hyprland_refusal(
                    "an exact window_id is required for isolated text",
                );
            }
            match crate::wayland::hyprland_input::text_actions(&text) {
                Ok(actions) if !actions.is_empty() => {}
                Ok(_) => return isolated_hyprland_refusal("background text must not be empty"),
                Err(error) => return isolated_hyprland_refusal(error.to_string()),
            }
            position_named_session_keyboard_cursor(&self.state, &args, pid, xid, None, None, true)
                .await;
            let owner = named_session_cursor_key(&args);
            let (_cancellation, dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_background_text(
                        owner,
                        pid,
                        xid,
                        &text,
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
        if resolved_elem_idx.is_none() {
            if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, true) {
                return refusal;
            }
        }

        let px = args.get("x").and_then(|value| value.as_f64());
        let py = args.get("y").and_then(|value| value.as_f64());
        if px.is_some() != py.is_some() {
            return invalid_pointer_arguments(
                "type_text",
                "x and y must be passed together (a pixel focus point), or neither",
            );
        }
        if px.is_some() && resolved_elem_idx.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to type_text, not both.",
            );
        }

        if hyprland_foreground(delivery) {
            if xid_opt.is_none() {
                return foreground_hyprland_refusal("an exact window_id is required");
            }
            // Native editables retain their verifiable, addressed AT-SPI route.
            if !is_chromium_embedder(pid) && !is_webkitgtk_embedder(pid) {
                if let Some(index) = resolved_elem_idx {
                    let text_ax = text.clone();
                    if matches!(
                        tokio::task::spawn_blocking(move || crate::atspi::type_into_editable_at(
                            pid, xid, index, &text_ax
                        ))
                        .await,
                        Ok(Ok(()))
                    ) {
                        position_named_session_keyboard_cursor(
                            &self.state,
                            &args,
                            pid,
                            xid,
                            resolved_elem_idx,
                            None,
                            true,
                        )
                        .await;
                        return type_text_ax_result(
                            pid,
                            text.chars().count(),
                            "via targeted AT-SPI",
                        );
                    }
                }
            }
            match crate::wayland::hyprland_input::text_actions(&text) {
                Ok(actions) if !actions.is_empty() => {}
                Ok(_) => return foreground_hyprland_refusal("foreground text must not be empty"),
                Err(error) => return foreground_hyprland_refusal(error.to_string()),
            }
            if let Err(error) = focus_hyprland_foreground(
                &self.state,
                &args,
                pid,
                xid,
                resolved_elem_idx,
                px.zip(py),
            )
            .await
            {
                return error;
            }
            if named_session_cursor_key(&args).is_some() {
                if px.is_none() {
                    position_named_session_keyboard_cursor(
                        &self.state,
                        &args,
                        pid,
                        xid,
                        resolved_elem_idx,
                        None,
                        true,
                    )
                    .await;
                }
            } else {
                announce_keyboard_target(&args, pid, xid, resolved_elem_idx, px.zip(py)).await;
            }
            let owner = named_session_cursor_key(&args);
            let (_cancellation, dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_foreground_text(
                        owner,
                        pid,
                        xid,
                        &text,
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
            return hyprland_input_result(
                match dispatch.await {
                    Ok(result) => result,
                    Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                        error.into(),
                        0,
                    )),
                },
                true,
            );
        }

        position_named_session_keyboard_cursor(
            &self.state,
            &args,
            pid,
            xid,
            resolved_elem_idx,
            px.zip(py),
            true,
        )
        .await;

        let text_len = text.chars().count();
        // Native toolkit editables have a stronger focus-free route than raw
        // compositor keyboard injection. Keep Chromium/WebKit on real key events
        // because their accessibility bridges may echo a write that never reaches
        // renderer-owned state.
        if let Some(idx) = resolved_elem_idx.filter(|_| {
            crate::wayland::is_inject_mode()
                && !is_chromium_embedder(pid)
                && !is_webkitgtk_embedder(pid)
        }) {
            let password_before = password_length_before(pid, xid, idx).await;
            let text_at = text.clone();
            let targeted = tokio::task::spawn_blocking(move || {
                crate::atspi::type_into_editable_at(pid, xid, idx, &text_at)
            })
            .await;
            if let Ok(Ok(())) = targeted {
                return type_text_ax_result_verified(
                    pid,
                    xid,
                    idx,
                    &text,
                    "via targeted AT-SPI",
                    password_before,
                )
                .await;
            }
        }
        // The private nested compositor can target the owning Wayland client
        // directly. Establish widget-local focus first, without changing the
        // compositor's focused toplevel, so keys reach the addressed control.
        if crate::wayland::is_inject_mode() {
            if let Err(error) =
                focus_nested_inject_target(pid, xid, resolved_elem_idx, px.zip(py)).await
            {
                return error;
            }
            let text_w = text.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::inject_type_text(pid, xid, &text_w)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Typed {text_len} character(s) (focus-free via cua-compositor)."
                ))
                .with_structured(type_text_structured(
                    "key_events",
                    text_len,
                    false,
                )),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // ── px form: focus by pixel-click, then type into the now-focused element ──
        // Pass x,y (no element_token) for an *element px action*: pixel-click
        // the field to give the renderer the real keyboard focus the AT-SPI path
        // can't, then fall through to the focused-element type path below (it
        // escalates AT-SPI → key events and lands once focused). Reuses ClickTool's
        // exact coordinate translation + delivery_mode.
        if let (Some(cx), Some(cy)) = (px, py) {
            if let Some(refusal) = unavailable_webkit_keyboard_background(pid, delivery) {
                return refusal;
            }
            let from_zoom = args.bool_or("from_zoom", false);
            if let Err(e) = focus_by_pixel(
                &self.state,
                pid,
                Some(xid),
                (cx, cy),
                delivery.is_foreground(),
                &args,
                from_zoom,
            )
            .await
            {
                return e;
            }
            // resolved_elem_idx stays None → the type path below writes to the now-
            // focused element via the background key / AT-SPI rung.
        }

        if resolved_elem_idx.is_some() {
            if let Some(refusal) = unavailable_webkit_keyboard_background(pid, delivery) {
                return refusal;
            }
        }

        // Renderer EditableText writes can update the accessible value without
        // emitting the DOM input event. For an explicit foreground request on
        // native Wayland, focus the named field and send real keyboard input so
        // Chromium/WebKit observe the same event sequence as a user.
        if delivery.is_foreground()
            && crate::wayland::wayland_input_enabled()
            && (is_chromium_embedder(pid) || is_webkitgtk_embedder(pid))
        {
            if let Some(idx) = resolved_elem_idx {
                let text_w = text.clone();
                let result = tokio::task::spawn_blocking(move || {
                    crate::wayland::with_target_foreground(pid, xid, || {
                        if !crate::atspi::focus_element(pid, xid, idx)? {
                            anyhow::bail!(
                                "AT-SPI Component.GrabFocus returned false for element {idx}; {GRAB_FOCUS_RETRY}"
                            );
                        }
                        crate::wayland::type_text_focused(&text_w)
                    })
                })
                .await;
                return match result {
                    Ok(Ok(())) => ToolResult::text(format!(
                        "Typed {text_len} character(s) (via Wayland virtual-keyboard)."
                    ))
                    .with_structured(type_text_structured(
                        "key_events",
                        text_len,
                        false,
                    )),
                    Ok(Err(error)) => ToolResult::error(error.to_string()),
                    Err(error) => ToolResult::error(format!("Task error: {error}")),
                };
            }
        }

        // AX addressing names one exact editable. Try this focus-free route
        // before native Wayland keyboard injection, which can only target the
        // compositor's globally focused surface.
        if let Some(idx) = resolved_elem_idx {
            let password_before = password_length_before(pid, xid, idx).await;
            let text_at = text.clone();
            let targeted = tokio::task::spawn_blocking(move || {
                crate::atspi::type_into_editable_at(pid, xid, idx, &text_at)
            })
            .await;
            match targeted {
                Ok(Ok(())) => {
                    return type_text_ax_result_verified(
                        pid,
                        xid,
                        idx,
                        &text,
                        "via targeted AT-SPI",
                        password_before,
                    )
                    .await;
                }
                Ok(Err(_)) | Err(_)
                    if !delivery.is_foreground() && crate::wayland::wayland_input_enabled() =>
                {
                    return crate::input::delivery::background_unavailable_error(
                        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                    );
                }
                _ => {}
            }
        }

        // Native Wayland: keys go to the *focused* surface (no pid/window
        // targeting in the protocol). Type via the virtual-keyboard tool; pair
        // with a prior `click`/`activate` to focus the intended window.
        if crate::wayland::wayland_input_enabled() {
            if !delivery.is_foreground() {
                return crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                );
            }
            let text_w = text.clone();
            let idx = resolved_elem_idx;
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::with_target_foreground(pid, xid, || {
                    if let Some(idx) = idx {
                        if !crate::atspi::focus_element(pid, xid, idx)? {
                            anyhow::bail!(
                                "AT-SPI Component.GrabFocus returned false for element {idx}; {GRAB_FOCUS_RETRY}"
                            );
                        }
                    }
                    crate::wayland::type_text_focused(&text_w)
                })
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Typed {text_len} character(s) (via Wayland virtual-keyboard)."
                ))
                .with_structured(type_text_structured(
                    "key_events",
                    text_len,
                    false,
                )),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // Terminal short-circuit: when the target window's WM_CLASS marks it
        // as a terminal emulator (Ghostty / Alacritty / kitty / …), skip the
        // AT-SPI path entirely. Terminals expose an editable text area that
        // AT-SPI `insertText` would aim at, but the write never reaches the
        // pty, so the user sees "type acknowledged but nothing appeared".
        // Try pty-master injection first (most reliable), then XTest key
        // synthesis. Either way the structured response reports
        // `path: "key_events"` so callers can verify the route taken.
        let pid_is_terminal = is_terminal_process(pid);
        let wm_class_is_terminal =
            tokio::task::spawn_blocking(move || crate::terminal::is_terminal_window(xid))
                .await
                .unwrap_or(false);
        if pid_is_terminal || wm_class_is_terminal {
            let text_len = text.chars().count();
            let text_t = text.clone();
            let foreground = delivery.is_foreground();
            let cursor_id = resolve_cursor_key(&args);
            let result = spawn_blocking_bounded(
                "type_text terminal",
                foreground_budget(text_len),
                move || -> anyhow::Result<(&'static str, Option<crate::input::KeyboardDeliveryReport>)> {
                // pty-master injection is preferred — it skips the X event
                // queue entirely. Falls through to key events if the terminal
                // isn't reachable that way (descendant pty unresolvable).
                if inject_terminal_input(pid, xid, &text_t)? {
                    return Ok(("pty", None));
                }
                if foreground {
                    crate::input::with_x11_foreground(xid, 80, || {
                        crate::input::send_type_text_xtest(&text_t)
                    })?;
                    return Ok(("key_events_fg", None));
                }
                // Background: real key events through the XI2 virtual master
                // keyboard (xterm/gnome-terminal accept them like physical input).
                match background_text_route(&cursor_id, pid, xid, &text_t)? {
                    Some(KeyRoute::Mpx(report)) => Ok((crate::input::MPX_UINPUT_PATH, Some(report))),
                    _ => Ok(("background_unavailable", None)),
                }
            })
            .await;
            return match result {
                Ok(Ok(("background_unavailable", _))) => background_keyboard_refusal(atspi_up()),
                Ok(Ok((_, Some(report)))) => type_text_mpx_result(text_len, report),
                Ok(Ok((path, None))) => ToolResult::text(format!(
                    "Typed {text_len} character(s) (terminal emulator: pty/XTest key events)."
                ))
                .with_structured(type_text_structured(path, text_len, false)),
                Ok(Err(e)) => input_error_result(e),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }
        // Foreground means the caller explicitly permits activation: behave like
        // a user. Activate the window and confirm it holds the input focus,
        // GrabFocus the indexed element when one was given, then type REAL key
        // events via XTest. The AT-SPI EditableText probes below are skipped on
        // purpose: on large trees (LibreOffice) they stall for the full bus
        // timeout, Chromium/WebKitGTK echo them without the renderer seeing the
        // input, and for a spreadsheet cell / canvas they would target the
        // wrong widget anyway. The whole transaction is deadline-bounded.
        if delivery.is_foreground() {
            let text_f = text.clone();
            let idx = resolved_elem_idx;
            let result = spawn_blocking_bounded(
                "foreground type_text",
                foreground_budget(text_len),
                move || {
                    crate::input::with_x11_foreground_opts(
                        xid,
                        crate::input::ForegroundOptions::keyboard(),
                        || {
                            if let Some(idx) = idx {
                                if !crate::atspi::focus_element(pid, xid, idx)? {
                                    anyhow::bail!(
                                        "AT-SPI Component.GrabFocus returned false for element {idx}; {GRAB_FOCUS_RETRY}"
                                    );
                                }
                            }
                            crate::input::send_type_text_xtest(&text_f)
                        },
                    )
                },
            )
            .await;
            return match result {
                Ok(Ok(((), report))) => {
                    let mut extra = serde_json::Map::new();
                    extra.insert("characters".into(), json!(text_len));
                    ToolResult::text(format!(
                        "Typed {text_len} character(s) as real key events {}",
                        foreground_key_tail(&report)
                    ))
                    .with_structured(foreground_structured(
                        "key_events_fg",
                        report,
                        extra,
                    ))
                }
                Ok(Err(e)) => input_error_result(e),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // With a focus-free real keyboard available, type where the widget
        // focus is — exactly what a physical keyboard does after the click that
        // focused the field. The blind AT-SPI editable search below can pick a
        // different editable of the app (it "succeeded" into GIMP's wrong entry
        // and VS Code's search box); it stays as the fallback for hosts without
        // the MPX route and for explicit element targets.
        if !delivery.is_foreground()
            && resolved_elem_idx.is_none()
            && crate::input::real_keyboard_input_available()
        {
            let text_b = text.clone();
            let cursor_id = resolve_cursor_key(&args);
            let result = spawn_blocking_bounded(
                "background type_text",
                foreground_budget(text_len),
                move || background_text_route(&cursor_id, pid, xid, &text_b),
            )
            .await;
            match result {
                Ok(Ok(Some(KeyRoute::Mpx(report)))) => {
                    return type_text_mpx_result(text_len, report)
                }
                Ok(Err(e)) => return input_error_result(e),
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
                Ok(Ok(_)) => {}
            }
        }

        // Prefer the focused widget — the element the user just clicked. If a
        // NON-editable input holds keyboard focus (a spreadsheet cell, a
        // terminal, a canvas), the focus-free AT-SPI editable search below would
        // grab the wrong field (e.g. gnumeric's name box instead of the selected
        // cell, or skip a terminal entirely), so synth-type into the focused
        // widget instead: terminals via pty injection, everything else via
        // XSendEvent to the focused window. A focused *editable* (Some(true)) or
        // nothing focused (None) falls through to the existing AT-SPI-first flow.
        let focus_kind = tokio::task::spawn_blocking(move || {
            crate::atspi::focused_is_editable(pid, xid).ok().flatten()
        })
        .await
        .ok()
        .flatten();
        if focus_kind == Some(false) {
            if !delivery.is_foreground() {
                // A spreadsheet cell / canvas / terminal holds the widget focus:
                // real key events aimed at this window are the only thing that
                // types there. The XI2 virtual master keyboard delivers them
                // without touching the user's focus; terminals take the pty.
                let text_b = text.clone();
                let cursor_id = resolve_cursor_key(&args);
                let result = spawn_blocking_bounded(
                    "background type_text",
                    foreground_budget(text_len),
                    move || -> anyhow::Result<Option<KeyRoute>> {
                        if inject_terminal_input(pid, xid, &text_b)? {
                            return Ok(Some(KeyRoute::Terminal));
                        }
                        background_text_route(&cursor_id, pid, xid, &text_b)
                    },
                )
                .await;
                return match result {
                    Ok(Ok(Some(KeyRoute::Terminal))) => ToolResult::text(format!(
                        "Typed {text_len} character(s) into the focused terminal (pty)."
                    ))
                    .with_structured(type_text_structured("key_events", text_len, false)),
                    Ok(Ok(Some(KeyRoute::Mpx(report)))) => type_text_mpx_result(text_len, report),
                    Ok(Ok(_)) => background_keyboard_refusal(atspi_up()),
                    Ok(Err(e)) => input_error_result(e),
                    Err(e) => ToolResult::error(format!("Task error: {e}")),
                };
            }
            let text_f = text.clone();
            let result = tokio::task::spawn_blocking(move || {
                if inject_terminal_input(pid, xid, &text_f)? {
                    return Ok(());
                }
                // XTest (real input to the focused window), NOT XSendEvent: GTK/Qt
                // drop synthetic key events, so a spreadsheet cell / canvas would
                // stay empty. The click that gave this widget focus already put it
                // under the X input focus, so XTest-to-focus lands correctly.
                crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_type_text_xtest(&text_f)
                })
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Typed {text_len} character(s) into the focused widget."
                ))
                .with_structured(type_text_structured(
                    "key_events",
                    text_len,
                    false,
                )),
                Ok(Err(e)) => input_error_result(e),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // Try AT-SPI EditableText first (focus-free, works for Qt6/GTK4).
        let text_clone = text.clone();
        let atspi_result = tokio::task::spawn_blocking(move || {
            crate::atspi::type_into_editable(pid, xid, &text_clone)
        })
        .await;

        match atspi_result {
            Ok(Ok(())) => {
                // AT-SPI succeeded — focus-free typing worked (Qt6, GTK4, etc.)!
                // Electron/Chromium can echo this write without the renderer
                // observing it, so the confirm is suppressed there (mirrors macOS).
                return type_text_ax_result(pid, text_len, "via AT-SPI");
            }
            _ => {
                // AT-SPI failed (no editable exposed). Qt5 doesn't expose widgets
                // when unfocused, so try the synthetic-focus workaround.
            }
        }

        // Qt5 workaround: send synthetic FocusIn to make Qt5's AT-SPI bridge
        // expose the widget tree, type via AT-SPI, then send FocusOut.
        // This doesn't change the X11 active window, so the test's focus check passes.
        let text_clone2 = text.clone();
        let qt5_result = tokio::task::spawn_blocking(move || {
            if !is_qt5_process(pid) {
                anyhow::bail!("not a Qt5 process; synthetic-FocusIn bridge workaround skipped");
            }
            // Send FocusIn to trigger Qt5's bridge
            crate::input::send_focus_in(xid)?;
            std::thread::sleep(std::time::Duration::from_millis(100));

            // Try AT-SPI again now that widgets should be exposed
            let result = crate::atspi::type_into_editable(pid, xid, &text_clone2);

            // Restore state with FocusOut
            crate::input::send_focus_out(xid)?;

            result
        })
        .await;

        match qt5_result {
            Ok(Ok(())) => {
                return type_text_ax_result(pid, text_len, "via AT-SPI with focus workaround");
            }
            _ => {
                // AT-SPI still didn't work. Fall back to X11 XSendEvent.
            }
        }

        // Track which path the final fallback chain took, so the
        // structured response stays honest. The closure can't borrow
        // a local mutably across `spawn_blocking`, so funnel the
        // decision through the success type instead.
        // delivery_mode: background (default) = focus-free AT-SPI / XSendEvent;
        // foreground = activate the window (EWMH), then synthesize REAL key
        // events to it via XTest — the escalation when background didn't land
        // (e.g. a GTK dialog whose widget ignores synthetic XSendEvent keys).
        let cursor_id = resolve_cursor_key(&args);
        let result = spawn_blocking_bounded(
            "type_text",
            foreground_budget(text_len),
            move || -> anyhow::Result<(&'static str, Option<crate::input::KeyboardDeliveryReport>)> {
            // Terminals: write to the pty master (focus-free, below the toolkit).
            if inject_terminal_input(pid, xid, &text)? {
                return Ok(("key_events", None));
            }
            // GUI apps: X11 only routes keystrokes to the *focused* toplevel's
            // focused widget, so background XSendEvent typing doesn't land. Fill
            // the editable field via AT-SPI instead — focus-free and toolkit-
            // agnostic. Fall back to Tk send or XSendEvent when no a11y field is exposed.
            if crate::atspi::insert_text(pid, xid, &text).unwrap_or(false) {
                return Ok(("ax", None));
            }
            // Tk apps: use Tk's `send` command (no AT-SPI bridge, so AT-SPI above
            // returned false). This is the Tk-specific override, like CDP for Chromium.
            if crate::input::inject_tk_send(&text).unwrap_or(false) {
                return Ok(("key_events", None));
            }
            if delivery.is_foreground() {
                crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_type_text_xtest(&text)
                })?;
                return Ok(("key_events_fg", None));
            }
            // No a11y editable, no pty: real key events through the XI2 virtual
            // master keyboard are the last focus-free route.
            match background_text_route(&cursor_id, pid, xid, &text)? {
                Some(KeyRoute::Mpx(report)) => Ok((crate::input::MPX_UINPUT_PATH, Some(report))),
                _ => Ok(("background_unavailable", None)),
            }
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        match result {
            // AT-SPI's boolean acknowledges the EditableText call; it is not a
            // fresh value readback. Keep the result unverifiable and apply the
            // stricter Chromium escalation where appropriate.
            Ok(Ok(("ax", _))) => type_text_ax_result(
                pid,
                text_len,
                &format!("via X11, delivery_mode={mode_label}"),
            ),
            Ok(Ok(("background_unavailable", _))) => background_keyboard_refusal(atspi_up()),
            Ok(Ok((_, Some(report)))) => type_text_mpx_result(text_len, report),
            Ok(Ok((path, None))) => ToolResult::text(format!(
                "Typed {text_len} character(s) (via X11, delivery_mode={mode_label})."
            ))
            .with_structured(type_text_structured(path, text_len, false)),
            Ok(Err(e)) => input_error_result(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// Background text delivery through the XI2 virtual master keyboard. `None`
/// when the host has no such route (Xvfb, no `/dev/uinput`); a uinput
/// permission error propagates so the tool can attach the operator hint.
fn background_text_route(
    cursor_id: &str,
    pid: u32,
    xid: u64,
    text: &str,
) -> anyhow::Result<Option<KeyRoute>> {
    if !crate::input::real_keyboard_input_available() {
        return Ok(None);
    }
    match crate::input::focus_guard::guarded(Some(pid), || {
        background_virtual_text(cursor_id, pid, xid, text)
    }) {
        Ok((mut report, guard)) => {
            report.focus_guard = guard;
            Ok(Some(KeyRoute::Mpx(report)))
        }
        Err(error) if crate::input::is_uinput_unavailable(&error) => Err(error),
        Err(error) if is_popup_keyboard_grab(&error) => Err(error),
        Err(error) => {
            tracing::warn!(pid, "MPX keyboard text delivery failed: {error}");
            Err(error.context("virtual master keyboard delivery failed"))
        }
    }
}

/// `type_text` result for a delivery through the virtual master keyboard.
fn type_text_mpx_result(
    text_len: usize,
    report: crate::input::KeyboardDeliveryReport,
) -> ToolResult {
    let mut structured = type_text_structured(report.path, text_len, false);
    for (key, value) in report.to_json().as_object().into_iter().flatten() {
        structured[key] = value.clone();
    }
    structured["delivery_mode"] = json!("background");
    let outcome = if report.skipped_characters.is_empty() {
        format!("Typed {text_len} character(s)")
    } else {
        "Partial typing result".to_owned()
    };
    let mut text = format!(
        "{outcome} as real key events {} (delivery_mode=background)",
        report.route_phrase()
    );
    if report.virtual_focus_held && report.delivery_confirmed {
        text.push_str(&format!("; {KEY_EFFECT_UNVERIFIED}."));
    } else {
        text.push('.');
    }
    if !report.skipped_characters.is_empty() {
        text.push_str(&format!(
            " {} character(s) had no keycode in the current keymap and were skipped: {:?}.",
            report.skipped_characters.len(),
            report.skipped_characters.iter().collect::<String>()
        ));
    }
    if !report.virtual_focus_held || !report.delivery_confirmed {
        structured["effect"] = json!("suspected_noop");
        text.push_str(
            " Warning: the target lost the virtual keyboard's focus or the server did \
             not confirm the last key; the text may not have landed.",
        );
    }
    text.push_str(&report.delivery_notes());
    let result = if report.skipped_characters.is_empty() {
        ToolResult::text(text)
    } else {
        ToolResult::error(text)
    };
    result.with_structured(structured)
}

#[cfg(test)]
#[test]
fn mpx_type_text_with_skipped_characters_is_an_error() {
    let report = |skipped_characters: Vec<char>| crate::input::KeyboardDeliveryReport {
        virtual_focus_held: true,
        core_focus_unchanged: true,
        delivery_confirmed: true,
        key_events: 2,
        skipped_characters,
        focus_guard: None,
        released_stuck: Vec::new(),
        path: crate::input::MPX_UINPUT_PATH,
        grab_popup: None,
    };
    assert_eq!(type_text_mpx_result(2, report(Vec::new())).is_error, None);
    let skipped = type_text_mpx_result(2, report(vec!['約']));
    assert_eq!(skipped.is_error, Some(true));
    assert!(!format!("{:?}", skipped.content).contains("Typed 2 character(s)"));
    assert_eq!(
        skipped.structured_content.unwrap()["skipped_characters"],
        "約"
    );
}

// ── press_key ─────────────────────────────────────────────────────────────────

pub struct PressKeyTool {
    state: Arc<ToolState>,
}
static PRESS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Turn a modified `press_key` request into the chord an underlying keyboard
/// path expects when it takes a single key and no separate modifier list.
///
/// Returns `None` when there is nothing to promote, so the caller keeps the
/// plain single-key route. Dropping the modifiers instead is not a silent
/// no-op: the bare key still reaches the app and is typed as a character.
fn press_key_chord(mods: &[String], key: &str) -> Option<Vec<String>> {
    if mods.is_empty() {
        return None;
    }
    let mut chord = mods.to_vec();
    chord.push(key.to_owned());
    Some(chord)
}

#[cfg(test)]
mod press_key_tests;

#[async_trait]
impl Tool for PressKeyTool {
    fn def(&self) -> &ToolDef {
        PRESS_DEF.get_or_init(|| ToolDef {
            name: "press_key".into(),
            description: "Press one key, with optional held modifiers, in a window. In the \
                background (default) the key is a real key event from a virtual keyboard where \
                the host has one (X11 with a writable /dev/uinput, not KDE Plasma). Elsewhere it \
                is a synthetic X key event: GTK apps drop those, so a GTK target is refused with \
                background_unavailable, and Qt apps act on them only while their window is the \
                active one, so on KDE Plasma X11, XWayland and Wayland a background key can be \
                refused or have no effect. To enter text, type_text with element_token works in \
                the background. delivery_mode \"foreground\" takes over the user's screen: use it \
                only with the user's leave, after a background refusal.".into(),
            input_schema: json!({
                "type":"object","required":["key"],"properties":{
                    "session": pincer_core::tool_schema::session_schema(),
                    "pid":{"type":"integer","description":"Target process ID. Omit with scope \"desktop\" to send the key to the focused application."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                    "key":{"type":"string","description":"Key name: enter/return, tab, escape, space, backspace, delete, insert, home, end, pageup, pagedown, up, down, left, right, f1-f12, or any single ASCII character."},
                    "modifiers":{"type":"array","items":{"type":"string"},"description":"Modifier keys held while the key is pressed: ctrl, shift, alt (or option), super (or cmd, win)."},
                    "element_token": pincer_core::tool_schema::element_token_schema(),
                    "x":{"type":"number","description":"Pixel X, the element px action form: pixel-click there to focus, then send the key. Use when the key must go to a Chromium/Electron surface the AX path can't focus. Pass with y, no element_token. Window-local pixels by default (same convention as click); for get_desktop_state pixels pass scope:\"desktop\" (or coordinate_frame:\"desktop\")."},
                    "y":{"type":"number","description":"Pixel Y (see x)."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with no pid to send the key to the focused application. Default \"window\"."},
                    "coordinate_frame": coordinate_frame_schema(),
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        if (args.opt_str("scope").as_deref() == Some("desktop")
            || coordinate_frame_is_desktop(&args))
            && args.get("pid").is_none()
        {
            let input = match parse_typed_projection::<PressKeyInput>("press_key", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let (combo_mods, key) = split_key_combo(&input.key);
            let display = input.key.clone();
            let mut modifiers = input.modifiers.unwrap_or_default();
            modifiers.extend(combo_mods);
            let modifiers = match modifier_arg("press_key", "modifiers", &modifiers) {
                Ok(modifiers) => modifiers,
                Err(refusal) => return refusal,
            };
            position_named_session_keyboard_cursor(&self.state, &args, 0, 0, None, None, false)
                .await;
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland { "wayland_focused" } else { "xtest" };
            let result = tokio::task::spawn_blocking(move || {
                if wayland && modifiers.is_empty() {
                    crate::wayland::press_key_focused(&key)
                } else if wayland {
                    let mut keys = modifiers;
                    keys.push(key);
                    crate::wayland::hotkey_focused(&keys)
                } else {
                    let refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    crate::input::send_key_xtest(&key, &refs)
                }
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!("Pressed desktop key '{display}'."))
                    .with_structured(
                        json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                    ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let mut pid = args.u64_or("pid", 0) as u32;
        let key = match args.require_str("key") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let mut mods: Vec<String> = args.str_array("modifiers");
        let (combo_mods, key) = split_key_combo(&key);
        mods.extend(combo_mods);
        let mods = match modifier_arg("press_key", "modifiers", &mods) {
            Ok(mods) => mods,
            Err(refusal) => return refusal,
        };

        // Resolve the element token into both its owning window and exact
        // child. Foreground delivery establishes child focus inside the
        // verified top-level activation transaction; background XSendEvent
        // retains the historical direct-window path.
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self.state.snapshots.resolve(pid as i32, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let resolved_element_index = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { element_index, .. } => {
                Some(*element_index)
            }
            pincer_core::element_token::ResolvedElement::None => None,
        };
        let xid_opt = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            pincer_core::element_token::ResolvedElement::None => window_id_arg,
        };
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

        // ── px form: pixel-click to focus, then the key goes to the focused element ──
        // Reuses click's translation + delivery_mode; after it, deliver via the plain
        // background path (the focus-click already handled fronting when fg). Pass x,y
        // (no element_token) for Chromium/Electron surfaces the AX path can't focus.
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        if isolated_hyprland_background(delivery) {
            if xid_opt.is_none() {
                return isolated_hyprland_refusal(
                    "an exact window_id is required for isolated keys",
                );
            }
            if resolved_element_index.is_some()
                || args.get("x").is_some()
                || args.get("y").is_some()
            {
                return isolated_hyprland_refusal(
                    "isolated keys address the exact top-level; first click the child explicitly",
                );
            }
            position_named_session_keyboard_cursor(&self.state, &args, pid, xid, None, None, false)
                .await;
            return isolated_hyprland_action(
                &args,
                pid,
                xid,
                crate::wayland::hyprland_input::Action::Key {
                    key,
                    modifiers: mods,
                },
            )
            .await;
        }
        if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
            return refusal;
        }

        if let Some(refusal) = unavailable_webkit_keyboard_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_gtk_keyboard_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, true) {
            return refusal;
        }
        if let Some(result) = wm_chord_background(pid, xid, &key, &mods, delivery).await {
            return result;
        }

        let px = args.get("x").and_then(|value| value.as_f64());
        let py = args.get("y").and_then(|value| value.as_f64());
        if px.is_some() != py.is_some() {
            return invalid_pointer_arguments(
                "press_key",
                "x and y must be passed together (a pixel focus point), or neither",
            );
        }
        if px.is_some() && resolved_element_index.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to press_key, not both.",
            );
        }

        if hyprland_foreground(delivery) {
            if xid_opt.is_none() {
                return foreground_hyprland_refusal("an exact window_id is required");
            }
            let action = match foreground_hyprland_key(key, mods) {
                Ok(action) => action,
                Err(error) => return error,
            };
            if let Err(error) = focus_hyprland_foreground(
                &self.state,
                &args,
                pid,
                xid,
                resolved_element_index,
                px.zip(py),
            )
            .await
            {
                return error;
            }
            if px.is_none() {
                position_named_session_keyboard_cursor(
                    &self.state,
                    &args,
                    pid,
                    xid,
                    resolved_element_index,
                    None,
                    false,
                )
                .await;
            }
            return foreground_hyprland_action(&args, pid, xid, action).await;
        }

        position_named_session_keyboard_cursor(
            &self.state,
            &args,
            pid,
            xid,
            resolved_element_index,
            px.zip(py),
            false,
        )
        .await;

        // Nested cua-compositor addresses the owning Wayland client directly.
        // Preserve legacy modifiers by promoting the request to a chord.
        if crate::wayland::is_inject_mode() {
            if let Err(error) =
                focus_nested_inject_target(pid, xid, resolved_element_index, px.zip(py)).await
            {
                return error;
            }
            let result = match press_key_chord(&mods, &key) {
                None => {
                    let key_w = key.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::wayland::inject_press_key(pid, xid, &key_w)
                    })
                    .await
                }
                Some(chord) => {
                    tokio::task::spawn_blocking(move || {
                        crate::wayland::inject_hotkey(pid, xid, &chord)
                    })
                    .await
                }
            };
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Pressed key '{key}' (focus-free via cua-compositor)."
                )),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        let px_target = {
            if let (Some(cx), Some(cy)) = (px, py) {
                let from_zoom = args.bool_or("from_zoom", false);
                if let Err(e) = focus_by_pixel(
                    &self.state,
                    pid,
                    Some(xid),
                    (cx, cy),
                    delivery.is_foreground(),
                    &args,
                    from_zoom,
                )
                .await
                {
                    return e;
                }
                Some((cx.round() as i32, cy.round() as i32))
            } else {
                None
            }
        };

        // Native Wayland: send the key to the focused surface via virtual-keyboard.
        // `wayland::press_key` carries no modifier list, so a modified request has
        // to become a chord here — otherwise the modifiers are dropped and the
        // bare key is typed as a character (Ctrl+S inserts a literal "s").
        if crate::wayland::wayland_input_enabled() {
            let key_w = key.clone();
            let chord = press_key_chord(&mods, &key);
            let idx = resolved_element_index;
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::with_target_foreground(pid, xid, || {
                    if let Some(idx) = idx {
                        if !crate::atspi::focus_element(pid, xid, idx)? {
                            anyhow::bail!(
                                "AT-SPI Component.GrabFocus returned false for element {idx}; {GRAB_FOCUS_RETRY}"
                            );
                        }
                    }
                    match chord {
                        Some(keys) => crate::wayland::hotkey_focused(&keys),
                        None => crate::wayland::press_key_focused(&key_w),
                    }
                })
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Pressed key '{key}' (via Wayland virtual-keyboard)."
                )),
                Ok(Err(e)) => ToolResult::error(e.to_string()),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        let key_for_task = key.clone();
        // The result names the whole combination ("ctrl+end"), not the bare
        // key, so a modified press doesn't read as a plain one.
        let pressed = mods
            .iter()
            .map(String::as_str)
            .chain([key.as_str()])
            .collect::<Vec<_>>()
            .join("+");
        let cursor_id = resolve_cursor_key(&args);
        // Foreground delivery is one atomic activate-and-XTest transaction.
        // A preceding PX click establishes internal widget focus, but that
        // click restores the prior top-level before returning.
        let deliver_fg = delivery.is_foreground();
        let result = spawn_blocking_bounded(
            "press_key",
            foreground_budget(1),
            move || -> anyhow::Result<KeyRoute> {
            if resolved_element_index.is_none()
                && mods.is_empty()
                && is_enter_key(&key_for_task)
                && inject_terminal_input(pid, xid, "\n")?
            {
                return Ok(KeyRoute::Terminal);
            }
            let m: Vec<&str> = mods.iter().map(String::as_str).collect();
            // foreground: activate the window first, then inject a REAL key via
            // XTest. Synthetic XSendEvent keys (`send_key`) are dropped by
            // GTK/Qt/Chromium/Firefox, so the foreground rung must use XTest —
            // it delivers to the now-focused window. background = real key
            // events through the XI2 virtual master keyboard (no focus steal),
            // else direct XSendEvent for apps that accept it.
            if deliver_fg {
                return crate::input::with_x11_foreground_opts(
                    xid,
                    crate::input::ForegroundOptions::keyboard(),
                    || {
                        if let Some(element_index) = resolved_element_index {
                            if !crate::atspi::focus_element(pid, xid, element_index)? {
                                anyhow::bail!(
                                    "AT-SPI Component.GrabFocus returned false for element {element_index}; {GRAB_FOCUS_RETRY}"
                                );
                            }
                        }
                        crate::input::send_key_xtest(&key_for_task, &m)
                    },
                )
                .map(|((), report)| KeyRoute::Foreground(report));
            }
            background_key_route(
                &cursor_id,
                pid,
                xid,
                px_target,
                &key_for_task,
                &m,
                resolved_element_index,
            )
        })
        .await;
        let mode_label = if deliver_fg {
            "foreground"
        } else {
            "background"
        };
        match result {
            Ok(Ok(route)) => key_route_result(&format!("Pressed key '{pressed}'"), route, mode_label),
            Ok(Err(e)) => input_error_result(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── hotkey ────────────────────────────────────────────────────────────────────

use crate::input::modifier::is_modifier;

/// A tool's `modifier` / `modifiers` argument as canonical names (ctrl,
/// shift, alt, super). A name that is not a modifier is refused: the X11
/// synthetic route used to drop it and send the unmodified action.
fn modifier_arg(tool: &str, field: &str, names: &[String]) -> Result<Vec<String>, ToolResult> {
    crate::input::modifier::canonical_modifier_names(names).map_err(|unknown| {
        unknown_modifier_refusal(tool, field, &unknown)
    })
}

fn unknown_modifier_refusal(tool: &str, field: &str, unknown: &[String]) -> ToolResult {
    let accepted = crate::input::modifier::ACCEPTED_NAMES;
    ToolResult::error(format!(
        "{tool}: {field} {} not a modifier name; accepted: {accepted}.",
        if unknown.len() == 1 {
            format!("\"{}\" is", unknown[0])
        } else {
            format!(
                "{} are",
                unknown
                    .iter()
                    .map(|name| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    ))
    .with_structured(json!({
        "code": "invalid_arguments",
        "tool": tool,
        "field": field,
        "unknown": unknown,
        "accepted": accepted,
    }))
}

/// Split hotkey `keys` into canonical modifier names and the one key that
/// is not a modifier. More than one non-modifier is refused: the extra one is
/// usually a modifier spelling the tool did not know, and the chord would
/// otherwise go out without it.
fn split_hotkey_keys(keys: &[String]) -> Result<(Vec<String>, String), ToolResult> {
    let (modifiers, others): (Vec<&String>, Vec<&String>) =
        keys.iter().partition(|key| is_modifier(key));
    match others.as_slice() {
        [key] => {
            let modifiers = crate::input::modifier::canonical_modifier_names(&modifiers)
                .expect("partitioned as modifiers");
            Ok((modifiers, (*key).clone()))
        }
        [] => Err(ToolResult::error("keys must include at least one non-modifier key.")
            .with_structured(json!({ "code": "invalid_arguments", "tool": "hotkey" }))),
        [.., key] => {
            let extra: Vec<String> = others[..others.len() - 1]
                .iter()
                .map(|name| (*name).clone())
                .collect();
            let mut refusal = unknown_modifier_refusal("hotkey", "keys", &extra);
            refusal.content = vec![pincer_core::protocol::Content::text(format!(
                "hotkey: keys takes modifiers plus one key (\"{key}\" here); {} {} not a \
                 modifier name. Accepted modifiers: {}.",
                extra
                    .iter()
                    .map(|name| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                if extra.len() == 1 { "is" } else { "are" },
                crate::input::modifier::ACCEPTED_NAMES
            ))];
            Err(refusal)
        }
    }
}

pub struct HotkeyTool {
    state: Arc<ToolState>,
}
static HOTKEY_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for HotkeyTool {
    fn def(&self) -> &ToolDef {
        HOTKEY_DEF.get_or_init(|| ToolDef {
            name: "hotkey".into(),
            description: "Press a combination of keys together, e.g. [\"ctrl\",\"c\"] for Copy. \
                In the background (default) the chord is real key events from a virtual keyboard \
                where the host has one (X11 with a writable /dev/uinput, not KDE Plasma), and the \
                target need not be frontmost. Elsewhere it is synthetic X key events: GTK apps \
                drop those, so a GTK target is refused with background_unavailable, and Qt apps \
                act on them only while their window is the active one, so on KDE Plasma X11, \
                XWayland and Wayland a background chord can be refused or have no effect. \
                delivery_mode \"foreground\" takes over the user's screen: use it only with the \
                user's leave, after a background refusal. Window-manager chords cannot reach the \
                WM in the background: alt+F4 on a window target closes that window through the \
                WM (_NET_CLOSE_WINDOW, effect confirmed when it unmaps); super+*, alt+tab, \
                ctrl+alt+* and other WM bindings are refused with code wm_chord_unavailable: use \
                bring_to_front, list_windows or kill_app instead.".into(),
            input_schema: json!({
                "type":"object","required":["keys"],"properties":{
                    "session": pincer_core::tool_schema::session_schema(),
                    "pid":{"type":"integer","description":"Target process ID. Omit with scope \"desktop\" to send the chord to the focused application."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                    "keys":{"type":"array","items":{"type":"string"},"minItems":2,
                        "description":"Modifier(s) + one non-modifier key, e.g. [\"ctrl\",\"c\"]."},
                    "element_token": pincer_core::tool_schema::element_token_schema(),
                    "x":{"type":"number","description":"Screenshot-pixel X, the element px action form: pixel-click there to focus, then send the combo (so e.g. Ctrl+V pastes into that field). Pass with y. Use for Chromium/Electron surfaces the background combo can't reach."},
                    "y":{"type":"number","description":"Screenshot-pixel Y (see x)."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with no pid to send the chord to the focused application. Default \"window\"."},
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        if args.opt_str("scope").as_deref() == Some("desktop") && args.get("pid").is_none() {
            let input = match parse_typed_projection::<HotkeyInput>("hotkey", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let keys = input.keys;
            if keys.len() < 2 {
                return ToolResult::error("hotkey.keys must contain at least two keys.")
                    .with_structured(json!({ "code": "invalid_arguments" }));
            }
            let (modifiers, key) = match split_hotkey_keys(&keys) {
                Ok(split) => split,
                Err(refusal) => return refusal,
            };
            let display = keys.join("+");
            position_named_session_keyboard_cursor(&self.state, &args, 0, 0, None, None, false)
                .await;
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland { "wayland_focused" } else { "xtest" };
            let result = tokio::task::spawn_blocking(move || {
                if wayland {
                    crate::wayland::hotkey_focused(&keys)
                } else {
                    let refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    crate::input::send_key_xtest(&key, &refs)
                }
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!("Pressed desktop hotkey {display}."))
                    .with_structured(
                        json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                    ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let mut pid = args.u64_or("pid", 0) as u32;
        let window_id_arg = args.opt_u64("window_id");
        let resolved = match self.state.snapshots.resolve(pid as i32, &args) {
            Ok(resolved) => resolved,
            Err(error) => return error,
        };
        let resolved_element_index = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { element_index, .. } => {
                Some(*element_index)
            }
            pincer_core::element_token::ResolvedElement::None => None,
        };
        let xid_opt = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            pincer_core::element_token::ResolvedElement::None => window_id_arg,
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

        // Parse keys array (preferred) or fall back to legacy key+modifiers.
        let (key, mods) = if let Some(arr) = args.get("keys").and_then(|v| v.as_array()) {
            let keys: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            match split_hotkey_keys(&keys) {
                Ok((modifiers, key)) => (key, modifiers),
                Err(refusal) => return refusal,
            }
        } else if let Some(k) = args.opt_str("key") {
            match modifier_arg("hotkey", "modifiers", &args.str_array("modifiers")) {
                Ok(mods) => (k, mods),
                Err(refusal) => return refusal,
            }
        } else {
            return ToolResult::error(
                "Provide 'keys' array (e.g. [\"ctrl\",\"c\"]) or 'key'+'modifiers' parameters.",
            );
        };

        let key_display = format!("{}+{}", mods.join("+"), key);
        let key_for_wayland = key.clone();
        let mods_for_wayland = mods.clone();
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);

        if isolated_hyprland_background(delivery) {
            if xid_opt.is_none() {
                return isolated_hyprland_refusal(
                    "an exact window_id is required for isolated hotkeys",
                );
            }
            if resolved_element_index.is_some()
                || args.get("x").is_some()
                || args.get("y").is_some()
            {
                return isolated_hyprland_refusal("isolated hotkeys address the exact top-level; first click the child explicitly");
            }
            if let Some(keys) = args.get("keys").and_then(Value::as_array) {
                if keys
                    .iter()
                    .filter(|value| value.as_str().is_some_and(|key| !is_modifier(key)))
                    .count()
                    != 1
                    || keys.iter().any(|value| !value.is_string())
                {
                    return isolated_hyprland_refusal(
                        "isolated hotkeys require exactly one non-modifier key",
                    );
                }
            }
            position_named_session_keyboard_cursor(&self.state, &args, pid, xid, None, None, false)
                .await;
            return isolated_hyprland_action(
                &args,
                pid,
                xid,
                crate::wayland::hyprland_input::Action::Key {
                    key,
                    modifiers: mods,
                },
            )
            .await;
        }
        if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_webkit_keyboard_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_gtk_keyboard_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, true) {
            return refusal;
        }
        if let Some(result) = wm_chord_background(pid, xid, &key, &mods, delivery).await {
            return result;
        }

        let px = args.get("x").and_then(|value| value.as_f64());
        let py = args.get("y").and_then(|value| value.as_f64());
        if px.is_some() != py.is_some() {
            return invalid_pointer_arguments(
                "hotkey",
                "x and y must be passed together (a pixel focus point), or neither",
            );
        }
        if px.is_some() && resolved_element_index.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to hotkey, not both.",
            );
        }

        if hyprland_foreground(delivery) {
            if xid_opt.is_none() {
                return foreground_hyprland_refusal("an exact window_id is required");
            }
            if let Some(keys) = args.get("keys").and_then(Value::as_array) {
                if keys.iter().any(|key| !key.is_string())
                    || keys
                        .iter()
                        .filter(|key| key.as_str().is_some_and(|key| !is_modifier(key)))
                        .count()
                        != 1
                {
                    return foreground_hyprland_refusal(
                        "hotkeys require exactly one non-modifier key",
                    );
                }
            }
            let action = match foreground_hyprland_key(key, mods) {
                Ok(action) => action,
                Err(error) => return error,
            };
            if let Err(error) = focus_hyprland_foreground(
                &self.state,
                &args,
                pid,
                xid,
                resolved_element_index,
                px.zip(py),
            )
            .await
            {
                return error;
            }
            if px.is_none() {
                position_named_session_keyboard_cursor(
                    &self.state,
                    &args,
                    pid,
                    xid,
                    resolved_element_index,
                    None,
                    false,
                )
                .await;
            }
            return foreground_hyprland_action(&args, pid, xid, action).await;
        }

        position_named_session_keyboard_cursor(
            &self.state,
            &args,
            pid,
            xid,
            resolved_element_index,
            px.zip(py),
            false,
        )
        .await;

        if crate::wayland::is_inject_mode() {
            if let Err(error) =
                focus_nested_inject_target(pid, xid, resolved_element_index, px.zip(py)).await
            {
                return error;
            }
            let mut chord = mods.clone();
            chord.push(key.clone());
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::inject_hotkey(pid, xid, &chord)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Pressed hotkey '{key_display}' (focus-free via cua-compositor)."
                )),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        // Chromium renderer shortcuts need DOM focus established by a real
        // pointer event. Component.GrabFocus alone can report success while
        // the renderer still drops the virtual-keyboard chord.
        if delivery.is_foreground()
            && crate::wayland::wayland_input_enabled()
            && is_chromium_embedder(pid)
        {
            if let Some(element_index) = resolved_element_index {
                let coordinates = tokio::task::spawn_blocking(move || {
                    resolve_element_local_coords(pid, element_index, Some(xid))
                })
                .await;
                let (_, x, y) = match coordinates {
                    Ok(Ok(value)) => value,
                    Ok(Err(error)) => return ToolResult::error(error.to_string()),
                    Err(error) => return ToolResult::error(format!("Task error: {error}")),
                };
                if let Err(error) =
                    focus_by_pixel(&self.state, pid, Some(xid), (x, y), true, &args, false).await
                {
                    return error;
                }
            }
        }

        // Foreground: GrabFocus the addressed element up front. Background
        // does it inside `background_key_route`, under the focus guard.
        if let Some(element_index) = resolved_element_index.filter(|_| delivery.is_foreground()) {
            let focused = tokio::task::spawn_blocking(move || {
                crate::atspi::focus_element(pid, xid, element_index)
            })
            .await;
            match focused {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    return ToolResult::error(format!(
                        "AT-SPI Component.GrabFocus returned false for element {element_index}; {GRAB_FOCUS_RETRY}"
                    ))
                }
                Ok(Err(error)) => return ToolResult::error(error.to_string()),
                Err(error) => return ToolResult::error(format!("Task error: {error}")),
            }
        }

        // ── px form: pixel-click to focus, then the combo acts on the focused field ──
        // (e.g. Ctrl+V into a Chromium input). Reuses click's translation +
        // delivery_mode; after it, deliver the combo via the plain background path
        // (the focus-click already fronted when fg).
        let px_target = {
            if let (Some(cx), Some(cy)) = (px, py) {
                let from_zoom = args.bool_or("from_zoom", false);
                if let Err(e) = focus_by_pixel(
                    &self.state,
                    pid,
                    Some(xid),
                    (cx, cy),
                    delivery.is_foreground(),
                    &args,
                    from_zoom,
                )
                .await
                {
                    return e;
                }
                Some((cx.round() as i32, cy.round() as i32))
            } else {
                None
            }
        };
        let deliver_fg = delivery.is_foreground();
        let cursor_id = resolve_cursor_key(&args);

        let result = spawn_blocking_bounded(
            "hotkey",
            foreground_budget(1),
            move || -> anyhow::Result<KeyRoute> {
                if crate::wayland::wayland_input_enabled() {
                    // Native Wayland: the chord goes inside the exact-target
                    // foreground guard that click, type_text and press_key use
                    // (pid and window checked, prior focus restored).
                    let mut combo: Vec<String> = mods_for_wayland.clone();
                    combo.push(key_for_wayland.clone());
                    return crate::wayland::with_target_foreground(pid, xid, || {
                        crate::wayland::hotkey_focused(&combo)
                    })
                    .map(|()| KeyRoute::Synthetic);
                }
                let m: Vec<&str> = mods.iter().map(String::as_str).collect();
                // foreground: activate the target first, then inject the accelerator
                // as REAL key events via XTest. Synthetic XSendEvent keys
                // (`send_key`) are dropped by GTK/Qt/Chromium/Firefox, so the
                // foreground rung must use XTest, which reaches the focused window.
                if deliver_fg {
                    return crate::input::with_x11_foreground_opts(
                        xid,
                        crate::input::ForegroundOptions::keyboard(),
                        || crate::input::send_key_xtest(&key, &m),
                    )
                    .map(|((), report)| KeyRoute::Foreground(report));
                }
                background_key_route(
                    &cursor_id,
                    pid,
                    xid,
                    px_target,
                    &key,
                    &m,
                    resolved_element_index,
                )
            },
        )
        .await;
        let mode_label = if deliver_fg {
            "foreground"
        } else {
            "background"
        };
        match result {
            Ok(Ok(route)) => key_route_result(
                &format!("Pressed {key_display} on pid {pid}"),
                route,
                mode_label,
            ),
            Ok(Err(e)) => input_error_result(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── set_value ─────────────────────────────────────────────────────────────────

pub struct SetValueTool {
    state: Arc<ToolState>,
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
fn no_such_option_result(idx: usize, error: &anyhow::Error) -> ToolResult {
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
struct QtComboPick {
    /// The option whose row was pressed, and how its text matched the value
    /// (`exact` or `case-insensitive`).
    option: String,
    matched: &'static str,
    /// `click_select_fg` (XTest), `click_select_mpx` or
    /// `click_select_xsendevent` (the background press on the row).
    path: &'static str,
    popup: u64,
    /// The popup closed after the press, as a press on a row closes it.
    popup_closed: bool,
    /// What the focus guards of the background open and press saw, when
    /// something moved.
    guards: Vec<crate::input::FocusGuardReport>,
}

/// Where to press a popup row: the middle of the part of its rectangle that
/// lies inside the popup (a Qt combo row can be wider than its popup).
/// `None` when less than half the row's height is inside it (a row scrolled
/// out of the popup's view), so a press could land on its neighbor.
fn popup_row_point(row: (i32, i32, i32, i32), popup: (i32, i32, u32, u32)) -> Option<(i32, i32)> {
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
fn qt_combo_pick_result(
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
fn values_agree(wanted: &str, observed: &str) -> bool {
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
fn web_page_engine_of(
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
fn set_value_page_result(
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
fn set_value_result(
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
fn path_normalised_to_basename(value: &str, seen: &str) -> bool {
    let value = value.trim();
    let seen = seen.trim();
    value.contains('/')
        && !seen.is_empty()
        && value
            .rsplit('/')
            .next()
            .is_some_and(|base| !base.is_empty() && base == seen)
}

#[cfg(test)]
mod set_value_tests;

// ── get_screen_size ───────────────────────────────────────────────────────────

pub struct GetScreenSizeTool;
static GSS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetScreenSizeTool {
    fn def(&self) -> &ToolDef {
        GSS_DEF.get_or_init(|| ToolDef {
            name: "get_screen_size".into(),
            description: "Return the desktop size and a scale factor. On X11 this is the \
                pixel size of the whole X screen (every monitor) with scale_factor 1.0; on \
                Hyprland it is the logical desktop frame of the powered monitors with the \
                output scale, plus a per-monitor list."
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
        if let Err(result) = parse_typed_input::<GetScreenSizeInput>("get_screen_size", args) {
            return result;
        }
        let result = tokio::task::spawn_blocking(|| {
            if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
                // Shared manifest admission needs content-free display
                // metadata even when this native desktop has no X11 DISPLAY.
                // The adapter attests the IPC/Wayland compositor peer and
                // refuses layouts its logical frame cannot represent: no
                // powered output, or a rotated powered output.
                // Scaled outputs report logical pixels plus the output scale.
                // The size and the monitor list come from one snapshot.
                let (frame, monitors) = crate::wayland::hyprland::screen_report()?;
                return Ok((frame.width, frame.height, frame.scale, Some(monitors)));
            }
            // X11 reports pixel dimensions; scale factor on X11 is not
            // well-defined per-monitor, so report 1.0 (matches DPI-unaware
            // assumption). Other Wayland compositors retain their existing
            // limitation; do not infer native metadata support there.
            let (w, h) = x11_screen_size()?;
            Ok::<(u32, u32, f64, Option<Value>), anyhow::Error>((w, h, 1.0, None))
        })
        .await;
        match result {
            Ok(Ok((w, h, scale, Some(monitors)))) => {
                // Tell the agent which monitors are actually on: the desktop
                // frame covers powered monitors only and changes as they turn
                // on or off, so re-read it after any display change.
                let describe = |m: &Value| {
                    let name = m["name"].as_str().unwrap_or("?");
                    match (
                        m["powered"].as_bool(),
                        m["frame_x"].as_i64(),
                        m["frame_y"].as_i64(),
                    ) {
                        (Some(true), Some(x), Some(y)) => format!(
                            "{name} {}x{} on, at ({x},{y}) in the desktop frame",
                            m["width"], m["height"]
                        ),
                        _ => format!("{name} off (standby), not in the desktop frame"),
                    }
                };
                let lines: Vec<String> = monitors
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(describe)
                    .collect();
                ToolResult::text(format!(
                    "✅ Desktop frame: {w}x{h} logical pixels @ {scale}x, spanning every monitor that is on.\n{}",
                    lines.join("\n")
                ))
                .with_structured(json!({
                    "width": w, "height": h, "scale_factor": scale, "monitors": monitors,
                }))
            }
            // The Windows wording: X11 sizes are pixels, not macOS points.
            Ok(Ok((w, h, scale, None))) => ToolResult::text(format!(
                "✅ Main display: {w}x{h} pixels @ {scale}x (the whole X screen, every monitor)"
            ))
            .with_structured(json!({ "width": w, "height": h, "scale_factor": scale })),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// Read the true X11 root-window size in pixels: (width, height).
/// Shared by `get_screen_size` and `get_desktop_state`.
fn x11_screen_size() -> anyhow::Result<(u32, u32)> {
    use x11rb::connection::Connection;
    use x11rb::rust_connection::RustConnection;
    let (conn, screen_num) = RustConnection::connect(None)
        .map_err(|e| anyhow::anyhow!("{e}{}", crate::no_display_hint()))?;
    let setup = conn.setup();
    let screen = &setup.roots[screen_num];
    let w = screen.width_in_pixels as u32;
    let h = screen.height_in_pixels as u32;
    // WSLg / headless XWayland quirk: the X server connects but the
    // root screen advertises a 0-px geometry until a real output is
    // attached. Returning {width:0,height:0} here would propagate a
    // success with zero dimensions to the client, which then either
    // divides by zero when scaling or feeds the value into `int(...)`
    // after the missing key collapses to None. Fail loudly with an
    // actionable, typed error instead (never emit a 0/null where the
    // client expects a usable int). See issue #2005.
    if w == 0 || h == 0 {
        anyhow::bail!(
            "X11 connected but reports a 0x0 root screen — no usable \
             display geometry.{}",
            crate::no_display_hint()
        );
    }
    Ok((w, h))
}

/// Put the desktop image in the exact coordinate frame consumed by desktop
/// actions. Native Wayland capture buffers may use backing pixels while the
/// compositor's pointer protocol and reported screen geometry use logical
/// pixels. Returning the backing image alongside logical dimensions violates
/// the screenshot-to-action contract and makes every vision-grounded action
/// miss by the output scale.
fn normalize_desktop_capture_for_action_frame(
    image: pincer_core::image_utils::CapturedImage,
    action_width: u32,
    action_height: u32,
) -> anyhow::Result<(pincer_core::image_utils::CapturedImage, u32, u32, f64)> {
    if action_width == 0 || action_height == 0 {
        anyhow::bail!("desktop action frame is empty: {action_width}x{action_height}");
    }

    let (capture_width, capture_height) = image.dimensions()?;
    let scale_x = f64::from(capture_width) / f64::from(action_width);
    let scale_y = f64::from(capture_height) / f64::from(action_height);
    if (scale_x - scale_y).abs() > 0.01 {
        anyhow::bail!(
            "desktop capture {capture_width}x{capture_height} cannot be mapped uniformly to \
             action frame {action_width}x{action_height} (scale {scale_x:.4}x{scale_y:.4})"
        );
    }

    if capture_width == action_width && capture_height == action_height {
        return Ok((image, action_width, action_height, 1.0));
    }

    let png = image.into_png()?;
    let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)?;
    let resized = image.resize_exact(
        action_width,
        action_height,
        image::imageops::FilterType::Lanczos3,
    );
    let mut encoded = std::io::Cursor::new(Vec::new());
    resized.write_to(&mut encoded, image::ImageFormat::Png)?;
    Ok((
        pincer_core::image_utils::CapturedImage::Png(encoded.into_inner()),
        action_width,
        action_height,
        scale_x,
    ))
}

/// One full-display capture in the desktop action coordinate frame. Shared by
/// `get_desktop_state` and `capture_series`.
struct DesktopActionFrame {
    image: pincer_core::image_utils::CapturedImage,
    width: u32,
    height: u32,
    /// Capture pixels per action-frame pixel before normalization.
    scale_factor: f64,
    overlay_capture: cursor_overlay::capture_exclusion::AgentOverlayCapture,
    /// The Hyprland layout the image was composed from.
    hyprland_frame: Option<crate::wayland::hyprland::DesktopFrame>,
}

fn capture_desktop_action_frame() -> anyhow::Result<DesktopActionFrame> {
    // Capture the full display at native size first. When the
    // compositor consumes logical input coordinates, normalize the
    // image below so screenshot pixels still land exactly.
    //
    // The agent reads this image, so the Driver's own cursor and
    // session pill are hidden around the grab (or the limitation is
    // reported) instead of being baked over the controls it reads.
    // Hyprland: read the desktop frame once. The screenshot, its action
    // size and the window geometry below all use it.
    let hyprland_plan = if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        Some(crate::wayland::hyprland::desktop_capture_plan()?)
    } else {
        None
    };
    let (native_image, overlay_capture) =
        cursor_overlay::capture_exclusion::capture_excluding_overlays(
            &crate::overlay_capture::OverlayExcluder,
            |hidden| {
                // A multi-monitor layout is composed from the frame
                // read above, so the image is exactly that frame.
                let image = match &hyprland_plan {
                    Some(plan) if plan.composite => pincer_core::image_utils::CapturedImage::Png(
                        crate::wayland::hyprland::compose_desktop_capture(&plan.frame)?,
                    ),
                    _ => crate::capture::screenshot_display_image()?,
                };
                Ok::<_, anyhow::Error>(crate::overlay_capture::verify_hidden_image_at(
                    image,
                    hidden,
                    (0, 0),
                ))
            },
        )?;
    let (native_w, native_h) = native_image.dimensions()?;
    // True screen size. On a pure-Wayland session (native backend
    // opted in, no X11 DISPLAY) the capture above came from the
    // wlroots `zwlr_screencopy` cascade, whose full-display buffer is
    // the whole output at native (physical) pixels, so the PNG
    // dimensions ARE the true screen size. Querying the X11 root
    // window here would fail with "$DISPLAY variable not set" and
    // abort the tool even though the screenshot already succeeded.
    // Only fall back to the X11 root-window geometry off Wayland, so
    // the X11 / XWayland path is unchanged. See #2017 / Sway testing.
    let hyprland_frame = hyprland_plan.map(|plan| plan.frame);
    let (screen_w, screen_h) = if let Some(frame) = &hyprland_frame {
        (frame.width, frame.height)
    } else if crate::wayland::is_wayland() {
        crate::capture_action_frame::desktop_action_dimensions((native_w, native_h))?
    } else {
        x11_screen_size()?
    };
    let (image, width, height, scale_factor) =
        normalize_desktop_capture_for_action_frame(native_image, screen_w, screen_h)?;
    Ok(DesktopActionFrame {
        image,
        width,
        height,
        scale_factor,
        overlay_capture,
        hyprland_frame,
    })
}

// ── get_desktop_state ─────────────────────────────────────────────────────────

pub struct GetDesktopStateTool {
    state: Arc<ToolState>,
}
static GDS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetDesktopStateTool {
    fn def(&self) -> &ToolDef {
        GDS_DEF.get_or_init(|| ToolDef {
            name: "get_desktop_state".into(),
            description: "Capture the full display in the desktop action coordinate frame. \
                Use the returned PNG directly as the coordinate source for actions whose target is \
                {kind:\"desktop\",display_id:\"primary\"}. No AT-SPI walk. Also lists the \
                visible windows with pid, window_id, z_index and geometry, labeled \
                `screenshot px` (this image) and `desktop px` (list_windows and set_window_frame). The \
                agent cursor is hidden for the capture; `agent_overlay_capture` says how, and \
                `excluded` means it is not in the image.".into(),
            input_schema: json!({"type":"object","properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "screenshot_out_file":{"type":"string","description":"Write PNG here instead of base64."},
                "max_image_dimension": pincer_core::tool_schema::desktop_max_image_dimension_schema(),
                "region": pincer_core::capture_series::region_schema(
                    "Optional crop, in pixels of this session's latest get_desktop_state image (mapped back to the desktop for you, through its frame_scale and region; full-size desktop pixels when there is no earlier image). Must lie inside that image. Applied before max_image_dimension."
                ),
                "native_pixels": native_pixels_schema("true: region is in full-size desktop pixels (list_windows bounds; physical screen pixels on X11) instead. Default false.")
            },"additionalProperties":false}),
            read_only: true, destructive: false, idempotent: false, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        // `region` is a Linux addition outside the portable contract type.
        let input = match parse_typed_projection::<GetDesktopStateInput>("get_desktop_state", &args)
        {
            Ok(input) => input,
            Err(result) => return result,
        };
        let invalid = |message: String| {
            ToolResult::error(format!("get_desktop_state: invalid arguments: {message}"))
                .with_structured(json!({
                    "code": "invalid_arguments",
                    "tool": "get_desktop_state",
                    "detail": message,
                }))
        };
        let requested_region = match pincer_core::capture_series::parse_region(args.get("region"))
        {
            Ok(region) => region,
            Err(message) => return invalid(message),
        };
        // The region is read off the image the caller was given; crop in
        // full-size desktop pixels.
        let (region, mapped_from) = match requested_region {
            None => (None, None),
            Some(requested) => match self.state.desktop_region_arg(&args, requested) {
                Ok((region, mapped)) => (Some(region), mapped.then_some(requested)),
                Err(message) => return invalid(message),
            },
        };
        let record_args = args.clone();
        let capture_args = args;
        let out_file = input.screenshot_out_file;
        let max_image_dimension = input.max_image_dimension;
        let capture_service = self.state.capture_service.clone();

        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let DesktopActionFrame {
                image,
                width: screen_w,
                height: screen_h,
                scale_factor,
                overlay_capture,
                hyprland_frame,
            } = capture_desktop_action_frame()?;
            // Optional crop, in action-frame pixels, before the cap.
            let (image, shot_w, shot_h) = match region {
                Some(region) => {
                    pincer_core::capture_series::check_region(region, (screen_w, screen_h))
                        .map_err(|message| anyhow::anyhow!(message))?;
                    (image.crop(region)?, region.width, region.height)
                }
                None => (image, screen_w, screen_h),
            };
            // Opt-in long-edge cap: the full-size capture is the desktop
            // action frame; a capped image is mapped back at dispatch
            // (pincer_core::desktop_capture_scale) and by its capture_id.
            let (image, shot_w, shot_h) = match max_image_dimension.filter(|cap| *cap > 0) {
                Some(cap) if shot_w.max(shot_h) > cap => {
                    let image = image.fit_long_edge(cap)?;
                    let (w, h) = image.dimensions()?;
                    (image, w, h)
                }
                _ => (image, shot_w, shot_h),
            };
            let (png, encoded_from) = image.into_png_and_rgba()?;
            // Optional: write PNG to disk instead of returning base64.
            let written = if let Some(path) = out_file.as_deref() {
                std::fs::write(path, &png)?;
                Some(path.to_string())
            } else {
                None
            };
            use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
            let b64 = if written.is_some() {
                None
            } else {
                Some(B64.encode(&png))
            };
            // Which pid owns which window is what the agent needs before its
            // first click; without it the first action guesses `pid: 1`.
            let mut windows = crate::wayland::list_windows_dispatch(None);
            windows
                .retain(|w| w.is_on_screen && w.pid.is_some_and(crate::proc_fs::is_process_live));
            if let Some(frame) = &hyprland_frame {
                // Window geometry is in layout coordinates; screenshot pixel
                // (0, 0) is the frame origin. Refuse rather than pair this
                // capture with a layout that changed meanwhile.
                anyhow::ensure!(
                    crate::wayland::hyprland::desktop_frame()? == *frame,
                    "the Hyprland monitor layout changed during get_desktop_state; call it again"
                );
                frame.rebase_windows(&mut windows);
            }
            let capture_id = match region {
                Some(region) => crate::capture_action_frame::publish_desktop_region(
                    &capture_service,
                    &capture_args,
                    &png,
                    encoded_from.as_ref(),
                    (shot_w, shot_h),
                    (screen_w, screen_h),
                    region,
                )?,
                None => crate::capture_action_frame::publish_desktop(
                    &capture_service,
                    &capture_args,
                    &png,
                    encoded_from.as_ref(),
                    (shot_w, shot_h),
                    (screen_w, screen_h),
                )?,
            };
            Ok((
                b64,
                shot_w,
                shot_h,
                screen_w,
                screen_h,
                scale_factor,
                written,
                windows,
                capture_id,
                overlay_capture,
            ))
        })
        .await;

        match result {
            Ok(Ok((
                b64_opt,
                shot_w,
                shot_h,
                screen_w,
                screen_h,
                scale_factor,
                written,
                windows,
                capture_id,
                overlay_capture,
            ))) => {
                self.state.record_desktop_image(
                    &record_args,
                    (shot_w, shot_h),
                    region.unwrap_or(pincer_core::image_utils::PixelRect {
                        x: 0,
                        y: 0,
                        width: screen_w,
                        height: screen_h,
                    }),
                );
                // Desktop pixels per screenshot pixel: the crop's width when a
                // region was taken, the whole screen's otherwise.
                let source_w = region.map_or(screen_w, |region| region.width);
                let frame_scale = if shot_w > 0 {
                    f64::from(source_w) / f64::from(shot_w)
                } else {
                    1.0
                };
                let mut content = Vec::new();
                let mut structured = json!({
                    "platform": "linux",
                    "display": "primary",
                    "screenshot_width": shot_w,
                    "screenshot_height": shot_h,
                    "screen_width": screen_w,
                    "screen_height": screen_h,
                    "scale_factor": scale_factor,
                    "frame_scale": frame_scale,
                    "screenshot_mime_type": "image/png",
                    "windows": windows.iter().map(window_record_json).collect::<Vec<_>>(),
                    "capture_id": capture_id,
                    "agent_overlay_capture": overlay_capture,
                });
                if let Some(region) = region {
                    structured["region"] = json!({
                        "x": region.x, "y": region.y,
                        "width": region.width, "height": region.height,
                    });
                    if let Some(requested) = mapped_from {
                        structured["region_requested"] = json!({
                            "x": requested.x, "y": requested.y,
                            "width": requested.width, "height": requested.height,
                            "frame": "previous_screenshot",
                        });
                    }
                } else if (frame_scale - 1.0).abs() > 0.001 {
                    // Capped: the uncapped capture is the action frame.
                    structured["screenshot_original_width"] = json!(screen_w);
                    structured["screenshot_original_height"] = json!(screen_h);
                }
                if let Some(b64) = b64_opt {
                    content.push(pincer_core::protocol::Content::image_png(b64));
                }
                // A cropped image is not the desktop frame, so window
                // geometry stays in full-size desktop pixels.
                let window_lines = desktop_window_lines(
                    &windows,
                    if region.is_some() { 1.0 } else { frame_scale },
                );
                let mut frame_note = if let Some(region) = region {
                    let from = mapped_from.map_or_else(String::new, |requested| {
                        format!(
                            " (your {},{} {}x{} in the previous screenshot)",
                            requested.x, requested.y, requested.width, requested.height
                        )
                    });
                    format!(
                        "; region {},{} {}x{} of the desktop{from}: screenshot pixel (px,py) is \
                         desktop ({} + px*{frame_scale:.2}, {} + py*{frame_scale:.2}); window \
                         positions below are full-size desktop pixels",
                        region.x, region.y, region.width, region.height, region.x, region.y
                    )
                } else if (frame_scale - 1.0).abs() > 0.001 {
                    format!(
                        "; x/y for scope:\"desktop\" actions are pixels of THIS screenshot \
                         (mapped ×{frame_scale:.2} back to the screen automatically)"
                    )
                } else {
                    String::new()
                };
                cursor_overlay::capture_exclusion::append_summary_note(
                    &mut frame_note,
                    &overlay_capture,
                );
                if let Some(path) = written {
                    structured["screenshot_file_path"] = json!(path);
                    content.push(pincer_core::protocol::Content::text(format!(
                        "✅ Desktop screenshot {shot_w}x{shot_h} written to {path} (screen {screen_w}x{screen_h}{frame_note}){window_lines}"
                    )));
                } else {
                    content.push(pincer_core::protocol::Content::text(format!(
                        "✅ Desktop screenshot {shot_w}x{shot_h} (screen {screen_w}x{screen_h}{frame_note}){window_lines}"
                    )));
                }
                ToolResult {
                    content,
                    is_error: None,
                    structured_content: Some(structured),
                    action_record: None,
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Capture error: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// Text block naming every visible window with the `pid` / `window_id` pair
/// that every other tool takes, so the model's first action targets a real
/// process instead of guessing. Each line names its unit: the geometry in
/// this screenshot's pixels when they differ from the desktop's, then in
/// desktop pixels as the structured `windows` records hold it, and the
/// z_index, so the line says the whole record. Titles and app names are
/// app text, escaped and quoted as in `window_record_line`.
fn desktop_window_lines(windows: &[crate::x11::WindowInfo], frame_scale: f64) -> String {
    let px = |v: i32| (f64::from(v) / frame_scale).round() as i32;
    let sz = |v: u32| (f64::from(v) / frame_scale).round() as u32;
    if windows.is_empty() {
        return "\nVisible windows: none (use list_windows / launch_app).".to_owned();
    }
    let scaled = (frame_scale - 1.0).abs() > 0.001;
    let mut out =
        String::from("\nVisible windows (use these pid + window_id values in every tool call):");
    for w in windows {
        let title = if w.title.is_empty() {
            "(no title)".to_owned()
        } else {
            quoted_title(&w.title)
        };
        let desktop = format!("{}x{}+{}+{} desktop px", w.width, w.height, w.x, w.y);
        let geometry = if scaled {
            format!(
                "{}x{} at ({},{}) screenshot px, {desktop}",
                sz(w.width),
                sz(w.height),
                px(w.x),
                px(w.y)
            )
        } else {
            desktop
        };
        out.push_str(&format!(
            "\n- pid={} window_id={} {} {geometry} z={}{}{}",
            w.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            w.xid,
            title,
            w.z_index
                .map(|z| z.to_string())
                .unwrap_or_else(|| "?".into()),
            if w.app_name.is_empty() {
                String::new()
            } else {
                format!(" app={}", untrusted_token(&w.app_name, SHOWN_NAME_CHARS))
            },
            if w.is_on_screen {
                ""
            } else {
                " is_on_screen=false"
            },
        ));
    }
    out.push_str("\n→ get_window_state(pid, window_id) lists clickable elements; click/type_text take the same pid (+ window_id for x,y).");
    out
}

// ── get_cursor_position ───────────────────────────────────────────────────────

pub struct GetCursorPositionTool;
static GCP_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetCursorPositionTool {
    fn def(&self) -> &ToolDef {
        GCP_DEF.get_or_init(|| ToolDef {
            name: "get_cursor_position".into(),
            description:
                "Return the pointer position in desktop pixels (origin top-left). On Wayland \
                 compositors other than Hyprland it is only the last position this process moved \
                 the pointer to (`source: \"synthetic\"`)."
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
    state: Arc<ToolState>,
}

static MCURSOR_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CursorControlScope {
    Agent,
    Desktop,
}

fn cursor_control_scope(args: &Value) -> CursorControlScope {
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
    state: Arc<ToolState>,
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
        "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"\"window\" (default): window_id's get_window_state screenshot pixels; the window is raised first. \"desktop\": get_desktop_state pixels, no pid/window_id."},
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
                at the end. On Wayland, only scope \"desktop\" without pid/window_id works."
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
                 pass scope \"desktop\" without them for get_desktop_state pixels",
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
    state: Arc<ToolState>,
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
    state: Arc<ToolState>,
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
    state: Arc<ToolState>,
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

// ── check_permissions ─────────────────────────────────────────────────────────

pub struct CheckPermissionsTool;
static PERMS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for CheckPermissionsTool {
    fn def(&self) -> &ToolDef {
        PERMS_DEF.get_or_init(|| ToolDef {
            name: "check_permissions".into(),
            description: "Report whether X11, a Wayland session, and the AT-SPI accessibility \
                bus are reachable, with a fix hint for each failure. Linux has no permission \
                prompts. Fields: `x11`, `wayland` (this is a Wayland session), \
                `wayland_enabled` (Pincer's experimental Wayland backend is on), `atspi`, \
                `xsend_event`. On a working X11 session Wayland is not applicable, not a \
                failure.".into(),
            input_schema: json!({"type":"object","properties":{"prompt":{"type":"boolean","description":"Ignored: Linux has no permission prompts."}},"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, _args: Value) -> ToolResult {
        // Check X11 connectivity (required for window enumeration and input injection).
        let x11_ok = tokio::task::spawn_blocking(|| {
            x11rb::rust_connection::RustConnection::connect(None).is_ok()
        })
        .await
        .unwrap_or(false);

        // Check AT-SPI: not merely "is there a session bus?" but "does
        // org.a11y.Bus actually answer on it?" — the previous env-var-or-
        // /run/user heuristic false-passed exactly the headless/container case
        // (/run/user exists, but no a11y bus → empty trees). Probe for real.
        let dbus_address = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
        let bus_ok = tokio::task::spawn_blocking(crate::health_report::probe_a11y_bus)
            .await
            .unwrap_or(false);
        // The bus name can have an owner while no registry runs, and then
        // every read comes back without a tree (round 8 of the first-call
        // test), so ask the registry itself.
        let registry = if bus_ok {
            tokio::task::spawn_blocking(crate::health_report::probe_atspi_registry)
                .await
                .unwrap_or_else(|error| Err(error.to_string()))
        } else {
            Err("org.a11y.Bus is not reachable".to_owned())
        };
        let atspi_ok = registry.is_ok();

        let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
        let atspi_status = if let (true, Err(why)) = (bus_ok, &registry) {
            format!(
                "❌ org.a11y.Bus reachable, but the AT-SPI registry does not answer: {why}. \
                 Window reads will return no element tree; start at-spi2-registryd on the \
                 accessibility bus"
            )
        } else if atspi_ok {
            match &dbus_address {
                Some(a) => format!(
                    "✅ org.a11y.Bus reachable and the registry answers (DBUS_SESSION_BUS_ADDRESS={a})"
                ),
                None => "✅ org.a11y.Bus reachable and the registry answers".to_string(),
            }
        } else if dbus_address.is_none() {
            "❌ no session bus (DBUS_SESSION_BUS_ADDRESS unset and none auto-discovered) — \
             AT-SPI trees will be empty; start the daemon inside the desktop session"
                .to_string()
        } else {
            format!(
                "❌ session bus present but org.a11y.Bus has no owner: {}",
                crate::a11y::no_bus_advice(crate::a11y::current_desktop().as_deref())
            )
        };
        let status_text = format!(
            "X11 display: {}\nWayland: {}\nAT-SPI (D-Bus): {}\nXSendEvent injection: {}",
            if x11_ok { "✅ connected" } else { "❌ DISPLAY not set or X11 unavailable" },
            match &wayland_display {
                Some(s) if crate::wayland::wayland_enabled() =>
                    format!("✅ native Wayland session (WAYLAND_DISPLAY={s}) — experimental backend ENABLED"),
                Some(s) => format!(
                    "⚠️  native Wayland session (WAYLAND_DISPLAY={s}) — experimental backend OFF; \
                     set {}=1 to enable it",
                    crate::wayland::ENABLE_WAYLAND_ENV
                ),
                // X11 serves every tool here, so no Wayland is no failure.
                None if x11_ok => "➖ not applicable (an X11 session)".to_string(),
                None => "❌ not a Wayland session".to_string(),
            },
            atspi_status,
            if x11_ok { "✅ available" } else { "❌ requires X11" }
        );
        ToolResult::text(status_text)
            .with_structured(json!({ "x11": x11_ok, "wayland": wayland_display.is_some(), "wayland_enabled": crate::wayland::wayland_enabled(), "atspi": atspi_ok, "dbus_session_bus_address": dbus_address, "xsend_event": x11_ok }))
    }
}

#[cfg(test)]
#[tokio::test]
async fn check_permissions_accepts_the_status_prompt_argument() {
    let result = build_registry_with_provider(false, None)
        .invoke("check_permissions", json!({ "prompt": false }))
        .await;
    let wire = serde_json::to_value(&result).unwrap();
    assert!(wire["structuredContent"].get("x11").is_some(), "{wire}");
}

// ── get_config ────────────────────────────────────────────────────────────────

pub struct GetConfigTool {
    state: Arc<ToolState>,
}
static GCFG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetConfigTool {
    fn def(&self) -> &ToolDef {
        GCFG_DEF.get_or_init(|| ToolDef {
            name: "get_config".into(),
            // The text result marks each setting `(saved)` or `(built-in
            // default)`, so the text renderer drops the structured
            // `saved_keys` list as already said (it prints it only when an
            // effect is saved); naming it here sent both round-2 testers
            // looking for it.
            description: "Return the current pincer configuration: each setting's value and \
                whether it is saved in ~/.pincer/config.json (by set_config) or a built-in \
                default. Cursor effects not saved follow the motion style."
                .into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, _args: Value) -> ToolResult {
        let max_image_dimension = self.state.config.read().unwrap().max_image_dimension;
        let (pip_enabled, pip_geometry) = pip_preview::read_pip_keys_from_file();
        let motion = cursor_overlay::motion_defaults::read_saved();
        let badges = cursor_overlay::badge_mode::read_saved();
        // A key counts as saved when ~/.pincer/config.json holds a value its
        // loader accepts; otherwise the built-in default applies. These are
        // the checks `load_driver_config`, `read_pip_keys_from_file` and the
        // cursor loaders make.
        let file = pip_preview::read_config_value;
        let saved_dimension = file("max_image_dimension")
            .and_then(|value| value.as_u64())
            .is_some_and(|value| u32::try_from(value).is_ok());
        let saved_pip = file("experimental_pip").is_some_and(|value| value.is_boolean());
        let saved_badges = file("cursor")
            .and_then(|cursor| cursor.get("badges").cloned())
            .is_some_and(|value| {
                matches!(cursor_overlay::badge_mode::parse_value(&value), Ok(Some(_)))
            });
        let saved_effects: Vec<(&str, bool)> = [
            ("trail", motion.effects.trail),
            ("glow", motion.effects.glow),
            ("magnet", motion.effects.magnet),
            ("ripple", motion.effects.ripple),
            ("squish", motion.effects.squish),
        ]
        .into_iter()
        .filter_map(|(name, value)| Some((name, value?)))
        .collect();

        let mut saved_keys = Vec::new();
        let mut mark = |key: String, saved: bool| {
            if saved {
                saved_keys.push(key);
                "saved"
            } else {
                "built-in default"
            }
        };
        let dimension_source = mark("max_image_dimension".into(), saved_dimension);
        let style_source = mark("cursor.motion.style".into(), motion.style.is_some());
        let timing_source = mark("cursor.motion.timing".into(), motion.timing.is_some());
        for (name, _) in &saved_effects {
            mark(format!("cursor.motion.effects.{name}"), true);
        }
        let badges_source = mark("cursor.badges".into(), saved_badges);
        let pip_source = mark("experimental_pip".into(), saved_pip);
        if pip_geometry.is_some() {
            mark("experimental_pip_geometry".into(), true);
        }

        let motion_json = motion.config_json();
        let effects = if saved_effects.is_empty() {
            "no effect saved, each follows the style".to_owned()
        } else {
            let pinned: Vec<String> = saved_effects
                .iter()
                // set_config's vocabulary (on/off), so a model can copy it back.
                .map(|(name, value)| format!("{name}={}", if *value { "on" } else { "off" }))
                .collect();
            format!("{} (saved), the rest follow the style", pinned.join(", "))
        };
        let mut lines = vec![
            format!(
                "pincer configuration (version {}, platform linux). \"saved\" values come from \
                 ~/.pincer/config.json; the others are built-in defaults.",
                env!("CARGO_PKG_VERSION")
            ),
            format!("- max_image_dimension={max_image_dimension} ({dimension_source})"),
            format!(
                "- cursor.motion.style={} ({style_source}), cursor.motion.timing={} \
                 ({timing_source}), cursor.motion.effects: {effects}",
                motion_json["style"].as_str().unwrap_or_default(),
                motion_json["timing"].as_str().unwrap_or_default(),
            ),
            format!("- cursor.badges={} ({badges_source})", badges.as_str()),
            format!("- experimental_pip={pip_enabled} ({pip_source})"),
        ];
        if let Some(geometry) = &pip_geometry {
            lines.push(format!("- experimental_pip_geometry={geometry} (saved)"));
        }
        ToolResult::text(lines.join("\n")).with_structured(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "source_sha": option_env!("PINCER_SOURCE_SHA"),
            "platform": "linux",
            "max_image_dimension": max_image_dimension,
            "cursor": {
                "motion": motion_json,
                "badges": badges.as_str(),
            },
            "experimental_pip": pip_enabled,
            "experimental_pip_geometry": pip_geometry,
            "saved_keys": saved_keys,
        }))
    }
}

// ── set_config ────────────────────────────────────────────────────────────────

fn with_cursor_motion_config_properties(mut schema: Value) -> Value {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.extend(cursor_overlay::motion_defaults::config_schema_properties());
        properties.extend(cursor_overlay::badge_mode::config_schema_properties());
    }
    schema
}

pub struct SetConfigTool {
    state: Arc<ToolState>,
}
static SCFG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetConfigTool {
    fn def(&self) -> &ToolDef {
        SCFG_DEF.get_or_init(|| ToolDef {
            name: "set_config".into(),
            description: "Update the driver configuration. max_image_dimension, the long-edge \
                cap of get_window_state screenshots, takes effect immediately (get_desktop_state \
                ignores it and takes its own max_image_dimension). The result echoes each key this call set as `key=value`; \
                get_config shows the rest.\n\n\
                Pass each setting as its own field: `{\"max_image_dimension\": 0}`.\n\n\
                The experimental_pip keys persist to ~/.pincer/config.json and apply on the \
                next daemon restart; the Linux PiP backend is a stub, so they show nothing here.\n\n\
                `cursor.motion.style`, `cursor.motion.timing` and `cursor.motion.effects.<name>` \
                (`on`, `off`, or `default`) save the default cursor motion for sessions started afterwards. \
                `cursor.motion: \"default\"` clears every saved cursor motion default.\n\n\
                `cursor.badges` (`minimal` default, `verbose`, or `off`) sets the agent cursor badge \
                for every cursor at once.".into(),
            input_schema: with_cursor_motion_config_properties(json!({"type":"object","properties":{
                "max_image_dimension":{"type":"integer","description":"Long-edge cap in pixels for get_window_state screenshots (0 = no limit); get_desktop_state ignores it."},
                "experimental_pip":{"type":"boolean","description":"Enable the experimental PiP preview window (applies next restart; Linux backend stubbed)."},
                "experimental_pip_geometry":{"type":"string","description":"PiP window size + optional position in `WxH` or `WxH+X+Y` form."}
            },"additionalProperties":false})),
            read_only: false, destructive: false, idempotent: true, open_world: false,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        if args.get("capture_scope").is_some() {
            return ToolResult::error(
                "config key 'capture_scope' is retired; select a window or desktop target on each action",
            )
            .with_structured(json!({
                "code": "config_key_retired",
                "key": "capture_scope",
                "replacement": "action.target",
            }));
        }
        let motion_keys = match cursor_overlay::motion_defaults::apply_config_args(&args) {
            Ok(keys) => keys,
            Err(message) => return ToolResult::error(message),
        };
        let badge_key = match cursor_overlay::badge_mode::apply_config_args(&args) {
            Ok(key) => key,
            Err(message) => return ToolResult::error(message),
        };
        let mut cfg = self.state.config.write().unwrap();
        // The result names each key this call set with its new value, in
        // the text and the structured echo, and nothing else: at 3eefd3e a
        // style change answered `cursor.motion.style` with no value and
        // then the unrelated max_image_dimension and experimental_pip.
        let mut updated = serde_json::Map::new();
        let as_set = |value: &Value| match value {
            Value::Null => "default".to_owned(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let mut parts: Vec<String> = Vec::new();
        for key in &motion_keys {
            let value = args.get(key).cloned().unwrap_or(Value::Null);
            let note = if key == cursor_overlay::motion_defaults::KEY_PREFIX {
                "cleared every saved cursor motion default; applies to sessions started from now on"
            } else {
                "applies to sessions started from now on"
            };
            parts.push(format!("{key}={} ({note})", as_set(&value)));
            updated.insert(key.clone(), value);
        }
        if let Some(key) = badge_key {
            let value = args.get(&key).cloned().unwrap_or(Value::Null);
            parts.push(format!("{key}={} (applies now)", as_set(&value)));
            updated.insert(key, value);
        }
        if let Some(dim) = args.opt_u64("max_image_dimension") {
            cfg.max_image_dimension = dim as u32;
            if let Err(e) = pip_preview::write_config_key("max_image_dimension", Value::from(dim)) {
                tracing::warn!("set_config: failed to persist max_image_dimension: {e}");
            }
            parts.push(format!("max_image_dimension={dim}"));
            updated.insert("max_image_dimension".into(), Value::from(dim));
        }
        if let Some(enabled) = args.get("experimental_pip").and_then(|v| v.as_bool()) {
            if let Err(e) = pip_preview::write_config_key("experimental_pip", Value::Bool(enabled))
            {
                return ToolResult::error(format!("failed to persist experimental_pip: {e}"));
            }
            parts.push(format!("experimental_pip={enabled} (next restart)"));
            updated.insert("experimental_pip".into(), Value::Bool(enabled));
        }
        if let Some(geom) = args.opt_str("experimental_pip_geometry") {
            if pip_preview::PipGeometry::parse(&geom).is_none() {
                return ToolResult::error(format!(
                    "experimental_pip_geometry `{geom}` is not a valid WxH or WxH+X+Y string"
                ));
            }
            if let Err(e) = pip_preview::write_config_key(
                "experimental_pip_geometry",
                Value::String(geom.clone()),
            ) {
                return ToolResult::error(format!(
                    "failed to persist experimental_pip_geometry: {e}"
                ));
            }
            parts.push(format!("experimental_pip_geometry={geom} (next restart)"));
            updated.insert("experimental_pip_geometry".into(), Value::String(geom));
        }
        let msg = if parts.is_empty() {
            "Config unchanged (no known parameters).".to_owned()
        } else {
            format!("Config updated: {}", parts.join(", "))
        };
        ToolResult::text(msg).with_structured(Value::Object(updated))
    }
}

// ── zoom ──────────────────────────────────────────────────────────────────────

pub struct ZoomTool {
    state: Arc<ToolState>,
}
static ZOOM_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for ZoomTool {
    fn def(&self) -> &ToolDef {
        ZOOM_DEF.get_or_init(|| ToolDef {
            name: "zoom".into(),
            description: "Capture a cropped JPEG of a window region from (x1,y1) to (x2,y2), in \
                pixels of this window's most recent get_window_state screenshot, padded by 20% \
                of the region's width and height on each side (clamped to the window). Output \
                is at most 500 px wide and never enlarged past the screenshot's own pixels. \
                Corners, not a size: x1,y1 is the top-left and x2,y2 the bottom-right (x,y,width,height is also accepted and converted). The result echoes the padded region the image actually shows, in the same screenshot pixels.\n\n\
                After a zoom, pass from_zoom=true to click, double_click, right_click, drag or \
                a mouse_button / mouse_drag call to auto-translate coordinates back to \
                full-window space. Coordinate actions return \
                `screenshot_context_missing` when no current snapshot contains a \
                screenshot owned by this session. `from_zoom` actions return \
                `zoom_context_missing` when the zoom was never created or was replaced; call \
                `get_window_state`, then `zoom`, again on the same connection.".into(),
            input_schema: json!({
                "type":"object","required":["window_id","x1","y1","x2","y2"],"properties":{
                    "window_id":{"type":"integer","description":"Window id of the window captured by get_window_state."},
                    "pid":{"type":"integer","description":"Optional target pid. When omitted, the driver resolves the unique current snapshot for this session and window."},
                    "x1":{"type":"number","description":"Left edge of the region in window-local screenshot pixels."},
                    "y1":{"type":"number","description":"Top edge of the region in window-local screenshot pixels."},
                    "x2":{"type":"number","description":"Right edge of the region in window-local screenshot pixels."},
                    "y2":{"type":"number","description":"Bottom edge of the region in window-local screenshot pixels."}
                },"additionalProperties":false
            }),
            read_only: true, destructive: false, idempotent: true, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        let xid = match args.require_u64("window_id") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let requested_pid = match args.get("pid") {
            None => None,
            Some(value) => match value.as_u64().and_then(|pid| i32::try_from(pid).ok()) {
                Some(pid) => Some(pid),
                None => return ToolResult::error("pid must be a positive 32-bit integer"),
            },
        };
        let session_id = args.opt_str("_session_id");
        let (pid, screenshot) = match self.state.snapshots.screenshot_context_for_zoom(
            requested_pid,
            xid,
            session_id.as_deref(),
        ) {
            Ok(context) => context,
            Err(refusal) => return refusal,
        };
        let x1 = match args.opt_f64("x1") {
            Some(v) => v,
            None => return ToolResult::error("Missing x1"),
        };
        let y1 = match args.opt_f64("y1") {
            Some(v) => v,
            None => return ToolResult::error("Missing y1"),
        };
        let x2 = match args.opt_f64("x2") {
            Some(v) => v,
            None => return ToolResult::error("Missing x2"),
        };
        let y2 = match args.opt_f64("y2") {
            Some(v) => v,
            None => return ToolResult::error("Missing y2"),
        };
        if x2 <= x1 || y2 <= y1 {
            return ToolResult::error("x2 must be > x1 and y2 must be > y1");
        }

        let requested = (x1, y1, x2, y2);
        let scale = screenshot.scale;
        let (x1, y1, x2, y2) = (x1 * scale, y1 * scale, x2 * scale, y2 * scale);
        let state = self.state.clone();
        let result = tokio::task::spawn_blocking(move || {
            // Route through the Wayland-aware window capture dispatcher so
            // pure-Wayland sessions surface a typed "per-window capture not
            // supported yet" error instead of accidentally calling the
            // X11-only path with a foreign-toplevel id.
            let image = crate::wayland::screenshot_window_image_dispatch(xid)?.into_dynamic()?;
            cursor_overlay::capture_utils::crop_image_to_jpeg(&image, x1, y1, x2, y2, 500)
        })
        .await;

        match result {
            Ok(Ok(crop)) => {
                if let Err(refusal) = state.snapshots.set_zoom(
                    pid,
                    session_id.as_deref(),
                    ZoomContext {
                        screenshot,
                        origin_x: crop.origin_x,
                        origin_y: crop.origin_y,
                        scale_inv: crop.scale_inv,
                    },
                ) {
                    return refusal;
                }
                use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
                let b64 = B64.encode(&crop.jpeg_bytes);
                let (w, h) = (crop.out_w, crop.out_h);
                // The padded crop the image shows, back in the caller's
                // screenshot pixels (the crop ran on the native window).
                let padded = (
                    crop.origin_x / scale,
                    crop.origin_y / scale,
                    (crop.origin_x + f64::from(crop.crop_w)) / scale,
                    (crop.origin_y + f64::from(crop.crop_h)) / scale,
                );
                let (rx1, ry1, rx2, ry2) = requested;
                let (px1, py1, px2, py2) = padded;
                use pincer_core::protocol::Content;
                ToolResult {
                    content: vec![
                        Content::image_jpeg(b64),
                        Content::text(format!(
                            "Zoom of ({rx1:.0},{ry1:.0})-({rx2:.0},{ry2:.0}): the image shows \
                             ({px1:.0},{py1:.0})-({px2:.0},{py2:.0}) with the padding, in \
                             screenshot pixels, as a {w}x{h} px JPEG."
                        )),
                    ],
                    is_error: None,
                    // Surface 7: `mime_type` mirrors the MCP image part's `mimeType`
                    // onto the structured payload (additive — `format` stays).
                    structured_content: Some(json!({
                        "width": w, "height": h, "format": "jpeg",
                        "mime_type": "image/jpeg",
                        "shown_region": {"x1": px1, "y1": py1, "x2": px2, "y2": py2},
                    })),
                    action_record: None,
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Zoom failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── capture_series ────────────────────────────────────────────────────────────

/// The whole desktop, in the desktop action frame `get_desktop_state` uses.
struct DesktopFrameSource;

impl pincer_core::capture_series::FrameSource for DesktopFrameSource {
    fn frame_size(&self) -> anyhow::Result<Option<(u32, u32)>> {
        // On X11 the desktop action frame is the root window. Wayland frames
        // are measured on the first capture.
        if crate::wayland::is_wayland() {
            Ok(None)
        } else {
            x11_screen_size().map(Some)
        }
    }

    fn capture(
        &self,
        region: Option<pincer_core::image_utils::PixelRect>,
    ) -> anyhow::Result<pincer_core::capture_series::SourceFrame> {
        if let (Some(region), false) = (region, crate::wayland::is_wayland()) {
            // X11 maps the root window 1:1 onto the action frame, so read
            // only the region from it instead of the whole screen.
            let frame_size = x11_screen_size()?;
            let origin = (i32::try_from(region.x)?, i32::try_from(region.y)?);
            let (image, _) = cursor_overlay::capture_exclusion::capture_excluding_overlays(
                &crate::overlay_capture::OverlayExcluder,
                |hidden| {
                    let rgba = crate::capture::screenshot_root_region_rgba(
                        origin.0,
                        origin.1,
                        region.width,
                        region.height,
                    )?;
                    Ok::<_, anyhow::Error>(crate::overlay_capture::verify_hidden_image_at(
                        pincer_core::image_utils::CapturedImage::Rgba(rgba),
                        hidden,
                        origin,
                    ))
                },
            )?;
            return Ok(pincer_core::capture_series::SourceFrame {
                image,
                origin: (region.x, region.y),
                frame_size,
            });
        }
        let frame = capture_desktop_action_frame()?;
        Ok(pincer_core::capture_series::SourceFrame {
            image: frame.image,
            origin: (0, 0),
            frame_size: (frame.width, frame.height),
        })
    }

    fn label(&self) -> String {
        "desktop".into()
    }
}

/// One window at native size, captured the way `get_window_state` does.
struct WindowFrameSource {
    pid: u32,
    xid: u64,
    /// The native size when a probe already measured it.
    known_size: Option<(u32, u32)>,
}

impl WindowFrameSource {
    fn grab(&self) -> anyhow::Result<pincer_core::image_utils::CapturedImage> {
        let overlays = if crate::wayland::is_wayland() {
            WindowOverlays::default()
        } else {
            window_overlays(self.pid, self.xid)
        };
        capture_window_image(self.xid, self.pid, &overlays).map_err(|error| {
            if !crate::wayland::is_wayland() && !crate::x11::window_exists(self.xid) {
                anyhow::anyhow!(
                    "Window target pid {}, window_id {} is stale or no longer running; refresh list_windows.",
                    self.pid,
                    self.xid
                )
            } else {
                error
            }
        })
    }
}

impl pincer_core::capture_series::FrameSource for WindowFrameSource {
    fn frame_size(&self) -> anyhow::Result<Option<(u32, u32)>> {
        if let Some(size) = self.known_size {
            return Ok(Some(size));
        }
        // Only a capture tells the exact native size, so probe once.
        self.grab()?.dimensions().map(Some)
    }

    fn capture(
        &self,
        _region: Option<pincer_core::image_utils::PixelRect>,
    ) -> anyhow::Result<pincer_core::capture_series::SourceFrame> {
        let image = self.grab()?;
        let frame_size = image.dimensions()?;
        Ok(pincer_core::capture_series::SourceFrame {
            image,
            origin: (0, 0),
            frame_size,
        })
    }

    fn label(&self) -> String {
        format!("window {} of pid {}", self.xid, self.pid)
    }
}

pub struct CaptureSeriesTool {
    state: Arc<ToolState>,
}
static CAPTURE_SERIES_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for CaptureSeriesTool {
    fn def(&self) -> &ToolDef {
        CAPTURE_SERIES_DEF.get_or_init(|| ToolDef {
            name: pincer_core::capture_series::TOOL_NAME.into(),
            description: "Take screenshots on a schedule: `count` frames `interval_ms` apart \
                (default 5 x 1000 ms) or at `offsets_ms` from the start, all timed from one \
                start so delays never accumulate. Captures the desktop, or one window with \
                pid + window_id. `region` crops each frame, then `max_image_dimension` caps \
                its long edge. A region is in the pixels of the image you read it off: this \
                session's latest get_desktop_state image for the desktop, the window's latest \
                get_window_state screenshot for a window (with native_pixels:true, full-size \
                desktop or native window pixels). `return_frames`: all, changed (differs from the last kept frame in more than \
                `change_threshold` of its pixels) or last. Inline replies carry at most \
                `max_inline_images`; `output:\"files\"` writes new frame-0001.png, ... into \
                `out_dir` instead. Each frame reports t_ms, and late when the previous capture \
                overran its slot. `stop_after_unchanged_ms` ends the series early once no frame \
                has differed from the one before it for that long. At most 120 frames within \
                600000 ms."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "pid":{"type":"integer","description":"Window owner pid; with window_id, capture that window. Omit both for the desktop."},
                "window_id":{"type":"integer","description":"Window id from list_windows, with pid."},
                "count":{"type":"integer","minimum":1,"maximum":120,"description":"Frames to take (default 5). Not with offsets_ms."},
                "interval_ms":{"type":"integer","minimum":0,"maximum":60000,"description":"Time between frame starts (default 1000; 0 = back to back). Not with offsets_ms."},
                "offsets_ms":{"type":"array","items":{"type":"integer","minimum":0},"minItems":1,"maxItems":120,"description":"Strictly increasing capture times in ms from the start, instead of count and interval_ms."},
                "region": pincer_core::capture_series::region_schema(
                    "Crop {x,y,width,height} in pixels of this session's latest get_desktop_state image (desktop), or of the window's latest get_window_state screenshot (pid/window_id); mapped to the frame for you. Must lie inside that image."
                ),
                "native_pixels": native_pixels_schema("true: region is in full-size desktop pixels (physical screen pixels on X11), or native window pixels with pid/window_id, instead. Default false."),
                "max_image_dimension":{"type":"integer","minimum":0,"description":"Long-edge cap applied after the crop (0 or omitted: none)."},
                "format":{"type":"string","enum":["png","jpeg"],"description":"Image format (default png)."},
                "quality":{"type":"integer","minimum":1,"maximum":100,"description":"JPEG quality (default 80)."},
                "output":{"type":"string","enum":["inline","files"],"description":"inline (default) returns the images; files writes them to out_dir."},
                "out_dir":{"type":"string","description":"Absolute path of an existing directory, not a symlink. Frame files that already exist are never overwritten; the call refuses instead."},
                "return_frames":{"type":"string","enum":["all","changed","last"],"description":"Which frames to return (default all)."},
                "max_inline_images":{"type":"integer","minimum":1,"maximum":30,"description":"Most images in an inline reply (default 10). For changed, the first and the latest frames are kept."},
                "change_threshold":{"type":"number","minimum":0,"maximum":1,"description":"Fraction of pixels that must differ for a frame to count as changed (default 0.001), measured on the cropped, capped frame."},
                "stop_after_unchanged_ms":{"type":"integer","minimum":0,"maximum":600000,"description":"End early once no frame has differed from the previous frame (per change_threshold) for this long. The first frame starts the clock and any change restarts it. The reply then reports stopped_early and stop_reason."}
            },"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: false,
            open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::capture_series::{self, FrameSource, SeriesRequest};
        let mut request = match SeriesRequest::parse(&args) {
            Ok(request) => request,
            Err(message) => return capture_series::invalid_arguments(&message),
        };
        let source: Arc<dyn FrameSource> = match (args.get("pid"), args.get("window_id")) {
            (None, None) => {
                if let Some(region) = request.region {
                    match self.state.desktop_region_arg(&args, region) {
                        Ok((region, _)) => request.region = Some(region),
                        Err(message) => return capture_series::invalid_arguments(&message),
                    }
                }
                Arc::new(DesktopFrameSource)
            }
            (Some(pid), Some(window_id)) => {
                let Some(pid) = pid
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .filter(|pid| *pid > 0)
                else {
                    return capture_series::invalid_arguments(
                        "`pid` must be a positive 32-bit integer",
                    );
                };
                let Some(xid) = window_id.as_u64().filter(|xid| *xid > 0) else {
                    return capture_series::invalid_arguments(
                        "`window_id` must be a positive integer",
                    );
                };
                let current = tokio::task::spawn_blocking(move || {
                    let meta = crate::wayland::list_windows_dispatch(Some(pid))
                        .into_iter()
                        .find(|w| w.xid == xid);
                    crate::proc_fs::is_process_live(pid)
                        && window_target_matches(pid, xid, meta.as_ref())
                })
                .await
                .unwrap_or(false);
                if !current {
                    return ToolResult::error(format!(
                        "Window target pid {pid}, window_id {xid} is stale or no longer running; refresh list_windows."
                    ));
                }
                let mut source = WindowFrameSource {
                    pid,
                    xid,
                    known_size: None,
                };
                if let Some(region) = request.region.filter(|_| !native_pixels_requested(&args)) {
                    // Screenshot pixels of the window's latest get_window_state
                    // read; the series crops native window pixels.
                    let scale = match screenshot_scale(&self.state, &args, pid, Some(xid)) {
                        Ok(scale) => scale,
                        Err(refusal) => return refusal,
                    };
                    let probe = WindowFrameSource {
                        pid,
                        xid,
                        known_size: None,
                    };
                    let native = match tokio::task::spawn_blocking(move || probe.frame_size()).await
                    {
                        Ok(Ok(size)) => size,
                        Ok(Err(error)) => return ToolResult::error(format!("{error:#}")),
                        Err(error) => return ToolResult::error(format!("Task error: {error}")),
                    };
                    source.known_size = native;
                    request.region = Some(window_region_from_screenshot(region, scale, native));
                }
                Arc::new(source)
            }
            _ => {
                return capture_series::invalid_arguments(
                    "pass both pid and window_id for a window, or neither for the desktop",
                )
            }
        };
        capture_series::run(request, source).await
    }
}

// ── registry ─────────────────────────────────────────────────────────────────

pub fn build_registry_with_provider(
    compat: bool,
    provider: Option<std::sync::Arc<dyn pincer_core::consent::ProtectedConsentProvider>>,
) -> ToolRegistry {
    let mut r = ToolRegistry::new_with_protected_consent_provider(provider);
    let state = ToolState::new_with_capture_service(r.capture_service());
    if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        let recording_state = Arc::downgrade(&state);
        r.recording
            .set_pixel_point_fn(move |args, window_id, pid, mut x, mut y| {
                let state = recording_state.upgrade()?;
                if let Some(pid) = pid {
                    let pid = u32::try_from(pid).ok()?;
                    // Match ClickTool's screenshot and zoom conversion before
                    // translating to the recording's full-output image.
                    if args.bool_or("from_zoom", false) {
                        (x, y) = state
                            .zoom_context(args, pid, args.opt_u64("window_id"))
                            .ok()?
                            .zoom_to_window(x, y);
                    } else if let Ok(ratio) =
                        screenshot_scale(&state, args, pid, args.opt_u64("window_id"))
                    {
                        x *= ratio;
                        y *= ratio;
                    }
                }
                crate::recording_hooks::hyprland_pixel_recording_point(window_id, pid, x, y)
            });
    }
    let cursor_outcome_reader = {
        let cursor_registry = state.cursor_registry.clone();
        pincer_core::session::register_scoped_cursor_outcome_reader(std::sync::Arc::new(
            move |session_id| {
                let state = cursor_registry.get(session_id);
                let motion_customized = state.is_some()
                    && crate::overlay::current_motion_for(session_id)
                        != cursor_overlay::MotionConfig::default();
                let active_cursor_count = cursor_registry
                    .all_states()
                    .iter()
                    .filter(|state| state.config.cursor_id != "default")
                    .count()
                    .max(1);
                match state {
                    Some(state) => pincer_core::session::bounded_cursor_outcome(
                        true,
                        state.config.enabled,
                        crate::overlay::is_visible_for_session(session_id),
                        Some(state.config.theme_id.as_str()),
                        motion_customized,
                        active_cursor_count,
                    ),
                    None => pincer_core::session::bounded_cursor_outcome(
                        false,
                        false,
                        false,
                        None,
                        false,
                        active_cursor_count,
                    ),
                }
            },
        ))
    };
    let session_end_hook = {
        let cursor_registry = state.cursor_registry.clone();
        let state_for_session_end = state.clone();
        pincer_core::session::register_scoped_fallible_session_end_hook(
            "linux_input",
            move |session_id| {
                if crate::wayland::is_wayland() {
                    release_mouse_hold_for_session(
                        &state_for_session_end,
                        session_id,
                        |cursor_id, held| {
                            crate::wayland::persistent_vptr::release(cursor_id, held.button)
                        },
                    )?;
                } else {
                    // A foreground hold is a real XTest button on the core
                    // pointer: release it so the user's pointer is not left
                    // pressed. Best effort; the hold is dropped either way.
                    let _ = release_mouse_hold_for_session(
                        &state_for_session_end,
                        session_id,
                        |_, held| {
                            if held.foreground {
                                crate::input::send_button_xtest_desktop(None, held.button, false)
                            } else {
                                Ok(())
                            }
                        },
                    );
                    clear_mouse_hold_after_release(&state_for_session_end, session_id);
                }

                state_for_session_end
                    .snapshots
                    .retire_session_screenshots(session_id);
                cursor_registry.remove(session_id);
                crate::overlay::remove_cursor(session_id.to_owned());
                crate::input::forget_master_pointer(session_id);
                crate::wayland::hyprland_input::cleanup_session(session_id);
                Ok(())
            },
        )
    };
    let session_revive_hook =
        pincer_core::session::register_scoped_session_revive_hook(move |session_id| {
            crate::overlay::revive_cursor(session_id.to_owned());
        });
    r.retain_cursor_outcome_reader(cursor_outcome_reader);
    r.retain_session_end_hook(session_end_hook);
    r.retain_session_revive_hook(session_revive_hook);
    if let Some(runtime_scope) = pincer_core::tool::current_dispatch_runtime_scope() {
        let prefix = format!("__pincer_runtime_{runtime_scope}:");
        let cursor_registry = state.cursor_registry.clone();
        let capture_service = state.capture_service.clone();
        r.retain_runtime_cleanup(move || {
            crate::capture_action_frame::retire_runtime(&capture_service);
            crate::wayland::hyprland_input::cleanup_runtime(&prefix);
            for cursor in cursor_registry
                .all_states()
                .into_iter()
                .filter(|cursor| cursor.config.cursor_id.starts_with(&prefix))
            {
                cursor_registry.remove(&cursor.config.cursor_id);
                crate::overlay::remove_cursor(cursor.config.cursor_id);
            }
        });
    }
    r.register(Box::new(ListAppsTool));
    r.register(Box::new(ListWindowsTool));
    r.register(Box::new(GetWindowStateTool {
        state: state.clone(),
    }));
    r.register(Box::new(pincer_core::expectation::VerifyStateTool::new(
        std::sync::Arc::new(pincer_core::expectation::ToolObservationProvider::new(
            std::sync::Arc::new(ListWindowsTool),
            std::sync::Arc::new(GetWindowStateTool {
                state: state.clone(),
            }),
        )),
    )));
    r.register(Box::new(LaunchAppTool));
    // AppImage run folders a crash or an earlier driver left behind.
    #[cfg(not(test))]
    if let Some(root) = appimage::run_root() {
        let _ = std::thread::Builder::new()
            .name("pincer-appimage-sweep".into())
            .spawn(move || {
                appimage::sweep_run_dirs(&root);
            });
    }
    let browser_engine = pincer_core::browser::BrowserEngine::new_with_runtime_services(
        Arc::new(crate::browser_platform::LinuxBrowserPlatform::new(
            state.cursor_registry.clone(),
        )),
        r.approval_broker(),
        r.protected_resource_ownership(),
    );
    r.register(Box::new(KillAppTool {
        browsers: browser_engine.clone(),
    }));
    let pid_window_candidates: PidWindowGuardParts = (
        Arc::new(pid_window_target_candidates),
        desktop_point_window_resolver(),
        pid_fallback_window_resolver(),
        snapshot_window_resolver(state.clone()),
        token_pid_resolver(state.clone()),
    );
    // run_steps gives a step that carries only an element_token the window
    // the token was read from.
    let token_state = state.clone();
    r.set_element_token_window_resolver(Arc::new(move |token| {
        let (pid, window_id) = token_state.snapshots.window_for_token(token)?;
        Some((i64::from(pid), window_id))
    }));
    r.register(pid_window_guarded(BringToFrontTool, &pid_window_candidates));
    r.register(Box::new(SetWindowFrameTool));
    r.register(Box::new(InvokeMenuTool));
    r.register(pid_window_guarded(
        ClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        DoubleClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        RightClickTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        DragTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseButtonDownTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseDragTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        MouseButtonUpTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    // Offered only where it can run: every call is refused elsewhere (KDE
    // Plasma X11, XWayland, Xvfb, no /dev/uinput, native Wayland without
    // the cua-compositor socket), and it has no foreground fallback. The
    // probe opens no device.
    match parallel_drag_unavailable_reason(
        crate::wayland::is_inject_mode(),
        crate::wayland::is_wayland(),
        crate::input::real_pointer_unavailable_reason,
    ) {
        None => r.register(Box::new(ParallelMouseDragTool {
            state: state.clone(),
        })),
        Some(reason) => tracing::debug!("parallel_mouse_drag not offered: {reason}"),
    }
    r.register(pid_window_guarded(
        TypeTextTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        PressKeyTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        HotkeyTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        SetValueTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    r.register(pid_window_guarded(
        ScrollTool {
            state: state.clone(),
        },
        &pid_window_candidates,
    ));
    pincer_core::clipboard::register_clipboard_tools(
        &mut r,
        Arc::new(crate::clipboard::LinuxClipboard::new()),
    );
    // `screenshot` removed - see the matching comment in
    // platform-windows/src/tools/impl_.rs::build_registry. Canonical
    // screenshot path is `get_window_state` (it returns one by default).
    let _ = compat;
    r.register(Box::new(GetScreenSizeTool));
    r.register(Box::new(GetDesktopStateTool {
        state: state.clone(),
    }));
    r.register(Box::new(GetCursorPositionTool));
    r.register(Box::new(MoveCursorTool {
        state: state.clone(),
    }));
    r.register(Box::new(MoveAlongPathTool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorEnabledV2Tool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorMotionV2Tool));
    r.register(Box::new(GetAgentCursorStateV2Tool {
        state: state.clone(),
    }));
    r.register(Box::new(SetAgentCursorThemeTool {
        state: state.clone(),
    }));
    r.register(Box::new(CheckPermissionsTool));
    // `health_report` — single-call cross-platform driver diagnostics.
    // Stable schema_version="1" contract for downstream consumers. Linux skips
    // tcc_* and bundle_identity with "not applicable on Linux".
    r.register(Box::new(pincer_core::health_report::HealthReportTool::new(
        std::sync::Arc::new(crate::health_report::LinuxHealthProvider),
    )));
    r.register(Box::new(GetConfigTool {
        state: state.clone(),
    }));
    r.register(Box::new(SetConfigTool {
        state: state.clone(),
    }));
    r.register(Box::new(GetAccessibilityTreeTool));
    r.register(Box::new(ZoomTool {
        state: state.clone(),
    }));
    r.register(Box::new(CaptureSeriesTool {
        state: state.clone(),
    }));
    // `type_text_chars` needs no tool here: pincer-core's registry resolves
    // it to `type_text` at invoke time and keeps it out of tools/list.
    pincer_core::browser::register_browser_tools(&browser_engine, &mut r);
    r.register_recording_tools();
    r.register_session_tools();
    r
}

#[cfg(test)]
mod click_button_schema_tests;

#[cfg(test)]
mod pid_window_target_tests;

#[cfg(test)]
mod session_cursor_target_tests;

#[cfg(test)]
mod display_only_tests;
#[cfg(test)]
mod since_screenshot_default_tests;
#[cfg(test)]
mod window_capture_dimension_tests;

#[cfg(test)]
mod desktop_capture_frame_tests;

#[cfg(test)]
mod background_budget_tests;

#[cfg(test)]
mod background_keyboard_route_tests;

#[cfg(test)]
mod visibility_tests;

#[cfg(test)]
mod result_wording_tests;
