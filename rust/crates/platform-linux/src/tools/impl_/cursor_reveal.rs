//! The agent cursor overlay: revealing pointer and keyboard targets, gliding
//! along pointer paths, and the cursor hook an embedder listens to.

use super::*;

pub(super) fn resolve_cursor_key(args: &Value) -> String {
    pincer_core::tool_args::resolve_session_key(args)
}

/// Return the cursor key only for a lifecycle-owned session. Cursor positioning
/// for keyboard actions deliberately does not opt anonymous calls or the
/// legacy cursor_id-only path into session semantics. The proxy-minted
/// `_session_id` is trusted lifecycle state and must behave like a public named
/// session for cursor ownership.
pub(super) fn named_session_cursor_key(args: &Value) -> Option<String> {
    cursor_overlay::named_session_cursor_key(args)
}

/// Put the agent cursor's tip on the pointer point `(sx, sy)` at once.
/// `SnapTo` places the cursor's anchor, so the point is converted for the
/// heading (the resting 45° pose when `None`).
pub(super) fn overlay_snap_to_for(cursor_id: &str, sx: f64, sy: f64, heading: Option<f64>) {
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

pub(super) fn overlay_move_to_for(cursor_id: &str, sx: f64, sy: f64, heading: Option<f64>) {
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

pub(super) async fn overlay_glide_to_for(cursor_id: &str, sx: f64, sy: f64) {
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

/// Keep the logical cursor position in sync with every visibly targeted
/// pointer action. Overlay delivery is intentionally best-effort: registry
/// state is still updated when no renderer is running or its queue is closed.
pub(super) async fn reveal_pointer_action_for(
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
pub(super) async fn reveal_pointer_action_target_for(
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

pub(super) fn keyboard_window_center(xid: u64) -> Option<(f64, f64)> {
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

pub(super) fn current_pointer_position() -> Option<(f64, f64)> {
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

pub(super) fn explicit_keyboard_cursor_target(
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
pub(super) async fn announce_keyboard_target(
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
pub(super) async fn position_named_session_keyboard_cursor(
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

pub(super) async fn track_overlay_drag_for(
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
pub(super) fn eased_jump_px(screen_width: Option<u32>) -> f64 {
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
pub(super) async fn follow_overlay_path_with(
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
