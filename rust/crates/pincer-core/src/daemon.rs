//! Shared Pincer daemon wire protocol and synchronous client.
//!
//! The daemon, CLI, MCP proxy, and imported SDK all use these exact request,
//! response, probing, timeout, and framing rules. Keeping them outside the
//! binary crate prevents language bindings from growing an independent
//! implementation of the native transport.

use serde::{Deserialize, Serialize};

/// Versioned identity returned by the daemon before an imported SDK or an MCP
/// proxy is allowed to treat an endpoint as ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonMetadata {
    pub driver_version: String,
    pub contract_version: String,
    pub tools_list_schema_version: String,
    pub capability_version: String,
    pub mcp_protocol_version: String,
    pub pid: u32,
    pub embedded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_bundle_id: Option<String>,
}

pub fn current_daemon_metadata() -> DaemonMetadata {
    DaemonMetadata {
        driver_version: env!("CARGO_PKG_VERSION").to_owned(),
        contract_version: pincer_contract::CONTRACT_VERSION.to_owned(),
        tools_list_schema_version: pincer_contract::TOOLS_LIST_SCHEMA_VERSION.to_owned(),
        capability_version: pincer_contract::CAPABILITY_VERSION.to_owned(),
        mcp_protocol_version: pincer_contract::MCP_PROTOCOL_VERSION.to_owned(),
        pid: std::process::id(),
        embedded: crate::embedded_mode(),
        host_bundle_id: std::env::var(crate::HOST_BUNDLE_ID_ENV)
            .ok()
            .filter(|value| !value.is_empty()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolObservationOrigin {
    McpProxy,
    Direct,
}

/// Closed, content-free identity for a direct daemon client. This is carried
/// separately from `observation_origin` so older daemons safely ignore the
/// additive field instead of rejecting a newer enum variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonClientKind {
    Cli,
    PythonSdk,
    TypescriptSdk,
    Unknown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    /// Transport-owned connection identity. Public session identity remains
    /// in `args.session`; callers must not use this field to select policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Bounded internal routing metadata for exactly-once completion
    /// observation. Older peers ignore this additive field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_origin: Option<ToolObservationOrigin>,
    /// Privacy-bounded direct-client identity used only for aggregate product
    /// telemetry. Raw package names, application names, and host identifiers
    /// are never accepted here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_kind: Option<DaemonClientKind>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl DaemonResponse {
    pub fn ok(result: serde_json::Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            error: None,
            exit_code: None,
        }
    }

    pub fn err(message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(message.into()),
            exit_code: Some(exit_code),
        }
    }
}

/// Perform the daemon identity/version handshake used by embedded startup and
/// imported SDK diagnostics. Unlike a connect-only probe, this proves the
/// endpoint speaks the expected Pincer wire protocol.
pub fn request_daemon_metadata(socket_path: &str) -> anyhow::Result<DaemonMetadata> {
    let response = send_request(
        socket_path,
        &DaemonRequest {
            method: "metadata".into(),
            name: None,
            args: None,
            session_id: None,
            observation_origin: None,
            client_kind: None,
        },
    )?;
    if !response.ok {
        anyhow::bail!(
            "daemon metadata request failed: {}",
            response.error.unwrap_or_else(|| "unknown error".into())
        );
    }
    serde_json::from_value(
        response
            .result
            .ok_or_else(|| anyhow::anyhow!("daemon metadata response omitted result"))?,
    )
    .map_err(Into::into)
}

/// Return the platform socket or pipe path for an installed-product namespace.
/// The executable chooses its release/local namespace; imported SDKs use the
/// release namespace unless the caller supplies an explicit path.
///
/// On Unix the socket lives in [`daemon_state_dir_for_namespace`]. When that
/// has no private directory to offer, this returns a placeholder under
/// `/nonexistent` whose name says what is missing, so connecting or binding
/// fails instead of using a shared directory such as `/tmp`. Prefer
/// [`try_socket_path_for_namespace`], which returns the reason as an error.
pub fn socket_path_for_namespace(namespace: &str) -> String {
    #[cfg(unix)]
    {
        try_socket_path_for_namespace(namespace)
            .unwrap_or_else(|_| unavailable_daemon_path(namespace, "sock"))
    }
    #[cfg(target_os = "windows")]
    {
        format!(r"\\.\pipe\{namespace}")
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        format!("/tmp/{namespace}.sock")
    }
}

