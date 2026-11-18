//! Where an X11 client-side-decorated window roots its AT-SPI
//! `CoordType::Window` coordinates.
//!
//! A window that draws its own decorations and shadow sets
//! `_GTK_FRAME_EXTENTS`: its X11 client window includes an invisible shadow
//! margin of `(left, right, top, bottom)` px around the visible frame. The
//! toolkits disagree on where `Window` coordinate (0, 0) lies. Measured on
//! KWin X11 with compositing, 2026-11-17:
//!
//! | toolkit             | extents (l,r,t,b) | frame accessible `Window` extents | (0, 0) is |
//! |---------------------|-------------------|-----------------------------------|-----------|
//! | GTK 3.24 header bar | 26,26,23,29       | 0,0 and the whole X window        | X window  |
//! | GTK 4.24 header bar | 14,14,12,17       | 0,0 and the visible frame         | visible frame |
//! | Firefox 159         | 26,26,23,29       | 26,23 and the visible frame       | X window  |
//! | Chromium 156        | 16,16,10,32       | 0,0 and the whole X window        | X window  |
//! | Electron app        | 16,16,10,32       | 0,0 and the whole X window        | X window  |
//!
//! So instead of a per-toolkit list, each window is measured: the frame
//! accessible's `Window` extents are compared with the X window's geometry.
//! Its size says which box it is (the whole X window or the visible frame,
//! whose X-window-local corner is known), and its reported position then
//! says where (0, 0) is. The answer is cached per window, so a lookup that
//! has no frame at hand (a cached element's bounds) uses what the last
//! snapshot measured. When the comparison is unclear the caller keeps the
//! earlier assumption: (0, 0) at the visible frame.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

/// Pixel slack for rounding (fractional scaling) when comparing sizes and
/// positions.
const TOLERANCE_PX: i32 = 2;
/// Windows remembered at most; the cache is cleared when it is full.
const CACHE_CAP: usize = 256;

/// `_GTK_FRAME_EXTENTS`: the invisible shadow margin around the visible frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameExtents {
    pub left: i32,
    pub right: i32,
    pub top: i32,
    pub bottom: i32,
}

impl FrameExtents {
    /// Parse the property's `CARDINAL[4]` (`[left, right, top, bottom]`).
    /// `None` when absent, short, or out of range.
    pub(crate) fn parse(vals: &[u32]) -> Option<Self> {
        let [left, right, top, bottom] = vals.get(..4)? else {
            return None;
        };
        Some(Self {
            left: i32::try_from(*left).ok()?,
            right: i32::try_from(*right).ok()?,
            top: i32::try_from(*top).ok()?,
            bottom: i32::try_from(*bottom).ok()?,
        })
    }
}

/// Where a toolkit puts `Window` coordinate (0, 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowCoordRoot {
    /// The X11 client window's corner, shadow included (GTK3, Firefox,
    /// Chromium, Electron).
    XWindow,
    /// The visible frame's corner, `(left, top)` px inside the X window
    /// (GTK4).
    VisibleFrame,
}

/// An X11 client window that carries `_GTK_FRAME_EXTENTS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CsdWindow {
    pub xid: u64,
    /// Root-relative origin of the X11 client window (shadow included).
    pub origin: (i32, i32),
    /// Size of the X11 client window (shadow included).
    pub size: (i32, i32),
    pub extents: FrameExtents,
}

impl CsdWindow {
    /// Read window `xid` from the X server. `None` when it has no
    /// `_GTK_FRAME_EXTENTS` (a server-decorated or non-GTK-protocol window)
    /// or cannot be read.
    pub(crate) fn read(xid: u64) -> Option<Self> {
        use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

        let window = u32::try_from(xid).ok()?;
        let (conn, _) = crate::x11::query_connection().ok()?;
        // only_if_exists: the atom is 0 when no client ever set the property.
        let atom = conn
            .intern_atom(true, b"_GTK_FRAME_EXTENTS")
            .ok()?
            .reply()
            .ok()?
            .atom;
        if atom == 0 {
            return None;
        }
        let reply = conn
            .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 4)
            .ok()?
            .reply()
            .ok()?;
        let vals: Vec<u32> = reply.value32()?.collect();
        let extents = FrameExtents::parse(&vals)?;
        let geom = conn.get_geometry(window).ok()?.reply().ok()?;
        let trans = conn
            .translate_coordinates(window, geom.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some(Self {
            xid,
            origin: (i32::from(trans.dst_x), i32::from(trans.dst_y)),
            size: (i32::from(geom.width), i32::from(geom.height)),
            extents,
        })
    }

    /// The screen offset that assumes (0, 0) at the visible frame: what
    /// Pincer used for every such window before it measured them.
    pub(crate) fn legacy_offset(&self) -> (i32, i32) {
        self.offset_for(WindowCoordRoot::VisibleFrame)
    }

    /// Screen point of `Window` coordinate (0, 0) under `root`.
    pub(crate) fn offset_for(&self, root: WindowCoordRoot) -> (i32, i32) {
        let (dx, dy) = self.root_in_window(root);
        (self.origin.0 + dx, self.origin.1 + dy)
    }

