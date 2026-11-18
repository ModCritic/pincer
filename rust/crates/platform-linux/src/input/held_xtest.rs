//! The X connection a held XTest button lives on.
//!
//! The foreground route of the held-button tools presses a real button on
//! the core pointer through XTest (mouse_button_down), moves it
//! (mouse_drag) and releases it (mouse_button_up), each in a call of its
//! own. Xorg and Xvfb keep an XTest button down after the connection that
//! pressed it closes. Xwayland does not: it turns each X client's XTest
//! requests into libei events on ei devices of that client's own, and stops
//! emulating on them when the client disconnects (`xwl_ei_handle_client_gone`
//! in hw/xwayland/xwayland-xtest.c, Xwayland 24.1.14). KWin then releases
//! every button and key those devices hold, one second later (the release
//! timer of `EisDevice::setEmulating` in src/plugins/eis/eisdevice.cpp, KWin
//! 6.7.5). Seen in a headless KWin 6.7.5 Wayland session on 2026-11-18: a
//! press from an xdotool process that then exited was still down 0.7 s
//! later and released by 0.9 s, so the GTK3 app under it saw a click at the
//! press point; one xdotool process that pressed, waited 3 s, moved and
//! released drew a stroke.
//!
//! So a press opens a connection that stays open while any button this
//! process pressed is down. Motion during the hold and every release go
//! through it, and the release of the last held button closes it. The
//! releases on the other paths (a hold whose window is gone, the session
//! end, the undo of a press no call recorded, the exit after a broken Xlib
//! connection) go through [`release_button_xtest_at_pointer`] or
//! [`send_button_xtest_desktop`], so they use it too.

use anyhow::Result;
use std::sync::{Mutex, MutexGuard, PoisonError};
use x11rb::connection::Connection as _;
use x11rb::errors::{ConnectionError, ReplyError};
use x11rb::protocol::xproto::{
    ConnectionExt as _, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// The bit of `button` (1 to 7) in a held-button mask; 0 for any other.
fn bit(button: u8) -> u8 {
    if (1..=7).contains(&button) {
        1 << (button - 1)
    } else {
        0
    }
}

/// The buttons this process holds down through XTest, and the connection
/// they were pressed on.
struct Hold<C> {
    conn: Option<C>,
    /// Buttons 1 to 7, one bit each.
    down: u8,
}

impl<C> Hold<C> {
    /// Run `send` on the open connection, opened with `open` first when none
    /// is. A connection that broke is forgotten, so the next send opens a
    /// new one.
    fn send<T>(
        &mut self,
        open: impl FnOnce() -> Result<C>,
        send: impl FnOnce(&C) -> Result<T>,
    ) -> Result<T> {
        if self.conn.is_none() {
            self.conn = Some(open()?);
        }
        let result = send(self.conn.as_ref().expect("opened above"));
        self.forget_if_broken(&result);
        result
    }

    fn forget_if_broken<T>(&mut self, result: &Result<T>) {
        if result.as_ref().is_err_and(connection_broke) {
            self.conn = None;
        }
    }

    fn close_when_idle(&mut self) {
        if self.down == 0 {
            self.conn = None;
        }
    }
}

/// Whether `error` says the X connection itself failed, as opposed to an X
/// error reply on a connection that still works.
fn connection_broke(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<ConnectionError>().is_some()
            || matches!(
                cause.downcast_ref::<ReplyError>(),
                Some(ReplyError::ConnectionError(_))
            )
    })
}

/// Held XTest buttons over connections of type `C` (an X connection, or a
/// stand-in in the unit tests).
struct HeldButtons<C> {
    hold: Mutex<Hold<C>>,
}

