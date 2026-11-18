//! Which toolkit or engine a target process runs, read from /proc, for the
//! input routes that depend on it.

use super::*;

/// True when `pid` is a Chromium-based embedder — a Chrome/Chromium browser or
/// any Electron/CEF app. On these surfaces an AT-SPI `EditableText.insertText`
/// can succeed at the bridge while the Chromium *renderer* never observes it,
/// so the AT-SPI "ax" rung must not be trusted as a confirmed insert (see
/// [`type_text_ax_result`]).
///
/// This is the Linux analogue of macOS `ElectronJs::is_electron` (which checks
/// for a bundled Electron Framework). Two signals, either one enough:
/// - the process's executable, by the same detection launch_app and the
///   browser tools use ([`crate::web_engine::is_chromium_process`]: a
///   Chromium name, or Chromium's resource packs beside the binary), which
///   holds before the app has started any helper;
/// - Chromium's multiprocess fingerprint: the embedder forks sandboxed
///   helpers whose argv carries `--type=renderer` / `--type=zygote` /
///   `--type=gpu-process` (a single-process embedder carries it itself).
///   Native GTK/Qt apps never spawn such helpers.
///
/// Reads `/proc` and stats a few files; cheap enough for every input call.
pub(super) fn is_chromium_embedder(pid: u32) -> bool {
    fn argv_is_chromium_helper(p: u32) -> bool {
        fs::read(format!("/proc/{p}/cmdline"))
            .map(|raw| crate::at_point_policy::cmdline_is_chromium_helper(&raw))
            .unwrap_or(false)
    }
    crate::web_engine::is_chromium_process(pid)
        || argv_is_chromium_helper(pid)
        || descendant_pids(pid).into_iter().any(argv_is_chromium_helper)
}

pub(super) fn is_webkitgtk_embedder(pid: u32) -> bool {
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

pub(super) fn maps_indicate_gtk(maps: &str) -> bool {
    maps.contains("libgtk-3.so") || maps.contains("libgtk-4.so")
}

/// Toolkits known to discard synthetic (`send_event`) X11 pointer events:
/// GTK3/4 (XInput2 only), LibreOffice VCL (all plugins), Qt5/6 (xcb, XI2).
pub(super) fn maps_indicate_synthetic_pointer_dropped(maps: &str) -> bool {
    maps_indicate_gtk(maps)
        || maps.contains("libvcl")
        || maps.contains("libmergedlo")
        || maps.contains("libQt5Gui")
        || maps.contains("libQt6Gui")
}

pub(super) fn synthetic_pointer_is_dropped(pid: u32) -> bool {
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
pub(super) fn maps_indicate_synthetic_press_dropped(maps: &str) -> bool {
    maps_indicate_gtk(maps) || maps.contains("libvcl") || maps.contains("libmergedlo")
}

pub(super) fn synthetic_press_is_dropped(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/maps"))
        .map(|maps| maps_indicate_synthetic_press_dropped(&maps))
        .unwrap_or(false)
}

/// Qt5 maps its own GUI library; only then is the synthetic-FocusIn bridge
/// workaround in `type_text` worth two extra AT-SPI walks.
pub(super) fn is_qt5_process(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/maps"))
        .map(|maps| maps.contains("libQt5Gui") || maps.contains("libQt5Widgets"))
        .unwrap_or(false)
}

pub(super) fn is_gtk_process(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/maps"))
        .map(|maps| maps_indicate_gtk(&maps))
        .unwrap_or(false)
}
