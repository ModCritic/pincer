//! The Xlib display connections the input routes open per call.
//!
//! Every Xlib display of this process is opened by [`open_display`], which
//! returns an [`XDisplay`] that closes itself when dropped. Callers hold it
//! for the whole operation and pass [`XDisplay::as_ptr`] to the Xlib calls,
//! so an early return (a `?`, a `bail!`) closes the connection like the
//! normal exit does. Before this guard, those returns left the connection
//! open for the life of the process.
//!
//! X protocol errors go to one error handler, installed once with
//! `XInitThreads` and never replaced. libX11's default handler prints the
//! error and exits the process, and a window can disappear between any two
//! requests, so a BadWindow is ordinary here. The handler records the error
//! against its display instead: [`XDisplay::take_error`] tells a caller that
//! needs to know. `XSetErrorHandler` sets one handler for the whole process,
//! so the earlier pattern of swapping in an ignoring handler around single
//! calls raced between threads and left calls outside the swaps on the
//! exiting default.
//!
//! A broken connection (the server closed it or went away) still ends the
//! process, cleanly: [`on_x_io_error`] logs why and releases the XTest
//! buttons Pincer still holds down, then libX11 exits. Failing only the call
//! that saw it is not sound with libX11 1.8 (checked in the 1.8.14 binary:
//! `_XIOError` unlocks the display's mutex before it calls the handlers and
//! does not lock it again, so returning into Xlib would unlock that mutex a
//! second time, which POSIX leaves undefined), and Rust has no `longjmp` out
//! of the handler. Surviving it means moving these routes from Xlib to
//! x11rb, whose errors are plain return values.

use anyhow::{bail, Result};
use std::os::raw::c_int;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

/// How an [`XDisplay`] closes its connection: `XCloseDisplay`, or a stand-in
/// in the unit tests.
type CloseFn = unsafe extern "C" fn(*mut x11::xlib::Display) -> c_int;

static XLIB_READY: OnceLock<Result<(), String>> = OnceLock::new();

/// One X protocol error that Xlib reported on a display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct XErrorRecord {
    /// `BadWindow` is 3, `BadMatch` 8, `BadAccess` 10.
    pub error_code: u8,
    /// Major opcode of the failed request (`X_MapWindow` is 8, an
    /// extension's is its major opcode).
    pub request_code: u8,
    pub minor_code: u8,
    pub resource_id: x11::xlib::XID,
    pub serial: std::os::raw::c_ulong,
}

impl std::fmt::Display for XErrorRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "X error {} on request {}.{} (resource 0x{:x}, serial {})",
            self.error_code, self.request_code, self.minor_code, self.resource_id, self.serial
        )
    }
}

/// The errors of one open display since the last [`XDisplay::take_error`].
#[derive(Clone, Copy, Debug, Default)]
struct DisplayErrors {
    first: Option<XErrorRecord>,
    count: u64,
}

/// Open displays by address, with their errors. A `Vec`: there are a few at
/// a time, and the handler finds its entry without allocating.
static DISPLAY_ERRORS: Mutex<Vec<(usize, DisplayErrors)>> = Mutex::new(Vec::new());

/// Errors on displays this module did not open (or had already closed).
static UNTRACKED_ERRORS: AtomicU64 = AtomicU64::new(0);

fn display_errors() -> std::sync::MutexGuard<'static, Vec<(usize, DisplayErrors)>> {
    DISPLAY_ERRORS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The process's one Xlib error handler: record the error, never exit.
