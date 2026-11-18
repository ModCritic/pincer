//! launch_app: resolving what to start, spawning it outside Pincer's own
//! environment, and waiting for its windows.

use super::*;

// ── launch_app ───────────────────────────────────────────────────────────────

pub struct LaunchAppTool;
static LAUNCH_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

/// What one launch_app call starts, resolved once for the `process_launch`
/// attestation and again, the same way, for the launch itself.
#[derive(Debug, Clone)]
enum LaunchPlan {
    /// Exec one program.
    Program {
        /// The canonical path that is exec'd: the attested executable.
        executable: std::path::PathBuf,
        /// The launch command's words: an installed entry's Exec= (as
        /// list_apps shows it) or the caller's launch_path. The first is
        /// argv[0] as spelled.
        command: Vec<String>,
        /// The caller's `additional_arguments`, then its `urls`.
        appended: Vec<String>,
        /// The installed .desktop entry the launch resolved through.
        entry: Option<crate::installed_apps::InstalledApp>,
        /// The caller's urls, for the result's "Opened ..." note.
        urls: Vec<String>,
        /// The caller's spelling: its launch_path, bundle_id or name.
        query: String,
        options: bool,
        argument_count: usize,
    },
    /// Open targets in their default apps with xdg-open.
    Open {
        executable: std::path::PathBuf,
        targets: Vec<String>,
        /// The caller's `name` when it was itself the first target.
        named: Option<String>,
        options: bool,
    },
}

impl LaunchPlan {
    /// The resource the `process_launch` adapter admits. Deterministic for
    /// the same arguments and installed entries, so dispatch can re-prove it.
    ///
    /// xdg-open launches attest `installed_app: true` with no desktop entry:
    /// each target opens in the default app the user installed for it.
    /// A target that could run a program (an option, a .desktop file, an
    /// executable file) is attested as an option instead.
    fn scope(&self) -> Value {
        match self {
            Self::Program {
                executable,
                entry,
                options,
                argument_count,
                ..
            } => json!({
                "kind": "program_launch",
                "executable": executable.to_string_lossy(),
                "installed_app": entry.is_some(),
                "desktop_entry": entry.as_ref().map(|entry| entry.bundle_id.clone()),
                "options": options,
                "argument_count": argument_count,
            }),
            Self::Open {
                executable,
                targets,
                options,
                ..
            } => json!({
                "kind": "program_launch",
                "executable": executable.to_string_lossy(),
                "installed_app": true,
                "desktop_entry": Value::Null,
                "options": options,
                "argument_count": targets.len(),
            }),
        }
    }

    /// Every string the caller or the entry puts on the command line, for
    /// the debugger-switch guard.
    fn words(&self) -> Vec<&str> {
        match self {
            Self::Program {
                command, appended, ..
            } => command.iter().chain(appended).map(String::as_str).collect(),
            Self::Open { targets, .. } => targets.iter().map(String::as_str).collect(),
        }
    }
}

/// The installed entries a launch resolves against when a test installed a
/// fixture on this thread; `None` reads the machine's own .desktop files.
/// Read on the calling thread, before any blocking hand-off.
fn launch_installed_fixture() -> Option<Vec<crate::installed_apps::InstalledApp>> {
    #[cfg(test)]
    {
        TEST_INSTALLED_APPS.with(|apps| apps.borrow().clone())
    }
    #[cfg(not(test))]
    {
        None
    }
}

/// Whether a `name` that is no installed app may run a program from PATH.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NameLookup {
    /// Attestation and attested launches (standard and bounded mode).
    InstalledOnly,
    /// A launch that was not attested: the registry skips launch
    /// attestation only in unrestricted mode without a manifest, where any
    /// program may run anyway, so `name` keeps its PATH fallback there.
    InstalledThenPath,
}

/// [`plan_launch`] against the fixture, or the installed .desktop entries.
fn plan_launch_with(
    args: &Value,
    fixture: Option<Vec<crate::installed_apps::InstalledApp>>,
    lookup: NameLookup,
) -> Result<LaunchPlan, String> {
    let installed = fixture.unwrap_or_else(crate::installed_apps::list_installed_apps);
    plan_launch(args, &installed, lookup)
}

/// Resolve a launch_app call to the exact program it starts.
///
/// `launch_path` (highest precedence) runs as written, split on spaces; it
/// counts as an installed app only when its words are exactly an entry's
/// list_apps launch_path. `bundle_id` resolves only through installed
/// .desktop entries, and so does `name` unless `lookup` is
/// [`NameLookup::InstalledThenPath`], which also runs a name from PATH. A
/// `name` that is a URL or an existing path, or `urls` alone, open with
/// xdg-open.
fn plan_launch(
    args: &Value,
    installed: &[crate::installed_apps::InstalledApp],
    lookup: NameLookup,
) -> Result<LaunchPlan, String> {
    use pincer_core::tool_args::ArgsExt;
    let present = |key: &str| args.opt_str(key).filter(|value| !value.trim().is_empty());
    let urls = args.str_array("urls");
    let additional = args.str_array("additional_arguments");
    // list_installed_apps orders by display name only, and equal names come
    // out in hash order. Sort fully so the same call resolves the same way
    // for attestation and dispatch.
    let mut apps = installed.to_vec();
    apps.sort_by(|a, b| {
        (a.name.to_lowercase(), &a.bundle_id).cmp(&(b.name.to_lowercase(), &b.bundle_id))
    });
    let words = |command: &str| -> Vec<String> {
        command.split_whitespace().map(str::to_owned).collect()
    };

    if let Some(launch_path) = present("launch_path") {
        let command = words(&launch_path);
        let entry = apps
            .iter()
            .find(|app| app.launch_path.split_whitespace().eq(command.iter().map(String::as_str)))
            .cloned();
        return program_plan(command, entry, additional, urls, launch_path.trim().to_owned());
    }
    if let Some(bundle_id) = present("bundle_id") {
        let bundle_id = bundle_id.trim();
        let entry = apps
            .iter()
            .find(|app| app.bundle_id == bundle_id)
            .or_else(|| {
                apps.iter()
                    .find(|app| app.bundle_id.eq_ignore_ascii_case(bundle_id))
            })
            .cloned()
            .ok_or_else(|| {
                format!(
                    "bundle_id '{bundle_id}' matches no installed .desktop application; call \
                     list_apps and pass an app's bundle_id, name or launch_path"
                )
            })?;
        let command = words(&entry.launch_path);
        return program_plan(command, Some(entry), additional, urls, bundle_id.to_owned());
    }
    if let Some(name) = present("name") {
        let name = name.trim().to_owned();
        if let Some(entry) = match_installed_app(&apps, &name).cloned() {
            let command = words(&entry.launch_path);
            return program_plan(command, Some(entry), additional, urls, name);
        }
        if name.contains("://") || std::path::Path::new(&name).exists() {
            let mut targets = vec![name.clone()];
            targets.extend(urls);
            return open_plan(targets, Some(name));
        }
        if lookup == NameLookup::InstalledThenPath {
            return program_plan(words(&name), None, additional, urls, name);
        }
        return Err(format!(
            "'{name}' matches no installed .desktop application (by display name, desktop-file \
             id or Exec name), and outside unrestricted mode launch_app does not run a name from \
             PATH. Call list_apps and \
             pass an installed app's name, bundle_id or launch_path. To run another program, \
             pass its path as launch_path; standard mode refuses programs that are not \
             installed apps"
        ));
    }
    if !urls.is_empty() {
        return open_plan(urls, None);
    }
    Err("Provide at least one of: launch_path, bundle_id, name, or urls.".to_owned())
}

