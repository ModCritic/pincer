//! Live Xlib checks against a private Xvfb.
//!
//! Each test starts its own X server on a free display number and runs its
//! body in a child copy of the test binary with `DISPLAY` set to that server.
//! An Xlib error that ends the process then ends only the child, which the
//! parent reports as a failure, and no other test sees the changed `DISPLAY`.
//! Ignored by default: run them inside a sandbox (`--ignored`), never beside
//! a desktop session.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Set in the child copy of the test binary.
const CHILD_ENV: &str = "PINCER_XLIB_LIVE_CHILD";

/// The private Xvfb each test starts, on a display number nobody else uses.
pub(crate) use crate::throwaway_display::PrivateXvfb as Server;

/// Arguments for [`Server`]: 640x480, any client may connect.
pub(crate) const XVFB_ARGS: &[&str] = &["-screen", "0", "640x480x24", "-ac"];

pub(crate) fn in_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// Run the ignored test `name` (its path inside this crate) in a child copy
/// of the test binary on `display`.
pub(crate) fn run_child(name: &str, display: &str) -> Output {
    Command::new(std::env::current_exe().expect("test binary path"))
        .args([
            name,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env("DISPLAY", display)
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .output()
        .expect("run the child test")
}

pub(crate) fn assert_child_passed(output: &Output, marker: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains(marker),
        "child test failed ({}):\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
        output.status
    );
}

/// A scratch folder for this test run, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("pincer-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch folder");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Clients the X server has open right now (X-Resource QueryClients).
fn server_client_count(conn: &x11rb::rust_connection::RustConnection) -> usize {
    use x11rb::protocol::res::ConnectionExt as _;
    conn.res_query_clients()
        .expect("XResQueryClients request")
        .reply()
        .expect("XResQueryClients reply")
        .clients
        .len()
}

/// A window id that was valid a moment ago and is destroyed now.
fn destroyed_window(display: *mut x11::xlib::Display) -> x11::xlib::Window {
    unsafe {
        let root = x11::xlib::XDefaultRootWindow(display);
        let window = x11::xlib::XCreateSimpleWindow(display, root, 0, 0, 10, 10, 0, 0, 0);
        x11::xlib::XDestroyWindow(display, window);
        x11::xlib::XSync(display, 0);
        window
    }
}

/// A BadWindow on one of the input routes' Xlib displays (here: mapping a
/// window that was just destroyed) must not end the process. libX11's
/// default error handler prints the error and calls `exit(1)`. The error is
/// recorded on its own display, also when several threads hit errors at
/// once.
#[test]
#[ignore = "starts its own Xvfb; run inside a sandbox"]
fn an_x_error_does_not_end_the_process() {
    const MARKER: &str = "XLIB_CHILD_SURVIVED_BAD_WINDOW";
    if in_child() {
        let xdisplay = super::open_display().expect("open the test display");
        let display = xdisplay.as_ptr();
        let window = destroyed_window(display);
        assert_eq!(
            xdisplay.take_error(),
            None,
            "no error before the bad request"
        );
        unsafe { x11::xlib::XMapWindow(display, window) };
        let (error, count) = xdisplay.take_error().expect("the BadWindow is recorded");
        // BadWindow is 3, X_MapWindow is request 8.
        assert_eq!(
            (
                error.error_code,
                error.request_code,
                error.resource_id,
                count
            ),
            (3, 8, window, 1),
            "{error}"
        );

        // remove_master_pointer never had an error handler around it: an
        // XIRemoveMaster for a device the server does not have answers
        // BadDevice.
        super::remove_master_pointer(display, 250).expect("the request is sent");
        assert!(
            xdisplay.take_error().is_some(),
            "XIRemoveMaster on a missing device reports an error"
        );

        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    let xdisplay = super::open_display().expect("open a thread's display");
                    let display = xdisplay.as_ptr();
                    let window = destroyed_window(display);
                    for _ in 0..25 {
                        unsafe { x11::xlib::XMapWindow(display, window) };
                        unsafe { x11::xlib::XSync(display, 0) };
                    }
                    xdisplay.take_error().map(|(_, count)| count)
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().expect("thread ran"), Some(25));
        }
        println!("{MARKER}");
        return;
    }
    let server = Server::start(XVFB_ARGS);
    let output = run_child(
        "input::xlib_live_tests::an_x_error_does_not_end_the_process",
        &server.display,
    );
    assert_child_passed(&output, MARKER);
}

