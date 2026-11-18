//! The Xlib display connections the input routes open per call.
//!
//! Every Xlib display of this process is opened by [`open_display`], which
//! returns an [`XDisplay`] that closes itself when dropped. Callers hold it
//! for the whole operation and pass [`XDisplay::as_ptr`] to the Xlib calls,
//! so an early return (a `?`, a `bail!`) closes the connection like the
//! normal exit does. Before this guard, those returns left the connection
//! open for the life of the process.

use anyhow::{bail, Result};
use std::os::raw::c_int;
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

/// How an [`XDisplay`] closes its connection: `XCloseDisplay`, or a stand-in
/// in the unit tests.
type CloseFn = unsafe extern "C" fn(*mut x11::xlib::Display) -> c_int;

static XLIB_THREADS_READY: OnceLock<Result<(), String>> = OnceLock::new();

/// An open Xlib display, closed exactly once when dropped.
///
/// Not `Send`: an Xlib display is used by the thread that opened it.
pub(super) struct XDisplay {
    ptr: NonNull<x11::xlib::Display>,
    close: CloseFn,
}

impl XDisplay {
    /// The raw display for Xlib calls. Valid while `self` is alive: keep the
    /// guard in a named binding for the whole operation, never
    /// `open_display()?.as_ptr()`, which closes the display at once.
    pub(super) fn as_ptr(&self) -> *mut x11::xlib::Display {
        self.ptr.as_ptr()
    }

    /// Take ownership of an open display that `close` closes.
    fn adopt(ptr: NonNull<x11::xlib::Display>, close: CloseFn) -> Self {
        Self { ptr, close }
    }
}

impl Drop for XDisplay {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from a successful open (or a test stand-in that
        // `close` accepts) and is closed only here, once.
        unsafe { (self.close)(self.ptr.as_ptr()) };
    }
}

/// Open the display named by `DISPLAY`. The first call runs `XInitThreads`,
/// which Xlib needs before any other call when more than one thread uses
/// it; this function is the only place this process opens an Xlib display.
pub(super) fn open_display() -> Result<XDisplay> {
    match XLIB_THREADS_READY.get_or_init(|| {
        let rc = unsafe { x11::xlib::XInitThreads() };
        if rc == 0 {
            Err("XInitThreads failed".to_owned())
        } else {
            Ok(())
        }
    }) {
        Ok(()) => {}
        Err(err) => bail!("{err}"),
    }
    let display = unsafe { x11::xlib::XOpenDisplay(ptr::null()) };
    let Some(display) = NonNull::new(display) else {
        bail!("XOpenDisplay returned null");
    };
    Ok(XDisplay::adopt(display, x11::xlib::XCloseDisplay))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CLOSED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn count_close(_display: *mut x11::xlib::Display) -> c_int {
        CLOSED.fetch_add(1, Ordering::SeqCst);
        0
    }

    fn early_return(display: XDisplay, fail: bool) -> Result<()> {
        let _ = display.as_ptr();
        if fail {
            bail!("refused early");
        }
        Ok(())
    }

    #[test]
    fn a_display_guard_closes_once_on_every_return_path() {
        let before = CLOSED.load(Ordering::SeqCst);
        let fake = || XDisplay::adopt(NonNull::dangling(), count_close);

        drop(fake());
        assert_eq!(CLOSED.load(Ordering::SeqCst), before + 1, "plain drop");

        assert!(early_return(fake(), true).is_err());
        assert_eq!(CLOSED.load(Ordering::SeqCst), before + 2, "early return");

        assert!(early_return(fake(), false).is_ok());
        assert_eq!(CLOSED.load(Ordering::SeqCst), before + 3, "normal return");
    }
}
