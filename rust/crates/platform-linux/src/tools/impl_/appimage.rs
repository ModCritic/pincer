//! AppImages where FUSE can't mount them. Under a user namespace that does
//! not map root (the `unshare -rn ... --map-user=1000` wrapper that keeps the
//! driver off the network), the setuid-root `fusermount` shows up as owned by
//! the overflow uid, the AppImage runtime refuses it, and the AppImage exits
//! 127 with "No suitable fusermount binary found on the $PATH". The runtime's
//! documented `--appimage-extract-and-run` flag skips FUSE: it unpacks the
//! image to `$TMPDIR` and runs it from there (checked on one tested
//! AppImage inside that wrapper: its window came up).

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests;
