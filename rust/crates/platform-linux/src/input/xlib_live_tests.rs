//! Live Xlib checks against a private Xvfb.
//!
//! Each test starts its own X server on a free display number and runs its
//! body in a child copy of the test binary with `DISPLAY` set to that server.
//! An Xlib error that ends the process then ends only the child, which the
//! parent reports as a failure, and no other test sees the changed `DISPLAY`.
//! Ignored by default: run them inside a sandbox (`--ignored`), never beside
//! a desktop session.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

/// Set in the child copy of the test binary.
const CHILD_ENV: &str = "PINCER_XLIB_LIVE_CHILD";

struct Server {
    child: Option<Child>,
    display: String,
}

impl Server {
    /// Start `binary` (an Xvfb, possibly under another file name) on the
    /// first free display number from 140.
    fn start(binary: &Path) -> Self {
        let number = (140..200)
            .find(|n| {
                !Path::new(&format!("/tmp/.X{n}-lock")).exists()
                    && !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
            })
            .expect("a free X display number in 140..200");
        let display = format!(":{number}");
        let mut child = Command::new(binary)
            .args([
                display.as_str(),
                "-screen",
                "0",
                "640x480x24",
                "-ac",
                "-noreset",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("start {}: {e}", binary.display()));
        for _ in 0..250 {
            if x11rb::rust_connection::RustConnection::connect(Some(&display)).is_ok() {
                return Self {
                    child: Some(child),
                    display,
                };
            }
            if let Some(status) = child.try_wait().expect("poll Xvfb") {
                panic!("{} exited before it was ready: {status}", binary.display());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("{} did not become ready on {display}", binary.display());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn in_child() -> bool {
    std::env::var_os(CHILD_ENV).is_some()
}

/// Run the ignored test `name` (its path inside this crate) in a child copy
/// of the test binary on `display`.
fn run_child(name: &str, display: &str) -> Output {
    Command::new(std::env::current_exe().expect("test binary path"))
        .args([name, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .env("DISPLAY", display)
        .stdin(Stdio::null())
        .output()
        .expect("run the child test")
}

fn assert_child_passed(output: &Output, marker: &str) {
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
            after, before,
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
    let server = Server::start(&fake);
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
