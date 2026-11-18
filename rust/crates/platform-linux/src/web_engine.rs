//! Which web engine a program or process runs.
//!
//! On a Chromium or Gecko surface an AT-SPI write can change a field's text,
//! which is all an accessibility read-back sees, without the page's scripts
//! ever getting the event a typed character fires. The tools ask this module
//! so they never report such a read-back as a confirmed change.

use std::path::{Path, PathBuf};

/// A browser engine whose page content Pincer reads through AT-SPI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebEngine {
    /// Chromium, its browsers, and Electron or CEF apps.
    Chromium,
    /// Gecko: Firefox, Thunderbird and their forks.
    Gecko,
}

impl WebEngine {
    /// The engine's name in a structured result.
    pub(crate) fn name(self) -> &'static str {
        match self {
            WebEngine::Chromium => "chromium",
            WebEngine::Gecko => "gecko",
        }
    }
}

/// The executable `/proc/<pid>/exe` names, without the " (deleted)" the
/// kernel appends once a package update replaced the file.
pub(crate) fn process_executable(pid: u32) -> Option<PathBuf> {
    let path = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let text = path.to_str()?;
    Some(PathBuf::from(text.strip_suffix(" (deleted)").unwrap_or(text)))
}

/// Whether `executable` is a Gecko application binary. Every Gecko app
/// (Firefox, Firefox ESR, Thunderbird, LibreWolf and the other forks, as
/// distribution packages, tarballs, Flatpaks or snaps) ships its engine as
/// `libxul.so` next to the binary, and nothing else does. Content processes
/// run the same binary, so this holds for them too.
pub(crate) fn is_gecko_executable(executable: &Path) -> bool {
    executable
        .parent()
        .is_some_and(|dir| dir.join("libxul.so").is_file())
}

/// Whether process `pid` runs a Gecko application ([`is_gecko_executable`]).
pub(crate) fn is_gecko_process(pid: u32) -> bool {
    process_executable(pid).is_some_and(|exe| is_gecko_executable(&exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_beside_libxul_is_gecko() {
        let dir = tempfile::tempdir().unwrap();
        let firefox = dir.path().join("firefox");
        std::fs::write(&firefox, b"\x7fELF").unwrap();
        assert!(!is_gecko_executable(&firefox));
        std::fs::write(dir.path().join("libxul.so"), b"\x7fELF").unwrap();
        assert!(is_gecko_executable(&firefox));
        // A folder called libxul.so is not the engine.
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join("libxul.so")).unwrap();
        assert!(!is_gecko_executable(&other.path().join("app")));
    }

    #[test]
    fn this_test_process_is_not_gecko() {
        assert!(!is_gecko_process(std::process::id()));
        assert_eq!(
            process_executable(std::process::id()),
            std::env::current_exe().ok()
        );
    }
}
