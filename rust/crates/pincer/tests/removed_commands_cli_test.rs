use std::process::{Command, Output};

fn run(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pincer"))
        .args(args)
        .env("PINCER_HOME", home)
        .env("PINCER_TELEMETRY_ENABLED", "0")
        .output()
        .expect("run pincer")
}

/// Upstream's update commands were removed in Pincer. They must say so and
/// point at rebuilding from the repository, not fall through to the
/// implicit tool call and report that the daemon is not running.
#[test]
fn removed_update_commands_explain_how_to_update() {
    let home = tempfile::tempdir().expect("temp home");
    for (args, command) in [
        (&["update"][..], "update"),
        (&["update", "--apply"], "update"),
        (&["check-update", "--json"], "check-update"),
        (&["channel"], "channel"),
        (&["channel", "set", "nightly"], "channel"),
        (&["--socket", "/nonexistent.sock", "update"], "update"),
    ] {
        let output = run(home.path(), args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(64), "{args:?}: {stderr}");
        assert!(output.stdout.is_empty(), "{args:?} wrote to stdout");
        assert!(
            stderr.contains(&format!(
                "pincer {command}: this command was removed in Pincer"
            )),
            "{args:?}: {stderr}"
        );
        assert!(
            stderr.contains("pull the repository and rebuild it"),
            "{args:?}: {stderr}"
        );
        assert!(
            stderr.contains("https://github.com/ModCritic/pincer#build-from-source"),
            "{args:?}: {stderr}"
        );
        assert!(
            !stderr.contains("daemon is not running"),
            "{args:?}: {stderr}"
        );
    }
}

/// `skills update` is a different command and still runs.
#[test]
fn skills_update_is_not_a_removed_command() {
    let home = tempfile::tempdir().expect("temp home");
    let output = run(home.path(), &["skills", "update", "--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(!stdout.contains("removed in Pincer"), "{stdout}");
}
