//! Discovery tools: list_apps, list_windows and get_accessibility_tree.

use super::*;

// ── list_apps ────────────────────────────────────────────────────────────────

pub struct ListAppsTool;
static LIST_APPS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

const LIST_APPS_APP_FILTER_DESCRIPTION: &str = "Only apps whose name contains this text \
    (case-insensitive), or whose bundle_id equals it. Without it every running process and \
    installed app is listed.";

/// Keep the list_apps records `app` selects, with list_windows' rule
/// (`app_filter_matches`): a case-insensitive substring of the record's
/// `name`, or its exact `bundle_id`. No filter keeps every record.
fn retain_apps_matching(apps: &mut Vec<Value>, filter: Option<&str>) {
    let Some(filter) = filter else {
        return;
    };
    apps.retain(|app| {
        pincer_core::tool_args::app_filter_matches(
            filter,
            app["name"].as_str().unwrap_or_default(),
            app["bundle_id"].as_str(),
        )
    });
}

#[cfg(test)]
mod list_apps_filter_tests {
    use super::*;

    fn names(apps: &[Value]) -> Vec<&str> {
        apps.iter()
            .map(|app| app["name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn app_keeps_name_substrings_and_exact_bundle_ids() {
        let all = vec![
            json!({"name": "Pincer Test Pad", "bundle_id": "pincer-test-pad", "running": true}),
            json!({"name": "KWrite", "bundle_id": "org.kde.kwrite", "running": true}),
            json!({"name": "dbus-broker", "bundle_id": null, "running": true}),
            json!({"name": "Dolphin", "bundle_id": "org.kde.dolphin", "running": false}),
        ];
        let filtered = |filter: Option<&str>| {
            let mut apps = all.clone();
            retain_apps_matching(&mut apps, filter);
            names(&apps)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(filtered(None).len(), 4);
        assert_eq!(filtered(Some("test pad")), ["Pincer Test Pad"]);
        assert_eq!(filtered(Some("KWRITE")), ["KWrite"]);
        assert_eq!(filtered(Some("org.kde.dolphin")), ["Dolphin"]);
        // A bundle id matches whole, not as a substring of the name.
        assert!(filtered(Some("org.kde")).is_empty());
        assert!(filtered(Some("firefox")).is_empty());
    }

    #[test]
    fn list_apps_advertises_the_app_filter() {
        let def = ListAppsTool.def();
        assert_eq!(
            def.input_schema["properties"]["app"]["type"],
            json!("string")
        );
        assert!(def.description.contains("`app`"), "{}", def.description);
        assert!(!def.description.contains('\u{2014}'), "{}", def.description);
    }
}

#[async_trait]
impl Tool for ListAppsTool {
    fn def(&self) -> &ToolDef {
        LIST_APPS_DEF.get_or_init(|| ToolDef {
            name: "list_apps".into(),
            description: "List apps, running and installed: every running process (from /proc, \
                daemons included) and every XDG `.desktop` entry without `NoDisplay` or \
                `Hidden`. An entry whose launcher matches a running process by basename, or \
                whose WM_CLASS one of its windows carries, is merged into one record with \
                `running: true`. Narrow the list with `app`. Each record:\n\
                - running: a process is live (pid is 0 when false).\n\
                - active: always false on Linux.\n\
                - kind: `desktop` for a `.desktop` entry.\n\
                - launch_path: the `Exec=` command without field codes; pass it to \
                `launch_app(launch_path=...)`.\n\
                - bundle_id: the XDG desktop file id (`kde4/konqbrowser.desktop` is \
                `kde4-konqbrowser`).\n\
                - last_used: RFC3339 mtime of the `.desktop` file, when readable.\n\
                - windows: the app's top-level windows, as list_windows lists them.\n\n\
                Use it for \"is X installed?\" and \"is X running?\"; for window titles and \
                geometry call list_windows."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "app":{"type":"string","description":LIST_APPS_APP_FILTER_DESCRIPTION}
            },"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let mut apps = tokio::task::spawn_blocking(|| -> Vec<serde_json::Value> {
            let procs = crate::proc_fs::list_processes();
            let installed = crate::installed_apps::list_installed_apps();
            let windows = crate::wayland::list_windows_dispatch(None);

            // An app whose launcher is not its process (`libreoffice --calc`
            // runs `soffice.bin`, `google-chrome` runs `chrome`, `gnome-terminal`
            // hands off to `gnome-terminal-server`) never matches by
            // basename. Its top-level windows still carry its WM_CLASS, so
            // owning one is what makes it running.
            let by_window: std::collections::HashMap<usize, u32> = installed
                .iter()
                .enumerate()
                .filter_map(|(i, app)| {
                    windows
                        .iter()
                        .find(|w| w.pid.is_some() && app.owns_window_class(&w.app_name))
                        .and_then(|w| w.pid)
                        .map(|pid| (i, pid))
                })
                .collect();

            // Match running processes to installed apps by executable
            // basename (Exec=firefox %u → "firefox"; cmdline /usr/bin/firefox
            // → "firefox"). Many distros register multiple .desktop entries
            // sharing the same basename (e.g. several `firefox` profiles, a
            // `code` and a `code-insiders` both shelling `code`), so the
            // bucket is `Vec<usize>` — we pick a winner per running process
            // via `disambiguate_installed_match` instead of overwriting.
            let mut by_exe: std::collections::HashMap<String, Vec<usize>> =
                std::collections::HashMap::new();
            for (i, app) in installed.iter().enumerate() {
                let basename = exec_basename(&app.launch_path);
                if !basename.is_empty() {
                    by_exe.entry(basename).or_default().push(i);
                }
            }

            let mut consumed: std::collections::HashSet<usize> = std::collections::HashSet::new();
            let mut out = Vec::new();
            for p in &procs {
                let key_source = if !p.cmdline.is_empty() {
                    &p.cmdline
                } else {
                    &p.name
                };
                let basename = exec_basename(key_source);
                if basename.is_empty() {
                    continue;
                }
                let candidates = by_exe.get(&basename).map(|v| v.as_slice()).unwrap_or(&[]);
                let merged = disambiguate_installed_match(candidates, &installed, key_source)
                    .or_else(|| {
                        by_window
                            .iter()
                            .filter(|(_, pid)| **pid == p.pid)
                            .map(|(idx, _)| *idx)
                            .min()
                    });
                if let Some(idx) = merged {
                    consumed.insert(idx);
                }
                let (name, bundle_id, launch_path, kind, last_used) = match merged {
                    Some(idx) => {
                        let a = &installed[idx];
                        (
                            a.name.clone(),
                            Some(a.bundle_id.clone()),
                            Some(a.launch_path.clone()),
                            Some("desktop".to_owned()),
                            a.last_used.clone(),
                        )
                    }
                    None => (
                        if !p.name.is_empty() {
                            p.name.clone()
                        } else {
                            basename.clone()
                        },
                        None,
                        None,
                        None,
                        None,
                    ),
                };
                let owned: Vec<Value> = windows
                    .iter()
                    .filter(|w| w.pid == Some(p.pid))
                    .filter(|w| {
                        merged.is_none_or(|idx| installed[idx].owns_window_class(&w.app_name))
                    })
                    .map(window_record_json)
                    .collect();
                out.push(json!({
                    "pid":         p.pid,
                    "bundle_id":   bundle_id,
                    "name":        name,
                    "running":     true,
                    "active":      false,
                    "kind":        kind,
                    "launch_path": launch_path,
                    "last_used":   last_used,
                    "windows":     owned,
                }));
            }
            for (i, app) in installed.iter().enumerate() {
                if consumed.contains(&i) {
                    continue;
                }
                // One process can own windows of several entries (a single
                // `soffice.bin` showing Calc and Writer): each entry with a
                // window is running under its own id.
                let window_pid = by_window.get(&i).copied();
                let owned: Vec<Value> = windows
                    .iter()
                    .filter(|w| w.pid.is_some() && w.pid == window_pid)
                    .filter(|w| app.owns_window_class(&w.app_name))
                    .map(window_record_json)
                    .collect();
                out.push(json!({
                    "pid":         window_pid.unwrap_or(0),
                    "bundle_id":   app.bundle_id.clone(),
                    "name":        app.name.clone(),
                    "running":     window_pid.is_some(),
                    "active":      false,
                    "kind":        "desktop",
                    "launch_path": app.launch_path.clone(),
                    "last_used":   app.last_used.clone(),
                    "windows":     owned,
                }));
            }
            out
        })
        .await
        .unwrap_or_default();
        let filter = args
            .get("app")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|filter| !filter.is_empty());
        retain_apps_matching(&mut apps, filter);

        let running_count = apps
            .iter()
            .filter(|a| a["running"].as_bool().unwrap_or(false))
            .count();
        let total = apps.len();
        let installed_only = total - running_count;
        let matching = filter
            .map(|filter| format!(" matching app \"{filter}\""))
            .unwrap_or_default();
        let mut header = format!(
            "✅ Found {total} app(s){matching}: {running_count} running, {installed_only} installed-not-running."
        );
        if apps.iter().any(|app| app["kind"] == "desktop") {
            header.push_str(" Entries with a bundle_id come from .desktop files (kind=desktop).");
        }
        // Every app on its own line with every field of its record, so the
        // text is the whole list: the text renderer then drops the
        // structured copy instead of printing apps twice (63 apps in a test
        // slot: 9473 bytes before, 6433 after). `active` is always false
        // here and goes unsaid.
        let mut lines = vec![header];
        for app in &apps {
            lines.push(app_line(app));
            lines.extend(
                app["windows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|record| format!("  {}", window_record_line(record))),
            );
        }
        // Unified `apps` array + legacy `processes` alias for older callers.
        let structured = json!({
            "apps": apps,
            "processes": apps.iter().filter(|a| a["running"].as_bool().unwrap_or(false))
                .map(|a| json!({
                    "pid":  a["pid"], "name": a["name"]
                })).collect::<Vec<_>>(),
        });
        ToolResult::text(lines.join("\n")).with_structured(structured)
    }
}

/// Pick which of several `.desktop`-derived installed apps best matches a
/// running process whose basename collided. Precedence:
///   1. exact full-launch-path equality with the process's cmdline token
///      (so `/usr/bin/firefox` beats `/snap/firefox/current/firefox`)
///   2. most recently modified launcher (`last_used` desc, RFC3339 string
///      ordering is lexicographic-safe for our format)
///   3. first candidate (deterministic by source order)
fn disambiguate_installed_match(
    candidates: &[usize],
    installed: &[crate::installed_apps::InstalledApp],
    proc_key_source: &str,
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }

    // Look for an exact path match against the cmdline's first token.
    let proc_path = proc_key_source.split_whitespace().next().unwrap_or("");
    if !proc_path.is_empty() {
        if let Some(&idx) = candidates
            .iter()
            .find(|&&i| installed[i].launch_path == proc_path)
        {
            return Some(idx);
        }
    }

    // Fall back to the candidate with the most recent `last_used`.
    candidates
        .iter()
        .copied()
        .max_by(|&a, &b| {
            installed[a]
                .last_used
                .as_deref()
                .unwrap_or("")
                .cmp(installed[b].last_used.as_deref().unwrap_or(""))
        })
        .or_else(|| candidates.first().copied())
}

/// Return the lowercase basename of an executable token, stripping any
/// leading shell-wrapper words and quoting. `env FOO=1 /usr/bin/firefox %U`
/// → `firefox`. `firefox %u` → `firefox`.
pub(super) fn exec_basename(s: &str) -> String {
    // Take the first whitespace-separated token that looks like a binary,
    // skipping `env`-style prefixes and `K=V` assignments.
    for tok in s.split_whitespace() {
        if tok == "env" {
            continue;
        }
        if tok.contains('=') && !tok.starts_with('/') && !tok.starts_with('-') {
            // `FOO=bar` env-var assignment — skip.
            continue;
        }
        let cleaned = tok.trim_matches(|c| c == '"' || c == '\'');
        let basename = std::path::Path::new(cleaned)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(cleaned);
        return basename.to_ascii_lowercase();
    }
    String::new()
}

// ── list_windows ─────────────────────────────────────────────────────────────

pub struct ListWindowsTool;
static LIST_WINDOWS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for ListWindowsTool {
    fn def(&self) -> &ToolDef {
        LIST_WINDOWS_DEF.get_or_init(|| ToolDef {
            name: "list_windows".into(),
            description: "List top-level windows, one record and one text line each: \
                `window_id=… pid=… \"title\" WxH+X+Y z=N app=…`, bounds in desktop pixels, \
                with `is_on_screen=false` added for a window that is not on screen. `z` is the \
                record's z_index: higher values are closer to the front; null (`z=?`) means \
                stacking order is unavailable and callers must not infer one. To select a \
                frontmost candidate, take the maximum integer z_index; if every value is null, \
                use an explicit fallback instead of relying on list order. Filter with `app` or \
                `pid`: an unfiltered call lists every window on the host.".into(),
            input_schema: json!({"type":"object","properties":{
                "pid":{"type":"integer","description":"Only list windows owned by this process ID."},
                "app":{"type":"string","description":pincer_core::tool_args::LIST_WINDOWS_APP_FILTER_DESCRIPTION},
                "on_screen_only":{"type":"boolean","description":"When true, filter to visible windows only. Default false."}
            },"additionalProperties":false}),
            read_only: true, destructive: false, idempotent: true, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        let filter_pid = args.opt_u64("pid").map(|v| v as u32);
        let on_screen_only = args.bool_or("on_screen_only", false);
        let mut windows =
            tokio::task::spawn_blocking(move || crate::wayland::list_windows_dispatch(filter_pid))
                .await
                .unwrap_or_default();
        // Exited applications can remain visible in AT-SPI/X11 as zombies;
        // never return those stale targets to callers.
        windows.retain(|window| window.pid.is_none_or(crate::proc_fs::is_process_live));
        if on_screen_only {
            windows.retain(|window| window.is_on_screen);
        }
        if let Some(filter) = args.opt_str("app") {
            windows.retain(|window| {
                pincer_core::tool_args::app_filter_matches(&filter, &window.app_name, None)
            });
        }
        if crate::wayland::is_wayland() {
            crate::wayland::remember_observed_window_origins(&windows);
        }
        let records: Vec<Value> = windows.iter().map(window_record_json).collect();
        list_windows_result(records, crate::wayland::current_xwayland_mode_list_note())
    }
}

/// The list_windows result: one line per record, then `note` (XWayland
/// mode on a Wayland session lists no native window) as the last line and
/// as a `note` field.
pub(super) fn list_windows_result(records: Vec<Value>, note: Option<&str>) -> ToolResult {
    let mut lines = vec![format!("Found {} window(s):", records.len())];
    lines.extend(
        records
            .iter()
            .map(|record| format!("  {}", window_record_line(record))),
    );
    let mut structured = json!({ "windows": records });
    if let Some(note) = note {
        lines.push(note.to_owned());
        structured["note"] = json!(note);
    }
    ToolResult::text(lines.join("\n")).with_structured(structured)
}

/// One `window_record_json` record on one text line, every field of it:
/// `window_id=… pid=… "title" WxH+X+Y z=N app=…`, plus
/// `is_on_screen=false` for a window that is not on screen. `z=?` and
/// `pid=?` stand for null. The text renderer drops the structured record
/// only when its line says all of it, so a field added to the record
/// belongs on this line too. The title and app name are app text: the title
/// is always quoted, the app name only when it is not one plain word, both
/// escaped and cut (see `quote_untrusted`); the record keeps them raw.
pub(super) fn window_record_line(record: &Value) -> String {
    let known = |value: &Value| {
        if value.is_null() {
            "?".to_owned()
        } else {
            value.to_string()
        }
    };
    let bounds = &record["bounds"];
    let mut line = format!(
        "window_id={} pid={} {} {}x{}+{}+{} z={}",
        record["window_id"],
        known(&record["pid"]),
        quoted_title(record["title"].as_str().unwrap_or_default()),
        bounds["width"],
        bounds["height"],
        bounds["x"],
        bounds["y"],
        known(&record["z_index"]),
    );
    if let Some(app) = record["app_name"].as_str().filter(|app| !app.is_empty()) {
        line.push_str(&format!(" app={}", untrusted_token(app, SHOWN_NAME_CHARS)));
    }
    if record["is_on_screen"] == false {
        line.push_str(" is_on_screen=false");
    }
    line
}

/// One list_apps record on one line, every field but the always-false
/// `active` and the windows (listed under it): `- Name pid=N running=true
/// bundle_id=… launch_path=… last_used=…`. The name and values come from
/// `.desktop` files and process names, which anyone who can write one sets:
/// each is shown bare when it is one plain word, else quoted with its own
/// quotes and backslashes escaped and its length capped (`untrusted_token`),
/// so `Name=Firefox pid=4242 running=true` stays one quoted name. An `Exec=`
/// line such as `/bin/sh -c "\\$SHELL -i -c scrcpy"` reads
/// `"/bin/sh -c \"\\\\$SHELL -i -c scrcpy\""`.
pub(super) fn app_line(app: &Value) -> String {
    let mut line = format!(
        "- {} pid={} running={}",
        untrusted_token(app["name"].as_str().unwrap_or("?"), SHOWN_NAME_CHARS),
        app["pid"],
        app["running"].as_bool().unwrap_or(false),
    );
    for (key, max_chars) in [
        ("bundle_id", SHOWN_NAME_CHARS),
        ("launch_path", SHOWN_COMMAND_CHARS),
        ("last_used", SHOWN_NAME_CHARS),
    ] {
        if let Some(value) = app[key].as_str() {
            line.push_str(&format!(" {key}={}", untrusted_token(value, max_chars)));
        }
    }
    line
}

/// Build the structured `list_windows` record for one Linux window.
///
/// Emits the canonical cross-platform shape — geometry nested under a
/// `bounds: {x,y,width,height}` object plus `app_name` / `is_on_screen`,
/// matching the macOS and Windows backends (#2017) — while KEEPING the
/// historical flat `x/y/width/height` fields inline as a legacy alias so
/// existing Linux callers don't break. Fully additive; no field removed,
/// no schema_version bump.
///
pub(super) fn window_record_json(w: &crate::x11::WindowInfo) -> Value {
    json!({
        "window_id": w.xid,
        "pid": w.pid,
        "app_name": w.app_name,
        "title": w.title,
        // Canonical cross-platform geometry (macOS/Windows parity).
        "bounds": { "x": w.x, "y": w.y, "width": w.width, "height": w.height },
        "is_on_screen": w.is_on_screen,
        "z_index": w.z_index,
        // Legacy alias: flat fields kept inline for pre-existing callers.
        "x": w.x, "y": w.y,
        "width": w.width, "height": w.height,
    })
}

// ── get_accessibility_tree ────────────────────────────────────────────────────

pub struct GetAccessibilityTreeTool;

static GAX_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetAccessibilityTreeTool {
    fn def(&self) -> &ToolDef {
        GAX_DEF.get_or_init(|| ToolDef {
            name: "get_accessibility_tree".into(),
            // The name is kept for the contract; round 2's Sonnet tester took
            // it for a window's element tree, so the first sentence says
            // what comes back and the second where the elements are.
            description: "List the running processes (name, pid) and the visible X11 windows \
                (pid, window_id, title, bounds) in one call; despite the name, no element \
                tree. For a window's elements, call get_window_state(pid, window_id)."
                .into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        })
    }
    async fn invoke(&self, _args: Value) -> ToolResult {
        let (procs, windows) = tokio::task::spawn_blocking(|| {
            (
                without_kernel_threads(crate::proc_fs::list_processes()),
                crate::x11::list_windows(None),
            )
        })
        .await
        .unwrap_or_default();

        let mut lines = vec![format!(
            "{} running process(es), {} visible window(s)",
            procs.len(),
            windows.len()
        )];
        // The name first, as the structured `processes` record has it (the
        // kernel's, cut to 15 characters), then argv[0] when it says more.
        // Printing only argv[0] left the records' names out of the text,
        // and the text renderer kept a partial copy of the list.
        lines.extend(procs.iter().map(process_line));
        if !windows.is_empty() {
            lines.push(String::new());
            lines.push("Windows:".to_owned());
            lines.extend(windows.iter().map(tree_window_line));
            lines.push(
                "→ Call get_window_state(pid, window_id) to inspect a window's UI.".to_owned(),
            );
        }

        let structured = json!({
            "processes": procs.iter().map(|p| json!({"pid":p.pid,"name":p.name})).collect::<Vec<_>>(),
            "windows": windows.iter().map(|w| json!({
                "window_id": w.xid, "pid": w.pid, "title": w.title,
                "x": w.x, "y": w.y, "width": w.width, "height": w.height
            })).collect::<Vec<_>>()
        });
        ToolResult::text(lines.join("\n")).with_structured(structured)
    }
}

/// One window of get_accessibility_tree: `- pid=N window_id=N "title"
/// WxH+X+Y`, the title escaped and quoted as in `window_record_line`.
pub(super) fn tree_window_line(w: &crate::x11::WindowInfo) -> String {
    let title = if w.title.is_empty() {
        "(no title)".to_owned()
    } else {
        quoted_title(&w.title)
    };
    format!(
        "- pid={} window_id={} {} {}x{}+{}+{}",
        w.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
        w.xid,
        title,
        w.width,
        w.height,
        w.x,
        w.y
    )
}

/// `- name (pid N)`, then argv[0] when it differs from the name
/// (`- at-spi-bus-laun (pid 15) /usr/lib/at-spi-bus-launcher`). Any process
/// sets its own name and argv[0] (argv[0] may hold newlines), so each is shown
/// bare only when it is one plain word, else escaped, quoted and cut
/// (`untrusted_token`).
pub(super) fn process_line(p: &crate::proc_fs::ProcessInfo) -> String {
    let name = untrusted_token(&p.name, SHOWN_NAME_CHARS);
    if p.cmdline.is_empty() || p.cmdline == p.name {
        format!("- {name} (pid {})", p.pid)
    } else {
        format!(
            "- {name} (pid {}) {}",
            p.pid,
            untrusted_token(&p.cmdline, SHOWN_COMMAND_CHARS)
        )
    }
}

/// Kernel threads have an empty `/proc/<pid>/cmdline` and never own a
/// window; listing them put 400+ rows ahead of three windows (2026-10-25).
fn without_kernel_threads(
    mut procs: Vec<crate::proc_fs::ProcessInfo>,
) -> Vec<crate::proc_fs::ProcessInfo> {
    procs.retain(|process| !process.cmdline.is_empty());
    procs
}

#[cfg(test)]
mod accessibility_tree_tests {
    use super::without_kernel_threads;
    use crate::proc_fs::ProcessInfo;

    #[test]
    fn kernel_threads_are_left_out() {
        let process = |pid: u32, name: &str, cmdline: &str| ProcessInfo {
            pid,
            name: name.to_owned(),
            cmdline: cmdline.to_owned(),
        };
        let kept = without_kernel_threads(vec![
            process(2, "kthreadd", ""),
            process(15, "ksoftirqd/0", ""),
            process(1040, "dolphin", "/usr/bin/dolphin"),
        ]);
        assert_eq!(kept.iter().map(|p| p.pid).collect::<Vec<_>>(), [1040]);
    }
}
