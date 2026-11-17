//! SIGTERM, SIGINT and SIGHUP for the long-lived transports.
//!
//! Direct `pincer mcp` and `pincer serve` hold state that only their own
//! cleanup releases: a held XTest mouse button, recordings (ffmpeg needs its
//! `q` to finish the file), the daemon socket and PID file. Without handlers
//! a signal kills the process before any of that runs. With them, a signal
//! ends the transport the way stdin EOF or a `shutdown` request does, and
//! each cleanup step gets [`CLEANUP_LIMIT`] before the process exits anyway.
//!
//! A signal the process inherited as ignored (`nohup`, a background job of a
//! non-interactive shell) stays ignored. Tokio never removes a handler once
//! it is installed, so only the production entry points install them, never
//! code that tests run inside the test harness. Other platforms keep their
//! default signal behavior: there, [`TerminationSignals::recv`] never fires.

use std::future::Future;
use std::time::Duration;

/// How long each cleanup step after a termination signal may take (finishing
/// the request in progress, shutting the SDK runtime down) before the process
/// exits without it.
pub(crate) const CLEANUP_LIMIT: Duration = Duration::from_secs(5);

/// Listeners for the signals that end a long-lived transport.
pub(crate) struct TerminationSignals {
    #[cfg(unix)]
    listeners: Vec<(&'static str, tokio::signal::unix::Signal)>,
}

impl TerminationSignals {
    /// Listen for SIGTERM, SIGINT and SIGHUP, except any the process
    /// inherited as ignored. Must run inside a Tokio runtime with I/O enabled.
    pub(crate) fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut listeners = Vec::new();
            for (name, number, kind) in [
                ("SIGTERM", libc::SIGTERM, SignalKind::terminate()),
                ("SIGINT", libc::SIGINT, SignalKind::interrupt()),
                ("SIGHUP", libc::SIGHUP, SignalKind::hangup()),
            ] {
                if !inherited_as_ignored(number) {
                    listeners.push((name, signal(kind)?));
                }
            }
            Ok(Self { listeners })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    /// Listeners that never fire, for callers that leave signal handling to
    /// whoever runs them (today only `run_serve`, which only tests call).
    #[cfg(unix)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn none() -> Self {
        Self {
            listeners: Vec::new(),
        }
    }

    /// Wait for the next termination signal and return its name.
    pub(crate) async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            std::future::poll_fn(|context| {
                for (name, listener) in &mut self.listeners {
                    if let std::task::Poll::Ready(Some(())) = listener.poll_recv(context) {
                        return std::task::Poll::Ready(*name);
                    }
                }
                std::task::Poll::Pending
            })
            .await
        }
        #[cfg(not(unix))]
        std::future::pending().await
    }
}

/// Whether the process ignores `signal`, as a parent arranges with `nohup` or
/// `trap '' HUP`. Pincer itself never ignores these signals.
#[cfg(unix)]
fn inherited_as_ignored(signal: libc::c_int) -> bool {
    let mut current = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: with a null new action, sigaction only writes the current
    // disposition into `current`, which is valid, zeroed storage for it.
    let read = unsafe { libc::sigaction(signal, std::ptr::null(), current.as_mut_ptr()) };
    // SAFETY: zeroed storage is a valid sigaction, and a successful call
    // filled it in.
    read == 0 && unsafe { current.assume_init() }.sa_sigaction == libc::SIG_IGN
}

/// Await `work`, a step already under way: a request in progress or a
/// cleanup step. Until a termination signal arrives it may take as long as it
/// needs. Once one has arrived, before or during the step, it gets
/// [`CLEANUP_LIMIT`] and then fails, so a request is not cut off between its
/// effect and its bookkeeping (a pressed button and its hold record) unless
/// it stalls. The first signal's name goes to `terminated`.
pub(crate) async fn finish_step<T>(
    signals: &mut TerminationSignals,
    terminated: &mut Option<&'static str>,
    step: &str,
    work: impl Future<Output = T>,
) -> anyhow::Result<T> {
    tokio::pin!(work);
    let signal = match *terminated {
        Some(signal) => signal,
        None => {
            let signal = tokio::select! {
                output = &mut work => return Ok(output),
                signal = signals.recv() => signal,
            };
            *terminated = Some(signal);
            signal
        }
    };
    within_limit(signal, step, work).await
}

/// Await `work` for at most [`CLEANUP_LIMIT`] after `signal`.
async fn within_limit<T>(
    signal: &str,
    step: &str,
    work: impl Future<Output = T>,
) -> anyhow::Result<T> {
    tokio::time::timeout(CLEANUP_LIMIT, work)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "{step} did not finish within {} seconds of {signal}",
                CLEANUP_LIMIT.as_secs()
            )
        })
}

