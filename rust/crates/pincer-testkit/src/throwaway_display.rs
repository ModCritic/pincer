//! Where tests that drive an X server may run.
//!
//! These tests create windows, move the pointer, press keys and take the
//! clipboard on the X server they reach. Beside a desktop session that is the
//! developer's own screen. One rule decides for every such test:
//!
//! * A test that starts its own server ([`PrivateXvfb`]) always runs.
//! * A test that drives a server it did not start runs only when
//!   `PINCER_TEST_THROWAWAY_DISPLAY` names every display it can reach:
//!   `DISPLAY`, and `WAYLAND_DISPLAY` too when that is set (a comma-separated
//!   list, for example `:93`). `rust/scripts/ci-x11-tests.sh` sets it for the
//!   servers it starts; a sandbox harness sets it for its own. Anything else
//!   panics with [`throwaway_display`]'s message.
//!
//! Tests in one process that read or change the display variables hold
//! [`display_env_lock`], so a test that points `DISPLAY` at its own server
//! for a moment cannot send another test's input to the developer's display.
//!
//! The file is shared: pincer-testkit exports it, and platform-linux (which
//! does not depend on pincer-testkit) includes it with `#[path]`. Keep it to
//! std, libc and x11rb, which both crates have.

#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Names the displays a test may drive; see the module documentation.
pub const THROWAWAY_DISPLAY_ENV: &str = "PINCER_TEST_THROWAWAY_DISPLAY";

static DISPLAY_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serializes every read and change of `DISPLAY` and `WAYLAND_DISPLAY` by
/// tests in this process. A panicking holder does not block the rest.
pub fn display_env_lock() -> MutexGuard<'static, ()> {
    DISPLAY_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Why a test may not drive the displays named by `display` and `wayland`
/// when `allowed` is the value of [`THROWAWAY_DISPLAY_ENV`], or `None` when
/// it may.
pub fn refusal(
    display: Option<&OsStr>,
    wayland: Option<&OsStr>,
    allowed: Option<&OsStr>,
) -> Option<String> {
    let display = display.filter(|value| !value.is_empty());
    let wayland = wayland.filter(|value| !value.is_empty());
    let Some(display) = display else {
        return Some("DISPLAY is not set".to_owned());
    };
    let allowed: Vec<&str> = allowed
        .and_then(OsStr::to_str)
        .map(|list| list.split(',').map(str::trim).collect())
        .unwrap_or_default();
    let mut unlisted = Vec::new();
    for (name, value) in [("DISPLAY", Some(display)), ("WAYLAND_DISPLAY", wayland)] {
        let Some(value) = value else { continue };
        if !value.to_str().is_some_and(|value| allowed.contains(&value)) {
            unlisted.push(format!("{name}={}", value.to_string_lossy()));
        }
    }
    if unlisted.is_empty() {
        None
    } else {
        Some(format!(
            "{} not named in {THROWAWAY_DISPLAY_ENV}",
            unlisted.join(" and ")
        ))
    }
}

/// The display a test may drive, with [`display_env_lock`] held until it
/// drops.
pub struct ThrowawayDisplay {
    pub display: String,
    _lock: MutexGuard<'static, ()>,
}

/// The display named by `DISPLAY`, if the rule in the module documentation
/// lets a test drive it. Panics otherwise, saying how to run the test.
pub fn throwaway_display() -> ThrowawayDisplay {
    let lock = display_env_lock();
    let display = std::env::var_os("DISPLAY");
    if let Some(reason) = refusal(
        display.as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os(THROWAWAY_DISPLAY_ENV).as_deref(),
    ) {
        panic!(
            "refusing to drive this X server ({reason}): this test creates windows and \
             sends input to it, which beside a desktop session is your own screen. Run it \
             with rust/scripts/ci-x11-tests.sh (it starts a private Xvfb, session bus and \
             accessibility bus), or start a throwaway X server yourself and set \
             {THROWAWAY_DISPLAY_ENV} to the DISPLAY value (plus WAYLAND_DISPLAY, \
             comma-separated, if that is set)."
        );
    }
    ThrowawayDisplay {
        display: display
            .and_then(|value| value.into_string().ok())
            .expect("a UTF-8 DISPLAY"),
        _lock: lock,
    }
}

/// `DISPLAY` pointed at another server, and `WAYLAND_DISPLAY` hidden, for
/// this process until the value drops; [`display_env_lock`] is held
/// meanwhile.
pub struct DisplayEnv {
    display: Option<OsString>,
    wayland: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}

/// Point this process's `DISPLAY` at `display` (a server the test started).
pub fn point_display_at(display: &str) -> DisplayEnv {
    let lock = display_env_lock();
    let saved = DisplayEnv {
        display: std::env::var_os("DISPLAY"),
        wayland: std::env::var_os("WAYLAND_DISPLAY"),
        _lock: lock,
    };
    std::env::set_var("DISPLAY", display);
    std::env::remove_var("WAYLAND_DISPLAY");
    saved
}