fn program_plan(
    command: Vec<String>,
    entry: Option<crate::installed_apps::InstalledApp>,
    additional: Vec<String>,
    urls: Vec<String>,
    query: String,
) -> Result<LaunchPlan, String> {
    let first = command
        .first()
        .ok_or_else(|| "the launch command is empty".to_owned())?;
    let executable = resolve_program(first).ok_or_else(|| match &entry {
        Some(entry) => format!(
            "installed app {} launches with {}, which is not an executable on PATH",
            quote_untrusted(&entry.name, SHOWN_NAME_CHARS),
            quote_untrusted(first, SHOWN_COMMAND_CHARS)
        ),
        None => format!("'{first}' is not an executable file or a program on PATH"),
    })?;
    let mut appended = additional;
    appended.extend(urls.iter().cloned());
    // An entry's own Exec= words are the app's; a raw launch_path's words
    // after the program are the caller's, like additional_arguments.
    let caller: Vec<&String> = if entry.is_some() {
        appended.iter().collect()
    } else {
        command[1..].iter().chain(&appended).collect()
    };
    let terminal = command.iter().any(|word| names_a_terminal(word))
        || entry.as_ref().is_some_and(|entry| {
            names_a_terminal(&entry.bundle_id)
                || entry.startup_wm_class.as_deref().is_some_and(names_a_terminal)
        });
    // A terminal runs its positional arguments as a command (kitty, foot,
    // st), so any argument to one is an option.
    let options =
        caller.iter().any(|word| word.starts_with('-')) || (terminal && !caller.is_empty());
    let argument_count = caller.len();
    Ok(LaunchPlan::Program {
        executable,
        command,
        appended,
        entry,
        urls,
        query,
        options,
        argument_count,
    })
}

fn open_plan(targets: Vec<String>, named: Option<String>) -> Result<LaunchPlan, String> {
    let executable = resolve_program("xdg-open")
        .ok_or_else(|| "xdg-open is not installed, so files and URLs cannot be opened".to_owned())?;
    let options = targets
        .iter()
        .any(|target| target.starts_with('-') || target_can_run_a_program(target));
    Ok(LaunchPlan::Open {
        executable,
        targets,
        named,
        options,
    })
}

/// The canonical path of the program a launch command's first word names:
/// a path (absolute, or relative to the driver's directory) or a name looked
/// up on PATH, executable either way.
fn resolve_program(word: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let runnable = |path: &std::path::Path| -> Option<std::path::PathBuf> {
        let metadata = std::fs::metadata(path).ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .then(|| std::fs::canonicalize(path).ok())
            .flatten()
    };
    if word.contains('/') {
        return runnable(std::path::Path::new(word));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| runnable(&dir.join(word)))
}

/// Terminal emulators that `crate::terminal`'s lists miss. Several run
/// their positional arguments as a command (foot, st, kgx).
const LAUNCH_EXTRA_TERMINALS: &[&str] = &[
    "contour",
    "cool-retro-term",
    "foot",
    "footclient",
    "guake",
    "kgx",
    "ptyxis",
    "rio",
    "rxvt",
    "sakura",
    "st",
    "terminator",
    "terminology",
    "tilda",
    "urxvt",
    "urxvtc",
    "x-terminal-emulator",
    "xdg-terminal-exec",
    "yakuake",
];

/// Whether a command word, desktop-file id or window class names a terminal
/// emulator, by its file name or its app id's last part.
fn names_a_terminal(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    let base = std::path::Path::new(&lower)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&lower);
    let is_terminal = |name: &str| {
        crate::terminal::is_terminal_process_name(name)
            || crate::terminal::wm_class_matches_terminal(name, "")
            || LAUNCH_EXTRA_TERMINALS.contains(&name)
    };
    is_terminal(base) || app_id_last(base).is_some_and(is_terminal)
}

/// Whether xdg-open could run a program for this target: a local .desktop
/// file (it may launch its Exec= line) or an executable file.
fn target_can_run_a_program(target: &str) -> bool {
    let local = if target.contains("://") {
        match url::Url::parse(target) {
            Ok(url) if url.scheme() == "file" => match url.to_file_path() {
                Ok(path) => path,
                Err(()) => return true,
            },
            _ => return false,
        }
    } else {
        std::path::PathBuf::from(target)
    };
    if local
        .to_string_lossy()
        .to_ascii_lowercase()
        .ends_with(".desktop")
    {
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(&local)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Remove every PINCER_* variable from a launched program's environment:
/// the permission mode and approval bypass, capability manifest and policy
/// files, launch grants and tokens. A Pincer started inside the app (from a
/// terminal, say) would otherwise run with the daemon's authority.
fn scrub_driver_environment(command: &mut std::process::Command) {
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"PINCER_") {
            command.env_remove(key);
        }
    }
}

