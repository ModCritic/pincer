//! The stdin reader shared by the stdio MCP adapters: direct `pincer mcp`
//! and the proxy to a daemon (both in [`crate::proxy`]).
//!
//! Requests still run one at a time, in the order they arrive, so tool order
//! and the global input lock behave as before. What changed is that stdin is
//! read while a request runs:
//! - `ping` is answered at once, even while a tool runs;
//! - `notifications/cancelled` cancels the request it names, waiting or
//!   running. Per the MCP cancellation rules (2025-06-18 and later) a
//!   cancelled request gets no response, a request that already finished
//!   keeps its answer, and `initialize` cannot be cancelled;
//! - EOF stops the reading only: the running request and every request
//!   already read run to completion in order and are answered, then the
//!   server exits. Closing stdin is how an MCP client shuts a stdio server
//!   down (Lifecycle, Shutdown: close stdin, wait for the exit, then
//!   SIGTERM), and a scripted client may close it right after its last
//!   request. SIGTERM cancels what is still running;
//! - a read error or a failed write (nobody left to read the answers)
//!   cancels every request not yet answered: a waiting `tools/call` does
//!   not start, and a running long tool stops at its next step;
//! - a request whose `_meta.progressToken` is a string or an integer gets
//!   `notifications/progress` from long tools, at most every
//!   [`MIN_PROGRESS_INTERVAL`] and always increasing.
//!
//! Cancellation reaches the tool through [`pincer_core::request_control`].

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use pincer_core::mcp_wire::ProtocolEra;
use pincer_core::protocol::{Request, Response};
use pincer_core::request_control::{CancelToken, Progress, ProgressSink, RequestControl};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{
    channel, unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender,
};
use tracing::{debug, error, warn};

/// Shortest gap between two progress notifications for one request. The
/// last one (progress equal to total) is always sent.
pub(crate) const MIN_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Requests read ahead of the one running. A client that sends more waits,
/// as it did on the pipe before stdin was read concurrently.
const MAX_QUEUED_REQUESTS: usize = 64;

/// Lines for stdout, written in the order they were sent.
#[derive(Clone)]
pub(crate) struct Output(UnboundedSender<String>);

impl Output {
    pub(crate) fn channel() -> (Self, UnboundedReceiver<String>) {
        let (sender, receiver) = unbounded_channel();
        (Self(sender), receiver)
    }

    pub(crate) fn send_response(&self, response: &Response) {
        let serialized = serde_json::to_string(response).unwrap_or_else(|error| {
            format!(
                r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize error: {error}"}}}}"#
            )
        });
        let _ = self.0.send(serialized);
    }

    fn send_value(&self, value: &Value) {
        let _ = self.0.send(value.to_string());
    }
}

/// One admitted request, from the moment it is read until it is answered.
pub(crate) struct Ticket {
    /// The request id as JSON text, so `1` and `"1"` stay different ids.
    key: String,
    cancel: CancelToken,
    /// `initialize` cannot be cancelled by the client.
    cancellable: bool,
    /// Cancelled by `notifications/cancelled`: send no response.
    by_client: AtomicBool,
    /// Answered (or dropped); progress for it stops.
    done: AtomicBool,
}

/// Requests read but not yet answered, so a cancellation can find them.
#[derive(Clone, Default)]
pub(crate) struct InFlight(Arc<Mutex<Vec<Arc<Ticket>>>>);

