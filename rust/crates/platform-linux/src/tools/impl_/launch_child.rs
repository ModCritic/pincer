//! What `launch_app` knows about the process it spawned: how and when it
//! exited, and the end of what it printed. Used to say what really happened
//! to a launch that never showed a window, instead of guessing.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Bytes at the end of a capture that a failed launch quotes.
const OUTPUT_TAIL_BYTES: u64 = 2048;
/// Lines of that tail kept in a result.
const OUTPUT_TAIL_LINES: usize = 12;
/// While the driver runs, a capture that grows past this is emptied, so a
/// chatty long-lived app costs at most this much disk.
const OUTPUT_CAP_BYTES: u64 = 1 << 20;
/// How often the reaper checks that cap while the app runs.
const OUTPUT_CAP_CHECK: Duration = Duration::from_secs(10);

/// How a launched child ended, as its reaper saw it.
#[derive(Clone, Copy, Debug)]
pub(super) struct LaunchExit {
    pub status: ExitStatus,
    /// From the spawn to the exit.
    pub after: Duration,
}

impl LaunchExit {
    /// "exited with code 127" / "was killed by signal 9 (SIGKILL)".
    pub fn describe(&self) -> String {
        use std::os::unix::process::ExitStatusExt;
        match (self.status.code(), self.status.signal()) {
            (Some(code), _) => format!("exited with code {code}"),
            (None, Some(signal)) => match signal_name(signal) {
                Some(name) => format!("was killed by signal {signal} ({name})"),
                None => format!("was killed by signal {signal}"),
            },
            (None, None) => format!("exited ({})", self.status),
        }
    }
}

fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGTERM => "SIGTERM",
        _ => return None,
    })
}

/// A spawned launch: its pid, the exit its reaper records, and its output.
#[derive(Clone)]
pub(super) struct LaunchedChild {
    pub pid: u32,
    exit: Arc<Mutex<Option<LaunchExit>>>,
    output: Option<Arc<OutputCapture>>,
}

impl LaunchedChild {
    /// The exit, once the reaper has collected it.
    pub fn exit(&self) -> Option<LaunchExit> {
        *self
            .exit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// [`Self::exit`], giving the reaper up to `grace` to record an exit
    /// that `/proc` already shows (it reaps, then stores).
    pub fn exit_within(&self, grace: Duration) -> Option<LaunchExit> {
        let deadline = Instant::now() + grace;
        loop {
            if let Some(exit) = self.exit() {
                return Some(exit);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The last lines the child printed, stdout and stderr together (empty
    /// when it printed nothing or its output could not be captured).
    pub fn output_tail(&self) -> String {
        self.output
            .as_ref()
            .map(|capture| capture.tail())
            .unwrap_or_default()
    }

    /// Whether the child's output is being captured at all.
    pub fn output_captured(&self) -> bool {
        self.output.is_some()
    }
}

/// Spawn `command` with its stdout and stderr captured together, and reap it
/// in the background, recording how and when it exited. Both streams are
/// kept: the AppImage runtime prints its FUSE error on stdout and only a
/// detail on stderr.
pub(super) fn spawn_watched(command: Command) -> io::Result<LaunchedChild> {
    spawn_watched_in(
        command,
        dirs::cache_dir()
            .map(|cache| cache.join("pincer"))
            .as_deref(),
    )
}

/// [`spawn_watched`] with the output capture in `capture_dir` (created when
/// missing); without one, or when it can't be written, the output is
/// discarded.
pub(super) fn spawn_watched_in(
    mut command: Command,
    capture_dir: Option<&std::path::Path>,
) -> io::Result<LaunchedChild> {
    let capture = capture_dir
        .and_then(OutputCapture::create)
        .and_then(|capture| {
            let stdout = capture.stdio().ok()?;
            let stderr = capture.stdio().ok()?;
            Some((Arc::new(capture), stdout, stderr))
        });
    let output = match capture {
        Some((capture, stdout, stderr)) => {
            command.stdout(stdout).stderr(stderr);
            Some(capture)
        }
        None => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
            None
        }
    };
    let started = Instant::now();
    let child = command.spawn()?;
    let launched = LaunchedChild {
        pid: child.id(),
        exit: Arc::new(Mutex::new(None)),
        output,
    };
    reap_recording(child, started, launched.clone());
    Ok(launched)
}

/// The reaper of [`spawn_watched`]: like `reap_in_background`, one thread
/// per child, blocked until the child exits; it also records the exit and
/// keeps the output capture under its cap while the child runs.
fn reap_recording(mut child: Child, started: Instant, launched: LaunchedChild) {
    let pid = child.id();
    let reaper = std::thread::Builder::new()
        .name(format!("pincer-reap-{pid}"))
        .stack_size(64 * 1024)
        .spawn(move || {
            let status = match launched.output.as_deref() {
                Some(capture) => wait_capping(&mut child, capture),
                None => child.wait(),
            };
            match status {
                Ok(status) => {
                    *launched
                        .exit
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(LaunchExit {
                        status,
                        after: started.elapsed(),
                    });
                }
                Err(e) => tracing::debug!(pid, "launched app could not be reaped: {e}"),
            }
        });
    if let Err(e) = reaper {
        tracing::debug!(pid, "could not start reaper thread for launched app: {e}");
    }
}

/// Wait for `child`, waking every [`OUTPUT_CAP_CHECK`] to cap its output
/// capture. Waits on a pidfd so the exit is seen at once; without pidfd
/// support (Linux before 5.3) it just waits, uncapped.
fn wait_capping(child: &mut Child, capture: &OutputCapture) -> io::Result<ExitStatus> {
    // SAFETY: pidfd_open takes a pid and a flags word and returns a new file
    // descriptor or -1; no memory is passed.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) };
    if raw < 0 {
        return child.wait();
    }
    // SAFETY: `raw` is a descriptor pidfd_open just returned; nothing else
    // owns it.
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw as RawFd) };
    let timeout_ms = OUTPUT_CAP_CHECK.as_millis() as libc::c_int;
    loop {
        let mut pollfd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ready != 0 {
            // Exited (readable), or poll failed: wait blocks until the exit.
            return child.wait();
        }
        capture.cap();
    }
}

/// A launched child's stdout and stderr, kept in an anonymous file in the
/// user's cache directory. A file rather than a pipe: an app that outlives
/// the driver keeps writing there, where a pipe with no reader would kill it
/// with SIGPIPE (or fail its writes with EPIPE). The file has no name
/// (`O_TMPFILE`, or unlinked at once where the filesystem lacks it), so
/// nothing is left behind: its space is freed when the app and the driver
/// have both closed it. Opened `O_APPEND`, so the child's writes follow a
/// truncation by [`Self::cap`].
struct OutputCapture {
    file: File,
}

impl OutputCapture {
    fn create(dir: &std::path::Path) -> Option<Self> {
        std::fs::create_dir_all(dir).ok()?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).append(true).mode(0o600);
        let file = options
            .clone()
            .custom_flags(libc::O_TMPFILE)
            .open(dir)
            .or_else(|_| {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or_default();
                let path = dir.join(format!(".launch-output-{}-{nanos}", std::process::id()));
                let file = options.create_new(true).open(&path)?;
                std::fs::remove_file(&path)?;
                io::Result::Ok(file)
            })
            .ok()?;
        Some(Self { file })
    }

    /// A descriptor for the child's stdout or stderr (the same open file).
    fn stdio(&self) -> io::Result<Stdio> {
        Ok(Stdio::from(self.file.try_clone()?))
    }

    fn cap(&self) {
        if self
            .file
            .metadata()
            .is_ok_and(|metadata| metadata.len() > OUTPUT_CAP_BYTES)
        {
            let _ = self.file.set_len(0);
        }
    }

    fn tail(&self) -> String {
        let len = self.file.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(OUTPUT_TAIL_BYTES);
        let mut bytes = vec![0; (len - start) as usize];
        let read = self.file.read_at(&mut bytes, start).unwrap_or(0);
        bytes.truncate(read);
        output_tail_text(&bytes, start > 0)
    }
}

/// The last [`OUTPUT_TAIL_LINES`] non-empty lines of `bytes`. `cut` says the
/// bytes start inside the stream, so their first line may be partial and is
/// dropped.
pub(super) fn output_tail_text(bytes: &[u8], cut: bool) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if cut && lines.len() > 1 {
        lines.remove(0);
    }
    let lines: Vec<&str> = lines
        .into_iter()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let skip = lines.len().saturating_sub(OUTPUT_TAIL_LINES);
    lines[skip..].join("\n")
}