/// Spawn a launch in the background: exec `executable`, the attested
/// program, with `argv` (argv[0] kept as the command spelled it, so a
/// multi-call binary behind a symlink still sees its name). The child's
/// exit and the end of its output are recorded for the launch result.
pub(super) fn spawn_launch(executable: &std::path::Path, argv: &[String]) -> std::io::Result<LaunchedChild> {
    use std::os::unix::process::CommandExt;
    let mut words = argv.to_vec();
    if words.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty launch command",
        ));
    }
    // A type-2 AppImage that can't mount with FUSE here (root unmapped in a
    // user namespace) unpacks and runs instead of failing with exit 127. It
    // unpacks into a folder of its own on disk, which the reaper deletes once
    // the app has exited, unless the command sets its own TMPDIR.
    let mut run_dir = None;
    let extract_and_run = appimage::extract_and_run_needed(&words).map(|(at, reason)| {
        if !appimage::command_sets_tmpdir(&words, at) {
            if let Some(root) = appimage::run_root() {
                appimage::sweep_run_dirs(&root);
                match appimage::RunDir::create(&root) {
                    Ok(dir) => run_dir = Some(dir),
                    Err(error) => {
                        tracing::debug!("no AppImage run folder under {}: {error}", root.display())
                    }
                }
            }
        }
        words.insert(at + 1, appimage::EXTRACT_AND_RUN_FLAG.to_owned());
        appimage::ExtractAndRun {
            reason,
            run_dir: run_dir.as_ref().map(|dir| dir.path().to_path_buf()),
        }
    });
    let prog = words[0].clone();
    let rest = &words[1..];
    let mut launch = std::process::Command::new(executable);
    launch
        .arg0(&prog)
        .args(rest)
        // The child must not inherit the driver's stdio: in private-worker mode
        // stdout is the JSON-RPC stream to the SDK, and a chatty app (Chromium's
        // zygote logs, GTK warnings) writing there corrupts a response and shuts
        // the worker down. Its stdout and stderr go to a capture
        // (`spawn_watched`) instead.
        .stdin(std::process::Stdio::null())
        // Enable accessibility for this child without toggling GNOME's global
        // ScreenReaderEnabled setting (which can launch Orca). Chromium and
        // Electron read ACCESSIBILITY_ENABLED, GTK's bridge NO_AT_BRIDGE, and
        // Firefox and Thunderbird GNOME_ACCESSIBILITY (the only one their
        // libxul names). Checked in a sandbox: Firefox 159 started while
        // the session's accessibility status was off published no AT-SPI
        // application without it and its whole tree with it. Toolkits
        // ignore the ones they do not read.
        .env("ACCESSIBILITY_ENABLED", "1")
        .env("GNOME_ACCESSIBILITY", "1")
        .env("NO_AT_BRIDGE", "0");
    scrub_driver_environment(&mut launch);
    if launch_gets_chromium_flags_for_command(executable, &words)
        && !rest
            .iter()
            .any(|arg| arg == "--force-renderer-accessibility")
    {
        launch.arg("--force-renderer-accessibility");
    }
    if let Some(dir) = &run_dir {
        launch.env("TMPDIR", dir.path());
    }
    if let Some(id) = background_startup_id() {
        launch.env("DESKTOP_STARTUP_ID", id);
    }
    let mut child = launch_child::spawn_watched(launch, run_dir)?;
    child.extract_and_run = extract_and_run;
    Ok(child)
}

/// A startup-notification id (`<unique>_TIME<timestamp>`) that asks the
/// window manager to leave a launched window inactive, or `None` outside a
/// plain X11 session or when the X server's clock cannot be read.
///
/// EWMH focus-stealing prevention activates a new window only when its
/// `_NET_WM_USER_TIME` is newer than the user's last input in the active
/// window. Assumption, checked in a KWin X11 slot at bfc42c0: GTK3 copies the
/// `_TIME` suffix into `_NET_WM_USER_TIME` (the Test Pad got 3331800000 and
/// stayed inactive and below, where the same launch without the variable
/// was activated and raised), while Qt 6.12 ignores it and stamps a fresh
/// server time of its own (KWrite was activated with the variable set), so
/// this keeps only some apps in the background. Not tried on Wayland or
/// XWayland, so not sent there.
fn background_startup_id() -> Option<String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() || crate::x11::server_is_xwayland() {
        return None;
    }
    use x11rb::connection::Connection as _;
    let (conn, screen) = x11rb::rust_connection::RustConnection::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let now = crate::input::server_time(&conn, root)?;
    static LAUNCHES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let launch = LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(format!(
        "pincer-launch-{}-{launch}_TIME{}",
        std::process::id(),
        background_user_time(now)
    ))
}

/// A user time older than any input in the last 2^30 ms (12.4 days) before
/// `now`. The X clock is 32-bit and window managers compare its times
/// modulo 2^32 (older when `a - b` wraps past 2^31), so this stays "in the
/// past" however long the server has run, where a fixed `_TIME1` turns into
/// "newer" after 24.8 days of server uptime. 0 is avoided: GTK reads it as
/// no time at all.
pub(super) fn background_user_time(now: u32) -> u32 {
    now.wrapping_sub(1 << 30).max(1)
}

/// Re-read the launched windows (`list` lists every window) until their
/// titles have not changed for `quiet`, at most `limit`. An app often maps
/// its window with a default title and retitles it right after: KWrite
/// showed "KWrite", then the file name before "KWrite" 15 ms later, and the
/// launch result at bfc42c0 could carry the first. Windows that close meanwhile
/// are dropped; ones that open are not added.
pub(super) fn settle_window_titles(
    mut windows: Vec<crate::x11::WindowInfo>,
    mut list: impl FnMut() -> Vec<crate::x11::WindowInfo>,
    quiet: std::time::Duration,
    limit: std::time::Duration,
) -> Vec<crate::x11::WindowInfo> {
    let started = std::time::Instant::now();
    let mut unchanged_since = started;
    while !windows.is_empty() && unchanged_since.elapsed() < quiet && started.elapsed() < limit {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let all = list();
        let fresh: Vec<_> = windows
            .iter()
            .filter_map(|old| all.iter().find(|new| new.xid == old.xid).cloned())
            .collect();
        let titles = |list: &[crate::x11::WindowInfo]| -> Vec<(u64, String)> {
            list.iter().map(|w| (w.xid, w.title.clone())).collect()
        };
        if titles(&fresh) != titles(&windows) {
            unchanged_since = std::time::Instant::now();
        }
        windows = fresh;
    }
    windows
}

/// How long launched windows' titles must stay the same, and the most
/// [`settle_window_titles`] waits for that.
const LAUNCH_TITLE_QUIET: std::time::Duration = std::time::Duration::from_millis(200);
const LAUNCH_TITLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The note a launch that unpacked an AppImage carries: why, where the copy
/// goes and what it costs. In a driver-owned folder the copy is deleted
/// after the app exits; elsewhere the runtime is left to it, and the
/// tested AppImage's runtime left its 547 MB copy behind.
fn appimage_extract_note(extract: &appimage::ExtractAndRun) -> String {
    let flag = appimage::EXTRACT_AND_RUN_FLAG;
    let reason = &extract.reason;
    match &extract.run_dir {
        Some(dir) => format!(
            "Started with {flag}: FUSE can't mount AppImages here ({reason}), so it unpacks \
             itself into {} (on disk; also the app's TMPDIR) instead of mounting, which costs \
             time and disk on every start. Pincer deletes that folder when the app exits.",
            dir.display()
        ),
        None => format!(
            "Started with {flag}: FUSE can't mount AppImages here ({reason}), so it unpacks \
             itself to $TMPDIR (default /tmp) instead of mounting, which costs time and disk \
             (RAM when /tmp is tmpfs) on every start and may leave the unpacked copy behind; \
             Pincer does not clean that up."
        ),
    }
}

