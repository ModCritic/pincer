//! A flag Pincer does not know is refused with a usage line and exit code 64
//! (the usage-error code in `pincer --help`'s exit codes), instead of being
//! dropped while the command runs as if it had not been given.

use std::process::{Command, Output};

use pincer_testkit::IsolatedStateRoot;

fn run(state: &IsolatedStateRoot, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pincer"))
        .args(args)
        .envs(state.env())
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("PINCER_TELEMETRY_ENABLED", "0")
        .output()
        .expect("run pincer")
}

#[test]
fn unknown_flags_are_refused_with_a_usage_line() {
    let state = IsolatedStateRoot::new().expect("isolated state root");
    for (args, named) in [
        (&["manifest", "--prety"][..], "--prety"),
        (&["--no-overaly", "manifest"][..], "--no-overaly"),
        (&["dump-docs", "--type", "cli", "--bogus=1"][..], "--bogus"),
        (&["manifest", "--pretty=yes"][..], "--pretty"),
        (&["manifest", "--socket"][..], "--socket"),
        (&["manifest", "-x"][..], "-x"),
    ] {
        let output = run(&state, args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(64),
            "{args:?} was not refused: {stderr}"
        );
        assert!(stderr.contains(named), "{args:?}: {stderr}");
        assert!(stderr.contains("Usage: pincer"), "{args:?}: {stderr}");
        assert!(
            output.stdout.is_empty(),
            "{args:?} still ran: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn documented_flags_still_work() {
    let state = IsolatedStateRoot::new().expect("isolated state root");
    for args in [
        &["manifest", "--pretty"][..],
        &["manifest", "-p"][..],
        &[
            "--no-overlay",
            "--cursor-theme",
            "pincer.default",
            "manifest",
        ][..],
        &["--glide-ms", "-1", "manifest"][..],
        &["dump-docs", "--type=cli"][..],
        &["mcp-config", "--client", "claude"][..],
        &["--version"][..],
        &["mcp", "--help"][..],
    ] {
        let output = run(&state, args);
        assert!(
            output.status.success(),
            "{args:?} exited {:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
