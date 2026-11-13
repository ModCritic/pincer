//! Admission checks shared by every platform's `launch_app`.

/// Refusal text for a `launch_app` call that tries to enable Chromium
/// DevTools or another debugger endpoint. `browser_prepare` owns that
/// transition for browsers.
pub const REMOTE_DEBUGGING_LAUNCH_REFUSAL: &str = "launch_app refuses debugger switches (Chromium remote-debugging, Node and Electron inspect, Firefox start-debugger-server); Chromium remote-debugging flags moved to browser_prepare so DevTools is never enabled on an unproven user profile";

/// Debugger switch names, compared after one or two leading dashes and any
/// `=value` are removed. Chromium's switch parser accepts a single dash as
/// well as two on POSIX (unverified: training data only, not checked against
/// `base/command_line.cc`), so both spellings are refused.
const DEBUGGER_SWITCHES: &[&str] = &[
    "remote-debugging-port",
    "remote-debugging-pipe",
    "remote-debugging-address",
    "inspect",
    "inspect-brk",
    "inspect-port",
    "start-debugger-server",
];

/// Whether a launch argument, executable path, or command line carries a
/// debugger switch in any spelling or case: a Chromium remote-debugging
/// switch, a Node or Electron inspector, or the Firefox debugger server.
///
/// The value is split on whitespace because callers pass whole command
/// lines (`launch_path`) as well as single arguments. This is a guard on the
/// literal argv only: a shell or interpreter that builds the switch at run
/// time is out of its reach, which is why launch_app needs the
/// `process_launch` adapter as well.
pub fn contains_remote_debugging_flag(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("--remote-debugging-port") || lower.contains("--remote-debugging-pipe") {
        return true;
    }
    lower.split_whitespace().any(|token| {
        let token = token.trim_matches(|c| c == '"' || c == '\'');
        let Some(switch) = token
            .strip_prefix("--")
            .or_else(|| token.strip_prefix('-'))
        else {
            return false;
        };
        let name = switch.split('=').next().unwrap_or_default();
        DEBUGGER_SWITCHES.contains(&name)
    })
}

#[cfg(test)]
mod tests {
    use super::contains_remote_debugging_flag;

    #[test]
    fn rejects_all_chromium_remote_debugging_spellings() {
        for flagged in [
            "--remote-debugging-port=0",
            "--REMOTE-DEBUGGING-PIPE",
            "/usr/bin/chrome --remote-debugging-port 9222",
            r#"C:\Program Files\Chrome\chrome.exe --remote-debugging-port 9222"#,
        ] {
            assert!(contains_remote_debugging_flag(flagged), "{flagged}");
        }
        for clean in [
            "--user-data-dir=/tmp/profile",
            r#"--user-data-dir=C:\Temp\profile"#,
        ] {
            assert!(!contains_remote_debugging_flag(clean), "{clean}");
        }
    }

    // TOOL-7 / LXTOOLS-6: the guard was a substring test for two
    // double-dash spellings, so a single dash, a different debugger switch,
    // or a Node or Firefox inspector passed.
    #[test]
    fn normalizes_dashes_and_values_and_covers_other_debugger_switches() {
        for flagged in [
            "-remote-debugging-port=9222",
            "-REMOTE-DEBUGGING-PIPE",
            "--remote-debugging-address=0.0.0.0",
            "-remote-debugging-address",
            "chromium -remote-debugging-port 9222",
            "--inspect",
            "--inspect=9229",
            "-inspect=127.0.0.1:9229",
            "--inspect-brk",
            "--inspect-brk=0",
            "--inspect-port=0",
            "code --inspect-port 9230",
            "--start-debugger-server",
            "-start-debugger-server=6000",
            "firefox --start-debugger-server ws:6000",
            r#""--inspect=9229""#,
        ] {
            assert!(contains_remote_debugging_flag(flagged), "{flagged}");
        }
        for clean in [
            "--inspector-theme=dark",
            "inspect",
            "/home/user/inspect.txt",
            "https://example.com/?inspect=1",
            "--new-window",
        ] {
            assert!(!contains_remote_debugging_flag(clean), "{clean}");
        }
    }
}