/// Programs whose whole job is to pass a launch request to another process
/// and exit: a clean exit from one of them is a hand-off.
const HANDOFF_LAUNCHERS: &[&str] = &[
    "xdg-open",
    "gio",
    "gtk-launch",
    "gtk4-launch",
    "gapplication",
    "kioclient",
    "kioclient5",
    "kioclient6",
    "kde-open",
    "kde-open5",
    "kde-open6",
    "kstart",
    "kstart5",
    "kstart6",
    "exo-open",
    "dex",
    "dbus-send",
    "gdbus",
    "busctl",
];

/// The program a launch command runs: its first word, past an `env` prefix
/// and its `NAME=value` assignments, as a file name.
pub(super) fn launch_program(command: &str) -> String {
    let mut words = command.split_whitespace().peekable();
    if words
        .peek()
        .is_some_and(|first| std::path::Path::new(first).file_name() == Some("env".as_ref()))
    {
        words.next();
        while let Some(word) = words.next_if(|word| word.starts_with('-') || word.contains('=')) {
            // env's options that take a separate argument.
            if matches!(word, "-u" | "--unset" | "-C" | "--chdir") {
                words.next();
            }
        }
    }
    words
        .next()
        .map(|program| {
            std::path::Path::new(program)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(program)
                .to_owned()
        })
        .unwrap_or_default()
}

/// Why a launch that exited without opening a new window counts as a
/// hand-off to another process.
#[derive(Debug)]
pub(super) enum Handoff {
    /// The program is a launcher that passes the request on and exits.
    Launcher(String),
    /// A window of the app, owned by another process, already exists: a
    /// single-instance app hands a second launch to its running instance.
    RunningInstance {
        pid: u32,
        windows: Vec<crate::x11::WindowInfo>,
    },
}

/// The hand-off rule for a launcher that exited and opened no new window.
/// A hand-off needs a clean exit (code 0) and one more piece of evidence:
/// the program is a known hand-off launcher ([`HANDOFF_LAUNCHERS`]), or a
/// window matching the launch is owned by another process (`running`). A
/// non-zero exit, a signal, an unknown exit, or a clean exit with neither is
/// reported as what it was: the process ended without a window.
pub(super) fn handoff_evidence(
    program: &str,
    exit: Option<&LaunchExit>,
    running: &[crate::x11::WindowInfo],
) -> Option<Handoff> {
    if !exit.is_some_and(|exit| exit.status.success()) {
        return None;
    }
    if HANDOFF_LAUNCHERS.contains(&program) {
        return Some(Handoff::Launcher(program.to_owned()));
    }
    let pid = running.iter().find_map(|window| window.pid)?;
    Some(Handoff::RunningInstance {
        pid,
        windows: running
            .iter()
            .filter(|window| window.pid == Some(pid))
            .cloned()
            .collect(),
    })
}

#[cfg(test)]
mod tests;
