//! AppImages where FUSE can't mount them. Under a user namespace that does
//! not map root (the `unshare -rn ... --map-user=1000` wrapper that keeps the
//! driver off the network), the setuid-root `fusermount` shows up as owned by
//! the overflow uid, the AppImage runtime refuses it, and the AppImage exits
//! 127 with "No suitable fusermount binary found on the $PATH". The runtime's
//! documented `--appimage-extract-and-run` flag skips FUSE: it unpacks the
//! image to `$TMPDIR` and runs it from there (checked on one tested
//! AppImage inside that wrapper: its window came up). That runtime never
//! deleted its unpacked tree (547 MB for the tested app), so the driver gives
//! each such launch its own `TMPDIR` on disk and deletes it after the app.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The flag the type-2 runtime documents in `--appimage-help`. It must be
/// the first `--` argument: the runtime only looks at that one.
pub(super) const EXTRACT_AND_RUN_FLAG: &str = "--appimage-extract-and-run";

/// Whether `path` is a type-2 AppImage: an ELF file whose bytes 8..11 are
/// the AppImage magic "AI\x02" (the tested app's AppImage starts
/// `7f 45 4c 46 02 01 01 00 41 49 02`).
pub(super) fn is_type2_appimage(path: &Path) -> bool {
    let mut head = [0u8; 11];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok()
        && head_is_type2_appimage(&head)
}

fn head_is_type2_appimage(head: &[u8]) -> bool {
    head.len() >= 11 && head[..4] == *b"\x7fELF" && head[8..11] == *b"AI\x02"
}

/// The file a launch command's program word names: itself when it has a
/// slash, else the first executable of that name on `PATH`.
pub(super) fn resolve_program(word: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if word.contains('/') {
        return Some(PathBuf::from(word));
    }
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(word))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
        })
}

/// What the AppImage runtime's FUSE mount depends on, read from this
/// process's view of the system (its user namespace included).
pub(super) struct FuseEnv<'a> {
    /// `PATH`, searched for `fusermount` and `fusermount<digits>`.
    pub path_var: Option<&'a OsStr>,
    /// `FUSERMOUNT_PROG`: when set the runtime uses it without checks.
    pub fusermount_prog: bool,
    pub dev_fuse: bool,
    /// `NoNewPrivs` in /proc/self/status: setuid can't raise privileges.
    pub no_new_privs: bool,
    /// The uid unmapped owners show up as (/proc/sys/kernel/overflowuid).
    pub overflow_uid: Option<u32>,
}

impl<'a> FuseEnv<'a> {
    fn current(path_var: Option<&'a OsStr>) -> Self {
        let no_new_privs = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("NoNewPrivs:"))
                    .map(|value| value.trim() == "1")
            })
            .unwrap_or(false);
        Self {
            path_var,
            fusermount_prog: std::env::var_os("FUSERMOUNT_PROG").is_some(),
            dev_fuse: Path::new("/dev/fuse").exists(),
            no_new_privs,
            overflow_uid: std::fs::read_to_string("/proc/sys/kernel/overflowuid")
                .ok()
                .and_then(|uid| uid.trim().parse().ok()),
        }
    }
}