impl InFlight {
    fn admit(&self, id: &Value, method: &str) -> Arc<Ticket> {
        let ticket = Arc::new(Ticket {
            key: id.to_string(),
            cancel: CancelToken::new(),
            cancellable: method != "initialize",
            by_client: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });
        self.lock().push(ticket.clone());
        ticket
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Ticket>>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Apply a client's `notifications/cancelled`. Unknown or finished ids
    /// are ignored, as the MCP spec asks. The client's own reason is only
    /// logged: it is client text, and the request gets no response anyway.
    fn cancel_by_client(&self, request_id: &Value) -> bool {
        let key = request_id.to_string();
        let mut matched = false;
        for ticket in self.lock().iter().filter(|ticket| ticket.key == key) {
            if !ticket.cancellable {
                debug!(request_id = %key, "ignoring a cancellation of initialize");
                continue;
            }
            ticket.by_client.store(true, Ordering::SeqCst);
            ticket.cancel.cancel("the client cancelled the request");
            matched = true;
        }
        matched
    }

    /// Cancel every request not yet answered.
    pub(crate) fn cancel_all(&self, reason: &str) {
        for ticket in self.lock().iter() {
            ticket.cancel.cancel(reason);
        }
    }

    fn finish(&self, ticket: &Arc<Ticket>) {
        ticket.done.store(true, Ordering::SeqCst);
        self.lock().retain(|other| !Arc::ptr_eq(other, ticket));
    }
}

/// The queue between the reader and the executor.
pub(crate) fn job_queue() -> (Sender<Job>, Receiver<Job>) {
    channel(MAX_QUEUED_REQUESTS)
}

/// Work for the executor, in arrival order.
pub(crate) enum Job {
    /// A reply that needs no execution: a parse error or a refused request.
    Reply(Response),
    Run(Admitted),
}

/// A request that passed admission, with its cancel token.
pub(crate) struct Admitted {
    pub(crate) request: Request,
    pub(crate) era: ProtocolEra,
    ticket: Arc<Ticket>,
    progress_token: Option<Value>,
}

impl Admitted {
    pub(crate) fn id(&self) -> Value {
        self.request.id.clone().unwrap_or(Value::Null)
    }

    /// The control the request runs under: its cancel token, and a progress
    /// sink when the request carries a progress token.
    pub(crate) fn control(&self, output: &Output) -> RequestControl {
        let control = RequestControl::new(self.ticket.cancel.clone());
        match &self.progress_token {
            Some(token) => control.with_progress(progress_sink(
                token.clone(),
                output.clone(),
                self.ticket.clone(),
            )),
            None => control,
        }
    }

    /// For a `tools/call` cancelled before it started: the request leaves
    /// the queue without running. Returns the reply to send, none when the
    /// client cancelled it.
    pub(crate) fn not_started(self, inflight: &InFlight) -> Result<Self, Option<Response>> {
        if self.request.method != "tools/call" || !self.ticket.cancel.is_cancelled() {
            return Ok(self);
        }
        inflight.finish(&self.ticket);
        if self.ticket.by_client.load(Ordering::SeqCst) {
            debug!(request_id = %self.ticket.key, "cancelled before it started");
            return Err(None);
        }
        let id = self.id();
        let name = self
            .request
            .tool_call()
            .map(|call| call.name)
            .unwrap_or_default();
        let reason = self
            .ticket
            .cancel
            .reason()
            .unwrap_or_else(|| "the request was cancelled".to_owned());
        let result = pincer_core::mcp_result::conforming_tool_result(
            &name,
            pincer_core::mcp_result::tool_error_result(
                format!("{name} did not run: cancelled ({reason})"),
                json!({"code": pincer_core::request_control::CANCELLED_CODE, "executed": false}),
            ),
        );
        let response = pincer_core::mcp_wire::finish_response(
            self.era,
            "tools/call",
            Response::ok(id, result),
        );
        Err(Some(pincer_core::mcp_text_results::for_client(
            "tools/call",
            response,
        )))
    }