/// The default socket path for `namespace`, or an error that says how to get
/// one when no private directory is available.
#[cfg(unix)]
pub fn try_socket_path_for_namespace(namespace: &str) -> anyhow::Result<String> {
    let directory = daemon_state_dir_for_namespace(namespace)?;
    Ok(directory
        .join(format!("{namespace}.sock"))
        .to_string_lossy()
        .into_owned())
}

/// The default socket path for `namespace` (named pipes need no directory).
#[cfg(not(unix))]
pub fn try_socket_path_for_namespace(namespace: &str) -> anyhow::Result<String> {
    Ok(socket_path_for_namespace(namespace))
}

/// Placeholder daemon file path used when no private directory exists. Only
/// root can create `/nonexistent`, so nothing can be listening there.
#[cfg(unix)]
pub fn unavailable_daemon_path(namespace: &str, extension: &str) -> String {
    format!(
        "/nonexistent/{namespace}-needs-HOME-or-a-private-XDG_RUNTIME_DIR/{namespace}.{extension}"
    )
}

/// Directory for a namespace's daemon socket and PID file:
/// `$HOME/.cache/<namespace>` (`$HOME/Library/Caches/<namespace>` on macOS).
/// Without HOME it is `$XDG_RUNTIME_DIR/<namespace>`, and only when
/// [`private_runtime_dir`] accepts XDG_RUNTIME_DIR. Anything else is an error:
/// the old fallback, `/tmp/.cache/<namespace>`, could be created first by
/// another account, which could then intercept the socket.
#[cfg(unix)]
pub fn daemon_state_dir_for_namespace(namespace: &str) -> anyhow::Result<std::path::PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        let home = std::path::PathBuf::from(home);
        let cache = if cfg!(target_os = "macos") {
            home.join("Library").join("Caches")
        } else {
            home.join(".cache")
        };
        return Ok(cache.join(namespace));
    }
    Ok(private_runtime_dir()?.join(namespace))
}

/// `$XDG_RUNTIME_DIR`, when it is an absolute path to a real directory owned
/// by this user with no group or other permissions (mode 0700), as the XDG
/// Base Directory specification requires. Anything else is an error that says
/// how to fix it.
#[cfg(unix)]
pub fn private_runtime_dir() -> anyhow::Result<std::path::PathBuf> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    private_runtime_dir_from(std::env::var_os("XDG_RUNTIME_DIR"), euid)
}

#[cfg(unix)]
fn private_runtime_dir_from(
    value: Option<std::ffi::OsString>,
    euid: u32,
) -> anyhow::Result<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    const FIX: &str = "set HOME, set XDG_RUNTIME_DIR to a directory you own with mode 0700, \
                       or pass --socket <path> in a directory only you can write";
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        anyhow::bail!(
            "HOME and XDG_RUNTIME_DIR are not set, so there is no private directory for the \
             Pincer daemon socket and PID file; {FIX}"
        );
    };
    let directory = std::path::PathBuf::from(value);
    let shown = directory.display();
    if !directory.is_absolute() {
        anyhow::bail!("HOME is not set and XDG_RUNTIME_DIR={shown} is not an absolute path; {FIX}");
    }
    let metadata = std::fs::symlink_metadata(&directory).map_err(|error| {
        anyhow::anyhow!("HOME is not set and XDG_RUNTIME_DIR={shown} cannot be read ({error}); {FIX}")
    })?;
    if !metadata.is_dir() {
        anyhow::bail!("HOME is not set and XDG_RUNTIME_DIR={shown} is not a directory; {FIX}");
    }
    if metadata.uid() != euid {
        anyhow::bail!(
            "HOME is not set and XDG_RUNTIME_DIR={shown} is owned by uid {}, not by you \
             (uid {euid}); {FIX}",
            metadata.uid()
        );
    }
    if metadata.mode() & 0o077 != 0 {
        anyhow::bail!(
            "HOME is not set and XDG_RUNTIME_DIR={shown} has mode {:o}, so other accounts can \
             use it; {FIX}",
            metadata.mode() & 0o7777
        );
    }
    Ok(directory)
}

