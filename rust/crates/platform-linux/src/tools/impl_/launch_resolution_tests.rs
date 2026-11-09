use super::*;

fn window(class: &str) -> crate::x11::WindowInfo {
    crate::x11::WindowInfo {
        xid: 1,
        pid: Some(7),
        app_name: class.into(),
        title: "t".into(),
        is_on_screen: true,
        z_index: None,
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    }
}

#[test]
fn launch_queries_match_window_classes() {
    assert!(window_matches_launch(
        &window("org.gnome.Nautilus"),
        "nautilus"
    ));
    assert!(window_matches_launch(
        &window("org.gnome.Nautilus"),
        "org.gnome.Nautilus"
    ));
    assert!(window_matches_launch(
        &window("Gnome-terminal"),
        "gnome-terminal"
    ));
    assert!(window_matches_launch(
        &window("Gnome-control-center"),
        "gnome-control-center"
    ));
    assert!(window_matches_launch(
        &window("Org.gnome.Nautilus"),
        "org.gnome.Nautilus.desktop"
    ));
    assert!(window_matches_launch(
        &window("Org.gnome.Nautilus"),
        "Files\u{0}org.gnome.Nautilus\u{0}nautilus --new-window"
    ));
    assert!(window_matches_launch(
        &window("Gnome-control-center"),
        "Settings\u{0}Settings\u{0}gnome-control-center"
    ));
    assert!(!window_matches_launch(&window("Gedit"), "nautilus"));
    assert!(!window_matches_launch(&window(""), "nautilus"));
}

#[test]
fn a_script_is_not_an_instance_of_the_desktop_shell() {
    // A plain script that opens no window, launched with Plasma's desktop
    // running: the result named pid 647, window "Desktop @ QRect(0,0
    // 2560x1440)", as the app the script handed off to, because the script's
    // name ended in ".sh" and "plasmashell" contains "sh".
    let mut desktop = window("plasmashell");
    desktop.pid = Some(647);
    desktop.title = "Desktop @ QRect(0,0 2560x1440) ".into();
    let script = "/tmp/scripts/hi.sh";
    assert!(!window_is_launched_app(&desktop, script));
    assert!(!window_matches_launch(&desktop, script));
    for class in [
        "gnome-shell",
        "fish",
        "Konsole",
        "firefox",
        "org.kde.dolphin",
    ] {
        assert!(!window_is_launched_app(&window(class), script), "{class}");
        assert!(!window_matches_launch(&window(class), script), "{class}");
    }
    assert!(!window_is_launched_app(&desktop, "/usr/bin/sleep"));
    assert!(!window_is_launched_app(&window("Gedit"), "nautilus"));
    // A wrapper's new window is still adopted by the loose match
    // (libreoffice runs soffice.bin), but a window of another class is not
    // evidence of a running instance.
    assert!(window_matches_launch(
        &window("libreoffice-writer"),
        "libreoffice"
    ));
    assert!(!window_is_launched_app(
        &window("libreoffice-writer"),
        "libreoffice"
    ));
    assert!(!window_is_launched_app(&window("plasmashell"), "plasma"));
}

#[test]
fn a_running_instance_is_a_window_of_the_launched_program() {
    let names = |class: &str, query: &str| window_is_launched_app(&window(class), query);
    assert!(names("gnome-calculator", "gnome-calculator"));
    assert!(names("Gnome-calculator", "/usr/bin/gnome-calculator"));
    assert!(names("org.gnome.Nautilus", "nautilus"));
    assert!(names(
        "org.gnome.Nautilus",
        "Files\u{0}org.gnome.Nautilus\u{0}nautilus --new-window"
    ));
    assert!(names(
        "firefox",
        "env MOZ_ENABLE_WAYLAND=0 /usr/lib/firefox/firefox --new-window"
    ));
    assert!(names("kate", "Kate\u{0}org.kde.kate\u{0}kate -b"));
    assert!(names("Code", "code"));
    assert!(names(
        "org.gnome.TextEditor",
        "Text Editor\u{0}org.gnome.TextEditor\u{0}gnome-text-editor"
    ));
    // Whole names only: no part of an app id, a display name's first word,
    // or an id's vendor prefix.
    assert!(!names("org.gnome.Nautilus", "naut"));
    assert!(!names(
        "text",
        "Text Editor\u{0}org.gnome.TextEditor\u{0}gnome-text-editor"
    ));
    assert!(!names("org.gnome", "org.gnome.Nautilus"));
    assert!(!names("", "nautilus"));
}

#[test]
fn only_an_app_id_has_a_last_part() {
    assert_eq!(app_id_last("org.gnome.Nautilus"), Some("Nautilus"));
    assert_eq!(app_id_last("hi.sh"), None);
    assert_eq!(app_id_last("soffice.bin"), None);
    assert_eq!(app_id_last("nautilus"), None);
    assert_eq!(app_id_last("/opt/a.b.c"), None);
    assert_eq!(app_id_last("org.gnome.x --flag"), None);
}