    pub(crate) fn into_parts(self) -> (Request, Arc<Ticket>) {
        (self.request, self.ticket)
    }
}

impl Ticket {
    /// The request was answered with `response`: forget it, and return the
    /// response unless the client cancelled the request.
    pub(crate) fn deliver(
        self: &Arc<Self>,
        inflight: &InFlight,
        response: Response,
    ) -> Option<Response> {
        inflight.finish(self);
        if self.by_client.load(Ordering::SeqCst) {
            debug!(request_id = %self.key, "no response for a cancelled request");
            return None;
        }
        Some(response)
    }
}

/// `_meta.progressToken`, when it is a string or an integer.
fn progress_token(request: &Request) -> Option<Value> {
    let token = request
        .params
        .as_ref()?
        .get("_meta")?
        .get("progressToken")?;
    (token.is_string() || token.is_i64() || token.is_u64()).then(|| token.clone())
}

fn progress_sink(token: Value, output: Output, ticket: Arc<Ticket>) -> ProgressSink {
    struct Sent {
        progress: f64,
        at: Option<Instant>,
    }
    let sent = Mutex::new(Sent {
        progress: f64::NEG_INFINITY,
        at: None,
    });
    Arc::new(move |update: Progress| {
        if ticket.done.load(Ordering::SeqCst) || !update.progress.is_finite() {
            return;
        }
        let mut sent = sent.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Progress MUST increase with each notification.
        if update.progress <= sent.progress {
            return;
        }
        let last = update.total == Some(update.progress);
        if !last
            && sent
                .at
                .is_some_and(|at| at.elapsed() < MIN_PROGRESS_INTERVAL)
        {
            return;
        }
        sent.progress = update.progress;
        sent.at = Some(Instant::now());
        let mut params = json!({"progressToken": token, "progress": number(update.progress)});
        if let Some(total) = update.total.filter(|total| total.is_finite()) {
            params["total"] = number(total);
        }
        if !update.message.is_empty() {
            params["message"] = json!(update.message);
        }
        output.send_value(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": params,
        }));
    })
}

/// A whole number as a JSON integer, anything else as a float.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

/// Read requests from `reader` until EOF or `stop`. Admitted requests and
/// replies go to `jobs` in arrival order; `ping` is answered on `output` at
/// once and `notifications/cancelled` is applied at once. At EOF the
/// requests already read still run; a read error cancels them.
pub(crate) async fn read_requests<R>(
    mut reader: R,
    mut admit: impl FnMut(&Request) -> Result<ProtocolEra, Response>,
    jobs: Sender<Job>,
    output: Output,
    inflight: InFlight,
    stop: CancelToken,
) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        let read = tokio::select! {
            read = reader.read_line(&mut line) => read,
            _ = stop.cancelled() => return Ok(()),
        };
        let read = match read {
            Ok(read) => read,
            Err(error) => {
                inflight.cancel_all(&format!("reading stdin failed: {error}"));
                return Err(error);
            }
        };
        if read == 0 {
            // EOF: the client is done sending. What it sent still runs and is
            // answered; dropping `jobs` lets the executor end after it.
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        debug!(raw = trimmed, "→ MCP request");
        let request = match serde_json::from_str::<Request>(trimmed) {
            Ok(request) => request,
            Err(parse_error) => {
                error!("JSON parse error: {parse_error}");
                if jobs
                    .send(Job::Reply(Response::parse_error()))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                continue;
            }
        };
        if request.is_notification() {
            if request.method == "notifications/cancelled" {
                let params = request.params.as_ref();
                let reason = params
                    .and_then(|params| params.get("reason"))
                    .and_then(Value::as_str);
                match params.and_then(|params| params.get("requestId")) {
                    Some(id) if inflight.cancel_by_client(id) => {
                        debug!(request_id = %id, reason, "request cancelled by the client");
                    }
                    // Unknown, finished or malformed: ignored, as the spec asks.
                    other => debug!(request_id = ?other, reason, "ignored cancellation"),
                }
            }
            // Other notifications are intentionally dropped.
            continue;
        }
        let era = match admit(&request) {
            Ok(era) => era,
            Err(response) => {
                if jobs.send(Job::Reply(response)).await.is_err() {
                    return Ok(());
                }
                continue;
            }
        };
        if request.method == "ping" {
            // Answered at once, even while a tool runs.
            let id = request.id.clone().unwrap_or(Value::Null);
            output.send_response(&pincer_core::mcp_wire::finish_response(
                era,
                "ping",
                Response::ok(id, json!({})),
            ));
            continue;
        }
        let id = request.id.clone().unwrap_or(Value::Null);
        let ticket = inflight.admit(&id, &request.method);
        let progress_token = progress_token(&request);
        let admitted = Admitted {
            request,
            era,
            ticket,
            progress_token,
        };
        if jobs.send(Job::Run(admitted)).await.is_err() {
            // The executor ended (a signal); nothing more will run.
            return Ok(());
        }
    }
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    debug!(raw = %line, "← MCP response");
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

