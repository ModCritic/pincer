//! Which window a pid-only tool call means: the resolvers behind the registry's
//! window-target guard.

use super::*;

pub(super) fn window_target_candidates_for_pid(
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

pub(super) fn pid_window_target_candidates(pid: i64) -> Vec<WindowTargetCandidate> {
    let Ok(pid) = u32::try_from(pid) else {
        return Vec::new();
    };
    window_target_candidates_for_pid(crate::wayland::list_windows_dispatch(Some(pid)), pid)
}

/// Topmost on-screen window of `pid` covering a desktop-frame point.
pub(super) fn desktop_point_window_resolver() -> pincer_core::window_target::DesktopPointWindowResolver {
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
pub(super) fn topmost_window_at(
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
pub(super) fn pid_fallback_window_resolver() -> pincer_core::window_target::PidFallbackWindowResolver {
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

pub(super) type PidWindowGuardParts = (
    WindowTargetCandidates,
    pincer_core::window_target::DesktopPointWindowResolver,
    pincer_core::window_target::PidFallbackWindowResolver,
    pincer_core::window_target::SnapshotWindowResolver,
    pincer_core::window_target::TokenPidResolver,
);

pub(super) fn pid_window_guarded<T: Tool + 'static>(
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
pub(super) fn token_pid_resolver(state: Arc<ToolState>) -> pincer_core::window_target::TokenPidResolver {
    Arc::new(move |args| state.snapshots.pid_for_token(args))
}

/// The window a `snapshot_id` was published for (the element cache lane of
/// that pid), so pid-only element actions follow the snapshot to its popup
/// or dialog.
pub(super) fn snapshot_window_resolver(
    state: Arc<ToolState>,
) -> pincer_core::window_target::SnapshotWindowResolver {
    Arc::new(move |pid, handle| {
        let pid = i32::try_from(pid).ok()?;
        let id = pincer_core::element_token::parse_snapshot_handle(handle)?;
        state.snapshots.window_for_snapshot(pid, id)
    })
}
