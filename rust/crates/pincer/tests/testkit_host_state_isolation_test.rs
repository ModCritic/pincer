//! Regression coverage for #4094: testkit-owned daemons must not inherit the
//! developer's installed-product state.
//!
//! A maintainer machine with Computer History admitted keeps
//! `{"history_preview_admitted":true}` in the per-user history root. A
//! source-built daemon that reads it requests admission, fails installed-app
//! verification, and exits before binding its socket, so every testkit
//! transport silently becomes unavailable.
//!
//! The test body runs in a child copy of this test binary whose `HOME` (and
//! the platform state variables) point at a temporary directory from its
//! start, so no process changes its own environment while threads read it.
//! The real user's files are never touched.

use std::path::{Path, PathBuf};
use std::process::Command;

use pincer_testkit::{
    driver_binary, ensure_driver_binary, CliDriver, McpDriver, RawDriver, SHARE_HOST_STATE,
};

/// Every per-user history root the driver may resolve for `home`.
fn history_roots(home: &Path) -> Vec<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        home.join("AppData").join("Local")
    } else if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        home.join(".local").join("state")
    };
    let mut roots: Vec<_> = ["pincer", "pincer-local"]
        .into_iter()
        .map(|namespace| base.join(namespace).join("computer-history"))
        .collect();
    roots.push(home.join(".pincer").join("computer-history"));
    roots
}

/// Set in the child copy of the test binary that runs the test body.
const CHILD_ENV: &str = "PINCER_HOST_STATE_CHILD";

fn seed_admitted_host_home() -> tempfile::TempDir {
    let home = tempfile::Builder::new()
        .prefix("pincer host home-")
        .tempdir()
        .expect("temporary host home");
    for root in history_roots(home.path()) {
        std::fs::create_dir_all(&root).expect("history root");
        std::fs::write(
            root.join("admission.json"),
            br#"{"history_preview_admitted":true}"#,
        )
        .expect("seed admission preference");
    }
    home
}

/// The environment of a user whose home is `home`.
fn host_home_env(home: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut env = vec![
        ("HOME", home.to_path_buf()),
        ("PINCER_HOME", home.join(".pincer")),
    ];
    if cfg!(target_os = "windows") {
        env.push(("USERPROFILE", home.to_path_buf()));
        env.push(("APPDATA", home.join("AppData").join("Roaming")));
        env.push(("LOCALAPPDATA", home.join("AppData").join("Local")));
    } else if !cfg!(target_os = "macos") {
        env.push(("XDG_STATE_HOME", home.join(".local").join("state")));
    }
    env
}

/// Run the test body in a child copy of this binary started with the seeded
/// host home's environment, and check that it passed.
fn run_in_host_home_child(home: &Path) {
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "testkit_daemons_ignore_host_history_admission",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, home)
        .envs(host_home_env(home))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the child test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "child test failed ({}):\n--- stdout\n{stdout}\n--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_admission_hint(driver_home: Option<&Path>, admitted: bool) {
    #[cfg(windows)]
    let socket = format!(r"\\.\pipe\pincer-history-missing-{}", uuid::Uuid::new_v4());
    #[cfg(unix)]
    let socket = format!("/tmp/pincer-history-missing-{}.sock", uuid::Uuid::new_v4());

    let mut command = Command::new(driver_binary());
    command
        .args(["history", "status", "--socket", &socket])
        .env("PINCER_TELEMETRY_ENABLED", "false");
    match driver_home {
        Some(path) => command.env("PINCER_HOME", path),
        None => command.env_remove("PINCER_HOME"),
    };
    let output = command.output().expect("history status command");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("Pincer daemon is not running."), "{stderr}");
    assert_eq!(
        stderr.contains(" --experimental-history"),
        admitted,
        "{stderr}"
    );
}

#[test]
fn testkit_daemons_ignore_host_history_admission() {
    if !ensure_driver_binary(&driver_binary()) {
        return;
    }
    let Some(host_home) = std::env::var_os(CHILD_ENV).map(PathBuf::from) else {
        let host_home = seed_admitted_host_home();
        run_in_host_home_child(host_home.path());
        return;
    };
    let host_home = host_home.as_path();
    assert_eq!(std::env::var_os("HOME").as_deref(), Some(host_home.as_os_str()));

    assert_admission_hint(None, true);
    assert_admission_hint(Some(Path::new("")), true);
    let empty_home = tempfile::tempdir().expect("empty driver home");
    assert_admission_hint(Some(empty_home.path()), false);
    assert_admission_hint(Some(&host_home.join(".pincer")), true);

    // Control: sharing the seeded host state reproduces the original failure,
    // proving the seed is the state that used to leak into test daemons.
    assert!(
        McpDriver::spawn_with_env(&[SHARE_HOST_STATE]).is_none(),
        "a daemon sharing the admitted host state is expected to refuse to start; \
         if product admission behavior changed, update this control"
    );

    let mcp =
        McpDriver::spawn().expect("MCP testkit daemon must start despite host history admission");
    let mcp_root = mcp.state_root().expect("MCP daemon uses an isolated root");
    assert!(!mcp_root.starts_with(host_home));

    let raw =
        RawDriver::spawn().expect("raw testkit daemon must start despite host history admission");
    let raw_root = raw.state_root().expect("raw daemon uses an isolated root");
    assert_ne!(raw_root, mcp_root, "each daemon owns its own state root");

    let cli = CliDriver::new();
    assert!(
        cli.available(),
        "CLI testkit daemon must start despite host history admission"
    );
    assert!(cli.state_root().is_some());

    // The seeded host preference is still intact: isolation is read-side only.
    for root in history_roots(host_home) {
        assert_eq!(
            std::fs::read(root.join("admission.json")).unwrap(),
            br#"{"history_preview_admitted":true}"#
        );
    }
}
