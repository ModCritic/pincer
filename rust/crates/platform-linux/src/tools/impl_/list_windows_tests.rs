use super::*;

#[test]
fn record_has_bounds_and_flat_legacy_fields() {
    let w = crate::x11::WindowInfo {
        xid: 42,
        pid: Some(1234),
        app_name: "example-app".to_owned(),
        title: "Example".to_owned(),
        is_on_screen: true,
        z_index: Some(3),
        x: 10,
        y: 20,
        width: 300,
        height: 400,
    };
    let rec = window_record_json(&w);

    // Canonical cross-platform shape: nested `bounds` object.
    let bounds = rec
        .get("bounds")
        .expect("record must carry a `bounds` object");
    assert_eq!(bounds["x"], json!(10));
    assert_eq!(bounds["y"], json!(20));
    assert_eq!(bounds["width"], json!(300));
    assert_eq!(bounds["height"], json!(400));

    // Legacy alias: flat fields must still be present.
    assert_eq!(rec["x"], json!(10));
    assert_eq!(rec["y"], json!(20));
    assert_eq!(rec["width"], json!(300));
    assert_eq!(rec["height"], json!(400));

    // Cross-platform companions.
    assert_eq!(rec["app_name"], json!("example-app"));
    assert_eq!(rec["is_on_screen"], json!(true));
    assert_eq!(rec["z_index"], json!(3));
    assert_eq!(rec["window_id"], json!(42));
    assert_eq!(rec["title"], json!("Example"));
}

#[test]
fn unavailable_wayland_order_serializes_as_null() {
    let w = crate::x11::WindowInfo {
        xid: 43,
        pid: Some(1234),
        app_name: "native-wayland-app".to_owned(),
        title: "Example".to_owned(),
        is_on_screen: true,
        z_index: None,
        x: 0,
        y: 0,
        width: 300,
        height: 400,
    };

    assert_eq!(window_record_json(&w)["z_index"], Value::Null);
}

fn kwrite_window(z_index: Option<i64>, is_on_screen: bool) -> crate::x11::WindowInfo {
    crate::x11::WindowInfo {
        xid: 16777223,
        pid: Some(166),
        app_name: "kwrite".to_owned(),
        title: "notes.txt  — KWrite".to_owned(),
        is_on_screen,
        z_index: z_index.map(|z| z as _),
        x: 960,
        y: 0,
        width: 640,
        height: 48,
    }
}

/// list_windows at 3eefd3e printed no z_index although its description
/// promises one per record.
#[test]
fn window_line_carries_z_index_and_off_screen_state() {
    let line = window_record_line(&window_record_json(&kwrite_window(Some(3), true)));
    assert_eq!(
        line,
        "window_id=16777223 pid=166 \"notes.txt  — KWrite\" 640x48+960+0 z=3 app=kwrite"
    );
    let hidden = window_record_line(&window_record_json(&kwrite_window(None, false)));
    assert!(
        hidden.ends_with(" z=? app=kwrite is_on_screen=false"),
        "{hidden}"
    );
}

/// The lines say every record whole, so the plain-text result carries no
/// structured copy of list_windows or list_apps (at 3eefd3e every running
/// app was printed twice).
#[test]
fn list_lines_leave_no_structured_copy() {
    let render = |text: &str, structured: &Value| {
        pincer_core::mcp_text_results::render_text(text, structured, false)
    };
    let windows: Vec<Value> = [kwrite_window(Some(0), true), kwrite_window(None, false)]
        .iter()
        .map(window_record_json)
        .collect();
    let text = windows
        .iter()
        .map(window_record_line)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(render(&text, &json!({ "windows": windows })), text);

    let apps = [
        json!({"pid": 1, "bundle_id": null, "name": "python3", "running": true, "active": false,
               "kind": null, "launch_path": null, "last_used": null, "windows": []}),
        json!({"pid": 166, "bundle_id": "org.kde.kwrite", "name": "KWrite", "running": true,
               "active": false, "kind": "desktop", "launch_path": "kwrite",
               "last_used": "2026-11-04T00:31:59Z", "windows": [windows[0].clone()]}),
        json!({"pid": 0, "bundle_id": "scrcpy", "name": "scrcpy", "running": false,
               "active": false, "kind": "desktop",
               "launch_path": "/bin/sh -c \"\\\\$SHELL -i -c scrcpy\"",
               "last_used": null, "windows": []}),
    ];
    let mut lines =
        vec!["Entries with a bundle_id come from .desktop files (kind=desktop).".to_owned()];
    for app in &apps {
        lines.push(app_line(app));
        for record in app["windows"].as_array().unwrap() {
            lines.push(format!("  {}", window_record_line(record)));
        }
    }
    let text = lines.join("\n");
    assert!(
        text.contains(" launch_path=\"/bin/sh -c \"\\\\$SHELL -i -c scrcpy\"\""),
        "{text}"
    );
    let processes = json!([{"pid": 1, "name": "python3"}, {"pid": 166, "name": "KWrite"}]);
    assert_eq!(
        render(&text, &json!({ "apps": apps, "processes": processes })),
        text
    );
    // An app whose running flag the line does not state keeps the list.
    let unstated = text.replace("pid=0 running=false", "pid=0");
    assert!(render(&unstated, &json!({ "apps": apps })).contains("\napps:\n"));
}

#[test]
fn chromium_launch_detection_uses_executable_basename() {
    assert!(chromium_family_program("/usr/bin/google-chrome-stable"));
    assert!(chromium_family_program("PincerTestHarness.Electron"));
    assert!(chromium_family_program("chromium-browser"));
    assert!(!chromium_family_program("/usr/bin/gnome-text-editor"));
}
