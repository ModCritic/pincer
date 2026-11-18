//! get_window_state: the window's element tree, screenshot, popups and the
//! windows that cover it.

use super::*;

// ── get_window_state ─────────────────────────────────────────────────────────

/// The largest window screenshot delivered as-is. The Anthropic API downsizes
/// any image above ~1.15 megapixels before the model sees it, so a 1568x861
/// PNG reached the model as ~1447x795 while the driver still mapped its x/y
/// as 1568x861: every pixel click landed a uniform 0.94x short. Capping here
/// keeps "pixels of THIS screenshot" true for the image the model reads.
pub(super) const WINDOW_SCREENSHOT_MAX_PIXELS: u64 = 1_150_000;

/// The long edge a `w`x`h` image must shrink to so that it holds at most
/// `max_pixels` pixels; `None` when it already fits.
pub(super) fn megapixel_long_edge_cap(w: u32, h: u32, max_pixels: u64) -> Option<u32> {
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
pub(super) fn window_screenshot_size(w: u32, h: u32, max_dim: u32, explicit_max_dim: bool) -> (u32, u32) {
    let (w, h) = pincer_core::image_utils::fit_long_edge_dimensions(w, h, max_dim);
    if explicit_max_dim {
        return (w, h);
    }
    match megapixel_long_edge_cap(w, h, WINDOW_SCREENSHOT_MAX_PIXELS) {
        Some(edge) => pincer_core::image_utils::fit_long_edge_dimensions(w, h, edge),
        None => (w, h),
    }
}

/// Build a single structured element entry for `get_window_state`.
/// Returns `None` when the node has no `element_index` (non-actionable rows).
pub(super) fn build_element_entry(
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
pub(super) fn query_note(
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
pub(super) fn popup_tree_markdown(
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
pub(super) fn popup_menu_elements(
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
pub(super) fn framed_elements_first(elements: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
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
pub(super) struct WindowOverlays {
    pub(super) dialogs: Vec<serde_json::Value>,
    pub(super) popups: Vec<serde_json::Value>,
    pub(super) covers_window: bool,
    pub(super) window_rect: Option<(i32, i32, u32, u32)>,
    /// The pid's window that pid-only keys go to (see
    /// `pid_fallback_window_resolver`), so only that dialog is told so.
    pub(super) key_window: Option<u64>,
}

impl WindowOverlays {
    /// One sentence per overlay naming the call that acts on it. A dialog's
    /// title is the app's text inside Pincer's instruction, so it stays one
    /// escaped, quoted value (`quoted_title`): a `"` or a line break in it
    /// cannot end the quote and continue the sentence as Pincer.
    pub(super) fn follow_up(&self, pid: u32) -> Option<String> {
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

pub(super) fn rects_intersect(a: (i32, i32, u32, u32), b: (i32, i32, u32, u32)) -> bool {
    a.0 < b.0 + b.2 as i32
        && a.0 + a.2 as i32 > b.0
        && a.1 < b.1 + b.3 as i32
        && a.1 + a.3 as i32 > b.1
}

/// Whether `xid` is still a window `pid` may be observed through. Shared by
/// `get_window_state` and `capture_series`; `window_meta` is the listed
/// window when the caller already enumerated it.
pub(super) fn window_target_matches(pid: u32, xid: u64, window_meta: Option<&crate::x11::WindowInfo>) -> bool {
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
pub(super) fn capture_window_image(
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
pub(super) fn dialog_is_over_target(
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
pub(super) fn transient_chain_reaches(
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

pub(super) fn window_overlays(pid: u32, xid: u64) -> WindowOverlays {
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
    pub(super) state: Arc<ToolState>,
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
pub(super) fn window_state_captures_screenshot(
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
                    // Windows of processes that have exited leave the store,
                    // and with it the AT-SPI side index, instead of holding
                    // their elements until the runtime ends.
                    if !observation_only {
                        for (gone_pid, gone_xid) in
                            crate::atspi::snapshot::windows_of_exited_processes()
                        {
                            state.snapshots.remove(gone_pid as i32, gone_xid);
                        }
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
                        structured["degraded_reason"] = json!(format!(
                            "atspi_tree_empty: the AT-SPI walk returned no actionable \
                             elements. Common causes: {}, the daemon is not on the \
                             desktop session bus (DBUS_SESSION_BUS_ADDRESS unreachable — \
                             run `pincer doctor`), or the window is a non-AX surface \
                             (canvas/WebGL/custom-drawn). Do not treat element data as \
                             authoritative — verify via the screenshot, and re-snapshot \
                             after enabling a11y or if the app just launched.",
                            crate::a11y::bridge_off_advice(
                                crate::a11y::current_desktop().as_deref()
                            )
                        ));
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
