//! What launch_app attests for the `process_launch` adapter, and that the
//! launch runs exactly that (LXTOOLS-1, -11, -12).

use super::*;
use crate::installed_apps::InstalledApp;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

struct InstalledFixture;

impl Drop for InstalledFixture {
    fn drop(&mut self) {
        TEST_INSTALLED_APPS.with(|apps| apps.borrow_mut().take());
    }
}

/// Resolve launches on this thread against `apps` instead of the machine's
/// own .desktop entries.
fn installed(apps: Vec<InstalledApp>) -> InstalledFixture {
    TEST_INSTALLED_APPS.with(|slot| *slot.borrow_mut() = Some(apps));
    InstalledFixture
}

fn app(name: &str, bundle_id: &str, launch_path: &str) -> InstalledApp {
    InstalledApp {
        name: name.to_owned(),
        bundle_id: bundle_id.to_owned(),
        launch_path: launch_path.to_owned(),
        last_used: None,
        startup_wm_class: None,
    }
}

/// An executable shell script `name` in `dir`, by its canonical path.
fn program(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::canonicalize(&path)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn canonical(path: &str) -> String {
    std::fs::canonicalize(path)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

async fn scope(args: Value) -> Result<Option<Value>, String> {
    LaunchAppTool
        .protected_resource_scope("process_launch", &args)
        .await
}

fn expected(
    executable: &str,
    desktop_entry: Option<&str>,
    installed_app: bool,
    options: bool,
    argument_count: usize,
) -> Value {
    json!({
        "kind": "program_launch",
        "executable": executable,
        "installed_app": installed_app,
        "desktop_entry": desktop_entry,
        "options": options,
        "argument_count": argument_count,
    })
}

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            pincer_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Set in the child copy of the test binary that [`in_child_with_env`] runs.
const CHILD_ENV: &str = "PINCER_LAUNCH_SCOPE_CHILD";

/// Whether this process should run the body of test `name` (in this module).
/// A launch passes on Pincer's own environment, so these tests need
/// variables in it; changing this process's environment while other tests
/// run (and read it, from Rust or C) is a race. Outside the child copy this
/// runs `name` in a child copy of the test binary with `env` added from its
/// start, checks that it passed, and answers false.
fn in_child_with_env(name: &str, env: &[(&str, &std::ffi::OsStr)]) -> bool {
    if std::env::var_os(CHILD_ENV).is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            &format!("tools::impl_::launch_scope_tests::{name}"),
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .envs(env.iter().copied())
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
    false
}

fn wait_for(path: &Path) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Let the writer finish the file it just created.
    std::thread::sleep(std::time::Duration::from_millis(100));
    path.exists()
}

// The user's harness launches installed apps by name, by desktop-file id
// and by the exact launch_path list_apps returned. Those resolve to the
// entry, so standard mode runs them.
#[tokio::test]
async fn an_installed_app_by_name_bundle_id_or_its_launch_path_attests_its_entry() {
    let dir = tempfile::tempdir().unwrap();
    let kwrite = program(dir.path(), "kwrite", "exit 0");
    let _apps = installed(vec![app("KWrite", "org.kde.kwrite", &kwrite)]);
    for args in [
        json!({"name": "KWrite", "urls": ["/home/u/notes.txt"]}),
        json!({"name": "kwrite", "urls": ["/home/u/notes.txt"]}),
        json!({"bundle_id": "org.kde.kwrite", "urls": ["/home/u/notes.txt"]}),
        json!({"launch_path": kwrite, "urls": ["/home/u/notes.txt"]}),
    ] {
        assert_eq!(
            scope(args.clone()).await,
            Ok(Some(expected(&kwrite, Some("org.kde.kwrite"), true, false, 1))),
            "{args}"
        );
    }
}

// LXTOOLS-1: `name` ran any program on PATH before the .desktop lookup, so
// {"name":"sh","additional_arguments":["-c", ...]} ran a shell command.
#[tokio::test]
async fn a_name_resolves_only_through_installed_desktop_entries() {
    let _apps = installed(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let args = json!({
        "name": "sh",
        "additional_arguments": ["-c", format!("touch '{}'", marker.display())],
    });
    let refusal = scope(args.clone()).await.unwrap_err();
    assert!(refusal.contains("launch_path"), "{refusal}");

    // An attested launch (standard or bounded mode) carries the scope the
    // registry admitted; with one, a name never runs from PATH.
    let mut attested = args;
    attested["_protected_launch_scope"] = json!({"kind": "program_launch"});
    let result = LaunchAppTool.invoke(attested).await;
    assert_eq!(result.is_error, Some(true), "{result:?}");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!marker.exists(), "an attested name must never run a program from PATH");
}

#[tokio::test]
async fn a_raw_program_needs_launch_path_and_is_not_an_installed_app() {
    let dir = tempfile::tempdir().unwrap();
    let kwrite = program(dir.path(), "kwrite", "exit 0");
    let _apps = installed(vec![app("KWrite", "org.kde.kwrite", &kwrite)]);
    assert_eq!(
        scope(json!({"launch_path": "/bin/sh", "additional_arguments": ["-c", "id"]})).await,
        Ok(Some(expected(&canonical("/bin/sh"), None, false, true, 2)))
    );
    assert_eq!(
        scope(json!({"launch_path": "sh"})).await,
        Ok(Some(expected(&canonical("/bin/sh"), None, false, false, 0)))
    );
    // A launch_path that is not exactly an entry's own counts its extra
    // words as caller arguments.
    assert_eq!(
        scope(json!({"launch_path": format!("{kwrite} --new-instance")})).await,
        Ok(Some(expected(&kwrite, None, false, true, 1)))
    );
}

#[tokio::test]
async fn options_are_dash_arguments_or_any_argument_to_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let kwrite = program(dir.path(), "kwrite", "exit 0");
    let kitty = program(dir.path(), "kitty", "exit 0");
    let foot = program(dir.path(), "foot", "exit 0");
    let _apps = installed(vec![
        app("KWrite", "org.kde.kwrite", &kwrite),
        app("kitty", "kitty", &kitty),
        app("Foot", "org.codeberg.dnkl.foot", &foot),
    ]);
    for (args, options) in [
        (json!({"name": "KWrite"}), false),
        (json!({"name": "KWrite", "urls": ["/tmp/a.txt"]}), false),
        (json!({"name": "KWrite", "additional_arguments": ["--new-instance"]}), true),
        (json!({"name": "KWrite", "urls": ["-e"]}), true),
        (json!({"name": "kitty"}), false),
        (json!({"name": "kitty", "urls": ["sh"]}), true),
        (json!({"name": "Foot", "additional_arguments": ["python3"]}), true),
    ] {
        let attested = scope(args.clone()).await.unwrap().unwrap();
        assert_eq!(attested["options"], options, "{args}: {attested}");
        assert_eq!(attested["installed_app"], true, "{args}: {attested}");
    }
}