/// The MPX click refuses an Xtigervnc server right after it opens its Xlib
/// display; that early return must still close the display. The server
/// here is Xvfb copied to a file named `Xtigervnc`, which is what the
/// refusal looks for.
#[test]
#[ignore = "starts its own Xvfb; run inside a sandbox"]
fn an_early_refusal_closes_its_display() {
    const MARKER: &str = "XLIB_CHILD_NO_LEAKED_DISPLAY";
    if in_child() {
        let (conn, _) =
            x11rb::rust_connection::RustConnection::connect(None).expect("connect counter");
        let before = server_client_count(&conn);
        let click = super::VirtualPointerClick {
            target_window: 1,
            x: 1,
            y: 1,
            button: 1,
            count: 1,
        };
        for _ in 0..3 {
            let refused = super::send_virtual_pointer_click("xlib-leak-check", &click)
                .expect_err("an Xtigervnc server is refused");
            assert!(
                refused.to_string().contains("Xtigervnc"),
                "unexpected refusal: {refused:#}"
            );
        }
        let after = server_client_count(&conn);
        assert_eq!(
            after,
            before,
            "the refused clicks left {} X connection(s) open",
            after.saturating_sub(before)
        );
        println!("{MARKER}");
        return;
    }
    let scratch = Scratch::new("xlib-leak");
    let fake = scratch.0.join("Xtigervnc");
    let xvfb = which("Xvfb").expect("Xvfb on PATH");
    std::fs::copy(&xvfb, &fake).expect("copy Xvfb as Xtigervnc");
    let server = Server::start_binary(&fake, XVFB_ARGS);
    let output = run_child(
        "input::xlib_live_tests::an_early_refusal_closes_its_display",
        &server.display,
    );
    drop(server);
    assert_child_passed(&output, MARKER);
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.is_file())
}

/// When an Xlib connection breaks (here the server kills the client), libX11
/// ends the process: no Xlib call may run on that display again, and its
/// handler cannot be returned from soundly while other threads use Xlib. The
/// exit must still say why, and release the core pointer buttons Pincer
/// pressed through XTest, which otherwise stay down after the process ends.
#[test]
#[ignore = "starts its own Xvfb; run inside a sandbox"]
fn a_broken_connection_releases_held_buttons_before_the_exit() {
    const MARKER: &str = "XLIB_CHILD_WENT_ON_AFTER_A_BROKEN_CONNECTION";
    if in_child() {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as _;
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .try_init();
        super::send_button_xtest_desktop(Some((20, 20)), 1, true).expect("XTest press");
        let xdisplay = super::open_display().expect("open the test display");
        let display = xdisplay.as_ptr();
        let window = unsafe {
            let root = x11::xlib::XDefaultRootWindow(display);
            let window = x11::xlib::XCreateSimpleWindow(display, root, 0, 0, 1, 1, 0, 0, 0);
            x11::xlib::XSync(display, 0);
            window
        };
        let (killer, _) =
            x11rb::rust_connection::RustConnection::connect(None).expect("connect killer");
        killer
            .kill_client(window as u32)
            .expect("KillClient request")
            .check()
            .expect("KillClient");
        killer.flush().expect("flush");
        // libX11 notices the closed connection here and ends the process.
        unsafe { x11::xlib::XSync(display, 0) };
        println!("{MARKER}");
        return;
    }
    let server = Server::start(XVFB_ARGS);
    let output = run_child(
        "input::xlib_live_tests::a_broken_connection_releases_held_buttons_before_the_exit",
        &server.display,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains(MARKER), "the child went on:\n{stdout}");
    assert_eq!(output.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(
        stderr.contains("connection to X display") && stderr.contains("released held button 1"),
        "the exit says why and what it released:\n{stderr}"
    );
    let (conn, screen_num) =
        x11rb::rust_connection::RustConnection::connect(Some(&server.display)).expect("connect");
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::ConnectionExt as _;
    let root = conn.setup().roots[screen_num].root;
    let mask = conn
        .query_pointer(root)
        .expect("QueryPointer request")
        .reply()
        .expect("QueryPointer reply")
        .mask;
    assert!(
        !mask.contains(x11rb::protocol::xproto::KeyButMask::BUTTON1),
        "the XTest button is still down after the exit"
    );
}