impl<C> HeldButtons<C> {
    const fn new() -> Self {
        Self {
            hold: Mutex::new(Hold {
                conn: None,
                down: 0,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Hold<C>> {
        self.hold.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Press `button` with `press` on the hold's connection, opened with
    /// `open` when no button is held. It stays open until the last held
    /// button is released.
    fn press(
        &self,
        button: u8,
        open: impl FnOnce() -> Result<C>,
        press: impl FnOnce(&C) -> Result<()>,
    ) -> Result<()> {
        let mut hold = self.lock();
        let result = hold.send(open, press);
        if result.is_ok() {
            hold.down |= bit(button);
        }
        hold.close_when_idle();
        result
    }

    /// Run `send` on the hold's connection while a button is held; `None`
    /// when none is, or its connection broke (the caller then uses a
    /// connection of its own).
    fn while_held<T>(&self, send: impl FnOnce(&C) -> Result<T>) -> Option<Result<T>> {
        let mut hold = self.lock();
        if hold.down == 0 {
            return None;
        }
        let result = send(hold.conn.as_ref()?);
        hold.forget_if_broken(&result);
        Some(result)
    }

    /// Release `button` with `release` on the hold's connection (or on one
    /// opened with `open` when none is open), then close the connection
    /// once no button is held.
    fn release<T>(
        &self,
        button: u8,
        open: impl FnOnce() -> Result<C>,
        release: impl FnOnce(&C) -> Result<T>,
    ) -> Result<T> {
        let mut hold = self.lock();
        let result = hold.send(open, release);
        if result.is_ok() {
            hold.down &= !bit(button);
        }
        hold.close_when_idle();
        result
    }

    fn is_down(&self, button: u8) -> bool {
        bit(button) != 0 && self.lock().down & bit(button) != 0
    }

    fn down(&self) -> Vec<u8> {
        let down = self.lock().down;
        (1..=7u8).filter(|&button| down & bit(button) != 0).collect()
    }
}

/// An X connection for XTest, with its screen's root window.
struct XtestLink {
    conn: RustConnection,
    root: Window,
}

fn open_link() -> Result<XtestLink> {
    let (conn, screen_num) = super::connect_x11_for_input()?;
    let root = conn.setup().roots[screen_num].root;
    Ok(XtestLink { conn, root })
}

impl XtestLink {
    /// Wait until the server handled what was sent. Best effort: a press or
    /// release already checked has landed whatever this answers.
    fn settle(&self) {
        if let Ok(cookie) = self.conn.get_input_focus() {
            let _ = cookie.reply();
        }
    }
}

/// Buttons 1 to 7 that [`send_button_xtest_desktop`] pressed and nothing has
/// released yet. They are the core pointer's real buttons: Xorg keeps them
/// down after this process ends.
static HELD: HeldButtons<XtestLink> = HeldButtons::new();

/// One edge of a held button on the real X11 pointer: move to the
/// screen-absolute `(x, y)` when given, then press (`press`) or release
/// `button` there via XTest. The foreground route of the held-button tools
/// (mouse_button_down / mouse_drag / mouse_button_up); motion while the
/// button is down is `send_move_xtest_desktop`, which goes through the same
/// connection (see the module notes for why).
pub fn send_button_xtest_desktop(point: Option<(i32, i32)>, button: u8, press: bool) -> Result<()> {
    let edge = |link: &XtestLink| -> Result<()> {
        let (conn, root) = (&link.conn, link.root);
        let (x, y) = match point {
            Some((x, y)) => {
                conn.xtest_fake_input(MOTION_NOTIFY_EVENT, 0, 0, root, x as i16, y as i16, 0)?;
                (x, y)
            }
            None => {
                let pointer = conn.query_pointer(root)?.reply()?;
                (i32::from(pointer.root_x), i32::from(pointer.root_y))
            }
        };
        let kind = if press {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        conn.xtest_fake_input(kind, button, 0, root, x as i16, y as i16, 0)?
            .check()?;
        link.settle();
        Ok(())
    };
    if press {
        HELD.press(button, open_link, edge)
    } else {
        HELD.release(button, open_link, edge)
    }
}

/// Move the real pointer to the screen-absolute `(x, y)` on the held
/// buttons' connection while one is held; `None` when none is.
pub(super) fn move_while_held(x: i32, y: i32) -> Option<Result<()>> {
    HELD.while_held(|link| {
        link.conn
            .xtest_fake_input(MOTION_NOTIFY_EVENT, 0, 0, link.root, x as i16, y as i16, 0)?;
        link.conn.flush()?;
        link.settle();
        Ok(())
    })
}

/// Release XTest `button` where the pointer is when this process pressed it
/// and has not released it; nothing otherwise. The undo for a held press
/// that must not stay down (see `held_press`).
pub(crate) fn release_xtest_button_if_down(button: u8) {
    if HELD.is_down(button) {
        if let Err(error) = release_button_xtest_at_pointer(button) {
            tracing::warn!("could not release XTest button {button}: {error:#}");
        }
    }
}

/// Release, where the pointer is, every XTest button this process pressed
/// and has not released; the buttons released. For an exit that skips the
/// session cleanup (see `xdisplay`): the held buttons' connection is an
/// x11rb one, not the Xlib display that broke.
pub(crate) fn release_xtest_buttons_still_down() -> Vec<u8> {
    HELD.down()
        .into_iter()
        .filter(|&button| release_button_xtest_at_pointer(button).is_ok())
        .collect()
}

/// Release XTest `button` where the core pointer is now, without moving it,
/// and return that screen point. For a held button whose window is gone:
/// the release needs no window, and the button must not stay down.
pub fn release_button_xtest_at_pointer(button: u8) -> Result<(i32, i32)> {
    HELD.release(button, open_link, |link| {
        let (conn, root) = (&link.conn, link.root);
        let pointer = conn.query_pointer(root)?.reply()?;
        conn.xtest_fake_input(
            BUTTON_RELEASE_EVENT,
            button,
            0,
            root,
            pointer.root_x,
            pointer.root_y,
            0,
        )?
        .check()?;
        link.settle();
        Ok((i32::from(pointer.root_x), i32::from(pointer.root_y)))
    })
}

#[cfg(test)]
mod tests {
    // A held button's press, its motion and its release go through one
    // connection, which closes with the release of the last held button.

    use super::{HeldButtons, Result};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct Counts {
        opened: AtomicUsize,
        closed: AtomicUsize,
    }

    impl Counts {
        fn open(&self) -> usize {
            self.opened.load(Ordering::SeqCst) - self.closed.load(Ordering::SeqCst)
        }
    }

    /// A stand-in connection: its number, and the counts it updates.
    struct Fake {
        id: usize,
        counts: Arc<Counts>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.counts.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn opener(counts: &Arc<Counts>) -> impl Fn() -> Result<Fake> + '_ {
        move || {
            let id = counts.opened.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Fake {
                id,
                counts: counts.clone(),
            })
        }
    }

    fn broken() -> anyhow::Error {
        x11rb::errors::ConnectionError::UnknownError.into()
    }

    #[test]
    fn a_hold_keeps_one_connection_from_its_press_to_its_release() {
        let counts = Arc::new(Counts::default());
        let held = HeldButtons::<Fake>::new();
        assert!(
            held.while_held(|_| Ok(())).is_none(),
            "no hold, no connection"
        );

        held.press(1, opener(&counts), |_| Ok(())).unwrap();
        assert_eq!(counts.open(), 1, "the press leaves its connection open");
        assert!(held.is_down(1));
        for _ in 0..3 {
            let id = held.while_held(|conn| Ok(conn.id)).unwrap().unwrap();
            assert_eq!(id, 1, "motion goes through the press's connection");
        }
        let id = held
            .release(1, opener(&counts), |conn| Ok(conn.id))
            .unwrap();
        assert_eq!(id, 1, "the release goes through it too");
        assert_eq!(counts.open(), 0, "and closes it");
        assert!(!held.is_down(1));
        assert_eq!(counts.opened.load(Ordering::SeqCst), 1);
        assert!(held.while_held(|_| Ok(())).is_none());
    }

    #[test]
    fn the_connection_closes_with_the_last_held_button() {
        let counts = Arc::new(Counts::default());
        let held = HeldButtons::<Fake>::new();
        held.press(1, opener(&counts), |_| Ok(())).unwrap();
        held.press(3, opener(&counts), |_| Ok(())).unwrap();
        assert_eq!(counts.opened.load(Ordering::SeqCst), 1, "one connection");
        assert_eq!(held.down(), vec![1, 3]);
        held.release(1, opener(&counts), |_| Ok(())).unwrap();
        assert_eq!(counts.open(), 1, "button 3 is still held");
        held.release(3, opener(&counts), |_| Ok(())).unwrap();
        assert_eq!(counts.open(), 0);
        assert!(held.down().is_empty());
    }

    #[test]
    fn a_failed_press_with_nothing_held_leaves_no_connection() {
        let counts = Arc::new(Counts::default());
        let held = HeldButtons::<Fake>::new();
        let failed = held.press(1, opener(&counts), |_| anyhow::bail!("BadValue"));
        assert!(failed.is_err());
        assert_eq!(counts.open(), 0);
        assert!(!held.is_down(1), "a press that failed is not held");
    }

    #[test]
    fn a_broken_connection_is_replaced_and_its_button_still_released() {
        let counts = Arc::new(Counts::default());
        let held = HeldButtons::<Fake>::new();
        held.press(1, opener(&counts), |_| Ok(())).unwrap();
        let moved = held.while_held(|_| -> Result<()> { Err(broken()) });
        assert!(moved.unwrap().is_err());
        assert_eq!(counts.open(), 0, "the broken connection is dropped");
        assert!(held.is_down(1), "its button still counts as held");
        assert!(
            held.while_held(|_| Ok(())).is_none(),
            "motion falls back to a connection of its own"
        );
        let id = held
            .release(1, opener(&counts), |conn| Ok(conn.id))
            .unwrap();
        assert_eq!(id, 2, "the release opens a new connection");
        assert_eq!(counts.open(), 0, "and closes it");
        assert!(!held.is_down(1));
    }

    #[test]
    fn a_failed_release_keeps_the_button_and_its_connection() {
        let counts = Arc::new(Counts::default());
        let held = HeldButtons::<Fake>::new();
        held.press(1, opener(&counts), |_| Ok(())).unwrap();
        let failed = held.release(1, opener(&counts), |_| -> Result<()> {
            anyhow::bail!("BadValue")
        });
        assert!(failed.is_err());
        assert!(held.is_down(1), "a later release can still try");
        assert_eq!(counts.open(), 1);
        held.release(1, opener(&counts), |_| Ok(())).unwrap();
        assert_eq!(counts.open(), 0);
    }

    #[test]
    fn buttons_outside_one_to_seven_are_never_held() {
        let counts = Arc::new(Counts::default());
        let held = HeldButtons::<Fake>::new();
        held.press(9, opener(&counts), |_| Ok(())).unwrap();
        assert!(!held.is_down(9));
        assert_eq!(counts.open(), 0);
    }
}