/// [`appimage_extract_note`] on a launch result, text and structured.
pub(super) fn with_appimage_note(
    mut result: ToolResult,
    extract: Option<&appimage::ExtractAndRun>,
) -> ToolResult {
    let Some(extract) = extract else {
        return result;
    };
    let note = appimage_extract_note(extract);
    result
        .content
        .push(pincer_core::protocol::Content::text(note));
    if let Some(structured) = result.structured_content.as_mut() {
        structured["appimage"] = json!({
            "mode": "extract_and_run",
            "reason": extract.reason,
            "run_dir": extract.run_dir,
            "removed_after_exit": extract.run_dir.is_some(),
        });
    }
    result
}

/// Match a launch_app `name` that failed direct exec against installed XDG
/// .desktop applications: exact display name, desktop-file id, or `Exec=`
/// basename first, then a display-name substring when it is unambiguous.
pub(super) fn match_installed_app<'a>(
    apps: &'a [crate::installed_apps::InstalledApp],
    query: &str,
) -> Option<&'a crate::installed_apps::InstalledApp> {
    let q = query.to_ascii_lowercase();
    if q.is_empty() {
        return None;
    }
    apps.iter()
        .find(|a| {
            a.name.to_ascii_lowercase() == q
                || a.bundle_id.to_ascii_lowercase() == q
                || exec_basename(&a.launch_path) == q
        })
        .or_else(|| {
            let mut matches = apps
                .iter()
                .filter(|a| a.name.to_ascii_lowercase().contains(&q));
            match (matches.next(), matches.next()) {
                (Some(only), None) => Some(only),
                _ => None,
            }
        })
}

/// Watch a just-spawned `xdg-open` long enough to catch a fast failure.
/// xdg-open's generic fallback can `exec` the target app and stay alive for
/// its whole lifetime, so a child still running after the grace period counts
/// as success; a quick non-zero exit (2 = file not found, 3 = no handler
/// tool, 4 = action failed) is the only reliable failure signal. The reaper
/// of [`launch_child::spawn_watched`] keeps watching a child still running.
fn xdg_open_failure(child: &LaunchedChild) -> Option<String> {
    match child.exit_within(std::time::Duration::from_millis(1500)) {
        Some(exit) if exit.status.success() => None,
        Some(exit) => {
            let tail = child.output_tail();
            Some(if tail.is_empty() {
                format!("xdg-open {}", exit.describe())
            } else {
                format!(
                    "xdg-open {}. The end of its output (program text):\n{}",
                    exit.describe(),
                    fenced_output(&tail)
                )
            })
        }
        None => None,
    }
}

/// Open each target with `xdg-open` (the attested `executable`) in its
/// default app, all started before any is watched, and refuse on the first
/// fast failure. Like a launched app, xdg-open gets no stdin, a private
/// capture for stdout and stderr (in private-worker mode the driver's stdout
/// is the JSON-RPC stream), and no PINCER_* variables.
fn open_with_xdg(executable: &std::path::Path, targets: &[String]) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut children = Vec::new();
    for target in targets {
        let mut command = std::process::Command::new(executable);
        command
            .arg0("xdg-open")
            .arg(target)
            .stdin(std::process::Stdio::null());
        scrub_driver_environment(&mut command);
        children.push((target, launch_child::spawn_watched(command, None)?));
    }
    for (target, child) in children {
        if let Some(reason) = xdg_open_failure(&child) {
            anyhow::bail!("could not open '{target}': {reason}");
        }
    }
    Ok(())
}

// Test-only stand-in for the installed .desktop entries a launch resolves
// against, per thread, so launch tests do not depend on the apps installed on
// the machine that runs them.
#[cfg(test)]
thread_local! {
    pub(super) static TEST_INSTALLED_APPS: std::cell::RefCell<Option<Vec<crate::installed_apps::InstalledApp>>> =
        const { std::cell::RefCell::new(None) };
}

#[async_trait]
impl Tool for LaunchAppTool {
    fn def(&self) -> &ToolDef {
        LAUNCH_DEF.get_or_init(|| ToolDef {
            name: "launch_app".into(),
            // Activation and title behavior measured in a KWin X11 slot at
            // bfc42c0; see background_startup_id and settle_window_titles.
            description: "Start a Linux app and return its pid and windows, titles read once \
                they stop changing (up to 1 s). The window manager may activate and raise the \
                new window (KWin does for Qt and KDE apps such as KWrite and Dolphin; GTK apps \
                stay behind an already active window); `active` says whether it did. Name the app with launch_path or \
                name. To open files or URLs in that app, pass them in `urls` (or \
                `additional_arguments`): they are appended to its command line, for example \
                {name:\"KWrite\", urls:[\"/path/notes.txt\"]}. `urls` without launch_path or \
                name go to xdg-open, which opens each in its default app and returns no pid. \
                Standard mode starts installed apps only (by name, bundle_id or their list_apps \
                launch_path), with no argument or url starting with '-' and no arguments at all \
                for a terminal; other programs need unrestricted mode or a capability manifest. \
                Refused when the name matches nothing launchable."
                .into(),
            input_schema: json!({"type":"object","properties":{
                "launch_path":{"type":"string","description":"Round-trip the `launch_path` returned by `list_apps`: the Exec= command from the .desktop file with XDG field codes already stripped. Highest precedence on Linux; split on spaces and run directly, without a shell. Any other command line runs a program that is not an installed app, which standard mode refuses."},
                "name":{"type":"string","description":"Installed app to launch, matched against installed .desktop applications (exact display name, desktop-file id, or Exec basename; else an unambiguous display-name substring). Never runs a program from PATH: pass a path as launch_path for that. A URL or existing file path here is opened with xdg-open."},
                "bundle_id":{"type":"string","description":"Desktop-file id of an installed app, as list_apps returns it (e.g. org.kde.kwrite). Used when launch_path is absent."},
                "urls":{"type":"array","items":{"type":"string"},"description":"Files or URLs to open. With launch_path or name, appended to that app's arguments (after additional_arguments); alone, each is opened with xdg-open in its default app."},
                "additional_arguments":{"type":"array","items":{"type":"string"},"description":"Extra command-line arguments passed to the launched process."}
            },"additionalProperties":false}),
            read_only: false, destructive: false, idempotent: false, open_world: true,
        })
    }

