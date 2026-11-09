#![cfg(target_os = "linux")]

//! Senders that open a short-lived X connection, send one request and return,
//! against a private Xvfb this test starts. Each must round-trip before its
//! connection drops: after a bare flush the server can discard the request.
//!
//! cargo test -p platform-linux --test short_connection_x11 -- --ignored --test-threads=1

use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// A private Xvfb on a display number it picks itself; `DISPLAY` points at it
/// while the value lives, so the code under test never sees the user's
/// display.
struct PrivateDisplay {
    server: std::process::Child,
    previous: Option<std::ffi::OsString>,
}

impl PrivateDisplay {
    fn start() -> Result<(Self, RustConnection, usize)> {
        let mut server = std::process::Command::new("Xvfb")
            .args(["-displayfd", "1", "-nolisten", "tcp"])
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let mut number = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(server.stdout.take().expect("piped stdout")),
            &mut number,
        )?;
        let display = format!(":{}", number.trim());
        let previous = std::env::var_os("DISPLAY");
        std::env::set_var("DISPLAY", &display);
        let (conn, screen) = x11rb::connect(Some(&display))?;
        Ok((Self { server, previous }, conn, screen))
    }
}

impl Drop for PrivateDisplay {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(display) => std::env::set_var("DISPLAY", display),
            None => std::env::remove_var("DISPLAY"),
        }
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

fn atom(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// The next `_NET_CLOSE_WINDOW` the "window manager" connection receives,
/// or `None` within `timeout`.
fn next_close_request(
    wm: &RustConnection,
    close_atom: Atom,
    timeout: Duration,
) -> Result<Option<ClientMessageEvent>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match wm.poll_for_event()? {
            Some(Event::ClientMessage(event)) if event.type_ == close_atom => {
                return Ok(Some(event))
            }
            Some(Event::Error(error)) => bail!("X11 error on the WM connection: {error:?}"),
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    Ok(None)
}

/// Every close request reaches the window manager. Before the fix
/// `close_window` dropped its connection right after `flush()`, and the X
/// server could discard the request unprocessed (6 of 8 Alt+F4 presses on
/// KDE did nothing).
#[test]
#[ignore = "requires Xvfb"]
fn every_close_request_reaches_the_window_manager() -> Result<()> {
    let (_display, wm, screen) = PrivateDisplay::start()?;
    let root = wm.setup().roots[screen].root;
    wm.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new()
            .event_mask(EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY),
    )?
    .check()?;
    let close_atom = atom(&wm, "_NET_CLOSE_WINDOW")?;

    // The "application": a top-level that advertises this process's pid,
    // on its own connection so the WM connection only sees the requests.
    let (app, _) = x11rb::connect(None)?;
    let window = app.generate_id()?;
    app.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        100,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new(),
    )?
    .check()?;
    let pid = std::process::id();
    app.change_property32(
        PropMode::REPLACE,
        window,
        atom(&app, "_NET_WM_PID")?,
        AtomEnum::CARDINAL,
        &[pid],
    )?
    .check()?;
    // Drain the CreateNotify the WM connection was sent.
    while wm.poll_for_event()?.is_some() {}

    const PRESSES: usize = 40;
    let mut lost = Vec::new();
    for press in 0..PRESSES {
        platform_linux::x11::close_window(u64::from(window), pid)?;
        // A delivered request is in the WM's queue before close_window
        // returns; the timeout only bounds a lost one.
        match next_close_request(&wm, close_atom, Duration::from_millis(500))? {
            Some(event) => {
                assert_eq!(event.window, window);
                assert_eq!(event.format, 32);
                assert_eq!(
                    event.data.as_data32()[1],
                    1,
                    "source indication: application"
                );
            }
            None => lost.push(press),
        }
    }
    assert!(
        lost.is_empty(),
        "{} of {PRESSES} close requests never reached the window manager (presses {lost:?})",
        lost.len()
    );
    Ok(())
}

/// Synthetic FocusIn / FocusOut (the Qt AT-SPI focus nudge) arrive every
/// time; they used the same send, flush and drop pattern.
#[test]
#[ignore = "requires Xvfb"]
fn every_synthetic_focus_event_arrives() -> Result<()> {
    let (_display, conn, screen) = PrivateDisplay::start()?;
    let root = conn.setup().roots[screen].root;
    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        100,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().event_mask(EventMask::FOCUS_CHANGE),
    )?
    .check()?;

    const ROUNDS: usize = 40;
    let mut lost = Vec::new();
    for round in 0..ROUNDS {
        platform_linux::input::send_focus_in(u64::from(window))?;
        platform_linux::input::send_focus_out(u64::from(window))?;
        let deadline = Instant::now() + Duration::from_millis(500);
        let (mut focus_in, mut focus_out) = (false, false);
        while !(focus_in && focus_out) && Instant::now() < deadline {
            match conn.poll_for_event()? {
                Some(Event::FocusIn(event)) if event.event == window => focus_in = true,
                Some(Event::FocusOut(event)) if event.event == window => focus_out = true,
                Some(Event::Error(error)) => bail!("X11 error: {error:?}"),
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        if !(focus_in && focus_out) {
            lost.push((round, focus_in, focus_out));
        }
    }
    assert!(
        lost.is_empty(),
        "{} of {ROUNDS} rounds lost an event (round, FocusIn seen, FocusOut seen): {lost:?}",
        lost.len()
    );
    Ok(())
}
