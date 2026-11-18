//! Linux process enumeration via the /proc filesystem.
//!
//! Each /proc/<pid>/status file contains Name:, Pid:, PPid: etc.
//! /proc/<pid>/cmdline is the full command line (NUL-separated).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub cmdline: String,
}

fn is_process_live_state(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("State:")
                .and_then(|state| state.trim().chars().next())
        })
        .is_some_and(|state| !matches!(state, 'Z' | 'X'))
}

/// Return whether a PID still represents a live process.
///
/// A zombie remains visible under `/proc` until its parent reaps it, so an
/// existence check alone is not enough for callers that may query AT-SPI or
/// X11 state for the process. Treat zombies and dead processes as gone.
pub fn is_process_live(pid: u32) -> bool {
    let status = match fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => status,
        Err(_) => return false,
    };

    is_process_live_state(&status)
}

/// Parse the kernel start time (field 22, in clock ticks) from the contents
/// of `/proc/<pid>/stat`.
pub(crate) fn process_start_time_from_stat(stat: &str) -> Option<u64> {
    // `comm` is parenthesized and may contain spaces and closing parentheses.
    // Fields after its final `)` begin with state (field 3); starttime is
    // field 22.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// Kernel start-time token for one PID. Unlike the numeric PID alone, this
/// distinguishes a live process from a later process that reused its PID.
pub fn process_instance_id(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    process_start_time_from_stat(&stat)
}

/// One process instance as `/proc/<pid>/stat` describes it: its scheduler
/// state and its kernel start time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessStat {
    pub state: char,
    pub start_time: u64,
}

/// Parse state (field 3) and start time (field 22) from `/proc/<pid>/stat`.
pub(crate) fn process_stat_from_stat(stat: &str) -> Option<ProcessStat> {
    let state = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .next()?
        .chars()
        .next()?;
    Some(ProcessStat {
        state,
        start_time: process_start_time_from_stat(stat)?,
    })
}

/// Read one pid's [`ProcessStat`]; `Ok(None)` when no such process exists.
pub(crate) fn read_process_stat(pid: u32) -> std::io::Result<Option<ProcessStat>> {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => process_stat_from_stat(&stat).map(Some).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed /proc stat")
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// How the process first read as `before` ended, judged from a later read of
/// the same pid, or `None` while that same instance is still alive. A zombie
/// has exited and only waits for its parent to reap it.
pub(crate) fn process_exit_observation(
    before: ProcessStat,
    now: Option<ProcessStat>,
) -> Option<&'static str> {
    match now {
        None => Some("process_absent"),
        Some(now) if now.start_time != before.start_time => Some("pid_reused"),
        Some(now) if matches!(now.state, 'Z' | 'X') => Some("zombie"),
        Some(_) => None,
    }
}

/// Return all running processes by reading /proc/<pid>/status.
pub fn list_processes() -> Vec<ProcessInfo> {
    let mut result = Vec::new();
    let proc_dir = Path::new("/proc");
    let entries = match fs::read_dir(proc_dir) {
        Ok(e) => e,
        Err(_) => return result,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue, // Skip non-numeric entries.
        };

        let status_path = proc_dir.join(&*pid_str).join("status");
        let status = match fs::read_to_string(&status_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if !is_process_live_state(&status) {
            continue;
        }

        let proc_name = status
            .lines()
            .find(|l| l.starts_with("Name:"))
            .map(|l| l[5..].trim().to_owned())
            .unwrap_or_default();

        let cmdline_path = proc_dir.join(&*pid_str).join("cmdline");
        let cmdline = fs::read(cmdline_path)
            .ok()
            .map(|b| {
                // cmdline is NUL-separated; first entry is argv[0].
                let s = String::from_utf8_lossy(&b);
                s.split('\0').next().unwrap_or("").trim().to_owned()
            })
            .unwrap_or_default();

        result.push(ProcessInfo {
            pid,
            name: proc_name,
            cmdline,
        });
    }

    result.sort_by_key(|p| p.pid);
    result
}

