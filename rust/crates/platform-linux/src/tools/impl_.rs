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

fn window_target_candidates_for_pid(
    windows: impl IntoIterator<Item = crate::x11::WindowInfo>,
    pid: u32,
) -> Vec<WindowTargetCandidate> {
    windows
        .into_iter()
        .filter(|window| window.pid == Some(pid))
        .map(|window| WindowTargetCandidate {
            window_id: window.xid,
            transient_for: (!crate::wayland::is_wayland())
                .then(|| crate::x11::transient_for(window.xid))
                .flatten(),
            title: window.title,
            app_name: Some(window.app_name),
            is_on_screen: window.is_on_screen,
        })
        .collect()
}

fn pid_window_target_candidates(pid: i64) -> Vec<WindowTargetCandidate> {
    let Ok(pid) = u32::try_from(pid) else {
        return Vec::new();
    };
    window_target_candidates_for_pid(crate::wayland::list_windows_dispatch(Some(pid)), pid)
}

/// Topmost on-screen window of `pid` covering a desktop-frame point.
fn desktop_point_window_resolver() -> pincer_core::window_target::DesktopPointWindowResolver {
    Arc::new(move |pid, x, y| {
        let pid = u32::try_from(pid).ok()?;
        // Window geometry is in layout coordinates; on Hyprland the desktop
        // frame starts at the top-left powered output, not the layout origin.
        let (x, y) = if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
            let frame = crate::wayland::hyprland::desktop_frame().ok()?;
            (x + f64::from(frame.x), y + f64::from(frame.y))
        } else {
            (x, y)
        };
        topmost_window_at(&crate::wayland::list_windows_dispatch(Some(pid)), pid, x, y)
    })
}

/// The pid's topmost on-screen window containing screen point `(sx, sy)`:
/// the highest `z_index` (bottom-to-top stacking position) wins, so an
/// app's own dialog beats the main window it covers. Windows without a
/// stacking position rank lowest.
fn topmost_window_at(
    windows: &[crate::x11::WindowInfo],
    pid: u32,
    sx: f64,
    sy: f64,
) -> Option<u64> {
    windows
        .iter()
        .filter(|w| w.pid == Some(pid) && w.is_on_screen && w.width > 0 && w.height > 0)
        .filter(|w| {
            sx >= f64::from(w.x)
                && sy >= f64::from(w.y)
                && sx < f64::from(w.x) + f64::from(w.width)
                && sy < f64::from(w.y) + f64::from(w.height)
        })
        .max_by_key(|w| w.z_index.map(|z| z as i64).unwrap_or(-1))
        .map(|w| w.xid)
}

/// The window a pid-only keyboard action means in a multi-window app: the
/// pid's window that holds the core focus, else its topmost transient dialog,
/// else the WM's active window when it is the pid's, else the largest mapped
/// toplevel (see [`crate::x11::pick_pid_window`]). A dialog the app just
/// opened therefore receives the following `type_text` / `press_key` even
/// while the main window is still the WM's active window.
fn pid_fallback_window_resolver() -> pincer_core::window_target::PidFallbackWindowResolver {
    Arc::new(|pid| {
        let pid = u32::try_from(pid).ok()?;
        let windows: Vec<crate::x11::WindowInfo> = crate::wayland::list_windows_dispatch(Some(pid))
            .into_iter()
            .filter(|w| w.pid == Some(pid))
            .collect();
        if crate::wayland::is_wayland() {
            return windows
                .into_iter()
                .filter(|w| w.is_on_screen)
                .max_by_key(|w| w.z_index.unwrap_or(0))
                .map(|w| w.xid);
        }
        let ids: Vec<u64> = windows.iter().map(|w| w.xid).collect();
        crate::x11::pick_pid_window(
            &windows,
            crate::x11::focused_window_among(&ids),
            crate::x11::transient_for,
            crate::x11::active_window(),
        )
    })
}

#[cfg(test)]
mod pid_window_resolver_tests;

type PidWindowGuardParts = (
    WindowTargetCandidates,
    pincer_core::window_target::DesktopPointWindowResolver,
    pincer_core::window_target::PidFallbackWindowResolver,
    pincer_core::window_target::SnapshotWindowResolver,
    pincer_core::window_target::TokenPidResolver,
);

fn pid_window_guarded<T: Tool + 'static>(
    tool: T,
    (candidates, point_resolver, fallback_resolver, snapshot_resolver, token_pid_resolver): &PidWindowGuardParts,
) -> Box<dyn Tool> {
    Box::new(
        PidOnlyWindowTargetGuard::new(Box::new(tool), candidates.clone())
            .with_point_resolver(point_resolver.clone())
            .with_fallback_resolver(fallback_resolver.clone())
            .with_snapshot_resolver(snapshot_resolver.clone())
            .with_token_pid_resolver(token_pid_resolver.clone()),
    )
}

/// The pid an `element_token` was minted for, so a token-only action (no
/// `pid`) reaches its tool with the pid the token carries.
fn token_pid_resolver(state: Arc<ToolState>) -> pincer_core::window_target::TokenPidResolver {
    Arc::new(move |args| state.snapshots.pid_for_token(args))
}

/// The window a `snapshot_id` was published for (the element cache lane of
/// that pid), so pid-only element actions follow the snapshot to its popup
/// or dialog.
fn snapshot_window_resolver(
    state: Arc<ToolState>,
) -> pincer_core::window_target::SnapshotWindowResolver {
    Arc::new(move |pid, handle| {
        let pid = i32::try_from(pid).ok()?;
        let id = pincer_core::element_token::parse_snapshot_handle(handle)?;
        state.snapshots.window_for_snapshot(pid, id)
    })
}

// ── DriverConfig ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DriverConfig {
    pub max_image_dimension: u32,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            max_image_dimension: 1568,
        }
    }
}

/// Load `DriverConfig` from `~/.pincer/config.json`, falling back to
/// defaults for any missing/malformed keys. Called once at `ToolState`
/// construction (i.e. on every fresh `pincer call` process) so that a
/// prior `set_config max_image_dimension=...` survives across stateless
/// one-shot invocations, matching the macOS daemon's startup load. Keys are
/// read one at a time, so unknown keys in the file (such as a `capture_mode`
/// saved by an older build) are ignored.
pub fn load_driver_config() -> DriverConfig {
    let mut cfg = DriverConfig::default();
    if let Some(v) = pip_preview::read_config_value("max_image_dimension").and_then(|v| v.as_u64())
    {
        if let Ok(v32) = u32::try_from(v) {
            cfg.max_image_dimension = v32;
        }
    }
    cfg
}

use pincer_core::snapshot_store::ZoomContext;

pub struct ToolState {
    pub snapshots: Arc<Snapshots>,
    pub cursor_registry: Arc<CursorRegistry>,
    pub capture_service: Arc<pincer_core::capture_runtime::CaptureService>,
    pub mouse_hold: std::sync::Mutex<std::collections::HashMap<String, MouseHoldState>>,
    pub config: Arc<RwLock<DriverConfig>>,
}

impl ToolState {}

#[derive(Clone, Debug)]
pub struct MouseHoldState {
    pub pid: u32,
    pub xid: u64,
    pub button: u8,
    pub x: f64,
    pub y: f64,
    /// Pressed with `delivery_mode:"foreground"` on X11: a real XTest button
    /// on the core pointer, so motion and release go the same way.
    pub foreground: bool,
}

impl ToolState {
    /// Test fixture: production registries share one capture service.
    #[cfg(test)]
    pub fn new() -> Arc<Self> {
        Self::new_with_capture_service(Arc::new(
            pincer_core::capture_runtime::CaptureService::default(),
        ))
    }

    fn new_with_capture_service(
        capture_service: Arc<pincer_core::capture_runtime::CaptureService>,
    ) -> Arc<Self> {
        let snapshots = Arc::new(Snapshots::new());
        pincer_core::snapshot_store::register_runtime_store(&snapshots);
        Arc::new(Self {
            snapshots,
            cursor_registry: Arc::new(CursorRegistry::new()),
            capture_service,
            mouse_hold: std::sync::Mutex::new(Default::default()),
            config: Arc::new(RwLock::new(load_driver_config())),
        })
    }

    fn zoom_context(
        &self,
        args: &Value,
        pid: u32,
        window_id: Option<u64>,
    ) -> Result<ZoomContext, ToolResult> {
        self.snapshots.zoom(
            pid as i32,
            window_id,
            args.get("_session_id").and_then(Value::as_str),
        )
    }
}

fn screenshot_scale(
    state: &ToolState,
    args: &Value,
    pid: u32,
    window_id: Option<u64>,
) -> Result<f64, ToolResult> {
    state
        .snapshots
        .screenshot_scale(pid as i32, window_id, args)
}

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
fn exec_basename(s: &str) -> String {
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
        let mut lines = vec![format!("Found {} window(s):", records.len())];
        lines.extend(
            records
                .iter()
                .map(|record| format!("  {}", window_record_line(record))),
        );
        ToolResult::text(lines.join("\n")).with_structured(json!({ "windows": records }))
    }
}

/// One `window_record_json` record on one text line, every field of it:
/// `window_id=… pid=… "title" WxH+X+Y z=N app=…`, plus
/// `is_on_screen=false` for a window that is not on screen. `z=?` and
/// `pid=?` stand for null. The text renderer drops the structured record
/// only when its line says all of it, so a field added to the record
/// belongs on this line too. The title and app name are app text: the title
/// is always quoted, the app name only when it is not one plain word, both
/// escaped and cut (see `quote_untrusted`); the record keeps them raw.
fn window_record_line(record: &Value) -> String {
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
fn app_line(app: &Value) -> String {
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
fn window_record_json(w: &crate::x11::WindowInfo) -> Value {
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

#[cfg(test)]
mod list_windows_tests;

#[cfg(test)]
mod untrusted_text_tests;

// ── get_window_state ─────────────────────────────────────────────────────────

/// The largest window screenshot delivered as-is. The Anthropic API downsizes
/// any image above ~1.15 megapixels before the model sees it, so a 1568x861
/// PNG reached the model as ~1447x795 while the driver still mapped its x/y
/// as 1568x861: every pixel click landed a uniform 0.94x short. Capping here
/// keeps "pixels of THIS screenshot" true for the image the model reads.
const WINDOW_SCREENSHOT_MAX_PIXELS: u64 = 1_150_000;

/// The long edge a `w`x`h` image must shrink to so that it holds at most
/// `max_pixels` pixels; `None` when it already fits.
fn megapixel_long_edge_cap(w: u32, h: u32, max_pixels: u64) -> Option<u32> {
    let pixels = u64::from(w) * u64::from(h);
    if w == 0 || h == 0 || pixels <= max_pixels {
        return None;
    }
    let long = w.max(h) as f64;
    let short = w.min(h) as f64;
    let mut edge = (long * (max_pixels as f64 / pixels as f64).sqrt()).floor() as u32;
    // The resizer rounds both sides; step down until the rounded image fits.
    while edge > 1 {
        let scale = edge as f64 / long;
        let fits = ((long * scale).round() as u64) * ((short * scale).round() as u64) <= max_pixels;
        if fits {
            break;
        }
        edge -= 1;
    }
    Some(edge)
}

/// The size a `w`x`h` window frame is delivered at: the long-edge limit
/// (`max_dim`, 0 = none), then the megapixel cap unless the caller passed
/// its own `max_image_dimension`. It's worked out as sizes so the frame is
/// resampled once; resampling twice gave the same size, took about 15 ms
/// longer on a 1600x1000 window and softened text edges.
fn window_screenshot_size(w: u32, h: u32, max_dim: u32, explicit_max_dim: bool) -> (u32, u32) {
    let (w, h) = pincer_core::image_utils::fit_long_edge_dimensions(w, h, max_dim);
    if explicit_max_dim {
        return (w, h);
    }
    match megapixel_long_edge_cap(w, h, WINDOW_SCREENSHOT_MAX_PIXELS) {
        Some(edge) => pincer_core::image_utils::fit_long_edge_dimensions(w, h, edge),
        None => (w, h),
    }
}

#[cfg(test)]
mod megapixel_cap_tests;

/// Build a single structured element entry for `get_window_state`.
/// Returns `None` when the node has no `element_index` (non-actionable rows).
fn build_element_entry(
    n: &crate::atspi::AtspiNode,
    snapshot_id: Option<u32>,
    bounds: Option<(i32, i32, u32, u32)>,
) -> Option<serde_json::Value> {
    let idx = n.element_index?;
    // `label` mirrors what a human reading the markdown row would call this
    // element: its name, else its description (Qt keeps a button's tooltip
    // there: "Play"/"Pause"). Never its value: four spin buttons all labelled
    // "0.0" cannot be told apart — such a control is `unlabelled` instead,
    // with its place among its siblings in `description`.
    let unlabelled = n.name.is_none()
        && n.description
            .as_deref()
            .is_some_and(|d| d.starts_with(crate::atspi::native::UNLABELLED_NOTE_PREFIX));
    let label = n
        .name
        .clone()
        .or_else(|| n.description.clone().filter(|_| !unlabelled));
    let mut entry = json!({
        "element_index": idx,
        "role": n.role,
        "depth": n.depth,
    });
    if let Some(snapshot_id) = snapshot_id {
        entry["element_token"] = json!(pincer_core::element_token::token_for(snapshot_id, idx));
    }
    if n.in_web_content {
        entry["in_web_content"] = json!(true);
    }
    if let Some(label) = label {
        entry["label"] = json!(label);
    }
    if unlabelled {
        entry["unlabelled"] = json!(true);
    }
    // Surface the element's value separately from `label` (which is the
    // name, else the description): a field with both a name AND typed text would
    // otherwise hide the text from a caller reading the structured side,
    // leaving it only in tree_markdown. See the macOS get_window_state builder
    // for the rationale.
    if let Some(value) = n.value.clone().filter(|v| !v.is_empty()) {
        entry["value"] = json!(value);
    }
    if let Some(enabled) = n.enabled {
        entry["enabled"] = json!(enabled);
    }
    if let Some(selected) = n.selected {
        entry["selected"] = json!(selected);
    }
    let actions: Vec<String> = n
        .actions
        .iter()
        .filter(|a| !a.trim().is_empty())
        .cloned()
        .collect();
    if !actions.is_empty() {
        entry["actions"] = json!(actions);
    }
    if let Some(parent) = n.parent_element_index {
        entry["parent_index"] = json!(parent);
    }
    if let Some((x, y, w, h)) = bounds {
        entry["frame"] = json!({ "x": x, "y": y, "w": w, "h": h });
    }
    if let Some(description) = n.description.clone().filter(|d| !d.is_empty()) {
        entry["description"] = json!(description);
    }
    Some(entry)
}

/// Roles a popup's rows carry: menu entries, and the list / tree / table
/// rows of a combo list, a completer or a chooser popover.
fn popup_item_role(role: &str) -> bool {
    let role = role.trim().to_ascii_lowercase();
    role.contains("menu item")
        || role == "menu"
        || role.contains("list item")
        || role.contains("tree item")
        || role.contains("table cell")
        || role == "cell"
        || role == "item"
        || role == "option"
}

/// What a `query` read's header says about how the query was applied: that
/// its role words were searched across the window instead of walked, or
/// that it matched nothing on a walk that stopped early (`stopped` carries
/// the truncation reason), where "no match" proves nothing. `None` otherwise.
fn query_note(
    query: Option<&str>,
    searched_roles: bool,
    stopped: Option<Option<&str>>,
    tree_markdown: &str,
) -> Option<String> {
    let query = query.map(str::trim).filter(|query| !query.is_empty())?;
    if searched_roles {
        return Some(format!(
            "Searched by role: query {query:?} names roles, so AT-SPI listed every match in \
             this window without a walk (names were not searched)."
        ));
    }
    let reason = stopped?;
    tree_markdown
        .trim()
        .is_empty()
        .then(|| pincer_core::walk_budget::unsearched_query_note(query, reason))
}

/// The rows of an open popup menu out of an application-wide tree: the
/// subtree holding the item-role elements drawn inside the popup's
/// rectangle, de-indented, with the application-wide indices kept so their
/// element tokens resolve. A read of Kate's open Projects menu used to print
/// the whole main window instead (desc-test rounds 2 and 3). `None` when no
/// item of the walk lies inside the popup: Qt publishes no accessible for a
/// context menu at all (KWrite, 2026-11-03).
fn popup_tree_markdown(
    md: &str,
    nodes: &[crate::atspi::AtspiNode],
    bounds: &[(usize, i32, i32, u32, u32)],
    popup: &crate::input::PopupWindow,
    is_menu: bool,
) -> Option<(String, std::collections::HashSet<usize>)> {
    let frames: std::collections::HashMap<usize, (i32, i32, u32, u32)> = bounds
        .iter()
        .map(|(idx, x, y, w, h)| (*idx, (*x, *y, *w, *h)))
        .collect();
    // A menu popup holds menu entries only: Kate's breadcrumb list items lie
    // under the open Projects menu's rectangle but are the main window's.
    let entries: Vec<serde_json::Value> = nodes
        .iter()
        .filter(|node| !is_menu || node.role.to_ascii_lowercase().contains("menu"))
        .filter_map(|node| {
            let idx = node.element_index?;
            let (x, y, w, h) = frames.get(&idx)?;
            Some(json!({
                "element_index": idx,
                "parent_index": node.parent_element_index,
                "role": node.role,
                "frame": {"x": x, "y": y, "w": w, "h": h},
            }))
        })
        .collect();
    let (px, py) = (i64::from(popup.x), i64::from(popup.y));
    let (pw, ph) = (i64::from(popup.width), i64::from(popup.height));
    let drawn_inside = |entry: &serde_json::Value| {
        let frame = &entry["frame"];
        let x = frame["x"].as_i64().unwrap_or(i64::MIN) - px;
        let y = frame["y"].as_i64().unwrap_or(i64::MIN) - py;
        x >= -2
            && y >= -2
            && x + frame["w"].as_i64().unwrap_or(0) <= pw + 2
            && y + frame["h"].as_i64().unwrap_or(0) <= ph + 2
    };
    let kept = popup_menu_elements(entries, (popup.x, popup.y), popup.width, popup.height);
    // `popup_menu_elements` hands back its whole input when nothing matched.
    if kept.is_empty() || !kept.iter().all(drawn_inside) {
        return None;
    }
    let inside: std::collections::HashSet<usize> = kept
        .iter()
        .filter_map(|entry| entry["element_index"].as_u64())
        .map(|idx| idx as usize)
        .collect();
    let row_index = |line: &str| -> Option<usize> {
        let rest = line.trim_start().strip_prefix("- [")?;
        rest.split_once(']')?.0.parse().ok()
    };
    let indent = |line: &str| line.len() - line.trim_start().len();
    let lines: Vec<&str> = md.lines().collect();
    let first = lines
        .iter()
        .position(|line| row_index(line).is_some_and(|idx| inside.contains(&idx)))?;
    // The container the first item hangs under (`popup menu = "Kate"`).
    let item_indent = indent(lines[first]);
    let root = (0..first)
        .rev()
        .find(|&pos| indent(lines[pos]) < item_indent)
        .unwrap_or(first);
    let root_indent = indent(lines[root]);
    let mut out = String::new();
    for (pos, line) in lines.iter().enumerate().skip(root) {
        if pos > root && indent(line) <= root_indent {
            break;
        }
        // An indexed row outside the popup (the menubar item that owns the
        // menu) is not one of its items.
        if row_index(line).is_some_and(|idx| !inside.contains(&idx)) {
            continue;
        }
        out.push_str(&line[root_indent.min(indent(line))..]);
        out.push('\n');
    }
    Some((out, inside))
}

/// The entries drawn inside a popup of `width`x`height` (frames are
/// popup-local here). When an indexed container (Qt exposes the combo list
/// under the combo box inside the dialog) fills the popup, its subtree is
/// the popup's content; otherwise the item-role elements inside the box.
/// Falls back to the input when nothing matches, so a popup that is not a
/// menu (a GTK popover without its own frame) still returns what the walk
/// found.
fn popup_menu_elements(
    elements: Vec<serde_json::Value>,
    (origin_x, origin_y): (i32, i32),
    width: u32,
    height: u32,
) -> Vec<serde_json::Value> {
    // `frame` is in screen coordinates; compare it with the popup's screen
    // rectangle.
    let frame_inside = |entry: &serde_json::Value| {
        let frame = &entry["frame"];
        let x = frame["x"].as_i64().unwrap_or(i64::MIN) - i64::from(origin_x);
        let y = frame["y"].as_i64().unwrap_or(i64::MIN) - i64::from(origin_y);
        frame.is_object()
            && x >= -2
            && y >= -2
            && x + frame["w"].as_i64().unwrap_or(0) <= width as i64 + 2
            && y + frame["h"].as_i64().unwrap_or(0) <= height as i64 + 2
    };
    let role_of =
        |entry: &serde_json::Value| entry["role"].as_str().unwrap_or("").to_ascii_lowercase();
    // A container whose frame covers most of the popup: the list behind a
    // combo box / completer, the menu behind a context menu.
    let popup_area = u64::from(width) * u64::from(height);
    let container = elements
        .iter()
        .filter(|entry| frame_inside(entry))
        .filter(|entry| {
            let role = role_of(entry);
            matches!(
                role.as_str(),
                "list"
                    | "list box"
                    | "menu"
                    | "tree"
                    | "tree table"
                    | "table"
                    | "panel"
                    | "scroll pane"
                    | "filler"
            )
        })
        .filter(|entry| {
            let frame = &entry["frame"];
            let area = frame["w"].as_u64().unwrap_or(0) * frame["h"].as_u64().unwrap_or(0);
            popup_area > 0 && area * 10 >= popup_area * 6
        })
        .max_by_key(|entry| {
            let frame = &entry["frame"];
            frame["w"].as_u64().unwrap_or(0) * frame["h"].as_u64().unwrap_or(0)
        })
        .and_then(|entry| entry["element_index"].as_u64());
    if let Some(container) = container {
        let parent_of: std::collections::HashMap<u64, u64> = elements
            .iter()
            .filter_map(|entry| {
                Some((
                    entry["element_index"].as_u64()?,
                    entry["parent_index"].as_u64()?,
                ))
            })
            .collect();
        let descends = |mut idx: u64| {
            for _ in 0..64 {
                match parent_of.get(&idx) {
                    Some(parent) if *parent == container => return true,
                    Some(parent) => idx = *parent,
                    None => return false,
                }
            }
            false
        };
        let subtree: Vec<serde_json::Value> = elements
            .iter()
            .filter(|entry| {
                entry["element_index"]
                    .as_u64()
                    .is_some_and(|idx| idx != container && descends(idx))
            })
            .cloned()
            .collect();
        if !subtree.is_empty() {
            return subtree;
        }
    }
    let items: Vec<serde_json::Value> = elements
        .iter()
        .filter(|entry| popup_item_role(&role_of(entry)) && frame_inside(entry))
        .cloned()
        .collect();
    // A popup is either a menu or a list: when list / tree / table rows are
    // drawn inside it, menubar entries whose extents happen to fit the box
    // (Qt reports the main window's menubar items inside a chooser popup's
    // rectangle) are not its content.
    let rows: Vec<serde_json::Value> = items
        .iter()
        .filter(|entry| !role_of(entry).contains("menu"))
        .cloned()
        .collect();
    if !rows.is_empty() {
        rows
    } else if items.is_empty() {
        elements
    } else {
        items
    }
}

/// Elements that have a frame (visible, hit-testable on the screenshot)
/// come first, in walk order; frameless ones follow. A client that caps the
/// result then still shows the document, toolbar and sidebar controls.
fn framed_elements_first(elements: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let (framed, frameless): (Vec<_>, Vec<_>) = elements
        .into_iter()
        .partition(|entry| entry.get("frame").is_some());
    framed.into_iter().chain(frameless).collect()
}

/// Same-pid windows mapped over a target window: transient dialogs (with
/// their modality) and override-redirect popups, plus whether any of them
/// overlaps the target's rectangle (then its own drawable is not what the
/// user sees).
#[derive(Default)]
struct WindowOverlays {
    dialogs: Vec<serde_json::Value>,
    popups: Vec<serde_json::Value>,
    covers_window: bool,
    window_rect: Option<(i32, i32, u32, u32)>,
    /// The pid's window that pid-only keys go to (see
    /// `pid_fallback_window_resolver`), so only that dialog is told so.
    key_window: Option<u64>,
}

impl WindowOverlays {
    /// One sentence per overlay naming the call that acts on it. A dialog's
    /// title is the app's text inside Pincer's instruction, so it stays one
    /// escaped, quoted value (`quoted_title`): a `"` or a line break in it
    /// cannot end the quote and continue the sentence as Pincer.
    fn follow_up(&self, pid: u32) -> Option<String> {
        let mut parts = Vec::new();
        for dialog in &self.dialogs {
            let id = dialog["window_id"].as_u64().unwrap_or(0);
            let owning_pid = dialog.get("owning_pid").and_then(|v| v.as_u64());
            // A dialog of another process (a portal file dialog) is read and
            // driven with that process's pid; pid-only keys for this pid never
            // reach it.
            let (read_pid, keys) = match owning_pid {
                Some(owner) => (
                    owner,
                    format!(
                        " (act on it with pid {owner}; pid-only keys for pid {pid} do not \
                         reach it)"
                    ),
                ),
                None if self.key_window == Some(id) => {
                    (u64::from(pid), " (pid-only keys go to it)".to_owned())
                }
                None => (u64::from(pid), String::new()),
            };
            parts.push(format!(
                "dialog {} (window_id {id}, transient of window {}{}{}) is open over this \
                 window: call get_window_state(pid={read_pid}, window_id={id}) to index it and \
                 act there{keys}.",
                quoted_title(dialog["title"].as_str().unwrap_or("")),
                dialog["transient_for"].as_u64().unwrap_or(0),
                if dialog["modal"].as_bool() == Some(true) {
                    ", modal"
                } else {
                    ""
                },
                owning_pid
                    .map(|owner| format!(
                        ", owned by a different process pid {owner}, not pid {pid}"
                    ))
                    .unwrap_or_default(),
            ));
        }
        for popup in &self.popups {
            let id = popup["window_id"].as_u64().unwrap_or(0);
            parts.push(format!(
                "popup (window_id {id}, bounds x={} y={} {}x{}) is open: call \
                 get_window_state(pid={pid}, window_id={id}) to index its items and click them \
                 by element_token.",
                popup["bounds"]["x"],
                popup["bounds"]["y"],
                popup["bounds"]["width"],
                popup["bounds"]["height"]
            ));
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

fn rects_intersect(a: (i32, i32, u32, u32), b: (i32, i32, u32, u32)) -> bool {
    a.0 < b.0 + b.2 as i32
        && a.0 + a.2 as i32 > b.0
        && a.1 < b.1 + b.3 as i32
        && a.1 + a.3 as i32 > b.1
}

/// Whether `xid` is still a window `pid` may be observed through. Shared by
/// `get_window_state` and `capture_series`; `window_meta` is the listed
/// window when the caller already enumerated it.
fn window_target_matches(pid: u32, xid: u64, window_meta: Option<&crate::x11::WindowInfo>) -> bool {
    if crate::wayland::is_wayland() {
        window_meta.is_some_and(|w| w.pid == Some(pid))
            || crate::wayland::window_was_listed_for_pid(pid, xid)
    } else {
        crate::x11::window_belongs_to_pid(xid, pid)
            || crate::input::popup_window_info(xid)
                .is_some_and(|popup| popup.pid.is_none_or(|owner| owner == pid))
            // A cross-process dialog/plugin window transient-for one of
            // pid's own windows (GIMP export dialogs, LibreOffice's
            // Document Recovery) legitimately belongs to a different pid
            // than the caller's target; accept it here too so the
            // get_window_state(window_id=...) follow-up named in the
            // dialogs[] overlay actually works.
            || crate::x11::list_cross_pid_transient_windows(pid)
                .iter()
                .any(|w| w.xid == xid)
    }
}

/// The pid that owns `xid` when it is not `pid` but a dialog another process
/// opened for one of `pid`'s windows (its `WM_TRANSIENT_FOR` chain reaches
/// them); `None` for `pid`'s own windows and anything unrelated.
fn cross_pid_dialog_owner(pid: u32, xid: u64) -> Option<u32> {
    if crate::x11::window_belongs_to_pid(xid, pid) {
        return None;
    }
    crate::x11::list_cross_pid_transient_windows(pid)
        .into_iter()
        .find(|w| w.xid == xid)
        .and_then(|w| w.pid)
        .filter(|owner| *owner != pid)
}

/// Native-size image of one window. When a same-pid popup or dialog covers
/// the window, the screen rectangle is read instead, since the window's own
/// drawable never shows them. Shared by `get_window_state` and
/// `capture_series`.
fn capture_window_image(
    xid: u64,
    pid: u32,
    overlays: &WindowOverlays,
) -> anyhow::Result<pincer_core::image_utils::CapturedImage> {
    if overlays.covers_window {
        overlays
            .window_rect
            .ok_or_else(|| anyhow::anyhow!("window geometry unavailable"))
            .and_then(|(x, y, w, h)| crate::capture::screenshot_root_region_rgba(x, y, w, h))
            .map(pincer_core::image_utils::CapturedImage::Rgba)
            .or_else(|_| crate::wayland::screenshot_image_dispatch_with_pid(xid, pid))
    } else {
        crate::wayland::screenshot_image_dispatch_with_pid(xid, pid)
    }
}

/// Whether a transient window is open over the window being read: it sits
/// above it in the window manager's stacking order (`_NET_CLIENT_LIST_STACKING`,
/// bottom to top) and its `WM_TRANSIENT_FOR` chain reaches it, so it is a
/// dialog of that window (or of one of its dialogs). Every other transient of
/// the application is somewhere else: a window opened from a modal dialog
/// (the tested app's receipt viewer, message boxes and input dialog) sits on top
/// of that dialog, which is not "open over" it. A target with no stacking
/// position is a popup, which is above every managed window, or a window the
/// list does not carry; the chain alone decides for the latter.
fn dialog_is_over_target(
    target_z: Option<usize>,
    target_is_popup: bool,
    dialog_z: Option<usize>,
    chain_reaches_target: bool,
) -> bool {
    let above = match (target_z, dialog_z) {
        (Some(target), Some(dialog)) => dialog > target,
        (Some(_), None) => true,
        (None, _) => !target_is_popup,
    };
    above && chain_reaches_target
}

/// Whether `window`'s `WM_TRANSIENT_FOR` chain (bounded) reaches `target`.
fn transient_chain_reaches(
    window: u64,
    target: u64,
    transient_for: impl Fn(u64) -> Option<u64>,
) -> bool {
    let mut current = window;
    for _ in 0..8 {
        match transient_for(current) {
            Some(parent) if parent == target => return true,
            Some(parent) if parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

fn window_overlays(pid: u32, xid: u64) -> WindowOverlays {
    let mut out = WindowOverlays {
        window_rect: crate::x11::window_info(xid).map(|w| (w.x, w.y, w.width, w.height)),
        ..Default::default()
    };
    let over = |rect: (i32, i32, u32, u32)| {
        out.window_rect
            .is_some_and(|target| rects_intersect(rect, target))
    };
    let mut covers = false;
    let all_windows = crate::x11::list_windows(None);
    let target_z = all_windows
        .iter()
        .find(|w| w.xid == xid)
        .and_then(|w| w.z_index);
    let target_is_popup = target_z.is_none() && crate::input::popup_window_info(xid).is_some();
    let own: Vec<crate::x11::WindowInfo> = all_windows
        .iter()
        .filter(|w| w.pid == Some(pid))
        .cloned()
        .collect();
    // The window pid-only keys go to (the same choice the pid-only target
    // resolver makes): only that dialog is told it receives them.
    let own_ids: Vec<u64> = own.iter().map(|w| w.xid).collect();
    out.key_window = crate::x11::pick_pid_window(
        &own,
        crate::x11::focused_window_among(&own_ids),
        crate::x11::transient_for,
        crate::x11::active_window(),
    );
    for window in &own {
        if window.xid == xid || !window.is_on_screen || window.width == 0 || window.height == 0 {
            continue;
        }
        let Some(owner) = crate::x11::transient_for(window.xid) else {
            continue;
        };
        if !dialog_is_over_target(
            target_z,
            target_is_popup,
            window.z_index,
            transient_chain_reaches(window.xid, xid, crate::x11::transient_for),
        ) {
            continue;
        }
        covers |= over((window.x, window.y, window.width, window.height));
        out.dialogs.push(json!({
            "window_id": window.xid,
            "title": window.title,
            "transient_for": owner,
            "modal": crate::x11::window_is_modal(window.xid),
            "bounds": { "x": window.x, "y": window.y, "width": window.width, "height": window.height },
        }));
    }
    // A dialog/plugin window can legitimately run as a DIFFERENT process than
    // the application it belongs to (GIMP's separate-process export dialogs,
    // LibreOffice's Document Recovery dialog under a distinct soffice.bin, a
    // portal file dialog). Exact-pid matching above would make such a window
    // invisible even though it is unambiguously this application's own popup.
    // WM_TRANSIENT_FOR resolving to one of pid's own windows is the
    // correlation signal (see `list_cross_pid_transient_windows`); such
    // entries are tagged with `owning_pid` so callers can tell it apart from a
    // same-process dialog.
    for window in crate::x11::cross_pid_transient_windows_among(pid, all_windows) {
        if window.xid == xid || !window.is_on_screen || window.width == 0 || window.height == 0 {
            continue;
        }
        let Some(owner) = crate::x11::transient_for(window.xid) else {
            continue;
        };
        if !dialog_is_over_target(
            target_z,
            target_is_popup,
            window.z_index,
            transient_chain_reaches(window.xid, xid, crate::x11::transient_for),
        ) {
            continue;
        }
        covers |= over((window.x, window.y, window.width, window.height));
        out.dialogs.push(json!({
            "window_id": window.xid,
            "title": window.title,
            "transient_for": owner,
            "modal": crate::x11::window_is_modal(window.xid),
            "bounds": { "x": window.x, "y": window.y, "width": window.width, "height": window.height },
            "owning_pid": window.pid,
        }));
    }
    for popup in crate::input::mapped_popup_windows() {
        if popup.window == xid || popup.pid.is_some_and(|owner| owner != pid) {
            continue;
        }
        // Desktop-wide override-redirect windows that are nobody's menu:
        // mutter's guard window and the driver's own cursor overlay.
        if popup.pid.is_none()
            && (popup.title.contains("guard window") || popup.title.starts_with("Pincer."))
        {
            continue;
        }
        covers |= over((popup.x, popup.y, popup.width, popup.height));
        out.popups.push(popup.to_json());
    }
    out.covers_window = covers;
    out
}

pub struct GetWindowStateTool {
    state: Arc<ToolState>,
}

const COLD_START_WALK_TIMEOUT_MS: u64 = 2_000;

/// The AT-SPI walker's own node budget when the caller gives none.
const LINUX_DEFAULT_MAX_ELEMENTS: usize = 5_000;

fn linux_snapshot_timeout_ms(timeout: Option<&Value>, has_prior_snapshot: bool) -> u64 {
    pincer_core::tool_schema::resolve_timeout_ms_with_first_snapshot_grace(
        timeout,
        has_prior_snapshot,
        COLD_START_WALK_TIMEOUT_MS,
    )
}

/// Whether a get_window_state read captures a screenshot.
///
/// A full read does by default. A `since` re-read of the tree does not unless
/// `include_screenshot:true` asks for one: it reports what changed after an
/// action, and a fresh frame costs far more context than the diff. A read
/// that skips the tree is not a diff, so `since` does not change its default.
/// `screenshot_out_file` always captures, to disk.
fn window_state_captures_screenshot(
    include_screenshot: Option<bool>,
    since_read: bool,
    want_tree: bool,
    out_file: bool,
) -> bool {
    out_file || include_screenshot.unwrap_or(!(since_read && want_tree))
}

static GWS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for GetWindowStateTool {
    fn def(&self) -> &ToolDef {
        GWS_DEF.get_or_init(|| ToolDef {
            name: "get_window_state".into(),
            description: "Read a window: its AT-SPI element tree as compact Markdown, plus a \
                screenshot. Each addressable row is tagged `[N]` (a text row with `actions=[]` still \
                takes type_text and set_value); its element_token is \
                `<snapshot_id>:N`, with snapshot_id in the response header and returned in the \
                result. A row ends with the states that change when it is used, such as \
                `(checked)`, `(unchecked)`, `(pressed)`, `(expanded)` or `(selected)`. Re-read \
                with `since` to get only the changes; filter rows with `query`.\n\n\
                Screenshot: returned by default, but a `since` tree read returns none unless \
                include_screenshot:true. The tree is wrong on some surfaces, so cross-check it \
                against the screenshot: act by element_token where the tree is right, by pixel \
                x,y off this screenshot where it is not. On Wayland, when capture cannot prove \
                the surface's identity, the tree comes back without a screenshot and \
                `screenshot_error.code` is `surface_identity_unproven`. The image is at most \
                1.15 megapixels (long edge 1568 by default); element `screenshot_frame`s and \
                pointer-tool x/y are pixels of this image, `frame` stays in screen \
                coordinates, and `frame_scale` < 1 reports the downscale.\n\n\
                Limits: `timeout_ms` (when omitted, 2000 ms for a window's first read and 1000 ms \
                after), `max_elements` and `max_depth` bound the walk of the window; `query` only \
                filters its rows. A cut tree says `truncated: true`; listed elements are real, later \
                ones are missing. The focused element is not reported on Linux yet.\n\n\
                Popup menus (context menus, popovers, combo lists) never appear in list_windows. \
                A click or right_click that opens one returns `popup: {window_id, bounds, \
                title}`; pass that window_id here to get element indices for its items.".into(),
            input_schema: pincer_core::window_state_view::extend_input_schema(json!({"type":"object","required":["pid"],"properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "pid":{"type":"integer","description":"Process ID that owns the window."},
                "window_id":{"type":"integer","description":"Window id from list_windows, or a click/right_click result's `popup.window_id` to read an open menu. Omitted: the pid's open popup menu, else its focused, active or largest window."},
                "include_accessibility_tree":{"type":"boolean",
                    "description":"Default true. False skips the tree walk and returns only the screenshot and window metadata (window_bounds, app_name, window_title). Setting this and include_screenshot both false is an error."},
                "include_screenshot":{"type":"boolean",
                    "description":"Default true, except with `since`, where the default is false. True returns a grounding screenshot alongside the tree; false skips the grab and returns the tree only (the cheap path for re-indexing before an element ax action)."},
                "screenshot_out_file":{"type":"string",
                    "description":"Write the PNG to this path (~ expanded) instead of embedding it; the result gives screenshot_file_path."},
                "query":{"type":"string","description":"Case-insensitive substring: matching rows plus their ancestors, original indices kept. Filters the walked rows; does not shorten the walk. In Qt apps, role words (\"button\", \"combo box\") instead search the whole window directly, by role only."},
                "max_elements":{"type":"integer","minimum":1,"description":"Cap on AT-SPI nodes walked. Default 250 (5000 with full_output). The response says when the tree was cut; raise it to read further."},
                "max_depth":{"type":"integer","minimum":1,"description":"Cap on tree walk depth. Default uncapped."},
                "timeout_ms": pincer_core::tool_schema::timeout_ms_schema(),
                "max_image_dimension":{"type":"integer","minimum":0,"description":"Long-edge cap in pixels for this call, overriding the configured limit; 0 returns native resolution."}
            },"additionalProperties":false})),
            read_only: true, destructive: false, idempotent: false, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let display_only = match pincer_core::window_state_view::display_only(&args) {
            Ok(display_only) => display_only,
            Err(refusal) => return refusal,
        };
        use pincer_core::tool_args::ArgsExt;
        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // `window_id` omitted: the open popup menu of this pid when one is
        // mapped (a context menu the caller wants to read), else the pid's
        // focused / dialog / active / largest window.
        let xid = match args.opt_u64("window_id") {
            Some(v) => v,
            None => {
                let chosen = tokio::task::spawn_blocking(move || {
                    if let Some(popup) = crate::input::mapped_popup_windows()
                        .into_iter()
                        .rev()
                        .find(|p| p.pid == Some(pid))
                    {
                        return Some(popup.window);
                    }
                    let windows = crate::x11::list_windows(Some(pid));
                    let candidates: Vec<u64> = windows.iter().map(|w| w.xid).collect();
                    crate::x11::pick_pid_window(
                        &windows,
                        crate::x11::focused_window_among(&candidates),
                        crate::x11::transient_for,
                        crate::x11::active_window(),
                    )
                })
                .await
                .ok()
                .flatten();
                match chosen {
                    Some(v) => v,
                    None => {
                        return ToolResult::error(format!(
                            "No windows found for pid {pid}. Provide window_id."
                        ))
                    }
                }
            }
        };
        // A dialog another process opened for one of this pid's windows (a
        // portal file dialog transient for the app's dialog) lives in that
        // process's accessibility tree, so a walk as this pid could only
        // return this pid's own windows. Read it as its owner: the snapshot,
        // its element tokens and the header all name the owner's pid.
        let requested_pid = pid;
        let pid = if crate::wayland::is_wayland() {
            pid
        } else {
            tokio::task::spawn_blocking(move || cross_pid_dialog_owner(pid, xid))
                .await
                .ok()
                .flatten()
                .unwrap_or(pid)
        };
        // The per-call field replaces the configured ceiling entirely
        // (including 0 = native size).
        let max_image_dimension = match args.get("max_image_dimension") {
            None => None,
            Some(value) => match value.as_u64().and_then(|value| u32::try_from(value).ok()) {
                Some(value) => Some(value),
                None => {
                    return ToolResult::error(
                        "get_window_state.max_image_dimension must be an integer from 0 through 4294967295.",
                    )
                    .with_structured(json!({ "code": "invalid_arguments" }))
                }
            },
        };
        let explicit_max_image_dimension = max_image_dimension.is_some();
        let max_dim = {
            let cfg = self.state.config.read().unwrap();
            pincer_core::image_utils::ImageDimensionLimits {
                configured: cfg.max_image_dimension,
                max_image_dimension,
            }
            .resolve()
        };
        // get_window_state returns BOTH the AT-SPI tree and a screenshot, so the
        // agent grounds on both and cross-checks (the tree lies often enough that
        // a grounding screenshot should always be present). The modality is
        // chosen at action time: an element ax action (element_token) or element
        // px action (x,y).
        let view = match pincer_core::window_state_view::ViewOptions::from_args(&args) {
            Ok(view) => view,
            Err(refusal) => return refusal,
        };
        let query = args.opt_str("query");
        let session_id = args.opt_str("_session_id");
        // include_screenshot (default true, false on a `since` re-read): see
        // `window_state_captures_screenshot`. A screenshot_out_file still
        // forces a capture (to disk), regardless.
        let include_screenshot = args.get("include_screenshot").and_then(|v| v.as_bool());
        // `include_accessibility_tree` (default true) mirrors include_screenshot:
        // set false to SKIP the AT-SPI walk and return just the screenshot +
        // window metadata (the capture-only / preview path).
        let want_tree = args
            .get("include_accessibility_tree")
            .and_then(|v| v.as_bool())
            != Some(false);
        // screenshot_out_file: when set, write the PNG to disk and surface the
        // path instead of embedding base64 in the response. `~` expands.
        let screenshot_out_file = args.opt_str("screenshot_out_file").map(|s| {
            if let Some(rest) = s.strip_prefix("~/") {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}/{rest}")
            } else {
                s
            }
        });
        // Optional caps — when omitted, the AT-SPI walker uses its built-in
        // defaults (#22865).
        let max_elements = Some(
            view.max_elements(
                &args,
                LINUX_DEFAULT_MAX_ELEMENTS,
                display_only
                    || args
                        .get("_observation_only")
                        .and_then(|value| value.as_bool())
                        == Some(true),
            ),
        );
        let max_depth = args
            .get("max_depth")
            .and_then(|v| v.as_u64())
            .map(|v| v.max(1) as usize);
        let timeout_ms = linux_snapshot_timeout_ms(
            args.get("timeout_ms"),
            self.state
                .snapshots
                .contains_semantic_window(pid as i32, xid),
        );
        let walk_timeout = std::time::Duration::from_millis(timeout_ms);

        let process_is_live = crate::proc_fs::is_process_live(pid);
        // Enumerate the pid's windows ONCE and reuse the result for both the
        // window-ownership check (Wayland) and the additive window metadata
        // below, instead of paying for the compositor/X11 enumeration twice.
        // `window_meta` also names the surface + its on-screen rectangle on the
        // capture-only path, where no AT-SPI tree identifies it.
        let popup_meta = (!crate::wayland::is_wayland())
            .then(|| crate::input::popup_window_info(xid))
            .flatten();
        let popup_is_menu = popup_meta.is_some() && crate::input::popup_is_menu(xid);
        // A popup read prints only the popup's rows, so a small node budget
        // would only cut the walk before it reaches the open menu.
        let max_elements = match &popup_meta {
            Some(_) => max_elements.map(|n| n.max(LINUX_DEFAULT_MAX_ELEMENTS)),
            None => max_elements,
        };
        let window_meta = crate::wayland::list_windows_dispatch(Some(pid))
            .into_iter()
            .find(|w| w.xid == xid)
            .or_else(|| {
                popup_meta.as_ref().map(|popup| crate::x11::WindowInfo {
                    xid: popup.window,
                    pid: popup.pid,
                    app_name: String::new(),
                    title: popup.title.clone(),
                    is_on_screen: true,
                    z_index: None,
                    x: popup.x,
                    y: popup.y,
                    width: popup.width,
                    height: popup.height,
                })
            });
        let window_matches = window_target_matches(pid, xid, window_meta.as_ref());
        if !process_is_live || !window_matches {
            return ToolResult::error(format!(
                "Window target pid {pid}, window_id {xid} is stale or no longer running; refresh list_windows."
            ));
        }

        // A full read captures the screenshot by default so the agent grounds
        // on both and cross-checks the (sometimes-lying) tree against the
        // frame; a `since` re-read does not unless asked. An unproven Wayland
        // surface returns the tree with a typed screenshot error instead of
        // unrelated pixels.
        let should_capture = window_state_captures_screenshot(
            include_screenshot,
            view.since.is_some(),
            want_tree,
            screenshot_out_file.is_some(),
        );
        if !want_tree && !should_capture {
            return ToolResult::error(
                "Nothing to return: both include_accessibility_tree:false and \
                 include_screenshot:false. Set at least one to true, or pass \
                 screenshot_out_file to force a capture.",
            );
        }
        // `display_only` is the public form of the internal observation-only
        // read: pixels for a preview, with no snapshot or capture change.
        let observation_only = display_only
            || args
                .get("_observation_only")
                .and_then(|value| value.as_bool())
                == Some(true);
        let state = self.state.clone();
        let state_for_capture = state.clone();
        let query_for_walk = query.clone();
        // full_output returns values whole, so it reads a text field's whole
        // content instead of its first 4096 characters.
        let text_read_chars = view
            .full_output
            .then_some(crate::atspi::native::FULL_TEXT_READ_CHARS);

        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            // Skip the AT-SPI walk on the capture-only path
            // (include_accessibility_tree:false).
            let tree_result = if want_tree {
                Some(crate::atspi::walk_tree_bounded_reading(
                    pid,
                    xid,
                    query_for_walk.as_deref(),
                    max_elements,
                    max_depth,
                    text_read_chars,
                    walk_timeout,
                ))
            } else {
                None
            };
            // Bounds and element indices come from the same captured AT-SPI
            // traversal. Joining two live walks by ordinal mis-associated
            // Chromium controls when its lazy subtree changed between walks.
            let bounds = tree_result
                .as_ref()
                .map(|tree| tree.bounds.clone())
                .unwrap_or_default();
            // Capture and DELIVER the screenshot alongside the tree by default — the
            // grounding frame the agent cross-checks the tree against. With
            // screenshot_out_file set, write to disk and surface the path instead
            // of embedding base64; otherwise embed base64. Skipped only when
            // include_screenshot:false and no disk path was requested.
            // Keep the exact delivered PNG for capture publication. The action
            // binding must identify the bytes returned to the caller, including
            // any max-dimension resize, rather than the backend's raw frame.
            let mut screenshot_error = None;
            // Same-pid popups (menus, combo lists) and transient dialogs mapped
            // over this window: listed for the caller, and when one overlaps
            // the window the screenshot is taken from the screen, since the
            // window's own drawable never shows them.
            let overlays = if crate::wayland::is_wayland() {
                WindowOverlays::default()
            } else {
                window_overlays(pid, xid)
            };
            let screenshot = if should_capture {
                // A screenshot-only read with a size cap (a live preview) asks
                // a GNOME helper with API 10 to downscale in the compositor,
                // and keeps the window's full size for frame_scale.
                let preview = if !want_tree && max_dim > 0 && crate::wayland::is_wayland() {
                    crate::wayland::shell_helper::screenshot_window_preview(xid, pid, max_dim)
                } else {
                    None
                };
                let captured = if let Some((png, width, height)) = preview {
                    Ok((
                        pincer_core::image_utils::CapturedImage::Png(png),
                        Some((width, height)),
                    ))
                } else {
                    capture_window_image(xid, pid, &overlays).map(|image| (image, None))
                };
                match captured {
                    Ok((raw, original_size)) => {
                        // Resized as pixels when the capture produced them,
                        // so the PNG is encoded once, at the delivered size.
                        let (orig_w, orig_h) = match original_size {
                            Some(size) => size,
                            None => raw.dimensions()?,
                        };
                        // An explicit per-call max_image_dimension (0 = native)
                        // is authoritative; the model-safe cap applies otherwise.
                        let (raw_w, raw_h) = raw.dimensions()?;
                        let (w, h) = window_screenshot_size(
                            raw_w,
                            raw_h,
                            max_dim,
                            explicit_max_image_dimension,
                        );
                        let image = raw.resize_to(w, h)?;
                        let (png, encoded_from) = image.into_png_and_rgba()?;
                        let original_w = if w < orig_w { Some(orig_w) } else { None };
                        let (b64, file_path) = if let Some(ref path) = screenshot_out_file {
                            std::fs::write(path, &png)?;
                            (None, Some(path.clone()))
                        } else {
                            use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
                            (Some(B64.encode(&png)), None)
                        };
                        let capture_id = if observation_only {
                            None
                        } else {
                            Some(crate::capture_action_frame::publish_window(
                                &state_for_capture.capture_service,
                                &args,
                                &png,
                                encoded_from.as_ref(),
                                pid,
                                xid,
                                (w, h),
                                (orig_w, orig_h),
                            )?)
                        };
                        Some((b64, file_path, w, h, original_w, capture_id))
                    }
                    Err(error) if crate::wayland::is_surface_identity_unproven(&error) => {
                        screenshot_error = Some(error.to_string());
                        None
                    }
                    // The window passed the ownership check above and went
                    // away during the AT-SPI walk (a document closed by the
                    // action being observed): a stale target, not a capture
                    // failure.
                    Err(_) if !crate::wayland::is_wayland() && !crate::x11::window_exists(xid) => {
                        return Err(anyhow::anyhow!(
                            "Window target pid {pid}, window_id {xid} is stale or no longer running; refresh list_windows."
                        ));
                    }
                    Err(error) => {
                        return Err(anyhow::anyhow!(
                            "window screenshot failed for window {xid}: {error}"
                        ));
                    }
                }
            } else {
                None
            };
            Ok((tree_result, screenshot, bounds, screenshot_error, overlays))
        })
        .await;

        match result {
            Ok(Ok((tree_opt, shot_opt, bounds, screenshot_error, overlays))) => {
                let mut content = Vec::new();
                let mut structured = json!({ "window_id": xid, "pid": pid });
                let screenshot_scale = shot_opt.as_ref().map(|(_, _, w, _, original_w, _)| {
                    original_w.map_or(1.0, |ow| ow as f64 / *w as f64)
                });
                // The window's size at this read. A later tree-only read keeps
                // this read's screenshot frame while the size holds, and
                // retires it once the window was resized.
                let window_size = window_meta
                    .as_ref()
                    .map(|meta| (f64::from(meta.width), f64::from(meta.height)));
                let mut published_snapshot = false;
                let mut invalidated = Vec::new();

                if let Some(tr) = tree_opt {
                    let source_trusted = tr.trusted;
                    let source_degraded_reason = tr.degraded_reason.clone();
                    // A popup without an AT-SPI frame of its own: print its
                    // rows only, or say that it has none.
                    let popup_view = match (&popup_meta, tr.window_scoped) {
                        (Some(popup), false) => Some(popup_tree_markdown(
                            &tr.tree_markdown,
                            &tr.nodes,
                            &tr.bounds,
                            popup,
                            popup_is_menu,
                        )),
                        _ => None,
                    };
                    let tree_markdown = match &popup_view {
                        Some(Some((md, _))) => md.clone(),
                        Some(None) => String::new(),
                        None => tr.tree_markdown.clone(),
                    };
                    let count = match &popup_view {
                        Some(Some((_, rows))) => rows.len(),
                        Some(None) => 0,
                        None => tr
                            .nodes
                            .iter()
                            .filter(|n| n.element_index.is_some())
                            .count(),
                    };
                    let mut header = format!(
                        "window_id={xid} pid={pid} elements={count} walk_ms={}\n",
                        tr.elapsed_ms
                    );
                    if tr.truncated {
                        header.push_str(&pincer_core::walk_budget::truncation_note(
                            tr.truncation_reason.as_deref(),
                            timeout_ms,
                            tr.nodes_visited,
                            tr.nodes_pending,
                        ));
                        header.push('\n');
                    } else if !tr.bounds_complete {
                        header.push_str(
                            "⚠️ bounds phase ran out of time: some elements have no frame \
                             (element_token clicks still work; pixel targeting may not). \
                             Retry with a larger timeout_ms if you need frames.\n",
                        );
                    }
                    if let Some(note) = query_note(
                        query.as_deref(),
                        tr.searched_roles,
                        tr.truncated.then_some(tr.truncation_reason.as_deref()),
                        &tree_markdown,
                    ) {
                        header.push_str(&note);
                        header.push('\n');
                    }
                    if tr.searched_roles {
                        structured["searched_by_role"] = json!(true);
                    }
                    header.push('\n');
                    content.push(pincer_core::protocol::Content::text(
                        header + &tree_markdown,
                    ));
                    if let (Some(None), Some(popup)) = (&popup_view, &popup_meta) {
                        let note = format!(
                            "This popup's items are not in the accessibility tree (Qt publishes \
                             no accessible for a context menu). Read them in this screenshot \
                             and click one with click(pid={pid}, window_id={}, x, y) using its \
                             pixels (the result says whether the popup closed, which a click on \
                             an item does), or press Escape on the window to close the popup.",
                            popup.window
                        );
                        structured["popup_items_unavailable"] = json!(true);
                        content.push(pincer_core::protocol::Content::text(note));
                    }
                    structured["element_count"] = json!(count);
                    // `elements_complete` is a real claim now: a native walk that
                    // finished without hitting the deadline / node budget saw
                    // every node the application publishes. Truncated or
                    // fallback trees keep negative existence unknown.
                    structured["elements_complete"] = json!(tr.trusted && !tr.truncated);
                    structured["truncated"] = json!(tr.truncated);
                    if let Some(reason) = &tr.truncation_reason {
                        structured["truncation_reason"] = json!(reason);
                    }
                    structured["nodes_visited"] = json!(tr.nodes_visited);
                    structured["nodes_pending"] = json!(tr.nodes_pending);
                    structured["bounds_complete"] = json!(tr.bounds_complete);
                    structured["walk_elapsed_ms"] = json!(tr.elapsed_ms as u64);
                    structured["timeout_ms"] = json!(timeout_ms);
                    structured["tree_markdown"] = json!(tree_markdown);

                    let target_scoped = !(crate::wayland::is_wayland()
                        && crate::wayland::hyprland::is_session())
                        || tr.window_scoped;
                    if !observation_only && !target_scoped {
                        invalidated.extend(state.snapshots.remove(pid as i32, xid));
                        crate::atspi::snapshot::forget_window(pid, xid);
                    }
                    let (snapshot_id, replaced) = (!observation_only && target_scoped)
                        .then(|| {
                            state.snapshots.publish_sized_for_session(
                                pid as i32,
                                xid,
                                crate::atspi::snapshot::update_snapshot(
                                    pid, xid, &tr.nodes, &tr.bounds,
                                ),
                                session_id.as_deref(),
                                screenshot_scale,
                                window_size,
                            )
                        })
                        .flatten()
                        .unzip();
                    published_snapshot = snapshot_id.is_some();
                    invalidated.extend(replaced.into_iter().flatten());

                    // Structured `elements` array: one entry per actionable node.
                    // Shape: `{element_index, element_token, role, label,
                    // depth, actions?, parent_index?, frame?: {x,y,w,h}}`. Frame is
                    // included whenever AT-SPI Component.GetExtents(Screen)
                    // reported usable bounds; omitted otherwise (some
                    // toolkits leave bounds unset on hidden / virtual
                    // elements).
                    use std::collections::HashMap;
                    // The walk produces screen extents (what the element
                    // cache and hit-tests compare against). The published
                    // `frame` is the element's screen rectangle, as on macOS and
                    // Windows; `screenshot_frame` (added below) is the same
                    // rectangle in pixels of the screenshot in this response,
                    // the space of the pointer tools' window-local x/y. The
                    // window-local origin is the X11 window's root-relative
                    // origin, the one `window_bounds` and
                    // `coordinate_frame:"desktop"` translation use. A popup
                    // (override-redirect) window is not in the WM's client
                    // list; when its origin cannot be read, the popup's own
                    // screen rectangle is the origin.
                    let local_origin = (!crate::wayland::is_wayland())
                        .then(|| crate::atspi::native::x11_window_origin(xid))
                        .flatten()
                        .or_else(|| popup_meta.as_ref().map(|popup| (popup.x, popup.y)));
                    let bounds_by_idx: HashMap<usize, (i32, i32, u32, u32)> = bounds
                        .into_iter()
                        .map(|(i, x, y, w, h)| (i, (x, y, w, h)))
                        .collect();
                    let elements: Vec<serde_json::Value> = tr
                        .nodes
                        .iter()
                        .filter_map(|n| {
                            build_element_entry(
                                n,
                                snapshot_id,
                                bounds_by_idx.get(&n.element_index?).copied(),
                            )
                        })
                        .collect();
                    let elements = pincer_core::element_query::project_elements_for_query(
                        elements,
                        query.as_deref(),
                        &tree_markdown,
                    );
                    let elements = framed_elements_first(elements);
                    // Screenshot pixels: the capture is the window's own X11
                    // drawable (screen pixels), downsized by delivered/native.
                    let elements = match (local_origin, shot_opt.as_ref()) {
                        (Some((ox, oy)), Some((_, _, w, _, orig_w, _))) => {
                            let scale = orig_w.map_or(1.0, |ow| *w as f64 / ow as f64);
                            pincer_core::element_frame::with_screenshot_frames(
                                elements,
                                (f64::from(ox), f64::from(oy)),
                                scale,
                            )
                        }
                        _ => elements,
                    };
                    // A popup that has no AT-SPI frame of its own (LibreOffice
                    // VCL menus live under the menubar's `menu` node): return
                    // the open menu's items, the ones drawn inside the popup,
                    // instead of the whole application.
                    let elements = match &popup_view {
                        Some(Some((_, rows))) => elements
                            .into_iter()
                            .filter(|entry| {
                                entry["element_index"]
                                    .as_u64()
                                    .is_some_and(|idx| rows.contains(&(idx as usize)))
                            })
                            .collect(),
                        Some(None) => Vec::new(),
                        None => elements,
                    };
                    structured["total_element_count"] = json!(count);
                    structured["returned_element_count"] = json!(elements.len());
                    structured["elements"] = json!(elements);
                    // Surface 6: snapshot id mirror for debug correlation.
                    if let Some(snapshot_id) = snapshot_id {
                        structured["snapshot_id"] =
                            json!(pincer_core::element_token::format_snapshot_id(snapshot_id));
                    }
                    structured["_note"] = json!(
                        "Prefer `elements` — `tree_markdown` will continue to work \
                         but new fields will only be added to the structured side. \
                         Use `max_elements` / `max_depth` to bound the \
                         AT-SPI walk on apps with very large trees."
                    );
                    // Best-effort-background ladder parity with macOS/Windows: an
                    // AT-SPI walk that ran but found zero actionable elements is
                    // NOT a clean "this window has no controls" — far more often
                    // the bridge wasn't ready (toolkit-accessibility off, or the
                    // daemon isn't on the desktop session bus so the registry is
                    // empty), or it's a non-AX surface (canvas/WebGL). Mark it
                    // degraded so callers don't read `elements: []` as authoritative.
                    if !target_scoped {
                        structured["degraded"] = json!(true);
                        structured["degraded_reason"] = json!("accessibility_window_identity_unproven: tree is application-scoped; exact-window element tokens and bounds are unavailable");
                    } else if !source_trusted {
                        structured["degraded"] = json!(true);
                        structured["degraded_reason"] = json!(source_degraded_reason
                            .unwrap_or_else(|| {
                                "x11_property_fallback_partial: AT-SPI was unavailable and \
                                 Pincer only recovered window metadata. Treat it as \
                                 discovery evidence; it cannot prove checked state."
                                    .to_owned()
                            }));
                    } else if count == 0 && tr.truncated {
                        structured["degraded"] = json!(true);
                        structured["degraded_reason"] =
                            json!(pincer_core::walk_budget::truncation_note(
                                tr.truncation_reason.as_deref(),
                                timeout_ms,
                                tr.nodes_visited,
                                tr.nodes_pending,
                            ));
                    } else if count == 0 {
                        structured["degraded"] = json!(true);
                        structured["degraded_reason"] = json!(
                            "atspi_tree_empty: the AT-SPI walk returned no actionable \
                             elements. Common causes: the a11y bridge is off (enable \
                             `gsettings set org.gnome.desktop.interface \
                             toolkit-accessibility true`), the daemon is not on the \
                             desktop session bus (DBUS_SESSION_BUS_ADDRESS unreachable — \
                             run `pincer doctor`), or the window is a non-AX surface \
                             (canvas/WebGL/custom-drawn). Do not treat element data as \
                             authoritative — verify via the screenshot, and re-snapshot \
                             after enabling a11y or if the app just launched."
                        );
                        // Point the agent at the next rung explicitly: an empty
                        // tree means element_index has nothing to bind to. The
                        // recommendation is session-dependent (X11 → px,
                        // Wayland → foreground) — see non_ax_escalation.
                        structured["escalation"] = non_ax_escalation();
                    }
                }

                // A screenshot-only read adds an image but no element rows: it
                // refreshes the current snapshot's pixel frame in place and
                // keeps its tokens valid. Only a window with no element
                // snapshot yet gets a new capture-only one.
                let mut refreshed = None;
                if let (false, false, Some(scale)) =
                    (observation_only, published_snapshot, screenshot_scale)
                {
                    refreshed = state.snapshots.refresh_screenshot_for_session(
                        pid as i32,
                        xid,
                        session_id.as_deref(),
                        scale,
                        window_size,
                    );
                    if refreshed.is_none() {
                        if let Some((_, replaced)) = state.snapshots.publish_capture_for_session(
                            pid as i32,
                            xid,
                            crate::atspi::snapshot::AtspiSnapshot::from_nodes(&[]),
                            session_id.as_deref(),
                            screenshot_scale,
                        ) {
                            invalidated.extend(replaced);
                        }
                    }
                }
                if let Some(id) = refreshed {
                    let sid = pincer_core::element_token::format_snapshot_id(id);
                    content.push(pincer_core::protocol::Content::text(format!(
                        "Screenshot only: snapshot {sid} keeps its rows, and its element_tokens \
                         ({sid}:N) stay valid."
                    )));
                    structured["snapshot_id"] = json!(sid);
                    structured["screenshot_refreshed_snapshot"] = json!(true);
                }
                if !invalidated.is_empty() {
                    let ids: Vec<String> = invalidated
                        .into_iter()
                        .map(pincer_core::element_token::format_snapshot_id)
                        .collect();
                    content.push(pincer_core::protocol::Content::text(format!(
                        "Invalidated snapshots {}: their element_tokens are stale.",
                        ids.join(", ")
                    )));
                    structured["invalidated_snapshot_ids"] = json!(ids);
                }

                if let Some((b64_opt, file_path, w, h, orig_w, capture_id)) = shot_opt {
                    // ax mode + screenshot_out_file writes the PNG to disk and
                    // returns b64=None — never embed the image bytes in that case.
                    // Keep a text content part when the image went to disk so the
                    // response is never empty on the capture-only path (which has
                    // no tree markdown either).
                    if let Some(b64) = b64_opt {
                        content.push(pincer_core::protocol::Content::image_png(b64));
                    } else if let Some(fp) = &file_path {
                        content.push(pincer_core::protocol::Content::text(format!(
                            "window_id={xid} pid={pid} size={w}x{h} screenshot written to {fp}"
                        )));
                    }
                    structured["screenshot_width"] = json!(w);
                    structured["screenshot_height"] = json!(h);
                    structured["screenshot_frame_valid"] = json!(true);
                    if let Some(ow) = orig_w {
                        if ow > 0 {
                            structured["frame_scale"] = json!(w as f64 / ow as f64);
                            structured["screenshot_original_width"] = json!(ow);
                        }
                    }
                    // Surface 7: mirror the MCP image part's `mimeType` onto
                    // the structured payload so consumers don't have to sniff
                    // magic bytes off the base64 to know the format.
                    structured["screenshot_mime_type"] = json!("image/png");
                    if let Some(capture_id) = capture_id {
                        structured["capture_id"] = json!(capture_id);
                    }
                    if let Some(fp) = file_path {
                        structured["screenshot_file_path"] = json!(fp);
                    }
                }
                if let Some(reason) = &screenshot_error {
                    structured["screenshot_frame_valid"] = json!(false);
                    structured["screenshot_error"] =
                        surface_identity_unproven_error(xid, reason.clone());
                }
                // Window identity metadata (additive): app + title + on-screen
                // rectangle for the requested window_id, useful on the
                // capture-only path where no AT-SPI tree names the surface.
                if popup_meta.is_some() {
                    structured["popup"] = json!(true);
                }
                if let Some(meta) = &window_meta {
                    if !meta.app_name.is_empty() {
                        structured["app_name"] = json!(meta.app_name);
                    }
                    if !meta.title.is_empty() {
                        structured["window_title"] = json!(meta.title);
                    }
                    structured["window_bounds"] = json!({
                        "x": meta.x, "y": meta.y, "width": meta.width, "height": meta.height
                    });
                }
                // Transient dialogs and popups of this pid that are open over
                // the window, each with the call that targets it.
                if !overlays.dialogs.is_empty() {
                    structured["dialogs"] = json!(overlays.dialogs);
                }
                if !overlays.popups.is_empty() {
                    structured["popups"] = json!(overlays.popups);
                }
                if overlays.covers_window {
                    structured["screenshot_composited"] = json!(true);
                }
                if let Some(note) = overlays.follow_up(pid) {
                    structured["follow_up"] = json!(note);
                    content.push(pincer_core::protocol::Content::text(note));
                }
                if pid != requested_pid {
                    structured["requested_pid"] = json!(requested_pid);
                    content.push(pincer_core::protocol::Content::text(format!(
                        "Window {xid} belongs to pid {pid}, not pid {requested_pid}: it is a \
                         dialog pid {pid} opened for pid {requested_pid}'s window. This read and \
                         its element tokens act as pid {pid}; pass pid={pid} for keys and later \
                         reads of this window."
                    )));
                }
                structured["coordinate_frame"] = json!("window");
                structured["frame_note"] = json!(
                    "x/y for click / double_click / right_click / drag / scroll on this \
                     window are pixels of THIS screenshot (window-local, 0..screenshot_width \
                     x 0..screenshot_height), as are the element frames; the screenshot is \
                     kept at or below 1.15 megapixels so the image you read is the image these \
                     pixels index (frame_scale < 1 says the window was downsized to it). \
                     window_bounds is where it sits on the screen. Pass scope:\"desktop\" only \
                     for get_desktop_state pixels."
                );
                if display_only {
                    pincer_core::window_state_view::mark_display_only(&mut structured);
                }

                // The capture-only path (include_accessibility_tree:false) leaves
                // `content` empty when the screenshot was also unavailable — most
                // often on Wayland, where per-window capture cannot prove surface
                // identity. Return a structured error rather than a "successful"
                // response with no content parts.
                if content.is_empty() {
                    let reason_note = match &screenshot_error {
                        Some(reason) => format!(" and no screenshot could be captured ({reason})"),
                        None => " and no screenshot was returned".to_string(),
                    };
                    return ToolResult::error(format!(
                        "No content produced for window_id {xid}: the accessibility tree was \
                         skipped (include_accessibility_tree:false){reason_note}."
                    ))
                    .with_structured(structured);
                }

                if !observation_only {
                    pincer_core::window_state_view::apply(
                        &view,
                        &pincer_core::window_state_view::ViewContext {
                            pid: i64::from(pid),
                            window_id: xid,
                            key: pincer_core::window_state_view::ViewKey {
                                query: query.clone(),
                                max_elements: max_elements.unwrap_or(LINUX_DEFAULT_MAX_ELEMENTS),
                                max_depth,
                            },
                            focus_probe: None,
                        },
                        &mut content,
                        &mut structured,
                    );
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

/// One-line, model-facing explanation of a partial tree and what to do about
/// it. Shared by the text header and the structured `degraded_reason`.
fn surface_identity_unproven_error(xid: u64, reason: String) -> Value {
    json!({
        "code": "surface_identity_unproven",
        "window_id": xid,
        "reason": reason,
        "suggestion": "capture the full output explicitly, or retry on a compositor backend that supports identified per-window capture"
    })
}

// ── launch_app ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod get_window_state_actions_tests;

pub struct LaunchAppTool;
static LAUNCH_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

mod appimage;
mod launch_child;
use launch_child::{LaunchExit, LaunchedChild};

/// What one launch_app call starts, resolved once for the `process_launch`
/// attestation and again, the same way, for the launch itself.
#[derive(Debug, Clone)]
enum LaunchPlan {
    /// Exec one program.
    Program {
        /// The canonical path that is exec'd: the attested executable.
        executable: std::path::PathBuf,
        /// The launch command's words: an installed entry's Exec= (as
        /// list_apps shows it) or the caller's launch_path. The first is
        /// argv[0] as spelled.
        command: Vec<String>,
        /// The caller's `additional_arguments`, then its `urls`.
        appended: Vec<String>,
        /// The installed .desktop entry the launch resolved through.
        entry: Option<crate::installed_apps::InstalledApp>,
        /// The caller's urls, for the result's "Opened ..." note.
        urls: Vec<String>,
        /// The caller's spelling: its launch_path, bundle_id or name.
        query: String,
        options: bool,
        argument_count: usize,
    },
    /// Open targets in their default apps with xdg-open.
    Open {
        executable: std::path::PathBuf,
        targets: Vec<String>,
        /// The caller's `name` when it was itself the first target.
        named: Option<String>,
        options: bool,
    },
}

impl LaunchPlan {
    /// The resource the `process_launch` adapter admits. Deterministic for
    /// the same arguments and installed entries, so dispatch can re-prove it.
    ///
    /// xdg-open launches attest `installed_app: true` with no desktop entry:
    /// each target opens in the default app the user installed for it.
    /// A target that could run a program (an option, a .desktop file, an
    /// executable file) is attested as an option instead.
    fn scope(&self) -> Value {
        match self {
            Self::Program {
                executable,
                entry,
                options,
                argument_count,
                ..
            } => json!({
                "kind": "program_launch",
                "executable": executable.to_string_lossy(),
                "installed_app": entry.is_some(),
                "desktop_entry": entry.as_ref().map(|entry| entry.bundle_id.clone()),
                "options": options,
                "argument_count": argument_count,
            }),
            Self::Open {
                executable,
                targets,
                options,
                ..
            } => json!({
                "kind": "program_launch",
                "executable": executable.to_string_lossy(),
                "installed_app": true,
                "desktop_entry": Value::Null,
                "options": options,
                "argument_count": targets.len(),
            }),
        }
    }

    /// Every string the caller or the entry puts on the command line, for
    /// the debugger-switch guard.
    fn words(&self) -> Vec<&str> {
        match self {
            Self::Program {
                command, appended, ..
            } => command.iter().chain(appended).map(String::as_str).collect(),
            Self::Open { targets, .. } => targets.iter().map(String::as_str).collect(),
        }
    }
}

/// The installed entries a launch resolves against when a test installed a
/// fixture on this thread; `None` reads the machine's own .desktop files.
/// Read on the calling thread, before any blocking hand-off.
fn launch_installed_fixture() -> Option<Vec<crate::installed_apps::InstalledApp>> {
    #[cfg(test)]
    {
        TEST_INSTALLED_APPS.with(|apps| apps.borrow().clone())
    }
    #[cfg(not(test))]
    {
        None
    }
}

/// [`plan_launch`] against the fixture, or the installed .desktop entries.
fn plan_launch_with(
    args: &Value,
    fixture: Option<Vec<crate::installed_apps::InstalledApp>>,
) -> Result<LaunchPlan, String> {
    let installed = fixture.unwrap_or_else(crate::installed_apps::list_installed_apps);
    plan_launch(args, &installed)
}

/// Resolve a launch_app call to the exact program it starts.
///
/// `launch_path` (highest precedence) runs as written, split on spaces; it
/// counts as an installed app only when its words are exactly an entry's
/// list_apps launch_path. `bundle_id` and `name` resolve only through
/// installed .desktop entries and never run a program from PATH by name. A
/// `name` that is a URL or an existing path, or `urls` alone, open with
/// xdg-open.
fn plan_launch(
    args: &Value,
    installed: &[crate::installed_apps::InstalledApp],
) -> Result<LaunchPlan, String> {
    use pincer_core::tool_args::ArgsExt;
    let present = |key: &str| args.opt_str(key).filter(|value| !value.trim().is_empty());
    let urls = args.str_array("urls");
    let additional = args.str_array("additional_arguments");
    // list_installed_apps orders by display name only, and equal names come
    // out in hash order. Sort fully so the same call resolves the same way
    // for attestation and dispatch.
    let mut apps = installed.to_vec();
    apps.sort_by(|a, b| {
        (a.name.to_lowercase(), &a.bundle_id).cmp(&(b.name.to_lowercase(), &b.bundle_id))
    });
    let words = |command: &str| -> Vec<String> {
        command.split_whitespace().map(str::to_owned).collect()
    };

    if let Some(launch_path) = present("launch_path") {
        let command = words(&launch_path);
        let entry = apps
            .iter()
            .find(|app| app.launch_path.split_whitespace().eq(command.iter().map(String::as_str)))
            .cloned();
        return program_plan(command, entry, additional, urls, launch_path.trim().to_owned());
    }
    if let Some(bundle_id) = present("bundle_id") {
        let bundle_id = bundle_id.trim();
        let entry = apps
            .iter()
            .find(|app| app.bundle_id == bundle_id)
            .or_else(|| {
                apps.iter()
                    .find(|app| app.bundle_id.eq_ignore_ascii_case(bundle_id))
            })
            .cloned()
            .ok_or_else(|| {
                format!(
                    "bundle_id '{bundle_id}' matches no installed .desktop application; call \
                     list_apps and pass an app's bundle_id, name or launch_path"
                )
            })?;
        let command = words(&entry.launch_path);
        return program_plan(command, Some(entry), additional, urls, bundle_id.to_owned());
    }
    if let Some(name) = present("name") {
        let name = name.trim().to_owned();
        if let Some(entry) = match_installed_app(&apps, &name).cloned() {
            let command = words(&entry.launch_path);
            return program_plan(command, Some(entry), additional, urls, name);
        }
        if name.contains("://") || std::path::Path::new(&name).exists() {
            let mut targets = vec![name.clone()];
            targets.extend(urls);
            return open_plan(targets, Some(name));
        }
        return Err(format!(
            "'{name}' matches no installed .desktop application (by display name, desktop-file \
             id or Exec name), and launch_app never runs a name from PATH. Call list_apps and \
             pass an installed app's name, bundle_id or launch_path. To run another program, \
             pass its path as launch_path; standard mode refuses programs that are not \
             installed apps"
        ));
    }
    if !urls.is_empty() {
        return open_plan(urls, None);
    }
    Err("Provide at least one of: launch_path, bundle_id, name, or urls.".to_owned())
}

fn program_plan(
    command: Vec<String>,
    entry: Option<crate::installed_apps::InstalledApp>,
    additional: Vec<String>,
    urls: Vec<String>,
    query: String,
) -> Result<LaunchPlan, String> {
    let first = command
        .first()
        .ok_or_else(|| "the launch command is empty".to_owned())?;
    let executable = resolve_program(first).ok_or_else(|| match &entry {
        Some(entry) => format!(
            "installed app {} launches with {}, which is not an executable on PATH",
            quote_untrusted(&entry.name, SHOWN_NAME_CHARS),
            quote_untrusted(first, SHOWN_COMMAND_CHARS)
        ),
        None => format!("'{first}' is not an executable file or a program on PATH"),
    })?;
    let mut appended = additional;
    appended.extend(urls.iter().cloned());
    // An entry's own Exec= words are the app's; a raw launch_path's words
    // after the program are the caller's, like additional_arguments.
    let caller: Vec<&String> = if entry.is_some() {
        appended.iter().collect()
    } else {
        command[1..].iter().chain(&appended).collect()
    };
    let terminal = command.iter().any(|word| names_a_terminal(word))
        || entry.as_ref().is_some_and(|entry| {
            names_a_terminal(&entry.bundle_id)
                || entry.startup_wm_class.as_deref().is_some_and(names_a_terminal)
        });
    // A terminal runs its positional arguments as a command (kitty, foot,
    // st), so any argument to one is an option.
    let options =
        caller.iter().any(|word| word.starts_with('-')) || (terminal && !caller.is_empty());
    let argument_count = caller.len();
    Ok(LaunchPlan::Program {
        executable,
        command,
        appended,
        entry,
        urls,
        query,
        options,
        argument_count,
    })
}

fn open_plan(targets: Vec<String>, named: Option<String>) -> Result<LaunchPlan, String> {
    let executable = resolve_program("xdg-open")
        .ok_or_else(|| "xdg-open is not installed, so files and URLs cannot be opened".to_owned())?;
    let options = targets
        .iter()
        .any(|target| target.starts_with('-') || target_can_run_a_program(target));
    Ok(LaunchPlan::Open {
        executable,
        targets,
        named,
        options,
    })
}

/// The canonical path of the program a launch command's first word names:
/// a path (absolute, or relative to the driver's directory) or a name looked
/// up on PATH, executable either way.
fn resolve_program(word: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let runnable = |path: &std::path::Path| -> Option<std::path::PathBuf> {
        let metadata = std::fs::metadata(path).ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .then(|| std::fs::canonicalize(path).ok())
            .flatten()
    };
    if word.contains('/') {
        return runnable(std::path::Path::new(word));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| runnable(&dir.join(word)))
}

/// Terminal emulators that `crate::terminal`'s lists miss. Several run
/// their positional arguments as a command (foot, st, kgx).
const LAUNCH_EXTRA_TERMINALS: &[&str] = &[
    "contour",
    "cool-retro-term",
    "foot",
    "footclient",
    "guake",
    "kgx",
    "ptyxis",
    "rio",
    "rxvt",
    "sakura",
    "st",
    "terminator",
    "terminology",
    "tilda",
    "urxvt",
    "urxvtc",
    "x-terminal-emulator",
    "xdg-terminal-exec",
    "yakuake",
];

/// Whether a command word, desktop-file id or window class names a terminal
/// emulator, by its file name or its app id's last part.
fn names_a_terminal(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    let base = std::path::Path::new(&lower)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&lower);
    let is_terminal = |name: &str| {
        crate::terminal::is_terminal_process_name(name)
            || crate::terminal::wm_class_matches_terminal(name, "")
            || LAUNCH_EXTRA_TERMINALS.contains(&name)
    };
    is_terminal(base) || app_id_last(base).is_some_and(is_terminal)
}

/// Whether xdg-open could run a program for this target: a local .desktop
/// file (it may launch its Exec= line) or an executable file.
fn target_can_run_a_program(target: &str) -> bool {
    let local = if target.contains("://") {
        match url::Url::parse(target) {
            Ok(url) if url.scheme() == "file" => match url.to_file_path() {
                Ok(path) => path,
                Err(()) => return true,
            },
            _ => return false,
        }
    } else {
        std::path::PathBuf::from(target)
    };
    if local
        .to_string_lossy()
        .to_ascii_lowercase()
        .ends_with(".desktop")
    {
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(&local)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Remove every PINCER_* variable from a launched program's environment:
/// the permission mode and approval bypass, capability manifest and policy
/// files, launch grants and tokens. A Pincer started inside the app (from a
/// terminal, say) would otherwise run with the daemon's authority.
fn scrub_driver_environment(command: &mut std::process::Command) {
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"PINCER_") {
            command.env_remove(key);
        }
    }
}

/// Spawn a launch in the background: exec `executable`, the attested
/// program, with `argv` (argv[0] kept as the command spelled it, so a
/// multi-call binary behind a symlink still sees its name). The child's
/// exit and the end of its output are recorded for the launch result.
fn spawn_launch(executable: &std::path::Path, argv: &[String]) -> std::io::Result<LaunchedChild> {
    use std::os::unix::process::CommandExt;
    let mut words = argv.to_vec();
    if words.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty launch command",
        ));
    }
    // A type-2 AppImage that can't mount with FUSE here (root unmapped in a
    // user namespace) unpacks and runs instead of failing with exit 127. It
    // unpacks into a folder of its own on disk, which the reaper deletes once
    // the app has exited, unless the command sets its own TMPDIR.
    let mut run_dir = None;
    let extract_and_run = appimage::extract_and_run_needed(&words).map(|(at, reason)| {
        if !appimage::command_sets_tmpdir(&words, at) {
            if let Some(root) = appimage::run_root() {
                appimage::sweep_run_dirs(&root);
                match appimage::RunDir::create(&root) {
                    Ok(dir) => run_dir = Some(dir),
                    Err(error) => {
                        tracing::debug!("no AppImage run folder under {}: {error}", root.display())
                    }
                }
            }
        }
        words.insert(at + 1, appimage::EXTRACT_AND_RUN_FLAG.to_owned());
        appimage::ExtractAndRun {
            reason,
            run_dir: run_dir.as_ref().map(|dir| dir.path().to_path_buf()),
        }
    });
    let prog = words[0].clone();
    let rest = &words[1..];
    let mut launch = std::process::Command::new(executable);
    launch
        .arg0(&prog)
        .args(rest)
        // The child must not inherit the driver's stdio: in private-worker mode
        // stdout is the JSON-RPC stream to the SDK, and a chatty app (Chromium's
        // zygote logs, GTK warnings) writing there corrupts a response and shuts
        // the worker down. Its stdout and stderr go to a capture
        // (`spawn_watched`) instead.
        .stdin(std::process::Stdio::null())
        // Enable accessibility for this child without toggling GNOME's global
        // ScreenReaderEnabled setting (which can launch Orca). Native
        // toolkits ignore these when they do not need them.
        .env("ACCESSIBILITY_ENABLED", "1")
        .env("NO_AT_BRIDGE", "0");
    scrub_driver_environment(&mut launch);
    if chromium_family_program(&prog)
        && !rest
            .iter()
            .any(|arg| arg == "--force-renderer-accessibility")
    {
        launch.arg("--force-renderer-accessibility");
    }
    if let Some(dir) = &run_dir {
        launch.env("TMPDIR", dir.path());
    }
    if let Some(id) = background_startup_id() {
        launch.env("DESKTOP_STARTUP_ID", id);
    }
    let mut child = launch_child::spawn_watched(launch, run_dir)?;
    child.extract_and_run = extract_and_run;
    Ok(child)
}

/// A startup-notification id (`<unique>_TIME<timestamp>`) that asks the
/// window manager to leave a launched window inactive, or `None` outside a
/// plain X11 session or when the X server's clock cannot be read.
///
/// EWMH focus-stealing prevention activates a new window only when its
/// `_NET_WM_USER_TIME` is newer than the user's last input in the active
/// window. Assumption, checked in a KWin X11 slot at bfc42c0: GTK3 copies the
/// `_TIME` suffix into `_NET_WM_USER_TIME` (the Test Pad got 3331800000 and
/// stayed inactive and below, where the same launch without the variable
/// was activated and raised), while Qt 6.12 ignores it and stamps a fresh
/// server time of its own (KWrite was activated with the variable set), so
/// this keeps only some apps in the background. Not tried on Wayland or
/// XWayland, so not sent there.
fn background_startup_id() -> Option<String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() || crate::x11::server_is_xwayland() {
        return None;
    }
    use x11rb::connection::Connection as _;
    let (conn, screen) = x11rb::rust_connection::RustConnection::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let now = crate::input::server_time(&conn, root)?;
    static LAUNCHES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let launch = LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(format!(
        "pincer-launch-{}-{launch}_TIME{}",
        std::process::id(),
        background_user_time(now)
    ))
}

/// A user time older than any input in the last 2^30 ms (12.4 days) before
/// `now`. The X clock is 32-bit and window managers compare its times
/// modulo 2^32 (older when `a - b` wraps past 2^31), so this stays "in the
/// past" however long the server has run, where a fixed `_TIME1` turns into
/// "newer" after 24.8 days of server uptime. 0 is avoided: GTK reads it as
/// no time at all.
fn background_user_time(now: u32) -> u32 {
    now.wrapping_sub(1 << 30).max(1)
}

/// Re-read the launched windows (`list` lists every window) until their
/// titles have not changed for `quiet`, at most `limit`. An app often maps
/// its window with a default title and retitles it right after: KWrite
/// showed "KWrite", then the file name before "KWrite" 15 ms later, and the
/// launch result at bfc42c0 could carry the first. Windows that close meanwhile
/// are dropped; ones that open are not added.
fn settle_window_titles(
    mut windows: Vec<crate::x11::WindowInfo>,
    mut list: impl FnMut() -> Vec<crate::x11::WindowInfo>,
    quiet: std::time::Duration,
    limit: std::time::Duration,
) -> Vec<crate::x11::WindowInfo> {
    let started = std::time::Instant::now();
    let mut unchanged_since = started;
    while !windows.is_empty() && unchanged_since.elapsed() < quiet && started.elapsed() < limit {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let all = list();
        let fresh: Vec<_> = windows
            .iter()
            .filter_map(|old| all.iter().find(|new| new.xid == old.xid).cloned())
            .collect();
        let titles = |list: &[crate::x11::WindowInfo]| -> Vec<(u64, String)> {
            list.iter().map(|w| (w.xid, w.title.clone())).collect()
        };
        if titles(&fresh) != titles(&windows) {
            unchanged_since = std::time::Instant::now();
        }
        windows = fresh;
    }
    windows
}

/// How long launched windows' titles must stay the same, and the most
/// [`settle_window_titles`] waits for that.
const LAUNCH_TITLE_QUIET: std::time::Duration = std::time::Duration::from_millis(200);
const LAUNCH_TITLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The note a launch that unpacked an AppImage carries: why, where the copy
/// goes and what it costs. In a driver-owned folder the copy is deleted
/// after the app exits; elsewhere the runtime is left to it, and the
/// tested AppImage's runtime left its 547 MB copy behind.
fn appimage_extract_note(extract: &appimage::ExtractAndRun) -> String {
    let flag = appimage::EXTRACT_AND_RUN_FLAG;
    let reason = &extract.reason;
    match &extract.run_dir {
        Some(dir) => format!(
            "Started with {flag}: FUSE can't mount AppImages here ({reason}), so it unpacks \
             itself into {} (on disk; also the app's TMPDIR) instead of mounting, which costs \
             time and disk on every start. Pincer deletes that folder when the app exits.",
            dir.display()
        ),
        None => format!(
            "Started with {flag}: FUSE can't mount AppImages here ({reason}), so it unpacks \
             itself to $TMPDIR (default /tmp) instead of mounting, which costs time and disk \
             (RAM when /tmp is tmpfs) on every start and may leave the unpacked copy behind; \
             Pincer does not clean that up."
        ),
    }
}

/// [`appimage_extract_note`] on a launch result, text and structured.
fn with_appimage_note(
    mut result: ToolResult,
    extract: Option<&appimage::ExtractAndRun>,
) -> ToolResult {
    let Some(extract) = extract else {
        return result;
    };
    let note = appimage_extract_note(extract);
    result
        .content
        .push(pincer_core::protocol::Content::text(note));
    if let Some(structured) = result.structured_content.as_mut() {
        structured["appimage"] = json!({
            "mode": "extract_and_run",
            "reason": extract.reason,
            "run_dir": extract.run_dir,
            "removed_after_exit": extract.run_dir.is_some(),
        });
    }
    result
}

/// Match a launch_app `name` that failed direct exec against installed XDG
/// .desktop applications: exact display name, desktop-file id, or `Exec=`
/// basename first, then a display-name substring when it is unambiguous.
fn match_installed_app<'a>(
    apps: &'a [crate::installed_apps::InstalledApp],
    query: &str,
) -> Option<&'a crate::installed_apps::InstalledApp> {
    let q = query.to_ascii_lowercase();
    if q.is_empty() {
        return None;
    }
    apps.iter()
        .find(|a| {
            a.name.to_ascii_lowercase() == q
                || a.bundle_id.to_ascii_lowercase() == q
                || exec_basename(&a.launch_path) == q
        })
        .or_else(|| {
            let mut matches = apps
                .iter()
                .filter(|a| a.name.to_ascii_lowercase().contains(&q));
            match (matches.next(), matches.next()) {
                (Some(only), None) => Some(only),
                _ => None,
            }
        })
}

/// Watch a just-spawned `xdg-open` long enough to catch a fast failure.
/// xdg-open's generic fallback can `exec` the target app and stay alive for
/// its whole lifetime, so a child still running after the grace period counts
/// as success; a quick non-zero exit (2 = file not found, 3 = no handler
/// tool, 4 = action failed) is the only reliable failure signal. The reaper
/// of [`launch_child::spawn_watched`] keeps watching a child still running.
fn xdg_open_failure(child: &LaunchedChild) -> Option<String> {
    match child.exit_within(std::time::Duration::from_millis(1500)) {
        Some(exit) if exit.status.success() => None,
        Some(exit) => {
            let tail = child.output_tail();
            Some(if tail.is_empty() {
                format!("xdg-open {}", exit.describe())
            } else {
                format!(
                    "xdg-open {}. The end of its output (program text):\n{}",
                    exit.describe(),
                    fenced_output(&tail)
                )
            })
        }
        None => None,
    }
}

/// Open each target with `xdg-open` (the attested `executable`) in its
/// default app, all started before any is watched, and refuse on the first
/// fast failure. Like a launched app, xdg-open gets no stdin, a private
/// capture for stdout and stderr (in private-worker mode the driver's stdout
/// is the JSON-RPC stream), and no PINCER_* variables.
fn open_with_xdg(executable: &std::path::Path, targets: &[String]) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut children = Vec::new();
    for target in targets {
        let mut command = std::process::Command::new(executable);
        command
            .arg0("xdg-open")
            .arg(target)
            .stdin(std::process::Stdio::null());
        scrub_driver_environment(&mut command);
        children.push((target, launch_child::spawn_watched(command, None)?));
    }
    for (target, child) in children {
        if let Some(reason) = xdg_open_failure(&child) {
            anyhow::bail!("could not open '{target}': {reason}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod launch_app_tests;
#[cfg(test)]
mod launch_scope_tests;

// Test-only stand-in for the installed .desktop entries a launch resolves
// against, per thread, so launch tests do not depend on the apps installed on
// the machine that runs them.
#[cfg(test)]
thread_local! {
    static TEST_INSTALLED_APPS: std::cell::RefCell<Option<Vec<crate::installed_apps::InstalledApp>>> =
        const { std::cell::RefCell::new(None) };
}

#[async_trait]
impl Tool for LaunchAppTool {
    fn def(&self) -> &ToolDef {
        LAUNCH_DEF.get_or_init(|| ToolDef {
            name: "launch_app".into(),
            // Activation and title behavior measured in a KWin X11 slot at
            // bfc42c0; see background_startup_id and settle_window_titles.
            description: "Start a Linux app and return its pid and windows, titles read once \
                they stop changing (up to 1 s). The window manager may activate and raise the \
                new window (KWin does for Qt and KDE apps such as KWrite and Dolphin; GTK apps \
                stay behind an already active window); `active` says whether it did. Name the app with launch_path or \
                name. To open files or URLs in that app, pass them in `urls` (or \
                `additional_arguments`): they are appended to its command line, for example \
                {name:\"KWrite\", urls:[\"/path/notes.txt\"]}. `urls` without launch_path or \
                name go to xdg-open, which opens each in its default app and returns no pid. \
                Standard mode starts installed apps only (by name, bundle_id or their list_apps \
                launch_path), with no argument or url starting with '-' and no arguments at all \
                for a terminal; other programs need unrestricted mode or a capability manifest. \
                Refused when the name matches nothing launchable."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "launch_path":{"type":"string","description":"Round-trip the `launch_path` returned by `list_apps`: the Exec= command from the .desktop file with XDG field codes already stripped. Highest precedence on Linux; split on spaces and run directly, without a shell. Any other command line runs a program that is not an installed app, which standard mode refuses."},
                "name":{"type":"string","description":"Installed app to launch, matched against installed .desktop applications (exact display name, desktop-file id, or Exec basename; else an unambiguous display-name substring). Never runs a program from PATH: pass a path as launch_path for that. A URL or existing file path here is opened with xdg-open."},
                "bundle_id":{"type":"string","description":"Desktop-file id of an installed app, as list_apps returns it (e.g. org.kde.kwrite). Used when launch_path is absent."},
                "urls":{"type":"array","items":{"type":"string"},"description":"Files or URLs to open. With launch_path or name, appended to that app's arguments (after additional_arguments); alone, each is opened with xdg-open in its default app."},
                "additional_arguments":{"type":"array","items":{"type":"string"},"description":"Extra command-line arguments passed to the launched process."}
            },"additionalProperties":false}),
            read_only: false, destructive: false, idempotent: false, open_world: true,
        })
    }

    /// The exact program this call starts, for the `process_launch` adapter.
    async fn protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        if adapter_id != "process_launch" {
            return Ok(None);
        }
        let args = args.clone();
        let fixture = launch_installed_fixture();
        tokio::task::spawn_blocking(move || {
            plan_launch_with(&args, fixture).map(|plan| Some(plan.scope()))
        })
        .await
        .map_err(|error| format!("launch resolution failed: {error}"))?
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        let launch_path_opt = args.opt_str("launch_path");
        let name_opt = args.opt_str("name");
        let urls: Vec<String> = args.str_array("urls");
        let additional_arguments: Vec<String> = args.str_array("additional_arguments");
        if args.get("cdp_debugging_port").is_some() {
            return ToolResult::error(
                "cdp_debugging_port moved to browser_prepare so DevTools is never enabled on an unproven user profile",
            );
        }
        // `urls` join the checked arguments: with an app named, they are
        // appended to its command line below.
        if launch_path_opt
            .as_deref()
            .into_iter()
            .chain(name_opt.as_deref())
            .chain(additional_arguments.iter().map(String::as_str))
            .chain(urls.iter().map(String::as_str))
            .any(pincer_core::launch_guard::contains_remote_debugging_flag)
        {
            return ToolResult::error(pincer_core::launch_guard::REMOTE_DEBUGGING_LAUNCH_REFUSAL);
        }

        // Resolve the exact program, the same way the attestation did. The
        // registry passes the scope it admitted as `_protected_launch_scope`
        // (callers cannot set reserved fields); the launch runs only that.
        let approved = args
            .get("_protected_launch_scope")
            .filter(|scope| !scope.is_null())
            .cloned();
        let fixture = launch_installed_fixture();
        let plan_args = args.clone();
        let plan = match tokio::task::spawn_blocking(move || plan_launch_with(&plan_args, fixture))
            .await
        {
            Ok(Ok(plan)) => plan,
            Ok(Err(message)) => return ToolResult::error(format!("Failed to launch: {message}")),
            Err(error) => return ToolResult::error(format!("Task error: {error}")),
        };
        if let Some(approved) = approved {
            if plan.scope() != approved {
                return ToolResult::error(
                    "Failed to launch: the program this launch_app call resolves to changed \
                     after it was approved; call launch_app again",
                )
                .with_structured(json!({
                    "code": "launch_target_changed",
                    "effect": "none",
                }));
            }
        }
        // An installed entry's own Exec= words get the same guard as the
        // caller's arguments.
        if plan
            .words()
            .into_iter()
            .any(pincer_core::launch_guard::contains_remote_debugging_flag)
        {
            return ToolResult::error(pincer_core::launch_guard::REMOTE_DEBUGGING_LAUNCH_REFUSAL);
        }

        // Snapshot the top-levels before spawning so a window created by an
        // already-running service (D-Bus activation) can be told apart.
        let windows_before: std::collections::HashSet<u64> =
            tokio::task::spawn_blocking(|| crate::wayland::list_windows_dispatch(None))
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|w| w.xid)
                .collect();
        let result = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(String, Option<LaunchedChild>, String)> {
                match plan {
                    LaunchPlan::Open {
                        executable,
                        targets,
                        named,
                        ..
                    } => {
                        open_with_xdg(&executable, &targets)?;
                        // xdg-open may spawn a helper and exit, so do not
                        // claim its pid is the app pid.
                        Ok(match named {
                            None => (
                                format!("Opened {} URL(s) via xdg-open.", targets.len()),
                                None,
                                "xdg-open".to_owned(),
                            ),
                            Some(name) => (
                                match targets.len() - 1 {
                                    0 => format!("Opened '{name}' via xdg-open."),
                                    n => format!("Opened '{name}' and {n} URL(s) via xdg-open."),
                                },
                                None,
                                name,
                            ),
                        })
                    }
                    LaunchPlan::Program {
                        executable,
                        command,
                        appended,
                        entry,
                        urls,
                        query,
                        ..
                    } => {
                        // With an app named, `urls` are its open targets, as
                        // on macOS. Assumption: appending them after
                        // `additional_arguments` matches where a .desktop
                        // Exec= line's trailing %U/%F field code puts them;
                        // list_apps strips those codes from launch_path.
                        let opened_note = match urls.len() {
                            0 => String::new(),
                            1 => format!(" Opened {} in it.", urls[0]),
                            n => format!(" Opened {n} files or URLs in it."),
                        };
                        let mut argv = command.clone();
                        argv.extend(appended);
                        let child = spawn_launch(&executable, &argv).map_err(|error| match &entry {
                            Some(app) => anyhow::anyhow!(
                                "'{query}' matched installed app {} but its launcher {} failed \
                                 to start: {error}",
                                quote_untrusted(&app.name, SHOWN_NAME_CHARS),
                                quote_untrusted(&app.launch_path, SHOWN_COMMAND_CHARS)
                            ),
                            None => anyhow::anyhow!("'{query}' failed to start: {error}"),
                        })?;
                        Ok(match entry {
                            Some(app) => {
                                // Match keys for the window: display name, the
                                // caller's spelling, StartupWMClass, and last
                                // the command that ran.
                                let mut keys = vec![app.name.clone(), query.clone()];
                                keys.extend(app.startup_wm_class.clone());
                                keys.push(app.launch_path.clone());
                                (
                                    format!(
                                        "✅ Launched {} ({}, pid {}), resolved from '{query}' \
                                         by its desktop entry.{opened_note}",
                                        quote_untrusted(&app.name, SHOWN_NAME_CHARS),
                                        quote_untrusted(&app.launch_path, SHOWN_COMMAND_CHARS),
                                        child.pid
                                    ),
                                    Some(child),
                                    keys.join("\u{0}"),
                                )
                            }
                            None => (
                                format!("✅ Launched {query} (pid {}).{opened_note}", child.pid),
                                Some(child),
                                query,
                            ),
                        })
                    }
                }
            },
        )
        .await;

        match result {
            Ok(Ok((message, child_opt, name))) => {
                if let Some(child) = child_opt {
                    let launcher_pid = child.pid;
                    // `name` may carry NUL-separated match keys (display name,
                    // query, Exec=) for a desktop-entry launch. The last key
                    // is the command that actually ran.
                    let query = name.clone();
                    let program = launch_child::launch_program(
                        query.rsplit('\u{0}').next().unwrap_or(&query),
                    );
                    let name = name.split('\u{0}').next().unwrap_or("").to_owned();
                    let watched = child.clone();
                    let program_w = program.clone();
                    let (resolved, active) = tokio::task::spawn_blocking(move || {
                        let mut resolved =
                            resolve_launched_windows(&watched, &query, &program_w, &windows_before);
                        resolved.windows = settle_window_titles(
                            std::mem::take(&mut resolved.windows),
                            || crate::wayland::list_windows_dispatch(None),
                            LAUNCH_TITLE_QUIET,
                            LAUNCH_TITLE_LIMIT,
                        );
                        // `_NET_ACTIVE_WINDOW` names X11 windows only.
                        let active = !crate::wayland::is_wayland()
                            && crate::x11::active_window().is_some_and(|active| {
                                resolved.windows.iter().any(|w| w.xid == active)
                            });
                        (resolved, active)
                    })
                    .await
                    .unwrap_or_else(|_| (LaunchedWindows::default(), false));
                    let windows: Vec<Value> =
                        resolved.windows.iter().map(window_record_json).collect();
                    let mut text = message;
                    let mut structured = json!({
                        "pid": resolved.pid,
                        "bundle_id": Value::Null,
                        "name": name,
                        "running": resolved.pid.is_some(),
                        "active": active,
                        "windows": windows,
                        "launcher_pid": launcher_pid,
                    });
                    if let Some(pid) = resolved.pid.filter(|_| resolved.child_of_launcher) {
                        structured["launched_by"] = json!(launcher_pid);
                        text.push_str(&format!(
                            " Its window belongs to pid {pid}, a process the launcher started."
                        ));
                    }
                    if resolved.handed_off {
                        match resolved.pid {
                            Some(pid) => {
                                structured["handoff"] = json!("dbus_activation");
                                text.push_str(&format!(
                                    " The launcher handed the request to another process \
                                     (a wrapper or D-Bus activation); the window belongs to pid {pid}."
                                ));
                            }
                            None => {
                                // The launcher exited and no new window appeared.
                                // This is not a success: no pid, no window. It is
                                // a hand-off only with evidence of one (see
                                // `launch_child::handoff_evidence`); otherwise the
                                // result says how the process ended and quotes
                                // its output.
                                let exit = resolved.exit.or_else(|| {
                                    child.exit_within(std::time::Duration::from_millis(200))
                                });
                                let handoff = launch_child::handoff_evidence(
                                    &program,
                                    exit.as_ref(),
                                    &resolved.running_instance,
                                    &resolved.still_running,
                                );
                                let result = match handoff {
                                    Some(handoff) => launch_handoff_timeout_result(
                                        &name,
                                        launcher_pid,
                                        LAUNCH_HANDOFF_BUDGET.as_secs(),
                                        exit.as_ref(),
                                        &handoff,
                                    ),
                                    None => launch_exited_result(
                                        &name,
                                        launcher_pid,
                                        exit.as_ref(),
                                        &child.output_tail(),
                                        child.output_captured(),
                                    ),
                                };
                                return with_appimage_note(result, child.extract_and_run.as_ref());
                            }
                        }
                    }
                    if active {
                        text.push_str(" The window manager made its window the active one.");
                    }
                    with_appimage_note(
                        ToolResult::text(text).with_structured(structured),
                        child.extract_and_run.as_ref(),
                    )
                } else {
                    ToolResult::text(message).with_structured(json!({
                        "pid": Value::Null,
                        "bundle_id": Value::Null,
                        "name": name,
                        "running": Value::Null,
                        "active": false,
                        "windows": [],
                    }))
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Failed to launch: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// A `launch_app` hand-off timed out: the launcher exited cleanly with
/// evidence that it handed the request on (`handoff`), but no new window of
/// the app appeared within the deadline. This has no pid and no confirmed
/// window, so it is a refusal (`code: "launch_handoff_timeout"`), never a
/// `pid: null` shape that a caller could mistake for success.
fn launch_handoff_timeout_result(
    name: &str,
    launcher_pid: u32,
    waited_secs: u64,
    exit: Option<&LaunchExit>,
    handoff: &launch_child::Handoff,
) -> ToolResult {
    let after = exit
        .map(|exit| format!(" after {}", launch_elapsed(exit.after)))
        .unwrap_or_default();
    let mut structured = json!({
        "code": "launch_handoff_timeout",
        "effect": "refused",
        "name": name,
        "launcher_pid": launcher_pid,
        "pid": Value::Null,
        "running": Value::Null,
        "waited_secs": waited_secs,
        "exit_code": exit.and_then(|exit| exit.status.code()),
        "exited_after_ms": exit.map(|exit| exit.after.as_millis() as u64),
    });
    let text = match handoff {
        launch_child::Handoff::Launcher(program) => {
            structured["handoff"] = json!("launcher");
            format!(
                "'{name}' (pid {launcher_pid}) exited cleanly{after}: {program} hands a launch \
                 to another process and exits, but no new window appeared within \
                 {waited_secs}s; the launch could not be confirmed. Call list_windows to check \
                 whether it started anyway, or retry."
            )
        }
        launch_child::Handoff::RunningInstance { pid, windows } => {
            structured["handoff"] = json!("running_instance");
            structured["running_instance"] = json!({
                "pid": pid,
                "windows": windows.iter().map(window_record_json).collect::<Vec<_>>(),
            });
            let named = windows
                .first()
                .map(|window| format!(" (window {} {})", window.xid, quoted_title(&window.title)))
                .unwrap_or_default();
            format!(
                "'{name}' (pid {launcher_pid}) exited cleanly{after} and no new window appeared \
                 within {waited_secs}s. The app is already running as pid {pid}{named}; a \
                 single-instance app hands a second launch to its running instance, which may \
                 only have raised its existing window. Act on pid {pid}, or call list_windows."
            )
        }
        launch_child::Handoff::StartedProcess { pid } => {
            structured["handoff"] = json!("started_process");
            structured["started_pid"] = json!(pid);
            format!(
                "'{name}' (pid {launcher_pid}) exited cleanly{after}, but pid {pid}, a process \
                 it started, is still running, and no window appeared within {waited_secs}s; \
                 the launch could not be confirmed. Call list_windows to check whether pid \
                 {pid} opens one later."
            )
        }
    };
    ToolResult::error(text).with_structured(structured)
}

/// A launched program's output as result text: escaped by
/// `untrusted_text::block` (controls, terminal escapes and invisible
/// characters shown as `\u{..}`) and every line fenced with `| `, so it can
/// neither forge a line of Pincer's own nor reach the client's terminal raw.
fn fenced_output(tail: &str) -> String {
    pincer_core::untrusted_text::block(tail)
        .lines()
        .map(|line| format!("| {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// "40 ms" under a second, "1.0 s" from there.
fn launch_elapsed(elapsed: std::time::Duration) -> String {
    if elapsed < std::time::Duration::from_secs(1) {
        format!("{} ms", elapsed.as_millis())
    } else {
        format!("{:.1} s", elapsed.as_secs_f64())
    }
}

/// The launched process ended without opening a window and nothing shows a
/// hand-off: say how it ended (exit code or signal, and when) and quote the
/// end of its output (stdout and stderr).
fn launch_exited_result(
    name: &str,
    launcher_pid: u32,
    exit: Option<&LaunchExit>,
    output_tail: &str,
    output_captured: bool,
) -> ToolResult {
    use std::os::unix::process::ExitStatusExt;
    let how = match exit {
        Some(exit) => format!("{} after {}", exit.describe(), launch_elapsed(exit.after)),
        None => "exited (its exit status was not recorded)".to_owned(),
    };
    let mut text =
        format!("'{name}' (pid {launcher_pid}) {how} without opening a window; it is not running.");
    if !output_tail.is_empty() {
        text.push_str(&format!(
            " The end of its output (program text):\n{}",
            fenced_output(output_tail)
        ));
    } else if output_captured {
        text.push_str(" It printed nothing.");
    }
    ToolResult::error(text).with_structured(json!({
        "code": "launch_exited",
        "effect": "none",
        "name": name,
        "launcher_pid": launcher_pid,
        "pid": Value::Null,
        "running": false,
        "exit_code": exit.and_then(|exit| exit.status.code()),
        "signal": exit.and_then(|exit| exit.status.signal()),
        "exited_after_ms": exit.map(|exit| exit.after.as_millis() as u64),
        "output_tail": if output_captured {
            json!(pincer_core::untrusted_text::block(output_tail))
        } else {
            Value::Null
        },
    }))
}

/// What `launch_app` could attribute to the launch after the spawn.
#[derive(Default)]
struct LaunchedWindows {
    /// The process that owns the app's window: the launcher itself, or the
    /// wrapper target / running service it handed off to.
    pid: Option<u32>,
    windows: Vec<crate::x11::WindowInfo>,
    /// The window belongs to another process than the launcher, or the
    /// launcher exited without one.
    handed_off: bool,
    /// The window belongs to a process the launcher started (`pid`).
    child_of_launcher: bool,
    /// How the launcher exited, when it did.
    exit: Option<LaunchExit>,
    /// When the launcher exited without a new window: windows of the
    /// launched app that other processes own (an already-running instance).
    running_instance: Vec<crate::x11::WindowInfo>,
    /// When the launcher exited without a new window: processes it started
    /// that are still running.
    still_running: Vec<u32>,
}

/// Whether `pid` descends from `ancestor`: a bounded walk up the parent
/// chain in /proc. A child whose parent already exited was reparented and no
/// longer counts.
fn descends_from(pid: u32, ancestor: u32) -> bool {
    let mut current = pid;
    for _ in 0..32 {
        let Some(parent) = std::fs::read_to_string(format!("/proc/{current}/stat"))
            .ok()
            .and_then(|stat| {
                let (_, rest) = stat.rsplit_once(')')?;
                rest.split_whitespace().nth(1)?.parse::<u32>().ok()
            })
        else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        if parent <= 1 {
            return false;
        }
        current = parent;
    }
    false
}

/// Whether the launched process is gone or a zombie (already reaped or being
/// reaped by the launch thread).
fn process_exited(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state == "Z" || state == "X"),
        Err(_) => true,
    }
}

/// Whether a new window is the launch's, by its WM_CLASS / app id: the
/// class, or an app-id class's last part, is one of the launch's names
/// whole ([`window_is_launched_app`]), or starts with one of them followed
/// by `-` (`libreoffice` opens `libreoffice-writer`). Never part of a name:
/// a window of class "e" is not a "kwrite" launch, and "plasmashell" is not
/// an "sh" one.
fn window_matches_launch(window: &crate::x11::WindowInfo, query: &str) -> bool {
    if window_is_launched_app(window, query) {
        return true;
    }
    let class = window.app_name.trim().to_ascii_lowercase();
    !class.is_empty()
        && launch_identity_names(query).iter().any(|name| {
            class
                .strip_prefix(name.as_str())
                .is_some_and(|rest| rest.starts_with('-'))
        })
}

/// The last part of a reverse-DNS app id ("org.gnome.Nautilus" ->
/// "Nautilus"): a single word with at least two dots. A file name with an
/// extension ("hi.sh", "soffice.bin") is not one.
fn app_id_last(name: &str) -> Option<&str> {
    let (_, last) = name.rsplit_once('.')?;
    (name.matches('.').count() >= 2
        && !last.is_empty()
        && !name.contains(char::is_whitespace)
        && !name.contains('/'))
    .then_some(last)
}

/// The names a launch's own window class can have, lowercase. A query is
/// NUL-separated keys (a desktop entry's display name, the query, and last
/// the command that ran) or one command. Each key counts whole; a key that
/// is one word also as its file name without ".desktop" and, for an app id,
/// its last part. The command adds the program it runs, with and without its
/// extension.
fn launch_identity_names(query: &str) -> Vec<String> {
    let keys: Vec<String> = query
        .split('\u{0}')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut candidates: Vec<String> = Vec::new();
    for key in &keys {
        candidates.push(key.clone());
        if key.contains(char::is_whitespace) {
            continue;
        }
        let stem = key
            .rsplit('/')
            .next()
            .unwrap_or(key)
            .trim_end_matches(".desktop");
        candidates.push(stem.to_owned());
        if let Some(last) = app_id_last(stem) {
            candidates.push(last.to_owned());
        }
    }
    if let Some(command) = keys.last() {
        let program = launch_child::launch_program(command);
        match app_id_last(&program) {
            Some(last) => candidates.push(last.to_owned()),
            None => {
                if let Some((base, _)) = program
                    .rsplit_once('.')
                    .filter(|(base, _)| !base.is_empty())
                {
                    candidates.push(base.to_owned());
                }
            }
        }
        candidates.push(program);
    }
    let mut names: Vec<String> = Vec::new();
    for name in candidates {
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Whether `window` is plainly an instance of the launched app: its class,
/// or an app-id class's last part, is one of the launch's names, whole
/// (case-insensitive). This is the evidence for a running instance, so,
/// unlike [`window_matches_launch`], it never matches part of a name: a
/// script "hi.sh" must not name the desktop shell ("plasmashell") as the app
/// it handed off to.
fn window_is_launched_app(window: &crate::x11::WindowInfo, query: &str) -> bool {
    let class = window.app_name.trim().to_ascii_lowercase();
    if class.is_empty() {
        return false;
    }
    let names = launch_identity_names(query);
    names.contains(&class)
        || app_id_last(&class).is_some_and(|last| names.iter().any(|name| name == last))
}

/// How long a launch may take to show a window while the launcher process is
/// still alive (the app is starting; LibreOffice on a small VM needs well over
/// ten seconds), and once it has exited (a wrapper or D-Bus activation has
/// taken over, so the window is either imminent or never coming).
const LAUNCH_STARTING_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
const LAUNCH_HANDOFF_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);
/// After a launcher fails (non-zero exit or a signal), how long a window of
/// the launch is still waited for: a failed start does not hand off, so its
/// window is not coming.
const LAUNCH_FAILED_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
/// After a clean exit with no sign of a hand-off, how long a late window is
/// still waited for. `sleep 1` used to be reported 8 s after it started.
const LAUNCH_CLEAN_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// The window budget once the launcher's state is known: a failed exit waits
/// only [`LAUNCH_FAILED_GRACE`] past the exit, a clean exit the hand-off
/// budget when there is a sign of a hand-off (`handoff_sign`, see
/// [`launch_child::handoff_evidence`]) and [`LAUNCH_CLEAN_EXIT_GRACE`] past
/// the exit when there is none, a running launcher the starting budget. An
/// exit not yet recorded keeps the hand-off budget until it is (the reaper
/// stores it right after the process is gone).
fn launch_window_budget(
    exit: Option<&LaunchExit>,
    launcher_exited: bool,
    handoff_sign: bool,
) -> std::time::Duration {
    match exit {
        Some(exit) if !exit.status.success() => exit.after + LAUNCH_FAILED_GRACE,
        Some(exit) if !handoff_sign => exit.after + LAUNCH_CLEAN_EXIT_GRACE,
        _ if launcher_exited => LAUNCH_HANDOFF_BUDGET,
        _ => LAUNCH_STARTING_BUDGET,
    }
}

/// Resolve the window(s) of a launch. Prefers the launcher's own window;
/// otherwise adopts a newly mapped top-level whose WM_CLASS matches the
/// launch, whoever owns it: a wrapper script that exec'd the real binary
/// (`libreoffice` → `soffice.bin`), or the running D-Bus service GNOME apps
/// hand their request to. `program` is the program the launch ran
/// ([`launch_child::launch_program`]).
fn resolve_launched_windows(
    launcher: &LaunchedChild,
    query: &str,
    program: &str,
    before: &std::collections::HashSet<u64>,
) -> LaunchedWindows {
    let launcher_pid = launcher.pid;
    let started = std::time::Instant::now();
    // Processes the launcher started, recorded while it runs: once it exits
    // they are reparented and no longer descend from it.
    let mut started_pids: Vec<u32> = Vec::new();
    loop {
        if !process_exited(launcher_pid) {
            for pid in launch_child::descendants(launcher_pid) {
                if !started_pids.contains(&pid) {
                    started_pids.push(pid);
                }
            }
        }
        let all = crate::wayland::list_windows_dispatch(None);
        let own: Vec<_> = all
            .iter()
            .filter(|w| w.pid == Some(launcher_pid))
            .cloned()
            .collect();
        if !own.is_empty() {
            return LaunchedWindows {
                pid: Some(launcher_pid),
                windows: own,
                ..LaunchedWindows::default()
            };
        }
        // A new window of a process the launcher started is the launch's
        // own, whatever its class: an AppImage runtime forks its app (the
        // window belongs to the app's pid), and so does a wrapper that
        // does not exec, or a CLI that starts its app detached and exits.
        if let Some(pid) = all
            .iter()
            .filter(|w| !before.contains(&w.xid))
            .filter_map(|w| w.pid)
            .find(|pid| started_pids.contains(pid) || descends_from(*pid, launcher_pid))
        {
            return LaunchedWindows {
                pid: Some(pid),
                windows: all.iter().filter(|w| w.pid == Some(pid)).cloned().collect(),
                child_of_launcher: true,
                ..LaunchedWindows::default()
            };
        }
        let fresh: Vec<_> = all
            .iter()
            .filter(|w| !before.contains(&w.xid) && w.pid.is_some())
            .filter(|w| window_matches_launch(w, query))
            .cloned()
            .collect();
        if let Some(pid) = fresh.first().and_then(|w| w.pid) {
            let windows = fresh.into_iter().filter(|w| w.pid == Some(pid)).collect();
            return LaunchedWindows {
                pid: Some(pid),
                windows,
                handed_off: true,
                exit: launcher.exit(),
                ..LaunchedWindows::default()
            };
        }
        let exit = launcher.exit();
        let launcher_exited = exit.is_some() || process_exited(launcher_pid);
        // After a clean exit, the evidence the result would cite for a
        // hand-off decides how long a late window is waited for.
        let (running_instance, still_running) = if launcher_exited {
            (
                all.iter()
                    .filter(|w| w.pid.is_some() && w.pid != Some(launcher_pid))
                    .filter(|w| window_is_launched_app(w, query))
                    .cloned()
                    .collect::<Vec<_>>(),
                started_pids
                    .iter()
                    .copied()
                    .filter(|pid| !process_exited(*pid))
                    .collect::<Vec<_>>(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let handoff_sign = launch_child::handoff_evidence(
            program,
            exit.as_ref(),
            &running_instance,
            &still_running,
        )
        .is_some();
        if started.elapsed() >= launch_window_budget(exit.as_ref(), launcher_exited, handoff_sign) {
            return LaunchedWindows {
                pid: (!launcher_exited).then_some(launcher_pid),
                handed_off: launcher_exited,
                exit,
                running_instance,
                still_running,
                ..LaunchedWindows::default()
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
mod launch_resolution_tests;

fn chromium_family_program(program: &str) -> bool {
    let basename = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    ["chrome", "chromium", "electron", "brave", "edge"]
        .iter()
        .any(|needle| basename.contains(needle))
}

// ── shared helpers ────────────────────────────────────────────────────────────

/// Resolve an AT-SPI element's center in window-local coordinates.
///
/// Returns `(xid, window_local_x, window_local_y)`.
/// Looks up element bounds via native AT-SPI, finds the owning window
/// (via `xid_hint` or the first window for `pid`), then converts screen-absolute
/// → window-local coords via compositor metadata on Wayland or
/// XTranslateCoordinates on X11.
fn resolve_element_local_coords(
    pid: u32,
    idx: usize,
    xid_hint: Option<u64>,
) -> anyhow::Result<(u64, f64, f64)> {
    // An element index is a position in one window's snapshot; without that
    // window it names nothing (a pid-wide lookup reached another window's
    // element with the same index).
    let xid = xid_hint.ok_or_else(|| element_without_window(idx))?;
    let (bx, by, bw, bh) = crate::atspi::get_element_bounds_for_window(pid, xid, idx)?;
    let screen_cx = bx as f64 + bw as f64 / 2.0;
    let screen_cy = by as f64 + bh as f64 / 2.0;

    if crate::wayland::wayland_input_enabled() {
        let (window_x, window_y, window_width, window_height) =
            crate::wayland::window_geometry(xid)
                .ok_or_else(|| anyhow::anyhow!("No Wayland geometry for window {xid}"))?;
        if window_width == 0 || window_height == 0 {
            anyhow::bail!("Wayland window {xid} has no usable geometry");
        }
        return Ok((
            xid,
            screen_cx - window_x as f64,
            screen_cy - window_y as f64,
        ));
    }

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;
    let (conn, screen_num) = crate::x11::query_connection()?;
    let root = conn.setup().roots[screen_num].root;
    let reply = conn
        .translate_coordinates(xid as u32, root, 0, 0)?
        .reply()?;
    let local_x = screen_cx - reply.dst_x as f64;
    let local_y = screen_cy - reply.dst_y as f64;
    Ok((xid, local_x, local_y))
}

/// An element whose AT-SPI extents do not fall inside its own toplevel: GTK4
/// reports empty extents for some widgets (a text view centre resolves to
/// (0,0), the GNOME hot corner), and a real pointer click there would hit
/// whatever is under that screen point. Refused before any input is sent.
#[derive(Debug)]
struct ElementBoundsUnusable {
    idx: usize,
    local: (f64, f64),
    window: (u32, u32),
}

impl std::fmt::Display for ElementBoundsUnusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "element [{}] resolves to window-local ({:.0}, {:.0}), outside its {}x{} window; \
             the toolkit reported unusable AT-SPI extents, so no pointer input was sent",
            self.idx, self.local.0, self.local.1, self.window.0, self.window.1
        )
    }
}
impl std::error::Error for ElementBoundsUnusable {}

/// `resolve_element_local_coords` plus the sanity check that the centre lies
/// inside the window it belongs to. Blocking.
fn checked_element_local_coords(
    pid: u32,
    idx: usize,
    xid_hint: Option<u64>,
) -> anyhow::Result<(u64, f64, f64)> {
    let (xid, lx, ly) = resolve_element_local_coords(pid, idx, xid_hint)?;
    if crate::wayland::wayland_input_enabled() {
        return Ok((xid, lx, ly));
    }
    use x11rb::protocol::xproto::ConnectionExt as _;
    let (conn, _) = crate::x11::query_connection()?;
    let geom = conn.get_geometry(xid as u32)?.reply()?;
    let (w, h) = (u32::from(geom.width), u32::from(geom.height));
    if lx < 0.0 || ly < 0.0 || lx >= f64::from(w) || ly >= f64::from(h) {
        return Err(ElementBoundsUnusable {
            idx,
            local: (lx, ly),
            window: (w, h),
        }
        .into());
    }
    Ok((xid, lx, ly))
}

/// The error for an element index that comes without the window whose
/// snapshot it indexes.
fn element_without_window(idx: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "element [{idx}] has no window: an element index belongs to one window's snapshot"
    )
}

fn element_screen_center(pid: u32, idx: usize, xid: Option<u64>) -> anyhow::Result<(f64, f64)> {
    element_screen_center_and_rect(pid, idx, xid).map(|(center, _)| center)
}

/// The element's screen centre plus its AT-SPI extents as the overlay's
/// `[x, y, width, height]`, both from one bounds lookup (same screen space).
fn element_screen_center_and_rect(
    pid: u32,
    idx: usize,
    xid: Option<u64>,
) -> anyhow::Result<((f64, f64), [f64; 4])> {
    let xid = xid.ok_or_else(|| element_without_window(idx))?;
    let (bx, by, bw, bh) = crate::atspi::get_element_bounds_for_window(pid, xid, idx)?;
    let rect = [bx as f64, by as f64, bw as f64, bh as f64];
    Ok(((rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0), rect))
}

/// A snapshot frame `(x, y, w, h)` as the overlay target rect, only when it
/// is non-empty and actually contains the glide point (a redirected press or
/// a frame in another space gets no rect rather than a wrong one).
fn frame_target_rect(bounds: Option<(i32, i32, u32, u32)>, sx: f64, sy: f64) -> Option<[f64; 4]> {
    let (x, y, w, h) = bounds?;
    let rect = [f64::from(x), f64::from(y), f64::from(w), f64::from(h)];
    (w > 0
        && h > 0
        && sx >= rect[0]
        && sy >= rect[1]
        && sx <= rect[0] + rect[2]
        && sy <= rect[1] + rect[3])
        .then_some(rect)
}

/// Shared schema for the optional `coordinate_frame` of pointer tools.
fn coordinate_frame_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["window", "desktop"],
        "description": "Frame of all x/y coordinates: \"window\" (default) get_window_state \
            screenshot pixels; \"desktop\" get_desktop_state pixels. scope:\"desktop\" with \
            pid/window_id means the same."
    })
}

fn coordinate_frame_is_desktop(args: &Value) -> bool {
    args.get("coordinate_frame").and_then(Value::as_str) == Some("desktop")
}

fn window_local_to_screen(xid: u64, x: f64, y: f64) -> anyhow::Result<(f64, f64)> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let (conn, screen_num) = crate::x11::query_connection()?;
    let root = conn.setup().roots[screen_num].root;
    let reply = conn
        .translate_coordinates(xid as u32, root, 0, 0)?
        .reply()?;
    Ok((reply.dst_x as f64 + x, reply.dst_y as f64 + y))
}

/// Upper bound for one AT-SPI element operation (bounds lookup / action).
/// The AT-SPI layer bounds single bus calls, but a full tree walk over a large
/// GTK/VCL window chains many of them; observed stalls ran to minutes.
const ELEMENT_AX_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Either `coordinate_frame:"desktop"` or `scope:"desktop"` marks `x,y` as
/// desktop (screen) pixels; both are translated exactly once by the caller.
fn desktop_frame_requested(args: &Value) -> bool {
    coordinate_frame_is_desktop(args)
        || args.get("scope").and_then(Value::as_str) == Some("desktop")
}

/// Convert desktop (screen) coordinates to `xid`'s window-local frame so the
/// rest of the pointer pipeline (which is window-local by contract) is reused.
fn desktop_to_window_local(xid: u64, x: f64, y: f64) -> anyhow::Result<(f64, f64)> {
    let (ox, oy) = window_local_to_screen(xid, 0.0, 0.0)?;
    Ok((x - ox, y - oy))
}

/// Upper bound for one foreground input transaction: focus confirmation plus
/// the injection itself. Typing sleeps per character, so scale with `chars`.
fn foreground_budget(chars: usize) -> std::time::Duration {
    std::time::Duration::from_millis(12_000 + (chars as u64) * 40)
}

/// `spawn_blocking` with a hard deadline. A wedged X server (or a client
/// holding a server grab) must surface as a structured `foreground_timeout`
/// error instead of a tool call that never returns. The blocking task is not
/// cancellable; it is left to finish (or leak) on the blocking pool.
async fn spawn_blocking_bounded<T: Send + 'static>(
    label: &'static str,
    budget: std::time::Duration,
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<anyhow::Result<T>, tokio::task::JoinError> {
    match tokio::time::timeout(budget, tokio::task::spawn_blocking(f)).await {
        Ok(joined) => joined,
        Err(_elapsed) => Ok(Err(anyhow::anyhow!(
            "{}: {label} did not complete within {budget:?}; the X server or the target \
             application may be wedged. Take a screenshot before retrying",
            crate::input::foreground::CODE_TIMEOUT
        ))),
    }
}

/// Map an input-path error to a tool error, attaching the structured
/// foreground `code` (`foreground_unavailable` / `foreground_timeout`) when the
/// message carries one so the caller can branch instead of parsing prose.
fn input_error_result(e: anyhow::Error) -> ToolResult {
    let text = e.to_string();
    if crate::input::is_uinput_unavailable(&e) {
        // The focus-free real-input route exists but this process cannot open
        // /dev/uinput: an honest refusal the operator can act on, not a bare
        // I/O error.
        let mut refusal = with_uinput_hint(crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
        ));
        if let Some(structured) = refusal.structured_content.as_mut() {
            structured["cause"] = json!(crate::input::UINPUT_UNAVAILABLE_CODE);
            structured["detail"] = json!(text);
        }
        return refusal;
    }
    match crate::input::foreground::error_code(&e) {
        Some(code) => ToolResult::error(text.clone()).with_structured(json!({
            "code": code,
            "detail": text,
            "effect": "none",
            "suggestion": if code == crate::input::foreground::CODE_TIMEOUT {
                "Take a screenshot (get_desktop_state) to confirm the desktop is responsive, then retry once."
            } else {
                "Call bring_to_front on the window (or its modal dialog), confirm with get_window_state, then retry."
            },
        })),
        None => linux_input_error(e),
    }
}

/// Structured payload for a completed foreground transaction.
fn foreground_structured(
    path: &str,
    report: crate::input::ForegroundReport,
    extra: serde_json::Map<String, Value>,
) -> Value {
    let mut v = json!({
        "path": path,
        "verified": false,
        "delivery_mode": "foreground",
        "effect": "unverifiable",
    });
    apply_foreground_report(&mut v, &report);
    for (k, val) in extra {
        v[k] = val;
    }
    v
}

/// Fold a foreground transaction's post-check into a result: the report
/// itself, `focus_after` at the top level, and an `evidence` item so the
/// observation survives the public-record reduction. A window of the target
/// process that appeared or closed is `window_change` evidence and an
/// `effect: confirmed` wherever the focus sits afterwards (the OK button of a
/// dialog destroys the very window that held the focus, and closing the last
/// window of a process leaves the focus to the window manager); a change
/// among unattributed popups only counts while the focus stayed in the
/// process (`ForegroundReport::window_change_confirms`). Otherwise, focus that
/// left the process is a suspected no-op and anything else stays
/// unverifiable with a `native_api_result` item.
fn apply_foreground_report(v: &mut Value, report: &crate::input::ForegroundReport) {
    v["foreground"] = report.to_json();
    v["focus_after"] = json!(report.focus_after.as_str());
    let confirming_change = report
        .window_change
        .as_ref()
        .filter(|_| report.window_change_confirms());
    let evidence = match confirming_change {
        Some(change) => {
            v["effect"] = json!("confirmed");
            v["verified"] = json!(true);
            json!({
                "kind": "window_change",
                "detail": format!("focus_after={}; {change}", report.focus_after.as_str()),
            })
        }
        None if report.focus_after == crate::input::FocusAfter::Elsewhere => {
            v["effect"] = json!("suspected_noop");
            v["warning"] = json!(
                "input focus left the target process before the post-check; the input \
                 may have reached another window. Verify with a screenshot."
            );
            return;
        }
        None => json!({
            "kind": "native_api_result",
            "detail": format!(
                "real input delivered to the activated window (focus_after={}); {}",
                report.focus_after.as_str(),
                match &report.window_change {
                    Some(change) => format!("{change}, not attributed to the target process"),
                    None => "no window change observed".to_owned(),
                }
            ),
        }),
    };
    let mut items = v
        .get("evidence")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items.push(evidence);
    v["evidence"] = json!(items);
}

/// What became of the open popup (menu, combo list) a click was aimed at.
/// A click on one of its items closes it, so a popup that closed is
/// `window_change` evidence that the click reached it; one still open after
/// the click means the click chose nothing that closes it, which the result
/// says instead of leaving the caller to guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PopupClickOutcome {
    window: u64,
    closed: bool,
    /// The click ran an item's accessible action instead of pressing it:
    /// Qt runs a menu item that way but leaves its menu open.
    ran_action: bool,
}

impl PopupClickOutcome {
    /// The outcome of a foreground transaction that went to a popup.
    fn from_foreground(report: &crate::input::ForegroundReport) -> Option<Self> {
        report.popup_target.map(|window| Self {
            window,
            closed: report.popup_closed,
            ran_action: false,
        })
    }

    /// Fold the outcome into a result's structured content (`popup_closed`,
    /// evidence, effect) and return the sentence for its text.
    fn apply(&self, v: &mut Value) -> String {
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
const POINTER_CHECK: &str =
    "read what changed with get_window_state since:\"latest\", or take a screenshot";

/// Text tail for a foreground pixel click: the confirming window change when
/// there is one, else how to check it.
fn foreground_click_suffix(path: &str, report: &crate::input::ForegroundReport) -> String {
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
fn invalid_pointer_arguments(tool: &str, detail: &str) -> ToolResult {
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
fn require_point_args(tool: &str, args: &Value) -> Option<ToolResult> {
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

#[cfg(test)]
mod pointer_argument_tests;

fn parse_mouse_button(name: &str) -> u8 {
    match name {
        "right" => 3,
        "middle" => 2,
        _ => 1,
    }
}

/// Escalation hint for a non-AX / suspected-no-op surface — the cross-platform
/// `escalation` field mirrored from macOS. The recommended next rung depends on
/// the session type: X11 can pixel-target a specific window in the background,
/// so the deliberate move is an element px action — click by pixel (x,y) off the
/// screenshot already in this response (`px`). Native Wayland CANNOT
/// background-target an unfocused window (libei injects to the compositor's input
/// focus — see `input::delivery`), and the pixel path itself routes through the
/// same coordinate-free AT-SPI actuation that just no-op'd, so the move there is
/// to bring the window to the foreground first (`foreground`).
fn non_ax_escalation() -> Value {
    if crate::wayland::is_wayland() {
        json!({
            "recommended": "foreground",
            "reason": "non-AX surface on Wayland: a specific unfocused window can't be \
                       pixel-targeted in the background (libei injects to the compositor's \
                       focus). bring_to_front, then click by pixel off the screenshot \
                       with delivery_mode:\"foreground\"."
        })
    } else {
        json!({
            "recommended": "px",
            "reason": "non-AX surface — act by pixel (x,y) off the screenshot \
                       in this response (an element px action)."
        })
    }
}

/// Structured payload for a `type_text` response. Keeps the legacy
/// `path`/`characters`/`verified` fields for back-compat and adds the cross-tool
/// `effect` tri-state. Linux's AT-SPI `insertText` return value acknowledges
/// the method call but does not read the widget value back, so it and every
/// keystroke / XSendEvent / XTest / Wayland rung are `"unverifiable"` (the
/// caller confirms through a separate observation). No escalation is
/// attached to a successful delivery: the public projection renders one as
/// `delivery_failed`, which is wrong for text that landed but was not read
/// back. Mirrors the macOS `type_text` contract.
fn type_text_structured(path: &str, characters: usize, verified: bool) -> Value {
    let s = json!({
        "path": path,
        "characters": characters,
        "verified": verified,
        "effect": if verified { "confirmed" } else { "unverifiable" },
    });
    // No escalation on a successful-but-unverified delivery: the public
    // projection renders a foreground escalation as `delivery_failed`, which
    // made models re-type text that had already landed. Refusals carry their own.
    let _ = verified;
    s
}

/// Structured payload for the Electron/Chromium AX-echo case: the AT-SPI
/// `insertText` rung returned success on a Chromium embedder, but on those
/// surfaces the a11y bridge can accept and echo the write while the renderer
/// never observes it — so a path=="ax" "confirm" is a shim echo, not ground
/// truth. Mirrors the macOS `type_text` `ax_echo_surface` branch: report
/// effect:"unverifiable" + escalation:{recommended:"px"} (a renderer-focus
/// problem — pixel-focus the field, not foreground-activate it).
fn type_text_structured_electron(text_len: usize) -> Value {
    json!({
        "path": "ax",
        "characters": text_len,
        "verified": false,
        "effect": "unverifiable",
        "escalation": {
            "recommended": "px",
            "reason": "Electron/web surface: the AX write was echoed but the \
                       renderer may not have observed it. Confirm via the screenshot; \
                       if it didn't land, re-type with the element px action (pass x,y \
                       to pixel-focus the field, then type)."
        }
    })
}

/// [`type_text_ax_result`] for an element-addressed insert: the snapshot
/// object is read back (as `set_value` does) and, when the typed text is now
/// part of its value, the result is `effect: confirmed` with a
/// `value_readback` evidence item instead of `unverifiable`.
async fn type_text_ax_result_verified(
    pid: u32,
    xid: u64,
    idx: usize,
    text: &str,
    route: &str,
    password_before: Option<usize>,
) -> ToolResult {
    let text_len = text.chars().count();
    if is_chromium_embedder(pid) {
        return type_text_ax_result(pid, text_len, route);
    }
    let typed = text.to_owned();
    let readback = tokio::task::spawn_blocking(move || {
        // The caret sits after the insert: the text there, with a little
        // context, proves it even past the 4,096 characters a value read
        // returns. A field whose caret read misses it (a spin button that
        // reformats "-40", a masked password) is read whole as before.
        let before = typed.chars().count() + READBACK_CONTEXT_CHARS;
        if let Some((around, more_before, more_after)) =
            crate::atspi::read_text_around_caret_in(pid, xid, idx, before, READBACK_CONTEXT_CHARS)
        {
            if !typed.trim().is_empty() && around.contains(typed.as_str()) {
                // Marked as a piece of the field, so a short one is not
                // quoted as its whole value.
                return Some(format!(
                    "{}{around}{}",
                    if more_before { "..." } else { "" },
                    if more_after { "..." } else { "" }
                ));
            }
        }
        crate::atspi::read_value_in(pid, xid, idx)
    })
    .await
    .ok()
    .flatten();
    type_text_readback_result(pid, idx, text, route, readback, password_before)
}

/// Characters of context quoted on each side of typed text in a read-back.
const READBACK_CONTEXT_CHARS: usize = 20;
/// A read-back up to this long is quoted whole.
const READBACK_WHOLE_CHARS: usize = 80;
/// Typed text longer than this is confirmed without quoting it back.
const READBACK_QUOTED_TEXT_CHARS: usize = 60;

/// One line of `seen` for a result, shown inside double quotes: the app's
/// text escaped by `untrusted_text::inline` (`\"`, `\\`, `\n`, `\u{2028}`),
/// so it can neither end its quotes nor start a line.
fn readback_one_line(seen: &str) -> String {
    pincer_core::untrusted_text::inline(seen).into_owned()
}

/// A `set_value` or combo box read-back for the result text: the first
/// [`READBACK_WHOLE_CHARS`] characters, escaped and quoted, with the full
/// length named when it was cut. The structured result keeps it whole.
fn readback_quoted(seen: &str) -> String {
    quote_untrusted(seen, READBACK_WHOLE_CHARS)
}

/// What a confirmed read-back quotes: the whole value when it is short, else
/// the typed text with [`READBACK_CONTEXT_CHARS`] of context on each side
/// (`...dog; HELLO 001 The...`). `None` when the typed text is too long to be
/// worth echoing.
fn readback_excerpt(seen: &str, text: &str) -> Option<String> {
    if seen.chars().count() <= READBACK_WHOLE_CHARS {
        return Some(readback_one_line(seen));
    }
    if text.chars().count() > READBACK_QUOTED_TEXT_CHARS {
        return None;
    }
    let Some(at) = seen.find(text) else {
        // Agreed numerically, not by substring: quote the start.
        let head: String = seen.chars().take(READBACK_WHOLE_CHARS).collect();
        return Some(format!("{}...", readback_one_line(&head)));
    };
    let before: Vec<char> = seen[..at].chars().collect();
    let lead_start = before.len().saturating_sub(READBACK_CONTEXT_CHARS);
    let lead: String = before[lead_start..].iter().collect();
    let tail: String = seen[at + text.len()..]
        .chars()
        .take(READBACK_CONTEXT_CHARS)
        .collect();
    let cut_after = seen[at + text.len()..].chars().count() > READBACK_CONTEXT_CHARS;
    Some(format!(
        "{}{}{}{}{}",
        if lead_start > 0 { "..." } else { "" },
        readback_one_line(&lead),
        readback_one_line(text),
        readback_one_line(&tail),
        if cut_after { "..." } else { "" }
    ))
}

/// For a password field, how many mask characters it holds before a
/// `type_text` insert, so the read-back can be compared by length (its text
/// is masked). `None` for any other element, and for a password field that
/// shows its text in the clear (that one reads back normally).
async fn password_length_before(pid: u32, xid: u64, idx: usize) -> Option<usize> {
    if is_chromium_embedder(pid) {
        return None;
    }
    tokio::task::spawn_blocking(move || {
        if !crate::atspi::is_password_in(pid, xid, idx) {
            return None;
        }
        let before = crate::atspi::read_value_in(pid, xid, idx).unwrap_or_default();
        masked_length(&before)
    })
    .await
    .ok()
    .flatten()
}

/// The number of characters `seen` masks when it is a password field's
/// masked text: empty (an empty field), or one repeated character that is
/// neither alphanumeric nor whitespace (Qt's and GTK's "●", "•", "*").
/// `None` for anything else, such as a password shown in the clear.
fn masked_length(seen: &str) -> Option<usize> {
    let mut chars = seen.chars();
    let Some(first) = chars.next() else {
        return Some(0);
    };
    if first.is_alphanumeric() || first.is_whitespace() {
        return None;
    }
    chars.all(|c| c == first).then(|| seen.chars().count())
}

/// Whether `masks` mask characters stand for `text`. Qt masks each UTF-16
/// unit ("a\u{1F600}b" read back as four masks on the Qt probe); GTK is
/// assumed to mask each character, as its entry draws one invisible char per
/// character (not checked against a GTK app here).
fn mask_count_fits(masks: usize, text: &str) -> bool {
    masks == text.chars().count() || masks == text.encode_utf16().count()
}

/// The typed text is confirmed when the element's fresh value contains it
/// (an insert at the caret) or agrees with it numerically (a spin button
/// that reformats "-40" as "-40.0").
fn type_text_readback_result(
    pid: u32,
    idx: usize,
    text: &str,
    route: &str,
    readback: Option<String>,
    password_before: Option<usize>,
) -> ToolResult {
    let text_len = text.chars().count();
    if let (Some(before), Some(seen)) = (password_before, readback.as_deref()) {
        if !seen.contains(text) {
            if let Some(after) = masked_length(seen) {
                return type_text_masked_result(idx, text, route, before, after);
            }
        }
    }
    match readback {
        Some(seen)
            if !text.trim().is_empty() && (seen.contains(text) || values_agree(text, &seen)) =>
        {
            let excerpt = readback_excerpt(&seen, text);
            let shown = match &excerpt {
                Some(excerpt) => format!("it reads back \"{excerpt}\""),
                None => "it reads back the typed text in place".to_owned(),
            };
            let mut structured = json!({
                "path": "ax",
                "characters": text_len,
                "verified": true,
                "effect": "confirmed",
                "evidence": [{
                    "kind": "value_readback",
                    "detail": format!("element [{idx}] reads back the typed text"),
                }],
            });
            if let Some(excerpt) = excerpt {
                structured["readback"] = json!(excerpt);
            }
            ToolResult::text(format!(
                "Typed {text_len} character(s) into element [{idx}] ({route}); {shown}."
            ))
            .with_structured(structured)
        }
        Some(seen) => {
            let mut result = type_text_ax_result(pid, text_len, route);
            let head: String = seen.chars().take(READBACK_WHOLE_CHARS).collect();
            let shown = if seen.chars().count() > READBACK_WHOLE_CHARS {
                format!("{}...", readback_one_line(&head))
            } else {
                readback_one_line(&head)
            };
            if let Some(structured) = result.structured_content.as_mut() {
                structured["readback"] = json!(shown);
            }
            result
                .content
                .push(pincer_core::protocol::Content::text(format!(
                    "The element reads back \"{shown}\", which does not contain the typed text \
                     at its caret or in its first 4,096 characters; confirm with \
                     get_window_state."
                )));
            result
        }
        None => type_text_ax_result(pid, text_len, route),
    }
}

/// `type_text` into a password field: its text reads back as mask
/// characters, so only its length can be compared. The insert landed when the
/// field grew by exactly the typed text's length (a masked-length match).
fn type_text_masked_result(
    idx: usize,
    text: &str,
    route: &str,
    before: usize,
    after: usize,
) -> ToolResult {
    let text_len = text.chars().count();
    let grew_by_text = after
        .checked_sub(before)
        .is_some_and(|grown| mask_count_fits(grown, text));
    let mut structured = json!({
        "path": "ax",
        "characters": text_len,
        "verified": grew_by_text,
        "effect": if grew_by_text { "confirmed" } else { "unverifiable" },
        "readback_masked": true,
        "masked_length_before": before,
        "masked_length": after,
    });
    let text = if grew_by_text {
        structured["evidence"] = json!([{
            "kind": "value_readback",
            "detail": format!(
                "password field element [{idx}] grew from {before} to {after} mask characters, \
                 the length of the typed text"
            ),
        }]);
        format!(
            "Typed {text_len} character(s) into password field element [{idx}] ({route}); it \
             grew from {before} to {after} mask characters, the length of the typed text (a \
             masked-length match: a password field shows only its length)."
        )
    } else {
        format!(
            "Typed {text_len} character(s) into password field element [{idx}] ({route}). The \
             field is masked, so the typed text can't be read back: it holds {after} mask \
             characters, {before} before typing. Not verified."
        )
    };
    ToolResult::text(text).with_structured(structured)
}

#[cfg(test)]
mod type_text_readback_tests;

/// Build the success `ToolResult` for an AT-SPI insert. The EditableText
/// method's boolean is a delivery acknowledgement, not a fresh value readback,
/// so native widgets remain `unverifiable`. Chromium embedders additionally
/// recommend the pixel rung because their accessibility bridge can acknowledge
/// a write the renderer never observes.
fn type_text_ax_result(pid: u32, text_len: usize, route: &str) -> ToolResult {
    if is_chromium_embedder(pid) {
        return ToolResult::text(format!(
            "📨 Sent (unverified) {text_len} character(s) ({route}). Electron/web \
             surface: the AX layer accepts and echoes the write but the renderer may \
             not have observed it, so the driver cannot confirm via AX. Verify via the \
             screenshot; if it didn't land, re-type with the px form (pass x,y to \
             pixel-focus the field)."
        ))
        .with_structured(type_text_structured_electron(text_len));
    }
    ToolResult::text(format!("Typed {text_len} character(s) ({route})."))
        .with_structured(type_text_structured("ax", text_len, false))
}

/// True when `pid` is a Chromium-based embedder — a Chrome/Chromium browser or
/// any Electron/CEF app. On these surfaces an AT-SPI `EditableText.insertText`
/// can succeed at the bridge while the Chromium *renderer* never observes it,
/// so the AT-SPI "ax" rung must not be trusted as a confirmed insert (see
/// [`type_text_ax_result`]).
///
/// This is the Linux analogue of macOS `ElectronJs::is_electron` (which checks
/// for a bundled Electron Framework). Linux has no bundle, so the signal is
/// Chromium's multiprocess fingerprint: the embedder forks sandboxed helpers
/// whose argv carries `--type=renderer` / `--type=zygote` / `--type=gpu-process`.
/// Native GTK/Qt apps never spawn such helpers, so this is conservative (very
/// low false-positive). The single-process fallback also matches the embedder's
/// own argv. Reads `/proc`; cheap and only invoked on the rare AT-SPI confirm.
fn is_chromium_embedder(pid: u32) -> bool {
    fn argv_is_chromium_helper(p: u32) -> bool {
        fs::read(format!("/proc/{p}/cmdline"))
            .map(|raw| crate::at_point_policy::cmdline_is_chromium_helper(&raw))
            .unwrap_or(false)
    }
    // Single-process / the embedder itself carrying a Chromium switch, or a
    // Chromium helper anywhere below it.
    argv_is_chromium_helper(pid) || descendant_pids(pid).into_iter().any(argv_is_chromium_helper)
}

fn is_webkitgtk_embedder(pid: u32) -> bool {
    fn argv_is_webkit_helper(pid: u32) -> bool {
        fs::read(format!("/proc/{pid}/cmdline"))
            .map(|raw| {
                let cmdline = String::from_utf8_lossy(&raw);
                cmdline.contains("WebKitWebProcess") || cmdline.contains("WebKitNetworkProcess")
            })
            .unwrap_or(false)
    }
    argv_is_webkit_helper(pid) || descendant_pids(pid).into_iter().any(argv_is_webkit_helper)
}

/// `pid`'s descendants, breadth first. These checks run on every input
/// call, so they read only the descendants' own child lists when the kernel
/// keeps them (CONFIG_PROC_CHILDREN), instead of every process's status:
/// with about 370 processes that was some 740 file reads per key press.
fn descendant_pids(pid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([pid]);
    let mut seen = std::collections::HashSet::from([pid]);
    // Built once, only when the kernel has no child lists.
    let mut by_parent: Option<std::collections::HashMap<u32, Vec<u32>>> = None;
    while let Some(current) = queue.pop_front() {
        let kids = match proc_children(current) {
            Some(kids) => kids,
            None => by_parent
                .get_or_insert_with(children_by_parent)
                .get(&current)
                .cloned()
                .unwrap_or_default(),
        };
        for kid in kids {
            if seen.insert(kid) {
                found.push(kid);
                queue.push_back(kid);
            }
        }
    }
    found
}

/// Direct children of `pid` from `/proc/<pid>/task/<tid>/children`. `None`
/// when the kernel doesn't provide those lists (or `pid` is gone).
fn proc_children(pid: u32) -> Option<Vec<u32>> {
    let tasks = fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut kids = Vec::new();
    let mut read_any = false;
    for task in tasks.flatten() {
        match fs::read_to_string(task.path().join("children")) {
            Ok(list) => {
                read_any = true;
                kids.extend(list.split_whitespace().filter_map(|kid| kid.parse::<u32>().ok()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !task.path().exists() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            // A thread that exited between the listing and the read.
            Err(_) => {}
        }
    }
    read_any.then_some(kids)
}

/// Every process's children, keyed by parent pid, from each /proc/*/status.
fn children_by_parent() -> std::collections::HashMap<u32, Vec<u32>> {
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return children;
    };
    for entry in entries.flatten() {
        let Ok(child) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(status) = fs::read_to_string(format!("/proc/{child}/status")) else {
            continue;
        };
        if let Some(ppid) = status
            .lines()
            .find(|line| line.starts_with("PPid:"))
            .and_then(|line| line[5..].trim().parse::<u32>().ok())
        {
            children.entry(ppid).or_default().push(child);
        }
    }
    children
}

fn maps_indicate_gtk(maps: &str) -> bool {
    maps.contains("libgtk-3.so") || maps.contains("libgtk-4.so")
}

/// Toolkits known to discard synthetic (`send_event`) X11 pointer events:
/// GTK3/4 (XInput2 only), LibreOffice VCL (all plugins), Qt5/6 (xcb, XI2).
fn maps_indicate_synthetic_pointer_dropped(maps: &str) -> bool {
    maps_indicate_gtk(maps)
        || maps.contains("libvcl")
        || maps.contains("libmergedlo")
        || maps.contains("libQt5Gui")
        || maps.contains("libQt6Gui")
}

fn synthetic_pointer_is_dropped(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/maps"))
        .map(|maps| maps_indicate_synthetic_pointer_dropped(&maps))
        .unwrap_or(false)
}

/// Toolkits that ignore a synthetic (`send_event`) X11 button press: GTK3/4
/// and LibreOffice VCL. Qt5/6 handle a core press as a "legacy" mouse event:
/// a synthetic right-click opens Kate's context menu and a synthetic press
/// moves KWrite's caret (checked 2026-11-03). Synthetic press, motion and
/// release drags select text in KWrite (Qt 6) and in a Qt 5.15 text edit,
/// with another window active (checked 2026-11-11). Qt does drop core wheel
/// buttons, so scroll keeps [`maps_indicate_synthetic_pointer_dropped`].
fn maps_indicate_synthetic_press_dropped(maps: &str) -> bool {
    maps_indicate_gtk(maps) || maps.contains("libvcl") || maps.contains("libmergedlo")
}

fn synthetic_press_is_dropped(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/maps"))
        .map(|maps| maps_indicate_synthetic_press_dropped(&maps))
        .unwrap_or(false)
}

/// Qt5 maps its own GUI library; only then is the synthetic-FocusIn bridge
/// workaround in `type_text` worth two extra AT-SPI walks.
fn is_qt5_process(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/maps"))
        .map(|maps| maps.contains("libQt5Gui") || maps.contains("libQt5Widgets"))
        .unwrap_or(false)
}

fn is_gtk_process(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/maps"))
        .map(|maps| maps_indicate_gtk(&maps))
        .unwrap_or(false)
}

fn unavailable_webkit_background(
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

fn unavailable_webkit_keyboard_background(
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

fn with_uinput_hint(mut refusal: ToolResult) -> ToolResult {
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
fn background_keyboard_refusal(atspi_ok: bool) -> ToolResult {
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

fn atspi_up() -> bool {
    let probe = std::thread::spawn(crate::health_report::probe_a11y_bus);
    crate::wayland::is_wayland() || probe.join().unwrap_or(false)
}

fn unavailable_gtk_keyboard_background(
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
enum KeyRoute {
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
fn background_virtual_key(
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
fn background_virtual_text(
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

fn is_popup_keyboard_grab(error: &anyhow::Error) -> bool {
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
fn background_key_route(
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
const GRAB_FOCUS_RETRY: &str = "the app would not move its focus to that element; to send \
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

/// Merge a background focus-guard report into a finished tool result: the
/// structured `focus_changed` / `focus_restored` / `grab_held_by` fields plus
/// one sentence of text when anything moved.
fn attach_focus_guard(
    result: ToolResult,
    report: Option<&crate::input::FocusGuardReport>,
) -> ToolResult {
    attach_focus_guard_opts(result, report, true)
}

/// [`attach_focus_guard`] for a result that already names the open popup
/// (`describe_popup: false` leaves the guard's popup sentence out).
fn attach_focus_guard_opts(
    mut result: ToolResult,
    report: Option<&crate::input::FocusGuardReport>,
    describe_popup: bool,
) -> ToolResult {
    let Some(report) = report else {
        return result;
    };
    let summary = report.summary_with_popup(describe_popup);
    if !summary.is_empty() {
        result.content.push(pincer_core::protocol::Content::text(
            summary.trim().to_owned(),
        ));
    }
    let mut structured = result
        .structured_content
        .take()
        .unwrap_or_else(|| json!({}));
    for (key, value) in report.to_json().as_object().into_iter().flatten() {
        structured[key] = value.clone();
    }
    if let Some(item) = report.evidence_item() {
        push_evidence(&mut structured, item);
    }
    result.structured_content = Some(structured);
    result
}

/// After a pointer press: name the grid cell that holds the widget focus now
/// (LibreOffice reports "D2"), so a one-row miss shows in the click result
/// instead of after six typed values. Read from the focus-event log only,
/// so it costs nothing when the press did not land on a grid.
async fn attach_focused_cell(result: ToolResult, pid: u32) -> ToolResult {
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
fn with_focused_cell(mut result: ToolResult, role: &str, name: &str) -> ToolResult {
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

/// Append an `evidence[]` item to a structured payload (the public action
/// contract keeps `evidence`, while flat diagnostic fields are reduced away).
fn push_evidence(structured: &mut Value, item: Value) {
    match structured.get_mut("evidence").and_then(Value::as_array_mut) {
        Some(items) => items.push(item),
        None => structured["evidence"] = json!([item]),
    }
}

/// Tool result for a completed keyboard route (`press_key` / `hotkey`). The
/// text is written after the effect is decided, so it never says "not
/// verified" next to an `effect: confirmed` line.
fn key_route_result(action: &str, route: KeyRoute, mode_label: &str) -> ToolResult {
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
const KEY_EFFECT_UNVERIFIED: &str =
    "not verified: read what changed with get_window_state since:\"latest\", or check it \
     with verify_state";

/// Text tail for a foreground key delivery, in step with the effect
/// [`apply_foreground_report`] decides: the window change that confirms it,
/// the focus that left the target (a suspected no-op), or a request to check.
fn foreground_key_tail(report: &crate::input::ForegroundReport) -> String {
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

fn unavailable_gtk_pointer_background(
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
fn unavailable_held_button_background(
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

fn unavailable_wayland_focused_input_background(
    delivery: crate::input::delivery::DeliveryMode,
    focus_free_inject_supported: bool,
) -> Option<ToolResult> {
    (crate::wayland::wayland_input_enabled()
        && !(focus_free_inject_supported && crate::wayland::is_inject_mode())
        && !delivery.is_foreground())
    .then(|| {
        crate::input::delivery::background_unavailable_error(
            crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
        )
    })
}

#[derive(Debug)]
enum CoordinateContext {
    Zoom(ZoomContext),
    Screenshot(f64),
}

fn coordinate_click_context(
    native_refusal: Option<ToolResult>,
    resolve_context: impl FnOnce() -> Result<CoordinateContext, ToolResult>,
) -> Result<CoordinateContext, ToolResult> {
    if let Some(refusal) = native_refusal {
        return Err(refusal);
    }
    resolve_context()
}

fn coordinate_drag_context(
    native_refusal: Option<ToolResult>,
    resolve_context: impl FnOnce() -> Result<CoordinateContext, ToolResult>,
) -> Result<CoordinateContext, ToolResult> {
    if let Some(refusal) = native_refusal {
        return Err(refusal);
    }
    resolve_context()
}

/// Pixel scroll shares click and drag's refusal order: an unsupported native
/// background delivery must report `background_unavailable` before the
/// screenshot frame is consulted, because native Wayland may legitimately
/// withhold the screenshot (`surface_identity_unproven`) that the frame needs.
fn coordinate_scroll_scale(
    native_refusal: Option<ToolResult>,
    resolve_scale: impl FnOnce() -> Result<f64, ToolResult>,
) -> Result<f64, ToolResult> {
    if let Some(refusal) = native_refusal {
        return Err(refusal);
    }
    resolve_scale()
}

/// Chromium's X11 renderer drops synthetic input sent to an occluded,
/// unfocused toplevel. Returning success here would be a silent loss, so all
/// input tools expose the same typed refusal and leave foreground activation
/// as the explicit escalation.
fn unavailable_chromium_background(
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

fn chromium_background_must_refuse(
    foreground: bool,
    focus_free_inject_mode: bool,
    chromium: bool,
) -> bool {
    chromium && !foreground && !focus_free_inject_mode
}

/// Screen-absolute center of a window (top-left from translate_coordinates plus
/// half its geometry). Used to position the no-focus-steal scroll over the
/// window's content. Blocking — call inside spawn_blocking.
fn window_screen_center(xid: u64) -> anyhow::Result<(i32, i32)> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let (conn, screen_num) = crate::x11::query_connection()?;
    let root = conn.setup().roots[screen_num].root;
    let geom = conn.get_geometry(xid as u32)?.reply()?;
    let trans = conn
        .translate_coordinates(xid as u32, root, 0, 0)?
        .reply()?;
    Ok((
        trans.dst_x as i32 + geom.width as i32 / 2,
        trans.dst_y as i32 + geom.height as i32 / 2,
    ))
}

/// How a background pointer action was delivered on X11, with the cheap
/// post-checks the real-pointer route can make.
#[derive(Debug)]
enum PointerRoute {
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
    /// Native Wayland pointer route.
    Wayland,
}

impl PointerRoute {
    /// Result `path`; the Wayland routes keep their own tool-level labels.
    fn path(&self) -> Option<&'static str> {
        match self {
            Self::Mpx(_) => Some(crate::input::MPX_POINTER_PATH),
            Self::Atspi(_) => Some("x11_atspi"),
            Self::Synthetic(_) => Some("x11_xsendevent"),
            Self::Foreground | Self::ForegroundPopups(_) => Some("x11_xtest_fg"),
            Self::Wayland => None,
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
    fn structured(&self, mode_label: &str) -> Value {
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
    fn text_suffix(&self, mode_label: &str) -> String {
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
struct BackgroundPointerFailed {
    reason: String,
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
fn with_pixel_points(
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
fn x11_pixel_click_no_focus_steal(
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

/// Error prefix and result code for an element token whose object or window
/// is gone.
const STALE_ELEMENT_TOKEN: &str = "stale_element_token";

/// Typed tool error for a Linux input failure: uinput permission, an occluded
/// target, or a real-pointer delivery that did not go through. Everything
/// else keeps its full error chain in the text.
fn linux_input_error(error: anyhow::Error) -> ToolResult {
    if crate::input::is_uinput_unavailable(&error) {
        return ToolResult::error(error.to_string()).with_structured(json!({
            "code": crate::input::UINPUT_UNAVAILABLE_CODE,
        }));
    }
    if let Some(occluded) = error.downcast_ref::<crate::input::TargetOccluded>() {
        let hint = format!(
            "The point is under window {}{}{}, which belongs to a different application, so \
             a pointer press there would hit that window instead of the target; \
             no input was sent. Either act on that window (pass its window_id{} and click / \
             press keys there, or dismiss it), click the target by element_token (an AT-SPI \
             action that needs no pointer), or pick a point of the target that is not \
             covered (list_windows gives bounds and z_index).",
            occluded.covering_window,
            if occluded.covering_title.is_empty() {
                String::new()
            } else {
                // App text inside Pincer's advice: escaped, so it cannot
                // close the quote or spell a retry instruction.
                format!(" {}", quoted_title(&occluded.covering_title))
            },
            occluded
                .covering_pid
                .map(|pid| format!(" of pid {pid}"))
                .unwrap_or_default(),
            occluded
                .covering_pid
                .map(|pid| format!(" with pid {pid}"))
                .unwrap_or_default(),
        );
        return ToolResult::error(format!("{occluded}. {hint}")).with_structured(json!({
            "code": "target_occluded",
            "effect": "refused",
            "path": crate::input::MPX_POINTER_PATH,
            "target_window": occluded.target_window,
            "covering_window": occluded.covering_window,
            "covering_title": occluded.covering_title,
            "covering_pid": occluded.covering_pid,
            "screen_point": [occluded.x, occluded.y],
            "hint": hint,
        }));
    }
    if let Some(grab) = error.downcast_ref::<crate::input::PopupKeyboardGrab>() {
        let hint = format!(
            "Keys for pid {} cannot be delivered while its {} is open: the popup's keyboard \
             grab drops keys from the virtual keyboard, and the core keyboard would go to the \
             focused application instead. Dismiss the popup first (click outside it, or \
             click an item in it with pid/window_id={}), or click the popup's item by \
             element_token after get_window_state(pid={}, window_id={}); then retry.",
            grab.pid,
            grab.popup.describe(),
            grab.popup.window,
            grab.pid,
            grab.popup.window
        );
        return ToolResult::error(format!("{grab}. {hint}")).with_structured(json!({
            "code": "popup_keyboard_grab",
            "effect": "refused",
            "verified": false,
            "popup": grab.popup.to_json(),
            "focus_owner_pid": grab.focus_owner,
            "hint": hint,
        }));
    }
    if let Some(outside) = error.downcast_ref::<crate::input::PointOutsideWindow>() {
        let (bx, by, bw, bh) = outside.bounds;
        let hint = format!(
            "x/y are window-local pixels of window {} (0..{bw} x 0..{bh}, its top-left is at \
             screen ({bx}, {by})). Pass window-local coordinates from get_window_state's \
             screenshot, or screen pixels with coordinate_frame:\"desktop\".",
            outside.target_window
        );
        return ToolResult::error(format!("{outside}. {hint}")).with_structured(json!({
            "code": "point_outside_window",
            "effect": "refused",
            "path": crate::input::MPX_POINTER_PATH,
            "target_window": outside.target_window,
            "screen_point": [outside.screen_x, outside.screen_y],
            "window_point": [outside.window_x, outside.window_y],
            "window_bounds": { "x": bx, "y": by, "width": bw, "height": bh },
            "hint": hint,
        }));
    }
    if let Some(closed) = error.downcast_ref::<crate::atspi::native::WindowClosed>() {
        return ToolResult::error(closed.to_string()).with_structured(json!({
            "code": STALE_ELEMENT_TOKEN,
            "effect": "none",
            "window_id": closed.xid,
        }));
    }
    if let Some(unusable) = error.downcast_ref::<ElementBoundsUnusable>() {
        let hint = "Re-snapshot with get_window_state and click by pixel (x/y) from the \
             screenshot, or pick a child element that reports a frame.";
        return ToolResult::error(format!("{unusable}. {hint}")).with_structured(json!({
            "code": "element_bounds_unavailable",
            "effect": "none",
            "element_index": unusable.idx,
            "hint": hint,
        }));
    }
    if let Some(failed) = error.downcast_ref::<BackgroundPointerFailed>() {
        let hint = "Click by element_token (AT-SPI action) or retry with \
             delivery_mode:\"foreground\".";
        return ToolResult::error(format!("{failed}. {hint}")).with_structured(json!({
            "code": "background_pointer_failed",
            "effect": "none",
            "path": crate::input::MPX_POINTER_PATH,
            "reason": failed.reason,
            "hint": hint,
        }));
    }
    ToolResult::error(format!("{error:#}"))
}

fn isolated_hyprland_background(delivery: crate::input::delivery::DeliveryMode) -> bool {
    !delivery.is_foreground() && crate::wayland::hyprland_input::enabled()
}

fn hyprland_foreground(delivery: crate::input::delivery::DeliveryMode) -> bool {
    delivery.is_foreground()
        && crate::wayland::wayland_input_enabled()
        && crate::wayland::hyprland::is_session()
}

/// AT-SPI roles whose `activate`/`press` action does not move the widget
/// focus into them, so a following keystroke would land elsewhere; a real
/// pointer click at their centre is what a user does.
fn element_needs_real_click(role: &str) -> bool {
    crate::atspi::is_focus_taking_role(role)
}

/// Menubar entries and the items of an open menu.
fn element_is_menu_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "menu" | "menu item" | "check menu item" | "radio menu item"
    )
}

/// A chord the window manager, not the application, acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WmChord {
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
fn wm_chord_kind(key: &str, modifiers: &[String]) -> Option<WmChord> {
    let mods: Vec<String> = modifiers
        .iter()
        .map(|m| m.trim().to_ascii_lowercase())
        .collect();
    let has = |names: &[&str]| mods.iter().any(|m| names.contains(&m.as_str()));
    let alt = has(&["alt", "alt_l", "alt_r", "option"]);
    let ctrl = has(&["ctrl", "control", "ctrl_l", "ctrl_r"]);
    let shift = has(&["shift", "shift_l", "shift_r"]);
    let super_ = has(&[
        "super", "super_l", "super_r", "meta", "win", "cmd", "command", "hyper",
    ]);
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
async fn wm_chord_background(
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
fn close_window_result(
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
fn split_key_combo(key: &str) -> (Vec<String>, String) {
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

fn element_click_prefers_ax(
    hyprland_foreground: bool,
    button: u8,
    count: usize,
    has_modifiers: bool,
) -> bool {
    !has_modifiers && (!hyprland_foreground || (button == 1 && count == 1))
}

fn foreground_ax_click_unknown(pid: u32, idx: usize) -> ToolResult {
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

fn isolated_hyprland_refusal(detail: impl Into<String>) -> ToolResult {
    let detail = detail.into();
    ToolResult::error(format!("background_unavailable: {detail}")).with_structured(json!({
        "ok": false, "code": "background_unavailable", "reason": "unsupported_operation",
        "detail": detail, "route": "synthetic_events", "verified": false, "effect": "refused"
    }))
}

fn isolated_hyprland_result(result: anyhow::Result<Value>) -> ToolResult {
    hyprland_input_result(result, false)
}

fn hyprland_input_result(result: anyhow::Result<Value>, foreground: bool) -> ToolResult {
    use pincer_core::action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
    };
    let record = |effect| {
        ActionExecutionRecord::builder(
            effect,
            if foreground {
                ActionTransport::LinuxHyprlandForegroundInput
            } else {
                ActionTransport::LinuxHyprlandIsolatedInput
            },
            if foreground {
                RequestedDelivery::Foreground
            } else {
                RequestedDelivery::Background
            },
        )
    };
    let actual = if foreground {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Background
    };
    let unavailable = if foreground {
        "foreground_unavailable"
    } else {
        "background_unavailable"
    };
    match result {
        Ok(value) if value["ok"] == true => ToolResult::text(
            "Dispatched exact-target Hyprland input; application effect is unverifiable.",
        )
        .with_action_record(
            record(ActionEffect::Unverifiable)
                .actual_delivery(actual)
                .build()
                .expect("isolated input record is valid"),
        ),
        Ok(mut value) => {
            if foreground && value["code"] == "foreground_partial_unknown" {
                return hyprland_input_result(
                    Err(crate::wayland::hyprland_input::unknown_dispatch(
                        anyhow::anyhow!(
                            "{}",
                            value["detail"]
                                .as_str()
                                .unwrap_or("foreground acquisition may have changed focus")
                        ),
                        value["delivery"]["delivered_count"]
                            .as_u64()
                            .and_then(|count| u32::try_from(count).ok())
                            .unwrap_or(0),
                    )),
                    true,
                );
            }
            let code = value["code"]
                .as_str()
                .unwrap_or("protocol_error")
                .to_owned();
            let detail = value["detail"]
                .as_str()
                .unwrap_or("input refused")
                .to_owned();
            value["reason"] = json!(code);
            value["code"] = json!(unavailable);
            value["route"] = json!(if foreground {
                "global_input"
            } else {
                "synthetic_events"
            });
            value["verified"] = json!(false);
            let delivered = value["delivery"]["delivered_count"]
                .as_u64()
                .and_then(|count| u32::try_from(count).ok())
                .filter(|count| *count > 0);
            let outcome = if value["effect"] == "partial"
                && delivered.is_some_and(|count| foreground || count == 1)
            {
                record(ActionEffect::Partial)
                    .actual_delivery(actual)
                    .delivered_count(delivered.unwrap())
            } else {
                value["effect"] = json!("refused");
                value
                    .as_object_mut()
                    .expect("protocol reply is an object")
                    .remove("delivery");
                record(ActionEffect::Refused)
            }
            .detail(&detail)
            .build()
            .expect("isolated input outcome is valid");
            ToolResult::error(format!("{unavailable} ({code}): {detail}"))
                .with_structured(value)
                .with_action_record(outcome)
        }
        Err(error) if error.is::<crate::wayland::hyprland_input::LaneBusy>() => {
            hyprland_input_result(
                Ok(json!({
                    "ok": false, "code": "lane_busy", "detail": error.to_string()
                })),
                foreground,
            )
        }
        Err(error) if error.is::<crate::wayland::hyprland_input::ActionCancelled>() => {
            hyprland_input_result(
                Ok(json!({
                    "ok": false, "code": "cancelled", "detail": error.to_string()
                })),
                foreground,
            )
        }
        Err(error) => {
            let count = error
                .downcast_ref::<crate::wayland::hyprland_input::DispatchUnknown>()
                .map(|error| error.acknowledged_phases)
                .filter(|count| *count > 0);
            let effect = if count.is_some() {
                ActionEffect::Partial
            } else {
                ActionEffect::Unverifiable
            };
            let mut outcome = record(effect)
                .actual_delivery(ActualDelivery::Unknown)
                .detail(error.to_string());
            if let Some(count) = count {
                outcome = outcome.delivered_count(count);
            }
            let outcome = outcome
                .build()
                .expect("unknown delivery preserves acknowledged progress");
            let mut value = serde_json::to_value(outcome.public_result().unwrap()).unwrap();
            value["ok"] = json!(false);
            value["code"] = json!(unavailable);
            value["reason"] = json!("transport_or_protocol_error");
            value["detail"] = json!(error.to_string());
            ToolResult::error(format!("{unavailable}: {error}"))
                .with_structured(value)
                .with_action_record(outcome)
        }
    }
}

fn isolated_hyprland_task_error(error: tokio::task::JoinError, acknowledged: bool) -> ToolResult {
    isolated_hyprland_result(Err(crate::wayland::hyprland_input::unknown_dispatch(
        error.into(),
        u32::from(acknowledged),
    )))
}

fn spawn_isolated_hyprland(
    args: &Value,
    work: impl FnOnce(crate::wayland::hyprland_input::ActionCancellation) -> anyhow::Result<Value>
        + Send
        + 'static,
) -> Result<
    (
        crate::wayland::hyprland_input::CancelOnDrop,
        tokio::task::JoinHandle<anyhow::Result<Value>>,
    ),
    ToolResult,
> {
    use pincer_core::session;
    let owner = named_session_cursor_key(args)
        .ok_or_else(|| isolated_hyprland_refusal("authenticated lifecycle required"))?;
    let transport_owner = args
        .get("_transport_session_id")
        .and_then(Value::as_str)
        .unwrap_or(&owner);
    let snapshot = session::session_snapshot(&owner, transport_owner, std::time::Duration::ZERO)
        .ok_or_else(|| isolated_hyprland_refusal("admitted lifecycle required"))?;
    // Retain admission before spawning: aborting the async caller must not
    // complete session teardown while its native worker still owns input.
    let lifecycle = session::begin_session_dispatch(
        &owner,
        snapshot.public_label.as_deref(),
        transport_owner,
        snapshot.implicit,
        snapshot.transport,
        snapshot.client_kind,
    )
    .map_err(isolated_hyprland_refusal)?;
    let (guard, cancellation) = crate::wayland::hyprland_input::ActionCancellation::invocation();
    let dispatch = tokio::task::spawn_blocking(move || {
        let _lifecycle = lifecycle;
        work(cancellation)
    });
    Ok((guard, dispatch))
}

async fn isolated_hyprland_action(
    args: &Value,
    pid: u32,
    xid: u64,
    action: crate::wayland::hyprland_input::Action,
) -> ToolResult {
    let owner = named_session_cursor_key(args);
    let (_cancellation, dispatch) = match spawn_isolated_hyprland(args, move |cancellation| {
        crate::wayland::hyprland_input::execute(owner, pid, xid, action, cancellation)
    }) {
        Ok(dispatch) => dispatch,
        Err(refusal) => return refusal,
    };
    match dispatch.await {
        Ok(result) => isolated_hyprland_result(result),
        Err(error) => isolated_hyprland_task_error(error, false),
    }
}

fn foreground_hyprland_refusal(detail: impl Into<String>) -> ToolResult {
    hyprland_input_result(
        Ok(json!({
            "ok": false, "code": "unsupported_operation", "detail": detail.into()
        })),
        true,
    )
}

async fn foreground_hyprland_action(
    args: &Value,
    pid: u32,
    xid: u64,
    action: crate::wayland::hyprland_input::Action,
) -> ToolResult {
    if !crate::wayland::hyprland_input::enabled() {
        return foreground_hyprland_refusal("production Hyprland input plugin is unavailable");
    }
    let owner = named_session_cursor_key(args);
    let (_cancellation, dispatch) = match spawn_isolated_hyprland(args, move |cancellation| {
        crate::wayland::hyprland_input::execute_foreground(owner, pid, xid, action, cancellation)
    }) {
        Ok(dispatch) => dispatch,
        Err(_) => return foreground_hyprland_refusal("authenticated admitted lifecycle required"),
    };
    hyprland_input_result(
        match dispatch.await {
            Ok(result) => result,
            Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                error.into(),
                0,
            )),
        },
        true,
    )
}

async fn foreground_hyprland_scroll(
    args: &Value,
    pid: u32,
    xid: u64,
    actions: Vec<crate::wayland::hyprland_input::Action>,
) -> ToolResult {
    if !crate::wayland::hyprland_input::enabled() {
        return foreground_hyprland_refusal("production Hyprland input plugin is unavailable");
    }
    let owner = named_session_cursor_key(args);
    let (_cancellation, dispatch) = match spawn_isolated_hyprland(args, move |cancellation| {
        crate::wayland::hyprland_input::execute_foreground_scroll(
            owner,
            pid,
            xid,
            actions,
            cancellation,
        )
    }) {
        Ok(dispatch) => dispatch,
        Err(_) => return foreground_hyprland_refusal("authenticated admitted lifecycle required"),
    };
    hyprland_input_result(
        match dispatch.await {
            Ok(result) => result,
            Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                error.into(),
                0,
            )),
        },
        true,
    )
}

fn foreground_hyprland_key(
    key: String,
    mut modifiers: Vec<String>,
) -> Result<crate::wayland::hyprland_input::Action, ToolResult> {
    let parts: Vec<&str> = key.split('+').collect();
    let key = if parts.len() > 1 && key != "+" {
        if parts[..parts.len() - 1]
            .iter()
            .any(|part| !is_modifier(part))
            || parts.last() == Some(&"")
        {
            return Err(foreground_hyprland_refusal("invalid modified key chord"));
        }
        modifiers.extend(
            parts[..parts.len() - 1]
                .iter()
                .map(|part| part.to_lowercase()),
        );
        parts.last().unwrap().to_string()
    } else {
        key
    };
    Ok(crate::wayland::hyprland_input::Action::Key { key, modifiers })
}

async fn focus_hyprland_foreground(
    state: &Arc<ToolState>,
    args: &Value,
    pid: u32,
    xid: u64,
    element: Option<usize>,
    pixel: Option<(f64, f64)>,
) -> Result<(), ToolResult> {
    if !crate::wayland::hyprland_input::enabled() {
        return Err(foreground_hyprland_refusal(
            "production Hyprland input plugin is unavailable",
        ));
    }
    if let Some((x, y)) = pixel {
        return focus_by_pixel(
            state,
            pid,
            Some(xid),
            (x, y),
            true,
            args,
            args.bool_or("from_zoom", false),
        )
        .await;
    }
    if let Some(index) = element {
        let activation = foreground_hyprland_action(
            args,
            pid,
            xid,
            crate::wayland::hyprland_input::Action::Activate,
        )
        .await;
        if activation.is_error == Some(true) {
            return Err(activation);
        }
        match tokio::task::spawn_blocking(move || crate::atspi::focus_element(pid, xid, index))
            .await
        {
            Ok(Ok(true)) => {}
            _ => return Err(foreground_hyprland_refusal("AT-SPI child focus failed")),
        }
    }
    Ok(())
}

#[cfg(test)]
#[tokio::test]
async fn isolated_worker_retains_lifecycle_until_native_work_exits() {
    use pincer_core::session::{self, SessionClientKind, SessionTransport};
    let sid = "isolated-worker-lifecycle-test";
    let owner = "isolated-worker-transport-test";
    let caller = session::begin_session_dispatch(
        sid,
        None,
        owner,
        true,
        SessionTransport::McpStdio,
        SessionClientKind::Mcp,
    )
    .unwrap();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (cancellation, dispatch) = spawn_isolated_hyprland(
        &json!({"_session_id":sid,"_transport_session_id":owner}),
        move |_| {
            started.send(()).unwrap();
            released.recv().unwrap();
            Ok(json!({"ok":true}))
        },
    )
    .unwrap();
    ready.await.unwrap();
    drop(cancellation);
    drop(caller);
    assert!(session::end_session_for_owner(sid, owner));
    assert!(session::is_session_ending(sid));
    assert!(!session::is_session_ended(sid));
    release.send(()).unwrap();
    dispatch.await.unwrap().unwrap();
    assert!(session::is_session_ended(sid));
}

#[cfg(test)]
#[test]
fn isolated_background_routes_do_not_reprobe_availability_before_primary_fallback() {
    // Structural regression for the availability-loss race: each tool keeps
    // the decision that bypassed background refusal until its returning
    // native branch. A second probe here could select primary-seat input.
    let source = include_str!("impl_.rs");
    for (start, end) in [
        ("impl Tool for ClickTool {", "impl Tool for TypeTextTool {"),
        (
            "impl Tool for ScrollTool {",
            "impl Tool for DoubleClickTool {",
        ),
        (
            "impl Tool for DragTool {",
            "impl Tool for MouseButtonDownTool {",
        ),
    ] {
        let body = source
            .rsplit_once(start)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0;
        let invoke = body.split_once("async fn invoke").unwrap().1;
        assert_eq!(
            invoke
                .matches("isolated_hyprland_background(delivery)")
                .count(),
            1,
            "{start}"
        );
        let native = invoke.split_once("if isolated_background {").unwrap().1;
        assert!(native.contains("return match dispatch.await"), "{start}");
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

#[cfg(test)]
#[test]
fn type_text_routes_isolated_background_before_generic_wayland_refusal() {
    let source = include_str!("impl_.rs");
    let invoke = source
        .rsplit_once("impl Tool for TypeTextTool {")
        .unwrap()
        .1
        .split_once("impl Tool for PressKeyTool {")
        .unwrap()
        .0
        .split_once("async fn invoke")
        .unwrap()
        .1;
    let isolated = invoke
        .find("isolated_hyprland_background(delivery)")
        .expect("type_text must select isolated Hyprland background delivery");
    let generic = invoke
        .find("unavailable_wayland_focused_input_background")
        .expect("type_text must retain the generic Wayland refusal");
    assert!(
        isolated < generic,
        "the plugin route must run before the generic focused-input refusal"
    );
    assert!(invoke.contains("execute_background_text"));
}

#[cfg(test)]
#[test]
fn coordinate_click_keeps_wayland_background_refusal_ahead_of_screenshot_context() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let resolved = AtomicBool::new(false);
    let native_refusal = crate::input::delivery::background_unavailable_error(
        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
    );
    let result = coordinate_click_context(Some(native_refusal), || {
        resolved.store(true, Ordering::SeqCst);
        Ok(CoordinateContext::Screenshot(7.35))
    })
    .expect_err("unsupported Wayland background delivery must refuse");

    assert_eq!(
        result.structured_content.as_ref().unwrap()["code"],
        "background_unavailable"
    );
    assert!(
        !resolved.load(Ordering::SeqCst),
        "snapshot context must not be consulted before the native refusal"
    );
}

#[cfg(test)]
#[test]
fn coordinate_drag_keeps_wayland_background_refusal_ahead_of_screenshot_context() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let resolved = AtomicBool::new(false);
    let native_refusal = crate::input::delivery::background_unavailable_error(
        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
    );
    let result = coordinate_drag_context(Some(native_refusal), || {
        resolved.store(true, Ordering::SeqCst);
        Ok(CoordinateContext::Screenshot(7.35))
    })
    .expect_err("unsupported Wayland background delivery must refuse drag");

    assert_eq!(
        result.structured_content.as_ref().unwrap()["code"],
        "background_unavailable"
    );
    assert!(
        !resolved.load(Ordering::SeqCst),
        "drag snapshot context must not be consulted before the native refusal"
    );
}

#[cfg(test)]
#[test]
fn coordinate_scroll_keeps_wayland_background_refusal_ahead_of_screenshot_context() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let resolved = AtomicBool::new(false);
    let native_refusal = crate::input::delivery::background_unavailable_error(
        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
    );
    let result = coordinate_scroll_scale(Some(native_refusal), || {
        resolved.store(true, Ordering::SeqCst);
        Err(ToolResult::error(
            "screenshot context must not be consulted",
        ))
    })
    .expect_err("unsupported Wayland background delivery must refuse pixel scroll");

    assert_eq!(
        result.structured_content.as_ref().unwrap()["code"],
        "background_unavailable"
    );
    assert!(
        !resolved.load(Ordering::SeqCst),
        "scroll snapshot context must not be consulted before the native refusal"
    );
    assert_eq!(coordinate_scroll_scale(None, || Ok(1.5)).unwrap(), 1.5);

    let source = include_str!("impl_.rs");
    let invoke = source
        .rsplit_once("impl Tool for ScrollTool {")
        .unwrap()
        .1
        .split_once("async fn invoke")
        .unwrap()
        .1;
    let (before_scale, _) = invoke
        .split_once("coordinate_scroll_scale(native_refusal")
        .expect("pixel scroll must resolve its frame through the refusal-first helper");
    assert!(
        before_scale.contains("unavailable_wayland_focused_input_background(delivery, true)"),
        "pixel scroll must compute the native Wayland refusal before its frame"
    );
    assert!(
        !before_scale.contains("screenshot_scale("),
        "pixel scroll must not consult the screenshot frame before the native refusal"
    );
}

#[cfg(test)]
#[test]
fn coordinate_less_mouse_button_up_survives_snapshot_replacement() {
    let state = ToolState::new();
    let cursor_id = "held-after-replacement";
    let hold = MouseHoldState {
        pid: 42,
        xid: 7,
        button: 1,
        x: 120.0,
        y: 80.0,
        foreground: false,
    };
    state.snapshots.publish_for_session(
        hold.pid as i32,
        hold.xid,
        crate::atspi::snapshot::AtspiSnapshot::from_nodes(&[]),
        Some("press-owner"),
        Some(2.0),
    );
    state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(cursor_id.to_owned(), hold.clone());

    state.snapshots.publish_for_session(
        hold.pid as i32,
        hold.xid,
        crate::atspi::snapshot::AtspiSnapshot::from_nodes(&[]),
        Some("replacement-owner"),
        Some(1.0),
    );

    let release_args = json!({"_session_id": "press-owner"});
    assert_eq!(
        mouse_button_up_coordinates(&state, &release_args, &hold).unwrap(),
        (hold.x, hold.y),
        "coordinate-less release must use the stored native hold point"
    );

    let stale_coordinate_args = json!({"_session_id": "press-owner", "x": 60.0, "y": 40.0});
    let refusal = mouse_button_up_coordinates(&state, &stale_coordinate_args, &hold)
        .expect_err("coordinate-bearing release must still reject a replaced snapshot");
    assert_eq!(
        refusal.structured_content.as_ref().unwrap()["code"],
        "screenshot_context_missing"
    );

    clear_mouse_hold_after_release(&state, cursor_id);
    assert!(state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(cursor_id.to_owned(), hold)
        .is_none());
}

#[cfg(test)]
#[test]
fn persistent_wayland_pointer_coordinates_include_window_origin() {
    let output = persistent_pointer_coordinates(true, 7, 10, 20, |window_id, x, y| {
        assert_eq!(window_id, 7);
        (x + 100, y + 200)
    });
    assert_eq!(output, (110, 220));

    let x11 = persistent_pointer_coordinates(false, 7, 10, 20, |_, _, _| {
        panic!("X11 coordinates must remain window-local")
    });
    assert_eq!(x11, (10, 20));
}

#[cfg(test)]
#[test]
fn session_end_releases_only_its_hold_before_discarding_state() {
    let state = ToolState::new();
    let session_id = "ending-session";
    let hold = MouseHoldState {
        pid: 42,
        xid: 7,
        button: 1,
        x: 10.0,
        y: 20.0,
        foreground: false,
    };
    let other_hold = MouseHoldState {
        pid: 43,
        xid: 8,
        button: 3,
        x: 30.0,
        y: 40.0,
        foreground: false,
    };
    {
        let mut holds = state.mouse_hold.lock().unwrap();
        holds.insert(session_id.to_owned(), hold.clone());
        holds.insert("other-session".to_owned(), other_hold.clone());
    }

    let mut released = Vec::new();
    release_mouse_hold_for_session(&state, session_id, |cursor_id, held| {
        assert!(state.mouse_hold.lock().unwrap().contains_key(cursor_id));
        released.push((cursor_id.to_owned(), held.button));
        Ok(())
    })
    .unwrap();

    assert_eq!(released, vec![(session_id.to_owned(), hold.button)]);
    let mut holds = state.mouse_hold.lock().unwrap();
    assert!(!holds.contains_key(session_id));
    assert_eq!(
        holds.get("other-session").unwrap().button,
        other_hold.button
    );
    assert!(holds.insert(session_id.to_owned(), hold).is_none());
}

#[cfg(test)]
#[test]
fn failed_session_end_release_remains_retryable_and_bounded() {
    let state = ToolState::new();
    let session_id = "failed-release-session";
    let hold = MouseHoldState {
        pid: 42,
        xid: 7,
        button: 1,
        x: 10.0,
        y: 20.0,
        foreground: false,
    };
    state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(session_id.to_owned(), hold.clone());

    let mut attempts = 0;
    let failure = release_mouse_hold_for_session(&state, session_id, |_, _| {
        attempts += 1;
        anyhow::bail!("compositor disconnected")
    })
    .expect_err("a failed compositor release must fail session cleanup");
    assert!(failure.contains("compositor disconnected"));
    assert_eq!(
        attempts, 1,
        "one cleanup pass must make one bounded attempt"
    );
    assert_eq!(
        state
            .mouse_hold
            .lock()
            .unwrap()
            .get(session_id)
            .unwrap()
            .button,
        hold.button,
        "failed cleanup must retain the hold for the next session-end retry"
    );

    release_mouse_hold_for_session(&state, session_id, |cursor_id, held| {
        attempts += 1;
        assert_eq!(cursor_id, session_id);
        assert_eq!(held.button, hold.button);
        Ok(())
    })
    .unwrap();
    assert_eq!(attempts, 2);
    assert!(state
        .mouse_hold
        .lock()
        .unwrap()
        .insert(session_id.to_owned(), hold)
        .is_none());
}

#[cfg(test)]
#[test]
fn isolated_hyprland_refused_partial_and_unknown_outcomes_stay_distinct() {
    let busy = isolated_hyprland_result(Err(crate::wayland::hyprland_input::LaneBusy.into()));
    let content = busy.structured_content.as_ref().unwrap();
    assert_eq!(content["reason"], "lane_busy");
    assert_eq!(content["effect"], "refused");
    assert!(content.get("delivery").is_none());
    assert!(busy
        .action_record
        .unwrap()
        .public_result()
        .unwrap()
        .delivery
        .is_none());

    let refused = isolated_hyprland_result(Ok(
        json!({"ok":false,"code":"stale_target","detail":"stale_target"}),
    ));
    assert_eq!(
        refused.structured_content.as_ref().unwrap()["effect"],
        "refused"
    );
    let public = refused.action_record.unwrap().public_result().unwrap();
    assert!(public.delivery.is_none());
    public.validate_invariants().unwrap();

    let partial = isolated_hyprland_result(Ok(
        json!({"ok":false,"code":"cancelled","detail":"cancelled",
        "effect":"partial","delivery":{"mode":"background","delivered_count":1}}),
    ));
    let public = partial.action_record.unwrap().public_result().unwrap();
    assert_eq!(public.effect, pincer_contract::ActionEffect::Partial);
    assert_eq!(public.delivery.unwrap().delivered_count, Some(1));

    let unknown = isolated_hyprland_result(Err(anyhow::anyhow!("connection lost")));
    let public = unknown.action_record.unwrap().public_result().unwrap();
    assert_eq!(public.effect, pincer_contract::ActionEffect::Unverifiable);
    assert_eq!(
        public.delivery.as_ref().unwrap().mode,
        pincer_contract::ActionDeliveryMode::Unknown
    );
    assert_eq!(public.delivery.unwrap().delivered_count, None);

    let unknown_after_start = isolated_hyprland_result(Err(
        crate::wayland::hyprland_input::unknown_dispatch(anyhow::anyhow!("connection lost"), 1),
    ));
    let public = unknown_after_start
        .action_record
        .unwrap()
        .public_result()
        .unwrap();
    public.validate_invariants().unwrap();
    assert_eq!(public.effect, pincer_contract::ActionEffect::Partial);
    let delivery = public.delivery.unwrap();
    assert_eq!(delivery.mode, pincer_contract::ActionDeliveryMode::Unknown);
    assert_eq!(delivery.delivered_count, Some(1));
}

#[cfg(test)]
#[test]
fn experimental_pending_grant_is_an_error_with_operator_context() {
    let result = isolated_hyprland_result(Ok(json!({
        "ok": false, "code": "permission_required", "detail": "external approval pending",
        "epoch": "epoch", "challenge": "challenge", "target": "target", "revision": 3
    })));
    assert_eq!(result.is_error, Some(true));
    let value = result.structured_content.unwrap();
    assert_eq!(value["code"], "background_unavailable");
    assert_eq!(value["reason"], "permission_required");
    assert_eq!(value["challenge"], "challenge");
    assert_eq!(value["target"], "target");
    assert_eq!(value["revision"], 3);
    assert_eq!(value["verified"], false);
}

#[cfg(test)]
#[test]
fn experimental_dispatch_does_not_claim_application_success() {
    let result = isolated_hyprland_result(Ok(json!({
        "ok": true, "effect": "unverifiable", "route": "synthetic_events"
    })));
    assert_ne!(result.is_error, Some(true));
    let public = result.action_record.unwrap().public_result().unwrap();
    public.validate_invariants().unwrap();
    let value = serde_json::to_value(public).unwrap();
    assert_eq!(value["effect"], "unverifiable");
    assert_eq!(value["route"], "synthetic_events");
    assert!(value.get("verified").is_none());
}

#[cfg(test)]
#[test]
fn foreground_hyprland_reports_foreground_and_preserves_partial_progress() {
    use pincer_core::action_record::{ActionTransport, RequestedDelivery};
    for (reply, expected_effect, count) in [
        (json!({"ok":true}), "unverifiable", None),
        (
            json!({"ok":false,"code":"stale_target","detail":"stale"}),
            "refused",
            None,
        ),
        (
            json!({"ok":false,"code":"cancelled","detail":"cancelled","effect":"partial",
            "delivery":{"mode":"foreground","delivered_count":3}}),
            "partial",
            Some(3),
        ),
    ] {
        let result = hyprland_input_result(Ok(reply), true);
        let record = result.action_record.unwrap();
        assert_eq!(
            record.transport,
            ActionTransport::LinuxHyprlandForegroundInput
        );
        assert_eq!(record.requested_delivery, RequestedDelivery::Foreground);
        let public = record.public_result().unwrap();
        public.validate_invariants().unwrap();
        let value = serde_json::to_value(public).unwrap();
        assert_eq!(value["route"], "global_input");
        assert_eq!(value["effect"], expected_effect);
        if expected_effect == "refused" {
            assert!(value.get("delivery").is_none());
        } else {
            assert_eq!(value["delivery"]["mode"], "foreground");
            assert_eq!(value["delivery"]["delivered_count"].as_u64(), count);
        }
    }
    for reply in [
        Err(crate::wayland::hyprland_input::unknown_dispatch(
            anyhow::anyhow!("lost"),
            2,
        )),
        Ok(json!({"ok":false,"code":"foreground_partial_unknown","detail":"focus changed"})),
    ] {
        let unknown = hyprland_input_result(reply, true);
        let value =
            serde_json::to_value(unknown.action_record.unwrap().public_result().unwrap()).unwrap();
        assert_eq!(value["delivery"]["mode"], "unknown");
        assert_ne!(value["effect"], "refused");
    }
}

#[cfg(test)]
#[test]
fn foreground_hyprland_chords_preserve_all_modifiers() {
    let action = foreground_hyprland_key("Ctrl+Shift+H".into(), vec!["alt".into()]).unwrap();
    match action {
        crate::wayland::hyprland_input::Action::Key { key, modifiers } => {
            assert_eq!(key, "H");
            assert_eq!(modifiers, ["alt", "ctrl", "shift"]);
        }
        _ => panic!("expected a key chord"),
    }
    assert!(foreground_hyprland_key("bogus+H".into(), vec![]).is_err());
}

#[cfg(test)]
#[test]
fn uinput_failure_has_a_stable_structured_code() {
    let result = linux_input_error(crate::input::uinput_unavailable("synthetic failure"));
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("code"))
            .and_then(Value::as_str),
        Some(crate::input::UINPUT_UNAVAILABLE_CODE)
    );
}

fn x11_pixel_click_no_focus_steal_modifiers(
    xid: u64,
    lx: i32,
    ly: i32,
    button: u8,
    count: usize,
    modifiers: &[&str],
) -> anyhow::Result<()> {
    crate::input::send_click_with_modifiers(xid, lx, ly, count, button, modifiers)
}

fn mouse_button_name(button: u8) -> &'static str {
    match button {
        3 => "right",
        2 => "middle",
        _ => "left",
    }
}

fn resolve_cursor_key(args: &Value) -> String {
    pincer_core::tool_args::resolve_session_key(args)
}

/// Return the cursor key only for a lifecycle-owned session. Cursor positioning
/// for keyboard actions deliberately does not opt anonymous calls or the
/// legacy cursor_id-only path into session semantics. The proxy-minted
/// `_session_id` is trusted lifecycle state and must behave like a public named
/// session for cursor ownership.
fn named_session_cursor_key(args: &Value) -> Option<String> {
    cursor_overlay::named_session_cursor_key(args)
}

fn mouse_hold_json(cursor_id: &str, hold: Option<&MouseHoldState>) -> Value {
    match hold {
        Some(hold) => json!({
            "cursor_id": cursor_id,
            "held": true,
            "pid": hold.pid,
            "window_id": hold.xid,
            "button": mouse_button_name(hold.button),
            "x": hold.x,
            "y": hold.y,
            "delivery_mode": held_delivery_mode(hold.foreground),
        }),
        None => json!({
            "cursor_id": cursor_id,
            "held": false,
            "pid": Value::Null,
            "window_id": Value::Null,
            "button": Value::Null,
            "x": Value::Null,
            "y": Value::Null,
        }),
    }
}

fn held_target_mismatch(
    args: &Value,
    cursor_id: &str,
    hold: &MouseHoldState,
) -> Option<ToolResult> {
    match args.opt_u32("pid") {
        Ok(Some(pid)) if pid != hold.pid => {
            return Some(
                ToolResult::error(format!(
                    "Cursor '{cursor_id}' is holding a button for pid {}, not pid {pid}.",
                    hold.pid
                ))
                .with_structured(mouse_hold_json(cursor_id, Some(hold))),
            );
        }
        Err(err) => return Some(err.with_structured(mouse_hold_json(cursor_id, Some(hold)))),
        _ => {}
    }

    match args.opt_u64("window_id") {
        Some(xid) if xid != hold.xid => Some(
            ToolResult::error(format!(
                "Cursor '{cursor_id}' is holding a button for window_id {}, not {xid}.",
                hold.xid
            ))
            .with_structured(mouse_hold_json(cursor_id, Some(hold))),
        ),
        _ => None,
    }
}

fn held_delivery_mode(foreground: bool) -> &'static str {
    if foreground {
        "foreground"
    } else {
        "background"
    }
}

/// mouse_drag / mouse_button_up follow the mode the button was pressed with;
/// a `delivery_mode` naming the other one is refused rather than ignored.
fn held_delivery_mismatch(
    args: &Value,
    cursor_id: &str,
    hold: &MouseHoldState,
) -> Option<ToolResult> {
    let requested = args.get("delivery_mode").and_then(Value::as_str)?;
    let held = held_delivery_mode(hold.foreground);
    (requested != held).then(|| {
        let mut structured = mouse_hold_json(cursor_id, Some(hold));
        structured["code"] = json!("delivery_mode_mismatch");
        ToolResult::error(format!(
            "Cursor '{cursor_id}' holds its button with delivery_mode {held}, and mouse_drag \
             and mouse_button_up continue in that mode. Omit delivery_mode, or release with \
             mouse_button_up and press again with delivery_mode:\"{requested}\"."
        ))
        .with_structured(structured)
    })
}

/// `(delivery_mode=..., path=...)` for a held-button result on X11; empty on
/// native Wayland, whose persistent virtual pointer has no X11 path label.
fn held_route_note(foreground: bool) -> String {
    if crate::wayland::is_wayland() {
        String::new()
    } else if foreground {
        " (delivery_mode=foreground, path=x11_xtest_fg)".to_owned()
    } else {
        " (delivery_mode=background, path=x11_xsendevent)".to_owned()
    }
}

/// A held-button result with its `path` added on X11.
fn with_held_route(mut result: ToolResult, foreground: bool) -> ToolResult {
    if !crate::wayland::is_wayland() {
        if let Some(structured) = result.structured_content.as_mut() {
            structured["path"] = json!(if foreground {
                "x11_xtest_fg"
            } else {
                "x11_xsendevent"
            });
        }
    }
    result
}

/// An input error from a held-button call, typed like the other pointer
/// tools' (target_occluded, point_outside_window, foreground_unavailable),
/// with the cursor's held-button state added and `path` naming this route
/// (the shared occlusion refusal names the MPX one).
fn held_input_error(
    error: anyhow::Error,
    cursor_id: &str,
    hold: Option<&MouseHoldState>,
    foreground: bool,
) -> ToolResult {
    let mut result = with_held_route(input_error_result(error), foreground);
    let held = mouse_hold_json(cursor_id, hold);
    let structured = result.structured_content.get_or_insert_with(|| json!({}));
    for (key, value) in held.as_object().into_iter().flatten() {
        if structured.get(key).is_none() {
            structured[key] = value.clone();
        }
    }
    result
}

fn mouse_button_up_coordinates(
    state: &ToolState,
    args: &Value,
    hold: &MouseHoldState,
) -> Result<(f64, f64), ToolResult> {
    if args.get("x").is_none() && args.get("y").is_none() {
        return Ok((hold.x, hold.y));
    }

    let mut x = args.opt_f64("x").unwrap_or(hold.x);
    let mut y = args.opt_f64("y").unwrap_or(hold.y);
    if args.bool_or("from_zoom", false) {
        let context = state.zoom_context(args, hold.pid, Some(hold.xid))?;
        return Ok(context.zoom_to_window(x, y));
    }

    let ratio = screenshot_scale(state, args, hold.pid, Some(hold.xid))?;
    x *= ratio;
    y *= ratio;
    Ok((x, y))
}

fn clear_mouse_hold_after_release(state: &ToolState, cursor_id: &str) {
    state.mouse_hold.lock().unwrap().remove(cursor_id);
}

fn persistent_pointer_coordinates(
    is_wayland: bool,
    window_id: u64,
    x: i32,
    y: i32,
    window_local_to_output: impl FnOnce(u64, i32, i32) -> (i32, i32),
) -> (i32, i32) {
    if is_wayland {
        window_local_to_output(window_id, x, y)
    } else {
        (x, y)
    }
}

fn release_mouse_hold_for_session<Release>(
    state: &ToolState,
    session_id: &str,
    mut release: Release,
) -> Result<(), String>
where
    Release: FnMut(&str, &MouseHoldState) -> anyhow::Result<()>,
{
    let hold = state.mouse_hold.lock().unwrap().get(session_id).cloned();
    let Some(hold) = hold else {
        return Ok(());
    };

    release(session_id, &hold).map_err(|error| error.to_string())?;
    state.mouse_hold.lock().unwrap().remove(session_id);
    Ok(())
}

/// Put the agent cursor's tip on the pointer point `(sx, sy)` at once.
/// `SnapTo` places the cursor's anchor, so the point is converted for the
/// heading (the resting 45° pose when `None`).
fn overlay_snap_to_for(cursor_id: &str, sx: f64, sy: f64, heading: Option<f64>) {
    let heading = heading.unwrap_or(std::f64::consts::FRAC_PI_4);
    let (x, y) = cursor_overlay::anchor_for_pointer(sx, sy, heading);
    crate::overlay::send_command_for(
        cursor_id.to_owned(),
        cursor_overlay::OverlayCommand::SnapTo {
            x,
            y,
            heading_radians: Some(heading),
        },
    );
}

fn overlay_move_to_for(cursor_id: &str, sx: f64, sy: f64, heading: Option<f64>) {
    overlay_move_to_target_for(cursor_id, sx, sy, heading, None);
}

/// [`overlay_move_to_for`] with the targeted element's screen rect
/// `[x, y, width, height]` (same space as `sx`/`sy`).
fn overlay_move_to_target_for(
    cursor_id: &str,
    sx: f64,
    sy: f64,
    heading: Option<f64>,
    target: Option<[f64; 4]>,
) {
    crate::overlay::send_command_for(
        cursor_id.to_owned(),
        cursor_overlay::OverlayCommand::MoveTo {
            x: sx,
            y: sy,
            end_heading_radians: heading.unwrap_or(std::f64::consts::FRAC_PI_4),
            target,
        },
    );
}

async fn overlay_glide_to_for(cursor_id: &str, sx: f64, sy: f64) {
    overlay_glide_to_target_for(cursor_id, sx, sy, None).await;
}

/// [`overlay_glide_to_for`] with the targeted element's screen rect
/// `[x, y, width, height]` (same space as `sx`/`sy`); `None` for pixel actions.
async fn overlay_glide_to_target_for(cursor_id: &str, sx: f64, sy: f64, target: Option<[f64; 4]>) {
    // Input always revives its agent cursor. Hiding it is useful while idle,
    // but a hidden cursor must not make pointer or keyboard control invisible.
    crate::overlay::send_command_for(
        cursor_id.to_owned(),
        cursor_overlay::OverlayCommand::SetEnabled(true),
    );
    // Every Wayland backend owns its animation loop. Send one destination and
    // let the Linux overlay layer select the semantic helper, layer-shell, or
    // older helper fallback without an interpolated stream fighting it.
    if crate::wayland::is_wayland() {
        overlay_move_to_target_for(cursor_id, sx, sy, None, target);
        return;
    }
    let pos = crate::overlay::current_position_for(cursor_id);
    if pos.0 < 0.0 && pos.1 < 0.0 {
        crate::overlay::send_command_for(
            cursor_id.to_owned(),
            cursor_overlay::OverlayCommand::ClickPulse { x: sx, y: sy },
        );
        return;
    }
    crate::overlay::animate_cursor_to_target_for(cursor_id.to_owned(), sx, sy, target).await;
}

/// The compositor snapshot a desktop-scope action is addressed in. One
/// snapshot converts the action's points, places the overlay and sizes the
/// virtual pointer, so they cannot disagree about the monitor layout.
async fn desktop_input_space() -> anyhow::Result<crate::wayland::DesktopInputSpace> {
    if !crate::wayland::wayland_input_enabled() {
        return Ok(crate::wayland::DesktopInputSpace::default());
    }
    tokio::task::spawn_blocking(crate::wayland::DesktopInputSpace::current)
        .await
        .map_err(|error| anyhow::anyhow!("task error: {error}"))?
}

/// Keep the logical cursor position in sync with every visibly targeted
/// pointer action. Overlay delivery is intentionally best-effort: registry
/// state is still updated when no renderer is running or its queue is closed.
async fn reveal_pointer_action_for(
    state: &ToolState,
    cursor_id: &str,
    sx: f64,
    sy: f64,
    click_pulse: bool,
) {
    reveal_pointer_action_target_for(state, cursor_id, sx, sy, click_pulse, None).await;
}

/// [`reveal_pointer_action_for`] with the targeted element's screen rect
/// `[x, y, width, height]` (same space as `sx`/`sy`).
async fn reveal_pointer_action_target_for(
    state: &ToolState,
    cursor_id: &str,
    sx: f64,
    sy: f64,
    click_pulse: bool,
    target: Option<[f64; 4]>,
) {
    if !sx.is_finite() || !sy.is_finite() {
        return;
    }
    state.cursor_registry.set_enabled(cursor_id, true);
    state.cursor_registry.update_position(cursor_id, sx, sy);
    emit_cursor_hook(cursor_id, sx, sy, false);
    overlay_glide_to_target_for(cursor_id, sx, sy, target).await;
    if click_pulse {
        emit_cursor_hook(cursor_id, sx, sy, true);
        crate::overlay::send_command_for(
            cursor_id.to_owned(),
            cursor_overlay::OverlayCommand::ClickPulse { x: sx, y: sy },
        );
    }
}

/// Report a commanded cursor move (or a press) to the embedder's cursor
/// hook, with the same guards as macOS's `CursorRegistry::emit_cursor_event`:
/// nothing is built unless a hook is registered, and an ended session never
/// reaches it. Emitted from the logical cursor write path, so it works with
/// no overlay renderer (a headless Xvfb session).
pub(crate) fn emit_cursor_hook(cursor_id: &str, x: f64, y: f64, pressed: bool) {
    if !pincer_core::cursor_hook::cursor_hook_enabled() {
        return;
    }
    if cursor_id.is_empty() || pincer_core::session::is_session_ended(cursor_id) {
        return;
    }
    pincer_core::cursor_hook::push_cursor_event(pincer_core::cursor_hook::CursorHookEvent {
        cursor_id: cursor_id.to_owned(),
        x,
        y,
        pressed,
    });
}

fn keyboard_window_center(xid: u64) -> Option<(f64, f64)> {
    if xid == 0 {
        return None;
    }
    if crate::wayland::is_wayland() {
        return crate::wayland::window_geometry(xid).and_then(|(x, y, width, height)| {
            (width > 0 && height > 0).then_some((
                f64::from(x) + f64::from(width) / 2.0,
                f64::from(y) + f64::from(height) / 2.0,
            ))
        });
    }
    window_screen_center(xid)
        .ok()
        .map(|(x, y)| (f64::from(x), f64::from(y)))
}

fn current_pointer_position() -> Option<(f64, f64)> {
    if crate::wayland::is_wayland() {
        return crate::wayland::last_synth_cursor_pos().map(|(x, y)| (f64::from(x), f64::from(y)));
    }

    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    let (connection, screen_num) = crate::x11::query_connection().ok()?;
    let root = connection.setup().roots[screen_num].root;
    let reply = connection.query_pointer(root).ok()?.reply().ok()?;
    Some((f64::from(reply.root_x), f64::from(reply.root_y)))
}

fn explicit_keyboard_cursor_target(
    pid: u32,
    xid: u64,
    element_index: Option<usize>,
    pixel_target: Option<(f64, f64)>,
) -> Option<(f64, f64)> {
    if let Some(element_index) = element_index {
        let (sx, sy) = element_screen_center(pid, element_index, (xid != 0).then_some(xid)).ok()?;
        return Some((sx, sy));
    }

    let (x, y) = pixel_target?;
    if crate::wayland::wayland_input_enabled() {
        return crate::wayland::window_geometry(xid)
            .map(|(wx, wy, _, _)| (f64::from(wx) + x.round(), f64::from(wy) + y.round()));
    }
    window_local_to_screen(xid, x, y).ok()
}

/// Tell the cursor hook where an anonymous keyboard action types, before the
/// keystrokes: the element or pixel target, else the window's center. The
/// overlay is left alone (anonymous calls keep their legacy visuals); only an
/// embedder publishing presence learns the position, so a viewer sees the
/// agent's cursor go to the field it is about to type into.
async fn announce_keyboard_target(
    args: &Value,
    pid: u32,
    xid: u64,
    element_index: Option<usize>,
    pixel_target: Option<(f64, f64)>,
) {
    if !pincer_core::cursor_hook::cursor_hook_enabled() {
        return;
    }
    let cursor_id = resolve_cursor_key(args);
    let target = tokio::task::spawn_blocking(move || {
        explicit_keyboard_cursor_target(pid, xid, element_index, pixel_target)
            .or_else(|| (xid != 0).then(|| keyboard_window_center(xid)).flatten())
    })
    .await
    .ok()
    .flatten();
    if let Some((x, y)) = target {
        emit_cursor_hook(&cursor_id, x, y, false);
    }
}

/// Position and reveal a named session's cursor before keyboard/value input.
/// `preserve_legacy_element_visual` retains the existing element-only feedback
/// for type_text/set_value anonymous calls without adding session-style fallback
/// placement to them. Geometry and overlay failures are observational and never
/// affect the tool's actual input result.
async fn position_named_session_keyboard_cursor(
    state: &ToolState,
    args: &Value,
    pid: u32,
    xid: u64,
    element_index: Option<usize>,
    pixel_target: Option<(f64, f64)>,
    preserve_legacy_element_visual: bool,
) {
    let named_cursor_id = named_session_cursor_key(args);
    let cursor_id = match named_cursor_id {
        Some(ref cursor_id) => cursor_id.clone(),
        None if preserve_legacy_element_visual && element_index.is_some() => {
            resolve_cursor_key(args)
        }
        None => {
            announce_keyboard_target(args, pid, xid, element_index, pixel_target).await;
            return;
        }
    };

    let remembered = named_cursor_id.as_ref().and_then(|_| {
        state
            .cursor_registry
            .get(&cursor_id)
            .and_then(|cursor| cursor.x.zip(cursor.y))
    });
    let explicit = tokio::task::spawn_blocking(move || {
        explicit_keyboard_cursor_target(pid, xid, element_index, pixel_target)
    })
    .await
    .ok()
    .flatten();

    let fallback = if named_cursor_id.is_some()
        && explicit.is_none()
        && cursor_overlay::keyboard_cursor_target(None, remembered, None, None).is_none()
    {
        tokio::task::spawn_blocking(move || {
            let center = keyboard_window_center(xid);
            let pointer = center.is_none().then(current_pointer_position).flatten();
            (center, pointer)
        })
        .await
        .unwrap_or((None, None))
    } else {
        (None, None)
    };

    let Some((sx, sy)) =
        cursor_overlay::keyboard_cursor_target(explicit, remembered, fallback.0, fallback.1)
    else {
        return;
    };
    if xid != 0 {
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
    }
    reveal_pointer_action_for(state, &cursor_id, sx, sy, false).await;
}

async fn track_overlay_drag_for(
    cursor_id: String,
    from: (f64, f64),
    to: (f64, f64),
    duration_ms: u64,
    steps: usize,
) {
    track_overlay_drag_with(
        |command| crate::overlay::send_command_for(cursor_id.clone(), command),
        from,
        to,
        duration_ms,
        steps,
    )
    .await;
}

async fn track_overlay_drag_with(
    send: impl Fn(cursor_overlay::OverlayCommand),
    from: (f64, f64),
    to: (f64, f64),
    duration_ms: u64,
    steps: usize,
) {
    send(cursor_overlay::OverlayCommand::SetEnabled(true));
    let _pressed = cursor_overlay::PressedVisualGuard::new(&send);
    let steps = steps.max(1);
    let step_delay = std::time::Duration::from_millis(duration_ms / steps as u64);
    for index in 0..=steps {
        let t = index as f64 / steps as f64;
        let x = from.0 + (to.0 - from.0) * t;
        let y = from.1 + (to.1 - from.1) * t;
        send(cursor_overlay::track_pointer_command(x, y));
        if index < steps && !step_delay.is_zero() {
            tokio::time::sleep(step_delay).await;
        }
    }
}

/// How often the agent cursor is redrawn while it follows a pointer path.
const OVERLAY_FOLLOW_FRAME: std::time::Duration = std::time::Duration::from_millis(8);

/// Share of the screen width above which the agent cursor eases across a
/// jump between path samples instead of sliding at an even speed.
const EASED_JUMP_SCREEN_FRACTION: f64 = 0.08;

/// The eased-jump threshold when the screen width is unknown (Wayland).
const EASED_JUMP_FALLBACK_PX: f64 = 200.0;

/// Jumps longer than this are eased: 8% of `screen_width`, else 200 px.
fn eased_jump_px(screen_width: Option<u32>) -> f64 {
    screen_width.map_or(EASED_JUMP_FALLBACK_PX, |width| {
        f64::from(width) * EASED_JUMP_SCREEN_FRACTION
    })
}

/// Quadratic ease-in-out: speeds up over the first half, slows down over
/// the second.
fn ease_in_out_quad(t: f64) -> f64 {
    if t < 0.5 {
        2.0 * t * t
    } else {
        1.0 - (2.0 - 2.0 * t).powi(2) / 2.0
    }
}

/// Where the agent cursor is drawn `elapsed_ms` into a pointer path whose
/// samples `points` are due at `offsets_ms`. Between two samples it is on the
/// line from the one the real pointer is at toward the next, so it reaches
/// each sample when the pointer jumps there and slides across wide gaps
/// instead of jumping with the pointer. Gaps longer than `ease_over_px` are
/// crossed with a quadratic ease, shorter ones at an even speed.
fn path_overlay_point(
    points: &[(f64, f64)],
    offsets_ms: &[f64],
    elapsed_ms: f64,
    ease_over_px: f64,
) -> (f64, f64) {
    let next = offsets_ms.partition_point(|&offset| offset <= elapsed_ms);
    let Some(&to) = points.get(next) else {
        return points.last().copied().unwrap_or((0.0, 0.0));
    };
    let Some(&from) = next.checked_sub(1).and_then(|index| points.get(index)) else {
        return to;
    };
    let (t0, t1) = (offsets_ms[next - 1], offsets_ms[next]);
    let mut fraction = if t1 > t0 {
        ((elapsed_ms - t0) / (t1 - t0)).clamp(0.0, 1.0)
    } else {
        1.0
    };
    if (to.0 - from.0).hypot(to.1 - from.1) > ease_over_px {
        fraction = ease_in_out_quad(fraction);
    }
    (
        from.0 + (to.0 - from.0) * fraction,
        from.1 + (to.1 - from.1) * fraction,
    )
}

/// Draw the agent cursor along a pointer path on the path's own schedule.
/// It waits for the path driver to report its first sample, then sends one
/// position per overlay frame until the last sample is due, shown pressed
/// while `pressed`, easing across gaps longer than `ease_over_px`. Returns
/// early if the driver stops before it starts.
async fn follow_overlay_path_with(
    send: impl Fn(cursor_overlay::OverlayCommand),
    points: &[(f64, f64)],
    offsets_ms: &[f64],
    pressed: bool,
    ease_over_px: f64,
    mut started: tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    if points.is_empty() || started.recv().await.is_none() {
        return;
    }
    let _pressed = pressed.then(|| cursor_overlay::PressedVisualGuard::new(&send));
    let start = tokio::time::Instant::now();
    let end_ms = offsets_ms.last().copied().unwrap_or(0.0);
    loop {
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        let (x, y) = path_overlay_point(points, offsets_ms, elapsed_ms, ease_over_px);
        send(cursor_overlay::track_pointer_command(x, y));
        if elapsed_ms >= end_ms {
            break;
        }
        tokio::time::sleep(OVERLAY_FOLLOW_FRAME).await;
    }
}

#[cfg(test)]
#[tokio::test]
async fn cancelled_overlay_drag_releases_visual_press_without_native_commands() {
    use std::{cell::RefCell, future::Future, task::Context};
    let commands = RefCell::new(Vec::new());
    let mut drag = Box::pin(track_overlay_drag_with(
        |command| commands.borrow_mut().push(command),
        (10.0, 20.0),
        (30.0, 40.0),
        2000,
        20,
    ));
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(drag.as_mut().poll(&mut context).is_pending());
    assert!(matches!(
        commands.borrow()[1],
        cursor_overlay::OverlayCommand::SetPressed(true)
    ));
    drop(drag);
    let commands = commands.borrow();
    assert_eq!(commands.len(), 4);
    assert!(matches!(
        commands.last(),
        Some(cursor_overlay::OverlayCommand::SetPressed(false))
    ));
}

#[cfg(test)]
#[tokio::test]
async fn completed_overlay_drag_balances_visual_press_once() {
    use std::cell::RefCell;
    let pressed = RefCell::new(Vec::new());
    track_overlay_drag_with(
        |command| {
            if let cursor_overlay::OverlayCommand::SetPressed(value) = command {
                pressed.borrow_mut().push(value);
            }
        },
        (10.0, 20.0),
        (30.0, 40.0),
        0,
        2,
    )
    .await;
    assert_eq!(*pressed.borrow(), vec![true, false]);
}

#[cfg(test)]
fn path_overlay_point_linear(
    points: &[(f64, f64)],
    offsets: &[f64],
    elapsed_ms: f64,
) -> (f64, f64) {
    path_overlay_point(points, offsets, elapsed_ms, f64::INFINITY)
}

#[cfg(test)]
#[test]
fn long_jumps_are_eased_and_short_ones_are_not() {
    assert_eq!(eased_jump_px(Some(2560)), 204.8);
    assert_eq!(eased_jump_px(None), 200.0);
    let long = [(0.0, 0.0), (300.0, 0.0)];
    let short = [(0.0, 0.0), (100.0, 0.0)];
    let offsets = [0.0, 100.0];
    // Slow start, full speed in the middle, slow arrival.
    assert_eq!(
        path_overlay_point(&long, &offsets, 25.0, 200.0),
        (37.5, 0.0)
    );
    assert_eq!(
        path_overlay_point(&long, &offsets, 50.0, 200.0),
        (150.0, 0.0)
    );
    assert_eq!(
        path_overlay_point(&long, &offsets, 75.0, 200.0),
        (262.5, 0.0)
    );
    assert_eq!(
        path_overlay_point(&long, &offsets, 100.0, 200.0),
        (300.0, 0.0)
    );
    assert_eq!(
        path_overlay_point(&short, &offsets, 25.0, 200.0),
        (25.0, 0.0)
    );
}

#[cfg(test)]
#[test]
fn path_overlay_point_leads_the_pointer_to_each_sample() {
    let points = [(0.0, 0.0), (100.0, 0.0), (100.0, 50.0)];
    let offsets = [0.0, 100.0, 200.0];
    assert_eq!(
        path_overlay_point_linear(&points, &offsets, -5.0),
        (0.0, 0.0)
    );
    assert_eq!(
        path_overlay_point_linear(&points, &offsets, 0.0),
        (0.0, 0.0)
    );
    // Halfway between samples, halfway along the jump the pointer makes next.
    assert_eq!(
        path_overlay_point_linear(&points, &offsets, 50.0),
        (50.0, 0.0)
    );
    assert_eq!(
        path_overlay_point_linear(&points, &offsets, 100.0),
        (100.0, 0.0)
    );
    assert_eq!(
        path_overlay_point_linear(&points, &offsets, 150.0),
        (100.0, 25.0)
    );
    assert_eq!(
        path_overlay_point_linear(&points, &offsets, 200.0),
        (100.0, 50.0)
    );
    assert_eq!(
        path_overlay_point_linear(&points, &offsets, 900.0),
        (100.0, 50.0)
    );
    // Samples due at the same time do not divide by zero.
    assert_eq!(
        path_overlay_point_linear(&[(0.0, 0.0), (10.0, 10.0)], &[0.0, 0.0], 0.0),
        (10.0, 10.0)
    );
}

#[cfg(test)]
#[tokio::test(start_paused = true)]
async fn overlay_path_follow_waits_for_the_start_and_ends_on_the_last_sample() {
    use std::cell::RefCell;
    let commands = RefCell::new(Vec::new());
    let (started, started_rx) = tokio::sync::mpsc::unbounded_channel();
    started.send(()).unwrap();
    follow_overlay_path_with(
        |command| commands.borrow_mut().push(command),
        &[(0.0, 0.0), (80.0, 0.0)],
        &[0.0, 40.0],
        true,
        f64::INFINITY,
        started_rx,
    )
    .await;
    let commands = commands.borrow();
    assert!(matches!(
        commands.first(),
        Some(cursor_overlay::OverlayCommand::SetPressed(true))
    ));
    assert!(matches!(
        commands.last(),
        Some(cursor_overlay::OverlayCommand::SetPressed(false))
    ));
    let tips: Vec<(f64, f64)> = commands
        .iter()
        .filter_map(|command| match command {
            cursor_overlay::OverlayCommand::SnapTo {
                x,
                y,
                heading_radians: Some(heading),
            } => Some(cursor_overlay::pointer_for_anchor(*x, *y, *heading)),
            _ => None,
        })
        .collect();
    // One frame every 8 ms over 40 ms, sliding toward the second sample.
    assert!(tips.len() >= 5, "{tips:?}");
    assert!(tips.windows(2).all(|pair| pair[1].0 >= pair[0].0 - 1e-9));
    let last = tips.last().unwrap();
    assert!(
        (last.0 - 80.0).abs() < 1e-6 && last.1.abs() < 1e-6,
        "{last:?}"
    );
}

#[cfg(test)]
#[tokio::test]
async fn overlay_path_follow_sends_nothing_when_the_path_never_starts() {
    use std::cell::RefCell;
    let commands = RefCell::new(Vec::new());
    let (started, started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    drop(started);
    follow_overlay_path_with(
        |command| commands.borrow_mut().push(command),
        &[(0.0, 0.0), (80.0, 0.0)],
        &[0.0, 40.0],
        true,
        f64::INFINITY,
        started_rx,
    )
    .await;
    assert!(commands.borrow().is_empty());
}

fn process_name(pid: u32) -> Option<String> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let first = String::from_utf8_lossy(&cmdline)
        .split('\0')
        .next()
        .unwrap_or("")
        .trim()
        .to_owned();
    if !first.is_empty() {
        return std::path::Path::new(&first)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .or(Some(first));
    }

    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|l| l.starts_with("Name:"))
        .map(|l| l[5..].trim().to_owned())
}

fn is_terminal_process(pid: u32) -> bool {
    // Canonical list lives in `crate::terminal::TERMINAL_PROCESS_NAMES`
    // — keeps the additive contract centralised. Adding a new terminal
    // here means appending one string in `crate::terminal`.
    match process_name(pid).as_deref() {
        Some(name) => crate::terminal::is_terminal_process_name(name),
        None => false,
    }
}

fn terminal_descendant_ttys(pid: u32) -> Vec<PathBuf> {
    let mut parent_to_children: std::collections::HashMap<u32, Vec<u32>> =
        std::collections::HashMap::new();
    let proc_dir = std::path::Path::new("/proc");
    let entries = match fs::read_dir(proc_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    for entry in entries.flatten() {
        let pid_str = entry.file_name();
        let pid_str = pid_str.to_string_lossy();
        let child_pid: u32 = match pid_str.parse() {
            Ok(pid) => pid,
            Err(_) => continue,
        };
        let status = match fs::read_to_string(proc_dir.join(&*pid_str).join("status")) {
            Ok(status) => status,
            Err(_) => continue,
        };
        let parent_pid = status
            .lines()
            .find(|l| l.starts_with("PPid:"))
            .and_then(|l| l[5..].trim().parse::<u32>().ok());
        if let Some(parent_pid) = parent_pid {
            parent_to_children
                .entry(parent_pid)
                .or_default()
                .push(child_pid);
        }
    }

    let mut descendants = Vec::new();
    let mut queue = std::collections::VecDeque::from([pid]);
    while let Some(current) = queue.pop_front() {
        if let Some(children) = parent_to_children.get(&current) {
            for &child in children {
                descendants.push(child);
                queue.push_back(child);
            }
        }
    }
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
fn is_enter_key(key: &str) -> bool {
    crate::input::key_name_to_keysym(key).ok() == Some(0xFF0D)
}

/// Type into a terminal window without touching X focus. Resolves the window's
/// pty, then borrows the emulator's master fd and writes to it (see
/// `crate::tty`). Returns `Ok(false)` when the target isn't a terminal we can
/// reach this way so the caller falls back to the generic XSendEvent path.
fn inject_terminal_input(pid: u32, xid: u64, text: &str) -> anyhow::Result<bool> {
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

// ── click ─────────────────────────────────────────────────────────────────────

pub struct ClickTool {
    state: Arc<ToolState>,
}

fn capture_admission_error(error: anyhow::Error) -> ToolResult {
    let code = pincer_core::capture_runtime::admission_error_code(&error);
    ToolResult::error(format!("capture-bound click refused: {error}"))
        .with_structured(json!({ "code": code, "effect": "refused" }))
}
static CLICK_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// Where a foreground element click may put the real pointer.
enum ForegroundElementPlacement {
    /// The element centre lies inside the target window (or a popup of the
    /// same process): XTest-click it there.
    Point { xid: u64, sx: f64, sy: f64 },
    /// The element has no usable on-screen bounds (its menu is closed, its
    /// page is unmapped, its centre is outside every window of the process):
    /// only the AT-SPI action can reach it.
    NoPoint { reason: String },
}

impl ClickTool {
    /// X11 foreground element click. Behave like a user when the element is
    /// really on screen: activate the window and XTest-click the element's
    /// centre. A degenerate centre — the walk answering (0,0) for a menu item
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
                let owned_ref = observed
                    .object_ref
                    .clone()
                    .filter(|_| !observed.in_web_content);
                let (sx, sy, redirect_note) = if let Some(oref) = owned_ref {
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
                        ),
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
                            }));
                        }
                        _ => (sx, sy, None),
                    }
                } else {
                    (sx, sy, None)
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
                                crate::input::send_click_xtest_desktop_with_modifiers(
                                    sx.round() as i32,
                                    sy.round() as i32,
                                    button,
                                    count,
                                    &modifier_refs,
                                )
                            },
                        )
                    },
                )
                .await;
                match result {
                    Ok(Ok(((), report))) => {
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
                             (delivery_mode=foreground{}, focus_after={}){}{}{popup_note}",
                            observed.described(idx),
                            report
                                .popup_target
                                .map(|popup| format!(
                                    ", popup window {popup} took the input without activation"
                                ))
                                .unwrap_or_default(),
                            report.focus_after.as_str(),
                            if confirmed {
                                format!("; {}.", report.window_change.as_deref().unwrap_or(""))
                            } else {
                                format!("; not verified: {POINTER_CHECK}.")
                            },
                            redirect_note
                                .as_deref()
                                .map(|n| format!(" ({n})."))
                                .unwrap_or_default(),
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
    // The centre is outside the named window: a popup menu of the same
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
                "its centre ({px}, {py}) lies outside window {placed_xid} and every popup of \
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
                    "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
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
        let cursor_id = resolve_cursor_key(&args);
        let modifiers: Vec<String> = args.str_array("modifier");

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
                return ToolResult::error(
                    "click: x,y given with no pid/window_id requires scope=\"desktop\"; \
                     use get_desktop_state to read true screen pixels first."
                        .to_string(),
                )
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
                    "✅ Sent screen-absolute click at ({sx},{sy}) (desktop scope)."
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
                "click: unknown button \"{button_str_raw}\" — expected one of left, right, middle."
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

        if let Some(idx) = elem_idx_resolved {
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
            // window and XTest-click the element's screen centre. No AT-SPI
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

        // Coordinate-based path.
        if let Some(refusal) = require_point_args("click", &args) {
            return refusal;
        }
        let xid = match args.opt_u64("window_id") {
            Some(v) => v,
            None => return ToolResult::error("Provide either element_token or pid + window_id + x/y."),
        };
        let desktop_frame = desktop_frame_requested(&args);
        let from_zoom = args.bool_or("from_zoom", false);
        let mut x = args.f64_or("x", 0.0);
        let mut y = args.f64_or("y", 0.0);
        if capture_id_arg.is_some() && from_zoom {
            return ToolResult::error(
                "click.capture_id cannot be combined with from_zoom; use coordinates from the published capture.",
            )
            .with_structured(json!({ "code": "invalid_arguments" }));
        }
        if capture_id_arg.is_some() && desktop_frame {
            return ToolResult::error(
                "click.capture_id cannot be combined with desktop-frame coordinates; use coordinates from the published capture.",
            )
            .with_structured(json!({ "code": "invalid_arguments" }));
        }
        let native_refusal = (!isolated_background)
            .then(|| unavailable_wayland_focused_input_background(delivery, true))
            .flatten();
        if desktop_frame {
            // Desktop pixels: no zoom/screenshot scaling; translated once below.
            if let Some(refusal) = native_refusal {
                return refusal;
            }
        } else if let Some(capture_id) = capture_id_arg {
            if let Some(refusal) = native_refusal {
                return refusal;
            }
            match crate::capture_action_frame::admit_window_click(
                &self.state.capture_service,
                &args,
                &capture_id,
                pid,
                xid,
                x,
                y,
            ) {
                Ok(point) => (x, y) = point,
                Err(error) => return capture_admission_error(error),
            }
        } else {
            let context = match coordinate_click_context(native_refusal, || {
                if from_zoom {
                    self.state
                        .zoom_context(&args, pid, Some(xid))
                        .map(CoordinateContext::Zoom)
                } else {
                    screenshot_scale(&self.state, &args, pid, Some(xid))
                        .map(CoordinateContext::Screenshot)
                }
            }) {
                Ok(context) => context,
                Err(refusal) => return refusal,
            };
            match context {
                CoordinateContext::Zoom(context) => {
                    (x, y) = context.zoom_to_window(x, y);
                }
                CoordinateContext::Screenshot(scale) => {
                    x *= scale;
                    y *= scale;
                }
            }
        }
        if desktop_frame {
            match tokio::task::spawn_blocking(move || desktop_to_window_local(xid, x, y)).await {
                Ok(Ok((lx, ly))) => {
                    x = lx;
                    y = ly;
                }
                Ok(Err(e)) => {
                    return ToolResult::error(format!(
                        "desktop-frame coordinates could not be mapped into window {xid}: {e}"
                    ))
                }
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            }
        }

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
            reveal_pointer_action_for(&self.state, &cursor_id, sx, sy, true).await;
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
        let result = spawn_blocking_bounded("foreground click", fg_budget, move || -> anyhow::Result<(&'static str, Option<crate::input::ForegroundReport>, Option<PointerRoute>, Option<crate::input::FocusGuardReport>)> {
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
                crate::input::with_x11_foreground_opts(xid, opts, || inject(true))
                .map(|((path, route, guard), report)| (path, Some(report), route, guard))
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
                attach_focus_guard(
                    ToolResult::text(format!("Clicked at ({x:.1}, {y:.1}) × {count} {suffix}"))
                        .with_structured(structured),
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

// ── px-focus helper (keyboard family) ───────────────────────────────────────

/// px-focus for the keyboard family (type_text / press_key / hotkey): pixel-click
/// at (x,y) to establish real renderer focus before a keystroke — the *element px
/// action* form of a keyboard tool. Reuses ClickTool's exact coordinate
/// translation + delivery_mode so it lands on the same pixel a px-click would.
/// `Ok(())` on success; `Err(ToolResult)` short-circuits the caller.
///
/// Retains the parent's authenticated session admission for the nested click.
/// `point` is the window-local pixel to click for focus.
async fn focus_by_pixel(
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
async fn focus_nested_inject_target(
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
                active one. On KDE Plasma X11, XWayland and Wayland use delivery_mode \
                \"foreground\" for keys; to enter text, type_text with element_token works in \
                the background.".into(),
            input_schema: json!({
                "type":"object","required":["key"],"properties":{
                    "session": pincer_core::tool_schema::session_schema(),
                    "pid":{"type":"integer","description":"Target process ID. Omit with scope \"desktop\" to send the key to the focused application."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                    "key":{"type":"string","description":"Key name: enter/return, tab, escape, space, backspace, delete, insert, home, end, pageup, pagedown, up, down, left, right, f1-f12, or any single ASCII character."},
                    "modifiers":{"type":"array","items":{"type":"string"},"description":"Modifier keys held while the key is pressed: ctrl, shift, alt, super."},
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
            let key = input.key;
            let display = key.clone();
            let modifiers = input.modifiers.unwrap_or_default();
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

fn is_modifier(k: &str) -> bool {
    matches!(
        k.to_lowercase().as_str(),
        "ctrl"
            | "control"
            | "shift"
            | "alt"
            | "super"
            | "meta"
            | "cmd"
            | "command"
            | "win"
            | "windows"
    )
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
                act on them only while their window is the active one. On KDE Plasma X11, \
                XWayland and Wayland use delivery_mode \"foreground\". Window-manager chords \
                cannot reach the WM that way: alt+F4 on a window target closes that window \
                through the WM (_NET_CLOSE_WINDOW, effect confirmed when it unmaps); super+*, \
                alt+tab, ctrl+alt+* and other WM bindings are refused with code \
                wm_chord_unavailable: use bring_to_front, list_windows, kill_app or \
                delivery_mode \"foreground\" instead.".into(),
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
            let modifiers: Vec<String> = keys
                .iter()
                .filter(|key| is_modifier(key))
                .cloned()
                .collect();
            let Some(key) = keys.iter().rev().find(|key| !is_modifier(key)).cloned() else {
                return ToolResult::error("keys must include at least one non-modifier key.");
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
            let modifiers: Vec<String> = keys.iter().filter(|k| is_modifier(k)).cloned().collect();
            let non_mods: Vec<String> = keys.iter().filter(|k| !is_modifier(k)).cloned().collect();
            if non_mods.is_empty() {
                return ToolResult::error("keys must include at least one non-modifier key.");
            }
            (non_mods.last().unwrap().clone(), modifiers)
        } else if let Some(k) = args.opt_str("key") {
            let mods: Vec<String> = args.str_array("modifiers");
            (k, mods)
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
                    // Native Wayland: route the modifier combo through wtype's
                    // -M/-k/-m sequence — the closest equivalent to the X11
                    // state-mask path. window_id is irrelevant once focused.
                    let mut combo: Vec<String> = mods_for_wayland.clone();
                    combo.push(key_for_wayland.clone());
                    return crate::wayland::hotkey(xid, &combo).map(|()| KeyRoute::Synthetic);
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

// ── scroll ────────────────────────────────────────────────────────────────────

pub struct ScrollTool {
    state: Arc<ToolState>,
}

fn atspi_scroll_result(progress: crate::atspi::ScrollProgress, foreground: bool) -> ToolResult {
    use pincer_core::action_record::{
        ActionEffect, ActionExecutionRecord, ActionTransport, ActualDelivery, RequestedDelivery,
    };
    let effect = if !progress.complete && progress.acknowledged > 0 {
        ActionEffect::Partial
    } else {
        ActionEffect::Unverifiable
    };
    let mut record = ActionExecutionRecord::builder(
        effect,
        ActionTransport::LinuxAtSpiAction,
        if foreground {
            RequestedDelivery::Foreground
        } else {
            RequestedDelivery::Background
        },
    )
    .actual_delivery(if !progress.complete {
        ActualDelivery::Unknown
    } else if foreground {
        ActualDelivery::Foreground
    } else {
        ActualDelivery::Background
    });
    if progress.acknowledged > 0 {
        record = record.delivered_count(progress.acknowledged);
    }
    let text = if progress.complete {
        format!(
            "AT-SPI acknowledged {} scroll action(s); effect remains unverified.",
            progress.acknowledged
        )
    } else {
        format!("AT-SPI scroll stopped after {} acknowledged action(s): {}. Refresh state before another action.", progress.acknowledged, progress.detail.as_deref().unwrap_or("unknown outcome"))
    };
    let record = record
        .detail(text.clone())
        .build()
        .expect("AT-SPI scroll preserves acknowledged progress");
    let public = serde_json::to_value(record.public_result().unwrap()).unwrap();
    let result = if progress.complete {
        ToolResult::text(text)
    } else {
        ToolResult::error(text)
    };
    result.with_structured(public).with_action_record(record)
}

#[test]
fn atspi_scroll_outcomes_preserve_uncertainty_and_acknowledged_count() {
    use pincer_core::action_record::ActionEffect;
    for (complete, acknowledged, effect) in [
        (false, 0, ActionEffect::Unverifiable),
        (false, 1, ActionEffect::Partial),
        (true, 2, ActionEffect::Unverifiable),
    ] {
        let result = atspi_scroll_result(
            crate::atspi::ScrollProgress {
                acknowledged,
                complete,
                detail: None,
            },
            false,
        );
        let record = result.action_record.unwrap();
        assert_eq!(record.effect, effect);
        let public = serde_json::to_value(record.public_result().unwrap()).unwrap();
        assert_eq!(
            public["delivery"]["mode"],
            if complete { "background" } else { "unknown" }
        );
        assert_eq!(
            public["delivery"]["delivered_count"],
            if acknowledged == 0 {
                Value::Null
            } else {
                json!(acknowledged)
            }
        );
    }
}
static SCROLL_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for ScrollTool {
    fn def(&self) -> &ToolDef {
        SCROLL_DEF.get_or_init(|| ToolDef {
            name: "scroll".into(),
            description: "Scroll a window without stealing focus: wheel steps over the \
                element_token or x,y point, else the window's center, sent by the session's \
                virtual pointer where uinput is available, else as synthetic X events (path \
                x11_xsendevent). direction required; amount (default 3) counts wheel steps, or with \
                by:\"page\" pages of 3 wheel steps each, not screenfuls; the app decides how far a \
                step moves. On X11 the result compares the window's scroll bars from \
                the last get_window_state (a narrow read must include them, e.g. query:\"scroll \
                bar\"): an error with code no_change when nothing moved, unverifiable when no \
                scroll bar is known. Qt ignores synthetic wheel events: use delivery_mode \
                foreground, or set_value on the scroll bar row with a value in the bar's own \
                units (the number its row shows, not a fraction). Where no focus-free pointer route \
                exists (a GTK3 canvas, for example) background delivery is refused with \
                background_unavailable: retry with delivery_mode foreground. Window x,y need a \
                get_window_state screenshot of the window earlier in the session \
                (screenshot_context_missing otherwise). modifier holds keys during the wheel \
                steps (ctrl+wheel zooms in most editors, canvases and browsers); X11 only. A \
                modified scroll is not checked against the scroll bars, since it usually zooms \
                instead of scrolling: its result is unverifiable.".into(),
            input_schema: json!({
                // `pid` is conditionally required (validated in code), so only
                // `direction` is pinned — matches the scroll→["direction"] canon
                // in pincer_core::tool_schema.
                "type":"object","required":["direction"],"properties":{
                    "session": pincer_core::tool_schema::session_schema(),
                    "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                    "pid":{"type":"integer","description":"Process id. Required unless scope is \"desktop\" or element_token is given (the token carries it)."},
                    "direction":{"type":"string","enum":["up","down","left","right"],"description":"Scroll direction."},
                    "by":{"type":"string","enum":["line","page"],"description":"line (default): one wheel step per amount; page: 3 wheel steps per amount."},
                    "amount":{"type":"integer","minimum":1,"maximum":50,"description":"Wheel steps, or pages with by:\"page\". Default: 3."},
                    "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                    "element_token": pincer_core::tool_schema::element_token_schema(),
                    "modifier": pincer_core::tool_schema::modifier_schema(),
                    "x":{"type":"number","description":"Window-local screenshot-pixel X of the scroll target. Pass with y and without element_token."},
                    "y":{"type":"number","description":"Window-local screenshot-pixel Y of the scroll target. Pass with x and without element_token."},
                    "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with x,y and no pid/window_id for get_desktop_state screen coordinates. Default \"window\"."},
                    "coordinate_frame": coordinate_frame_schema(),
                    "delivery_mode": crate::input::delivery::delivery_mode_schema()
                },"additionalProperties":false
            }),
            read_only: false, destructive: false, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        // Keys held around the wheel steps (ctrl+wheel to zoom). Only the X11
        // XTest and XSendEvent routes can carry them.
        let modifiers: Vec<String> = args.str_array("modifier");
        if !modifiers.is_empty() && crate::wayland::wayland_input_enabled() {
            return ToolResult::error(
                "scroll with modifier is unavailable on native Wayland: the wheel route \
                 cannot carry keyboard modifier state. Use press_key or hotkey for the \
                 app's keyboard zoom instead.",
            );
        }
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<ScrollInput>("scroll", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let direction = input.direction.as_str().to_owned();
            let (x, y) = (input.x, input.y);
            let x = x.round() as i32;
            let y = y.round() as i32;
            let amount = input.amount.unwrap_or(3).clamp(1, 50) as usize;
            let display = direction.clone();
            let wayland = crate::wayland::wayland_input_enabled();
            let path = if wayland { "wayland_desktop" } else { "xtest" };
            let space = match desktop_input_space().await {
                Ok(space) => space,
                Err(error) => return ToolResult::error(error.to_string()),
            };
            if named_session_cursor_key(&args).is_some() {
                let (overlay_x, overlay_y) = space.to_layout(x, y);
                reveal_pointer_action_for(
                    &self.state,
                    &cursor_id,
                    f64::from(overlay_x),
                    f64::from(overlay_y),
                    false,
                )
                .await;
            }
            let modifiers_for_task = modifiers.clone();
            let result = tokio::task::spawn_blocking(move || {
                if wayland {
                    crate::wayland::scroll_desktop(&space, x, y, &direction, amount as u32)
                } else if modifiers_for_task.is_empty() {
                    crate::input::send_scroll_xtest_desktop(x, y, &direction, amount)
                } else {
                    let refs: Vec<&str> = modifiers_for_task.iter().map(String::as_str).collect();
                    crate::input::send_click_xtest_desktop_with_modifiers(
                        x,
                        y,
                        wheel_button(&direction),
                        amount,
                        &refs,
                    )
                }
            })
            .await;
            let held = held_note(&modifiers);
            return match result {
                Ok(Ok(())) => {
                    ToolResult::text(format!("Scrolled desktop {display} × {amount}{held}."))
                        .with_structured(
                            json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                        )
                }
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let mut pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let direction = match args.require_str("direction") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let amount = args.u64_or("amount", 3).clamp(1, 50) as usize;
        let by = match serde_json::from_value::<pincer_contract::ScrollBy>(
            args.get("by").cloned().unwrap_or(json!("line")),
        ) {
            Ok(by) => by,
            Err(error) => return ToolResult::error(format!("Invalid scroll by: {error}")),
        };
        // The Linux scroll implementation doesn't pre-focus the element (X11
        // scroll buttons go to the window root), but the token still needs to
        // be validated so a stale token surfaces an error instead of silently
        // no-op'ing.
        let resolved = match self.state.snapshots.resolve(pid as i32, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let xid_opt: Option<u64> = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            pincer_core::element_token::ResolvedElement::None => args.opt_u64("window_id"),
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

        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let isolated_background = isolated_hyprland_background(delivery);
        let pixel_target = match (
            args.get("x").and_then(|value| value.as_f64()),
            args.get("y").and_then(|value| value.as_f64()),
        ) {
            (Some(x), Some(y)) if desktop_frame_requested(&args) => {
                // Desktop-frame pixels (from get_desktop_state) against a named
                // window: map into the window-local frame the pipeline expects.
                match tokio::task::spawn_blocking(move || desktop_to_window_local(xid, x, y)).await
                {
                    Ok(Ok(local)) => Some(local),
                    Ok(Err(e)) => {
                        return ToolResult::error(format!(
                            "desktop-frame coordinates could not be mapped into window {xid}: {e}"
                        ))
                    }
                    Err(e) => return ToolResult::error(format!("Task error: {e}")),
                }
            }
            (Some(x), Some(y)) => {
                // Pixel targets use the latest screenshot's coordinate frame.
                // Apply the same buffer-to-window ratio as click/drag before
                // positioning either the agent cursor or the input device.
                // An unsupported native background delivery refuses first.
                let native_refusal = (!isolated_background)
                    .then(|| unavailable_wayland_focused_input_background(delivery, true))
                    .flatten();
                let ratio = match coordinate_scroll_scale(native_refusal, || {
                    screenshot_scale(&self.state, &args, pid, Some(xid))
                }) {
                    Ok(ratio) => ratio,
                    Err(refusal) => return refusal,
                };
                Some((x * ratio, y * ratio))
            }
            (None, None) => None,
            (Some(_), None) => return invalid_pointer_arguments("scroll", "x was given without y"),
            (None, Some(_)) => return invalid_pointer_arguments("scroll", "y was given without x"),
        };
        let resolved_element_index = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { element_index, .. } => {
                Some(*element_index)
            }
            pincer_core::element_token::ResolvedElement::None => None,
        };
        if pixel_target.is_some() && resolved_element_index.is_some() {
            return ToolResult::error(
                "Pass either element_token (ax) or x,y (px) to scroll, not both.",
            );
        }

        if named_session_cursor_key(&args).is_some() {
            let visual_target = tokio::task::spawn_blocking(move || {
                explicit_keyboard_cursor_target(pid, xid, resolved_element_index, pixel_target)
                    .or_else(|| keyboard_window_center(xid))
            })
            .await
            .ok()
            .flatten();
            if let Some((sx, sy)) = visual_target {
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::OverlayCommand::PinAbove(xid),
                );
                reveal_pointer_action_for(&self.state, &cursor_id, sx, sy, false).await;
            }
        }

        if !isolated_background {
            if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                return refusal;
            }
        }
        if !modifiers.is_empty()
            && (isolated_background
                || hyprland_foreground(delivery)
                || crate::wayland::is_inject_mode())
        {
            return ToolResult::error(
                "scroll with modifier needs X11 input: this route cannot carry keyboard \
                 modifier state. Use press_key or hotkey for the app's keyboard zoom instead.",
            );
        }
        // An AT-SPI scroll moves the view and cannot hold keys, so a modified
        // scroll goes to the wheel over the element instead.
        if let (pincer_core::element_token::ResolvedElement::Element { element_index, .. }, true) =
            (&resolved, modifiers.is_empty())
        {
            let idx = *element_index;
            // WebKitGTK acknowledges the AT-SPI scroll action without moving
            // the DOM scroller. On native Wayland, use a real compositor wheel
            // event at the resolved element instead of reporting a silent
            // success. Other AX targets keep their semantic route before any
            // coordinate-based compositor fallback.
            if !(crate::wayland::wayland_input_enabled() && is_webkitgtk_embedder(pid)) {
                let direction_for_ax = direction.clone();
                let ax_result = tokio::task::spawn_blocking(move || {
                    crate::atspi::scroll_element(pid, xid, idx, &direction_for_ax, amount, by)
                })
                .await;
                match ax_result {
                    Ok(Ok(progress)) => {
                        return atspi_scroll_result(progress, delivery.is_foreground())
                    }
                    Err(error) => {
                        return atspi_scroll_result(
                            crate::atspi::ScrollProgress {
                                acknowledged: 0,
                                complete: false,
                                detail: Some(error.to_string()),
                            },
                            delivery.is_foreground(),
                        )
                    }
                    Ok(Err(_)) => {}
                }
            }
        }
        if hyprland_foreground(delivery) {
            if xid_opt.is_none() {
                return foreground_hyprland_refusal(
                    "an exact window_id or window-bound element token is required",
                );
            }
            let point = if let Some(index) = resolved_element_index {
                match tokio::task::spawn_blocking(move || {
                    resolve_element_local_coords(pid, index, Some(xid))
                })
                .await
                {
                    Ok(Ok((_, x, y))) => Some((x, y)),
                    _ => {
                        return foreground_hyprland_refusal(
                            "could not resolve exact element coordinates",
                        )
                    }
                }
            } else {
                pixel_target
            };
            // `by: "page"` is the documented 3-detent approximation, not an
            // exact viewport; large requests become several wheel packets.
            let actions =
                match crate::wayland::hyprland_input::scroll_actions(point, &direction, amount, by)
                {
                    Ok(actions) => actions,
                    Err(error) => return foreground_hyprland_refusal(error.to_string()),
                };
            return foreground_hyprland_scroll(&args, pid, xid, actions).await;
        }
        if isolated_background {
            if xid_opt.is_none() {
                return isolated_hyprland_refusal(
                    "an exact window_id or window-bound element token is required",
                );
            }
            let owner = named_session_cursor_key(&args);
            let (_cancellation, dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    let point = if let Some(idx) = resolved_element_index {
                        let (_, x, y) = resolve_element_local_coords(pid, idx, Some(xid))?;
                        Some((x, y))
                    } else {
                        pixel_target
                    };
                    let actions = crate::wayland::hyprland_input::scroll_actions(
                        point, &direction, amount, by,
                    )?;
                    crate::wayland::hyprland_input::execute_scroll(
                        owner,
                        pid,
                        xid,
                        actions,
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

        if crate::wayland::is_inject_mode() {
            let Some((x, y)) = pixel_target else {
                return crate::input::delivery::background_unavailable_error(
                    crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
                );
            };
            let direction_for_inject = direction.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::inject_scroll(pid, xid, x, y, &direction_for_inject, amount as u32)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Scrolled {direction} {amount} ticks (focus-free via cua-compositor)."
                ))
                .with_structured(json!({
                    "verified": false,
                    "delivery_mode": if delivery.is_foreground() { "foreground" } else { "background" },
                    "route": "cua_compositor_inject"
                })),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        if crate::wayland::wayland_input_enabled() {
            if let Some(refusal) = unavailable_wayland_focused_input_background(delivery, false) {
                return refusal;
            }
            let direction_for_wayland = direction.clone();
            let local_point = match (pixel_target, &resolved) {
                (Some(point), _) => Some(point),
                (
                    None,
                    pincer_core::element_token::ResolvedElement::Element { element_index, .. },
                ) => {
                    let idx = *element_index;
                    match tokio::task::spawn_blocking(move || {
                        resolve_element_local_coords(pid, idx, Some(xid))
                    })
                    .await
                    {
                        Ok(Ok((_resolved_xid, x, y))) => Some((x, y)),
                        Ok(Err(error)) => {
                            return ToolResult::error(format!(
                                "Could not resolve scroll element [{idx}] coordinates: {error}"
                            ))
                        }
                        Err(error) => {
                            return ToolResult::error(format!(
                                "Scroll element coordinate task failed: {error}"
                            ))
                        }
                    }
                }
                (None, pincer_core::element_token::ResolvedElement::None) => None,
            };
            let output_point = local_point.map(|(x, y)| {
                crate::wayland::window_local_to_output(xid, x.round() as i32, y.round() as i32)
            });
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::scroll_at(xid, output_point, &direction_for_wayland, amount as u32)
            })
            .await;
            return match result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Scrolled {direction} {amount} ticks (delivery_mode=foreground)."
                ))
                .with_structured(json!({ "verified": false, "delivery_mode": "foreground" })),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        if let Some(refusal) = unavailable_webkit_background(pid, delivery) {
            return refusal;
        }
        if let Some(refusal) = unavailable_gtk_pointer_background(pid, delivery) {
            return refusal;
        }

        // An element-addressed scroll must land over the element. The old
        // fallback used (0, 0) in the window, which can report success while
        // Chromium/GTK routes the wheel to the toplevel instead of the
        // requested scroll region.
        let element_point = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { element_index, .. } => {
                let idx = *element_index;
                match tokio::task::spawn_blocking(move || {
                    resolve_element_local_coords(pid, idx, Some(xid))
                })
                .await
                {
                    Ok(Ok((_resolved_xid, local_x, local_y))) => {
                        let screen = window_local_to_screen(xid, local_x, local_y).ok();
                        Some(((local_x, local_y), screen))
                    }
                    Ok(Err(e)) => {
                        return ToolResult::error(format!(
                            "Could not resolve scroll element [{idx}] coordinates: {e}"
                        ))
                    }
                    Err(e) => {
                        return ToolResult::error(format!(
                            "Scroll element coordinate task failed: {e}"
                        ))
                    }
                }
            }
            pincer_core::element_token::ResolvedElement::None => pixel_target.map(|local| {
                let screen = window_local_to_screen(xid, local.0, local.1).ok();
                (local, screen)
            }),
        };

        // X11 scroll buttons: 4=up, 5=down, 6=left, 7=right
        let button = wheel_button(&direction);
        // X11 has no page step: a page is WHEEL_STEPS_PER_PAGE wheel steps,
        // the approximation the Hyprland route documents.
        let steps = match by {
            pincer_contract::ScrollBy::Page => amount * WHEEL_STEPS_PER_PAGE,
            _ => amount,
        };
        // The window's scroll bars as the last snapshot indexed them, read
        // before and after: the only way to tell a scroll from wheel events
        // the toolkit ignored.
        // A modified wheel usually zooms: the scroll bars say nothing about it.
        let before = if crate::wayland::wayland_input_enabled() || !modifiers.is_empty() {
            Vec::new()
        } else {
            tokio::task::spawn_blocking(move || crate::atspi::scroll_bar_ranges(pid, xid))
                .await
                .unwrap_or_default()
        };
        let cursor_id_for_task = cursor_id.clone();
        let direction_for_wayland = direction.clone();
        let steps_u32 = steps as u32;
        let modifiers_for_task = modifiers.clone();
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<&'static str> {
            let modifier_refs: Vec<&str> = modifiers_for_task.iter().map(String::as_str).collect();
            if crate::wayland::wayland_input_enabled() {
                return crate::wayland::scroll(xid, &direction_for_wayland, steps_u32)
                    .map(|()| "wayland");
            }
            // foreground: activate the window, then scroll, then restore — for
            // surfaces that only route wheel events to the active window.
            let x11_scroll = || -> anyhow::Result<&'static str> {
                // X11: synthetic Button4-7 XSendEvents are dropped by XInput2
                // toolkits (GTK never scrolls, Qt ignores core wheel buttons
                // once XInput 2.1 is up). On a real Xorg host, drive a real
                // wheel detent through the MPX uinput pointer over the window's
                // center — libinput turns it into the XI2 smooth-scroll GTK reads —
                // without stealing focus. Falls back to the legacy XSendEvent
                // Button4-7 path on Xvfb / unsupported servers.
                // Button → axis/sign: 4=up(+v) 5=down(-v) 6=left(-h) 7=right(+h).
                // The virtual pointer has no keyboard: a modified scroll goes
                // as XSendEvent wheel buttons with the modifier state set.
                if modifier_refs.is_empty() && crate::input::real_pointer_input_available() {
                    let pointer_point = element_point
                        .and_then(|(_, screen)| screen)
                        .map(|(x, y)| (x as i32, y as i32))
                        .or_else(|| window_screen_center(xid).ok());
                    if let Some((cx, cy)) = pointer_point {
                        let horizontal = matches!(button, 6 | 7);
                        let ticks = match button {
                            4 | 7 => steps as i32,
                            _ => -(steps as i32), // 5 (down) and 6 (left)
                        };
                        match crate::input::send_virtual_pointer_scroll(
                            &cursor_id_for_task,
                            &crate::input::VirtualPointerScroll {
                                target_window: xid,
                                x: cx,
                                y: cy,
                                horizontal,
                                ticks,
                            },
                        ) {
                            Ok(()) => return Ok(crate::input::MPX_POINTER_PATH),
                            Err(error) if crate::input::is_uinput_unavailable(&error) => {
                                return Err(error)
                            }
                            Err(e) => tracing::warn!("MPX scroll fell back to XSendEvent: {e}"),
                        }
                    }
                }
                // Over the element or point, else the window's center (the
                // window's top-left corner is usually its menu bar).
                let (local_x, local_y) = element_point
                    .map(|(local, _)| local)
                    .or_else(|| {
                        window_screen_center(xid).ok().and_then(|(cx, cy)| {
                            desktop_to_window_local(xid, f64::from(cx), f64::from(cy)).ok()
                        })
                    })
                    .unwrap_or((0.0, 0.0));
                crate::input::send_click_with_modifiers(
                    xid,
                    local_x as i32,
                    local_y as i32,
                    steps,
                    button,
                    &modifier_refs,
                )
                .map(|()| "x11_xsendevent")
            };
            if delivery.is_foreground() {
                let point = element_point
                    .and_then(|(_, screen)| screen)
                    .or_else(|| {
                        window_screen_center(xid)
                            .ok()
                            .map(|(x, y)| (x as f64, y as f64))
                    })
                    .ok_or_else(|| anyhow::anyhow!("could not resolve foreground scroll point"))?;
                crate::input::with_x11_foreground(xid, 80, || {
                    crate::input::send_click_xtest_desktop_with_modifiers(
                        point.0 as i32,
                        point.1 as i32,
                        button,
                        steps,
                        &modifier_refs,
                    )
                })
                .map(|()| "x11_xtest_fg")
            } else {
                x11_scroll()
            }
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        match result {
            Ok(Ok(path)) if !modifiers.is_empty() => modified_scroll_result(
                &direction,
                steps,
                &modifiers,
                path,
                mode_label,
                path == "x11_xsendevent" && synthetic_pointer_is_dropped(pid),
            ),
            Ok(Ok(path)) => {
                let after = if before.is_empty() {
                    Vec::new()
                } else {
                    // Let the toolkit apply the events before reading back.
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    tokio::task::spawn_blocking(move || crate::atspi::scroll_bar_ranges(pid, xid))
                        .await
                        .unwrap_or_default()
                };
                let synthetic_wheel_ignored =
                    path == "x11_xsendevent" && synthetic_pointer_is_dropped(pid);
                scroll_outcome_result(
                    &direction,
                    amount,
                    by,
                    steps,
                    path,
                    mode_label,
                    scroll_movement(&before, &after, &direction),
                    synthetic_wheel_ignored,
                )
            }
            Ok(Err(e)) => input_error_result(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// X11 wheel button for a scroll direction: 4 up, 5 down, 6 left, 7 right.
fn wheel_button(direction: &str) -> u8 {
    match direction {
        "up" => 4,
        "left" => 6,
        "right" => 7,
        _ => 5,
    }
}

/// " with ctrl+shift held" for a result line, or nothing.
fn held_note(modifiers: &[String]) -> String {
    if modifiers.is_empty() {
        String::new()
    } else {
        format!(" with {} held", modifiers.join("+"))
    }
}

#[cfg(test)]
#[test]
fn modified_scroll_reports_unverifiable_with_the_held_keys() {
    let modifiers = vec!["ctrl".to_owned()];
    let result = modified_scroll_result("up", 4, &modifiers, "x11_xtest_fg", "foreground", false);
    assert!(!result.is_error.unwrap_or(false));
    let text = format!("{:?}", result.content);
    assert!(text.contains("Sent 4 wheel steps up with ctrl held"), "{text}");
    let structured = result.structured_content.expect("structured result");
    assert_eq!(structured["effect"], "unverifiable");
    assert_eq!(structured["verified"], false);
    assert_eq!(structured["modifier"], json!(["ctrl"]));
    assert_eq!(structured["path"], "x11_xtest_fg");

    let ignored = modified_scroll_result("down", 1, &modifiers, "x11_xsendevent", "background", true);
    let text = format!("{:?}", ignored.content);
    assert!(text.contains("1 wheel step down"), "{text}");
    assert!(text.contains("retry with delivery_mode"), "{text}");
}

#[cfg(test)]
#[test]
fn wheel_buttons_match_x11_numbering() {
    assert_eq!(
        ["up", "down", "left", "right"].map(wheel_button),
        [4, 5, 6, 7]
    );
    assert_eq!(held_note(&[]), "");
    assert_eq!(held_note(&["ctrl".to_owned(), "shift".to_owned()]), " with ctrl+shift held");
}

/// Result of a wheel scroll sent with keys held. Ctrl+wheel zooms in most
/// apps, so the scroll bars are no evidence either way: report it sent and
/// unverified, and name the toolkits that drop the synthetic route.
fn modified_scroll_result(
    direction: &str,
    steps: usize,
    modifiers: &[String],
    path: &str,
    mode_label: &str,
    synthetic_wheel_ignored: bool,
) -> ToolResult {
    let what = format!("{steps} wheel step{}", if steps == 1 { "" } else { "s" });
    let mut text = format!(
        "Sent {what} {direction}{} (delivery_mode={mode_label}, path={path}); not verified: \
         a modified wheel usually zooms or does something other than scroll, so check the \
         window with get_window_state.",
        held_note(modifiers)
    );
    if synthetic_wheel_ignored {
        text.push_str(
            " This toolkit (Qt, GTK or LibreOffice) ignores synthetic wheel events, so this \
             most likely did nothing: retry with delivery_mode:\"foreground\".",
        );
    }
    ToolResult::text(text).with_structured(json!({
        "verified": false,
        "effect": "unverifiable",
        "delivery_mode": mode_label,
        "path": path,
        "modifier": modifiers,
    }))
}

/// Wheel steps for one `by: "page"` step on X11 (Hyprland's documented
/// approximation).
const WHEEL_STEPS_PER_PAGE: usize = 3;

/// What a wheel scroll did to the window's scroll bars.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ScrollMovement {
    /// No scroll bar of the window was readable: nothing to compare.
    Unknown,
    /// A scroll bar moved from the first value to the second.
    Moved(f64, f64),
    /// No scroll bar moved. `at_end`: one already sat at the end the wheel
    /// pushes toward, so there was nowhere to go.
    Still { value: f64, at_end: bool },
}

fn scroll_movement(
    before: &[Option<(f64, f64, f64)>],
    after: &[Option<(f64, f64, f64)>],
    direction: &str,
) -> ScrollMovement {
    let pairs: Vec<_> = before
        .iter()
        .zip(after)
        .filter_map(|(b, a)| Some(((*b)?, (*a)?)))
        .collect();
    let Some(first) = pairs.first() else {
        return ScrollMovement::Unknown;
    };
    if let Some((b, a)) = pairs.iter().find(|(b, a)| (b.0 - a.0).abs() > f64::EPSILON) {
        return ScrollMovement::Moved(b.0, a.0);
    }
    let toward_max = matches!(direction, "down" | "right");
    let at_end = pairs.iter().any(|((current, minimum, maximum), _)| {
        if toward_max {
            current >= maximum
        } else {
            current <= minimum
        }
    });
    ScrollMovement::Still {
        value: (first.0).0,
        at_end,
    }
}

/// The scroll result, told from what the scroll bars did. A wheel scroll
/// that Qt ignored used to read "Scrolled down 10 ticks" while nothing
/// moved (desc-test rounds 2 and 3).
#[allow(clippy::too_many_arguments)]
fn scroll_outcome_result(
    direction: &str,
    amount: usize,
    by: pincer_contract::ScrollBy,
    steps: usize,
    path: &str,
    mode_label: &str,
    movement: ScrollMovement,
    synthetic_wheel_ignored: bool,
) -> ToolResult {
    // "3 pages" read as three screenfuls (round 2 of the first-call test:
    // 9 wheel steps moved KWrite 27 lines, about half its view), so the
    // result counts wheel steps and says what a page was.
    let what = format!("{steps} wheel step{}", if steps == 1 { "" } else { "s" });
    let page_note = match by {
        pincer_contract::ScrollBy::Page if amount == 1 => {
            format!(" The page was {WHEEL_STEPS_PER_PAGE} wheel steps, not a screenful.")
        }
        pincer_contract::ScrollBy::Page => format!(
            " Each of the {amount} pages was {WHEEL_STEPS_PER_PAGE} wheel steps, not a \
             screenful."
        ),
        _ => String::new(),
    };
    let route = format!("delivery_mode={mode_label}, path={path}");
    match movement {
        ScrollMovement::Moved(from, to) => ToolResult::text(format!(
            "Scrolled {direction} {what} ({route}); the scroll bar moved from {from} to \
             {to}.{page_note}"
        ))
        .with_structured(json!({
            "verified": true,
            "effect": "confirmed",
            "delivery_mode": mode_label,
            "path": path,
            "scroll_bar": {"before": from, "after": to},
            "evidence": [{
                "kind": "accessibility_readback",
                "detail": format!("scroll bar value {from} -> {to}"),
            }],
        })),
        ScrollMovement::Still {
            value,
            at_end: true,
        } => ToolResult::text(format!(
            "Sent {what} {direction} ({route}), but the view is already at its end: the \
             scroll bar stays at {value}.{page_note}"
        ))
        .with_structured(json!({
            "verified": true,
            "effect": "suspected_noop",
            "delivery_mode": mode_label,
            "path": path,
            "scroll_bar": {"before": value, "after": value},
        })),
        ScrollMovement::Still {
            value,
            at_end: false,
        } => {
            let why = if synthetic_wheel_ignored {
                "this toolkit (Qt, GTK or LibreOffice) ignores synthetic wheel events"
            } else {
                "the application ignored the wheel events"
            };
            ToolResult::error(format!(
                "Nothing scrolled: sent {what} {direction} ({route}), but the window's scroll \
                 bar stayed at {value}; {why}. In background, set the scroll position with \
                 set_value on the `scroll bar` row's element_token, in the bar's own units (the \
                 number its row's value shows, not a fraction), or press_key pagedown / pageup \
                 on the window (Qt takes keys only while its window is active); otherwise retry \
                 with delivery_mode:\"foreground\".\
                 {page_note}"
            ))
            .with_structured(json!({
                "code": "no_change",
                "verified": true,
                "effect": "no_change",
                "delivery_mode": mode_label,
                "path": path,
                "scroll_bar": {"before": value, "after": value},
            }))
        }
        ScrollMovement::Unknown => {
            let mut text = format!(
                "Sent {what} {direction} ({route}); not verified: no scroll bar of this \
                 window is known, so {POINTER_CHECK}.{page_note}"
            );
            if synthetic_wheel_ignored {
                text.push_str(
                    " This toolkit (Qt, GTK or LibreOffice) ignores synthetic wheel events, so \
                     this most likely did nothing: retry with delivery_mode:\"foreground\", or \
                     read the bar with query:\"scroll bar\" and set_value its row in the bar's \
                     own units.",
                );
            }
            ToolResult::text(text).with_structured(json!({
                "verified": false,
                "effect": "unverifiable",
                "delivery_mode": mode_label,
                "path": path,
            }))
        }
    }
}

// `ScreenshotTool` and `ScreenshotCompatTool` removed in PR #1692 — see the
// matching note in platform-windows/src/tools/impl_.rs. `get_window_state` is
// the canonical screenshot path (it returns one by default); the
// underlying capture machinery (XGetImage / `import` shell-out / etc.)
// stays reachable through GetWindowStateTool.

// ── double_click ──────────────────────────────────────────────────────────────

pub struct DoubleClickTool {
    state: Arc<ToolState>,
}
static DCLICK_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for DoubleClickTool {
    fn def(&self) -> &ToolDef {
        DCLICK_DEF.get_or_init(|| ToolDef {
            name: "double_click".into(),
            description: "Double-click at (x,y) or at the center of an element_token's bounds. \
                Background delivery on X11 uses the session's virtual pointer (path mpx_pointer) \
                when it can, else synthetic XSendEvent (path x11_xsendevent), which some \
                toolkits read as a single click: check `path` and verify the result. Where no \
                focus-free pointer route exists (a GTK3 canvas, for example) background delivery \
                is refused with background_unavailable: retry with delivery_mode foreground. In background, a \
                point covered by another application's window is refused \
                (target_occluded), one covered by this app's own dialog is retargeted to it \
                (retargeted_to). No focus steal. Pass pid + window_id + x,y, or an element_token \
                (pid optional). \
                After a zoom call, pass from_zoom=true to auto-translate zoom-image coords.".into(),
            input_schema: json!({"type":"object","required":[],"properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Target process ID. Optional with element_token (the token carries the pid)."},
                "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                "x":{"type":"number","description":"Pixel X in the window's get_window_state screenshot (0..screenshot_width). Pass scope:\"desktop\" for get_desktop_state pixels."},
                "y":{"type":"number","description":"Pixel Y in the window's get_window_state screenshot (0..screenshot_height); see x."},
                "coordinate_frame": coordinate_frame_schema(),
                "element_token": pincer_core::tool_schema::element_token_schema(),
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
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
            // Share exact-target validation and the admitted native click lifecycle.
            let mut args = args;
            args["button"] = json!("left");
            args["count"] = json!(2);
            return ClickTool {
                state: self.state.clone(),
            }
            .invoke(args)
            .await;
        }
        let resolved = match self.state.snapshots.resolve(pid as i32, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let elem_idx_resolved = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { element_index, .. } => {
                Some(*element_index)
            }
            pincer_core::element_token::ResolvedElement::None => None,
        };
        let window_id_resolved: Option<u64> = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            pincer_core::element_token::ResolvedElement::None => args.opt_u64("window_id"),
        };
        if let Some(idx) = elem_idx_resolved {
            let xid_hint = window_id_resolved;
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<(u64, f64, f64)> {
                checked_element_local_coords(pid, idx, xid_hint)
            })
            .await;
            return match result {
                Ok(Ok((xid, lx, ly))) => {
                    if let Ok(Ok(((sx, sy), rect))) = tokio::task::spawn_blocking(move || {
                        element_screen_center_and_rect(pid, idx, Some(xid))
                    })
                    .await
                    {
                        crate::overlay::send_command_for(
                            cursor_id.clone(),
                            cursor_overlay::OverlayCommand::PinAbove(xid),
                        );
                        reveal_pointer_action_target_for(
                            &self.state,
                            &cursor_id,
                            sx,
                            sy,
                            true,
                            Some(rect),
                        )
                        .await;
                    }
                    let lxi = lx as i32;
                    let lyi = ly as i32;
                    let wayland_point = crate::wayland::wayland_input_enabled()
                        .then(|| crate::wayland::window_local_to_output(xid, lxi, lyi));
                    let cursor_id_for_task = cursor_id.clone();
                    let click_result =
                        tokio::task::spawn_blocking(move || -> anyhow::Result<PointerRoute> {
                            if crate::wayland::is_inject_mode() {
                                return crate::wayland::inject_click(pid, xid, lx, ly, 2, 1)
                                    .map(|()| PointerRoute::Wayland);
                            }
                            if crate::wayland::wayland_input_enabled() {
                                let (output_x, output_y) = wayland_point.unwrap_or((lxi, lyi));
                                return crate::wayland::click(xid, output_x, output_y, 2, 1)
                                    .map(|()| PointerRoute::Wayland);
                            }
                            if delivery.is_foreground() {
                                // The result never reports the foreground
                                // observation (`PointerRoute::Foreground`
                                // carries none of it), so skip it.
                                return crate::input::with_x11_foreground_unobserved(
                                    xid,
                                    80,
                                    || {
                                        let (sx, sy) =
                                            window_local_to_screen(xid, lxi as f64, lyi as f64)?;
                                        crate::input::send_click_xtest_desktop(
                                            sx.round() as i32,
                                            sy.round() as i32,
                                            1,
                                            2,
                                        )
                                    },
                                )
                                .map(|()| PointerRoute::Foreground);
                            }
                            x11_pixel_click_no_focus_steal(&cursor_id_for_task, xid, lxi, lyi, 1, 2)
                        })
                        .await;
                    let mode_label = if delivery.is_foreground() {
                        "foreground"
                    } else {
                        "background"
                    };
                    match click_result {
                        Ok(Ok(route)) => ToolResult::text(format!(
                            "Double-clicked element [{idx}] {}",
                            route.text_suffix(mode_label)
                        ))
                        .with_structured(route.structured(mode_label)),
                        Ok(Err(e)) => input_error_result(e),
                        Err(e) => ToolResult::error(format!("Task error: {e}")),
                    }
                }
                Ok(Err(e)) => input_error_result(e.context("AT-SPI bounds failed")),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }
        if let Some(refusal) = require_point_args("double_click", &args) {
            return refusal;
        }
        let xid = match window_id_resolved {
            Some(v) => v,
            None => return ToolResult::error("Provide either element_token or pid + window_id + x/y."),
        };
        let desktop_frame = desktop_frame_requested(&args);
        let from_zoom = args.bool_or("from_zoom", false);
        let mut x = args.f64_or("x", 0.0);
        let mut y = args.f64_or("y", 0.0);
        if desktop_frame {
            // Desktop pixels: no zoom/screenshot scaling; translated once below.
        } else if from_zoom {
            match self.state.zoom_context(&args, pid, Some(xid)) {
                Ok(ctx) => {
                    let (wx, wy) = ctx.zoom_to_window(x, y);
                    x = wx;
                    y = wy;
                }
                Err(refusal) => return refusal,
            }
        } else {
            let ratio = match screenshot_scale(&self.state, &args, pid, Some(xid)) {
                Ok(ratio) => ratio,
                Err(refusal) => return refusal,
            };
            x *= ratio;
            y *= ratio;
        }
        if desktop_frame {
            match tokio::task::spawn_blocking(move || desktop_to_window_local(xid, x, y)).await {
                Ok(Ok((lx, ly))) => {
                    x = lx;
                    y = ly;
                }
                Ok(Err(e)) => {
                    return ToolResult::error(format!(
                        "desktop-frame coordinates could not be mapped into window {xid}: {e}"
                    ))
                }
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            }
        }
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
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
            reveal_pointer_action_for(&self.state, &cursor_id, sx, sy, true).await;
        }
        let (xi, yi) = (x as i32, y as i32);
        let cursor_id_for_task = cursor_id.clone();
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<PointerRoute> {
            if crate::wayland::is_inject_mode() {
                return crate::wayland::inject_click(pid, xid, x, y, 2, 1)
                    .map(|()| PointerRoute::Wayland);
            }
            if crate::wayland::wayland_input_enabled() {
                let (output_x, output_y) = wayland_output_point.unwrap_or((xi, yi));
                return crate::wayland::click(xid, output_x, output_y, 2, 1)
                    .map(|()| PointerRoute::Wayland);
            }
            if delivery.is_foreground() {
                // Unobserved: the result never reports the foreground
                // observation (`PointerRoute::Foreground` carries none of it).
                return crate::input::with_x11_foreground_unobserved(xid, 80, || {
                    // Real XTest double-click at the screen point — synthetic
                    // XSendEvent button events are dropped by GTK/Qt and the MPX
                    // uinput path needs /dev/uinput (absent on Xvfb/Xtigervnc), so
                    // neither lands. Mirrors the single-click foreground path.
                    if let Ok((sx, sy)) = window_local_to_screen(xid, xi as f64, yi as f64) {
                        crate::input::send_click_xtest_desktop(
                            sx.round() as i32,
                            sy.round() as i32,
                            1,
                            2,
                        )?;
                        return Ok(());
                    }
                    x11_pixel_click_no_focus_steal(&cursor_id_for_task, xid, xi, yi, 1, 2)
                        .map(|_| ())
                })
                .map(|()| PointerRoute::Foreground);
            }
            x11_pixel_click_no_focus_steal(&cursor_id_for_task, xid, xi, yi, 1, 2)
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        let outcome = match result {
            Ok(Ok(route)) => ToolResult::text(format!(
                "Double-clicked at ({x:.1}, {y:.1}) {}",
                route.text_suffix(mode_label)
            ))
            .with_structured(route.structured(mode_label)),
            Ok(Err(e)) => input_error_result(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        };
        with_pixel_points(
            outcome,
            glide_target.map(|(sx, sy)| (sx.round() as i32, sy.round() as i32)),
            (xi, yi),
        )
    }
}

// ── right_click ───────────────────────────────────────────────────────────────

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

/// Foreground right-click at window-local `(lx, ly)`: activate the window,
/// press button 3 there via XTest, then name the context menu it opened so
/// the result carries `popup` as the background route's does. Blocking.
fn foreground_right_click(pid: u32, xid: u64, lx: f64, ly: f64) -> anyhow::Result<PointerRoute> {
    let before: Vec<u64> = crate::input::mapped_popup_windows()
        .iter()
        .map(|popup| popup.window)
        .collect();
    // Unobserved: the popup wait below is this route's observation.
    crate::input::with_x11_foreground_unobserved(xid, 80, || {
        // Real XTest right-click at the screen point (synthetic XSendEvent is
        // dropped by GTK/Qt; MPX needs /dev/uinput). Mirrors the single-click
        // foreground path.
        let (sx, sy) = window_local_to_screen(xid, lx, ly)?;
        crate::input::send_click_xtest_desktop(sx.round() as i32, sy.round() as i32, 3, 1)
    })?;
    let popups = wait_for_new_popups(pid, &before, CONTEXT_MENU_WAIT);
    Ok(if popups.is_empty() {
        PointerRoute::Foreground
    } else {
        PointerRoute::ForegroundPopups(popups)
    })
}

pub struct RightClickTool {
    state: Arc<ToolState>,
}
static RCLICK_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for RightClickTool {
    fn def(&self) -> &ToolDef {
        RCLICK_DEF.get_or_init(|| ToolDef {
            name: "right_click".into(),
            description: "Right-click at window pixel (x, y) or an element_token. Pass pid + \
                window_id + x,y, or an element_token (pid optional); after a zoom, from_zoom=true \
                maps zoom-image coordinates \
                back. Background delivery does not steal focus; where no focus-free pointer \
                route exists (a GTK3 canvas, for example) it is refused with \
                background_unavailable: retry with delivery_mode foreground. In background, a point \
                covered by another app's \
                window is refused (target_occluded), one covered by this app's own dialog goes to \
                the dialog (retargeted_to), and one outside the window is refused \
                (point_outside_window); results report window_point and screen_point. When Pincer \
                can identify the context menu it opened (in either delivery mode), the result \
                includes `popup: {window_id, \
                bounds, title}`: read it with get_window_state(pid, window_id) and click its \
                items by element_token. Without `popup`, a menu may still be open even though \
                window screenshots never show popup menus; press Escape on the window to close \
                it.".into(),
            input_schema: json!({"type":"object","required":[],"properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Target process ID. Optional with element_token (the token carries the pid)."},
                "window_id":{"type":"integer","description":"Window id from list_windows. Required with x/y; optional with element_token (the token carries it)."},
                "x":{"type":"number","description":"Pixel X in the window's get_window_state screenshot (0..screenshot_width). Pass scope:\"desktop\" for get_desktop_state pixels."},
                "y":{"type":"number","description":"Pixel Y in the window's get_window_state screenshot (0..screenshot_height); see x."},
                "coordinate_frame": coordinate_frame_schema(),
                "element_token": pincer_core::tool_schema::element_token_schema(),
                "modifier": pincer_core::tool_schema::modifier_schema(),
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
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
            // Share exact-target validation and the admitted native click lifecycle.
            let mut args = args;
            args["button"] = json!("right");
            args["count"] = json!(1);
            return ClickTool {
                state: self.state.clone(),
            }
            .invoke(args)
            .await;
        }
        let resolved = match self.state.snapshots.resolve(pid as i32, &args) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let elem_idx_resolved = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { element_index, .. } => {
                Some(*element_index)
            }
            pincer_core::element_token::ResolvedElement::None => None,
        };
        let window_id_resolved: Option<u64> = match &resolved {
            pincer_core::element_token::ResolvedElement::Element { window_id, .. } => {
                Some(*window_id)
            }
            pincer_core::element_token::ResolvedElement::None => args.opt_u64("window_id"),
        };
        if let Some(idx) = elem_idx_resolved {
            let xid_hint = window_id_resolved;
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<(u64, f64, f64)> {
                checked_element_local_coords(pid, idx, xid_hint)
            })
            .await;
            return match result {
                Ok(Ok((xid, lx, ly))) => {
                    if let Ok(Ok(((sx, sy), rect))) = tokio::task::spawn_blocking(move || {
                        element_screen_center_and_rect(pid, idx, Some(xid))
                    })
                    .await
                    {
                        crate::overlay::send_command_for(
                            cursor_id.clone(),
                            cursor_overlay::OverlayCommand::PinAbove(xid),
                        );
                        reveal_pointer_action_target_for(
                            &self.state,
                            &cursor_id,
                            sx,
                            sy,
                            true,
                            Some(rect),
                        )
                        .await;
                    }
                    let lxi = lx as i32;
                    let lyi = ly as i32;
                    let wayland_point = crate::wayland::wayland_input_enabled()
                        .then(|| crate::wayland::window_local_to_output(xid, lxi, lyi));
                    let cursor_id_for_task = cursor_id.clone();
                    let click_result =
                        tokio::task::spawn_blocking(move || -> anyhow::Result<PointerRoute> {
                            if crate::wayland::is_inject_mode() {
                                return crate::wayland::inject_click(pid, xid, lx, ly, 1, 3)
                                    .map(|()| PointerRoute::Wayland);
                            }
                            if crate::wayland::wayland_input_enabled() {
                                let (output_x, output_y) = wayland_point.unwrap_or((lxi, lyi));
                                return crate::wayland::click(xid, output_x, output_y, 1, 3)
                                    .map(|()| PointerRoute::Wayland);
                            }
                            if delivery.is_foreground() {
                                return foreground_right_click(pid, xid, lxi as f64, lyi as f64);
                            }
                            x11_pixel_click_no_focus_steal(&cursor_id_for_task, xid, lxi, lyi, 3, 1)
                        })
                        .await;
                    let mode_label = if delivery.is_foreground() {
                        "foreground"
                    } else {
                        "background"
                    };
                    match click_result {
                        Ok(Ok(route)) => ToolResult::text(format!(
                            "Right-clicked element [{idx}] {}",
                            route.text_suffix(mode_label)
                        ))
                        .with_structured(route.structured(mode_label)),
                        Ok(Err(e)) => input_error_result(e),
                        Err(e) => ToolResult::error(format!("Task error: {e}")),
                    }
                }
                Ok(Err(e)) => input_error_result(e.context("AT-SPI bounds failed")),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }
        if let Some(refusal) = require_point_args("right_click", &args) {
            return refusal;
        }
        let xid = match window_id_resolved {
            Some(v) => v,
            None => return ToolResult::error("Provide either element_token or pid + window_id + x/y."),
        };
        let desktop_frame = desktop_frame_requested(&args);
        let from_zoom = args.bool_or("from_zoom", false);
        let mut x = args.f64_or("x", 0.0);
        let mut y = args.f64_or("y", 0.0);
        if desktop_frame {
            // Desktop pixels: no zoom/screenshot scaling; translated once below.
        } else if from_zoom {
            match self.state.zoom_context(&args, pid, Some(xid)) {
                Ok(ctx) => {
                    let (wx, wy) = ctx.zoom_to_window(x, y);
                    x = wx;
                    y = wy;
                }
                Err(refusal) => return refusal,
            }
        } else {
            let ratio = match screenshot_scale(&self.state, &args, pid, Some(xid)) {
                Ok(ratio) => ratio,
                Err(refusal) => return refusal,
            };
            x *= ratio;
            y *= ratio;
        }
        if desktop_frame {
            match tokio::task::spawn_blocking(move || desktop_to_window_local(xid, x, y)).await {
                Ok(Ok((lx, ly))) => {
                    x = lx;
                    y = ly;
                }
                Ok(Err(e)) => {
                    return ToolResult::error(format!(
                        "desktop-frame coordinates could not be mapped into window {xid}: {e}"
                    ))
                }
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            }
        }
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
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
            reveal_pointer_action_for(&self.state, &cursor_id, sx, sy, true).await;
        }
        let (xi, yi) = (x as i32, y as i32);
        let cursor_id_for_task = cursor_id.clone();
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<PointerRoute> {
            if crate::wayland::is_inject_mode() {
                return crate::wayland::inject_click(pid, xid, x, y, 1, 3)
                    .map(|()| PointerRoute::Wayland);
            }
            if crate::wayland::wayland_input_enabled() {
                let (output_x, output_y) = wayland_output_point.unwrap_or((xi, yi));
                return crate::wayland::click(xid, output_x, output_y, 1, 3)
                    .map(|()| PointerRoute::Wayland);
            }
            if delivery.is_foreground() {
                return foreground_right_click(pid, xid, xi as f64, yi as f64);
            }
            x11_pixel_click_no_focus_steal(&cursor_id_for_task, xid, xi, yi, 3, 1)
        })
        .await;
        let mode_label = if delivery.is_foreground() {
            "foreground"
        } else {
            "background"
        };
        let outcome = match result {
            Ok(Ok(route)) => ToolResult::text(format!(
                "Right-clicked at ({x:.1}, {y:.1}) {}",
                route.text_suffix(mode_label)
            ))
            .with_structured(route.structured(mode_label)),
            Ok(Err(e)) => input_error_result(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        };
        with_pixel_points(
            outcome,
            glide_target.map(|(sx, sy)| (sx.round() as i32, sy.round() as i32)),
            (xi, yi),
        )
    }
}

// ── drag ─────────────────────────────────────────────────────────────────────

/// Typed refusal for a modified drag on native Wayland. The Wayland pointer
/// routes (virtual pointer, libei, and the nested cua-compositor socket) do not
/// hold keyboard modifier state across a pointer gesture, so the drag is
/// refused before any input is dispatched rather than degraded to an
/// unmodified drag. The stable `code` lets callers branch without parsing text.
fn wayland_modified_drag_refusal() -> ToolResult {
    const DETAIL: &str = "the pointer route cannot carry keyboard modifier state";
    ToolResult::error(format!(
        "modified drags are unavailable on native Wayland: {DETAIL}"
    ))
    .with_structured(json!({
        "code": "modified_pointer_unavailable",
        "effect": "refused",
        "verified": false,
        "detail": DETAIL,
    }))
}

pub struct DragTool {
    state: Arc<ToolState>,
}
static DRAG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for DragTool {
    fn has_independent_input_lane(&self, args: &Value) -> bool {
        // This opt-in branch below either uses its own leased synthetic seat
        // or refuses. It never falls back to primary-seat input. All ordinary
        // Linux routes retain the common process-wide input coordinator.
        // Socket availability can change between admission and invocation.
        // Native Hyprland window drags remain fail-closed on their own lane.
        args.opt_str("scope").as_deref() != Some("desktop")
            && !crate::input::delivery::DeliveryMode::from_args(args).is_foreground()
            && crate::wayland::wayland_input_enabled()
            && crate::wayland::hyprland::is_session()
    }

    fn def(&self) -> &ToolDef {
        DRAG_DEF.get_or_init(|| ToolDef {
            name: "drag".into(),
            description: "Press, move and release from (from_x, from_y) to (to_x, to_y) in \
                          window screenshot pixels, without stealing focus. Window points need a \
                          get_window_state screenshot of the window earlier in the session \
                          (screenshot_context_missing otherwise). In background, a point covered by \
                          another app's window is refused (target_occluded); one covered by this \
                          app's own dialog goes to the dialog. Background drags need a pointer \
                          backend some apps lack even where clicks work, so a drag can return \
                          background_unavailable; it then needs delivery_mode foreground.".into(),
            input_schema: json!({"type":"object","required":["from_x","from_y","to_x","to_y"],"properties":{
                "session": pincer_core::tool_schema::session_schema(),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Target process ID. Required unless scope is \"desktop\"."},
                "window_id":{"type":"integer","description":"Target window XID. Required unless scope is \"desktop\"."},
                "from_x":{"type":"number","description":"Start X in the window's get_window_state screenshot pixels; pass scope:\"desktop\" for get_desktop_state pixels."},
                "from_y":{"type":"number","description":"Drag start Y (same frame as from_x)."},
                "to_x":{"type":"number","description":"Drag end X (same frame as from_x)."},
                "to_y":{"type":"number","description":"Drag end Y (same frame as from_x)."},
                "duration_ms":{"type":"integer","minimum":0,"maximum":10000,"description":"Total drag duration in ms. Default: 500."},
                "steps":{"type":"integer","minimum":1,"maximum":200,"description":"Intermediate motion events. Default: 20."},
                "modifier": pincer_core::tool_schema::modifier_schema(),
                "button": pincer_core::tool_schema::button_schema(),
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "scope":{"type":"string","enum":["window","desktop"],"default":"window","description":"Use \"desktop\" with no pid/window_id for get_desktop_state screen coordinates. Default \"window\"."},
                "coordinate_frame": coordinate_frame_schema(),
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        let modifiers: Vec<String> = args.str_array("modifier");
        if args.opt_str("scope").as_deref() == Some("desktop")
            && args.get("pid").is_none()
            && args.get("window_id").is_none()
        {
            let input = match parse_typed_projection::<DragInput>("drag", &args) {
                Ok(input) => input,
                Err(result) => return result,
            };
            let (from_x, from_y) = (input.from_x, input.from_y);
            let (to_x, to_y) = (input.to_x, input.to_y);
            let button = parse_mouse_button(input.button.unwrap_or(ClickButton::Left).as_str());
            let duration_ms = input.duration_ms.unwrap_or(500).min(10_000);
            let steps = input.steps.unwrap_or(20).clamp(1, 200) as usize;
            let modifiers = input.modifier.unwrap_or_default();
            let wayland = crate::wayland::wayland_input_enabled();
            if wayland && !modifiers.is_empty() {
                return wayland_modified_drag_refusal();
            }
            let path = if wayland { "wayland_desktop" } else { "xtest" };
            let space = match desktop_input_space().await {
                Ok(space) => space,
                Err(error) => return ToolResult::error(error.to_string()),
            };
            // The overlay draws in layout coordinates, like the input below.
            let overlay_point = |x: f64, y: f64| {
                let (lx, ly) = space.to_layout(x.round() as i32, y.round() as i32);
                (f64::from(lx), f64::from(ly))
            };
            let (overlay_from, overlay_to) =
                (overlay_point(from_x, from_y), overlay_point(to_x, to_y));
            // Glide to the start before the real pointer jumps there; the
            // tracking below then follows the drag itself.
            reveal_pointer_action_for(
                &self.state,
                &cursor_id,
                overlay_from.0,
                overlay_from.1,
                false,
            )
            .await;
            let result = tokio::task::spawn_blocking(move || {
                if wayland {
                    crate::wayland::drag_desktop(
                        &space,
                        from_x.round() as i32,
                        from_y.round() as i32,
                        to_x.round() as i32,
                        to_y.round() as i32,
                        steps as u32,
                        duration_ms,
                        button,
                    )
                } else {
                    let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                    crate::input::send_drag_xtest_desktop_with_modifiers(
                        from_x.round() as i32,
                        from_y.round() as i32,
                        to_x.round() as i32,
                        to_y.round() as i32,
                        button,
                        duration_ms,
                        steps,
                        &modifier_refs,
                    )
                }
            });
            let visual_drag = track_overlay_drag_for(
                cursor_id.clone(),
                overlay_from,
                overlay_to,
                duration_ms,
                steps,
            );
            let (result, ()) = tokio::join!(result, visual_drag);
            if matches!(&result, Ok(Ok(()))) {
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, overlay_to.0, overlay_to.1);
            }
            return match result {
                Ok(Ok(())) => ToolResult::text("Dragged on the desktop.").with_structured(
                    json!({"scope":"desktop","path":path,"effect":"unverifiable"}),
                ),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }
        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let xid = match args.opt_u64("window_id") {
            Some(v) => v,
            None => return ToolResult::error("window_id is required on Linux."),
        };
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let isolated_background = isolated_hyprland_background(delivery);
        let native_refusal = if !isolated_background {
            if crate::wayland::wayland_input_enabled() && !modifiers.is_empty() {
                return wayland_modified_drag_refusal();
            }
            if let Some(refusal) = unavailable_chromium_background(pid, delivery) {
                return refusal;
            }
            if let Some(refusal) = unavailable_webkit_background(pid, delivery) {
                return refusal;
            }
            if let Some(refusal) = unavailable_gtk_pointer_background(pid, delivery) {
                return refusal;
            }
            unavailable_wayland_focused_input_background(delivery, true)
        } else {
            None
        };

        let from_zoom = args.bool_or("from_zoom", false);
        let desktop_frame = desktop_frame_requested(&args);
        let context = match coordinate_drag_context(native_refusal, || {
            if desktop_frame {
                // Desktop pixels: no zoom/screenshot scaling; translated below.
                Ok(CoordinateContext::Screenshot(1.0))
            } else if from_zoom {
                self.state
                    .zoom_context(&args, pid, Some(xid))
                    .map(CoordinateContext::Zoom)
            } else {
                screenshot_scale(&self.state, &args, pid, Some(xid))
                    .map(CoordinateContext::Screenshot)
            }
        }) {
            Ok(context) => context,
            Err(refusal) => return refusal,
        };

        let coerce = |key: &str| -> Option<f64> {
            args.opt_f64(key)
                .or_else(|| args.opt_i64(key).map(|i| i as f64))
        };
        let mut from_x = match coerce("from_x") {
            Some(v) => v,
            None => {
                return invalid_pointer_arguments(
                    "drag",
                    "from_x is missing (from_x/from_y/to_x/to_y are all required)",
                )
            }
        };
        let mut from_y = match coerce("from_y") {
            Some(v) => v,
            None => {
                return invalid_pointer_arguments(
                    "drag",
                    "from_y is missing (from_x/from_y/to_x/to_y are all required)",
                )
            }
        };
        let mut to_x = match coerce("to_x") {
            Some(v) => v,
            None => return ToolResult::error("Missing: to_x"),
        };
        let mut to_y = match coerce("to_y") {
            Some(v) => v,
            None => return ToolResult::error("Missing: to_y"),
        };

        let duration_ms = args.u64_or("duration_ms", 500);
        let steps = args.u64_or("steps", 20) as usize;
        let button_str = args.str_or("button", "left");
        let button = parse_mouse_button(button_str.as_str());
        match context {
            CoordinateContext::Zoom(context) => {
                (from_x, from_y) = context.zoom_to_window(from_x, from_y);
                (to_x, to_y) = context.zoom_to_window(to_x, to_y);
            }
            CoordinateContext::Screenshot(scale) => {
                from_x *= scale;
                from_y *= scale;
                to_x *= scale;
                to_y *= scale;
            }
        }
        if desktop_frame {
            let mapped = tokio::task::spawn_blocking(move || {
                Ok::<_, anyhow::Error>((
                    desktop_to_window_local(xid, from_x, from_y)?,
                    desktop_to_window_local(xid, to_x, to_y)?,
                ))
            })
            .await;
            match mapped {
                Ok(Ok(((fx, fy), (tx, ty)))) => {
                    from_x = fx;
                    from_y = fy;
                    to_x = tx;
                    to_y = ty;
                }
                Ok(Err(e)) => {
                    return ToolResult::error(format!(
                        "desktop-frame coordinates could not be mapped into window {xid}: {e}"
                    ))
                }
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            }
        }

        if hyprland_foreground(delivery) {
            if button_str != "left" || args.get("modifier").is_some_and(|value| !value.is_null()) {
                return foreground_hyprland_refusal(
                    "foreground drag supports only an unmodified left button",
                );
            }
            if !crate::wayland::hyprland_input::enabled() {
                return foreground_hyprland_refusal(
                    "production Hyprland input plugin is unavailable",
                );
            }
            let owner = named_session_cursor_key(&args);
            let from = crate::wayland::window_local_to_output(
                xid,
                from_x.round() as i32,
                from_y.round() as i32,
            );
            let to = crate::wayland::window_local_to_output(
                xid,
                to_x.round() as i32,
                to_y.round() as i32,
            );
            let (started, acknowledged) = tokio::sync::oneshot::channel();
            let (_cancellation, mut dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_foreground_with_started(
                        owner,
                        pid,
                        xid,
                        crate::wayland::hyprland_input::Action::Drag {
                            from: (from_x, from_y),
                            to: (to_x, to_y),
                            duration_ms,
                        },
                        Some(started),
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
            let acknowledged = acknowledged.await.is_ok();
            let result = if acknowledged {
                overlay_snap_to_for(&cursor_id, from.0 as f64, from.1 as f64, None);
                tokio::select! {
                    result = &mut dispatch => result,
                    () = track_overlay_drag_for(cursor_id.clone(), (from.0 as f64, from.1 as f64),
                        (to.0 as f64, to.1 as f64), duration_ms, steps) => dispatch.await,
                }
            } else {
                dispatch.await
            };
            return hyprland_input_result(
                match result {
                    Ok(result) => result,
                    Err(error) => Err(crate::wayland::hyprland_input::unknown_dispatch(
                        error.into(),
                        u32::from(acknowledged),
                    )),
                },
                true,
            );
        }

        if isolated_background {
            if button_str != "left" || args.get("modifier").is_some_and(|value| !value.is_null()) {
                return isolated_hyprland_refusal(
                    "isolated drag supports only an unmodified left button",
                );
            }
            let owner = named_session_cursor_key(&args);
            let from = crate::wayland::window_local_to_output(
                xid,
                from_x.round() as i32,
                from_y.round() as i32,
            );
            let to = crate::wayland::window_local_to_output(
                xid,
                to_x.round() as i32,
                to_y.round() as i32,
            );
            let (started, acknowledged) = tokio::sync::oneshot::channel();
            let (_cancellation, mut dispatch) =
                match spawn_isolated_hyprland(&args, move |cancellation| {
                    crate::wayland::hyprland_input::execute_with_started(
                        owner,
                        pid,
                        xid,
                        crate::wayland::hyprland_input::Action::Drag {
                            from: (from_x, from_y),
                            to: (to_x, to_y),
                            duration_ms,
                        },
                        Some(started),
                        cancellation,
                    )
                }) {
                    Ok(dispatch) => dispatch,
                    Err(refusal) => return refusal,
                };
            // Do not animate a drag that was refused. The compositor starts
            // the overlay clock only after accepting and pressing the button.
            let acknowledged = acknowledged.await.is_ok();
            if acknowledged {
                overlay_snap_to_for(&cursor_id, from.0 as f64, from.1 as f64, None);
                tokio::select! {
                    result = &mut dispatch => {
                        return match result { Ok(result) => isolated_hyprland_result(result), Err(error) => isolated_hyprland_task_error(error, acknowledged) };
                    }
                    () = track_overlay_drag_for(cursor_id.clone(), (from.0 as f64, from.1 as f64),
                        (to.0 as f64, to.1 as f64), duration_ms, steps) => {}
                }
            }
            return match dispatch.await {
                Ok(result) => isolated_hyprland_result(result),
                Err(error) => isolated_hyprland_task_error(error, acknowledged),
            };
        }

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        let wayland_points = crate::wayland::wayland_input_enabled().then(|| {
            (
                crate::wayland::window_local_to_output(
                    xid,
                    from_x.round() as i32,
                    from_y.round() as i32,
                ),
                crate::wayland::window_local_to_output(
                    xid,
                    to_x.round() as i32,
                    to_y.round() as i32,
                ),
            )
        });
        let screen_from = if let Some((from, _)) = wayland_points {
            Some((from.0 as f64, from.1 as f64))
        } else {
            tokio::task::spawn_blocking(move || window_local_to_screen(xid, from_x, from_y))
                .await
                .ok()
                .and_then(|result| result.ok())
        };
        if let Some((sx_from, sy_from)) = screen_from {
            overlay_glide_to_for(&cursor_id, sx_from, sy_from).await;
            self.state
                .cursor_registry
                .update_position(&cursor_id, sx_from, sy_from);
            overlay_snap_to_for(&cursor_id, sx_from, sy_from, None);
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::ClickPulse {
                    x: sx_from,
                    y: sy_from,
                },
            );
        }
        // Native Wayland: emit press + interpolated motion + release as one
        // virtual-pointer (wlroots) or libei (GNOME/KDE) sequence, output-relative
        // coords. Returns early so we don't fall into the X11 XSendEvent loop below.
        if crate::wayland::wayland_input_enabled() {
            let steps_u32 = steps as u32;
            let drag_result = if crate::wayland::is_inject_mode() {
                tokio::task::spawn_blocking(move || {
                    crate::wayland::inject_drag(
                        pid,
                        xid,
                        (from_x, from_y),
                        (to_x, to_y),
                        steps,
                        button as u32,
                    )
                })
            } else {
                let ((fxi, fyi), (txi, tyi)) = wayland_points.unwrap_or((
                    (from_x.round() as i32, from_y.round() as i32),
                    (to_x.round() as i32, to_y.round() as i32),
                ));
                tokio::task::spawn_blocking(move || {
                    crate::wayland::drag(
                        xid,
                        (fxi, fyi),
                        (txi, tyi),
                        steps_u32,
                        duration_ms,
                        button,
                    )
                })
            };
            let ((from_output_x, from_output_y), (to_output_x, to_output_y)) = wayland_points
                .unwrap_or((
                    (from_x.round() as i32, from_y.round() as i32),
                    (to_x.round() as i32, to_y.round() as i32),
                ));
            let visual_drag = track_overlay_drag_for(
                cursor_id.clone(),
                (f64::from(from_output_x), f64::from(from_output_y)),
                (f64::from(to_output_x), f64::from(to_output_y)),
                duration_ms,
                steps,
            );
            let (drag_result, ()) = tokio::join!(drag_result, visual_drag);
            if matches!(&drag_result, Ok(Ok(()))) {
                self.state.cursor_registry.update_position(
                    &cursor_id,
                    f64::from(to_output_x),
                    f64::from(to_output_y),
                );
            }
            return match drag_result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "✅ Posted drag ({button_str}) to pid {pid} \
                     from ({from_x:.0}, {from_y:.0}) → ({to_x:.0}, {to_y:.0}) \
                     in {duration_ms}ms / {steps} steps."
                )),
                Ok(Err(e)) => input_error_result(e),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        if delivery.is_foreground() {
            let screen_points = tokio::task::spawn_blocking(move || {
                Ok::<_, anyhow::Error>((
                    window_local_to_screen(xid, from_x, from_y)?,
                    window_local_to_screen(xid, to_x, to_y)?,
                ))
            })
            .await;
            let ((screen_from_x, screen_from_y), (screen_to_x, screen_to_y)) = match screen_points {
                Ok(Ok(points)) => points,
                Ok(Err(e)) => return ToolResult::error(e.to_string()),
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            };
            let drag_result = tokio::task::spawn_blocking(move || {
                let modifier_refs: Vec<&str> = modifiers.iter().map(String::as_str).collect();
                // Unobserved: the result below never reports the foreground
                // observation. The agent cursor's drag runs alongside and is
                // awaited on its own.
                crate::input::with_x11_foreground_unobserved(xid, 80, || {
                    crate::input::send_drag_xtest_desktop_with_modifiers(
                        screen_from_x.round() as i32,
                        screen_from_y.round() as i32,
                        screen_to_x.round() as i32,
                        screen_to_y.round() as i32,
                        button,
                        duration_ms,
                        steps,
                        &modifier_refs,
                    )
                })
            });
            let visual_drag = track_overlay_drag_for(
                cursor_id.clone(),
                (screen_from_x, screen_from_y),
                (screen_to_x, screen_to_y),
                duration_ms,
                steps,
            );
            let (drag_result, ()) = tokio::join!(drag_result, visual_drag);
            if matches!(&drag_result, Ok(Ok(()))) {
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, screen_to_x, screen_to_y);
            }
            return match drag_result {
                Ok(Ok(())) => ToolResult::text(format!(
                    "Dragged ({button_str}) to pid {pid} from ({from_x:.0}, {from_y:.0}) \
                     to ({to_x:.0}, {to_y:.0}) in {duration_ms}ms / {steps} steps \
                     (delivery_mode=foreground, path=x11_xtest_fg); not verified: {POINTER_CHECK}."
                ))
                .with_structured(json!({
                    "path": "x11_xtest_fg",
                    "verified": false,
                    "delivery_mode": "foreground"
                })),
                Ok(Err(e)) => input_error_result(e),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // Background X11 drag. With a real Xorg server and /dev/uinput the
        // gesture is a held press + interpolated motion + release on the
        // session's MPX virtual master pointer (GTK/VCL/Qt/Chromium drop the
        // synthetic XSendEvent gesture below); the target's screen points are
        // resolved once and the agent cursor tracks the glide.
        if crate::input::real_pointer_input_available() {
            let screen_points = tokio::task::spawn_blocking(move || {
                Ok::<_, anyhow::Error>((
                    window_local_to_screen(xid, from_x, from_y)?,
                    window_local_to_screen(xid, to_x, to_y)?,
                ))
            })
            .await;
            let ((sfx, sfy), (stx, sty)) = match screen_points {
                Ok(Ok(points)) => points,
                Ok(Err(e)) => return ToolResult::error(format!("{e:#}")),
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            };
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::SetPressed(true),
            );
            let cursor_id_for_task = cursor_id.clone();
            let drag_result = tokio::task::spawn_blocking(move || {
                let (outcome, guard) =
                    crate::input::focus_guard::guarded_settled(Some(pid), || {
                        Ok(crate::input::send_virtual_pointer_drag(
                            &cursor_id_for_task,
                            &crate::input::VirtualPointerDrag {
                                target_window: xid,
                                button,
                                path: vec![
                                    (sfx.round() as i32, sfy.round() as i32),
                                    (stx.round() as i32, sty.round() as i32),
                                ],
                                duration_ms,
                                steps,
                            },
                        ))
                    })?;
                outcome.map(|mut effect| {
                    effect.focus_guard = guard;
                    effect
                })
            })
            .await;
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::SetPressed(false),
            );
            if matches!(&drag_result, Ok(Ok(_))) {
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::track_pointer_command(stx, sty),
                );
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, stx, sty);
            }
            return match drag_result {
                Ok(Ok(effect)) => {
                    let route = PointerRoute::Mpx(effect);
                    ToolResult::text(format!(
                        "Dragged ({button_str}) pid {pid} from ({from_x:.0}, {from_y:.0}) to \
                         ({to_x:.0}, {to_y:.0}) in {duration_ms}ms / {steps} steps with a real \
                         held pointer gesture {}",
                        route.text_suffix("background")
                    ))
                    .with_structured(route.structured("background"))
                }
                Ok(Err(e)) => input_error_result(e),
                Err(e) => ToolResult::error(format!("Task error: {e}")),
            };
        }

        // Legacy target-addressed XSendEvent gesture (headless servers / no
        // uinput). GTK and LibreOffice drop it; Qt takes it (a synthetic drag
        // selected text in KWrite and in a Qt 5.15 text edit while another
        // window was active, 2026-11-11), so Qt is no longer refused here.
        if synthetic_press_is_dropped(pid) {
            return crate::input::delivery::background_unavailable_error(
                crate::input::delivery::BackgroundUnavailable::SyntheticPointerDropped,
            );
        }
        // Qt asks the window manager to activate a window that gets a press
        // (KWrite became the active window after that drag): snapshot the
        // focus before the press and restore it after the release, as the
        // synthetic click does.
        let focus_snapshot =
            tokio::task::spawn_blocking(crate::input::focus_guard::FocusSnapshot::capture)
                .await
                .ok()
                .flatten();
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::SetPressed(true),
        );
        let press_result = tokio::task::spawn_blocking(move || {
            crate::input::send_button_down(
                xid,
                from_x.round() as i32,
                from_y.round() as i32,
                button,
            )
        })
        .await;
        let mut result: anyhow::Result<()> = match press_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(anyhow::anyhow!("Task error: {e}")),
        };

        if result.is_ok() {
            let step_delay_ms = if steps > 1 {
                duration_ms / steps as u64
            } else {
                duration_ms
            };
            for i in 1..=steps {
                let t = i as f64 / steps.max(1) as f64;
                let ix = from_x + (to_x - from_x) * t;
                let iy = from_y + (to_y - from_y) * t;
                let motion_result = tokio::task::spawn_blocking(move || {
                    crate::input::send_motion(
                        xid,
                        ix.round() as i32,
                        iy.round() as i32,
                        Some(button),
                    )
                })
                .await;
                match motion_result {
                    Ok(Ok(())) => {
                        if let Ok(Ok((sx, sy))) =
                            tokio::task::spawn_blocking(move || window_local_to_screen(xid, ix, iy))
                                .await
                        {
                            self.state
                                .cursor_registry
                                .update_position(&cursor_id, sx, sy);
                            crate::overlay::send_command_for(
                                cursor_id.clone(),
                                cursor_overlay::track_pointer_command(sx, sy),
                            );
                        }
                        if step_delay_ms > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(step_delay_ms))
                                .await;
                        }
                    }
                    Ok(Err(e)) => {
                        result = Err(e);
                        break;
                    }
                    Err(e) => {
                        result = Err(anyhow::anyhow!("Task error: {e}"));
                        break;
                    }
                }
            }
        }

        let release_result = tokio::task::spawn_blocking(move || {
            crate::input::send_button_up(xid, to_x.round() as i32, to_y.round() as i32, button)
        })
        .await;
        if result.is_ok() {
            result = match release_result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("Task error: {e}")),
            };
        }
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::SetPressed(false),
        );
        let guard = match focus_snapshot {
            Some(snapshot) => {
                tokio::task::spawn_blocking(move || snapshot.restore_if_changed(Some(pid)))
                    .await
                    .ok()
            }
            None => None,
        };

        if result.is_ok() {
            if let Ok(Ok((sx_to, sy_to))) =
                tokio::task::spawn_blocking(move || window_local_to_screen(xid, to_x, to_y)).await
            {
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::track_pointer_command(sx_to, sy_to),
                );
                self.state
                    .cursor_registry
                    .update_position(&cursor_id, sx_to, sy_to);
            }
        }

        let route = PointerRoute::Synthetic(guard);
        match result {
            Ok(()) => ToolResult::text(format!(
                "Posted drag ({button_str}) to pid {pid} \
                 from ({from_x:.0}, {from_y:.0}) → ({to_x:.0}, {to_y:.0}) \
                 in {duration_ms}ms / {steps} steps {}",
                route.text_suffix("background")
            ))
            .with_structured(route.structured("background")),
            Err(e) => input_error_result(e),
        }
    }
}

// ── mouse_button_down / mouse_drag / mouse_button_up ────────────────────────

/// `coordinate_frame` of mouse_button_down / mouse_button_up. The shared
/// [`coordinate_frame_schema`] also names `scope`, which these tools lack.
fn held_coordinate_frame_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["window", "desktop"],
        "description": "Frame of x/y: \"window\" (default) get_window_state screenshot pixels; \
            \"desktop\" get_desktop_state pixels."
    })
}

/// `delivery_mode` of mouse_drag / mouse_button_up: they follow the mode the
/// button was pressed with, so the field can only confirm it.
fn held_delivery_mode_schema() -> Value {
    pincer_core::tool_schema::delivery_mode_schema_with(
        "Optional: the held button keeps the mode mouse_button_down used, and a different \
         value is refused.",
    )
}

pub struct MouseButtonDownTool {
    state: Arc<ToolState>,
}
static MDOWN_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for MouseButtonDownTool {
    fn def(&self) -> &ToolDef {
        MDOWN_DEF.get_or_init(|| ToolDef {
            name: "mouse_button_down".into(),
            description: "Press and hold a mouse button at window pixel (x, y); move it with \
                mouse_drag and release it with mouse_button_up. For one complete stroke, drag \
                is simpler. x,y need a get_window_state screenshot of the window earlier in \
                the session (screenshot_context_missing otherwise). On X11, background delivery \
                sends synthetic events, which GTK, Qt, LibreOffice, Chromium and WebKitGTK \
                windows ignore, so for those it is refused with background_unavailable; pass \
                delivery_mode foreground, which activates the window and holds a real button \
                on the system pointer until mouse_button_up. In background, a point covered by \
                another app's window is refused (target_occluded); foreground raises the window \
                first, so there only a window kept above it refuses. A point outside the window \
                is refused (point_outside_window). Returns a line naming the held button and \
                point."
                .into(),
            input_schema: json!({"type":"object","required":["pid","window_id","x","y"],"properties":{
                "session": pincer_core::tool_schema::session_schema_with(
                    "Takes precedence over cursor_id."
                ),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Target process ID."},
                "window_id":{"type":"integer","description":"Window id from list_windows."},
                "x":{"type":"number","description":"Pixel X in the window's latest get_window_state screenshot (0..screenshot_width), or in get_desktop_state pixels with coordinate_frame \"desktop\"."},
                "y":{"type":"number","description":"Pixel Y, in the same frame as x."},
                "coordinate_frame": held_coordinate_frame_schema(),
                "button": pincer_core::tool_schema::button_schema(),
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "delivery_mode": crate::input::delivery::delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        if let Some(held) = self
            .state
            .mouse_hold
            .lock()
            .unwrap()
            .get(&cursor_id)
            .cloned()
        {
            return ToolResult::error(format!(
                "Cursor '{cursor_id}' already has a held mouse button. Call mouse_button_up first."
            ))
            .with_structured(mouse_hold_json(&cursor_id, Some(&held)));
        }

        let pid = match args.require_u32("pid") {
            Ok(v) => v,
            Err(e) => return e,
        };
        let xid = match args.opt_u64("window_id") {
            Some(v) => v,
            None => return ToolResult::error("window_id is required on Linux."),
        };
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        // Like drag: the background refusal comes before the screenshot
        // frame is consulted.
        if let Some(refusal) = unavailable_held_button_background(pid, delivery) {
            return refusal;
        }
        let button_name = args.str_or("button", "left");
        let button = parse_mouse_button(button_name.as_str());
        let mut x = args.f64_or("x", 0.0);
        let mut y = args.f64_or("y", 0.0);
        let desktop_frame = desktop_frame_requested(&args);
        if desktop_frame {
            // Desktop pixels: no zoom/screenshot scaling; translated once below.
        } else if args.bool_or("from_zoom", false) {
            match self.state.zoom_context(&args, pid, Some(xid)) {
                Ok(ctx) => {
                    let (wx, wy) = ctx.zoom_to_window(x, y);
                    x = wx;
                    y = wy;
                }
                Err(refusal) => return refusal,
            }
        } else {
            let ratio = match screenshot_scale(&self.state, &args, pid, Some(xid)) {
                Ok(ratio) => ratio,
                Err(refusal) => return refusal,
            };
            x *= ratio;
            y *= ratio;
        }
        if desktop_frame {
            match tokio::task::spawn_blocking(move || desktop_to_window_local(xid, x, y)).await {
                Ok(Ok((lx, ly))) => {
                    x = lx;
                    y = ly;
                }
                Ok(Err(e)) => {
                    return ToolResult::error(format!(
                        "desktop-frame coordinates could not be mapped into window {xid}: {e}"
                    ))
                }
                Err(e) => return ToolResult::error(format!("Task error: {e}")),
            }
        }

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        if let Ok(Ok((sx, sy))) =
            tokio::task::spawn_blocking(move || window_local_to_screen(xid, x, y)).await
        {
            overlay_glide_to_for(&cursor_id, sx, sy).await;
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::ClickPulse { x: sx, y: sy },
            );
        }

        let xi = x as i32;
        let yi = y as i32;
        let is_wl = crate::wayland::is_wayland();
        // Native Wayland keeps the persistent virtual pointer in both modes,
        // as drag outside Hyprland does: it is real input already, and this
        // route has no activation step to add.
        let foreground = delivery.is_foreground() && !is_wl;
        let result = if is_wl {
            let cid = cursor_id.clone();
            let (output_x, output_y) = persistent_pointer_coordinates(
                is_wl,
                xid,
                xi,
                yi,
                crate::wayland::window_local_to_output,
            );
            tokio::task::spawn_blocking(move || {
                crate::wayland::persistent_vptr::press(&cid, xid, output_x, output_y, button)
            })
            .await
        } else if foreground {
            spawn_blocking_bounded(
                "foreground mouse_button_down",
                foreground_budget(0),
                move || {
                    crate::input::with_x11_foreground_opts(
                        xid,
                        crate::input::ForegroundOptions::from_settle_hint(80)
                            .for_pid(pid)
                            .without_observation(),
                        || {
                            // Checked after the activation raised the window,
                            // so only a window that stays above it (keep
                            // above, another app's popup) refuses the press.
                            let (sx, sy) = window_local_to_screen(xid, x, y)?;
                            let point = (sx.round() as i32, sy.round() as i32);
                            crate::input::refuse_covered_point(xid, point.0, point.1)?;
                            crate::input::send_button_xtest_desktop(Some(point), button, true)
                        },
                    )
                    .map(|((), _report)| ())
                },
            )
            .await
        } else {
            tokio::task::spawn_blocking(move || {
                // The synthetic press is window-addressed and would reach a
                // covered window too; refuse it like the MPX routes do, so a
                // covered point never looks like a press the user can see.
                let (sx, sy) = window_local_to_screen(xid, x, y)?;
                crate::input::refuse_covered_point(xid, sx.round() as i32, sy.round() as i32)?;
                crate::input::send_button_down(xid, xi, yi, button)
            })
            .await
        };
        match result {
            Ok(Ok(())) => {
                let hold = MouseHoldState {
                    pid,
                    xid,
                    button,
                    x,
                    y,
                    foreground,
                };
                self.state
                    .mouse_hold
                    .lock()
                    .unwrap()
                    .insert(cursor_id.clone(), hold.clone());
                if let Ok(Ok((sx, sy))) =
                    tokio::task::spawn_blocking(move || window_local_to_screen(xid, x, y)).await
                {
                    self.state
                        .cursor_registry
                        .update_position(&cursor_id, sx, sy);
                    overlay_snap_to_for(&cursor_id, sx, sy, None);
                    crate::overlay::send_command_for(
                        cursor_id.clone(),
                        cursor_overlay::OverlayCommand::SetPressed(true),
                    );
                }
                with_held_route(
                    ToolResult::text(format!(
                        "✅ Cursor '{cursor_id}' held {} button down at ({x:.1}, {y:.1}){}.",
                        mouse_button_name(button),
                        held_route_note(foreground),
                    ))
                    .with_structured(mouse_hold_json(&cursor_id, Some(&hold))),
                    foreground,
                )
            }
            Ok(Err(e)) => held_input_error(
                e,
                &cursor_id,
                self.state.mouse_hold.lock().unwrap().get(&cursor_id),
                foreground,
            ),
            Err(e) => {
                ToolResult::error(format!("Task error: {e}")).with_structured(mouse_hold_json(
                    &cursor_id,
                    self.state.mouse_hold.lock().unwrap().get(&cursor_id),
                ))
            }
        }
    }
}

pub struct MouseDragTool {
    state: Arc<ToolState>,
}
static MDRAG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for MouseDragTool {
    fn def(&self) -> &ToolDef {
        MDRAG_DEF.get_or_init(|| ToolDef {
            name: "mouse_drag".into(),
            description: "Move the button held by mouse_button_down to window pixel (x, y), \
                sending motion events along the way; does not release it. Acts on the held \
                button's window in the delivery mode it was pressed with (a foreground hold \
                moves the system pointer). Returns the held-button state."
                .into(),
            input_schema: json!({"type":"object","required":["x","y"],"properties":{
                "session": pincer_core::tool_schema::session_schema_with(
                    "Takes precedence over cursor_id."
                ),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Optional process ID; must match the held button's target."},
                "window_id":{"type":"integer","description":"Optional window id; must match the held button's target."},
                "x":{"type":"number","description":"Pixel X in the held window's latest get_window_state screenshot (0..screenshot_width), whatever coordinate_frame mouse_button_down used."},
                "y":{"type":"number","description":"Pixel Y, in the same frame as x."},
                "duration_ms":{"type":"integer","minimum":0,"maximum":10000,"description":"Total drag duration in ms. Default: 500."},
                "steps":{"type":"integer","minimum":1,"maximum":200,"description":"Intermediate motion events. Default: 20."},
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "delivery_mode": held_delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        let Some(mut hold) = self
            .state
            .mouse_hold
            .lock()
            .unwrap()
            .get(&cursor_id)
            .cloned()
        else {
            return ToolResult::error(format!(
                "No mouse button is currently held for cursor '{cursor_id}'. Call mouse_button_down first."
            ))
            .with_structured(mouse_hold_json(&cursor_id, None));
        };
        if let Some(err) = held_target_mismatch(&args, &cursor_id, &hold) {
            return err;
        }
        if let Some(err) = held_delivery_mismatch(&args, &cursor_id, &hold) {
            return err;
        }

        if let Some(refusal) = require_point_args("mouse_drag", &args) {
            return refusal;
        }
        let mut to_x = args.f64_or("x", 0.0);
        let mut to_y = args.f64_or("y", 0.0);
        if args.bool_or("from_zoom", false) {
            match self.state.zoom_context(&args, hold.pid, Some(hold.xid)) {
                Ok(ctx) => {
                    let (wx, wy) = ctx.zoom_to_window(to_x, to_y);
                    to_x = wx;
                    to_y = wy;
                }
                Err(refusal) => return refusal,
            }
        } else {
            let ratio = match screenshot_scale(&self.state, &args, hold.pid, Some(hold.xid)) {
                Ok(ratio) => ratio,
                Err(refusal) => return refusal,
            };
            to_x *= ratio;
            to_y *= ratio;
        }

        let xid = hold.xid;

        let from_x = hold.x;
        let from_y = hold.y;
        let duration_ms = args.u64_or("duration_ms", 500);
        let steps = args.u64_or("steps", 20).max(1) as usize;
        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        if let Ok(Ok((sx, sy))) =
            tokio::task::spawn_blocking(move || window_local_to_screen(xid, from_x, from_y)).await
        {
            overlay_glide_to_for(&cursor_id, sx, sy).await;
            self.state
                .cursor_registry
                .update_position(&cursor_id, sx, sy);
            overlay_snap_to_for(&cursor_id, sx, sy, None);
            crate::overlay::send_command_for(
                cursor_id.clone(),
                cursor_overlay::OverlayCommand::SetPressed(true),
            );
        }

        let button = hold.button;
        let foreground = hold.foreground;
        let step_delay_ms = if steps > 1 {
            duration_ms / steps as u64
        } else {
            duration_ms
        };
        let mut result: anyhow::Result<()> = Ok(());
        let mut prev_x = from_x;
        let mut prev_y = from_y;
        let is_wl = crate::wayland::is_wayland();
        // A foreground hold moves the real pointer in screen pixels: resolve
        // the window origin once for the whole motion.
        let origin = if foreground {
            match tokio::task::spawn_blocking(move || window_local_to_screen(xid, 0.0, 0.0)).await {
                Ok(Ok(origin)) => Some(origin),
                Ok(Err(e)) => return held_input_error(e, &cursor_id, Some(&hold), foreground),
                Err(e) => {
                    return ToolResult::error(format!("Task error: {e}"))
                        .with_structured(mouse_hold_json(&cursor_id, Some(&hold)))
                }
            }
        } else {
            None
        };
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let ix = from_x + (to_x - from_x) * t;
            let iy = from_y + (to_y - from_y) * t;
            // Native Wayland: route motion through the persistent virtual-
            // pointer so the held button stays held across the entire drag;
            // X11 moves the real pointer (foreground) or posts synthetic
            // motion (background).
            let cid_inner = cursor_id.clone();
            let move_result = if is_wl {
                let (output_x, output_y) = persistent_pointer_coordinates(
                    is_wl,
                    xid,
                    ix.round() as i32,
                    iy.round() as i32,
                    crate::wayland::window_local_to_output,
                );
                tokio::task::spawn_blocking(move || {
                    crate::wayland::persistent_vptr::move_to(&cid_inner, output_x, output_y)
                })
                .await
            } else if let Some((ox, oy)) = origin {
                tokio::task::spawn_blocking(move || {
                    crate::input::send_move_xtest_desktop(
                        (ox + ix).round() as i32,
                        (oy + iy).round() as i32,
                    )
                })
                .await
            } else {
                tokio::task::spawn_blocking(move || {
                    crate::input::send_motion(
                        xid,
                        ix.round() as i32,
                        iy.round() as i32,
                        Some(button),
                    )
                })
                .await
            };
            match move_result {
                Ok(Ok(())) => {
                    if let Ok(Ok((sx, sy))) =
                        tokio::task::spawn_blocking(move || window_local_to_screen(xid, ix, iy))
                            .await
                    {
                        let heading = if (ix - prev_x).abs() > f64::EPSILON
                            || (iy - prev_y).abs() > f64::EPSILON
                        {
                            Some((iy - prev_y).atan2(ix - prev_x))
                        } else {
                            None
                        };
                        self.state
                            .cursor_registry
                            .update_position(&cursor_id, sx, sy);
                        overlay_move_to_for(&cursor_id, sx, sy, heading);
                    }
                    prev_x = ix;
                    prev_y = iy;
                    if step_delay_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(step_delay_ms)).await;
                    }
                }
                Ok(Err(e)) => {
                    result = Err(e);
                    break;
                }
                Err(e) => {
                    result = Err(anyhow::anyhow!("Task error: {e}"));
                    break;
                }
            }
        }

        match result {
            Ok(()) => {
                hold.x = to_x;
                hold.y = to_y;
                self.state
                    .mouse_hold
                    .lock()
                    .unwrap()
                    .insert(cursor_id.clone(), hold.clone());
                if let Ok(Ok((sx, sy))) =
                    tokio::task::spawn_blocking(move || window_local_to_screen(xid, to_x, to_y))
                        .await
                {
                    self.state
                        .cursor_registry
                        .update_position(&cursor_id, sx, sy);
                    overlay_snap_to_for(
                        &cursor_id,
                        sx,
                        sy,
                        Some((to_y - from_y).atan2(to_x - from_x)),
                    );
                    crate::overlay::send_command_for(
                        cursor_id.clone(),
                        cursor_overlay::OverlayCommand::ClickPulse { x: sx, y: sy },
                    );
                }
                with_held_route(
                    ToolResult::text(format!(
                        "✅ Cursor '{cursor_id}' dragged held {} button to ({to_x:.1}, {to_y:.1}){}.",
                        mouse_button_name(hold.button),
                        held_route_note(foreground),
                    ))
                    .with_structured(mouse_hold_json(&cursor_id, Some(&hold))),
                    foreground,
                )
            }
            Err(e) => held_input_error(e, &cursor_id, Some(&hold), foreground),
        }
    }
}

pub struct MouseButtonUpTool {
    state: Arc<ToolState>,
}
static MUP_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for MouseButtonUpTool {
    fn def(&self) -> &ToolDef {
        MUP_DEF.get_or_init(|| ToolDef {
            name: "mouse_button_up".into(),
            description: "Release the button held by mouse_button_down, at window pixel (x, y) \
                or, without x/y, where it is held. Acts on the held button's window in the \
                delivery mode it was pressed with. Returns the cleared held-button state."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "session": pincer_core::tool_schema::session_schema_with(
                    "Takes precedence over cursor_id."
                ),
                "cursor_id":{"type":"string","description":"Optional multi-cursor instance id. Default: 'default'."},
                "pid":{"type":"integer","description":"Optional process ID; must match the held button's target."},
                "window_id":{"type":"integer","description":"Optional window id; must match the held button's target."},
                "x":{"type":"number","description":"Optional release X in the held window's latest get_window_state screenshot (0..screenshot_width), or in get_desktop_state pixels with coordinate_frame \"desktop\". Default: where the button is held."},
                "y":{"type":"number","description":"Optional release Y, in the same frame as x."},
                "coordinate_frame": held_coordinate_frame_schema(),
                "from_zoom":{"type":"boolean","description":"True when the coordinates come from a zoom image; maps them back to window pixels."},
                "delivery_mode": held_delivery_mode_schema()
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let cursor_id = resolve_cursor_key(&args);
        let Some(hold) = self
            .state
            .mouse_hold
            .lock()
            .unwrap()
            .get(&cursor_id)
            .cloned()
        else {
            return ToolResult::error(format!(
                "No mouse button is currently held for cursor '{cursor_id}'."
            ))
            .with_structured(mouse_hold_json(&cursor_id, None));
        };
        if let Some(err) = held_target_mismatch(&args, &cursor_id, &hold) {
            return err;
        }
        if let Some(err) = held_delivery_mismatch(&args, &cursor_id, &hold) {
            return err;
        }

        let xid = hold.xid;

        let desktop_frame = args.opt_f64("x").is_some()
            && args.opt_f64("y").is_some()
            && desktop_frame_requested(&args);
        let (mut x, mut y) = if desktop_frame {
            // Desktop pixels: no zoom/screenshot scaling; translated below.
            (args.f64_or("x", hold.x), args.f64_or("y", hold.y))
        } else {
            match mouse_button_up_coordinates(&self.state, &args, &hold) {
                Ok(point) => point,
                Err(refusal) => return refusal,
            }
        };
        if desktop_frame {
            match tokio::task::spawn_blocking(move || desktop_to_window_local(xid, x, y)).await {
                Ok(Ok((lx, ly))) => {
                    x = lx;
                    y = ly;
                }
                Ok(Err(e)) => {
                    return ToolResult::error(format!(
                        "desktop-frame coordinates could not be mapped into window {xid}: {e}"
                    ))
                    .with_structured(mouse_hold_json(&cursor_id, Some(&hold)))
                }
                Err(e) => {
                    return ToolResult::error(format!("Task error: {e}"))
                        .with_structured(mouse_hold_json(&cursor_id, Some(&hold)))
                }
            }
        }

        crate::overlay::send_command_for(
            cursor_id.clone(),
            cursor_overlay::OverlayCommand::PinAbove(xid),
        );
        if let Ok(Ok((sx, sy))) =
            tokio::task::spawn_blocking(move || window_local_to_screen(xid, x, y)).await
        {
            overlay_glide_to_for(&cursor_id, sx, sy).await;
        }

        let button = hold.button;
        let foreground = hold.foreground;
        let xi = x as i32;
        let yi = y as i32;
        // Native Wayland: release through the persistent virtual-pointer so
        // the same vptr device that emitted the press also emits the release
        // (single logical drag rather than a click pair).
        let result = if crate::wayland::is_wayland() {
            let cid = cursor_id.clone();
            tokio::task::spawn_blocking(move || {
                crate::wayland::persistent_vptr::release(&cid, button)
            })
            .await
        } else if foreground {
            tokio::task::spawn_blocking(move || {
                let (sx, sy) = window_local_to_screen(xid, x, y)?;
                crate::input::send_button_xtest_desktop(
                    Some((sx.round() as i32, sy.round() as i32)),
                    button,
                    false,
                )
            })
            .await
        } else {
            tokio::task::spawn_blocking(move || crate::input::send_button_up(xid, xi, yi, button))
                .await
        };
        match result {
            Ok(Ok(())) => {
                if let Ok(Ok((sx, sy))) =
                    tokio::task::spawn_blocking(move || window_local_to_screen(xid, x, y)).await
                {
                    self.state
                        .cursor_registry
                        .update_position(&cursor_id, sx, sy);
                    overlay_snap_to_for(&cursor_id, sx, sy, None);
                }
                crate::overlay::send_command_for(
                    cursor_id.clone(),
                    cursor_overlay::OverlayCommand::SetPressed(false),
                );
                clear_mouse_hold_after_release(&self.state, &cursor_id);
                let mut cleared = mouse_hold_json(&cursor_id, None);
                cleared["delivery_mode"] = json!(held_delivery_mode(foreground));
                with_held_route(
                    ToolResult::text(format!(
                        "✅ Cursor '{cursor_id}' released held {} button at ({x:.1}, {y:.1}){}.",
                        mouse_button_name(button),
                        held_route_note(foreground),
                    ))
                    .with_structured(cleared),
                    foreground,
                )
            }
            Ok(Err(e)) => held_input_error(e, &cursor_id, Some(&hold), foreground),
            Err(e) => ToolResult::error(format!("Task error: {e}"))
                .with_structured(mouse_hold_json(&cursor_id, Some(&hold))),
        }
    }
}

pub struct ParallelMouseDragTool {
    state: Arc<ToolState>,
}
static PMDRAG_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// cua-compositor path for parallel_mouse_drag: build window-local drag paths
/// and run them as concurrent multi-cursor injections over the control socket.
/// Coordinates stay window-local (the compositor maps them per app_id), so no
/// X11 geometry/MPX is needed — the X11 path's hard blocker on Wayland.
async fn parallel_drag_inject(args: &Value) -> ToolResult {
    let Some(items) = args.get("drags").and_then(|v| v.as_array()) else {
        return ToolResult::error("drags[] is required.");
    };
    if items.len() < 2 {
        return ToolResult::error("parallel_mouse_drag requires at least two drag items.");
    }
    let mut drags = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let Some(xid) = item.get("window_id").and_then(|v| v.as_u64()) else {
            return ToolResult::error("each drag item requires window_id.");
        };
        let local: Vec<(f64, f64)> = if let Some(pts) = item.get("path").and_then(|v| v.as_array())
        {
            pts.iter()
                .filter_map(|p| {
                    let a = p.as_array()?;
                    Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
                })
                .collect()
        } else {
            let g = |k: &str| item.get(k).and_then(|v| v.as_f64());
            match (g("from_x"), g("from_y"), g("to_x"), g("to_y")) {
                (Some(fx), Some(fy), Some(tx), Some(ty)) => vec![(fx, fy), (tx, ty)],
                _ => {
                    return ToolResult::error(
                        "each drag item requires path[] or from_x/from_y/to_x/to_y.",
                    )
                }
            }
        };
        if local.len() < 2 {
            return ToolResult::error("drag path needs at least 2 points.");
        }
        let steps = item
            .get("steps")
            .and_then(|v| v.as_u64())
            .unwrap_or(60)
            .clamp(1, 300) as usize;
        let x_button = parse_mouse_button(
            item.get("button")
                .and_then(|v| v.as_str())
                .unwrap_or("left"),
        ) as u32;
        let app = match tokio::task::spawn_blocking(move || {
            crate::wayland::inject_target_for_window(xid)
        })
        .await
        {
            Ok(Ok(target)) => target,
            Ok(Err(error)) => return ToolResult::error(error.to_string()),
            Err(error) => return ToolResult::error(format!("Task error: {error}")),
        };
        drags.push(crate::wayland::InjectDrag {
            app_id: app,
            idx: i,
            x_button,
            path: local,
            steps,
        });
    }
    let n = drags.len();
    match tokio::task::spawn_blocking(move || crate::wayland::inject_parallel_drags(&drags)).await {
        Ok(Ok(())) => ToolResult::text(format!(
            "Ran {n} concurrent drags (multi-cursor via cua-compositor)."
        )),
        Ok(Err(e)) => ToolResult::error(e.to_string()),
        Err(e) => ToolResult::error(format!("Task error: {e}")),
    }
}

/// parallel_mouse_drag without its uinput device. Unlike the other pointer
/// tools it has no foreground route, so the advice is to draw the strokes
/// one at a time with drag.
fn parallel_uinput_refusal(reason: &str) -> ToolResult {
    let suggestion = "Draw the strokes one at a time with drag (with \
         delivery_mode:\"foreground\" if drag refuses background delivery); \
         parallel_mouse_drag has no foreground fallback.";
    ToolResult::error(format!(
        "parallel_mouse_drag needs a Linux uinput device for its virtual pointers, which is \
         unavailable here: {reason}. No input was sent. {suggestion}"
    ))
    .with_structured(json!({
        "code": crate::input::UINPUT_UNAVAILABLE_CODE,
        "effect": "refused",
        "reason": reason,
        "suggestion": suggestion,
    }))
}

#[async_trait]
impl Tool for ParallelMouseDragTool {
    fn def(&self) -> &ToolDef {
        PMDRAG_DEF.get_or_init(|| ToolDef {
            name: "parallel_mouse_drag".into(),
            description: "Run two or more mouse drags at the same time, each on its own virtual \
                pointer (an X11 MPX master pointer backed by a Linux uinput device): each item \
                presses once, moves along its whole path and releases once. Needs that uinput \
                device, which is unavailable on KDE Plasma X11, XWayland, Xvfb and wherever \
                /dev/uinput cannot be opened; the call is then refused (uinput_unavailable) and \
                there is no foreground fallback: draw the strokes one at a time with drag. Each \
                item names its window by window_id alone (no pid) and needs its own session, \
                different from the other items'. Coordinates are window pixels: get_window_state \
                screenshot pixels divided by frame_scale (the same when frame_scale is 1); no \
                screenshot is required. A path is a straight segment (from_x/from_y to \
                to_x/to_y), waypoints (path), or a function fn = y(x) sampled over [x_from, x_to] \
                (fn:\"300+120*sin(x/40)\" is a sine wave; + - * / ^, sin, cos, tan, sqrt, abs, \
                exp, ln, pi, e). A start point covered by another app's window is refused \
                (target_occluded).".into(),
            input_schema: json!({"type":"object","required":["drags"],"properties":{
                "drags":{"type":"array","minItems":2,"description":"Two or more drags to run at the same time, each on its own virtual pointer.","items":{"type":"object","required":["session","window_id"],"properties":{
                    "session":{"type":"string","description":"Cursor id for this item, different from every other item's: it keys the item's virtual pointer and agent cursor."},
                    "window_id":{"type":"integer","description":"Window id from list_windows; the window is found by this id alone."},
                    "path":{"type":"array","items":{"type":"array","items":{"type":"number"}},"description":"Waypoints [[x,y],...] (at least 2) in window pixels; pressed once, moved through, released once. Takes precedence over fn and from/to."},
                    "fn":{"type":"string","description":"Expression y(x) in window pixels, sampled over [x_from, x_to]. Instead of from_x/to_x."},
                    "x_from":{"type":"number","description":"Domain start (window x) when `fn` is used."},
                    "x_to":{"type":"number","description":"Domain end (window x) when `fn` is used."},
                    "samples":{"type":"integer","minimum":2,"maximum":400,"description":"Waypoints sampled along `fn`. Default: 80."},
                    "from_x":{"type":"number","description":"Straight-segment start X (window pixels: screenshot pixels / frame_scale)."},
                    "from_y":{"type":"number","description":"Straight-segment start Y (window pixels)."},
                    "to_x":{"type":"number","description":"Straight-segment end X (window pixels)."},
                    "to_y":{"type":"number","description":"Straight-segment end Y (window pixels)."},
                    "button": pincer_core::tool_schema::button_schema(),
                    "duration_ms":{"type":"integer","minimum":0,"maximum":10000,"description":"Default: 1500 for fn paths, 500 for straight."},
                    "steps":{"type":"integer","minimum":1,"maximum":300,"description":"Motion sub-steps along the whole path. Default: scaled to path length."}
                },"additionalProperties":false}}
            },"additionalProperties":false}),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        // Nested cua-compositor: run the drags as concurrent multi-cursor
        // injections (window-local, no X11 MPX/geometry needed).
        if crate::wayland::is_inject_mode() {
            return parallel_drag_inject(&args).await;
        }
        // Native Wayland without the inject socket: MPX/XI2 + uinput master
        // pointers don't exist on Wayland. Surface a typed error instead of
        // silently calling the X11 path that's guaranteed to fail.
        if crate::wayland::is_wayland() {
            return ToolResult::error(
                "parallel_mouse_drag requires the cua-compositor inject socket on Wayland \
                 (set CUA_INJECT_SOCKET to the cua-compositor control socket), \
                 or run the target under X11.",
            );
        }
        // Refuse before any device is created: the MPX route has no
        // foreground fallback, so the advice names drag instead of the
        // delivery_mode the shared uinput refusal suggests.
        match tokio::task::spawn_blocking(crate::input::real_pointer_unavailable_reason).await {
            Ok(None) => {}
            Ok(Some(reason)) => return parallel_uinput_refusal(&reason),
            Err(e) => return ToolResult::error(format!("Task error: {e}")),
        }

        let Some(items) = args.get("drags").and_then(|v| v.as_array()) else {
            return ToolResult::error("drags[] is required.");
        };
        if items.len() < 2 {
            return ToolResult::error("parallel_mouse_drag requires at least two drag items.");
        }

        let mut drags = Vec::with_capacity(items.len());
        let mut sessions = std::collections::HashSet::new();
        for item in items {
            let Some(session) = item.get("session").and_then(|v| v.as_str()) else {
                return ToolResult::error("each drag item requires session.");
            };
            // The session keys the item's virtual pointer: two items on one
            // pointer would press and move the same device.
            if !sessions.insert(session) {
                return ToolResult::error(format!(
                    "parallel_mouse_drag: two drag items share session \"{session}\"; give each \
                     item its own session, which keys its virtual pointer. No input was sent."
                ))
                .with_structured(json!({
                    "code": "invalid_arguments",
                    "effect": "refused",
                    "session": session,
                }));
            }
            let Some(xid) = item.get("window_id").and_then(|v| v.as_u64()) else {
                return ToolResult::error("each drag item requires window_id.");
            };

            // Build the window-local waypoint path from one of: an explicit
            // `path` of [x,y] points, a function `fn` (y = f(x) sampled over
            // [x_from, x_to]), or a straight from→to segment.
            let is_fn = item.get("fn").and_then(|v| v.as_str()).is_some();
            let local: Vec<(f64, f64)> = if let Some(pts) =
                item.get("path").and_then(|v| v.as_array())
            {
                let mut out = Vec::with_capacity(pts.len());
                for p in pts {
                    let a = p.as_array();
                    let (Some(px), Some(py)) = (
                        a.and_then(|a| a.first()).and_then(|v| v.as_f64()),
                        a.and_then(|a| a.get(1)).and_then(|v| v.as_f64()),
                    ) else {
                        return ToolResult::error("each `path` entry must be [x, y].");
                    };
                    out.push((px, py));
                }
                if out.len() < 2 {
                    return ToolResult::error("`path` needs at least 2 points.");
                }
                out
            } else if let Some(expr_str) = item.get("fn").and_then(|v| v.as_str()) {
                let Some(x_from) = item.get("x_from").and_then(|v| v.as_f64()) else {
                    return ToolResult::error("`fn` requires x_from.");
                };
                let Some(x_to) = item.get("x_to").and_then(|v| v.as_f64()) else {
                    return ToolResult::error("`fn` requires x_to.");
                };
                let samples = item
                    .get("samples")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(80)
                    .clamp(2, 400);
                match crate::input::sample_function(expr_str, x_from, x_to, samples) {
                    Ok(pts) => pts,
                    Err(e) => return ToolResult::error(e.to_string()),
                }
            } else {
                let coerce = |k: &str| item.get(k).and_then(|v| v.as_f64());
                match (coerce("from_x"), coerce("from_y"), coerce("to_x"), coerce("to_y")) {
                    (Some(fx), Some(fy), Some(tx), Some(ty)) => vec![(fx, fy), (tx, ty)],
                    _ => return ToolResult::error("each drag item requires either `fn`+x_from+x_to, or from_x/from_y/to_x/to_y."),
                }
            };

            let button = parse_mouse_button(
                item.get("button")
                    .and_then(|v| v.as_str())
                    .unwrap_or("left"),
            );
            let duration_ms = item
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(if is_fn { 1500 } else { 500 });

            // One translate gives the window origin; the path is a pure offset.
            let origin =
                match tokio::task::spawn_blocking(move || window_local_to_screen(xid, 0.0, 0.0))
                    .await
                {
                    Ok(Ok(o)) => o,
                    Ok(Err(e)) => return ToolResult::error(e.to_string()),
                    Err(e) => return ToolResult::error(format!("Task error: {e}")),
                };
            let path: Vec<(i32, i32)> = local
                .iter()
                .map(|(lx, ly)| {
                    (
                        (origin.0 + lx).round() as i32,
                        (origin.1 + ly).round() as i32,
                    )
                })
                .collect();

            // Default sub-step count scaled to path length (smooth glide),
            // overridable via `steps`.
            let total_len: f64 = path
                .windows(2)
                .map(|w| {
                    (((w[1].0 - w[0].0) as f64).powi(2) + ((w[1].1 - w[0].1) as f64).powi(2)).sqrt()
                })
                .sum();
            let steps = item
                .get("steps")
                .and_then(|v| v.as_u64())
                .map(|s| (s as usize).clamp(1, 300))
                .unwrap_or_else(|| ((total_len / 3.0).round() as usize).clamp(24, 300));

            let start = path[0];
            self.state
                .cursor_registry
                .update_position(session, start.0 as f64, start.1 as f64);
            crate::overlay::send_command_for(
                session.to_owned(),
                cursor_overlay::OverlayCommand::PinAbove(xid),
            );

            drags.push((
                session.to_owned(),
                crate::input::VirtualPointerDrag {
                    target_window: xid,
                    button,
                    path,
                    duration_ms,
                    steps,
                },
            ));
        }

        // Glide every cursor to its drag's start together, so none jumps
        // there, then show them pressed for the drags.
        futures_util::future::join_all(drags.iter().map(|(session, drag)| {
            overlay_glide_to_for(session, drag.path[0].0 as f64, drag.path[0].1 as f64)
        }))
        .await;
        for (session, drag) in &drags {
            overlay_snap_to_for(session, drag.path[0].0 as f64, drag.path[0].1 as f64, None);
            crate::overlay::send_command_for(
                session.to_owned(),
                cursor_overlay::OverlayCommand::SetPressed(true),
            );
        }

        let drags_for_task = drags.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::input::send_parallel_virtual_pointer_drags(&drags_for_task)
        })
        .await;
        match result {
            Ok(Ok(())) => {
                for (session, drag) in &drags {
                    let n = drag.path.len();
                    let end = drag.path[n - 1];
                    let prev = drag.path[n.saturating_sub(2)];
                    self.state
                        .cursor_registry
                        .update_position(session, end.0 as f64, end.1 as f64);
                    overlay_snap_to_for(
                        session,
                        end.0 as f64,
                        end.1 as f64,
                        Some(((end.1 - prev.1) as f64).atan2((end.0 - prev.0) as f64)),
                    );
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::SetPressed(false),
                    );
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::ClickPulse {
                            x: end.0 as f64,
                            y: end.1 as f64,
                        },
                    );
                }
                ToolResult::text(format!(
                    "✅ Ran {} MPX drag gesture(s) concurrently.",
                    drags.len()
                ))
                .with_structured(json!({"count": drags.len()}))
            }
            Ok(Err(e)) => {
                for (session, _) in &drags {
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::SetPressed(false),
                    );
                }
                if crate::input::is_uinput_unavailable(&e) {
                    return parallel_uinput_refusal(&format!("{e:#}"));
                }
                linux_input_error(e)
            }
            Err(e) => {
                for (session, _) in &drags {
                    crate::overlay::send_command_for(
                        session.to_owned(),
                        cursor_overlay::OverlayCommand::SetPressed(false),
                    );
                }
                ToolResult::error(format!("Task error: {e}"))
            }
        }
    }
}

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
                    "Optional crop in full-size desktop pixels (the uncapped get_desktop_state frame; physical screen pixels on X11), applied before max_image_dimension. Must lie inside the screen. Screenshot pixel (px,py) is desktop (x + px*frame_scale, y + py*frame_scale); capture_id maps it for you."
                )
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
        let region = match pincer_core::capture_series::parse_region(args.get("region")) {
            Ok(region) => region,
            Err(message) => {
                return ToolResult::error(format!(
                    "get_desktop_state: invalid arguments: {message}"
                ))
                .with_structured(json!({
                    "code": "invalid_arguments",
                    "tool": "get_desktop_state",
                    "detail": message,
                }))
            }
        };
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
                    format!(
                        "; region {},{} {}x{} of the desktop: screenshot pixel (px,py) is desktop \
                         ({} + px*{frame_scale:.2}, {} + py*{frame_scale:.2}); window positions \
                         below are full-size desktop pixels",
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
            "❌ session bus present but org.a11y.Bus has no owner — enable accessibility \
             (gsettings set org.gnome.desktop.interface toolkit-accessibility true) / \
             start at-spi-bus-launcher"
                .to_string()
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
fn tree_window_line(w: &crate::x11::WindowInfo) -> String {
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
fn process_line(p: &crate::proc_fs::ProcessInfo) -> String {
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

pub struct CaptureSeriesTool;
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
                its long edge. Desktop regions are full-size desktop pixels (physical screen \
                pixels on X11), not pixels of a capped get_desktop_state image; window regions \
                are native window pixels (get_window_state pixels divided by its frame_scale). \
                `return_frames`: all, changed (differs from the last kept frame in more than \
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
                    "Crop {x,y,width,height}: full-size desktop pixels, or native window pixels with pid/window_id. Must lie inside the frame."
                ),
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
        let request = match SeriesRequest::parse(&args) {
            Ok(request) => request,
            Err(message) => return capture_series::invalid_arguments(&message),
        };
        let source: Arc<dyn FrameSource> = match (args.get("pid"), args.get("window_id")) {
            (None, None) => Arc::new(DesktopFrameSource),
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
                Arc::new(WindowFrameSource { pid, xid })
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

// ── type_text_chars ───────────────────────────────────────────────────────────

pub struct TypeTextCharsTool;
static TYPE_CHARS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for TypeTextCharsTool {
    fn def(&self) -> &ToolDef {
        TYPE_CHARS_DEF.get_or_init(|| ToolDef {
            name: "type_text_chars".into(),
            description: "Type text character-by-character with a configurable inter-character \
                delay (default 30 ms). Useful for apps that miss keystrokes. \
                Otherwise identical to type_text (XSendEvent, no focus steal).".into(),
            input_schema: json!({
                "type":"object","required":["pid","text"],"properties":{
                    "pid":{"type":"integer","description":"Target process ID."},
                    "window_id":{"type":"integer","description":"Window id from list_windows."},
                    "text":{"type":"string","description":"Text to type, one character at a time."},
                    "delay_ms":{"type":"integer","description":"Milliseconds between chars (default 30)."}
                },"additionalProperties":false
            }),
            read_only: false, destructive: true, idempotent: false, open_world: true,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        #[allow(unused_assignments)]
        let pid = args.u64_or("pid", 0) as u32;
        let text_raw = match args.require_str("text") {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Same trailing-protocol-tag scrub as TypeTextTool — see
        // pincer_core::text_sanitize for rationale.
        let text =
            pincer_core::text_sanitize::strip_trailing_agent_protocol_tags(&text_raw).into_owned();
        let delay_ms = args.u64_or("delay_ms", 30);
        let xid_opt = args.opt_u64("window_id");
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
                    Some(w) => w.xid,
                    None => {
                        return ToolResult::error(format!(
                            "No windows found for pid {pid}. Provide window_id."
                        ))
                    }
                }
            }
        };
        let text_len = text.chars().count();
        let result = tokio::task::spawn_blocking(move || {
            if crate::wayland::wayland_input_enabled() {
                // Per-char `wtype` loop with the requested delay — mirrors the
                // X11 XSendEvent per-char path. Sleeping here is fine because
                // we're inside spawn_blocking.
                let mut buf = [0u8; 4];
                for ch in text.chars() {
                    let s = ch.encode_utf8(&mut buf);
                    crate::wayland::type_text(xid, s)?;
                    if delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                }
                return Ok(());
            }
            crate::input::send_type_text_with_delay(xid, &text, delay_ms)
        })
        .await;
        match result {
            Ok(Ok(())) => ToolResult::text(format!(
                "Typed {text_len} character(s) with {delay_ms}ms delay."
            )),
            Ok(Err(e)) => ToolResult::error(e.to_string()),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── kill_app ──────────────────────────────────────────────────────────────────

/// Plain words for a `kill_app` observation a caller could misread: a
/// "zombie" reads as "not dead", but it has exited and only waits for its
/// parent to collect its exit status (desc-test round 1).
fn kill_observation_note(observation: &str) -> &'static str {
    match observation {
        "zombie" => {
            "; the process has exited and only waits for its parent to reap it, so ps may \
             list it as <defunct> for a moment"
        }
        _ => "",
    }
}

/// What `kill_app` adds after it ended an isolated browser that
/// `browser_prepare` launched.
const KILLED_PREPARED_BROWSER_NOTE: &str = "; it was the isolated browser browser_prepare \
     launched, so Pincer collected its exit and ended that browser (call browser_prepare \
     again for a new one)";

pub struct KillAppTool {
    /// Holds the isolated browsers `browser_prepare` launched, whose exit
    /// only the engine's own child handle may collect.
    browsers: Arc<pincer_core::browser::BrowserEngine>,
}
static KILL_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for KillAppTool {
    fn def(&self) -> &ToolDef {
        KILL_DEF.get_or_init(|| ToolDef {
            name: "kill_app".into(),
            description: "Force-terminate a process by pid with SIGKILL and confirm that it \
                exited. Unsaved work is lost, so close the app the normal way first: its Quit or \
                Close menu item through invoke_menu (e.g. [\"File\",\"Quit\"]), or hotkey \
                [\"alt\",\"F4\"] on its window; use kill_app when that does not end it. In \
                standard mode it terminates only a process Pincer launched (launch_app, \
                browser_prepare), from the session that launched it or, once that session has \
                ended, from any session; any other pid is refused with \
                foreign_process_termination_denied."
                .into(),
            input_schema: json!({"type":"object","required":["pid"],"properties":{
                "pid":{"type":"integer","description":"PID of the process to terminate."}
            },"additionalProperties":false}),
            read_only: false,
            destructive: true,
            idempotent: true,
            open_world: false,
        })
    }

    async fn protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        if adapter_id != "process_control" {
            return Ok(None);
        }
        use pincer_core::browser::platform::BrowserPlatform;
        let pid = args
            .get("pid")
            .and_then(Value::as_i64)
            .filter(|pid| *pid > 0)
            .ok_or_else(|| "kill_app requires a positive integer pid".to_owned())?;
        let fingerprint = crate::browser_platform::LinuxBrowserPlatform::default()
            .process_fingerprint(pid)
            .await
            .map_err(|error| error.message)?;
        Ok(Some(json!({
            "kind": "process_instance",
            "fingerprint": fingerprint,
        })))
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if let Some(expected) = args.get("_protected_process_fingerprint") {
            let current = match self
                .protected_resource_scope("process_control", &args)
                .await
            {
                Ok(Some(scope)) => scope["fingerprint"].clone(),
                Ok(None) => Value::Null,
                Err(message) => return kill_app_stale_process_refusal(message),
            };
            if current != *expected {
                return kill_app_stale_process_refusal(
                    "the process identity changed at the termination boundary".to_owned(),
                );
            }
        }
        let pid_i = match args.get("pid").and_then(|v| v.as_i64()) {
            Some(p) if p > 0 && p <= i32::MAX as i64 => p as i32,
            Some(_) => {
                return ToolResult::error("kill_app: `pid` must be a positive integer".to_string())
            }
            None => {
                return ToolResult::error(
                    "kill_app: missing required integer field `pid`".to_string(),
                )
            }
        };
        // Read the instance before signaling so the confirmation below can
        // tell its exit from a later process that reused the pid.
        let before = match crate::proc_fs::read_process_stat(pid_i as u32) {
            Ok(Some(before)) => before,
            Ok(None) => {
                return kill_app_failure(
                    pid_i,
                    "process_not_found",
                    format!("kill_app: pid {pid_i} does not exist; no signal was sent."),
                )
            }
            Err(error) => {
                return kill_app_failure(
                    pid_i,
                    "process_identity_unavailable",
                    format!(
                        "kill_app: cannot read /proc/{pid_i}/stat to verify termination: \
                         {error}; no signal was sent."
                    ),
                )
            }
        };
        // SAFETY: libc::kill is a thin syscall wrapper, no thread-safety concerns.
        let rc = unsafe { libc::kill(pid_i, libc::SIGKILL) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return kill_app_failure(
                pid_i,
                "signal_failed",
                format!(
                    "kill_app: kill(pid={pid_i}, SIGKILL) failed: {err}. \
                     The process may not exist, or the daemon lacks permission to signal it."
                ),
            );
        }
        // An accepted signal is not an exit: a sandbox (gVisor) can accept
        // SIGKILL for a process that keeps running. Report success only once
        // the same instance is gone.
        let deadline = tokio::time::Instant::now() + KILL_CONFIRM_TIMEOUT;
        loop {
            let now = crate::proc_fs::read_process_stat(pid_i as u32);
            if let Some(mut observation) = now
                .as_ref()
                .ok()
                .and_then(|now| crate::proc_fs::process_exit_observation(before, *now))
            {
                let mut note = kill_observation_note(observation);
                // An isolated browser from browser_prepare is this server's
                // child: it stays a zombie until the browser engine reaps it.
                if observation == "zombie" {
                    let browsers = self.browsers.clone();
                    let released = tokio::task::spawn_blocking(move || {
                        browsers.release_exited_browser(i64::from(pid_i), KILL_CONFIRM_TIMEOUT)
                    })
                    .await
                    .unwrap_or(false);
                    if released {
                        if let Ok(after) = crate::proc_fs::read_process_stat(pid_i as u32) {
                            observation = crate::proc_fs::process_exit_observation(before, after)
                                .unwrap_or(observation);
                        }
                        note = KILLED_PREPARED_BROWSER_NOTE;
                    }
                }
                return ToolResult::text(format!(
                    "✅ Terminated pid {pid_i} with SIGKILL (confirmed: {observation}){note}."
                ))
                .with_structured(json!({
                    "status": "terminated",
                    "effect": "confirmed",
                    "pid": pid_i,
                    "terminated": true,
                    "observation": observation,
                }));
            }
            if tokio::time::Instant::now() >= deadline {
                let detail = match now {
                    Err(error) => format!("/proc/{pid_i}/stat became unreadable: {error}"),
                    Ok(_) => format!(
                        "the same process was still alive {} ms later",
                        KILL_CONFIRM_TIMEOUT.as_millis()
                    ),
                };
                let mut result = kill_app_failure(
                    pid_i,
                    "termination_unconfirmed",
                    format!("kill_app: SIGKILL was accepted for pid {pid_i}, but {detail}."),
                );
                if let Some(structured) = result.structured_content.as_mut() {
                    structured["effect"] = json!("suspected_noop");
                }
                return result;
            }
            tokio::time::sleep(KILL_CONFIRM_INTERVAL).await;
        }
    }
}

/// How long `kill_app` waits for the signaled process to exit, and how often
/// it re-reads `/proc/<pid>/stat` meanwhile.
const KILL_CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const KILL_CONFIRM_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

fn kill_app_failure(pid: i32, code: &str, message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(json!({
        "status": "failed",
        "pid": pid,
        "terminated": false,
        "code": code,
        "message": message,
    }))
}

fn kill_app_stale_process_refusal(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(json!({
        "status": "refused",
        "refusal": {
            "code": "protected_resource_scope_stale",
            "message": message,
        }
    }))
}

// ── invoke_menu (Linux) ──────────────────────────────────────────────────────

pub struct InvokeMenuTool;

static INVOKE_MENU_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn normalized_menu_path(path: Vec<String>) -> Result<Vec<String>, String> {
    if path.is_empty() || path.len() > 16 {
        return Err("invoke_menu: path must contain between 1 and 16 segments".into());
    }
    path.into_iter()
        .enumerate()
        .map(|(index, segment)| {
            let segment = segment.trim();
            if segment.is_empty() {
                Err(format!("invoke_menu: path segment {index} is empty"))
            } else {
                Ok(segment.to_owned())
            }
        })
        .collect()
}

/// The invoke_menu success result: what ran, then what became of the menu
/// Pincer opened to reach the item (closed, or still open with how to close
/// it), and whether the app answered the final action
/// ([`crate::atspi::native::MenuInvoked::unacknowledged`]).
fn invoke_menu_result(
    route: &str,
    cleanup: &crate::atspi::native::MenuCleanup,
    unacknowledged: bool,
    pid: u32,
    window_id: u64,
    delivery_mode: &str,
) -> ToolResult {
    use crate::atspi::native::MenuCleanup;
    let mut text = format!(
        "Resolved the live native menu path and dispatched its final accessibility action \
         ({route})"
    );
    let mut structured = json!({
        "path": "ax",
        "verified": false,
        "effect": "unverifiable",
        "delivery_mode": delivery_mode,
    });
    match cleanup {
        MenuCleanup::NoneOpen => {}
        MenuCleanup::Closed { menu } => {
            text.push_str(&format!(
                ", then closed the {menu} menu it had opened to reach the item"
            ));
            structured["menu_closed"] = json!(menu);
        }
        MenuCleanup::StillOpen { menu } => {
            let popup = crate::input::popup_of_pid(pid);
            text.push_str(&format!(
                ". The {menu} menu Pincer opened to reach the item is still open{}, so pid {pid} \
                 holds a keyboard grab until it closes: press_key escape on window {window_id} \
                 closes it (with delivery_mode foreground if a background press is refused)",
                popup
                    .as_ref()
                    .map(|popup| format!(" ({})", popup.describe()))
                    .unwrap_or_default()
            ));
            structured["menu_left_open"] = json!(menu);
            if let Some(popup) = popup {
                structured["popup"] = popup.to_json();
            }
        }
    }
    if unacknowledged {
        text.push_str(&format!(
            ". The app did not answer the final action within {} s, as when the item opens a \
             modal dialog: check list_windows before calling again",
            crate::atspi::native::CALL_TIMEOUT.as_secs()
        ));
        structured["acknowledged"] = json!(false);
    }
    text.push_str("; verify the command's semantic effect from fresh state.");
    ToolResult::text(text).with_structured(structured)
}

/// The action record's evidence for a menu path that resolved.
fn menu_evidence(unacknowledged: bool) -> &'static str {
    if unacknowledged {
        "Every menu hop resolved uniquely; the final action was sent but not answered in time"
    } else {
        "Every menu hop resolved uniquely and AT-SPI accepted the final action"
    }
}

/// A refused invoke_menu call. The top-level `code` lets the text result
/// print the refusal once with `[code: ...]`, instead of the message a
/// second time inside `refusal` (a long label list would show twice).
fn menu_refusal(message: String) -> ToolResult {
    ToolResult::error(message.clone()).with_structured(json!({
        "status": "refused",
        "code": "menu_path_unavailable",
        "refusal": { "code": "menu_path_unavailable", "message": message }
    }))
}

#[async_trait]
impl Tool for InvokeMenuTool {
    fn def(&self) -> &ToolDef {
        INVOKE_MENU_DEF.get_or_init(|| {
            let contract =
                pincer_contract::tool_contract("invoke_menu").expect("invoke_menu contract");
            ToolDef {
                name: contract.name,
                description: contract.description,
                input_schema: contract.input_schema,
                read_only: contract.annotations.read_only,
                destructive: contract.annotations.destructive,
                idempotent: contract.annotations.idempotent,
                open_world: contract.annotations.open_world,
            }
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::action_record::{
            ActionEffect, ActionEvidence, ActionExecutionRecord, ActionTransport, ActualDelivery,
            EvidenceKind, RequestedDelivery,
        };

        // Menu paths are resolved and fired through AT-SPI, which needs no
        // window activation on X11: the default is background (focus-free,
        // under the focus guard). `delivery_mode:"foreground"` is the explicit
        // escalation that activates the window first, as every other tool.
        let delivery = crate::input::delivery::DeliveryMode::from_args(&args);
        let mut args = args;
        if let Some(object) = args.as_object_mut() {
            // Not part of the closed InvokeMenuInput contract; consumed above.
            object.remove("delivery_mode");
        }
        let input: InvokeMenuInput = match parse_typed_input("invoke_menu", args) {
            Ok(input) => input,
            Err(result) => return result,
        };
        let path = match normalized_menu_path(input.path) {
            Ok(path) => path,
            Err(error) => return menu_refusal(error),
        };
        let pid = input.pid;
        let window_id = input.window_id;
        if !crate::wayland::list_windows_dispatch(Some(pid))
            .iter()
            .any(|window| window.xid == window_id && window.pid == Some(pid))
        {
            return menu_refusal("invoke_menu: window_id does not belong to pid".into());
        }

        if !crate::wayland::is_wayland() && !delivery.is_foreground() {
            let outcome = tokio::task::spawn_blocking(move || {
                crate::input::focus_guard::guarded(Some(pid), || {
                    crate::atspi::native::invoke_menu_path_in(pid, window_id, &path)
                })
            })
            .await;
            return match outcome {
                Ok(Ok((invoked, guard))) => attach_focus_guard_opts(
                    invoke_menu_result(
                        "delivery_mode=background, focus untouched",
                        &invoked.cleanup,
                        invoked.unacknowledged,
                        pid,
                        window_id,
                        "background",
                    )
                    .with_action_record(
                        ActionExecutionRecord::builder(
                            ActionEffect::Unverifiable,
                            ActionTransport::LinuxAtSpiAction,
                            RequestedDelivery::Background,
                        )
                        .actual_delivery(ActualDelivery::Background)
                        .evidence(ActionEvidence {
                            kind: EvidenceKind::NativeApiResult,
                            detail: menu_evidence(invoked.unacknowledged).into(),
                        })
                        .build()
                        .expect("invoke_menu record is valid"),
                    ),
                    guard.as_ref(),
                    // A menu left open is named by the cleanup sentence.
                    !matches!(
                        invoked.cleanup,
                        crate::atspi::native::MenuCleanup::StillOpen { .. }
                    ),
                ),
                Ok(Err(error)) => menu_refusal(format!("invoke_menu: {error}")),
                Err(error) => menu_refusal(format!("invoke_menu: blocking task failed: {error}")),
            };
        }

        let activation = if crate::wayland::is_wayland() {
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::activate_window_for_input_target(window_id, Some(pid))
            })
            .await;
            match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => {
                    return menu_refusal(format!(
                        "invoke_menu: target window could not be activated: {error}"
                    ))
                }
                Err(error) => {
                    return menu_refusal(format!("invoke_menu: activation task failed: {error}"))
                }
            }
        } else {
            match tokio::task::spawn_blocking(move || {
                crate::input::x11_activate_window_persistent(window_id)
            })
            .await
            {
                Ok(Ok(prior)) => Some(prior),
                Ok(Err(error)) => {
                    return menu_refusal(format!(
                        "invoke_menu: target window could not be activated: {error}"
                    ))
                }
                Err(error) => {
                    return menu_refusal(format!("invoke_menu: activation task failed: {error}"))
                }
            }
        };

        let outcome = tokio::task::spawn_blocking(move || {
            let result = crate::atspi::native::invoke_menu_path_in(pid, window_id, &path);
            if let Some(Some(prior_window)) = activation {
                let _ = crate::input::x11_activate_window_persistent(prior_window);
            }
            result
        })
        .await;

        match outcome {
            Ok(Ok(invoked)) => invoke_menu_result(
                "delivery_mode=foreground",
                &invoked.cleanup,
                invoked.unacknowledged,
                pid,
                window_id,
                "foreground",
            )
            .with_action_record(
                ActionExecutionRecord::builder(
                    ActionEffect::Unverifiable,
                    ActionTransport::LinuxAtSpiAction,
                    RequestedDelivery::Foreground,
                )
                .actual_delivery(ActualDelivery::Foreground)
                .evidence(ActionEvidence {
                    kind: EvidenceKind::NativeApiResult,
                    detail: menu_evidence(invoked.unacknowledged).into(),
                })
                .build()
                .expect("invoke_menu record is valid"),
            ),
            Ok(Err(error)) => menu_refusal(format!("invoke_menu: {error}")),
            Err(error) => menu_refusal(format!("invoke_menu: blocking task failed: {error}")),
        }
    }
}

// ── set_window_frame (Linux) ─────────────────────────────────────────────────

pub struct SetWindowFrameTool;

static SET_WINDOW_FRAME_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for SetWindowFrameTool {
    fn def(&self) -> &ToolDef {
        SET_WINDOW_FRAME_DEF.get_or_init(|| {
            let contract = pincer_contract::tool_contract("set_window_frame")
                .expect("set_window_frame contract");
            ToolDef {
                name: contract.name,
                // X11 geometry is the client window's (checked under KWin,
                // 2026-11-11: a frame set at y=100 left the 28 px title bar
                // at y=72..100, per _NET_FRAME_EXTENTS).
                description: format!(
                    "{} On Linux the frame is the client area, as in list_windows bounds: \
                     the window manager's title bar and borders lie outside it, so the title \
                     bar sits above y. On X11 a maximized or full-screen window is restored \
                     first. The result gives the frame read back as WxH+X+Y.",
                    contract.description
                ),
                input_schema: contract.input_schema,
                read_only: contract.annotations.read_only,
                destructive: contract.annotations.destructive,
                idempotent: contract.annotations.idempotent,
                open_world: contract.annotations.open_world,
            }
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_contract::SetWindowFrameInput;
        use pincer_core::action_record::{
            effect_from_value_readback, ActionEvidence, ActionExecutionRecord, ActionTransport,
            ActualDelivery, EvidenceKind, RequestedDelivery,
        };

        let input: SetWindowFrameInput =
            match pincer_core::tool_args::parse_typed_input("set_window_frame", args) {
                Ok(input) => input,
                Err(result) => return result,
            };
        if crate::wayland::is_wayland() {
            return ToolResult::error(
                "set_window_frame: this Wayland compositor exposes no protocol that permits a client to set another top-level window's geometry",
            );
        }
        let values = [input.x, input.y, input.width, input.height];
        if values.iter().any(|value| !value.is_finite())
            || input.width <= 0.0
            || input.height <= 0.0
        {
            return ToolResult::error(
                "set_window_frame: x/y must be finite and width/height must be finite positive numbers",
            );
        }
        let to_i32 = |name: &str, value: f64| -> Result<i32, String> {
            let rounded = value.round();
            if rounded < i32::MIN as f64 || rounded > i32::MAX as f64 {
                Err(format!("{name} is out of range for an X11 coordinate"))
            } else {
                Ok(rounded as i32)
            }
        };
        let to_u32 = |name: &str, value: f64| -> Result<u32, String> {
            let rounded = value.round();
            if rounded < 1.0 || rounded > u32::MAX as f64 {
                Err(format!("{name} is out of range for an X11 size"))
            } else {
                Ok(rounded as u32)
            }
        };
        let x = match to_i32("x", input.x) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let y = match to_i32("y", input.y) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let width = match to_u32("width", input.width) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let height = match to_u32("height", input.height) {
            Ok(value) => value,
            Err(error) => return ToolResult::error(format!("set_window_frame: {error}")),
        };
        let window_id = input.window_id;
        let pid = input.pid;
        let outcome = tokio::task::spawn_blocking(move || {
            let before = crate::x11::list_windows(Some(pid))
                .into_iter()
                .find(|window| window.xid == window_id)
                .map(|window| (window.x, window.y, window.width, window.height));
            let (observed, confirmed, mutation_error) =
                crate::x11::set_window_frame(window_id, pid, x, y, width, height)
                    .map_err(|error| error.to_string())?;
            let observed_frame =
                observed.map(|window| (window.x, window.y, window.width, window.height));
            let changed = observed_frame.is_some_and(|observed| before != Some(observed));
            Ok::<_, String>((observed_frame, confirmed, changed, mutation_error))
        })
        .await;
        let (observed, confirmed, changed, mutation_error) = match outcome {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => return ToolResult::error(format!("set_window_frame: {error}")),
            Err(error) => {
                return ToolResult::error(format!(
                    "set_window_frame: blocking task failed: {error}"
                ));
            }
        };
        let effect = effect_from_value_readback(confirmed, changed, observed.is_some());
        let requested = (x, y, width, height);
        let mut record = ActionExecutionRecord::builder(
            effect,
            ActionTransport::LinuxX11ConfigureWindow,
            RequestedDelivery::NotApplicable,
        )
        .actual_delivery(ActualDelivery::NotApplicable)
        .detail(format!(
            "requested={requested:?} observed={observed:?} mutation_error={mutation_error:?}"
        ));
        if observed.is_some() {
            record = record.evidence(ActionEvidence {
                kind: EvidenceKind::ValueReadback,
                detail: if confirmed {
                    "X11 geometry readback matched the requested frame".into()
                } else {
                    "X11 geometry readback did not match the requested frame".into()
                },
            });
        }
        ToolResult::text(set_window_frame_text(requested, observed, confirmed))
            .with_action_record(record.build().expect("set_window_frame record is valid"))
    }
}

/// set_window_frame's text, with the frame read back in list_windows' own
/// `WxH+X+Y` form so the caller needs no list_windows call to see it (round
/// 2 of the first-call test).
fn set_window_frame_text(
    requested: (i32, i32, u32, u32),
    observed: Option<(i32, i32, u32, u32)>,
    confirmed: bool,
) -> String {
    let frame = |(x, y, width, height): (i32, i32, u32, u32)| format!("{width}x{height}+{x}+{y}");
    match observed {
        Some(observed) if confirmed => format!(
            "Set and verified the window frame: it reads back as {}.",
            frame(observed)
        ),
        Some(observed) => format!(
            "The window frame did not settle at the requested geometry: asked for {}, it \
             reads back as {}.",
            frame(requested),
            frame(observed)
        ),
        None => "The native window mutation was attempted, but its resulting frame could not \
                 be read back."
            .to_owned(),
    }
}

// ── bring_to_front (Linux) ───────────────────────────────────────────────────

pub struct BringToFrontTool;

static BTF_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for BringToFrontTool {
    fn def(&self) -> &ToolDef {
        BTF_DEF.get_or_init(|| ToolDef {
            name: "bring_to_front".into(),
            description:
                // Screenshot advice checked on KWin X11, 2026-11-11: with
                // compositing on, a covered window's capture showed only its
                // own pixels; with compositing blocked
                // (_KDE_NET_WM_BLOCK_COMPOSITING) it showed the window over
                // it, and a minimized window's capture failed (BadMatch)
                // until bring_to_front restored it.
                "Activate a window and raise it above the others, and leave it that way, \
                 so later input lands on it. On X11 this sends _NET_ACTIVE_WINDOW, which \
                 window managers such as KWin answer by raising the window (and restoring \
                 a minimized one); the result reads the stacking order back: whether the \
                 window is now active and on top, and any overlapping window still above \
                 it (a keep-above window, for example). \
                 This deliberately breaks the no-foreground contract and is not part of \
                 the normal input ladder. For an ordinary `background_unavailable` \
                 response, retry only the refused action with \
                 `delivery_mode:\"foreground\"`; the input tool activates the window \
                 itself. Use `bring_to_front` for a surface that must stay in front \
                 across several calls, such as a remote desktop session, or before \
                 reading the screenshot of a minimized window (its capture fails) or, \
                 with no compositing window manager running, of a covered one (its \
                 capture shows the window over it). Wayland: activates through a \
                 target-addressable compositor adapter (wlroots foreign-toplevel or the \
                 GNOME Shell helper) and refuses when the compositor offers no safe \
                 adapter."
                .into(),
            input_schema: serde_json::json!({
                "type":"object","required":["pid"],"properties":{
                    "pid":{"type":"integer","description":"Process ID of the app to activate."},
                    "window_id":{"type":"integer","description":"X11 window id (xid) to activate. If omitted, the first window of `pid` is used."}
                },"additionalProperties":false
            }),
            read_only: false, destructive: false, idempotent: true, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        let pid = args.u64_or("pid", 0) as u32;
        if crate::wayland::is_wayland() {
            let window_id = match args.opt_u64("window_id") {
                Some(window_id) => window_id,
                None => match crate::wayland::list_windows_dispatch(Some(pid)).first() {
                    Some(window) => window.xid,
                    None => {
                        return ToolResult::error(format!(
                            "bring_to_front: no window_id given and no Wayland windows found for pid {pid}."
                        ))
                    }
                },
            };
            if crate::wayland::wayland_input_enabled() && crate::wayland::hyprland::is_session() {
                if args.opt_u64("window_id").is_none() {
                    return foreground_hyprland_refusal("an exact window_id is required");
                }
                return foreground_hyprland_action(
                    &args,
                    pid,
                    window_id,
                    crate::wayland::hyprland_input::Action::Activate,
                )
                .await;
            }
            let result = tokio::task::spawn_blocking(move || {
                crate::wayland::activate_window_for_input_target(window_id, Some(pid))
            })
            .await;
            return match result {
                Ok(Ok(())) => {
                    ToolResult::text(format!("Brought Wayland window {window_id} to front."))
                        .with_structured(serde_json::json!({
                            "window_id": window_id,
                            "platform": "linux",
                            "session": "wayland",
                        }))
                }
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(format!("Task error: {error}")),
            };
        }

        // X11: resolve the target xid (window_id, else first window for pid).
        let xid = match args.opt_u64("window_id") {
            Some(x) => x,
            None => {
                let windows =
                    tokio::task::spawn_blocking(move || crate::x11::list_windows(Some(pid)))
                        .await
                        .unwrap_or_default();
                match windows.first() {
                    Some(w) => w.xid,
                    None => {
                        return ToolResult::error(format!(
                        "bring_to_front: no window_id given and no windows found for pid {pid}."
                    ))
                    }
                }
            }
        };
        let r = tokio::task::spawn_blocking(move || {
            let before = StackingOutcome::read(xid);
            let prior = crate::input::x11_activate_window_persistent(xid)?;
            Ok::<_, anyhow::Error>((prior, before, StackingOutcome::settle(xid)))
        })
        .await;
        match r {
            Ok(Ok((prior, before, after))) => bring_to_front_result(xid, prior, before, after),
            Ok(Err(e)) => ToolResult::error(format!("bring_to_front failed: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// Where a window sits after bring_to_front: whether the window manager
/// reports it active, and the on-screen windows stacked above it
/// (`_NET_CLIENT_LIST_STACKING`, bottom to top) that overlap it.
#[derive(Debug, Clone, PartialEq)]
struct StackingOutcome {
    /// `_NET_ACTIVE_WINDOW`; `None` without an EWMH window manager.
    active: Option<u64>,
    /// No on-screen window is stacked above it at all.
    top_of_stack: bool,
    /// On-screen windows above it whose bounds intersect its own.
    covered_by: Vec<(u64, String)>,
}

impl StackingOutcome {
    /// Blocking. `None` when the window is not in the stacking list.
    fn read(xid: u64) -> Option<Self> {
        let windows = crate::x11::list_windows(None);
        Self::from_windows(xid, &windows, crate::x11::active_window())
    }

    fn from_windows(
        xid: u64,
        windows: &[crate::x11::WindowInfo],
        active: Option<u64>,
    ) -> Option<Self> {
        let target = windows.iter().find(|w| w.xid == xid)?;
        let z = target.z_index?;
        let overlaps = |w: &crate::x11::WindowInfo| {
            w.x < target.x + target.width as i32
                && target.x < w.x + w.width as i32
                && w.y < target.y + target.height as i32
                && target.y < w.y + w.height as i32
        };
        let above: Vec<&crate::x11::WindowInfo> = windows
            .iter()
            .filter(|w| w.is_on_screen && w.xid != xid && w.z_index.is_some_and(|wz| wz > z))
            .collect();
        Some(Self {
            active,
            top_of_stack: above.is_empty(),
            covered_by: above
                .into_iter()
                .filter(|w| overlaps(w))
                .map(|w| (w.xid, w.title.clone()))
                .collect(),
        })
    }

    fn settled(&self, xid: u64) -> bool {
        self.active.is_none_or(|active| active == xid) && self.covered_by.is_empty()
    }

    /// The outcome once the window manager has acted on the request: polled
    /// until the window is active and uncovered, for up to a second (KWin
    /// restacks within a few milliseconds of the client message).
    fn settle(xid: u64) -> Option<Self> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let outcome = Self::read(xid);
            if outcome.as_ref().is_none_or(|o| o.settled(xid))
                || std::time::Instant::now() >= deadline
            {
                return outcome;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

/// `5 "Kate", 6`: window ids with their titles escaped and quoted
/// (`quoted_title`).
fn describe_windows(windows: &[(u64, String)]) -> String {
    windows
        .iter()
        .map(|(id, title)| {
            if title.is_empty() {
                id.to_string()
            } else {
                format!("{id} {}", quoted_title(title))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// bring_to_front's X11 result: what the stacking order says now, and
/// whether anything changed, so a call on a window that was already in
/// front still has something to check.
fn bring_to_front_result(
    xid: u64,
    prior: Option<u64>,
    before: Option<StackingOutcome>,
    after: Option<StackingOutcome>,
) -> ToolResult {
    let Some(after) = after else {
        return ToolResult::text(format!(
            "Asked the window manager to activate window {xid} (X11 _NET_ACTIVE_WINDOW), but \
             it is not in the stacking list, so whether it is in front cannot be read."
        ))
        .with_structured(json!({
            "window_id": xid,
            "prior_active": prior,
            "platform": "linux",
        }));
    };
    let active = after.active.is_none_or(|active| active == xid);
    let on_top = after.covered_by.is_empty();
    let unchanged = before.as_ref() == Some(&after) && prior == Some(xid);
    let structured = json!({
        "window_id": xid,
        "prior_active": prior,
        "active": active,
        "top_of_stack": after.top_of_stack,
        "covered_by": after.covered_by.iter().map(|(id, title)| json!({"window_id": id, "title": title})).collect::<Vec<_>>(),
        "raised": before.as_ref().is_some_and(|b| !b.covered_by.is_empty()) && on_top,
        "platform": "linux",
    });
    if !active {
        return ToolResult::error(format!(
            "bring_to_front: asked the window manager to activate window {xid}, but after 1 s \
             the active window is still {}. The window may be minimized, on another desktop, \
             or blocked by a modal dialog (target that dialog instead), or the window \
             manager's focus-stealing prevention kept it back.",
            after
                .active
                .map(|a| a.to_string())
                .unwrap_or_else(|| "unset".to_owned())
        ))
        .with_structured(structured);
    }
    let position = if on_top {
        "the active window with no other window over it".to_owned()
    } else {
        format!(
            "the active window, but these windows are stacked above it and overlap it: {}",
            describe_windows(&after.covered_by)
        )
    };
    let change = if unchanged {
        " It already was before this call; nothing changed.".to_owned()
    } else {
        match before.as_ref().filter(|b| !b.covered_by.is_empty()) {
            Some(b) if on_top => {
                format!(" Before, it was under {}.", describe_windows(&b.covered_by))
            }
            _ => String::new(),
        }
    };
    // "is now" only when this call changed something (round 6 read "is now
    // ... It already was" as a contradiction).
    let now = if unchanged { "" } else { "now " };
    ToolResult::text(format!(
        "✅ Window {xid} is {now}{position} (X11 _NET_ACTIVE_WINDOW).{change}"
    ))
    .with_structured(structured)
}

#[cfg(test)]
#[test]
fn stacking_outcome_names_overlapping_windows_above_the_target() {
    let window = |xid, z, x, y, on_screen| crate::x11::WindowInfo {
        xid,
        pid: Some(1),
        app_name: String::new(),
        title: format!("w{xid}"),
        is_on_screen: on_screen,
        z_index: Some(z),
        x,
        y,
        width: 100,
        height: 100,
    };
    // Bottom to top: target, an overlapping window, a distant one, a
    // minimized overlapping one.
    let windows = [
        window(10, 0, 0, 0, true),
        window(11, 1, 50, 50, true),
        window(12, 2, 500, 500, true),
        window(13, 3, 10, 10, false),
    ];
    let outcome = StackingOutcome::from_windows(10, &windows, Some(11)).unwrap();
    assert!(!outcome.top_of_stack);
    assert_eq!(outcome.covered_by, vec![(11, "w11".to_owned())]);
    assert!(!outcome.settled(10));

    let top = StackingOutcome::from_windows(12, &windows, Some(12)).unwrap();
    assert!(top.top_of_stack && top.covered_by.is_empty() && top.settled(12));
    // Without an EWMH active window, an uncovered window counts as settled.
    let no_wm = StackingOutcome::from_windows(12, &windows, None).unwrap();
    assert!(no_wm.settled(12));
    assert!(StackingOutcome::from_windows(99, &windows, None).is_none());

    let result = bring_to_front_result(10, Some(11), Some(outcome.clone()), Some(outcome));
    assert_eq!(result.is_error, Some(true), "not active must be refused");
}

#[cfg(test)]
#[test]
fn held_button_moves_only_in_its_own_delivery_mode() {
    let hold = MouseHoldState {
        pid: 1,
        xid: 2,
        button: 1,
        x: 0.0,
        y: 0.0,
        foreground: true,
    };
    assert!(held_delivery_mismatch(&json!({}), "c", &hold).is_none());
    assert!(held_delivery_mismatch(&json!({"delivery_mode": "foreground"}), "c", &hold).is_none());
    let refused =
        held_delivery_mismatch(&json!({"delivery_mode": "background"}), "c", &hold).unwrap();
    let structured = refused.structured_content.unwrap();
    assert_eq!(structured["code"], "delivery_mode_mismatch");
    assert_eq!(structured["delivery_mode"], "foreground");
    assert_eq!(structured["held"], true);
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
    r.register(Box::new(ParallelMouseDragTool {
        state: state.clone(),
    }));
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
    r.register(Box::new(CaptureSeriesTool));
    // `type_text_chars` is a deprecated invoke-time alias for `type_text`.
    // Keep it out of tools/list, matching the macOS and Windows registries.
    let _: &TypeTextCharsTool = &TypeTextCharsTool;
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

#[cfg(test)]
mod cursor_hook_emission_tests {
    use super::emit_cursor_hook;
    use pincer_core::cursor_hook::{set_cursor_hook_fn, CursorHookEvent};
    use std::sync::{Arc, Mutex};

    /// Linux pointer actions report the agent cursor to an embedder's hook
    /// (cua-spacesd turns them into presence), moves and presses alike, and
    /// an anonymous cursor id never reaches it.
    #[test]
    fn pointer_actions_reach_the_cursor_hook() {
        let seen: Arc<Mutex<Vec<CursorHookEvent>>> = Arc::default();
        let sink = seen.clone();
        if !set_cursor_hook_fn(move |e| sink.lock().unwrap().push(e)) {
            // Another test in this binary owns the process-wide hook.
            return;
        }
        emit_cursor_hook("agent-7", 10.0, 20.0, false);
        emit_cursor_hook("agent-7", 10.0, 20.0, true);
        emit_cursor_hook("", 1.0, 1.0, false);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            (
                seen[0].cursor_id.as_str(),
                seen[0].x,
                seen[0].y,
                seen[0].pressed
            ),
            ("agent-7", 10.0, 20.0, false)
        );
        assert!(seen[1].pressed);
    }
}
