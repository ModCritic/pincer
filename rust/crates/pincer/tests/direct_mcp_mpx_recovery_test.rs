//! Direct `pincer mcp` removes the XI2 master pointers a crashed Pincer left
//! behind, as `pincer serve` does at startup, and keeps the ones whose owner
//! still runs. On KDE Plasma X11 it removes none.
//!
//! The test starts its own Xvfb (`-displayfd`, so it never sees the user's
//! display) and creates two masters named the way Pincer names its own (see
//! platform-linux `input/mpx_owner.rs`): one owned by a process that has
//! exited, one owned by this test process. It skips when Xvfb or sha256sum is
//! missing.

#![cfg(target_os = "linux")]

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io::{BufRead as _, Write as _};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use pincer_testkit::IsolatedStateRoot;

#[repr(C)]
struct XIAddMasterInfo {
    kind: c_int,
    name: *const c_char,
    send_core: c_int,
    enable: c_int,
}

#[repr(C)]
struct XIDeviceInfo {
    deviceid: c_int,
    name: *mut c_char,
    use_: c_int,
    attachment: c_int,
    enabled: c_int,
    num_classes: c_int,
    classes: *mut c_void,
}

#[link(name = "X11")]
extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
}

#[link(name = "Xi")]
extern "C" {
    fn XIQueryVersion(display: *mut c_void, major: *mut c_int, minor: *mut c_int) -> c_int;
    fn XIChangeHierarchy(display: *mut c_void, changes: *mut XIAddMasterInfo, count: c_int)
        -> c_int;
    fn XIQueryDevice(display: *mut c_void, device: c_int, count: *mut c_int) -> *mut XIDeviceInfo;
    fn XIFreeDeviceInfo(info: *mut XIDeviceInfo);
}

const XI_ADD_MASTER: c_int = 1;
const XI_ALL_DEVICES: c_int = 0;
const XI_MASTER_POINTER: c_int = 1;

/// A private Xvfb and one XI2 client connection to it.
struct PrivateDisplay {
    server: std::process::Child,
    name: String,
    connection: *mut c_void,
}

impl PrivateDisplay {
    fn start() -> Option<Self> {
        let mut server = match Command::new("Xvfb")
            .args(["-displayfd", "1", "-nolisten", "tcp", "-screen", "0", "640x480x24"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(server) => server,
            Err(error) => {
                eprintln!("skipping: Xvfb is not available ({error})");
                return None;
            }
        };
        let mut number = String::new();
        std::io::BufReader::new(server.stdout.take().expect("piped stdout"))
            .read_line(&mut number)
            .expect("Xvfb display number");
        let name = format!(":{}", number.trim());
        let display_name = CString::new(name.clone()).unwrap();
        // SAFETY: a valid C string; the connection is closed in Drop.
        let connection = unsafe { XOpenDisplay(display_name.as_ptr()) };
        assert!(!connection.is_null(), "open {name}");
        let (mut major, mut minor) = (2, 2);
        // The server ignores XI2 hierarchy requests from a client that never
        // announced its XI2 version.
        // SAFETY: an open display and two writable integers.
        let status = unsafe { XIQueryVersion(connection, &mut major, &mut minor) };
        assert_eq!(status, 0, "XI2 is not available on {name}");
        Some(Self {
            server,
            name,
            connection,
        })
    }

    fn add_master(&self, name: &str) {
        let name = CString::new(name).unwrap();
        let mut change = XIAddMasterInfo {
            kind: XI_ADD_MASTER,
            name: name.as_ptr(),
            send_core: 1,
            enable: 1,
        };
        // SAFETY: one XIAddMaster change whose name outlives the call.
        unsafe {
            XIChangeHierarchy(self.connection, &mut change, 1);
            XSync(self.connection, 0);
        }
    }

    fn master_pointers(&self) -> Vec<String> {
        let mut count = 0;
        // SAFETY: an open display; the list is freed below.
        let devices = unsafe { XIQueryDevice(self.connection, XI_ALL_DEVICES, &mut count) };
        assert!(!devices.is_null(), "XIQueryDevice");
        // SAFETY: XIQueryDevice returned `count` entries.
        let names = unsafe { std::slice::from_raw_parts(devices, count as usize) }
            .iter()
            .filter(|device| device.use_ == XI_MASTER_POINTER)
            // SAFETY: each name is a C string owned by the list.
            .map(|device| unsafe { CStr::from_ptr(device.name) }.to_string_lossy().into_owned())
            .collect();
        // SAFETY: freed once, after the last read.
        unsafe { XIFreeDeviceInfo(devices) };
        names
    }
}

impl Drop for PrivateDisplay {
    fn drop(&mut self) {
        // SAFETY: opened in start and not used after this.
        unsafe { XCloseDisplay(self.connection) };
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

/// The owner domain Pincer encodes in a master name: SHA-256 over the boot
/// id, this PID namespace and the effective uid, first 16 bytes.
fn owner_domain() -> Option<String> {
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    let namespace = std::fs::read_link("/proc/self/ns/pid").unwrap();
    let mut input = boot.trim().as_bytes().to_vec();
    input.push(0);
    input.extend_from_slice(namespace.as_os_str().as_encoded_bytes());
    input.push(0);
    // SAFETY: geteuid has no preconditions.
    input.extend_from_slice(&unsafe { libc::geteuid() }.to_be_bytes());
    let mut hasher = match Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(hasher) => hasher,
        Err(error) => {
            eprintln!("skipping: sha256sum is not available ({error})");
            return None;
        }
    };
    hasher.stdin.take().unwrap().write_all(&input).unwrap();
    let output = hasher.wait_with_output().unwrap();
    let hex = String::from_utf8(output.stdout).unwrap();
    let digest: Vec<u8> = (0..16)
        .map(|index| u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap())
        .collect();
    Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest))
}