/// Refuse a daemon socket served by another account. The daemon checks its
/// clients' uid; this is the same check from the client side. Without it, an
/// account that creates the socket path first (for example in `/tmp`) would
/// receive every request, typed text included, and could answer with forged
/// results.
#[cfg(unix)]
pub fn check_daemon_peer_uid(
    socket_path: &str,
    expected_uid: u32,
    peer_uid: u32,
) -> anyhow::Result<()> {
    if peer_uid != expected_uid {
        anyhow::bail!(
            "refusing daemon socket {socket_path}: it is served by uid {peer_uid}, not by uid \
             {expected_uid} (this user), so another account may be listening there to read your \
             requests. Remove that socket if you can, or use a socket in a directory only you \
             can write, such as the default path or $XDG_RUNTIME_DIR."
        );
    }
    Ok(())
}

/// [`check_daemon_peer_uid`] against this process's effective uid.
#[cfg(unix)]
pub fn verify_daemon_peer_uid(socket_path: &str, peer_uid: u32) -> anyhow::Result<()> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    check_daemon_peer_uid(socket_path, unsafe { libc::geteuid() }, peer_uid)
}

/// Read the uid of the process listening on the other end of a connected
/// daemon socket and refuse it unless it is this account.
#[cfg(unix)]
pub fn verify_daemon_peer(
    stream: &std::os::unix::net::UnixStream,
    socket_path: &str,
) -> anyhow::Result<()> {
    let peer_uid = unix_peer_uid(stream).map_err(|error| {
        anyhow::anyhow!("read the owner of daemon socket {socket_path}: {error}")
    })?;
    verify_daemon_peer_uid(socket_path, peer_uid)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn unix_peer_uid(stream: &std::os::unix::net::UnixStream) -> std::io::Result<u32> {
    use std::os::unix::io::AsRawFd as _;
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the pointer and length describe a writable ucred, which is what
    // SO_PEERCRED fills in for a connected Unix stream socket.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(credentials.uid)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn unix_peer_uid(stream: &std::os::unix::net::UnixStream) -> std::io::Result<u32> {
    use std::os::unix::io::AsRawFd as _;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: both pointers refer to writable locals.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(uid)
}

/// Probe whether a daemon is listening on `socket_path`.
///
/// On Windows this uses `WaitNamedPipeW` without consuming a pipe instance.
/// Unix probes with the protocol's read-only `list` request.
pub fn is_daemon_listening(socket_path: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;

        #[link(name = "kernel32")]
        extern "system" {
            fn WaitNamedPipeW(lp_named_pipe_name: *const u16, timeout_ms: u32) -> i32;
        }

        let wide: Vec<u16> = std::ffi::OsStr::new(socket_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // NMPWAIT_NOWAIT == 1.
        unsafe { WaitNamedPipeW(wide.as_ptr(), 1) != 0 }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let request = DaemonRequest {
            method: "list".into(),
            name: None,
            args: None,
            session_id: None,
            observation_origin: None,
            client_kind: None,
        };
        send_request(socket_path, &request)
            .ok()
            .is_some_and(|response| response.ok)
    }
}

/// Send one newline-delimited request and read one response.
///
/// On Unix the request is sent only after [`verify_daemon_peer`] confirms that
/// this account runs the listening daemon. Unix reads and writes retry transient timeouts until a 120-second overall
/// deadline so slow accessibility walks and large screenshots are supported.
#[cfg(unix)]
pub fn send_request(socket_path: &str, request: &DaemonRequest) -> anyhow::Result<DaemonResponse> {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    let mut stream = UnixStream::connect(socket_path)
        .map_err(|error| anyhow::anyhow!("connect to {socket_path}: {error}"))?;
    // Send nothing to a socket another account is serving.
    verify_daemon_peer(&stream, socket_path)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;

    let mut writer = stream.try_clone()?;
    let line = serde_json::to_string(request)? + "\n";
    let write_deadline = Instant::now() + Duration::from_secs(120);
    crate::socket_io::write_all_with_retry(&mut writer, line.as_bytes(), write_deadline)?;

    let overall_deadline = Instant::now() + Duration::from_secs(120);
    let mut buffer = Vec::with_capacity(64 * 1024);
    let mut chunk = [0_u8; 64 * 1024];
    let response_line = loop {
        if let Some(newline) = buffer.iter().position(|&byte| byte == b'\n') {
            break String::from_utf8_lossy(&buffer[..newline]).into_owned();
        }
        match stream.read(&mut chunk) {
            Ok(0) if buffer.is_empty() => {
                anyhow::bail!("daemon closed connection without response");
            }
            Ok(0) => break String::from_utf8_lossy(&buffer).into_owned(),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= overall_deadline {
                    anyhow::bail!(
                        "timed out after 120s waiting for daemon response (received {} bytes so far)",
                        buffer.len()
                    );
                }
            }
            Err(error) => return Err(error.into()),
        }
    };

    Ok(serde_json::from_str(&response_line)?)
}

