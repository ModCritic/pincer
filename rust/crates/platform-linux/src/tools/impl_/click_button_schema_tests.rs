use super::{chromium_background_must_refuse, maps_indicate_gtk, ClickTool};
use pincer_core::tool::Tool;

/// Surface 5: schema must advertise the three canonical button values and
/// describe the back-compat default. Linux already routed button=middle/right
/// pre-Surface-5; this freezes the schema shape so the contract can't drift.
#[test]
fn schema_advertises_button_enum_and_description() {
    let tool = ClickTool {
        state: super::ToolState::new(),
    };
    let d = tool.def();
    let props = d.input_schema.get("properties").expect("properties");
    let button = props.get("button").expect("button field present");
    assert_eq!(button.get("type").and_then(|v| v.as_str()), Some("string"));
    let enum_vals: Vec<&str> = button
        .get("enum")
        .and_then(|v| v.as_array())
        .expect("button.enum present")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for need in ["left", "right", "middle"] {
        assert!(enum_vals.contains(&need), "missing {need} in button.enum");
    }
    let desc = button
        .get("description")
        .and_then(|v| v.as_str())
        .expect("button.description present");
    let lc = desc.to_ascii_lowercase();
    assert!(lc.contains("left"), "description should mention default");
    assert!(
        lc.contains("wayland"),
        "description should call out wayland fallback"
    );
}

#[test]
fn chromium_background_requires_focus_free_inject_mode() {
    assert!(chromium_background_must_refuse(false, false, true));
    assert!(!chromium_background_must_refuse(false, true, true));
    assert!(!chromium_background_must_refuse(true, false, true));
    assert!(!chromium_background_must_refuse(false, false, false));
}

#[test]
fn gtk_process_maps_are_detected_without_matching_unrelated_libraries() {
    assert!(maps_indicate_gtk(
        "7f00-7f01 r-xp /usr/lib/x86_64-linux-gnu/libgtk-3.so.0.2404.32"
    ));
    assert!(maps_indicate_gtk(
        "7f00-7f01 r-xp /nix/store/hash-gtk4/lib/libgtk-4.so.1"
    ));
    assert!(!maps_indicate_gtk(
        "7f00-7f01 r-xp /usr/lib/x86_64-linux-gnu/libgdk_pixbuf-2.0.so"
    ));
}

/// A property description that tells the caller to pass `scope:"desktop"`
/// must sit in a schema that accepts `scope`: dispatch refuses unknown
/// arguments, so the advice would always fail (double_click and right_click
/// said so with no `scope` field).
#[test]
fn schemas_that_advise_desktop_scope_accept_scope() {
    let registry = super::build_registry_with_provider(false, None);
    let mut missing = Vec::new();
    for (name, def) in registry.iter_defs() {
        let Some(properties) = def.input_schema["properties"].as_object() else {
            continue;
        };
        // "Pass scope:\"desktop\"" advice, and the shared coordinate_frame
        // text that names this tool's own scope:"desktop".
        let advises_scope = properties.iter().any(|(field, property)| {
            property["description"].as_str().is_some_and(|text| {
                let text = text.to_ascii_lowercase();
                text.contains("pass scope:\"desktop\"")
                    || (field == "coordinate_frame" && text.contains("scope:\"desktop\""))
            })
        });
        if advises_scope && !properties.contains_key("scope") {
            missing.push(name.to_owned());
        }
    }
    assert!(
        missing.is_empty(),
        "these tools advise scope:\"desktop\" but do not accept scope: {missing:?}"
    );
}

#[test]
fn double_and_right_click_take_click_scope_values() {
    let registry = super::build_registry_with_provider(false, None);
    let click = &registry.get_def("click").unwrap().input_schema["properties"]["scope"];
    for name in ["double_click", "right_click"] {
        let scope = &registry.get_def(name).unwrap().input_schema["properties"]["scope"];
        assert_eq!(scope["enum"], click["enum"], "{name}");
        assert_eq!(scope["default"], click["default"], "{name}");
    }
}

#[test]
fn only_the_windowless_desktop_form_takes_the_screen_branch() {
    use serde_json::json;
    assert!(super::windowless_desktop_scope(
        &json!({"scope": "desktop", "x": 1, "y": 2})
    ));
    assert!(super::windowless_desktop_scope(
        &json!({"scope": "desktop", "x": 1, "y": 2, "pid": null})
    ));
    assert!(!super::windowless_desktop_scope(
        &json!({"scope": "desktop", "pid": 7, "x": 1, "y": 2})
    ));
    assert!(!super::windowless_desktop_scope(
        &json!({"scope": "desktop", "window_id": 9, "x": 1, "y": 2})
    ));
    assert!(!super::windowless_desktop_scope(&json!({"x": 1, "y": 2})));
}

#[test]
fn parallel_drag_is_offered_only_where_it_can_run() {
    use super::parallel_drag_unavailable_reason;
    let never = || -> Option<String> { panic!("the X11 probe must not run here") };
    // The cua-compositor socket runs it; native Wayland without it cannot.
    assert_eq!(parallel_drag_unavailable_reason(true, true, never), None);
    assert!(parallel_drag_unavailable_reason(false, true, never).is_some());
    // On X11 the real-pointer probe decides.
    assert_eq!(parallel_drag_unavailable_reason(false, false, || None), None);
    assert_eq!(
        parallel_drag_unavailable_reason(false, false, || Some("Xvfb".to_owned())),
        Some("Xvfb".to_owned())
    );
}

/// Registration follows the host probe (it used to register everywhere, so
/// every call on KDE Plasma X11, XWayland or Xvfb was a refusal).
#[test]
fn parallel_drag_registration_follows_the_host_probe() {
    let registry = super::build_registry_with_provider(false, None);
    let can_run = super::parallel_drag_unavailable_reason(
        crate::wayland::is_inject_mode(),
        crate::wayland::is_wayland(),
        crate::input::real_pointer_unavailable_reason,
    )
    .is_none();
    assert_eq!(registry.get_def("parallel_mouse_drag").is_some(), can_run);
}
