//! A finite CLI command whose reader closes early (`cua-driver ... | head`)
//! must stop like a normal pipeline stage instead of panicking with
//! `failed printing to stdout: Broken pipe`.

use std::process::{Command, ExitStatus, Stdio};
use tempfile::TempDir;

/// Exit code Windows uses for a broken pipe, the status a shell shows for a
/// SIGPIPE death.
#[cfg(windows)]
const BROKEN_PIPE_EXIT_CODE: i32 = 141;

fn run_with_closed_stdout() -> (ExitStatus, String) {
    let home = TempDir::new().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cua-driver"));
    // `dump-docs` is desktop-free and prints far more than a pipe buffer, so
    // the write fails even if the reader closes after the first chunk.
    command
        .args(["dump-docs", "--pretty"])
        .env("CUA_DRIVER_RS_HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn cua-driver");
    drop(child.stdout.take());
    let output = child.wait_with_output().expect("wait for cua-driver");
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn assert_quiet(stderr: &str) {
    assert!(
        !stderr.contains("panicked") && !stderr.contains("failed printing"),
        "closed stdout must not panic:\n{stderr}"
    );
}

#[test]
fn finite_command_stops_cleanly_when_its_reader_closes() {
    let (status, stderr) = run_with_closed_stdout();
    assert_quiet(&stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGPIPE), "{status:?}");
    }
    #[cfg(windows)]
    assert_eq!(status.code(), Some(BROKEN_PIPE_EXIT_CODE), "{status:?}");
}