#[cfg(not(unix))]
pub fn send_request(socket_path: &str, request: &DaemonRequest) -> anyhow::Result<DaemonResponse> {
    #[cfg(target_os = "windows")]
    {
        use std::io::{BufRead, BufReader, Write};
        use std::time::Duration;

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let pipe = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(socket_path)
            {
                Ok(pipe) => break pipe,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => anyhow::bail!("connect to named pipe {socket_path}: {error}"),
            }
        };

        let mut writer = pipe.try_clone()?;
        writer.write_all((serde_json::to_string(request)? + "\n").as_bytes())?;
        writer.flush()?;

        let response_line = BufReader::new(pipe)
            .lines()
            .next()
            .ok_or_else(|| anyhow::anyhow!("daemon closed connection without response"))??;
        Ok(serde_json::from_str(&response_line)?)
    }
    #[cfg(not(target_os = "windows"))]
    {
        anyhow::bail!("daemon client not supported on this platform (socket path: {socket_path})");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        current_daemon_metadata, socket_path_for_namespace, DaemonClientKind, DaemonRequest,
        DaemonResponse, ToolObservationOrigin,
    };

    #[test]
    fn socket_path_keeps_the_selected_namespace() {
        let path = socket_path_for_namespace("pincer-test");
        assert!(path.contains("pincer-test"));
    }

    #[test]
    fn legacy_request_without_additive_fields_deserializes() {
        let request: DaemonRequest = serde_json::from_value(serde_json::json!({
            "method": "call",
            "name": "get_screen_size",
            "args": {}
        }))
        .unwrap();
        assert_eq!(request.session_id, None);
        assert_eq!(request.observation_origin, None);
        assert_eq!(request.client_kind, None);
    }

    #[test]
    fn direct_client_kind_is_bounded_and_snake_case() {
        let request = DaemonRequest {
            method: "call".into(),
            name: Some("start_session".into()),
            args: None,
            session_id: None,
            observation_origin: Some(ToolObservationOrigin::Direct),
            client_kind: Some(DaemonClientKind::TypescriptSdk),
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["client_kind"], "typescript_sdk");
        assert!(value.get("application_name").is_none());
        assert!(value.get("package_name").is_none());
    }

    #[test]
    fn response_helpers_preserve_wire_shape() {
        let ok = DaemonResponse::ok(serde_json::json!({"value": 1}));
        assert!(ok.ok);
        assert_eq!(ok.result.unwrap()["value"], 1);

        let error = DaemonResponse::err("nope", 64);
        assert!(!error.ok);
        assert_eq!(error.error.as_deref(), Some("nope"));
        assert_eq!(error.exit_code, Some(64));
    }

    #[test]
    fn observation_origin_is_snake_case() {
        assert_eq!(
            serde_json::to_value(ToolObservationOrigin::McpProxy).unwrap(),
            "mcp_proxy"
        );
    }

    /// A fresh directory under the system temp dir with `mode`, removed on drop.
    #[cfg(unix)]
    struct ScratchDir(std::path::PathBuf);

    #[cfg(unix)]
    impl ScratchDir {
        fn new(label: &str, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "pincer-core-daemon-test-{label}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            Self(path)
        }
    }

    #[cfg(unix)]
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run test `name` again in a child copy of this binary with the given
    /// environment changes; HOME and XDG_RUNTIME_DIR are process-wide.
    #[cfg(unix)]
    fn run_path_child(name: &str, set: &[(&str, &std::ffi::OsStr)], remove: &[&str]) {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env("PINCER_CORE_TEST_CHILD", "1");
        for (key, value) in set {
            command.env(key, value);
        }
        for key in remove {
            command.env_remove(key);
        }
        let output = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "child {name} failed\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // DAEMON-5 and CLI-3: with HOME unset the default socket used to fall
    // back to /tmp/.cache/<namespace>, a directory any account can create
    // first. A private XDG_RUNTIME_DIR is used instead.
    #[cfg(unix)]
    #[test]
    fn home_unset_uses_a_private_xdg_runtime_dir() {
        if std::env::var_os("PINCER_CORE_TEST_CHILD").is_none() {
            let runtime = ScratchDir::new("xdg-private", 0o700);
            run_path_child(
                "daemon::tests::home_unset_uses_a_private_xdg_runtime_dir",
                &[("XDG_RUNTIME_DIR", runtime.0.as_os_str())],
                &["HOME"],
            );
            return;
        }
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap();
        assert_eq!(
            socket_path_for_namespace("pincer-test"),
            format!("{runtime}/pincer-test/pincer-test.sock")
        );
    }

    // DAEMON-5 and CLI-11: without HOME and without a private runtime
    // directory there is no safe default, so no shared path may be returned.
    #[cfg(unix)]
    #[test]
    fn home_unset_without_a_private_runtime_dir_has_no_shared_default() {
        if std::env::var_os("PINCER_CORE_TEST_CHILD").is_none() {
            let shared = ScratchDir::new("xdg-shared", 0o777);
            let name =
                "daemon::tests::home_unset_without_a_private_runtime_dir_has_no_shared_default";
            run_path_child(name, &[("XDG_RUNTIME_DIR", shared.0.as_os_str())], &["HOME"]);
            run_path_child(name, &[], &["HOME", "XDG_RUNTIME_DIR"]);
            return;
        }
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        let path = socket_path_for_namespace("pincer-test");
        assert!(!path.starts_with("/tmp/"), "shared default {path}");
        assert!(
            runtime.is_empty() || !path.starts_with(&runtime),
            "default {path} inside a world-writable XDG_RUNTIME_DIR"
        );
        let error = super::try_socket_path_for_namespace("pincer-test")
            .expect_err("no private directory, so no default socket")
            .to_string();
        assert!(error.contains("HOME"), "{error}");
        assert!(error.contains("XDG_RUNTIME_DIR"), "{error}");
        assert!(error.contains("--socket"), "{error}");
    }

    // CLI-3: a client must refuse a daemon socket served by another account
    // before it sends anything, since that account would see every request.
    #[cfg(unix)]
    #[test]
    fn daemon_client_refuses_a_socket_served_by_another_uid() {
        super::check_daemon_peer_uid("/run/user/1000/pincer/pincer.sock", 1000, 1000)
            .expect("same uid is accepted");
        let error =
            super::check_daemon_peer_uid("/tmp/pincer.sock", 1000, 1001).expect_err("foreign uid");
        let message = error.to_string();
        assert!(message.contains("/tmp/pincer.sock"), "{message}");
        assert!(message.contains("1001"), "{message}");
        assert!(message.contains("1000"), "{message}");
    }

    // CLI-3: the client reads the listening process's uid from the connected
    // socket and accepts a daemon run by the same account.
    #[cfg(unix)]
    #[test]
    fn daemon_client_accepts_a_socket_served_by_its_own_uid() {
        let directory = ScratchDir::new("peer", 0o700);
        let socket = directory.0.join("d.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        super::verify_daemon_peer(&stream, socket.to_str().unwrap())
            .expect("a socket served by this account is accepted");
    }

    #[test]
    fn metadata_is_versioned_and_process_bound() {
        let metadata = current_daemon_metadata();
        assert_eq!(metadata.pid, std::process::id());
        assert_eq!(metadata.contract_version, pincer_contract::CONTRACT_VERSION);
        assert_eq!(
            metadata.mcp_protocol_version,
            pincer_contract::MCP_PROTOCOL_VERSION
        );
    }
}
