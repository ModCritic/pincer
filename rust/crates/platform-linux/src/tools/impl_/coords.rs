//! Coordinate frames: desktop and window pixels, screenshot regions, element
//! centers, and the point resolver of the pointer tools.

use super::*;

/// `native_pixels:true`: coordinates or a region are in the target's own
/// pixels (full-size desktop pixels, or the window's native pixels), not in
/// the screenshot the caller was given.
pub(super) fn native_pixels_requested(args: &Value) -> bool {
    args.get("native_pixels").and_then(Value::as_bool) == Some(true)
}

/// Schema of the `native_pixels` flag.
pub(super) fn native_pixels_schema(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

/// Map a region read off a desktop image back to full-size desktop pixels,
/// rounded to whole pixels and kept inside what the image shows.
pub(super) fn desktop_region_from_image(
    region: pincer_core::image_utils::PixelRect,
    frame: &DesktopImageFrame,
) -> Result<pincer_core::image_utils::PixelRect, String> {
    let (image_w, image_h) = frame.image;
    if !region.fits_within(image_w, image_h) {
        return Err(format!(
            "region {},{} {}x{} does not fit the {image_w}x{image_h} get_desktop_state image \
             it is read from (needs x+width <= {image_w} and y+height <= {image_h}); pass \
             native_pixels:true for full-size desktop pixels",
            region.x, region.y, region.width, region.height
        ));
    }
    let shows = frame.shows;
    let axis = |start: u32, length: u32, image: u32, shows_start: u32, shows_len: u32| {
        let scale = f64::from(shows_len) / f64::from(image);
        let lo = (f64::from(shows_start) + f64::from(start) * scale).round() as u32;
        let hi = (f64::from(shows_start) + f64::from(start + length) * scale).round() as u32;
        let lo = lo.clamp(shows_start, shows_start + shows_len - 1);
        let hi = hi.clamp(lo + 1, shows_start + shows_len);
        (lo, hi - lo)
    };
    let (x, width) = axis(region.x, region.width, image_w, shows.x, shows.width);
    let (y, height) = axis(region.y, region.height, image_h, shows.y, shows.height);
    Ok(pincer_core::image_utils::PixelRect {
        x,
        y,
        width,
        height,
    })
}

/// Map a region read off a window's get_window_state screenshot to its
/// native pixels (`scale` native pixels per screenshot pixel), rounded to
/// whole pixels and kept inside `native` when that size is known.
pub(super) fn window_region_from_screenshot(
    region: pincer_core::image_utils::PixelRect,
    scale: f64,
    native: Option<(u32, u32)>,
) -> pincer_core::image_utils::PixelRect {
    let axis = |start: u32, length: u32, limit: Option<u32>| {
        let mut lo = (f64::from(start) * scale).round().max(0.0) as u32;
        let mut hi = (f64::from(start + length) * scale).round().max(0.0) as u32;
        if let Some(limit) = limit.filter(|limit| *limit > 0) {
            lo = lo.min(limit - 1);
            hi = hi.min(limit);
        }
        (lo, hi.max(lo + 1) - lo)
    };
    let (x, width) = axis(region.x, region.width, native.map(|size| size.0));
    let (y, height) = axis(region.y, region.height, native.map(|size| size.1));
    pincer_core::image_utils::PixelRect {
        x,
        y,
        width,
        height,
    }
}

// ── shared helpers ────────────────────────────────────────────────────────────

/// Resolve an AT-SPI element's center in window-local coordinates.
///
/// Returns `(xid, window_local_x, window_local_y)`.
/// Looks up element bounds via native AT-SPI, finds the owning window
/// (via `xid_hint` or the first window for `pid`), then converts screen-absolute
/// → window-local coords via compositor metadata on Wayland or
/// XTranslateCoordinates on X11.
pub(super) fn resolve_element_local_coords(
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
/// reports empty extents for some widgets (a text view center resolves to
/// (0,0), the GNOME hot corner), and a real pointer click there would hit
/// whatever is under that screen point. Refused before any input is sent.
#[derive(Debug)]
pub(super) struct ElementBoundsUnusable {
    pub(super) idx: usize,
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

/// `resolve_element_local_coords` plus the sanity check that the center lies
/// inside the window it belongs to. Blocking.
pub(super) fn checked_element_local_coords(
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

pub(super) fn element_screen_center(pid: u32, idx: usize, xid: Option<u64>) -> anyhow::Result<(f64, f64)> {
    element_screen_center_and_rect(pid, idx, xid).map(|(center, _)| center)
}

/// The element's screen center plus its AT-SPI extents as the overlay's
/// `[x, y, width, height]`, both from one bounds lookup (same screen space).
pub(super) fn element_screen_center_and_rect(
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
pub(super) fn frame_target_rect(bounds: Option<(i32, i32, u32, u32)>, sx: f64, sy: f64) -> Option<[f64; 4]> {
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
pub(super) fn coordinate_frame_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["window", "desktop"],
        "description": "Frame of all x/y: \"window\" (default) get_window_state screenshot \
            pixels; \"desktop\" get_desktop_state pixels (scope:\"desktop\" with pid/window_id \
            means the same)."
    })
}

/// Shared schema for the optional `cursor_id` of pointer tools (move_cursor
/// says more, since there the session label names the cursor).
pub(super) fn cursor_id_schema() -> Value {
    json!({
        "type": "string",
        "description": "Multi-cursor instance id (default \"default\")."
    })
}

pub(super) fn coordinate_frame_is_desktop(args: &Value) -> bool {
    args.get("coordinate_frame").and_then(Value::as_str) == Some("desktop")
}

pub(super) fn window_local_to_screen(xid: u64, x: f64, y: f64) -> anyhow::Result<(f64, f64)> {
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
pub(super) const ELEMENT_AX_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Either `coordinate_frame:"desktop"` or `scope:"desktop"` marks `x,y` as
/// desktop (screen) pixels; both are translated exactly once by the caller.
pub(super) fn desktop_frame_requested(args: &Value) -> bool {
    coordinate_frame_is_desktop(args)
        || args.get("scope").and_then(Value::as_str) == Some("desktop")
}

/// Convert desktop (screen) coordinates to `xid`'s window-local frame so the
/// rest of the pointer pipeline (which is window-local by contract) is reused.
pub(super) fn desktop_to_window_local(xid: u64, x: f64, y: f64) -> anyhow::Result<(f64, f64)> {
    desktop_to_window_local_in(
        crate::wayland::wayland_input_enabled(),
        xid,
        (x, y),
        crate::wayland::window_geometry,
    )
}

/// [`desktop_to_window_local`] with the backend and the Wayland geometry
/// source passed in. A native Wayland window id is no X11 window: its
/// origin comes from the geometry `wayland::window_local_to_output` maps the
/// point back to the output with.
pub(super) fn desktop_to_window_local_in(
    wayland: bool,
    xid: u64,
    (x, y): (f64, f64),
    wayland_geometry: impl FnOnce(u64) -> Option<(i32, i32, u32, u32)>,
) -> anyhow::Result<(f64, f64)> {
    let (ox, oy) = if wayland {
        match wayland_geometry(xid) {
            Some((wx, wy, width, height)) if width > 0 && height > 0 => {
                (f64::from(wx), f64::from(wy))
            }
            _ => anyhow::bail!("No Wayland geometry for window {xid}"),
        }
    } else {
        window_local_to_screen(xid, 0.0, 0.0)?
    };
    Ok((x - ox, y - oy))
}

#[derive(Debug)]
enum CoordinateContext {
    Zoom(ZoomContext),
    Screenshot(f64),
    /// Desktop (get_desktop_state) pixels, translated into the window.
    Desktop,
    /// Pixels of a published capture, admitted through its `capture_id`.
    Capture(String),
}

/// The frame of a pointer call's x,y. An unsupported native background
/// delivery must report `background_unavailable` before the frame is
/// consulted, because native Wayland may legitimately withhold the
/// screenshot (`surface_identity_unproven`) that the frame needs.
fn coordinate_context(
    native_refusal: Option<ToolResult>,
    resolve_context: impl FnOnce() -> Result<CoordinateContext, ToolResult>,
) -> Result<CoordinateContext, ToolResult> {
    if let Some(refusal) = native_refusal {
        return Err(refusal);
    }
    resolve_context()
}

/// Map the x,y `points` of a pointer call on window `xid` into its real
/// window pixels, from the frame the call names: desktop pixels
/// (`coordinate_frame`/`scope` "desktop"), a published capture
/// (`capture_id`, one point only), a zoom image (`from_zoom`), or by default
/// the window's latest get_window_state screenshot. The one resolver behind
/// click, double_click, right_click, drag, scroll, mouse_button_down,
/// mouse_drag and mouse_button_up; `native_refusal` is reported before any
/// frame is read.
pub(super) async fn resolve_window_points<const N: usize>(
    state: &ToolState,
    args: &Value,
    tool: &str,
    (pid, xid): (u32, u64),
    points: [(f64, f64); N],
    native_refusal: Option<ToolResult>,
) -> Result<[(f64, f64); N], ToolResult> {
    let desktop_frame = desktop_frame_requested(args);
    let from_zoom = args.bool_or("from_zoom", false);
    let capture_id = args.opt_str("capture_id");
    let invalid = |message: String| {
        ToolResult::error(message).with_structured(json!({ "code": "invalid_arguments" }))
    };
    if capture_id.is_some() {
        if from_zoom {
            return Err(invalid(format!(
                "{tool}.capture_id cannot be combined with from_zoom; use coordinates from the \
                 published capture."
            )));
        }
        if desktop_frame {
            return Err(invalid(format!(
                "{tool}.capture_id cannot be combined with desktop-frame coordinates; use \
                 coordinates from the published capture."
            )));
        }
        if N != 1 {
            return Err(invalid(format!("{tool}.capture_id takes a single x,y point.")));
        }
    }
    let context = coordinate_context(native_refusal, || {
        if desktop_frame {
            Ok(CoordinateContext::Desktop)
        } else if let Some(capture_id) = &capture_id {
            Ok(CoordinateContext::Capture(capture_id.clone()))
        } else if from_zoom {
            state
                .zoom_context(args, pid, Some(xid))
                .map(CoordinateContext::Zoom)
        } else {
            screenshot_scale(state, args, pid, Some(xid)).map(CoordinateContext::Screenshot)
        }
    })?;
    match context {
        CoordinateContext::Screenshot(scale) => Ok(points.map(|(x, y)| (x * scale, y * scale))),
        CoordinateContext::Zoom(context) => Ok(points.map(|(x, y)| context.zoom_to_window(x, y))),
        CoordinateContext::Capture(capture_id) => {
            let mut points = points;
            for point in &mut points {
                *point = crate::capture_action_frame::admit_window_click(
                    &state.capture_service,
                    args,
                    &capture_id,
                    pid,
                    xid,
                    point.0,
                    point.1,
                )
                .map_err(capture_admission_error)?;
            }
            Ok(points)
        }
        CoordinateContext::Desktop => {
            let mapped = tokio::task::spawn_blocking(move || {
                let mut points = points;
                for point in &mut points {
                    *point = desktop_to_window_local(xid, point.0, point.1)?;
                }
                Ok::<_, anyhow::Error>(points)
            })
            .await;
            match mapped {
                Ok(Ok(points)) => Ok(points),
                Ok(Err(e)) => Err(ToolResult::error(format!(
                    "desktop-frame coordinates could not be mapped into window {xid}: {e}"
                ))),
                Err(e) => Err(ToolResult::error(format!("Task error: {e}"))),
            }
        }
    }
}

/// Screen-absolute center of a window (top-left from translate_coordinates plus
/// half its geometry). Used to position the no-focus-steal scroll over the
/// window's content. Blocking — call inside spawn_blocking.
pub(super) fn window_screen_center(xid: u64) -> anyhow::Result<(i32, i32)> {
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

#[cfg(test)]
#[test]
fn coordinate_context_keeps_wayland_background_refusal_ahead_of_screenshot_context() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let resolved = AtomicBool::new(false);
    let native_refusal = crate::input::delivery::background_unavailable_error(
        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
    );
    let result = coordinate_context(Some(native_refusal), || {
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
    assert!(matches!(
        coordinate_context(None, || Ok(CoordinateContext::Screenshot(1.5))),
        Ok(CoordinateContext::Screenshot(scale)) if scale == 1.5
    ));
}

#[cfg(test)]
#[tokio::test]
async fn window_point_resolver_maps_each_frame_and_refuses_first() {
    let state = ToolState::new();
    let refusal = crate::input::delivery::background_unavailable_error(
        crate::input::delivery::BackgroundUnavailable::FocusedInputOnly,
    );
    let args = json!({"_session_id": "resolver"});
    // The refusal wins over the missing screenshot.
    let refused =
        resolve_window_points(&state, &args, "click", (42, 7), [(1.0, 2.0)], Some(refusal))
            .await
            .expect_err("refused");
    assert_eq!(
        refused.structured_content.as_ref().unwrap()["code"],
        "background_unavailable"
    );
    // Without it, a window never read has no frame.
    let missing = resolve_window_points(&state, &args, "drag", (42, 7), [(1.0, 2.0); 2], None)
        .await
        .expect_err("no screenshot");
    assert_eq!(
        missing.structured_content.as_ref().unwrap()["code"],
        "screenshot_context_missing"
    );
    // A trusted caller's native window pixels pass through.
    let native = json!({
        "_session_id": "resolver",
        pincer_core::snapshot_store::NATIVE_WINDOW_PIXELS_ARG: true,
    });
    assert_eq!(
        resolve_window_points(&state, &native, "drag", (42, 7), [(1.0, 2.0), (3.0, 4.0)], None)
            .await
            .unwrap(),
        [(1.0, 2.0), (3.0, 4.0)]
    );
    // capture_id names one image point and no other frame.
    for (bad, detail) in [
        (json!({"capture_id": "c", "from_zoom": true}), "from_zoom"),
        (json!({"capture_id": "c", "coordinate_frame": "desktop"}), "desktop-frame"),
    ] {
        let refusal = resolve_window_points(&state, &bad, "click", (42, 7), [(1.0, 2.0)], None)
            .await
            .expect_err("invalid");
        assert!(format!("{:?}", refusal.content).contains(detail), "{detail}");
    }
    let two = resolve_window_points(
        &state,
        &json!({"capture_id": "c"}),
        "drag",
        (42, 7),
        [(1.0, 2.0); 2],
        None,
    )
    .await
    .expect_err("one point only");
    assert_eq!(
        two.structured_content.as_ref().unwrap()["code"],
        "invalid_arguments"
    );
}

/// click, drag and scroll compute the native Wayland refusal before they
/// resolve their frame (the resolver reports it before reading any frame).
#[cfg(test)]
#[test]
fn pointer_tools_compute_the_native_refusal_before_resolving_points() {
    let source = concat!(
        include_str!("click.rs"),
        include_str!("scroll.rs"),
        include_str!("drag.rs"),
    );
    for (start, tool) in [
        ("    async fn click(", "click"),
        ("impl Tool for DragTool {", "drag"),
        ("impl Tool for ScrollTool {", "scroll"),
    ] {
        let body = source
            .rsplit_once(start)
            .unwrap_or_else(|| panic!("{tool} source"))
            .1;
        let (before, _) = body
            .split_once("resolve_window_points(")
            .unwrap_or_else(|| panic!("{tool} resolves its points through the shared resolver"));
        assert!(
            before.contains("unavailable_wayland_focused_input_background(delivery, true)"),
            "{tool} must compute the native Wayland refusal before its frame"
        );
        assert!(
            !before.contains("screenshot_scale("),
            "{tool} must not consult the screenshot frame before the native refusal"
        );
    }
}

/// The compositor snapshot a desktop-scope action is addressed in. One
/// snapshot converts the action's points, places the overlay and sizes the
/// virtual pointer, so they cannot disagree about the monitor layout.
pub(super) async fn desktop_input_space() -> anyhow::Result<crate::wayland::DesktopInputSpace> {
    if !crate::wayland::wayland_input_enabled() {
        return Ok(crate::wayland::DesktopInputSpace::default());
    }
    tokio::task::spawn_blocking(crate::wayland::DesktopInputSpace::current)
        .await
        .map_err(|error| anyhow::anyhow!("task error: {error}"))?
}