/// Why an AppImage can't mount itself with FUSE in this environment, or
/// `None` when it should. Mirrors the type-2 runtime's own search
/// (`find_fusermount` in AppImage/type2-runtime's runtime.c at fedfad5, the
/// runtime the tested app's AppImage reports): a file on `PATH` named
/// `fusermount` plus optional digits, owned by uid 0 with the setuid bit;
/// `FUSERMOUNT_PROG` skips the search. Without `/dev/fuse`, or with
/// no_new_privs set, no fusermount can mount either.
pub(super) fn fuse_unavailable_reason(env: &FuseEnv<'_>) -> Option<String> {
    if env.fusermount_prog {
        return None;
    }
    if !env.dev_fuse {
        return Some("/dev/fuse does not exist".to_owned());
    }
    if env.no_new_privs {
        return Some(
            "no_new_privs is set for this process, so a setuid fusermount can't gain root"
                .to_owned(),
        );
    }
    let mut refused = Vec::new();
    for dir in env
        .path_var
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
    {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(suffix) = name.to_str().and_then(|n| n.strip_prefix("fusermount")) else {
                continue;
            };
            if !suffix.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(meta) = std::fs::metadata(entry.path()) else {
                continue;
            };
            if meta.uid() == 0 && meta.mode() & 0o4000 != 0 {
                return None;
            }
            refused.push((entry.path(), meta.uid(), meta.mode() & 0o4000 != 0));
        }
    }
    let Some((path, uid, setuid)) = refused.first() else {
        return Some("no fusermount on PATH".to_owned());
    };
    let path = path.display();
    Some(if *uid != 0 && Some(*uid) == env.overflow_uid {
        format!(
            "{path} shows owner uid {uid} here because root is not mapped in this user \
             namespace, so it can't run setuid root"
        )
    } else if *uid != 0 {
        format!("{path} is owned by uid {uid}, not root")
    } else if !setuid {
        format!("{path} is not setuid")
    } else {
        format!("{path} is not usable")
    })
}

/// When `words` (a launch command plus its extra arguments) runs a type-2
/// AppImage that can't mount with FUSE here: the position of the AppImage
/// word, after which the extract-and-run flag goes, and the reason.
pub(super) fn extract_and_run_needed(words: &[String]) -> Option<(usize, String)> {
    let path_var = std::env::var_os("PATH");
    extract_and_run_needed_in(words, path_var.as_deref(), FuseEnv::current)
}

/// [`extract_and_run_needed`] with `PATH` and the FUSE environment given;
/// `fuse_env` is only read for an AppImage.
fn extract_and_run_needed_in<'a>(
    words: &[String],
    path_var: Option<&'a OsStr>,
    fuse_env: impl FnOnce(Option<&'a OsStr>) -> FuseEnv<'a>,
) -> Option<(usize, String)> {
    let at = super::launch_child::program_position(words)?;
    if words
        .get(at + 1)
        .is_some_and(|word| word.starts_with("--appimage-"))
    {
        return None;
    }
    let program = resolve_program(&words[at], path_var)?;
    if !is_type2_appimage(&program) {
        return None;
    }
    fuse_unavailable_reason(&fuse_env(path_var)).map(|reason| (at, reason))
}

/// How a launch runs an AppImage that can't mount: why, and the folder it
/// unpacks into (its `TMPDIR`), when the driver owns one.
#[derive(Clone, Debug)]
pub(super) struct ExtractAndRun {
    pub reason: String,
    /// The per-launch folder the driver deletes once the app has exited;
    /// `None` when the command sets its own `TMPDIR` or no folder could be
    /// made (the runtime then unpacks to `$TMPDIR` or /tmp, not cleaned up).
    pub run_dir: Option<PathBuf>,
}

/// Whether the words before the program set `TMPDIR` (`env TMPDIR=... app`):
/// the caller chose where the image unpacks, so the driver leaves it alone.
pub(super) fn command_sets_tmpdir(words: &[String], program_at: usize) -> bool {
    words[..program_at.min(words.len())]
        .iter()
        .any(|word| word.starts_with("TMPDIR="))
}

/// Where extract-and-run launches unpack: one folder per launch in the
/// user's cache directory, on disk (/tmp is tmpfs, RAM, on many desktops).
pub(super) fn run_root() -> Option<PathBuf> {
    dirs::cache_dir().map(|cache| cache.join("pincer").join("appimage-run"))
}

/// The file in a run folder that names the launcher (pid and start time).
const LAUNCHER_MARKER: &str = ".pincer-launcher";
/// A run folder with no launcher recorded is left alone this long: its
/// launch may be starting in another driver.
const UNRECORDED_GRACE: Duration = Duration::from_secs(60);
/// How often a release checks whether the folder is still in use.
const RELEASE_POLL: Duration = Duration::from_secs(2);