// LXTOOLS-1: dispatch must start what the registry admitted, not whatever
// the arguments resolve to by the time the tool runs.
#[tokio::test]
async fn the_launch_runs_exactly_the_attested_program() {
    let _apps = installed(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let result = LaunchAppTool
        .invoke(json!({
            "launch_path": "/bin/sh",
            "additional_arguments": ["-c", format!("touch '{}'", marker.display())],
            "_protected_launch_scope": expected("/usr/bin/kwrite", Some("org.kde.kwrite"), true, false, 0),
        }))
        .await;
    assert_eq!(result.is_error, Some(true), "{result:?}");
    assert!(text_of(&result).contains("changed"), "{}", text_of(&result));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!marker.exists());

    // The program runs from its canonical path with its own argv[0].
    let cmdline = dir.path().join("cmdline");
    let args = json!({
        "launch_path": "sh",
        "additional_arguments": ["-c", format!("cat /proc/$$/cmdline > '{}'", cmdline.display())],
    });
    let approved = scope(args.clone()).await.unwrap().unwrap();
    let mut admitted = args;
    admitted["_protected_launch_scope"] = approved;
    let _ = LaunchAppTool.invoke(admitted).await;
    assert!(wait_for(&cmdline));
    let argv = std::fs::read(&cmdline).unwrap();
    assert!(argv.starts_with(b"sh\0-c\0"), "{:?}", String::from_utf8_lossy(&argv));
}

