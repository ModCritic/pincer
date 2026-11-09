use super::*;
use std::os::unix::process::ExitStatusExt;

fn exit(code: i32, after_ms: u64) -> LaunchExit {
    LaunchExit {
        status: ExitStatus::from_raw(code << 8),
        after: Duration::from_millis(after_ms),
    }
}

fn killed(signal: i32) -> LaunchExit {
    LaunchExit {
        status: ExitStatus::from_raw(signal),
        after: Duration::from_millis(50),
    }
}

fn window(pid: u32, class: &str) -> crate::x11::WindowInfo {
    crate::x11::WindowInfo {
        xid: 42,
        pid: Some(pid),
        app_name: class.to_owned(),
        title: "Calculator".to_owned(),
        is_on_screen: true,
        z_index: None,
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    }
}

/// An executable shell script with `body`.
fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn an_exit_is_described_by_its_code_or_signal() {
    assert_eq!(exit(127, 40).describe(), "exited with code 127");
    assert_eq!(exit(0, 1000).describe(), "exited with code 0");
    assert_eq!(
        killed(libc::SIGKILL).describe(),
        "was killed by signal 9 (SIGKILL)"
    );
}

#[test]
fn the_launch_program_skips_an_env_prefix() {
    assert_eq!(launch_program("/usr/bin/sleep 1"), "sleep");
    assert_eq!(
        launch_program("env PROBE_LOG=/tmp/x.log /opt/app/PincerTestHarness.Qt6"),
        "PincerTestHarness.Qt6"
    );
    assert_eq!(
        launch_program("/usr/bin/env -u FOO A=1 xdg-open /tmp"),
        "xdg-open"
    );
    assert_eq!(
        launch_program("gtk-launch org.gnome.Calculator"),
        "gtk-launch"
    );
    assert_eq!(launch_program(""), "");
}

#[test]
fn only_a_clean_exit_with_evidence_is_a_handoff() {
    let other = [window(900, "gnome-calculator")];
    // A plain program that exits, cleanly or not: no hand-off.
    assert!(handoff_evidence("sleep", Some(&exit(0, 1000)), &[]).is_none());
    assert!(handoff_evidence("TestApp-x86_64.AppImage", Some(&exit(127, 40)), &[]).is_none());
    // A failed exit is never a hand-off, even from a launcher or with a
    // window of the app around.
    assert!(handoff_evidence("xdg-open", Some(&exit(4, 100)), &[]).is_none());
    assert!(handoff_evidence("gnome-calculator", Some(&exit(1, 100)), &other).is_none());
    assert!(handoff_evidence("xdg-open", Some(&killed(libc::SIGSEGV)), &[]).is_none());
    // No recorded exit: no evidence.
    assert!(handoff_evidence("xdg-open", None, &[]).is_none());
    // A known launcher that exited cleanly.
    match handoff_evidence("xdg-open", Some(&exit(0, 300)), &[]) {
        Some(Handoff::Launcher(program)) => assert_eq!(program, "xdg-open"),
        other => panic!("expected a launcher hand-off, got {other:?}"),
    }
    // A clean exit while another process owns a window of the app.
    match handoff_evidence("gnome-calculator", Some(&exit(0, 300)), &other) {
        Some(Handoff::RunningInstance { pid, windows }) => {
            assert_eq!(pid, 900);
            assert_eq!(windows.len(), 1);
        }
        other => panic!("expected a running-instance hand-off, got {other:?}"),
    }
}

#[test]
fn the_output_tail_keeps_the_last_whole_lines() {
    assert_eq!(output_tail_text(b"", false), "");
    assert_eq!(
        output_tail_text(b"Error: one\n\n  \nError: two\n", false),
        "Error: one\nError: two"
    );
    // A cut stream drops its partial first line.
    assert_eq!(
        output_tail_text(b"ial line\nwhole line\n", true),
        "whole line"
    );
    let many: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    let tail = output_tail_text(many.as_bytes(), false);
    assert!(tail.starts_with("line 9\n"), "{tail}");
    assert!(tail.ends_with("line 20"), "{tail}");
    assert_eq!(
        output_tail_text(b"bad \xff byte\n", false),
        "bad \u{fffd} byte"
    );
}

#[test]
fn a_child_that_exits_is_recorded_with_its_code_and_output() {
    let dir = tempfile::tempdir().unwrap();
    // Like the AppImage runtime: the main error on stdout, a detail on stderr.
    let path = script(
        dir.path(),
        "fails.sh",
        "echo 'Error: No suitable fusermount binary found on the $PATH'\n\
         echo 'Error: $FUSERMOUNT_PROG not set' >&2\nexit 127",
    );
    let capture_dir = dir.path().join("cache");
    let child = spawn_watched_in(Command::new(&path), Some(&capture_dir)).unwrap();
    assert!(child.output_captured());
    let exit = child
        .exit_within(Duration::from_secs(10))
        .expect("the reaper records the exit");
    assert_eq!(exit.status.code(), Some(127));
    assert!(exit.after < Duration::from_secs(10));
    assert_eq!(
        child.output_tail(),
        "Error: No suitable fusermount binary found on the $PATH\n\
         Error: $FUSERMOUNT_PROG not set"
    );
    // The capture leaves no file behind in its directory.
    assert_eq!(std::fs::read_dir(&capture_dir).unwrap().count(), 0);
}

#[test]
fn a_silent_child_has_an_empty_tail_and_without_a_directory_nothing_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let quiet = script(dir.path(), "quiet.sh", "exit 0");
    let child = spawn_watched_in(Command::new(&quiet), Some(&dir.path().join("cache"))).unwrap();
    let exit = child.exit_within(Duration::from_secs(10)).unwrap();
    assert!(exit.status.success());
    assert_eq!(child.output_tail(), "");

    let loud = script(dir.path(), "loud.sh", "echo lost; echo lost >&2; exit 2");
    let child = spawn_watched_in(Command::new(&loud), None).unwrap();
    assert!(!child.output_captured());
    assert_eq!(
        child
            .exit_within(Duration::from_secs(10))
            .unwrap()
            .status
            .code(),
        Some(2)
    );
    assert_eq!(child.output_tail(), "");
}

#[test]
fn a_capture_over_its_cap_is_emptied_and_appends_again() {
    let dir = tempfile::tempdir().unwrap();
    let capture = OutputCapture::create(dir.path()).unwrap();
    let writer = capture.file.try_clone().unwrap();
    use std::io::Write;
    (&writer)
        .write_all(&vec![b'x'; OUTPUT_CAP_BYTES as usize + 1])
        .unwrap();
    capture.cap();
    assert_eq!(capture.file.metadata().unwrap().len(), 0);
    (&writer).write_all(b"after the cap\n").unwrap();
    assert_eq!(capture.file.metadata().unwrap().len(), 14);
    assert_eq!(capture.tail(), "after the cap");
}