/// One extract-and-run launch's folder, used as its `TMPDIR`. The runtime
/// names its unpacked tree after the md5 of the AppImage file
/// (`appimage_extracted_<md5>`; checked: the tested app's tree name is the
/// file's md5sum), so two launches sharing a `TMPDIR` would share one tree
/// and removing it for one would pull files from under the other. Each
/// launch gets its own folder instead.
#[derive(Debug)]
pub(super) struct RunDir {
    path: PathBuf,
}

impl RunDir {
    pub fn create(root: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(root)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let path = root.join(format!(
            "{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record the launched process, with its start time so a reused pid
    /// does not keep the folder alive.
    pub fn record_launcher(&self, pid: u32) {
        if let Some(started) = process_start_time(pid) {
            let _ = std::fs::write(
                self.path.join(LAUNCHER_MARKER),
                format!("{pid} {started}\n"),
            );
        }
    }

    /// Once the launcher has exited: wait while any process still runs from
    /// the folder (an app whose runtime was killed keeps running from it),
    /// then delete it. Blocks; the launch's reaper thread calls it.
    pub fn release(self) {
        while in_use(&self.path) {
            std::thread::sleep(RELEASE_POLL);
        }
        self.remove_now();
    }

    /// Delete the folder at once (the spawn failed).
    pub fn remove_now(self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::debug!(path = %self.path.display(), "could not remove AppImage run folder: {error}");
            }
        }
    }
}

/// The start time of `pid` in clock ticks since boot (/proc/<pid>/stat
/// field 22), which tells a process from a later one that reuses its pid.
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    // Fields after the command name start at field 3 (state).
    rest.split_whitespace().nth(22 - 3)?.parse().ok()
}

/// Whether the launcher recorded in a run folder is still running.
fn launcher_alive(dir: &Path) -> Option<bool> {
    let marker = std::fs::read_to_string(dir.join(LAUNCHER_MARKER)).ok()?;
    let mut fields = marker.split_whitespace();
    let pid: u32 = fields.next()?.parse().ok()?;
    let started: u64 = fields.next()?.parse().ok()?;
    Some(process_start_time(pid) == Some(started))
}

/// Whether a live process runs from `dir`: its executable, or one of its
/// command-line arguments, lies under it. The command line covers a process
/// whose executable link can't be read (another user namespace); the
/// tested app runs as `<dir>/appimage_extracted_<md5>/usr/python/bin/python3.13`.
pub(super) fn in_use(dir: &Path) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let Some(prefix) = dir.to_str().map(|dir| format!("{dir}/")) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        if !name
            .to_str()
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        {
            return false;
        }
        let proc_dir = entry.path();
        if std::fs::read_link(proc_dir.join("exe"))
            .ok()
            .and_then(|exe| exe.to_str().map(|exe| exe.starts_with(&prefix)))
            .unwrap_or(false)
        {
            return true;
        }
        std::fs::read(proc_dir.join("cmdline")).is_ok_and(|cmdline| {
            cmdline
                .split(|byte| *byte == 0)
                .any(|arg| arg.starts_with(prefix.as_bytes()))
        })
    })
}

/// Delete the run folders under `root` that nothing needs any more: the
/// launcher recorded in them is gone (or none was recorded and the folder is
/// older than [`UNRECORDED_GRACE`]), and no process runs from them. Catches
/// folders a crash or a driver exit left behind. Returns how many it removed.
pub(super) fn sweep_run_dirs(root: &Path) -> usize {
    sweep_run_dirs_with(root, UNRECORDED_GRACE)
}

fn sweep_run_dirs_with(root: &Path, unrecorded_grace: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let keep = match launcher_alive(&dir) {
            Some(alive) => alive,
            None => entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_none_or(|age| age < unrecorded_grace),
        };
        if keep || in_use(&dir) {
            continue;
        }
        if std::fs::remove_dir_all(&dir).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests;