// LXTOOLS-11: a launched app inherited the daemon's PINCER_* permission,
// manifest and grant variables, so a Pincer started inside it ran with
// them.
#[tokio::test]
async fn launched_programs_do_not_inherit_pincer_variables() {
    let variable = "PINCER_TEST_LAUNCH_INHERITS";
    if !in_child_with_env(
        "launched_programs_do_not_inherit_pincer_variables",
        &[(variable, "leaked".as_ref())],
    ) {
        return;
    }
    assert!(std::env::var_os(variable).is_some());
    let _apps = installed(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("env");
    let _ = LaunchAppTool
        .invoke(json!({
            "launch_path": "/bin/sh",
            "additional_arguments": ["-c", format!("env > '{}.tmp' && mv '{0}.tmp' '{0}'", marker.display())],
        }))
        .await;
    assert!(wait_for(&marker));
    let env = std::fs::read_to_string(&marker).unwrap();
    assert!(!env.contains(variable), "{env}");
    assert!(env.contains("ACCESSIBILITY_ENABLED=1"), "{env}");
    // Firefox and Thunderbird switch their accessibility on only for this
    // variable (or the session-wide status); without it a launched Firefox
    // had no tree.
    assert!(env.contains("GNOME_ACCESSIBILITY=1"), "{env}");
}

// LXTOOLS-12: xdg-open inherited the driver's stdin, stdout and stderr (in
// private-worker mode stdout is the JSON-RPC stream) and the PINCER_*
// variables. Urls alone attest xdg-open; a target that is a .desktop file
// or an executable is an option, since opening it can run a program.
//
// This test puts a fake xdg-open first on PATH, so every xdg-open case
// lives in this one test.
#[tokio::test]
async fn urls_alone_open_through_xdg_open_with_private_stdio() {
    const DIR_ENV: &str = "PINCER_TEST_XDG_DIR";
    let variable = "PINCER_TEST_XDG_INHERITS";
    if std::env::var_os(CHILD_ENV).is_none() {
        // The fake xdg-open's folder goes first on the child's PATH.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let mut path = std::ffi::OsString::from(&bin);
        if let Some(original) = std::env::var_os("PATH") {
            path.push(":");
            path.push(original);
        }
        in_child_with_env(
            "urls_alone_open_through_xdg_open_with_private_stdio",
            &[
                ("PATH", &path),
                (DIR_ENV, dir.path().as_os_str()),
                (variable, "leaked".as_ref()),
            ],
        );
        return;
    }
    assert!(std::env::var_os(variable).is_some());
    let _apps = installed(Vec::new());
    let dir = std::path::PathBuf::from(std::env::var_os(DIR_ENV).expect("the parent's folder"));
    let bin = dir.join("bin");
    let report = dir.join("report");
    let fake = program(
        &bin,
        "xdg-open",
        &format!(
            "in0=$(readlink /proc/$$/fd/0); in1=$(readlink /proc/$$/fd/1); \
             in2=$(readlink /proc/$$/fd/2)\n\
             {{ printf '%s\\n%s\\n%s\\n' \"$in0\" \"$in1\" \"$in2\"; env; }} \
             > '{report}.tmp' && mv '{report}.tmp' '{report}'\n\
             echo 'stdout noise from xdg-open'\necho 'stderr noise' >&2",
            report = report.display()
        ),
    );

    let text = dir.join("notes.txt");
    std::fs::write(&text, "notes").unwrap();
    let entry = dir.join("evil.desktop");
    std::fs::write(&entry, "[Desktop Entry]\nType=Application\nExec=sh\n").unwrap();
    let script = program(&dir, "run.sh", "exit 0");
    let text = text.to_string_lossy().into_owned();
    let entry = entry.to_string_lossy().into_owned();
    let result = LaunchAppTool.invoke(json!({"urls": [text.clone()]})).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert!(wait_for(&report));
    let seen = std::fs::read_to_string(&report).unwrap();
    let mut lines = seen.lines();
    assert_eq!(lines.next(), Some("/dev/null"), "{seen}");
    let own = |fd: u32| std::fs::read_link(format!("/proc/self/fd/{fd}")).ok();
    for fd in [1, 2] {
        let child = lines.next().map(std::path::PathBuf::from);
        assert!(child.is_some(), "{seen}");
        assert_ne!(child, own(fd), "xdg-open fd {fd} is the driver's own: {seen}");
    }
    assert!(!seen.contains(variable), "{seen}");

    for (urls, options) in [
        (vec![text.clone()], false),
        (vec!["https://example.com/a".to_owned()], false),
        (vec![text.clone(), entry.clone()], true),
        (vec![format!("file://{entry}")], true),
        (vec![script.clone()], true),
        (vec!["-x".to_owned()], true),
    ] {
        let count = urls.len();
        assert_eq!(
            scope(json!({"urls": urls})).await,
            Ok(Some(expected(&fake, None, true, options, count))),
            "{urls:?}"
        );
    }
    assert_eq!(
        scope(json!({"name": "https://example.com/a"})).await,
        Ok(Some(expected(&fake, None, true, false, 1)))
    );
}

/// Unrestricted mode without a manifest skips launch attestation, so the
/// tool gets no `_protected_launch_scope`. There a `name` that is no
/// installed app runs from PATH, as it did before the process_launch
/// adapter. Attestation (standard and bounded mode) still refuses it.
#[tokio::test]
async fn an_unattested_launch_runs_a_name_from_path_and_attestation_still_refuses_it() {
    let _apps = installed(Vec::new());
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("ran");
    let args = json!({
        "name": "sh",
        "additional_arguments": ["-c", format!("touch '{}'", marker.display())],
    });
    assert!(
        LaunchAppTool
            .protected_resource_scope("process_launch", &args)
            .await
            .is_err(),
        "attestation must still refuse a name that is not an installed app"
    );
    let _ = LaunchAppTool.invoke(args).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(marker.exists(), "an unattested launch should run `sh` from PATH");
}