///
/// Called by Xlib on the thread that read the error, while that thread is
/// inside an Xlib call on `display`. It makes no Xlib call and takes only
/// the [`DISPLAY_ERRORS`] lock, which is never held across an Xlib call.
unsafe extern "C" fn record_x_error(
    display: *mut x11::xlib::Display,
    event: *mut x11::xlib::XErrorEvent,
) -> c_int {
    // SAFETY: Xlib passes the error it is reporting; checked for null anyway.
    let Some(event) = (unsafe { event.as_ref() }) else {
        return 0;
    };
    let record = XErrorRecord {
        error_code: event.error_code,
        request_code: event.request_code,
        minor_code: event.minor_code,
        resource_id: event.resourceid,
        serial: event.serial,
    };
    let tracked = {
        let mut errors = display_errors();
        match errors.iter_mut().find(|(key, _)| *key == display as usize) {
            Some((_, entry)) => {
                entry.count += 1;
                entry.first.get_or_insert(record);
                true
            }
            None => false,
        }
    };
    if !tracked {
        UNTRACKED_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
    tracing::debug!(%record, "Xlib reported an X error; recorded, not fatal");
    0
}

/// The process's Xlib I/O error handler, for a connection that broke. libX11
/// calls it, then the display's exit handler, whose default exits with
/// status 1. Log why, and release the core pointer buttons Pincer pressed
/// through XTest: the server keeps them down after the process is gone, and
/// the session cleanup that would release them never runs. The release goes
/// through the held buttons' x11rb connection (see `held_xtest`), not the
/// Xlib display that broke, and gets at most a second, so a server that went
/// away or stopped answering cannot hold up the exit.
unsafe extern "C" fn on_x_io_error(display: *mut x11::xlib::Display) -> c_int {
    // SAFETY: a field read on the display Xlib passes; no protocol request.
    let name = unsafe { x11::xlib::XDisplayString(display) };
    let name = if name.is_null() {
        String::new()
    } else {
        // SAFETY: XDisplayString returns the display's NUL-terminated name.
        unsafe { std::ffi::CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned()
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("pincer-xio-release".into())
        .spawn(move || {
            let _ = tx.send(super::release_xtest_buttons_still_down());
        });
    let released = match rx.recv_timeout(std::time::Duration::from_secs(1)) {
        Ok(buttons) if buttons.is_empty() => "no held button to release".to_owned(),
        Ok(buttons) => buttons
            .iter()
            .map(|button| format!("released held button {button}"))
            .collect::<Vec<_>>()
            .join(", "),
        Err(_) => "could not release held buttons within a second".to_owned(),
    };
    tracing::error!(
        "the connection to X display {name:?} broke (the server closed it or went away); \
         libX11 cannot go on after that, so Pincer exits now ({released})"
    );
    0
}

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

    /// Wait until the server has handled every request sent so far
    /// (`XSync`), then return the first X error reported on this display
    /// since the last call, and how many there were. Clears them.
    pub(super) fn take_error(&self) -> Option<(XErrorRecord, u64)> {
        // SAFETY: the display is open while `self` lives.
        unsafe { x11::xlib::XSync(self.as_ptr(), 0) };
        self.take_recorded()
    }

    /// [`Self::take_error`] without the `XSync`.
    fn take_recorded(&self) -> Option<(XErrorRecord, u64)> {
        let mut errors = display_errors();
        let (_, entry) = errors.iter_mut().find(|(key, _)| *key == self.key())?;
        let taken = std::mem::take(entry);
        taken.first.map(|first| (first, taken.count))
    }

    fn key(&self) -> usize {
        self.ptr.as_ptr() as usize
    }

    /// Take ownership of an open display that `close` closes, and start
    /// recording its errors.
    fn adopt(ptr: NonNull<x11::xlib::Display>, close: CloseFn) -> Self {
        let display = Self { ptr, close };
        display_errors().push((display.key(), DisplayErrors::default()));
        display
    }
}

impl Drop for XDisplay {
    fn drop(&mut self) {
        // Forget the entry before the close: once closed, the address can
        // belong to the next display opened on another thread. An error the
        // close itself flushes out counts as untracked.
        let key = self.key();
        display_errors().retain(|(open, _)| *open != key);
        // SAFETY: `ptr` came from a successful open (or a test stand-in that
        // `close` accepts) and is closed only here, once.
        unsafe { (self.close)(self.ptr.as_ptr()) };
    }
}

/// Open the display named by `DISPLAY`. The first call runs `XInitThreads`,
/// which Xlib needs before any other call when more than one thread uses
/// it, and installs [`record_x_error`] as the process's X error handler;
/// this function is the only place this process opens an Xlib display.
pub(super) fn open_display() -> Result<XDisplay> {
    match XLIB_READY.get_or_init(|| {
        let rc = unsafe { x11::xlib::XInitThreads() };
        if rc == 0 {
            return Err("XInitThreads failed".to_owned());
        }
        // SAFETY: installs handlers with the signatures Xlib expects; this
        // crate is the only Xlib user in the process.
        unsafe {
            x11::xlib::XSetErrorHandler(Some(record_x_error));
            x11::xlib::XSetIOErrorHandler(Some(on_x_io_error));
        }
        Ok(())
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
    use std::sync::atomic::AtomicUsize;

    static CLOSED: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn count_close(_display: *mut x11::xlib::Display) -> c_int {
        CLOSED.fetch_add(1, Ordering::SeqCst);
        0
    }

    /// A stand-in display at a unique fake address (never dereferenced).
    fn fake(address: usize) -> XDisplay {
        XDisplay::adopt(
            NonNull::new(address as *mut x11::xlib::Display).unwrap(),
            count_close,
        )
    }

    fn early_return(display: XDisplay, fail: bool) -> Result<()> {
        let _ = display.as_ptr();
        if fail {
            bail!("refused early");
        }
        Ok(())
    }

    fn tracked(address: usize) -> bool {
        display_errors().iter().any(|(key, _)| *key == address)
    }

    #[test]
    fn a_display_guard_closes_once_on_every_return_path() {
        let before = CLOSED.load(Ordering::SeqCst);

        drop(fake(0x1000));
        assert_eq!(CLOSED.load(Ordering::SeqCst), before + 1, "plain drop");
        assert!(!tracked(0x1000), "a closed display keeps no error entry");

        assert!(early_return(fake(0x1008), true).is_err());
        assert_eq!(CLOSED.load(Ordering::SeqCst), before + 2, "early return");

        assert!(early_return(fake(0x1010), false).is_ok());
        assert_eq!(CLOSED.load(Ordering::SeqCst), before + 3, "normal return");
    }

    fn bad_window(serial: std::os::raw::c_ulong) -> x11::xlib::XErrorEvent {
        x11::xlib::XErrorEvent {
            type_: 0,
            display: ptr::null_mut(),
            resourceid: 0x20_0001,
            serial,
            error_code: 3,
            request_code: 8,
            minor_code: 0,
        }
    }

    #[test]
    fn the_error_handler_records_errors_per_display_and_returns() {
        let display = fake(0x2000);
        let other = fake(0x2008);
        assert_eq!(display.take_recorded(), None);

        for serial in [10, 11] {
            let mut event = bad_window(serial);
            let handled = unsafe { record_x_error(display.as_ptr(), &mut event) };
            assert_eq!(handled, 0);
        }
        let (first, count) = display.take_recorded().expect("recorded error");
        assert_eq!((first.error_code, first.request_code), (3, 8));
        assert_eq!(
            (first.serial, count),
            (10, 2),
            "the first error, and how many"
        );
        assert_eq!(display.take_recorded(), None, "taking clears them");
        assert_eq!(other.take_recorded(), None, "another display saw nothing");

        // A display this module never opened: counted, not recorded.
        let untracked = UNTRACKED_ERRORS.load(Ordering::Relaxed);
        let mut event = bad_window(12);
        let handled = unsafe { record_x_error(0x2010 as *mut _, &mut event) };
        assert_eq!(handled, 0);
        assert!(UNTRACKED_ERRORS.load(Ordering::Relaxed) > untracked);
        assert!(!tracked(0x2010));
    }
}
