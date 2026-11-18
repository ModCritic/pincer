//! check_permissions, get_config and set_config.

use super::*;

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
    pub(super) state: Arc<ToolState>,
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
    pub(super) state: Arc<ToolState>,
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
