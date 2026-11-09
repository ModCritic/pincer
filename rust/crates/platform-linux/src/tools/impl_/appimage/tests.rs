use super::*;
use std::os::unix::fs::PermissionsExt;

/// The first bytes of a tested app's AppImage (read with od).
const TESTED_APPIMAGE_HEAD: &[u8] = b"\x7fELF\x02\x01\x01\x00AI\x02\x00\x00\x00\x00\x00";

fn file(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn own_uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

fn env(path_var: Option<&OsStr>) -> FuseEnv<'_> {
    FuseEnv {
        path_var,
        fusermount_prog: false,
        dev_fuse: true,
        no_new_privs: false,
        overflow_uid: Some(65534),
    }
}

#[test]
fn a_type2_appimage_is_an_elf_with_the_ai2_magic() {
    assert!(head_is_type2_appimage(TESTED_APPIMAGE_HEAD));
    assert!(!head_is_type2_appimage(
        b"\x7fELF\x02\x01\x01\x00\x00\x00\x00"
    ));
    // Type 1 AppImages carry "AI\x01" and use another runtime.
    assert!(!head_is_type2_appimage(b"\x7fELF\x02\x01\x01\x00AI\x01"));
    assert!(!head_is_type2_appimage(b"#!/bin/sh\nAI\x02"));
    assert!(!head_is_type2_appimage(b"\x7fELF"));

    let dir = tempfile::tempdir().unwrap();
    assert!(is_type2_appimage(&file(
        dir.path(),
        "App.AppImage",
        TESTED_APPIMAGE_HEAD,
        0o755
    )));
    assert!(!is_type2_appimage(&file(
        dir.path(),
        "short",
        b"\x7fELF",
        0o755
    )));
    assert!(!is_type2_appimage(&dir.path().join("missing")));
}

#[test]
fn a_bare_program_name_is_found_on_path_only_when_executable() {
    let dir = tempfile::tempdir().unwrap();
    let app = file(dir.path(), "tool", b"#!/bin/sh\n", 0o755);
    file(dir.path(), "data", b"x", 0o644);
    let path_var = dir.path().as_os_str();
    assert_eq!(resolve_program("tool", Some(path_var)), Some(app));
    assert_eq!(resolve_program("data", Some(path_var)), None);
    assert_eq!(resolve_program("tool", None), None);
    assert_eq!(
        resolve_program("/opt/x/App.AppImage", None),
        Some(PathBuf::from("/opt/x/App.AppImage"))
    );
}

#[test]
fn fuse_is_unavailable_without_a_setuid_root_fusermount() {
    let dir = tempfile::tempdir().unwrap();
    let path_var = dir.path().as_os_str();
    assert_eq!(
        fuse_unavailable_reason(&env(Some(path_var))).as_deref(),
        Some("no fusermount on PATH")
    );
    // Names the runtime does not take are ignored.
    file(dir.path(), "fusermount-helper", b"", 0o4755);
    assert_eq!(
        fuse_unavailable_reason(&env(Some(path_var))).as_deref(),
        Some("no fusermount on PATH")
    );
    // A fusermount3 this test owns stands in for the one the wrapper shows
    // as owned by the overflow uid.
    let fusermount = file(dir.path(), "fusermount3", b"", 0o4755);
    let uid = own_uid();
    if uid != 0 {
        let unmapped = FuseEnv {
            overflow_uid: Some(uid),
            ..env(Some(path_var))
        };
        let reason = fuse_unavailable_reason(&unmapped).unwrap();
        assert!(
            reason.contains(&fusermount.display().to_string()),
            "{reason}"
        );
        assert!(
            reason.contains("root is not mapped in this user namespace"),
            "{reason}"
        );
        let plain = fuse_unavailable_reason(&env(Some(path_var))).unwrap();
        assert!(
            plain.ends_with(&format!("is owned by uid {uid}, not root")),
            "{plain}"
        );
    }
}

#[test]
fn fuse_overrides_and_blockers_are_honored() {
    let dir = tempfile::tempdir().unwrap();
    let path_var = dir.path().as_os_str();
    let prog = FuseEnv {
        fusermount_prog: true,
        ..env(Some(path_var))
    };
    assert_eq!(fuse_unavailable_reason(&prog), None);
    let no_device = FuseEnv {
        dev_fuse: false,
        ..env(Some(path_var))
    };
    assert_eq!(
        fuse_unavailable_reason(&no_device).as_deref(),
        Some("/dev/fuse does not exist")
    );
    let no_new_privs = FuseEnv {
        no_new_privs: true,
        ..env(Some(path_var))
    };
    assert!(fuse_unavailable_reason(&no_new_privs)
        .unwrap()
        .contains("no_new_privs"));
}

#[test]
fn a_root_owned_setuid_fusermount_counts_as_usable_where_the_system_has_one() {
    // Environment-dependent: outside a user namespace that leaves root
    // unmapped, /usr/bin/fusermount3 is root's and setuid on most systems.
    let usable = ["/usr/bin/fusermount3", "/usr/bin/fusermount"]
        .iter()
        .any(|path| {
            std::fs::metadata(path).is_ok_and(|meta| meta.uid() == 0 && meta.mode() & 0o4000 != 0)
        });
    if usable && Path::new("/dev/fuse").exists() {
        let probe = FuseEnv {
            dev_fuse: true,
            ..env(Some(OsStr::new("/usr/bin")))
        };
        assert_eq!(fuse_unavailable_reason(&probe), None);
    }
}

#[test]
fn the_flag_goes_right_after_the_appimage_when_fuse_cannot_mount() {
    let dir = tempfile::tempdir().unwrap();
    let appimage = file(dir.path(), "App.AppImage", TESTED_APPIMAGE_HEAD, 0o755);
    let elf = file(
        dir.path(),
        "plain",
        b"\x7fELF\x02\x01\x01\x00\x00\x00\x00",
        0o755,
    );
    let empty = tempfile::tempdir().unwrap();
    let path_var = empty.path().as_os_str();
    let words = |list: &[&str]| list.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    let app = appimage.to_str().unwrap();

    let needed = extract_and_run_needed_in(&words(&[app, "--verbose"]), Some(path_var), env);
    assert_eq!(needed, Some((0, "no fusermount on PATH".to_owned())));
    let behind_env = extract_and_run_needed_in(
        &words(&["/usr/bin/env", "A=1", "-u", "B", app]),
        Some(path_var),
        env,
    );
    assert_eq!(behind_env.map(|(at, _)| at), Some(4));
    // Already asked for an AppImage mode: left alone.
    assert_eq!(
        extract_and_run_needed_in(&words(&[app, EXTRACT_AND_RUN_FLAG]), Some(path_var), env),
        None
    );
    // Not an AppImage, or FUSE works: left alone.
    assert_eq!(
        extract_and_run_needed_in(&words(&[elf.to_str().unwrap()]), Some(path_var), env),
        None
    );
    let fuse_works = |path_var| FuseEnv {
        fusermount_prog: true,
        ..env(path_var)
    };
    assert_eq!(
        extract_and_run_needed_in(&words(&[app]), Some(path_var), fuse_works),
        None
    );
}
