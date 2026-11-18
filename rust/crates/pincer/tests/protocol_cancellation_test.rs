//! MCP `ping`, `notifications/cancelled` and stdin EOF while a tool runs,
//! over stdio through the built binary: direct `pincer mcp` and the proxy to
//! a test-owned daemon. Also requests piped in with stdin closed at once, as
//! a scripted smoke test sends them.
//!
//! The tool is a run_script that sleeps in 100 ms steps, so it needs no
//! desktop. A client that sends a progress token gets a progress
//! notification for each step, which shows the call is running.
//!
//! `PINCER_TEST_DRIVER_BIN` selects another binary (as for the rest of the
//! testkit), for example an older build to compare against.
#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pincer_testkit::{spawn_in_job, ChildReaper, CliDriver, IsolatedStateRoot};
use serde_json::{json, Value};

/// A run_script that sleeps `steps` times 100 ms and returns "finished".
fn sleeping_script(steps: u32) -> String {
    format!("for (let i = 0; i < {steps}; i++) {{ await pincer.sleep(100); }} return 'finished';")
}

struct Client {
    _reaper: ChildReaper,
    child_pid: u32,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    seen: Vec<Value>,
}

impl Client {
    /// Start `pincer <args>` with private state and no display, session bus
    /// or accessibility bus, so it cannot reach the developer's desktop.
    fn spawn(args: &[&str], state: &IsolatedStateRoot) -> Self {
        let mut command = Command::new(pincer_testkit::driver_binary());
        command
            .args(args)
            .envs(state.env())
            .env("PINCER_TELEMETRY_ENABLED", "false")
            .env("NO_AT_BRIDGE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for key in [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "AT_SPI_BUS_ADDRESS",
        ] {
            command.env_remove(key);
        }
        let mut child = spawn_in_job(&mut command).expect("spawn pincer mcp");
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let child_pid = child.id();
        let mut reaper = ChildReaper::new();
        reaper.push(child);
        Self {
            _reaper: reaper,
            child_pid,
            stdin,
            lines,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    /// The next message `matches` accepts, within `timeout`. Every message
    /// read on the way is kept in `seen`.
    fn wait_for(&mut self, timeout: Duration, matches: impl Fn(&Value) -> bool) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).ok()?;
            let message: Value = serde_json::from_str(&line).expect("stdout carries JSON lines");
            self.seen.push(message.clone());
            if matches(&message) {
                return Some(message);
            }
        }
    }

    fn reply(&mut self, id: u64, timeout: Duration) -> Option<Value> {
        self.wait_for(timeout, |message| message["id"] == id)
    }

    fn initialize(&mut self) {
        self.send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "cancellation-test", "version": "1"}
            }
        }));
        let reply = self.reply(1, Duration::from_secs(60));
        assert!(
            reply
                .as_ref()
                .is_some_and(|reply| reply.get("result").is_some()),
            "initialize: {reply:?}"
        );
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    /// Start a script of `steps` 100 ms sleeps as request `id` and wait for
    /// its first progress notification.
    fn start_script(&mut self, id: u64, steps: u32) {
        let token = format!("progress-{id}");
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {
                "name": "run_script",
                "arguments": {"script": sleeping_script(steps), "timeout_ms": 90000},
                "_meta": {"progressToken": token}
            }
        }));
        let progress = self.wait_for(Duration::from_secs(30), |message| {
            message["method"] == "notifications/progress"
                && message["params"]["progressToken"] == token
        });
        assert!(
            progress.is_some(),
            "run_script {id} sent no progress: {:?}",
            self.seen
        );
    }

    /// Wait up to `timeout` for the process to exit; true when it did.
    fn exited_within(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let alive = || {
            std::path::Path::new(&format!("/proc/{}", self.child_pid)).exists()
                && !std::fs::read_to_string(format!("/proc/{}/stat", self.child_pid))
                    .is_ok_and(|stat| stat.split_whitespace().nth(2) == Some("Z"))
        };
        while Instant::now() < deadline {
            if !alive() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        !alive()
    }
}