#[test]
fn a_missing_process_counts_as_exited() {
    assert!(process_exited(u32::MAX - 1));
    assert!(!process_exited(std::process::id()));
}

fn clean_exit(after_ms: u64) -> LaunchExit {
    use std::os::unix::process::ExitStatusExt;
    LaunchExit {
        status: std::process::ExitStatus::from_raw(0),
        after: std::time::Duration::from_millis(after_ms),
    }
}

fn failed_exit(code: i32, after_ms: u64) -> LaunchExit {
    use std::os::unix::process::ExitStatusExt;
    LaunchExit {
        status: std::process::ExitStatus::from_raw(code << 8),
        after: std::time::Duration::from_millis(after_ms),
    }
}

fn result_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            pincer_core::protocol::Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_handoff_timeout_is_a_refusal_not_a_null_pid_success() {
    let result = launch_handoff_timeout_result(
        "Nautilus",
        4242,
        8,
        Some(&clean_exit(300)),
        &launch_child::Handoff::Launcher("gapplication".into()),
    );
    assert_eq!(
        result.is_error,
        Some(true),
        "a hand-off timeout must be reported as an error, not success"
    );
    let s = result.structured_content.unwrap();
    assert_eq!(s["code"], "launch_handoff_timeout");
    assert_eq!(s["effect"], "refused");
    assert_eq!(s["pid"], Value::Null);
    assert_eq!(s["running"], Value::Null);
    assert_eq!(s["launcher_pid"], 4242);
    assert_eq!(s["waited_secs"], 8);
}

#[test]
fn a_running_instance_handoff_names_the_running_pid() {
    let mut window = window("gnome-calculator");
    window.pid = Some(900);
    window.xid = 77;
    let result = launch_handoff_timeout_result(
        "Calculator",
        4242,
        8,
        Some(&clean_exit(300)),
        &launch_child::Handoff::RunningInstance {
            pid: 900,
            windows: vec![window],
        },
    );
    let text = result_text(&result);
    assert!(text.contains("already running as pid 900"), "{text}");
    assert!(!text.contains("D-Bus activation"), "{text}");
    let s = result.structured_content.unwrap();
    assert_eq!(s["handoff"], "running_instance");
    assert_eq!(s["running_instance"]["pid"], 900);
    assert_eq!(s["exit_code"], 0);
}

#[test]
fn a_process_that_exits_without_a_window_is_reported_as_it_ended() {
    let result = launch_exited_result("/usr/bin/sleep 1", 4242, Some(&clean_exit(1003)), "", true);
    assert_eq!(result.is_error, Some(true));
    let text = result_text(&result);
    assert!(
        text.contains(
            "'/usr/bin/sleep 1' (pid 4242) exited with code 0 after 1.0 s without opening a window"
        ),
        "{text}"
    );
    assert!(text.contains("It printed nothing."), "{text}");
    assert!(!text.contains("D-Bus"), "{text}");
    assert!(!text.contains("handed off"), "{text}");
    let s = result.structured_content.unwrap();
    assert_eq!(s["code"], "launch_exited");
    assert_eq!(s["exit_code"], 0);
    assert_eq!(s["running"], false);
    assert_eq!(s["exited_after_ms"], 1003);
    assert!(s.get("handoff").is_none());

    let failed = launch_exited_result(
        "TestApp-x86_64.AppImage",
        4243,
        Some(&failed_exit(127, 40)),
        "Error: No suitable fusermount binary found on the $PATH",
        true,
    );
    let text = result_text(&failed);
    assert!(text.contains("exited with code 127 after 40 ms"), "{text}");
    assert!(
        text.ends_with(
            "The end of its output:\nError: No suitable fusermount binary found on the $PATH"
        ),
        "{text}"
    );
    let s = failed.structured_content.unwrap();
    assert_eq!(s["exit_code"], 127);
    assert_eq!(
        s["output_tail"],
        "Error: No suitable fusermount binary found on the $PATH"
    );

    // Output not captured: no claim that it printed nothing.
    let blind = launch_exited_result("x", 1, None, "", false);
    let text = result_text(&blind);
    assert!(text.contains("its exit status was not recorded"), "{text}");
    assert!(!text.contains("printed nothing"), "{text}");
    assert_eq!(
        blind.structured_content.unwrap()["output_tail"],
        Value::Null
    );
}

#[test]
fn a_failed_launcher_is_not_waited_on_for_the_handoff_budget() {
    use std::time::Duration;
    for sign in [false, true] {
        assert_eq!(
            launch_window_budget(Some(&failed_exit(127, 40)), true, sign),
            Duration::from_millis(40) + LAUNCH_FAILED_GRACE
        );
    }
    assert_eq!(
        launch_window_budget(Some(&clean_exit(1000)), true, true),
        LAUNCH_HANDOFF_BUDGET
    );
    assert_eq!(
        launch_window_budget(None, true, false),
        LAUNCH_HANDOFF_BUDGET
    );
    assert_eq!(
        launch_window_budget(None, false, false),
        LAUNCH_STARTING_BUDGET
    );
}