    /// The exact program this call starts, for the `process_launch` adapter.
    async fn protected_resource_scope(
        &self,
        adapter_id: &str,
        args: &Value,
    ) -> Result<Option<Value>, String> {
        if adapter_id != "process_launch" {
            return Ok(None);
        }
        let args = args.clone();
        let fixture = launch_installed_fixture();
        tokio::task::spawn_blocking(move || {
            plan_launch_with(&args, fixture, NameLookup::InstalledOnly).map(|plan| Some(plan.scope()))
        })
        .await
        .map_err(|error| format!("launch resolution failed: {error}"))?
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use pincer_core::tool_args::ArgsExt;
        let launch_path_opt = args.opt_str("launch_path");
        let name_opt = args.opt_str("name");
        let urls: Vec<String> = args.str_array("urls");
        let additional_arguments: Vec<String> = args.str_array("additional_arguments");
        if args.get("cdp_debugging_port").is_some() {
            return ToolResult::error(
                "cdp_debugging_port moved to browser_prepare so DevTools is never enabled on an unproven user profile",
            );
        }
        // `urls` join the checked arguments: with an app named, they are
        // appended to its command line below.
        if launch_path_opt
            .as_deref()
            .into_iter()
            .chain(name_opt.as_deref())
            .chain(additional_arguments.iter().map(String::as_str))
            .chain(urls.iter().map(String::as_str))
            .any(pincer_core::launch_guard::contains_remote_debugging_flag)
        {
            return ToolResult::error(pincer_core::launch_guard::REMOTE_DEBUGGING_LAUNCH_REFUSAL);
        }

        // Resolve the exact program, the same way the attestation did. The
        // registry passes the scope it admitted as `_protected_launch_scope`
        // (callers cannot set reserved fields); the launch runs only that.
        let approved = args
            .get("_protected_launch_scope")
            .filter(|scope| !scope.is_null())
            .cloned();
        let fixture = launch_installed_fixture();
        let plan_args = args.clone();
        // No approved scope means the registry skipped attestation, which it
        // does only in unrestricted mode without a manifest.
        let lookup = if approved.is_some() {
            NameLookup::InstalledOnly
        } else {
            NameLookup::InstalledThenPath
        };
        let plan = match tokio::task::spawn_blocking(move || {
            plan_launch_with(&plan_args, fixture, lookup)
        })
        .await
        {
            Ok(Ok(plan)) => plan,
            Ok(Err(message)) => return ToolResult::error(format!("Failed to launch: {message}")),
            Err(error) => return ToolResult::error(format!("Task error: {error}")),
        };
        if let Some(approved) = approved {
            if plan.scope() != approved {
                return ToolResult::error(
                    "Failed to launch: the program this launch_app call resolves to changed \
                     after it was approved; call launch_app again",
                )
                .with_structured(json!({
                    "code": "launch_target_changed",
                    "effect": "none",
                }));
            }
        }
        // An installed entry's own Exec= words get the same guard as the
        // caller's arguments.
        if plan
            .words()
            .into_iter()
            .any(pincer_core::launch_guard::contains_remote_debugging_flag)
        {
            return ToolResult::error(pincer_core::launch_guard::REMOTE_DEBUGGING_LAUNCH_REFUSAL);
        }

        // Snapshot the top-levels before spawning so a window created by an
        // already-running service (D-Bus activation) can be told apart.
        let windows_before: std::collections::HashSet<u64> =
            tokio::task::spawn_blocking(|| crate::wayland::list_windows_dispatch(None))
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|w| w.xid)
                .collect();
        let result = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(String, Option<LaunchedChild>, String)> {
                match plan {
                    LaunchPlan::Open {
                        executable,
                        targets,
                        named,
                        ..
                    } => {
                        open_with_xdg(&executable, &targets)?;
                        // xdg-open may spawn a helper and exit, so do not
                        // claim its pid is the app pid.
                        Ok(match named {
                            None => (
                                format!("Opened {} URL(s) via xdg-open.", targets.len()),
                                None,
                                "xdg-open".to_owned(),
                            ),
                            Some(name) => (
                                match targets.len() - 1 {
                                    0 => format!("Opened '{name}' via xdg-open."),
                                    n => format!("Opened '{name}' and {n} URL(s) via xdg-open."),
                                },
                                None,
                                name,
                            ),
                        })
                    }
                    LaunchPlan::Program {
                        executable,
                        command,
                        appended,
                        entry,
                        urls,
                        query,
                        ..
                    } => {
                        // With an app named, `urls` are its open targets, as
                        // on macOS. Assumption: appending them after
                        // `additional_arguments` matches where a .desktop
                        // Exec= line's trailing %U/%F field code puts them;
                        // list_apps strips those codes from launch_path.
                        let opened_note = match urls.len() {
                            0 => String::new(),
                            1 => format!(" Opened {} in it.", urls[0]),
                            n => format!(" Opened {n} files or URLs in it."),
                        };
                        let mut argv = command.clone();
                        argv.extend(appended);
                        let child = spawn_launch(&executable, &argv).map_err(|error| match &entry {
                            Some(app) => anyhow::anyhow!(
                                "'{query}' matched installed app {} but its launcher {} failed \
                                 to start: {error}",
                                quote_untrusted(&app.name, SHOWN_NAME_CHARS),
                                quote_untrusted(&app.launch_path, SHOWN_COMMAND_CHARS)
                            ),
                            None => anyhow::anyhow!("'{query}' failed to start: {error}"),
                        })?;
                        Ok(match entry {
                            Some(app) => {
                                // Match keys for the window: display name, the
                                // caller's spelling, StartupWMClass, and last
                                // the command that ran.
                                let mut keys = vec![app.name.clone(), query.clone()];
                                keys.extend(app.startup_wm_class.clone());
                                keys.push(app.launch_path.clone());
                                (
                                    format!(
                                        "✅ Launched {} ({}, pid {}), resolved from '{query}' \
                                         by its desktop entry.{opened_note}",
                                        quote_untrusted(&app.name, SHOWN_NAME_CHARS),
                                        quote_untrusted(&app.launch_path, SHOWN_COMMAND_CHARS),
                                        child.pid
                                    ),
                                    Some(child),
                                    keys.join("\u{0}"),
                                )
                            }
                            None => (
                                format!("✅ Launched {query} (pid {}).{opened_note}", child.pid),
                                Some(child),
                                query,
                            ),
                        })
                    }
                }
            },
        )
        .await;

        match result {
            Ok(Ok((message, child_opt, name))) => {
                if let Some(child) = child_opt {
                    let launcher_pid = child.pid;
                    // `name` may carry NUL-separated match keys (display name,
                    // query, Exec=) for a desktop-entry launch. The last key
                    // is the command that actually ran.
                    let query = name.clone();
                    let program = launch_child::launch_program(
                        query.rsplit('\u{0}').next().unwrap_or(&query),
                    );
                    let name = name.split('\u{0}').next().unwrap_or("").to_owned();
                    let watched = child.clone();
                    let program_w = program.clone();
                    let (resolved, active) = tokio::task::spawn_blocking(move || {
                        let mut resolved =
                            resolve_launched_windows(&watched, &query, &program_w, &windows_before);
                        resolved.windows = settle_window_titles(
                            std::mem::take(&mut resolved.windows),
                            || crate::wayland::list_windows_dispatch(None),
                            LAUNCH_TITLE_QUIET,
                            LAUNCH_TITLE_LIMIT,
                        );
                        // `_NET_ACTIVE_WINDOW` names X11 windows only.
                        let active = !crate::wayland::is_wayland()
                            && crate::x11::active_window().is_some_and(|active| {
                                resolved.windows.iter().any(|w| w.xid == active)
                            });
                        (resolved, active)
                    })
                    .await
                    .unwrap_or_else(|_| (LaunchedWindows::default(), false));
                    let windows: Vec<Value> =
                        resolved.windows.iter().map(window_record_json).collect();
                    let mut text = message;
                    let mut structured = json!({
                        "pid": resolved.pid,
                        "bundle_id": Value::Null,
                        "name": name,
                        "running": resolved.pid.is_some(),
                        "active": active,
                        "windows": windows,
                        "launcher_pid": launcher_pid,
                    });
                    if let Some(pid) = resolved.pid.filter(|_| resolved.child_of_launcher) {
                        structured["launched_by"] = json!(launcher_pid);
                        text.push_str(&format!(
                            " Its window belongs to pid {pid}, a process the launcher started."
                        ));
                    }
                    if resolved.handed_off {
                        match resolved.pid {
                            Some(pid) => {
                                structured["handoff"] = json!("dbus_activation");
                                text.push_str(&format!(
                                    " The launcher handed the request to another process \
                                     (a wrapper or D-Bus activation); the window belongs to pid {pid}."
                                ));
                            }
                            None => {
                                // The launcher exited and no new window appeared.
                                // This is not a success: no pid, no window. It is
                                // a hand-off only with evidence of one (see
                                // `launch_child::handoff_evidence`); otherwise the
                                // result says how the process ended and quotes
                                // its output.
                                let exit = resolved.exit.or_else(|| {
                                    child.exit_within(std::time::Duration::from_millis(200))
                                });
                                let handoff = launch_child::handoff_evidence(
                                    &program,
                                    exit.as_ref(),
                                    &resolved.running_instance,
                                    &resolved.still_running,
                                );
                                let result = match handoff {
                                    Some(handoff) => launch_handoff_timeout_result(
                                        &name,
                                        launcher_pid,
                                        LAUNCH_HANDOFF_BUDGET.as_secs(),
                                        exit.as_ref(),
                                        &handoff,
                                    ),
                                    None => launch_exited_result(
                                        &name,
                                        launcher_pid,
                                        exit.as_ref(),
                                        &child.output_tail(),
                                        child.output_captured(),
                                    ),
                                };
                                return with_appimage_note(result, child.extract_and_run.as_ref());
                            }
                        }
                    }
                    if active {
                        text.push_str(" The window manager made its window the active one.");
                    }
                    with_appimage_note(
                        ToolResult::text(text).with_structured(structured),
                        child.extract_and_run.as_ref(),
                    )
                } else {
                    ToolResult::text(message).with_structured(json!({
                        "pid": Value::Null,
                        "bundle_id": Value::Null,
                        "name": name,
                        "running": Value::Null,
                        "active": false,
                        "windows": [],
                    }))
                }
            }
            Ok(Err(e)) => ToolResult::error(format!("Failed to launch: {e}")),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

/// A `launch_app` hand-off timed out: the launcher exited cleanly with
/// evidence that it handed the request on (`handoff`), but no new window of
/// the app appeared within the deadline. This has no pid and no confirmed
/// window, so it is a refusal (`code: "launch_handoff_timeout"`), never a
/// `pid: null` shape that a caller could mistake for success.
pub(super) fn launch_handoff_timeout_result(
    name: &str,
    launcher_pid: u32,
    waited_secs: u64,
    exit: Option<&LaunchExit>,
    handoff: &launch_child::Handoff,
) -> ToolResult {
    let after = exit
        .map(|exit| format!(" after {}", launch_elapsed(exit.after)))
        .unwrap_or_default();
    let mut structured = json!({
        "code": "launch_handoff_timeout",
        "effect": "refused",
        "name": name,
        "launcher_pid": launcher_pid,
        "pid": Value::Null,
        "running": Value::Null,
        "waited_secs": waited_secs,
        "exit_code": exit.and_then(|exit| exit.status.code()),
        "exited_after_ms": exit.map(|exit| exit.after.as_millis() as u64),
    });
    let text = match handoff {
        launch_child::Handoff::Launcher(program) => {
            structured["handoff"] = json!("launcher");
            format!(
                "'{name}' (pid {launcher_pid}) exited cleanly{after}: {program} hands a launch \
                 to another process and exits, but no new window appeared within \
                 {waited_secs}s; the launch could not be confirmed. Call list_windows to check \
                 whether it started anyway, or retry."
            )
        }
        launch_child::Handoff::RunningInstance { pid, windows } => {
            structured["handoff"] = json!("running_instance");
            structured["running_instance"] = json!({
                "pid": pid,
                "windows": windows.iter().map(window_record_json).collect::<Vec<_>>(),
            });
            let named = windows
                .first()
                .map(|window| format!(" (window {} {})", window.xid, quoted_title(&window.title)))
                .unwrap_or_default();
            format!(
                "'{name}' (pid {launcher_pid}) exited cleanly{after} and no new window appeared \
                 within {waited_secs}s. The app is already running as pid {pid}{named}; a \
                 single-instance app hands a second launch to its running instance, which may \
                 only have raised its existing window. Act on pid {pid}, or call list_windows."
            )
        }
        launch_child::Handoff::StartedProcess { pid } => {
            structured["handoff"] = json!("started_process");
            structured["started_pid"] = json!(pid);
            format!(
                "'{name}' (pid {launcher_pid}) exited cleanly{after}, but pid {pid}, a process \
                 it started, is still running, and no window appeared within {waited_secs}s; \
                 the launch could not be confirmed. Call list_windows to check whether pid \
                 {pid} opens one later."
            )
        }
    };
    ToolResult::error(text).with_structured(structured)
}

/// A launched program's output as result text: escaped by
/// `untrusted_text::block` (controls, terminal escapes and invisible
/// characters shown as `\u{..}`) and every line fenced with `| `, so it can
/// neither forge a line of Pincer's own nor reach the client's terminal raw.
fn fenced_output(tail: &str) -> String {
    pincer_core::untrusted_text::block(tail)
        .lines()
        .map(|line| format!("| {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// "40 ms" under a second, "1.0 s" from there.
fn launch_elapsed(elapsed: std::time::Duration) -> String {
    if elapsed < std::time::Duration::from_secs(1) {
        format!("{} ms", elapsed.as_millis())
    } else {
        format!("{:.1} s", elapsed.as_secs_f64())
    }
}

/// The launched process ended without opening a window and nothing shows a
/// hand-off: say how it ended (exit code or signal, and when) and quote the
/// end of its output (stdout and stderr).
pub(super) fn launch_exited_result(
    name: &str,
    launcher_pid: u32,
    exit: Option<&LaunchExit>,
    output_tail: &str,
    output_captured: bool,
) -> ToolResult {
    use std::os::unix::process::ExitStatusExt;
    let how = match exit {
        Some(exit) => format!("{} after {}", exit.describe(), launch_elapsed(exit.after)),
        None => "exited (its exit status was not recorded)".to_owned(),
    };
    let mut text =
        format!("'{name}' (pid {launcher_pid}) {how} without opening a window; it is not running.");
    if !output_tail.is_empty() {
        text.push_str(&format!(
            " The end of its output (program text):\n{}",
            fenced_output(output_tail)
        ));
    } else if output_captured {
        text.push_str(" It printed nothing.");
    }
    ToolResult::error(text).with_structured(json!({
        "code": "launch_exited",
        "effect": "none",
        "name": name,
        "launcher_pid": launcher_pid,
        "pid": Value::Null,
        "running": false,
        "exit_code": exit.and_then(|exit| exit.status.code()),
        "signal": exit.and_then(|exit| exit.status.signal()),
        "exited_after_ms": exit.map(|exit| exit.after.as_millis() as u64),
        "output_tail": if output_captured {
            json!(pincer_core::untrusted_text::block(output_tail))
        } else {
            Value::Null
        },
    }))
}

/// What `launch_app` could attribute to the launch after the spawn.
#[derive(Default)]
struct LaunchedWindows {
    /// The process that owns the app's window: the launcher itself, or the
    /// wrapper target / running service it handed off to.
    pid: Option<u32>,
    windows: Vec<crate::x11::WindowInfo>,
    /// The window belongs to another process than the launcher, or the
    /// launcher exited without one.
    handed_off: bool,
    /// The window belongs to a process the launcher started (`pid`).
    child_of_launcher: bool,
    /// How the launcher exited, when it did.
    exit: Option<LaunchExit>,
    /// When the launcher exited without a new window: windows of the
    /// launched app that other processes own (an already-running instance).
    running_instance: Vec<crate::x11::WindowInfo>,
    /// When the launcher exited without a new window: processes it started
    /// that are still running.
    still_running: Vec<u32>,
}

/// Whether `pid` descends from `ancestor`: a bounded walk up the parent
/// chain in /proc. A child whose parent already exited was reparented and no
/// longer counts.
pub(super) fn descends_from(pid: u32, ancestor: u32) -> bool {
    let mut current = pid;
    for _ in 0..32 {
        let Some(parent) = std::fs::read_to_string(format!("/proc/{current}/stat"))
            .ok()
            .and_then(|stat| {
                let (_, rest) = stat.rsplit_once(')')?;
                rest.split_whitespace().nth(1)?.parse::<u32>().ok()
            })
        else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        if parent <= 1 {
            return false;
        }
        current = parent;
    }
    false
}

/// Whether the launched process is gone or a zombie (already reaped or being
/// reaped by the launch thread).
pub(super) fn process_exited(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state == "Z" || state == "X"),
        Err(_) => true,
    }
}

/// Whether a new window is the launch's, by its WM_CLASS / app id: the
/// class, or an app-id class's last part, is one of the launch's names
/// whole ([`window_is_launched_app`]), or starts with one of them followed
/// by `-` (`libreoffice` opens `libreoffice-writer`). Never part of a name:
/// a window of class "e" is not a "kwrite" launch, and "plasmashell" is not
/// an "sh" one.
pub(super) fn window_matches_launch(window: &crate::x11::WindowInfo, query: &str) -> bool {
    if window_is_launched_app(window, query) {
        return true;
    }
    let class = window.app_name.trim().to_ascii_lowercase();
    !class.is_empty()
        && launch_identity_names(query).iter().any(|name| {
            class
                .strip_prefix(name.as_str())
                .is_some_and(|rest| rest.starts_with('-'))
        })
}

/// The last part of a reverse-DNS app id ("org.gnome.Nautilus" ->
/// "Nautilus"): a single word with at least two dots. A file name with an
/// extension ("hi.sh", "soffice.bin") is not one.
pub(super) fn app_id_last(name: &str) -> Option<&str> {
    let (_, last) = name.rsplit_once('.')?;
    (name.matches('.').count() >= 2
        && !last.is_empty()
        && !name.contains(char::is_whitespace)
        && !name.contains('/'))
    .then_some(last)
}

/// The names a launch's own window class can have, lowercase. A query is
/// NUL-separated keys (a desktop entry's display name, the query, and last
/// the command that ran) or one command. Each key counts whole; a key that
/// is one word also as its file name without ".desktop" and, for an app id,
/// its last part. The command adds the program it runs, with and without its
/// extension.
fn launch_identity_names(query: &str) -> Vec<String> {
    let keys: Vec<String> = query
        .split('\u{0}')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut candidates: Vec<String> = Vec::new();
    for key in &keys {
        candidates.push(key.clone());
        if key.contains(char::is_whitespace) {
            continue;
        }
        let stem = key
            .rsplit('/')
            .next()
            .unwrap_or(key)
            .trim_end_matches(".desktop");
        candidates.push(stem.to_owned());
        if let Some(last) = app_id_last(stem) {
            candidates.push(last.to_owned());
        }
    }
    if let Some(command) = keys.last() {
        let program = launch_child::launch_program(command);
        match app_id_last(&program) {
            Some(last) => candidates.push(last.to_owned()),
            None => {
                if let Some((base, _)) = program
                    .rsplit_once('.')
                    .filter(|(base, _)| !base.is_empty())
                {
                    candidates.push(base.to_owned());
                }
            }
        }
        candidates.push(program);
    }
    let mut names: Vec<String> = Vec::new();
    for name in candidates {
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// Whether `window` is plainly an instance of the launched app: its class,
/// or an app-id class's last part, is one of the launch's names, whole
/// (case-insensitive). This is the evidence for a running instance, so,
/// unlike [`window_matches_launch`], it never matches part of a name: a
/// script "hi.sh" must not name the desktop shell ("plasmashell") as the app
/// it handed off to.
pub(super) fn window_is_launched_app(window: &crate::x11::WindowInfo, query: &str) -> bool {
    let class = window.app_name.trim().to_ascii_lowercase();
    if class.is_empty() {
        return false;
    }
    let names = launch_identity_names(query);
    names.contains(&class)
        || app_id_last(&class).is_some_and(|last| names.iter().any(|name| name == last))
}

/// How long a launch may take to show a window while the launcher process is
/// still alive (the app is starting; LibreOffice on a small VM needs well over
/// ten seconds), and once it has exited (a wrapper or D-Bus activation has
/// taken over, so the window is either imminent or never coming).
pub(super) const LAUNCH_STARTING_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
pub(super) const LAUNCH_HANDOFF_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);
/// After a launcher fails (non-zero exit or a signal), how long a window of
/// the launch is still waited for: a failed start does not hand off, so its
/// window is not coming.
pub(super) const LAUNCH_FAILED_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
/// After a clean exit with no sign of a hand-off, how long a late window is
/// still waited for. `sleep 1` used to be reported 8 s after it started.
pub(super) const LAUNCH_CLEAN_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// The window budget once the launcher's state is known: a failed exit waits
/// only [`LAUNCH_FAILED_GRACE`] past the exit, a clean exit the hand-off
/// budget when there is a sign of a hand-off (`handoff_sign`, see
/// [`launch_child::handoff_evidence`]) and [`LAUNCH_CLEAN_EXIT_GRACE`] past
/// the exit when there is none, a running launcher the starting budget. An
/// exit not yet recorded keeps the hand-off budget until it is (the reaper
/// stores it right after the process is gone).
pub(super) fn launch_window_budget(
    exit: Option<&LaunchExit>,
    launcher_exited: bool,
    handoff_sign: bool,
) -> std::time::Duration {
    match exit {
        Some(exit) if !exit.status.success() => exit.after + LAUNCH_FAILED_GRACE,
        Some(exit) if !handoff_sign => exit.after + LAUNCH_CLEAN_EXIT_GRACE,
        _ if launcher_exited => LAUNCH_HANDOFF_BUDGET,
        _ => LAUNCH_STARTING_BUDGET,
    }
}

/// Resolve the window(s) of a launch. Prefers the launcher's own window;
/// otherwise adopts a newly mapped top-level whose WM_CLASS matches the
/// launch, whoever owns it: a wrapper script that exec'd the real binary
/// (`libreoffice` → `soffice.bin`), or the running D-Bus service GNOME apps
/// hand their request to. `program` is the program the launch ran
/// ([`launch_child::launch_program`]).
fn resolve_launched_windows(
    launcher: &LaunchedChild,
    query: &str,
    program: &str,
    before: &std::collections::HashSet<u64>,
) -> LaunchedWindows {
    let launcher_pid = launcher.pid;
    let started = std::time::Instant::now();
    // Processes the launcher started, recorded while it runs: once it exits
    // they are reparented and no longer descend from it.
    let mut started_pids: Vec<u32> = Vec::new();
    loop {
        if !process_exited(launcher_pid) {
            for pid in launch_child::descendants(launcher_pid) {
                if !started_pids.contains(&pid) {
                    started_pids.push(pid);
                }
            }
        }
        let all = crate::wayland::list_windows_dispatch(None);
        let own: Vec<_> = all
            .iter()
            .filter(|w| w.pid == Some(launcher_pid))
            .cloned()
            .collect();
        if !own.is_empty() {
            return LaunchedWindows {
                pid: Some(launcher_pid),
                windows: own,
                ..LaunchedWindows::default()
            };
        }
        // A new window of a process the launcher started is the launch's
        // own, whatever its class: an AppImage runtime forks its app (the
        // window belongs to the app's pid), and so does a wrapper that
        // does not exec, or a CLI that starts its app detached and exits.
        if let Some(pid) = all
            .iter()
            .filter(|w| !before.contains(&w.xid))
            .filter_map(|w| w.pid)
            .find(|pid| started_pids.contains(pid) || descends_from(*pid, launcher_pid))
        {
            return LaunchedWindows {
                pid: Some(pid),
                windows: all.iter().filter(|w| w.pid == Some(pid)).cloned().collect(),
                child_of_launcher: true,
                ..LaunchedWindows::default()
            };
        }
        let fresh: Vec<_> = all
            .iter()
            .filter(|w| !before.contains(&w.xid) && w.pid.is_some())
            .filter(|w| window_matches_launch(w, query))
            .cloned()
            .collect();
        if let Some(pid) = fresh.first().and_then(|w| w.pid) {
            let windows = fresh.into_iter().filter(|w| w.pid == Some(pid)).collect();
            return LaunchedWindows {
                pid: Some(pid),
                windows,
                handed_off: true,
                exit: launcher.exit(),
                ..LaunchedWindows::default()
            };
        }
        let exit = launcher.exit();
        let launcher_exited = exit.is_some() || process_exited(launcher_pid);
        // After a clean exit, the evidence the result would cite for a
        // hand-off decides how long a late window is waited for.
        let (running_instance, still_running) = if launcher_exited {
            (
                all.iter()
                    .filter(|w| w.pid.is_some() && w.pid != Some(launcher_pid))
                    .filter(|w| window_is_launched_app(w, query))
                    .cloned()
                    .collect::<Vec<_>>(),
                started_pids
                    .iter()
                    .copied()
                    .filter(|pid| !process_exited(*pid))
                    .collect::<Vec<_>>(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let handoff_sign = launch_child::handoff_evidence(
            program,
            exit.as_ref(),
            &running_instance,
            &still_running,
        )
        .is_some();
        if started.elapsed() >= launch_window_budget(exit.as_ref(), launcher_exited, handoff_sign) {
            return LaunchedWindows {
                pid: (!launcher_exited).then_some(launcher_pid),
                handed_off: launcher_exited,
                exit,
                running_instance,
                still_running,
                ..LaunchedWindows::default()
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// [`launch_gets_chromium_flags`] for a launch command `words` whose program
/// resolved to `executable`. A command run through `env` (`env NAME=value
/// program ...`, common in .desktop entries) is judged by the program `env`
/// runs.
pub(super) fn launch_gets_chromium_flags_for_command(executable: &std::path::Path, words: &[String]) -> bool {
    if executable.file_name().is_some_and(|name| name == "env") {
        let mut rest = words.iter().skip(1);
        while let Some(word) = rest.next() {
            match word.as_str() {
                "-u" | "--unset" | "-C" | "--chdir" => {
                    rest.next();
                }
                // `env -S "program args"` splits a string: not followed.
                "-S" | "--split-string" => return false,
                _ if word.starts_with('-') || word.contains('=') => {}
                _ => {
                    return resolve_program(word)
                        .is_some_and(|program| launch_gets_chromium_flags(&program, word))
                }
            }
        }
        return false;
    }
    launch_gets_chromium_flags(executable, words.first().map(String::as_str).unwrap_or(""))
}

/// Whether launch_app adds `--force-renderer-accessibility`: the program
/// runs Chromium (a Chromium-family browser, or an Electron or CEF app), by
/// the name it was launched as or by the executable it resolved to
/// ([`crate::web_engine::is_chromium_executable`], which also reads a
/// wrapper script such as /usr/bin/claude-desktop for the binary it execs).
/// No other program gets Chromium's flags.
pub(super) fn launch_gets_chromium_flags(executable: &std::path::Path, program: &str) -> bool {
    let name = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    crate::web_engine::is_chromium_name(name)
        || crate::web_engine::is_chromium_executable(executable)
}
