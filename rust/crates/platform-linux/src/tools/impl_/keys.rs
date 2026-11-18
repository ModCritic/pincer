//! press_key and hotkey, and the modifier and chord parsing they share.

use super::*;

// ── press_key ─────────────────────────────────────────────────────────────────

pub struct PressKeyTool {
    pub(super) state: Arc<ToolState>,
}
static PRESS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Turn a modified `press_key` request into the chord an underlying keyboard
/// path expects when it takes a single key and no separate modifier list.
///
/// Returns `None` when there is nothing to promote, so the caller keeps the
/// plain single-key route. Dropping the modifiers instead is not a silent
/// no-op: the bare key still reaches the app and is typed as a character.
pub(super) fn press_key_chord(mods: &[String], key: &str) -> Option<Vec<String>> {
    if mods.is_empty() {
        return None;
    }
    let mut chord = mods.to_vec();
    chord.push(key.to_owned());
    Some(chord)
}

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

/// A tool's `modifier` / `modifiers` argument as canonical names (ctrl,
/// shift, alt, super). A name that is not a modifier is refused: the X11
/// synthetic route used to drop it and send the unmodified action.
pub(super) fn modifier_arg(tool: &str, field: &str, names: &[String]) -> Result<Vec<String>, ToolResult> {
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
pub(super) fn split_hotkey_keys(keys: &[String]) -> Result<(Vec<String>, String), ToolResult> {
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
    pub(super) state: Arc<ToolState>,
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
