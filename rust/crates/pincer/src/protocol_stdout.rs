//! The protocol stream's own handle on stdout, for every stdio transport:
//! direct `pincer mcp`, the proxy to a daemon, the MCP envelope relay and
//! the private worker.
//!
//! These transports answer on fd 1, and so does every `println!` (or C
//! `printf`) anywhere in the process. clipboard-rs 0.3.5 prints "Somebody
//! else owns the clipboard now" there when another app takes the X11
//! clipboard after clipboard_write, and an MCP client reading JSON lines then
//! read a line that is not JSON. [`claim`] moves the stream to a private
//! descriptor and points fd 1 at stderr, so a stray print from any library
//! lands in the log instead of the protocol (a client that merges stderr
//! into stdout gets it in the stream anyway, as it gets the log). CLI
//! commands that print their results never call it, so their stdout is
//! unchanged.
//!
//! Unix only. On Windows `println!` writes to whatever handle
//! `GetStdHandle(STD_OUTPUT_HANDLE)` returns on each call and C code writes
//! to the C runtime's own descriptor 1, so the same move needs
//! `SetStdHandle` (the windows crate's Win32_System_Console feature, which
//! this crate does not enable) and the C runtime's `_dup2`; the stray print
//! this guards against is in clipboard-rs's X11 backend. There the stream
//! stays on the process's stdout as before.

/// The writer a stdio transport answers on.
#[cfg(unix)]
pub(crate) type AsyncWriter = tokio::fs::File;
/// The writer a stdio transport answers on.
#[cfg(not(unix))]
pub(crate) type AsyncWriter = tokio::io::Stdout;

/// The protocol stream, once [`claim`] took it from fd 1.
#[cfg(unix)]
static CLAIMED: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

/// Take stdout for the protocol stream: the stream moves to a new
/// descriptor (close-on-exec, so a launched app never holds the client's
/// pipe) and fd 1 becomes a copy of stderr, or of /dev/null when stderr is
/// closed. Anything a Rust `print!` left buffered goes to stderr too. Call
/// it before the transport (or anything it starts) can print; later calls
/// return the same stream.
#[cfg(unix)]
pub(crate) fn claim() -> std::io::Result<std::fs::File> {
    use std::io::Write as _;
    use std::os::fd::{AsFd as _, AsRawFd as _};
    let mut claimed = CLAIMED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(stream) = claimed.as_ref() {
        return stream.try_clone();
    }
    // Hold std's stdout lock so no Rust print lands between the two steps.
    let mut stdout = std::io::stdout().lock();
    // try_clone_to_owned duplicates with F_DUPFD_CLOEXEC.
    let stream = std::fs::File::from(stdout.as_fd().try_clone_to_owned()?);
    // SAFETY: dup2 takes plain descriptor numbers and touches no memory.
    if unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } < 0 {
        let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
        // SAFETY: as above; `null` is open for the duration of the call.
        if unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // A partial line printed before the claim now goes to stderr instead
    // of ahead of the first reply.
    let _ = stdout.flush();
    *claimed = Some(stream.try_clone()?);
    Ok(stream)
}

/// [`claim`] at the start of a transport, before it builds anything that
/// could print. Nothing to do on Windows (see the module comment).
pub(crate) fn claim_at_startup() -> std::io::Result<()> {
    #[cfg(unix)]
    claim()?;
    Ok(())
}

/// [`claim`] as a Tokio writer.
#[cfg(unix)]
pub(crate) fn async_writer() -> std::io::Result<AsyncWriter> {
    claim().map(tokio::fs::File::from_std)
}

/// The process's stdout; see the module comment for why Windows keeps it.
#[cfg(not(unix))]
pub(crate) fn async_writer() -> std::io::Result<AsyncWriter> {
    Ok(tokio::io::stdout())
}

/// The writer a blocking transport answers on.
#[cfg(unix)]
pub(crate) type BlockingWriter = std::io::BufWriter<std::fs::File>;
/// The writer a blocking transport answers on.
#[cfg(not(unix))]
pub(crate) type BlockingWriter = std::io::StdoutLock<'static>;

/// [`claim`], buffered: flush after each message.
#[cfg(unix)]
pub(crate) fn blocking_writer() -> std::io::Result<BlockingWriter> {
    claim().map(std::io::BufWriter::new)
}

/// The process's stdout, locked; see the module comment for why Windows
/// keeps it.
#[cfg(not(unix))]
pub(crate) fn blocking_writer() -> std::io::Result<BlockingWriter> {
    Ok(std::io::stdout().lock())
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write as _;

    /// In a child copy of this test binary whose stdout and stderr are
    /// pipes: after the claim a `println!` and a raw write to fd 1 reach
    /// stderr, the claimed stream reaches stdout, and the stream does not
    /// leak into launched programs.
    #[test]
    fn a_claim_sends_stray_prints_to_stderr_and_keeps_the_stream() {
        const NAME: &str =
            "protocol_stdout::tests::a_claim_sends_stray_prints_to_stderr_and_keeps_the_stream";
        const CHILD_ENV: &str = "PINCER_TEST_PROTOCOL_STDOUT_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            print!("partial line from before the claim");
            let mut stream = super::claim().unwrap();
            println!("stray println");
            // SAFETY: writes a static buffer to fd 1.
            unsafe {
                libc::write(
                    libc::STDOUT_FILENO,
                    b"stray raw write\n".as_ptr().cast(),
                    16,
                )
            };
            writeln!(stream, "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}").unwrap();
            // A second claim returns the same stream, not stderr.
            let mut again = super::claim().unwrap();
            writeln!(again, "{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{}}}}").unwrap();
            // SAFETY: F_GETFD reads the descriptor's flags.
            let flags =
                unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&stream), libc::F_GETFD) };
            assert!(flags & libc::FD_CLOEXEC != 0, "the stream is inherited");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let report = format!("{}\nstdout:\n{stdout}\nstderr:\n{stderr}", output.status);
        assert!(output.status.success(), "{report}");
        // The test harness wrote "running 1 test" and "test <name> ... "
        // before the claim; the stream starts after them.
        let start = stdout.find("{\"jsonrpc\"").unwrap_or(stdout.len());
        let stream: Vec<&str> = stdout[start..].lines().collect();
        assert_eq!(
            stream,
            [
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}",
                "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}",
            ],
            "{report}"
        );
        for printed in [
            "partial line from before the claim",
            "stray println",
            "stray raw write",
            "1 passed",
        ] {
            assert!(stderr.contains(printed), "{printed:?} missing: {report}");
            assert!(!stdout.contains(printed), "{printed:?} on stdout: {report}");
        }
    }
}