#[test]
fn a_clean_exit_without_a_handoff_sign_is_reported_after_a_short_grace() {
    use std::time::Duration;
    // `sleep 1` exited cleanly at 1.0 s with no window: it was reported 8 s
    // after the launch, the whole hand-off budget.
    let budget = launch_window_budget(Some(&clean_exit(1000)), true, false);
    assert_eq!(
        budget,
        Duration::from_millis(1000) + LAUNCH_CLEAN_EXIT_GRACE
    );
    assert!(budget < Duration::from_secs(3));
    // A known launcher, a running instance or a process it left running is
    // a sign, and keeps the hand-off budget.
    let exit = clean_exit(77);
    let calculator = {
        let mut window = window("gnome-calculator");
        window.pid = Some(900);
        window
    };
    for (program, running, started) in [
        ("xdg-open", vec![], vec![]),
        ("gnome-calculator", vec![calculator], vec![]),
        ("code", vec![], vec![5150]),
    ] {
        let sign =
            launch_child::handoff_evidence(program, Some(&exit), &running, &started).is_some();
        assert!(sign, "{program}");
        assert_eq!(
            launch_window_budget(Some(&exit), true, sign),
            LAUNCH_HANDOFF_BUDGET
        );
    }
    let sign = launch_child::handoff_evidence("sleep", Some(&exit), &[], &[]).is_some();
    assert!(!sign);
}

#[test]
fn a_process_left_running_is_named_when_no_window_came() {
    let result = launch_handoff_timeout_result(
        "code",
        4242,
        8,
        Some(&clean_exit(400)),
        &launch_child::Handoff::StartedProcess { pid: 5150 },
    );
    let text = result_text(&result);
    assert!(
        text.contains("pid 5150, a process it started, is still running"),
        "{text}"
    );
    assert!(!text.contains("not running"), "{text}");
    let s = result.structured_content.unwrap();
    assert_eq!(s["code"], "launch_handoff_timeout");
    assert_eq!(s["handoff"], "started_process");
    assert_eq!(s["started_pid"], 5150);
}

#[test]
fn an_unpacked_appimage_launch_says_so_and_why() {
    let reason = "/usr/bin/fusermount3 shows owner uid 65534 here because root is not mapped \
                  in this user namespace, so it can't run setuid root";
    let owned = appimage::ExtractAndRun {
        reason: reason.to_owned(),
        run_dir: Some(PathBuf::from("/home/u/.cache/pincer/appimage-run/1-2-0")),
    };
    let launched = with_appimage_note(
        ToolResult::text("Launched TestApp-x86_64.AppImage (pid 7) in background.")
            .with_structured(json!({ "pid": 7 })),
        Some(&owned),
    );
    let text = result_text(&launched);
    assert!(text.contains("--appimage-extract-and-run"), "{text}");
    assert!(text.contains("root is not mapped"), "{text}");
    assert!(
        text.contains("unpacks itself into /home/u/.cache/pincer/appimage-run/1-2-0"),
        "{text}"
    );
    assert!(
        text.contains("Pincer deletes that folder when the app exits."),
        "{text}"
    );
    let s = launched.structured_content.unwrap();
    assert_eq!(s["appimage"]["mode"], "extract_and_run");
    assert_eq!(s["appimage"]["reason"], reason);
    assert_eq!(
        s["appimage"]["run_dir"],
        "/home/u/.cache/pincer/appimage-run/1-2-0"
    );
    assert_eq!(s["appimage"]["removed_after_exit"], true);

    // The command set its own TMPDIR: not ours to clean up, and it says so.
    let theirs = appimage::ExtractAndRun {
        reason: reason.to_owned(),
        run_dir: None,
    };
    let launched = with_appimage_note(
        ToolResult::text("Launched x.").with_structured(json!({ "pid": 7 })),
        Some(&theirs),
    );
    let text = result_text(&launched);
    assert!(text.contains("Pincer does not clean that up"), "{text}");
    assert_eq!(
        launched.structured_content.unwrap()["appimage"]["removed_after_exit"],
        false
    );

    let plain = with_appimage_note(
        ToolResult::text("Launched x.").with_structured(json!({ "pid": 7 })),
        None,
    );
    assert_eq!(result_text(&plain), "Launched x.");
    assert!(plain.structured_content.unwrap().get("appimage").is_none());
}

#[test]
fn a_child_process_descends_from_its_launcher_only() {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 5"])
        .spawn()
        .expect("/bin/sh should spawn");
    let me = std::process::id();
    assert!(descends_from(child.id(), me));
    assert!(!descends_from(me, child.id()));
    assert!(!descends_from(u32::MAX - 1, me));
    let _ = child.kill();
    let _ = child.wait();
}