/// Ping during a minute-long call, cancel it by id, then end stdin during a
/// second, shorter one.
fn ping_cancel_and_eof(mut client: Client) {
    client.initialize();
    client.start_script(2, 600);

    // ping is answered while run_script still runs.
    let asked = Instant::now();
    client.send(json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}));
    let pong = client.reply(3, Duration::from_secs(5));
    assert_eq!(
        pong,
        Some(json!({"jsonrpc": "2.0", "id": 3, "result": {}})),
        "no ping reply while a tool ran: {:?}",
        client.seen
    );
    assert!(asked.elapsed() < Duration::from_secs(5));

    // Cancelling request 2 stops the script; the next call answers soon
    // after, and request 2 gets no response at all.
    let cancelled = Instant::now();
    client.send(json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": 2, "reason": "test cancel"}
    }));
    client.send(json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "get_config", "arguments": {}}
    }));
    let config = client.reply(4, Duration::from_secs(20));
    assert!(
        config.is_some(),
        "the call after the cancelled one never ran: {:?}",
        client.seen
    );
    assert!(
        cancelled.elapsed() < Duration::from_secs(20),
        "the cancelled script kept running"
    );
    assert!(
        !client.seen.iter().any(|message| message["id"] == 2),
        "a cancelled request got a response: {:?}",
        client.seen
    );

    // EOF while a call runs: the client is shutting the server down, as the
    // MCP stdio lifecycle does (close stdin, wait, then SIGTERM). The
    // running call and the one read after it finish and are answered in
    // order, then the process exits.
    client.start_script(5, 20);
    client.send(json!({
        "jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": {"name": "get_config", "arguments": {}}
    }));
    let closed = Instant::now();
    drop(client.stdin.take());
    let script = client.reply(5, Duration::from_secs(30));
    let script = script.unwrap_or_else(|| panic!("no reply to 5 after EOF: {:?}", client.seen));
    assert_ne!(script["result"]["isError"], true, "{script}");
    assert_eq!(
        script["result"]["structuredContent"]["value"], "finished",
        "{script}"
    );
    let config = client.reply(6, Duration::from_secs(10));
    assert!(
        config.is_some_and(|config| config["result"]["isError"] != true),
        "the call read before EOF did not run: {:?}",
        client.seen
    );
    assert!(
        client.exited_within(Duration::from_secs(20)),
        "pincer mcp did not exit after EOF"
    );
    assert!(closed.elapsed() < Duration::from_secs(40));
}

/// A scripted smoke test writes initialize and a few tool calls, closes
/// stdin at once and reads until the server exits: every request is
/// answered, none with "did not run".
fn piped_requests_all_run(mut client: Client) {
    let mut requests = vec![json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "smoke-test", "version": "1"}
        }
    })];
    for (id, tool) in [(2, "get_screen_size"), (3, "list_apps"), (4, "get_config")] {
        requests.push(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": {}}
        }));
    }
    for request in requests {
        client.send(request);
    }
    drop(client.stdin.take());
    for id in 1..=4 {
        let reply = client
            .reply(id, Duration::from_secs(60))
            .unwrap_or_else(|| panic!("no reply to {id}: {:?}", client.seen));
        assert!(
            !reply.to_string().contains("did not run"),
            "request {id} was dropped: {reply}"
        );
    }
    assert!(
        client.exited_within(Duration::from_secs(20)),
        "pincer mcp did not exit after EOF"
    );
}

#[test]
fn direct_mcp_answers_ping_and_stops_a_cancelled_run_script() {
    let state = IsolatedStateRoot::new().expect("state root");
    ping_cancel_and_eof(Client::spawn(&["mcp"], &state));
}

#[test]
fn daemon_proxy_answers_ping_and_stops_a_cancelled_run_script() {
    let daemon = CliDriver::new();
    if !daemon.available() {
        eprintln!("[cancellation] test daemon unavailable; skipping");
        return;
    }
    let socket = daemon.daemon_socket().unwrap().to_owned();
    let state = IsolatedStateRoot::new().expect("state root");
    ping_cancel_and_eof(Client::spawn(&["mcp", "--socket", &socket], &state));
}

#[test]
fn direct_mcp_answers_every_request_piped_before_eof() {
    let state = IsolatedStateRoot::new().expect("state root");
    piped_requests_all_run(Client::spawn(&["mcp"], &state));
}

#[test]
fn daemon_proxy_answers_every_request_piped_before_eof() {
    let daemon = CliDriver::new();
    if !daemon.available() {
        eprintln!("[cancellation] test daemon unavailable; skipping");
        return;
    }
    let socket = daemon.daemon_socket().unwrap().to_owned();
    let state = IsolatedStateRoot::new().expect("state root");
    piped_requests_all_run(Client::spawn(&["mcp", "--socket", &socket], &state));
}
