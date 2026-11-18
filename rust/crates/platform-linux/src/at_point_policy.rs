//! Pure routing facts for Linux background pixel clicks.
//!
//! Kept free of X11, AT-SPI, and `/proc` I/O so the decisions are unit-tested
//! on every host, not only on Linux.

/// AT-SPI roles of an application's top-level shell: the application node,
/// its frames and windows. A pixel hit test that resolves only to one of
/// these did not find the control under the point. Chromium on Linux is the
/// common case: until its accessibility tree is populated, the page content
/// is a bare `frame` whose `doDefault` does nothing. Firing that action and
/// reporting a click is a false success, so the shell is never an at-point
/// click target; the caller falls through to its pointer route or refuses.
pub(crate) fn is_top_level_shell_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "application" | "frame" | "window" | "desktop frame"
    )
}

/// Whether one `/proc/<pid>/cmdline` belongs to a Chromium helper process
/// (renderer, zygote, or GPU process).
///
/// The kernel separates arguments with NUL, but Chromium rewrites its own
/// process title with `setproctitle`, which joins the whole command line
/// with spaces into a single NUL-terminated string. Matching only
/// NUL-separated arguments misses every Google Chrome and Chromium helper,
/// so both separators are accepted.
pub(crate) fn cmdline_is_chromium_helper(raw: &[u8]) -> bool {
    String::from_utf8_lossy(raw)
        .split(|ch: char| ch == '\0' || ch.is_whitespace())
        .any(|arg| {
            matches!(
                arg,
                "--type=renderer" | "--type=zygote" | "--type=gpu-process"
            )
        })
}

/// What a toolkit's hit test answered for one point, once an answer that is
/// not the clicked element has been followed up its parent chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HitAnswer {
    /// An object with a role, or one whose parent chain reaches one below
    /// the window's frame.
    Control,
    /// Only an object with no role (AT-SPI `invalid`) and no such ancestor:
    /// the toolkit named nothing at the point.
    Unidentified,
    /// No answer at all.
    Nothing,
}

/// Whether a toolkit cannot hit-test a window: its answer for the press
/// point names nothing, and its answer for the window frame's own center
/// names nothing either. Then its hit test is no evidence for or against
/// the press, and the X11 stacking check stands in for it. A toolkit that
/// names a control at the center can hit-test, so an unnamed answer at the
/// press point still refuses the press.
pub(crate) fn toolkit_cannot_hit_test(point: HitAnswer, frame_center: HitAnswer) -> bool {
    point == HitAnswer::Unidentified && frame_center != HitAnswer::Control
}

/// What the X11 stacking puts at a press point meant for a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum X11Cover {
    /// The target window's own toplevel (its window-manager frame).
    Target,
    /// An override-redirect window (a menu, a tooltip, Pincer's cursor
    /// overlay) of the target's process, of Pincer, or of no known process.
    Popup,
    /// Another toplevel, or another process's popup.
    Covered { window: u32, pid: Option<u32> },
    /// No window at all at the point (the root).
    Outside,
    /// The target is not on this screen, or the server did not answer.
    Unknown,
}

impl X11Cover {
    /// Whether a real press at the point reaches the target window.
    pub(crate) fn reaches_target(self) -> bool {
        matches!(self, Self::Target | Self::Popup)
    }
}