fn master_name(domain: &str, pid: u32, start_ticks: u64, nonce: u64) -> String {
    let mut process = pid.to_be_bytes().to_vec();
    process.extend_from_slice(&start_ticks.to_be_bytes());
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!(
        "PINCER v1.{domain}.{}.{}",
        encode(&process),
        encode(&nonce.to_be_bytes())
    )
}

/// Field 22 of /proc/self/stat: when this process started, in clock ticks.
fn own_start_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let after_command = &stat[stat.rfind(')').unwrap() + 2..];
    after_command.split_whitespace().nth(19).unwrap().parse().unwrap()
}

/// Run direct `pincer mcp` on `display` with stdin closed, so it starts and
/// exits at once, in a desktop session described by `session`.
fn run_direct_mcp(display: &PrivateDisplay, session: &[(&str, &str)]) -> String {
    let state = IsolatedStateRoot::new().expect("isolated state root");
    let bus = state.path().join("no-session-bus");
    let mut command = Command::new(env!("CARGO_BIN_EXE_pincer"));
    command
        .args(["mcp", "--no-overlay"])
        .envs(state.env())
        .env("DISPLAY", &display.name)
        .env_remove("WAYLAND_DISPLAY")
        .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}", bus.display()))
        .env("NO_AT_BRIDGE", "1")
        .env("PINCER_DISABLE_A11Y_ADVERTISE", "1")
        .env("PINCER_TELEMETRY_ENABLED", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for name in [
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
        "KDE_FULL_SESSION",
    ] {
        command.env_remove(name);
    }
    command.envs(session.iter().copied());
    let mut mcp = command.spawn().expect("spawn pincer mcp");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = mcp.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = mcp.kill();
            panic!("pincer mcp did not exit at stdin EOF within 60 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut mcp.stderr.take().unwrap(), &mut stderr).unwrap();
    format!("{status}: {stderr}")
}

/// A master owned by a process that has exited, and one owned by this test.
fn add_test_masters(display: &PrivateDisplay, domain: &str, nonce: u64) -> (String, String) {
    let mut exited = Command::new("true").spawn().expect("spawn true");
    let dead_pid = exited.id();
    exited.wait().unwrap();
    let orphan = format!("{} pointer", master_name(domain, dead_pid, 1, nonce));
    let live = format!(
        "{} pointer",
        master_name(domain, std::process::id(), own_start_ticks(), nonce + 1)
    );
    for name in [&orphan, &live] {
        display.add_master(name.strip_suffix(" pointer").unwrap());
    }
    let masters = display.master_pointers();
    for name in [&orphan, &live] {
        assert!(masters.contains(name), "test master {name} missing: {masters:?}");
    }
    (orphan, live)
}

#[test]
fn direct_mcp_removes_orphaned_masters_and_keeps_live_ones() {
    let Some(display) = PrivateDisplay::start() else {
        return;
    };
    let Some(domain) = owner_domain() else {
        return;
    };
    let (orphan, live) = add_test_masters(&display, &domain, 7);

    let outcome = run_direct_mcp(&display, &[("XDG_SESSION_TYPE", "x11")]);

    let after = display.master_pointers();
    assert!(
        !after.contains(&orphan),
        "direct mcp left the orphaned master: {after:?}\n{outcome}"
    );
    assert!(
        after.contains(&live),
        "direct mcp removed a master whose owner still runs: {after:?}\n{outcome}"
    );
}

/// On KDE Plasma X11 Pincer adds no masters, and removing one crashes every
/// Qt 6 client, so recovery leaves even an orphan alone there.
#[test]
fn direct_mcp_leaves_masters_alone_on_kde_x11() {
    let Some(display) = PrivateDisplay::start() else {
        return;
    };
    let Some(domain) = owner_domain() else {
        return;
    };
    let (orphan, live) = add_test_masters(&display, &domain, 17);

    let outcome = run_direct_mcp(
        &display,
        &[("XDG_SESSION_TYPE", "x11"), ("XDG_CURRENT_DESKTOP", "KDE")],
    );

    let after = display.master_pointers();
    for name in [&orphan, &live] {
        assert!(after.contains(name), "{name} was removed on KDE X11: {after:?}\n{outcome}");
    }
}