/// What /proc says about running processes, read as the questions come.
///
/// A process's children are the union of the kernel's per-thread lists,
/// `/proc/<pid>/task/<tid>/children` (a child is listed under the thread
/// that started it, so every thread's list is read). Reading those lists
/// for a subtree costs far less than the status of every process: the input
/// tools ask on every call, and with about 370 processes the status scan
/// was some 740 file reads per key press. A kernel built without
/// CONFIG_PROC_CHILDREN keeps no lists; then each process's parent is read
/// once from every `/proc/<pid>/status`, into a table this value keeps for
/// its later questions.
#[derive(Default)]
pub(crate) struct ProcessFacts {
    by_parent: std::cell::OnceCell<HashMap<u32, Vec<u32>>>,
}

impl ProcessFacts {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `pid`'s children; none when `pid` is gone.
    pub(crate) fn children(&self, pid: u32) -> Vec<u32> {
        match child_lists(pid) {
            ChildLists::Read(children) => children,
            ChildLists::Gone => Vec::new(),
            ChildLists::Missing => self
                .by_parent
                .get_or_init(children_by_parent)
                .get(&pid)
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// `pid`'s descendants, breadth first (children first), each once and
    /// without `pid` itself, at most `cap` of them.
    pub(crate) fn descendants(&self, pid: u32, cap: usize) -> Vec<u32> {
        descendants_with(pid, cap, |current| self.children(current))
    }

    /// The program `pid` runs, as the file name of its argv[0], else its
    /// [`command_name`](Self::command_name) (a kernel thread or a zombie has
    /// no argv). The terminal check needs this name: the kernel's command
    /// name stops at 15 bytes, so `gnome-terminal-server` shows there as
    /// `gnome-terminal-`.
    pub(crate) fn program_name(pid: u32) -> Option<String> {
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let first = String::from_utf8_lossy(&cmdline)
            .split('\0')
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        if !first.is_empty() {
            return Path::new(&first)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .or(Some(first));
        }
        Self::command_name(pid)
    }

    /// The kernel's command name for `pid` (`/proc/<pid>/comm`, at most 15
    /// bytes). For a script started through its `#!` line it is the script's
    /// own name, where argv[0] names the interpreter (`python3`), so the
    /// AT-SPI fallback that matches apps by process name uses this one.
    pub(crate) fn command_name(pid: u32) -> Option<String> {
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// What reading one process's child lists found.
enum ChildLists {
    Read(Vec<u32>),
    /// The process (its task folder) does not exist.
    Gone,
    /// The kernel keeps no child lists.
    Missing,
}

fn child_lists(pid: u32) -> ChildLists {
    let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
        return ChildLists::Gone;
    };
    let mut children = Vec::new();
    let mut read_any = false;
    for task in tasks.flatten() {
        match fs::read_to_string(task.path().join("children")) {
            Ok(list) => {
                read_any = true;
                children.extend(
                    list.split_whitespace()
                        .filter_map(|kid| kid.parse::<u32>().ok()),
                );
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {}
            // A thread that exited between the listing and the read.
            Err(_) if !task.path().exists() => {}
            // The thread is there but its list is not: no CONFIG_PROC_CHILDREN.
            Err(_) => return ChildLists::Missing,
        }
    }
    if read_any {
        ChildLists::Read(children)
    } else if Path::new(&format!("/proc/{pid}")).exists() {
        ChildLists::Missing
    } else {
        ChildLists::Gone
    }
}

/// Every process's children, keyed by parent pid, from each
/// `/proc/<pid>/status`.
fn children_by_parent() -> HashMap<u32, Vec<u32>> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return HashMap::new();
    };
    children_table(entries.flatten().filter_map(|entry| {
        let child = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
        let status = fs::read_to_string(format!("/proc/{child}/status")).ok()?;
        let parent = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))?
            .trim()
            .parse::<u32>()
            .ok()?;
        Some((child, parent))
    }))
}