    /// X-window-local position of `Window` coordinate (0, 0) under `root`.
    fn root_in_window(&self, root: WindowCoordRoot) -> (i32, i32) {
        match root {
            WindowCoordRoot::XWindow => (0, 0),
            WindowCoordRoot::VisibleFrame => (self.extents.left, self.extents.top),
        }
    }

    /// Which root the toolkit uses, from the frame accessible's own
    /// `Window` extents. `None` when the comparison is unclear: no inset to
    /// tell the roots apart, a frame size that matches neither box (or
    /// both), or a position that fits neither root.
    pub(crate) fn measure(&self, frame_window: (i32, i32, i32, i32)) -> Option<WindowCoordRoot> {
        let near = |a: i32, b: i32| (a - b).abs() <= TOLERANCE_PX;
        let (fx, fy, fw, fh) = frame_window;
        if fw <= 0 || fh <= 0 {
            return None;
        }
        let FrameExtents {
            left,
            right,
            top,
            bottom,
        } = self.extents;
        // Without an inset both roots are the same point (a maximized
        // window): nothing to learn, and nothing to get wrong.
        if near(left, 0) && near(top, 0) {
            return None;
        }
        let (w, h) = self.size;
        let whole = near(fw, w) && near(fh, h);
        let visible = near(fw, w - left - right) && near(fh, h - top - bottom);
        // The frame's X-window-local corner, known from which box it is.
        let frame_corner = match (whole, visible) {
            (true, false) => (0, 0),
            (false, true) => (left, top),
            _ => return None,
        };
        // Its reported position is relative to (0, 0), so (0, 0) lies here.
        let root = (frame_corner.0 - fx, frame_corner.1 - fy);
        if near(root.0, 0) && near(root.1, 0) {
            Some(WindowCoordRoot::XWindow)
        } else if near(root.0, left) && near(root.1, top) {
            Some(WindowCoordRoot::VisibleFrame)
        } else {
            None
        }
    }
}

fn roots() -> &'static Mutex<HashMap<(u32, u64), WindowCoordRoot>> {
    static ROOTS: OnceLock<Mutex<HashMap<(u32, u64), WindowCoordRoot>>> = OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember(pid: u32, xid: u64, root: WindowCoordRoot) {
    let mut roots = roots().lock().unwrap_or_else(PoisonError::into_inner);
    if roots.len() >= CACHE_CAP && !roots.contains_key(&(pid, xid)) {
        roots.clear();
    }
    roots.insert((pid, xid), root);
}

/// The root the last measurement of window `xid` (owned by `pid`) found.
pub(crate) fn cached_root(pid: u32, xid: u64) -> Option<WindowCoordRoot> {
    roots()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&(pid, xid))
        .copied()
}

/// Screen offset to add to `Window` extents of `window`'s elements. Measures
/// with the frame accessible's `Window` extents when given (and caches the
/// answer), else uses the cached answer, else the earlier assumption.
pub(crate) fn csd_window_offset(
    pid: u32,
    window: &CsdWindow,
    frame_window: Option<(i32, i32, i32, i32)>,
) -> (i32, i32) {
    let measured = frame_window.and_then(|frame| window.measure(frame));
    if let Some(root) = measured {
        remember(pid, window.xid, root);
    }
    resolve_offset(
        window,
        measured.or_else(|| cached_root(pid, window.xid)),
        frame_window,
    )
}