impl Drop for DisplayEnv {
    fn drop(&mut self) {
        for (name, value) in [
            ("DISPLAY", self.display.take()),
            ("WAYLAND_DISPLAY", self.wayland.take()),
        ] {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// An Xvfb this test started, on a display number nobody else uses, stopped
/// when the value drops.
pub struct PrivateXvfb {
    child: Option<Child>,
    pub number: u32,
    pub display: String,
}

/// Lowest display number [`PrivateXvfb`] tries; well above the numbers
/// desktops and the sandbox slots (:80 to :91) take.
const FIRST_NUMBER: u32 = 140;

impl PrivateXvfb {
    /// Start `Xvfb` with `args` after the display number.
    pub fn start(args: &[&str]) -> Self {
        Self::start_binary(Path::new("Xvfb"), args)
    }

    /// Start `binary` (an Xvfb, possibly under another file name) with `args`
    /// after the display number, on the first number from 140 with neither a
    /// lock file nor a socket. Not `-displayfd`: a server started with it
    /// writes no lock file, so Pincer (which reads `/tmp/.X<n>-lock` to tell
    /// Xvfb and Xtigervnc apart) cannot identify it, and Xvfb's own search
    /// can land on a number whose server has no lock file either (Xorg under
    /// SDDM) and, from another network namespace, unlink that server's socket.
    pub fn start_binary(binary: &Path, args: &[&str]) -> Self {
        for number in FIRST_NUMBER..FIRST_NUMBER + 200 {
            if display_number_taken(number) {
                continue;
            }
            if let Some(server) = Self::try_start(binary, number, args) {
                return server;
            }
        }
        panic!(
            "no display number from {FIRST_NUMBER} to {} started {}",
            FIRST_NUMBER + 199,
            binary.display()
        );
    }

    /// Start `Xvfb` again on `number`, which this test held before (a restart
    /// on the same display). Panics if it does not come up.
    pub fn restart_on(number: u32, args: &[&str]) -> Self {
        Self::try_start(Path::new("Xvfb"), number, args)
            .unwrap_or_else(|| panic!("Xvfb did not start again on :{number}"))
    }

    fn try_start(binary: &Path, number: u32, args: &[&str]) -> Option<Self> {
        let display = format!(":{number}");
        let mut child = Command::new(binary)
            .arg(&display)
            .args(["-nolisten", "tcp", "-noreset"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("start {}: {error}", binary.display()));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // A server that lost the number to another one exits at once.
            if matches!(child.try_wait(), Ok(Some(_))) {
                return None;
            }
            // Ours once it accepts clients and its lock names it.
            if x11rb::rust_connection::RustConnection::connect(Some(&display)).is_ok()
                && lock_pid(number) == Some(child.id())
            {
                return Some(Self {
                    child: Some(child),
                    number,
                    display,
                });
            }
            if Instant::now() >= deadline {
                stop_child(&mut child);
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Stop the server now: SIGTERM, so it removes its lock file and socket
    /// (SIGKILL leaves both), then SIGKILL after 2 s.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            stop_child(&mut child);
        }
    }
}

impl Drop for PrivateXvfb {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The pid in display `number`'s lock file.
fn lock_pid(number: u32) -> Option<u32> {
    std::fs::read_to_string(format!("/tmp/.X{number}-lock"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether display `number` has a lock file or a socket in `/tmp`.
pub fn display_number_taken(number: u32) -> bool {
    Path::new(&format!("/tmp/.X{number}-lock")).exists()
        || Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
}

fn stop_child(child: &mut Child) {
    // SAFETY: plain libc call on the pid of a child this process spawned and
    // has not reaped.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::refusal;
    use std::ffi::OsStr;

    fn check(display: Option<&str>, wayland: Option<&str>, allowed: Option<&str>) -> bool {
        refusal(
            display.map(OsStr::new),
            wayland.map(OsStr::new),
            allowed.map(OsStr::new),
        )
        .is_none()
    }

    #[test]
    fn only_displays_named_in_the_variable_may_be_driven() {
        assert!(check(Some(":93"), None, Some(":93")));
        assert!(check(Some(":5"), Some("wayland-test"), Some(":5, wayland-test")));
        // The developer's session: nothing named, or another display named.
        assert!(!check(Some(":0"), None, None));
        assert!(!check(Some(":0"), None, Some(":93")));
        assert!(!check(Some(":0"), None, Some("1")));
        assert!(!check(Some(":0"), None, Some("")));
        // A Wayland session's socket reachable beside the X server.
        assert!(!check(Some(":93"), Some("wayland-0"), Some(":93")));
        assert!(!check(None, None, Some(":93")));
        assert!(!check(Some(""), None, Some("")));
    }
}