/// `(child, parent)` pairs as children keyed by parent.
fn children_table(pairs: impl IntoIterator<Item = (u32, u32)>) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (child, parent) in pairs {
        children.entry(parent).or_default().push(child);
    }
    children
}

/// [`ProcessFacts::descendants`] over any child relation.
fn descendants_with(root: u32, cap: usize, mut children: impl FnMut(u32) -> Vec<u32>) -> Vec<u32> {
    let mut found = Vec::new();
    let mut seen = HashSet::from([root]);
    let mut queue = VecDeque::from([root]);
    while let Some(current) = queue.pop_front() {
        for child in children(current) {
            if !seen.insert(child) {
                continue;
            }
            if found.len() >= cap {
                return found;
            }
            found.push(child);
            queue.push_back(child);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::ProcessFacts;
    use std::time::{Duration, Instant};

    /// Breadth first, each process once (a pid listed twice, or a list that
    /// leads back to the root, adds nothing), without the root, at most `cap`.
    #[test]
    fn descendants_are_breadth_first_unique_and_capped() {
        let tree = std::collections::HashMap::from([
            (1, vec![2, 3]),
            (2, vec![4, 3]),
            (3, vec![5]),
            (4, vec![1]),
        ]);
        let children = |pid: u32| tree.get(&pid).cloned().unwrap_or_default();
        assert_eq!(
            super::descendants_with(1, usize::MAX, children),
            [2, 3, 4, 5]
        );
        assert_eq!(super::descendants_with(1, 3, children), [2, 3, 4]);
        assert!(super::descendants_with(1, 0, children).is_empty());
        assert!(super::descendants_with(5, usize::MAX, children).is_empty());
    }

    /// The status table's family of a process holds only what descends from
    /// it (this test moved here from browser_platform with its walk).
    #[test]
    fn process_family_contains_only_transitive_descendants() {
        let table = super::children_table([(11, 10), (12, 11), (20, 1), (21, 20)]);
        let children = |pid: u32| table.get(&pid).cloned().unwrap_or_default();
        assert_eq!(super::descendants_with(10, usize::MAX, children), [11, 12]);
    }

    /// The kernel's child lists and the status table name the same child; a
    /// process that is gone has none.
    #[test]
    fn a_live_child_is_listed_by_both_sources() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (me, pid) = (std::process::id(), child.id());
        let facts = ProcessFacts::new();
        let listed = facts.children(me).contains(&pid);
        let in_table = super::children_by_parent()
            .get(&me)
            .is_some_and(|children| children.contains(&pid));
        let descends = facts.descendants(me, usize::MAX).contains(&pid);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            listed && in_table && descends,
            "{listed} {in_table} {descends}"
        );
        assert!(facts.children(pid).is_empty());
        assert!(facts.descendants(u32::MAX - 1, usize::MAX).is_empty());
    }

    /// argv[0] names a script's interpreter, the kernel's command name the
    /// script itself (cut to 15 bytes); without an argv (a zombie) both are
    /// the command name.
    #[test]
    fn program_and_command_names_of_a_script_and_a_zombie() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("script");
        std::fs::write(&source, "#!/bin/sh\nsleep 30\nexit 0\n").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = dir.path().join("pincer-shebang-script");
        crate::test_env::copy_executable(source.to_str().unwrap(), &script);
        let mut running = std::process::Command::new(&script).spawn().unwrap();
        let pid = running.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while ProcessFacts::command_name(pid).as_deref() != Some("pincer-shebang-")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let names = (
            ProcessFacts::program_name(pid),
            ProcessFacts::command_name(pid),
        );
        let _ = running.kill();
        let _ = running.wait();
        assert_eq!(
            names,
            (Some("sh".to_owned()), Some("pincer-shebang-".to_owned()))
        );

        let mut done = std::process::Command::new("true").spawn().unwrap();
        let zombie = done.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !std::fs::read_to_string(format!("/proc/{zombie}/status"))
            .is_ok_and(|status| status.contains("State:\tZ"))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let names = (
            ProcessFacts::program_name(zombie),
            ProcessFacts::command_name(zombie),
        );
        let _ = done.wait();
        assert_eq!(names, (Some("true".to_owned()), Some("true".to_owned())));
        assert_eq!(ProcessFacts::program_name(u32::MAX - 1), None);
        assert_eq!(ProcessFacts::command_name(u32::MAX - 1), None);
    }

    #[test]
    fn process_start_time_handles_spaces_in_comm() {
        let mut fields = vec!["S".to_owned()];
        fields.extend((4..=21).map(|field| field.to_string()));
        fields.push("987654".to_owned());
        let fields = fields.join(" ");
        for comm in [
            "fixture with spaces",
            "name with ) parentheses",
            "ends with )",
        ] {
            let stat = format!("42 ({comm}) {fields} 23");
            assert_eq!(
                super::process_start_time_from_stat(&stat),
                Some(987654),
                "{comm}"
            );
        }
        assert_eq!(super::process_start_time_from_stat("bad"), None);
        assert_eq!(
            super::process_start_time_from_stat("42 (short) S 1 2"),
            None
        );
    }

    #[test]
    fn exit_observation_tells_the_same_live_instance_from_its_end() {
        let stat = |state| super::ProcessStat {
            state,
            start_time: 7,
        };
        let parsed = super::process_stat_from_stat(&format!(
            "42 (a) b) S {} 7 23",
            (4..=21)
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        ));
        assert_eq!(parsed, Some(stat('S')));
        assert_eq!(
            super::process_exit_observation(stat('S'), Some(stat('R'))),
            None
        );
        assert_eq!(
            super::process_exit_observation(stat('S'), Some(stat('Z'))),
            Some("zombie")
        );
        assert_eq!(
            super::process_exit_observation(stat('S'), None),
            Some("process_absent")
        );
        let reused = super::ProcessStat {
            state: 'S',
            start_time: 8,
        };
        assert_eq!(
            super::process_exit_observation(stat('S'), Some(reused)),
            Some("pid_reused")
        );
    }

    #[test]
    fn process_states_fail_closed() {
        assert!(super::is_process_live_state(
            "Name:\ttest\nState:\tR (running)\n"
        ));
        assert!(super::is_process_live_state("State:\tS (sleeping)"));
        assert!(!super::is_process_live_state("State:\tZ (zombie)"));
        assert!(!super::is_process_live_state("State:\tX (dead)"));
        assert!(!super::is_process_live_state("Name:\ttest\n"));
        assert!(!super::is_process_live_state("State:\t"));
    }

    #[test]
    fn real_zombie_is_not_live_and_disappears_after_reaping() {
        let child = unsafe { libc::fork() };
        assert!(
            child >= 0,
            "fork failed: {}",
            std::io::Error::last_os_error()
        );
        if child == 0 {
            unsafe { libc::_exit(0) };
        }

        let pid = child as u32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut observed_zombie = false;
        while std::time::Instant::now() < deadline {
            if std::fs::read_to_string(format!("/proc/{pid}/status"))
                .ok()
                .is_some_and(|status| status.contains("State:\tZ"))
            {
                observed_zombie = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let live_while_zombie = super::is_process_live(pid);
        let mut status = 0;
        let reaped = unsafe { libc::waitpid(child, &mut status, 0) };

        assert!(observed_zombie, "child never entered zombie state");
        assert!(!live_while_zombie, "zombie pid was reported live");
        assert_eq!(reaped, child, "failed to reap child");
        assert!(!super::is_process_live(pid), "reaped pid was reported live");
    }
}