/// Classify the topmost root child `under` a point (`0`: none) against the
/// target's toplevel `frame`.
pub(crate) fn classify_x11_cover(
    under: u32,
    frame: Option<u32>,
    under_is_override_redirect: bool,
    under_pid: Option<u32>,
    target_pid: u32,
    own_pid: u32,
) -> X11Cover {
    let Some(frame) = frame else {
        return X11Cover::Unknown;
    };
    if under == 0 {
        return X11Cover::Outside;
    }
    if under == frame {
        return X11Cover::Target;
    }
    if under_is_override_redirect && under_pid.is_none_or(|pid| pid == target_pid || pid == own_pid)
    {
        return X11Cover::Popup;
    }
    X11Cover::Covered {
        window: under,
        pid: under_pid,
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_x11_cover, toolkit_cannot_hit_test, HitAnswer, X11Cover};
    use super::{cmdline_is_chromium_helper, is_top_level_shell_role};

    #[test]
    fn only_a_toolkit_that_names_nothing_anywhere_hands_over_to_x11() {
        use HitAnswer::*;
        assert!(toolkit_cannot_hit_test(Unidentified, Unidentified));
        assert!(toolkit_cannot_hit_test(Unidentified, Nothing));
        // It names a control at the frame's center: it can hit-test, and its
        // unnamed answer at the press point keeps refusing.
        assert!(!toolkit_cannot_hit_test(Unidentified, Control));
        // A named answer or none at the press point is decided as before.
        assert!(!toolkit_cannot_hit_test(Control, Unidentified));
        assert!(!toolkit_cannot_hit_test(Nothing, Nothing));
    }

    #[test]
    fn x11_cover_passes_the_target_and_its_popups_only() {
        let (target, own) = (500, 900);
        assert_eq!(
            classify_x11_cover(7, Some(7), false, None, target, own),
            X11Cover::Target
        );
        // Its own menu, Pincer's overlay, or a popup without a pid.
        for pid in [Some(target), Some(own), None] {
            assert_eq!(
                classify_x11_cover(8, Some(7), true, pid, target, own),
                X11Cover::Popup
            );
        }
        // Another app's window or popup over the point.
        assert_eq!(
            classify_x11_cover(9, Some(7), false, Some(33), target, own),
            X11Cover::Covered {
                window: 9,
                pid: Some(33)
            }
        );
        assert_eq!(
            classify_x11_cover(9, Some(7), true, Some(33), target, own),
            X11Cover::Covered {
                window: 9,
                pid: Some(33)
            }
        );
        // The target's own other toplevel (a dialog) is not the target either.
        assert_eq!(
            classify_x11_cover(10, Some(7), false, Some(target), target, own),
            X11Cover::Covered {
                window: 10,
                pid: Some(target)
            }
        );
        assert_eq!(
            classify_x11_cover(0, Some(7), false, None, target, own),
            X11Cover::Outside
        );
        assert_eq!(
            classify_x11_cover(7, None, false, None, target, own),
            X11Cover::Unknown
        );
        assert!(X11Cover::Target.reaches_target() && X11Cover::Popup.reaches_target());
        assert!(!X11Cover::Outside.reaches_target() && !X11Cover::Unknown.reaches_target());
    }

    #[test]
    fn application_frames_and_windows_are_never_click_targets() {
        for role in ["frame", "window", "application", "desktop frame", " Frame "] {
            assert!(is_top_level_shell_role(role), "{role:?}");
        }
    }

    #[test]
    fn controls_and_documents_remain_click_targets() {
        for role in [
            "push button",
            "button",
            "link",
            "check box",
            "menu item",
            "document web",
            "internal frame",
            "dialog",
            "panel",
        ] {
            assert!(!is_top_level_shell_role(role), "{role:?}");
        }
    }

    #[test]
    fn nul_separated_chromium_helper_is_detected() {
        assert!(cmdline_is_chromium_helper(
            b"/usr/lib/chromium/chromium\0--type=renderer\0--lang=en-US\0"
        ));
        assert!(cmdline_is_chromium_helper(
            b"/opt/app/app\0--type=zygote\0--no-zygote-sandbox\0"
        ));
    }

    #[test]
    fn setproctitle_rewritten_chrome_helper_is_detected() {
        // Google Chrome 151 renderer as `/proc/<pid>/cmdline` shows it after
        // Chromium's setproctitle: one space-joined string.
        assert!(cmdline_is_chromium_helper(
            b"/opt/google/chrome/chrome --type=renderer --crashpad-handler-pid=7960 \
              --enable-crash-reporter --lang=en-US --num-raster-threads=4\0"
        ));
        assert!(cmdline_is_chromium_helper(
            b"/opt/google/chrome/chrome --type=gpu-process --ozone-platform=x11\0\0\0"
        ));
    }

    #[test]
    fn unrelated_processes_are_not_chromium_helpers() {
        assert!(!cmdline_is_chromium_helper(
            b"/usr/bin/gedit\0--new-window\0"
        ));
        assert!(!cmdline_is_chromium_helper(
            b"/usr/bin/python3 tool.py --type=renderer-like\0"
        ));
        assert!(!cmdline_is_chromium_helper(
            b"/usr/lib/webkit2gtk-4.1/WebKitWebProcess 7 12\0"
        ));
        assert!(!cmdline_is_chromium_helper(b""));
    }
}