#[cfg(all(test, unix))]
pub(crate) mod test_child {
    use std::io::BufRead as _;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// One test of this binary running again in a child process, which the
    /// parent test then signals.
    pub(crate) struct SignalTestChild {
        pub(crate) child: Child,
        stdout_lines: mpsc::Receiver<String>,
        stdout_seen: Vec<String>,
        stderr: Option<std::thread::JoinHandle<String>>,
    }

    /// How a signaled child ended: its status (None when it outlived the
    /// wait and was killed) and everything it wrote.
    pub(crate) struct SignalTestOutcome {
        pub(crate) status: Option<ExitStatus>,
        pub(crate) stdout: String,
        pub(crate) stderr: String,
    }

    impl SignalTestOutcome {
        /// The child exited by itself with status 0 and its test passed.
        pub(crate) fn passed(&self) -> bool {
            self.status.is_some_and(|status| status.success()) && self.stdout.contains("1 passed")
        }
    }

    impl std::fmt::Display for SignalTestOutcome {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.status {
                Some(status) => write!(formatter, "child ended with {status}")?,
                None => write!(formatter, "child did not exit and was killed")?,
            }
            write!(
                formatter,
                "\nstdout:\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        }
    }

    /// Start the single test `name` again in a fresh copy of this test
    /// binary, with `set` added to its environment. The child gets the
    /// private HOME and state directories of `state` and no display, Wayland,
    /// D-Bus session or AT-SPI address, so it cannot act on the developer's
    /// desktop. Its stdin is a pipe that stays open until the child is
    /// dropped or waited for.
    pub(crate) fn spawn(
        name: &str,
        state: &pincer_testkit::IsolatedStateRoot,
        set: &[(&str, &std::ffi::OsStr)],
    ) -> SignalTestChild {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(crate::TEST_CHILD_ENV, "1")
            .envs(state.env())
            .env("NO_AT_BRIDGE", "1")
            .env("PINCER_TELEMETRY_ENABLED", "false")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "AT_SPI_BUS_ADDRESS",
        ] {
            command.env_remove(key);
        }
        for (key, value) in set {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, stdout_lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut stderr_pipe = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut stderr_pipe, &mut text);
            text
        });
        SignalTestChild {
            child,
            stdout_lines,
            stdout_seen: Vec::new(),
            stderr: Some(stderr),
        }
    }

    impl SignalTestChild {
        /// Wait up to `timeout` for a stdout line that `matches`.
        pub(crate) fn wait_for_line(
            &mut self,
            timeout: Duration,
            matches: impl Fn(&str) -> bool,
        ) -> Option<String> {
            let deadline = Instant::now() + timeout;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let line = self.stdout_lines.recv_timeout(left).ok()?;
                self.stdout_seen.push(line.clone());
                if matches(&line) {
                    return Some(line);
                }
            }
        }

        /// Send `signal` to the child and wait up to `timeout` for it to
        /// exit; kill it after that. The child's stdin stays open until it
        /// has exited, like an MCP client that signals its server without
        /// closing the pipe first.
        pub(crate) fn signal_and_wait(
            mut self,
            signal: libc::c_int,
            timeout: Duration,
        ) -> SignalTestOutcome {
            let pid = libc::pid_t::try_from(self.child.id()).unwrap();
            // SAFETY: kill has no memory preconditions; pid is our own child,
            // which has not been waited for, so the pid cannot be reused yet.
            assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
            let deadline = Instant::now() + timeout;
            let status = loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break Some(status);
                }
                if Instant::now() >= deadline {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            drop(self.child.stdin.take());
            self.stdout_seen.extend(self.stdout_lines.iter());
            SignalTestOutcome {
                status,
                stdout: self.stdout_seen.join("\n"),
                stderr: self.stderr.take().unwrap().join().unwrap(),
            }
        }

        /// Kill the child after a failed setup step and panic with `message`
        /// and the child's output.
        pub(crate) fn fail(self, message: &str) -> ! {
            let outcome = self.signal_and_wait(libc::SIGKILL, Duration::from_secs(5));
            panic!("{message}\n{outcome}");
        }
    }

    /// An SDK driver for a signal test child: no cursor overlay and no
    /// desktop preparation (no X authority, session bus or AT-SPI setup).
    pub(crate) fn headless_driver() -> std::sync::Arc<pincer_sdk::Pincer> {
        pincer_sdk::Pincer::create_for_host(pincer_sdk::DriverHostOptions {
            cursor: cursor_overlay::CursorConfig {
                enabled: false,
                ..cursor_overlay::CursorConfig::default()
            },
            host_owns_permission_ux: false,
            host_bundle_id: None,
            claude_code_compatibility: false,
            prepare_desktop_environment: false,
            register_host_tools: None,
            authorization_host: None,
            activity_observer: None,
        })
    }
}