/// Run the reader and the executor together and write what they send to
/// `output` to `writer`, in order, until the executor ends. A failed write
/// cancels everything in flight and stops the reader, so the executor drains
/// and the error is returned; so is a read error.
pub(crate) async fn drive<W>(
    mut writer: W,
    mut lines: UnboundedReceiver<String>,
    reader: impl std::future::Future<Output = std::io::Result<()>>,
    executor: impl std::future::Future<Output = anyhow::Result<()>>,
    inflight: &InFlight,
    stop: &CancelToken,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    tokio::pin!(reader);
    tokio::pin!(executor);
    let mut reader_done = false;
    let mut writable = true;
    let mut failure: Option<anyhow::Error> = None;
    let outcome = loop {
        tokio::select! {
            biased;
            Some(line) = lines.recv() => {
                if writable {
                    if let Err(error) = write_line(&mut writer, &line).await {
                        writable = false;
                        warn!("writing to stdout failed: {error}");
                        inflight.cancel_all("writing to stdout failed");
                        stop.cancel("writing to stdout failed");
                        failure.get_or_insert(error.into());
                    }
                }
            }
            read = &mut reader, if !reader_done => {
                reader_done = true;
                if let Err(error) = read {
                    failure.get_or_insert(error.into());
                }
            }
            result = &mut executor => break result,
        }
    };
    // The executor's last reply may still be queued.
    while let Ok(line) = lines.try_recv() {
        if writable {
            if let Err(error) = write_line(&mut writer, &line).await {
                writable = false;
                failure.get_or_insert(error.into());
            }
        }
    }
    outcome?;
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pincer_core::mcp_wire::ProtocolSession;
    use tokio::io::BufReader;

    /// An executor for fake tools: "slow" runs until cancelled, "steady"
    /// runs for 300 ms unless cancelled first, and both report two progress
    /// steps first (the second one rate limited); anything else is answered
    /// at once.
    async fn fake_executor(
        mut jobs: Receiver<Job>,
        output: Output,
        inflight: InFlight,
        observed: Arc<Mutex<Vec<String>>>,
    ) -> anyhow::Result<()> {
        while let Some(job) = jobs.recv().await {
            let admitted = match job {
                Job::Reply(response) => {
                    output.send_response(&response);
                    continue;
                }
                Job::Run(admitted) => admitted,
            };
            let admitted = match admitted.not_started(&inflight) {
                Ok(admitted) => admitted,
                Err(reply) => {
                    observed.lock().unwrap().push("not started".into());
                    if let Some(reply) = reply {
                        output.send_response(&reply);
                    }
                    continue;
                }
            };
            let control = admitted.control(&output);
            let (request, ticket) = admitted.into_parts();
            let id = request.id.clone().unwrap_or(Value::Null);
            let name = request
                .tool_call()
                .map(|call| call.name)
                .unwrap_or_default();
            let observed_in = observed.clone();
            let response = control
                .scope(async move {
                    let runs_for = match name.as_str() {
                        "slow" => Duration::from_secs(3600),
                        "steady" => Duration::from_millis(300),
                        _ => return Response::ok(id, json!({"done": true})),
                    };
                    pincer_core::request_control::report_progress(1.0, Some(3.0), || {
                        "step 1".into()
                    });
                    pincer_core::request_control::report_progress(2.0, Some(3.0), || {
                        "step 2 (rate limited)".into()
                    });
                    tokio::select! {
                        _ = tokio::time::sleep(runs_for) => {
                            observed_in.lock().unwrap().push(format!("{name} finished"));
                            Response::ok(id, json!({"done": true, "tool": name}))
                        }
                        _ = pincer_core::request_control::cancelled() => {
                            observed_in.lock().unwrap().push(format!(
                                "{name} stopped: {}",
                                pincer_core::request_control::cancel_reason().unwrap_or_default()
                            ));
                            Response::ok(id, json!({"cancelled": true}))
                        }
                    }
                })
                .await;
            if let Some(response) = ticket.deliver(&inflight, response) {
                output.send_response(&response);
            }
        }
        Ok(())
    }

    struct Harness {
        to_server: tokio::io::DuplexStream,
        from_server: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
        observed: Arc<Mutex<Vec<String>>>,
        server: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Harness {
        fn start() -> Self {
            let (to_server, server_in) = tokio::io::duplex(64 * 1024);
            let (server_out, from_server) = tokio::io::duplex(64 * 1024);
            let observed = Arc::new(Mutex::new(Vec::new()));
            let server_observed = observed.clone();
            let server = tokio::spawn(async move {
                let (output, lines) = Output::channel();
                let (jobs_tx, jobs) = job_queue();
                let inflight = InFlight::default();
                let stop = CancelToken::new();
                let mut session = ProtocolSession::default();
                let reader = read_requests(
                    BufReader::new(server_in),
                    move |request| session.validate(request),
                    jobs_tx,
                    output.clone(),
                    inflight.clone(),
                    stop.clone(),
                );
                let executor = fake_executor(jobs, output, inflight.clone(), server_observed);
                drive(server_out, lines, reader, executor, &inflight, &stop).await
            });
            Self {
                to_server,
                from_server: BufReader::new(from_server).lines(),
                observed,
                server,
            }
        }

        async fn send(&mut self, value: Value) {
            self.to_server
                .write_all(format!("{value}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn next(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(5), self.from_server.next_line())
                .await
                .expect("a reply within 5 s")
                .unwrap()
                .expect("the server is still writing");
            serde_json::from_str(&line).unwrap()
        }
    }

    fn call(id: u64, name: &str, progress_token: Option<Value>) -> Value {
        let mut params = json!({"name": name, "arguments": {}});
        if let Some(token) = progress_token {
            params["_meta"] = json!({"progressToken": token});
        }
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params})
    }

    async fn initialized() -> Harness {
        let mut harness = Harness::start();
        harness
            .send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
            .await;
        assert_eq!(harness.next().await["id"], 1);
        harness
    }

    #[tokio::test]
    async fn ping_is_answered_while_a_call_runs() {
        let mut harness = initialized().await;
        harness.send(call(2, "steady", Some(json!("tok")))).await;
        let progress = harness.next().await;
        assert_eq!(progress["method"], "notifications/progress");
        assert_eq!(progress["params"]["progressToken"], "tok");
        assert_eq!(progress["params"]["progress"], 1);
        assert_eq!(progress["params"]["total"], 3);
        harness
            .send(json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}))
            .await;
        let pong = harness.next().await;
        assert_eq!(pong, json!({"jsonrpc": "2.0", "id": 3, "result": {}}));
        assert!(
            harness.observed.lock().unwrap().is_empty(),
            "the call is still running"
        );
        assert_eq!(harness.next().await["result"]["tool"], "steady");
        drop(harness.to_server);
        harness.server.await.unwrap().unwrap();
    }

    /// Start the fake tool `name` as call `id` and wait until it runs (its
    /// first progress notification), so what follows cannot reach it before
    /// it starts.
    async fn start(harness: &mut Harness, id: u64, name: &str) {
        harness.send(call(id, name, Some(json!(id)))).await;
        let progress = harness.next().await;
        assert_eq!(progress["method"], "notifications/progress", "{progress}");
    }

    #[tokio::test]
    async fn notifications_cancelled_stops_that_request_and_sends_no_response() {
        let mut harness = initialized().await;
        start(&mut harness, 2, "slow").await;
        // Waits behind the slow call; cancelled before it starts.
        harness.send(call(3, "slow", None)).await;
        harness.send(call(4, "fast", None)).await;
        // Cancelling an unknown id or initialize changes nothing.
        for request_id in [json!(99), json!(1), json!("2")] {
            harness
                .send(
                    json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": {"requestId": request_id}}),
                )
                .await;
        }
        harness
            .send(
                json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": {"requestId": 3}}),
            )
            .await;
        harness
            .send(
                json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": {"requestId": 2, "reason": "user pressed Esc"}}),
            )
            .await;
        // The next reply is the fast call's: 2 and 3 got none.
        let reply = harness.next().await;
        assert_eq!(reply["id"], 4, "{reply}");
        assert_eq!(reply["result"]["done"], true);
        assert_eq!(
            *harness.observed.lock().unwrap(),
            vec![
                "slow stopped: the client cancelled the request".to_owned(),
                "not started".to_owned(),
            ]
        );
        drop(harness.to_server);
        harness.server.await.unwrap().unwrap();
        assert!(
            harness.from_server.next_line().await.unwrap().is_none(),
            "nothing after EOF"
        );
    }

    /// EOF is how a client shuts a stdio server down, and a scripted client
    /// may close stdin right after its last request: everything already
    /// read runs to completion and is answered, in order, nothing is
    /// cancelled, and then the loop ends.
    #[tokio::test]
    async fn eof_lets_requests_already_read_finish() {
        let mut harness = initialized().await;
        start(&mut harness, 2, "steady").await;
        harness.send(call(3, "steady", None)).await;
        harness.send(call(4, "fast", None)).await;
        harness
            .send(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list", "params": {}}))
            .await;
        drop(harness.to_server);
        tokio::time::timeout(Duration::from_secs(5), harness.server)
            .await
            .expect("the loop ends once what was read is answered")
            .unwrap()
            .unwrap();
        let mut replies = Vec::new();
        while let Some(line) = harness.from_server.next_line().await.unwrap() {
            replies.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        let ids: Vec<Value> = replies.iter().map(|reply| reply["id"].clone()).collect();
        assert_eq!(ids, [json!(2), json!(3), json!(4), json!(5)], "{replies:?}");
        for reply in &replies[..3] {
            assert_eq!(reply["result"]["done"], true, "{reply}");
        }
        assert_eq!(
            *harness.observed.lock().unwrap(),
            vec!["steady finished".to_owned(), "steady finished".to_owned()]
        );
    }

    /// With nobody left to read the answers (the write to stdout fails),
    /// everything in flight is cancelled and the loop ends with the error.
    #[tokio::test]
    async fn a_failed_write_cancels_everything_in_flight() {
        let mut harness = initialized().await;
        start(&mut harness, 2, "slow").await;
        harness.send(call(3, "fast", None)).await;
        let Harness {
            mut to_server,
            from_server,
            observed,
            server,
        } = harness;
        drop(from_server);
        // Its reply is the next write, and it fails.
        to_server
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("a failed write ends the loop with stdin still open")
            .unwrap();
        assert!(ended.is_err(), "the write error is returned");
        assert_eq!(
            *observed.lock().unwrap(),
            vec![
                "slow stopped: writing to stdout failed".to_owned(),
                "not started".to_owned(),
            ]
        );
    }

    #[test]
    fn progress_increases_and_stops_after_completion() {
        let (output, mut lines) = Output::channel();
        let inflight = InFlight::default();
        let ticket = inflight.admit(&json!(7), "tools/call");
        let sink = progress_sink(json!(5), output, ticket.clone());
        let report = |progress: f64, total: Option<f64>| {
            sink(Progress {
                progress,
                total,
                message: String::new(),
            })
        };
        report(1.0, None);
        report(1.0, None); // not an increase
        report(0.5, None); // a decrease
        report(2.0, None); // too soon after the first
        report(3.0, Some(3.0)); // the last one is always sent
        inflight.finish(&ticket);
        report(4.0, Some(4.0)); // after completion
        let sent: Vec<Value> = std::iter::from_fn(|| lines.try_recv().ok())
            .map(|line| serde_json::from_str(&line).unwrap())
            .collect();
        assert_eq!(
            sent.iter()
                .map(|value| value["params"]["progress"].clone())
                .collect::<Vec<_>>(),
            vec![json!(1), json!(3)]
        );
        assert_eq!(sent[0]["params"]["progressToken"], 5);
        assert!(sent[0]["params"].get("total").is_none());
        assert!(sent[0]["params"].get("message").is_none());
        assert_eq!(number(2.5), json!(2.5));
    }
}