/// [`csd_window_offset`] once the root is known (or not).
fn resolve_offset(
    window: &CsdWindow,
    root: Option<WindowCoordRoot>,
    frame_window: Option<(i32, i32, i32, i32)>,
) -> (i32, i32) {
    match root {
        Some(root) => window.offset_for(root),
        // Unclear: the visible-frame assumption, rebased by a negative
        // renderer frame origin as before.
        None => super::rebase_renderer_window_offset(
            window.legacy_offset(),
            frame_window.map(|(x, y, _, _)| (x, y)),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(origin: (i32, i32), size: (i32, i32), extents: [i32; 4]) -> CsdWindow {
        let [left, right, top, bottom] = extents;
        CsdWindow {
            xid: 0x100_0016,
            origin,
            size,
            extents: FrameExtents {
                left,
                right,
                top,
                bottom,
            },
        }
    }

    // Real geometry from the KWin X11 sandbox, 2026-11-17.
    fn firefox() -> CsdWindow {
        window((614, 177), (1332, 1092), [26, 26, 23, 29])
    }
    fn chromium() -> CsdWindow {
        window((10, 10), (1265, 1420), [16, 16, 10, 32])
    }
    fn gtk3() -> CsdWindow {
        window((954, 473), (652, 499), [26, 26, 23, 29])
    }
    fn gtk4() -> CsdWindow {
        window((966, 508), (628, 429), [14, 14, 12, 17])
    }

    #[test]
    fn firefox_frame_at_its_inset_means_x_window_root() {
        let frame = (26, 23, 1280, 1040);
        assert_eq!(firefox().measure(frame), Some(WindowCoordRoot::XWindow));
        // The probe field: Window (126, 188), drawn at (126, 188) in the
        // window's own pixels, screen (740, 365).
        let (ox, oy) = resolve_offset(&firefox(), firefox().measure(frame), Some(frame));
        assert_eq!((126 + ox, 188 + oy), (740, 365));
    }

    #[test]
    fn chromium_and_gtk3_frames_cover_the_whole_x_window() {
        let frame = (0, 0, 1265, 1420);
        assert_eq!(chromium().measure(frame), Some(WindowCoordRoot::XWindow));
        let (ox, oy) = resolve_offset(&chromium(), chromium().measure(frame), Some(frame));
        // Probe field: Window (116, 177), screen (126, 187).
        assert_eq!((116 + ox, 177 + oy), (126, 187));
        let frame = (0, 0, 652, 499);
        assert_eq!(gtk3().measure(frame), Some(WindowCoordRoot::XWindow));
        assert_eq!(
            resolve_offset(&gtk3(), gtk3().measure(frame), Some(frame)),
            (954, 473)
        );
    }

    #[test]
    fn gtk4_frame_at_zero_with_visible_size_means_visible_frame_root() {
        let frame = (0, 0, 600, 400);
        assert_eq!(gtk4().measure(frame), Some(WindowCoordRoot::VisibleFrame));
        // Probe field: Window (100, 127), drawn at (114, 139) in the
        // window's pixels.
        let (ox, oy) = resolve_offset(&gtk4(), gtk4().measure(frame), Some(frame));
        assert_eq!((100 + ox - 966, 127 + oy - 508), (114, 139));
    }

    #[test]
    fn unclear_comparisons_keep_the_visible_frame_assumption() {
        let ff = firefox();
        // A size that matches neither box.
        assert_eq!(ff.measure((26, 23, 900, 700)), None);
        // A position that fits neither root.
        assert_eq!(ff.measure((300, 23, 1280, 1040)), None);
        // Sentinel and empty extents.
        assert_eq!(ff.measure((-1, -1, -1, -1)), None);
        assert_eq!(ff.measure((0, 0, 0, 0)), None);
        // Nothing measured, nothing cached: the old offset, origin + inset.
        assert_eq!(resolve_offset(&ff, None, None), (640, 200));
        // ...still rebased by a negative renderer frame origin.
        assert_eq!(
            resolve_offset(&ff, None, Some((-8, -29, 900, 700))),
            (648, 229)
        );
    }

    #[test]
    fn no_inset_is_not_a_measurement() {
        // A maximized window keeps the property at zero: both roots agree.
        let max = window((0, 0), (2560, 1440), [0, 0, 0, 0]);
        assert_eq!(max.measure((0, 0, 2560, 1440)), None);
        assert_eq!(resolve_offset(&max, None, Some((0, 0, 2560, 1440))), (0, 0));
    }

    #[test]
    fn a_frame_matching_both_boxes_is_unclear() {
        // Extents within the tolerance on the sizes but not on the corner.
        let w = window((50, 60), (400, 300), [3, 0, 3, 0]);
        assert_eq!(w.measure((0, 0, 398, 298)), None);
    }

    #[test]
    fn a_cached_root_serves_lookups_without_a_frame() {
        let ff = window((614, 177), (1332, 1092), [26, 26, 23, 29]);
        let pid = 4_000_001;
        assert_eq!(csd_window_offset(pid, &ff, None), (640, 200));
        assert_eq!(
            csd_window_offset(pid, &ff, Some((26, 23, 1280, 1040))),
            (614, 177)
        );
        assert_eq!(cached_root(pid, ff.xid), Some(WindowCoordRoot::XWindow));
        // Same window, no frame at hand: the cached measurement applies.
        assert_eq!(csd_window_offset(pid, &ff, None), (614, 177));
        // An unclear frame read does not erase it.
        assert_eq!(
            csd_window_offset(pid, &ff, Some((26, 23, 10, 10))),
            (614, 177)
        );
        // Another process's window with the same id knows nothing.
        assert_eq!(cached_root(pid + 1, ff.xid), None);
    }

    #[test]
    fn extents_parse_left_right_top_bottom_in_order() {
        assert_eq!(
            FrameExtents::parse(&[10, 20, 30, 40]),
            Some(FrameExtents {
                left: 10,
                right: 20,
                top: 30,
                bottom: 40
            })
        );
        assert_eq!(
            FrameExtents::parse(&[0, 0, 0, 0]).map(|e| (e.left, e.top)),
            Some((0, 0))
        );
        // Asymmetric values prove left and top come from [0] and [2].
        assert_eq!(
            FrameExtents::parse(&[61, 61, 55, 67]).map(|e| (e.left, e.top)),
            Some((61, 55))
        );
        assert_eq!(FrameExtents::parse(&[]), None);
        assert_eq!(FrameExtents::parse(&[61, 61]), None);
        assert_eq!(FrameExtents::parse(&[61, 61, 55]), None);
        assert_eq!(FrameExtents::parse(&[u32::MAX, 0, 0, 0]), None);
    }
}
