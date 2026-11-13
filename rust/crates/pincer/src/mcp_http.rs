//! Streamable-HTTP MCP transport for the daemon (trycua/cua#1799).
//!
//! Why: over **stdio**, one `pincer mcp` process is a single pipe, so all of
//! a client's tool calls — including those from multiple subagents — serialize.
//! The daemon itself is already concurrent (a task per connection). This HTTP
//! front-end lets each agent open its **own** connection to the shared daemon:
//! per-connection FIFO ordering keeps a single agent's ordered calls correct,
//! while distinct connections run truly in parallel. That parallelism is sound
//! because the per-`(pid, window_id)` element cache + per-session cursor make
//! concurrent cross-connection actions non-colliding (see the session-identity
//! work in trycua/cua#1799).
//!
//! Minimal hand-rolled HTTP/1.1 — no new dependency, mirroring how the daemon
//! already hand-rolls its UDS line protocol. `POST` with a JSON-RPC body → the
//! shared MCP dispatch (`pincer_core::server::handle_request`) → an
//! `application/json` JSON-RPC response. Each TCP connection is its own task, so
//! N clients run concurrently. (SSE streaming + transport-level session headers
//! are a follow-up; tool calls are request/response, so `application/json`
//! suffices.) Loopback-only — a local automation surface, not a public endpoint.
//!
//! Every local account can reach a loopback port, so nothing is read or
//! allocated for a request body until its headers pass the Origin and bearer
//! checks. Connections are capped, and a connection that has not yet sent an
//! authorized request must send a complete request head within a time limit.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use pincer_core::protocol::{Request, Response};
use pincer_core::server::{
    handle_request_with_transport_session, tool_observation_timer, StdioExecutionPath,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tracing::{debug, info, warn};

const MCP_HTTP_TOKEN_ENV: &str = "PINCER_MCP_HTTP_TOKEN";

/// Connections served at once. Each one is a task and a descriptor, so the
/// cap bounds what clients without the token can tie up.
const MAX_CONNECTIONS: usize = 64;
/// Time allowed for a whole request head (request line and headers). Until a
/// connection has sent an authorized request, the clock starts at accept, so
/// a client without the token cannot hold a slot for long. After that it
/// starts at the first byte of each request, so an authenticated agent may
/// leave its keep-alive connection idle between tool calls.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Time allowed for an authorized request body.
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a refused connection may take to accept the error response and
/// close, so the client reads the error instead of a connection reset.
const LINGER_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;

/// Resolve the configured HTTP MCP port: `PINCER_MCP_HTTP_PORT` (> 0), or
/// `None` (disabled — the daemon spawns the listener only when this is set).
pub fn configured_port() -> anyhow::Result<Option<u16>> {
    configured_port_from(std::env::var_os("PINCER_MCP_HTTP_PORT"))
}

fn configured_port_from(value: Option<std::ffi::OsString>) -> anyhow::Result<Option<u16>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("PINCER_MCP_HTTP_PORT must be valid UTF-8"))?;
    let port = value
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("PINCER_MCP_HTTP_PORT must be an integer from 1 to 65535"))?;
    if port == 0 {
        anyhow::bail!("PINCER_MCP_HTTP_PORT must be greater than zero");
    }
    Ok(Some(port))
}

/// A running MCP HTTP listener. Dropping it stops the listener and closes its
/// connections.
#[must_use = "dropping the Server stops the MCP HTTP listener"]
pub struct Server(tokio::task::JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Bind the HTTP MCP listener to `127.0.0.1:port` (loopback only) and serve it
/// on a task. The port is bound before this returns, and a bind failure is an
/// error: if another process holds the port, it would otherwise receive every
/// client's bearer token and tool traffic.
pub fn spawn(sdk: Arc<crate::sdk_adapter::SdkAdapter>, port: u16) -> anyhow::Result<Server> {
    let token = Arc::<str>::from(configured_auth_token().map_err(anyhow::Error::msg)?);
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let listener = std::net::TcpListener::bind(addr)
        .and_then(|listener| {
            listener.set_nonblocking(true)?;
            TcpListener::from_std(listener)
        })
        .map_err(|error| {
            anyhow::anyhow!(
                "MCP HTTP transport cannot listen on http://{addr}/mcp: {error}. Pincer does \
                 not start without it, so that another process holding port {port} never \
                 receives the bearer token. Stop that process or set PINCER_MCP_HTTP_PORT \
                 to a free port."
            )
        })?;
    info!("MCP HTTP transport listening on http://{addr}/mcp (one connection per agent → parallel)");
    Ok(Server(tokio::spawn(serve_listener(listener, sdk, token))))
}

async fn serve_listener(
    listener: TcpListener,
    sdk: Arc<crate::sdk_adapter::SdkAdapter>,
    token: Arc<str>,
) {
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    // Owned here so that stopping the listener also closes its connections.
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let Ok(permit) = permits.clone().try_acquire_owned() else {
                        debug!(%peer, "MCP HTTP connection refused: {MAX_CONNECTIONS} connections already open");
                        refuse_over_capacity(stream);
                        continue;
                    };
                    let sdk = sdk.clone();
                    let token = token.clone();
                    connections.spawn(async move {
                        let _permit = permit;
                        if let Err(e) = serve_conn(stream, sdk, token).await {
                            debug!(%peer, "MCP HTTP connection closed: {e}");
                        }
                    });
                }
                Err(e) => {
                    warn!("MCP HTTP accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

/// Answer a connection over [`MAX_CONNECTIONS`] with 503 and close it at once,
/// without a task, so a flood of connections cannot pile up.
fn refuse_over_capacity(stream: TcpStream) {
    if let Ok(mut stream) = stream.into_std() {
        use std::io::Write as _;
        // A new socket has an empty send buffer, so this short write completes
        // at once even though the socket is nonblocking.
        let _ = stream.write_all(&http_response(
            503,
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Too many MCP HTTP connections are open; close idle connections and retry"}}"#,
            false,
        ));
    }
}

/// Handle one TCP connection: a keep-alive loop of HTTP requests. Requests on a
/// single connection stay FIFO-ordered (so one agent's ordered calls are safe);
/// parallelism comes from DISTINCT connections, each its own task.
async fn serve_conn(
    mut stream: TcpStream,
    sdk: Arc<crate::sdk_adapter::SdkAdapter>,
    token: Arc<str>,
) -> anyhow::Result<()> {
    let transport_session = format!("http-{}", uuid::Uuid::new_v4());
    struct TransportCleanup {
        sdk: Arc<crate::sdk_adapter::SdkAdapter>,
        transport_session: String,
    }
    impl Drop for TransportCleanup {
        fn drop(&mut self) {
            self.sdk.end_transport_sessions(&self.transport_session);
        }
    }
    let _cleanup = TransportCleanup {
        sdk: sdk.clone(),
        transport_session: transport_session.clone(),
    };
    let mut authenticated = false;
    loop {
        let head = match read_request_head(&mut stream, authenticated).await? {
            HeadRead::Closed => return Ok(()), // clean EOF
            HeadRead::TimedOut => {
                refuse(
                    &mut stream,
                    408,
                    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"The request headers did not arrive in time"}}"#,
                )
                .await;
                return Ok(());
            }
            HeadRead::Head(head) => parse_request_head(&head, &token),
        };
        // Decide Origin and Authorization from the headers alone, before any
        // body byte is read or allocated. A refused connection closes, since
        // its unread body would otherwise be parsed as the next request.
        if head.has_origin {
            refuse(
                &mut stream,
                403,
                br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Browser-origin requests are forbidden"}}"#,
            )
            .await;
            return Ok(());
        }
        if !head.authorized {
            refuse(
                &mut stream,
                401,
                br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"Authentication required"}}"#,
            )
            .await;
            return Ok(());
        }
        authenticated = true;
        let Some(content_length) = head.content_length else {
            refuse(
                &mut stream,
                400,
                br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Send one valid Content-Length header"}}"#,
            )
            .await;
            return Ok(());
        };
        if content_length > MAX_BODY {
            refuse(
                &mut stream,
                413,
                br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Request body exceeds the 16 MiB limit"}}"#,
            )
            .await;
            return Ok(());
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            match tokio::time::timeout(BODY_TIMEOUT, stream.read_exact(&mut body)).await {
                Ok(read) => {
                    read?;
                }
                Err(_) => {
                    refuse(
                        &mut stream,
                        408,
                        br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"The request body did not arrive in time"}}"#,
                    )
                    .await;
                    return Ok(());
                }
            }
        }
        let keep_alive = head.keep_alive;
        if !head.method.eq_ignore_ascii_case("POST") {
            write_http(
                &mut stream,
                405,
                br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Use POST /mcp with a JSON-RPC body"}}"#,
                keep_alive,
            )
            .await?;
        } else {
            if let Some(error) =
                unsupported_protocol_error(&body, head.protocol_version.as_deref())
            {
                write_http(&mut stream, 400, error.as_bytes(), keep_alive).await?;
                if !keep_alive {
                    return Ok(());
                }
                continue;
            }
            match dispatch(&body, &sdk, &transport_session).await {
                Some(resp_json) => {
                    write_http(&mut stream, 200, resp_json.as_bytes(), keep_alive).await?
                }
                // Notification (no id): MCP wants 202 Accepted, no body.
                None => write_http(&mut stream, 202, b"", keep_alive).await?,
            }
        }
        // Honor the client's Connection: close (and HTTP/1.0 default) — close the
        // connection so a client reading until EOF doesn't hang. Parallelism comes
        // from distinct connections regardless of keep-alive.
        if !keep_alive {
            return Ok(());
        }
    }
}

/// Send an error response, then close without a reset: stop writing and
/// discard what the client already sent, for at most [`LINGER_TIMEOUT`], so
/// the client can read the response. Nothing read here is kept.
async fn refuse(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let deadline = Instant::now() + LINGER_TIMEOUT;
    let written =
        tokio::time::timeout_at(deadline, write_http(stream, status, body, false)).await;
    if !matches!(written, Ok(Ok(()))) {
        return;
    }
    let _ = stream.shutdown().await;
    let mut discard = [0u8; 8192];
    while let Ok(Ok(read)) = tokio::time::timeout_at(deadline, stream.read(&mut discard)).await {
        if read == 0 {
            break;
        }
    }
}

/// Parse a JSON-RPC request body and dispatch via the shared MCP handler. Returns
/// `Some(json)` for a request, or `None` for a notification (no `id`). Applies the
/// caller-declared `session` identity so HTTP behaves identically to stdio.
async fn dispatch(
    body: &[u8],
    sdk: &Arc<crate::sdk_adapter::SdkAdapter>,
    transport_session: &str,
) -> Option<String> {
    if let Some(error) = unsupported_protocol_error(body, None) {
        return Some(error);
    }
    let mut req: Request = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(_) => return Some(serialize(&Response::parse_error())),
    };
    req.id.as_ref()?;
    apply_session_identity(&mut req, transport_session);
    let session_context = req.tool_call().ok().and_then(|call| {
        sdk.begin_tool_call(
            &call.name,
            &call.args,
            pincer_core::session::SessionTransport::McpHttp,
            pincer_core::session::SessionClientKind::Mcp,
        )
    });
    let id = req.id.clone().unwrap_or(serde_json::Value::Null);
    let timer = http_tool_observation_timer(&req, |name| sdk.is_known_tool(name));
    let method = req.method.clone();
    let response =
        handle_request_with_transport_session(req, id, sdk.as_ref(), transport_session).await;
    if let Some(timer) = timer {
        let outcome = timer.finish(&response);
        if let Some(context) = session_context {
            context.complete(&outcome);
        }
    }
    // After the observation, so it reads the structured result.
    let response = pincer_core::mcp_text_results::for_client(&method, response);
    Some(serialize(&response))
}

fn is_legacy_protocol(version: &str) -> bool {
    matches!(version, "2024-11-05" | "2025-03-26" | "2025-06-18")
}

// HTTP still uses connection-scoped sessions. A legacy error permits dual-era
// clients to fall back to initialize; -32022 would falsely identify a modern
// endpoint and tell clients to retry per-request version negotiation here.
fn unsupported_protocol_error(body: &[u8], header: Option<&str>) -> Option<String> {
    let body: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let requested = header
        .filter(|version| !is_legacy_protocol(version))
        .map(serde_json::Value::from)
        .or_else(|| {
            let params = body.get("params")?;
            params
                .get("_meta")
                .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
                .filter(|version| !version.as_str().is_some_and(is_legacy_protocol))
                .cloned()
        })
        .or_else(|| {
            (body.get("method")?.as_str()? == "server/discover")
                .then(|| serde_json::Value::from("2026-07-28"))
        })?;
    Some(serde_json::json!({
        "jsonrpc": "2.0",
        "id": body.get("id").cloned().unwrap_or(serde_json::Value::Null),
        "error": {
            "code": -32600,
            "message": "This HTTP endpoint supports legacy MCP only. Modern MCP is currently available over stdio; use pincer mcp or request HTTP protocol version 2025-06-18.",
            "data": {"supported": ["2025-06-18"], "requested": requested}
        }
    }).to_string())
}

fn http_tool_observation_timer(
    req: &Request,
    is_known_tool: impl Fn(&str) -> bool,
) -> Option<pincer_core::server::ToolObservationTimer> {
    tool_observation_timer(req, is_known_tool, StdioExecutionPath::DirectDaemon)
}

fn serialize(resp: &Response) -> String {
    serde_json::to_string(resp).unwrap_or_else(|e| {
        format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"serialize error: {e}"}}}}"#)
    })
}

/// Apply the shared session identity mapping
/// ([`pincer_core::tool_args::apply_session_identity`]) to a tool call's
/// arguments, with the MCP HTTP session as the transport session.
/// Runtime-private idle-TTL activity is refreshed later at the authorized
/// registry boundary.
fn apply_session_identity(req: &mut Request, transport_session: &str) {
    if let Some(args) = req
        .params
        .as_mut()
        .and_then(|params| params.get_mut("arguments"))
    {
        pincer_core::tool_args::apply_session_identity(args, Some(transport_session));
    }
}

/// The request line and headers of one HTTP/1.1 request.
struct RequestHead {
    method: String,
    /// `None` when Content-Length is malformed or repeated with different
    /// values; `Some(0)` when it is absent.
    content_length: Option<usize>,
    /// Browser requests carry Origin; native MCP clients normally do not.
    has_origin: bool,
    /// Whether to keep the connection open after responding (HTTP/1.1 default;
    /// false if the client sent `Connection: close` or spoke HTTP/1.0).
    keep_alive: bool,
    authorized: bool,
    protocol_version: Option<String>,
}

enum HeadRead {
    Head(Vec<u8>),
    /// The peer closed the connection before a complete head.
    Closed,
    /// The head did not arrive within [`HEAD_TIMEOUT`].
    TimedOut,
}

/// Read one request head, up to and including the blank line. Until the
/// connection is `authenticated`, the whole head must arrive within
/// [`HEAD_TIMEOUT`] of this call; afterwards the wait for the first byte is
/// unbounded and the limit starts there.
async fn read_request_head(
    stream: &mut TcpStream,
    authenticated: bool,
) -> anyhow::Result<HeadRead> {
    let mut head = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    let mut deadline = (!authenticated).then(|| Instant::now() + HEAD_TIMEOUT);
    loop {
        let read = match deadline {
            Some(deadline) => {
                match tokio::time::timeout_at(deadline, stream.read(&mut byte)).await {
                    Ok(read) => read?,
                    Err(_) => return Ok(HeadRead::TimedOut),
                }
            }
            None => stream.read(&mut byte).await?,
        };
        if read == 0 {
            return Ok(HeadRead::Closed);
        }
        deadline.get_or_insert_with(|| Instant::now() + HEAD_TIMEOUT);
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            return Ok(HeadRead::Head(head));
        }
        if head.len() > MAX_HEAD {
            anyhow::bail!("HTTP headers too large");
        }
    }
}

fn parse_request_head(head: &[u8], token: &str) -> RequestHead {
    let head_str = String::from_utf8_lossy(head);
    let mut lines = head_str.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let _path = parts.next().unwrap_or("/");
    let version = parts.next().unwrap_or("HTTP/1.1");
    let mut content_length = Some(0usize);
    let mut seen_content_length = false;
    let mut has_origin = false;
    let mut keep_alive = version.eq_ignore_ascii_case("HTTP/1.1"); // 1.1 defaults to keep-alive
    let mut authorized = false;
    let mut protocol_version = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("origin") {
                has_origin = true;
            } else if k.eq_ignore_ascii_case("content-length") {
                let parsed = v.parse::<usize>().ok();
                content_length = if seen_content_length && parsed != content_length {
                    None
                } else {
                    parsed
                };
                seen_content_length = true;
            } else if k.eq_ignore_ascii_case("connection") {
                if v.eq_ignore_ascii_case("close") {
                    keep_alive = false;
                } else if v.eq_ignore_ascii_case("keep-alive") {
                    keep_alive = true;
                }
            } else if k.eq_ignore_ascii_case("authorization") {
                authorized = v.split_once(' ').is_some_and(|(scheme, candidate)| {
                    scheme.eq_ignore_ascii_case("bearer") && constant_time_equal(candidate, token)
                });
            } else if k.eq_ignore_ascii_case("mcp-protocol-version") {
                // Keep an unsupported value even if a later duplicate is legacy.
                if protocol_version.as_deref().is_none_or(is_legacy_protocol) {
                    protocol_version = Some(v.to_owned());
                }
            }
        }
    }
    RequestHead {
        method,
        content_length,
        has_origin,
        keep_alive,
        authorized,
        protocol_version,
    }
}

/// The bearer token, read from the environment once. Reading it removes it
/// from the process environment, so apps the daemon launches never inherit it.
static AUTH_TOKEN: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();

/// Move `PINCER_MCP_HTTP_TOKEN` out of the process environment into this
/// module. The daemon calls this at startup, before it starts other threads;
/// [`spawn`] reads the token through the same path, so a later call is safe.
pub fn capture_auth_token() {
    let _ = captured_auth_token();
}

fn captured_auth_token() -> Option<std::ffi::OsString> {
    AUTH_TOKEN
        .get_or_init(|| {
            let token = std::env::var_os(MCP_HTTP_TOKEN_ENV);
            if token.is_some() {
                std::env::remove_var(MCP_HTTP_TOKEN_ENV);
            }
            token
        })
        .clone()
}

fn configured_auth_token() -> Result<String, String> {
    configured_auth_token_from(captured_auth_token())
}

fn configured_auth_token_from(value: Option<std::ffi::OsString>) -> Result<String, String> {
    let token = value
        .ok_or_else(|| {
            format!(
                "{MCP_HTTP_TOKEN_ENV} must be set to a host-generated bearer token when the HTTP endpoint is enabled"
            )
        })?
        .into_string()
        .map_err(|_| format!("{MCP_HTTP_TOKEN_ENV} must be valid UTF-8"))?;
    if token.len() < 32
        || token.len() > 4096
        || token
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(format!(
            "{MCP_HTTP_TOKEN_ENV} must contain 32-4096 non-whitespace, non-control characters"
        ));
    }
    Ok(token)
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut difference = left.len() ^ right.len();
    let width = left.len().max(right.len());
    for index in 0..width {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

fn http_response(status: u16, body: &[u8], keep_alive: bool) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {conn}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

async fn write_http(
    stream: &mut TcpStream,
    status: u16,
    body: &[u8],
    keep_alive: bool,
) -> anyhow::Result<()> {
    stream
        .write_all(&http_response(status, body, keep_alive))
        .await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn serve_raw_request(sdk: Arc<crate::sdk_adapter::SdkAdapter>, request: &[u8]) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_conn(
                stream,
                sdk,
                Arc::<str>::from("0123456789abcdef0123456789abcdef"),
            )
            .await
            .unwrap();
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(request).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        server.await.unwrap();
        String::from_utf8(response).unwrap()
    }

    #[tokio::test]
    async fn origin_header_is_forbidden_but_originless_request_is_unchanged() {
        let _runtime_guard = crate::test_runtime_lock().lock().await;
        let sdk = crate::sdk_adapter::SdkAdapter::load(crate::build_driver_without_cursor())
            .await
            .expect("SDK adapter");
        let without_origin = serve_raw_request(
            sdk.clone(),
            b"POST /mcp HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef0123456789abcdef\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot-json",
        )
        .await;
        assert!(without_origin.starts_with("HTTP/1.1 200 OK\r\n"));

        let with_origin = serve_raw_request(
            sdk.clone(),
            b"POST /mcp HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef0123456789abcdef\r\noRiGiN:\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot-json",
        )
        .await;
        assert!(with_origin.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        sdk.shutdown().await.expect("SDK shutdown");
    }

    #[tokio::test]
    async fn http_rejects_modern_requests_before_dispatch() {
        let _runtime_guard = crate::test_runtime_lock().lock().await;
        let sdk = crate::sdk_adapter::SdkAdapter::load(crate::build_driver_without_cursor())
            .await
            .expect("SDK adapter");
        let tool_call = json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "unknown", "arguments": {}}
        });
        let mut modern_tool_call = tool_call.clone();
        modern_tool_call["params"]["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28"
        });
        let discover = json!({"jsonrpc": "2.0", "id": 7, "method": "server/discover"});
        for (headers, body, requested) in [
            (
                "mCp-PrOtOcOl-VeRsIoN: 2026-07-28\r\n",
                &tool_call,
                "2026-07-28",
            ),
            ("MCP-Protocol-Version: future\r\n", &tool_call, "future"),
            ("", &modern_tool_call, "2026-07-28"),
            ("", &discover, "2026-07-28"),
            (
                "MCP-Protocol-Version: 2025-06-18\r\n",
                &modern_tool_call,
                "2026-07-28",
            ),
            (
                "MCP-Protocol-Version: future\r\nMCP-Protocol-Version: 2025-06-18\r\n",
                &tool_call,
                "future",
            ),
        ] {
            let body = body.to_string();
            let request = format!(
                "POST /mcp HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef0123456789abcdef\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let response = serve_raw_request(sdk.clone(), request.as_bytes()).await;
            assert!(
                response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
                "{response}"
            );
            let response: serde_json::Value =
                serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(response["id"], 7);
            assert_eq!(response["error"]["code"], -32600);
            assert_eq!(
                response["error"]["data"],
                json!({
                    "supported": ["2025-06-18"], "requested": requested
                })
            );
            assert!(response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stdio"));
            assert!(response.get("result").is_none());
        }
        for body in [&modern_tool_call, &discover] {
            let response = dispatch(body.to_string().as_bytes(), &sdk, "http-test")
                .await
                .unwrap();
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["error"]["code"], -32600);
        }
        for (headers, status) in [
            ("Authorization: Bearer wrong\r\n", "401 Unauthorized"),
            ("Authorization: Bearer 0123456789abcdef0123456789abcdef\r\nOrigin: https://example.com\r\n", "403 Forbidden"),
        ] {
            let body = discover.to_string();
            let request = format!(
                "POST /mcp HTTP/1.1\r\n{headers}MCP-Protocol-Version: 2026-07-28\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let response = serve_raw_request(sdk.clone(), request.as_bytes()).await;
            assert!(response.starts_with(&format!("HTTP/1.1 {status}\r\n")));
            assert!(!response.contains("Modern MCP is currently available"));
        }
        sdk.shutdown().await.expect("SDK shutdown");
    }

    #[tokio::test]
    async fn http_keeps_legacy_initialize_negotiation() {
        let _runtime_guard = crate::test_runtime_lock().lock().await;
        let sdk = crate::sdk_adapter::SdkAdapter::load(crate::build_driver_without_cursor())
            .await
            .expect("SDK adapter");
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "legacy-test", "version": "1"}}
        })
        .to_string();
        for version in [
            None,
            Some("2024-11-05"),
            Some("2025-03-26"),
            Some("2025-06-18"),
        ] {
            let header = version
                .map(|version| format!("MCP-Protocol-Version: {version}\r\n"))
                .unwrap_or_default();
            let request = format!(
                "POST /mcp HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef0123456789abcdef\r\n{header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let response = serve_raw_request(sdk.clone(), request.as_bytes()).await;
            assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
            let response: serde_json::Value =
                serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
            assert!(response["result"].get("protocolVersions").is_none());
            assert!(response["result"]["capabilities"].get("apps").is_none());
        }
        sdk.shutdown().await.expect("SDK shutdown");
    }

    #[test]
    fn apply_session_identity_uses_transport_implicit_without_session() {
        let mut req: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "list_apps", "arguments": {} }
        }))
        .unwrap();
        apply_session_identity(&mut req, "http-test");
        let args = req.params.unwrap();
        assert_eq!(args["arguments"]["_session_id"], "http-test");
        assert_eq!(args["arguments"]["_transport_session_id"], "http-test");
    }

    #[test]
    fn http_observes_tool_calls_but_not_initialize_requests() {
        let tool_call: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "unknown", "arguments": {} }
        }))
        .unwrap();
        assert!(http_tool_observation_timer(&tool_call, |_| false).is_some());

        let initialize: Request = serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {}
        }))
        .unwrap();
        assert!(http_tool_observation_timer(&initialize, |_| false).is_none());
    }

    #[test]
    fn configured_port_parses_env() {
        assert_eq!(configured_port_from(None).unwrap(), None);
        assert_eq!(
            configured_port_from(Some("43123".into())).unwrap(),
            Some(43123)
        );
        assert!(configured_port_from(Some("0".into())).is_err());
        assert!(configured_port_from(Some("not-a-port".into())).is_err());
    }

    #[test]
    fn bearer_comparison_requires_the_exact_value() {
        assert!(constant_time_equal(
            "0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef"
        ));
        assert!(!constant_time_equal(
            "0123456789abcdef0123456789abcdee",
            "0123456789abcdef0123456789abcdef"
        ));
        assert!(!constant_time_equal(
            "0123456789abcdef0123456789abcdef-extra",
            "0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn configured_auth_token_fails_closed() {
        assert!(configured_auth_token_from(None).is_err());
        assert!(configured_auth_token_from(Some("too-short".into())).is_err());
        assert!(
            configured_auth_token_from(Some("0123456789abcdef 123456789abcdef".into())).is_err()
        );
        assert_eq!(
            configured_auth_token_from(Some("0123456789abcdef0123456789abcdef".into())).unwrap(),
            "0123456789abcdef0123456789abcdef"
        );
    }

    #[tokio::test]
    async fn http_tools_list_is_the_sdk_inventory() {
        let _runtime_guard = crate::test_runtime_lock().lock().await;
        let sdk = crate::sdk_adapter::SdkAdapter::load(crate::build_driver_without_cursor())
            .await
            .expect("SDK adapter");
        let expected = sdk.tools_list();
        let response = dispatch(
            br#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#,
            &sdk,
            "http-test",
        )
        .await
        .expect("JSON-RPC response");
        let response: serde_json::Value = serde_json::from_str(&response).expect("response JSON");
        assert_eq!(response["result"]["tools"], expected["tools"]);
        sdk.shutdown().await.expect("SDK shutdown");
    }

    #[tokio::test]
    async fn invalid_bearer_receives_401_before_json_rpc_dispatch() {
        let _runtime_guard = crate::test_runtime_lock().lock().await;
        let sdk = crate::sdk_adapter::SdkAdapter::load(crate::build_driver_without_cursor())
            .await
            .expect("SDK adapter");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_sdk = sdk.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_conn(
                stream,
                server_sdk,
                Arc::<str>::from("0123456789abcdef0123456789abcdef"),
            )
            .await
            .unwrap();
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(
                b"POST /mcp HTTP/1.1\r\nAuthorization: Bearer wrong\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(response.contains("Authentication required"));
        server.await.unwrap();
        sdk.shutdown().await.expect("SDK shutdown");
    }

    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// Serve exactly one connection with `serve_conn` on a loopback port.
    async fn one_connection_server(
        sdk: Arc<crate::sdk_adapter::SdkAdapter>,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = serve_conn(stream, sdk, Arc::<str>::from(TEST_TOKEN)).await;
        });
        (addr, server)
    }

    /// Send `head` and nothing else, then read until the server closes.
    /// `None` means the server neither answered nor closed within `limit`.
    async fn send_head_and_wait(
        addr: SocketAddr,
        head: &[u8],
        limit: std::time::Duration,
    ) -> Option<String> {
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(head).await.unwrap();
        let mut response = Vec::new();
        match tokio::time::timeout(limit, client.read_to_end(&mut response)).await {
            Ok(_) => Some(String::from_utf8_lossy(&response).into_owned()),
            Err(_) => None,
        }
    }

    // DAEMON-2: a client without the token must be refused as soon as its
    // headers arrive. Before the fix the server waited for (and buffered) the
    // whole declared body first, so these requests got no answer at all.
    #[tokio::test]
    async fn unauthenticated_requests_are_refused_before_the_body_is_read() {
        let _runtime_guard = crate::test_runtime_lock().lock().await;
        let sdk = crate::sdk_adapter::SdkAdapter::load(crate::build_driver_without_cursor())
            .await
            .expect("SDK adapter");
        let limit = std::time::Duration::from_secs(3);
        for (head, status) in [
            (
                "POST /mcp HTTP/1.1\r\nAuthorization: Bearer wrong\r\nContent-Length: 16777216\r\n\r\n",
                "HTTP/1.1 401 Unauthorized\r\n",
            ),
            (
                "POST /mcp HTTP/1.1\r\nContent-Length: 16777216\r\n\r\n",
                "HTTP/1.1 401 Unauthorized\r\n",
            ),
            (
                "POST /mcp HTTP/1.1\r\nOrigin: https://example.com\r\nContent-Length: 16777216\r\n\r\n",
                "HTTP/1.1 403 Forbidden\r\n",
            ),
            (
                "POST /mcp HTTP/1.1\r\nAuthorization: Bearer wrong\r\nContent-Length: 1073741824\r\n\r\n",
                "HTTP/1.1 401 Unauthorized\r\n",
            ),
        ] {
            let (addr, server) = one_connection_server(sdk.clone()).await;
            let response = send_head_and_wait(addr, head.as_bytes(), limit).await;
            server.abort();
            let response = response.unwrap_or_else(|| {
                panic!("no answer within {limit:?} to an unauthenticated request: {head:?}")
            });
            assert!(response.starts_with(status), "{head:?} got {response:?}");
        }
        sdk.shutdown().await.expect("SDK shutdown");
    }

    // DAEMON-2: a connection that never completes its request head must not
    // hold a daemon socket forever.
    #[tokio::test]
    async fn stalled_request_heads_are_closed() {
        let _runtime_guard = crate::test_runtime_lock().lock().await;
        let sdk = crate::sdk_adapter::SdkAdapter::load(crate::build_driver_without_cursor())
            .await
            .expect("SDK adapter");
        let limit = std::time::Duration::from_secs(20);
        let (silent_addr, silent_server) = one_connection_server(sdk.clone()).await;
        let (partial_addr, partial_server) = one_connection_server(sdk.clone()).await;
        let (silent, partial) = tokio::join!(
            send_head_and_wait(silent_addr, b"", limit),
            send_head_and_wait(
                partial_addr,
                b"POST /mcp HTTP/1.1\r\nContent-Length: 2\r\n",
                limit
            ),
        );
        silent_server.abort();
        partial_server.abort();
        assert!(
            silent.is_some(),
            "a connection that sent nothing stayed open for {limit:?}"
        );
        assert!(
            partial.is_some(),
            "a connection with an unfinished head stayed open for {limit:?}"
        );
        sdk.shutdown().await.expect("SDK shutdown");
    }

    /// Load the SDK and run `body` inside a fresh runtime (child tests only).
    fn with_child_sdk<F, Fut>(body: F)
    where
        F: FnOnce(Arc<crate::sdk_adapter::SdkAdapter>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let sdk = crate::sdk_adapter::SdkAdapter::load(crate::build_driver_without_cursor())
                .await
                .expect("SDK adapter");
            body(sdk.clone()).await;
            sdk.shutdown().await.expect("SDK shutdown");
        });
    }

    fn token_env() -> [(&'static str, &'static std::ffi::OsStr); 1] {
        [(MCP_HTTP_TOKEN_ENV, std::ffi::OsStr::new(TEST_TOKEN))]
    }

    // DAEMON-3: if the port is taken, whoever holds it would receive every
    // client's bearer token. Startup must fail instead of only warning.
    #[test]
    fn mcp_http_bind_failure_fails_startup() {
        if !crate::is_test_child() {
            crate::run_test_child(
                "mcp_http::tests::mcp_http_bind_failure_fails_startup",
                &token_env(),
                &[],
                std::time::Duration::from_secs(60),
            );
            return;
        }
        with_child_sdk(|sdk| async move {
            let squatter = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = squatter.local_addr().unwrap().port();
            let result = spawn(sdk, port);
            assert!(
                result.is_err(),
                "MCP HTTP started although 127.0.0.1:{port} belongs to another listener"
            );
            let message = format!("{:#}", result.err().unwrap());
            assert!(message.contains(&port.to_string()), "{message}");
            drop(squatter);
        });
    }

    // DAEMON-2: concurrent connections are capped, and a connection over the
    // cap is answered and closed at once instead of holding a task.
    #[test]
    fn mcp_http_connections_over_the_cap_are_refused() {
        if !crate::is_test_child() {
            crate::run_test_child(
                "mcp_http::tests::mcp_http_connections_over_the_cap_are_refused",
                &token_env(),
                &[],
                std::time::Duration::from_secs(60),
            );
            return;
        }
        with_child_sdk(|sdk| async move {
            let port = {
                let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
                probe.local_addr().unwrap().port()
            };
            let _server = spawn(sdk, port).expect("MCP HTTP listener");
            let addr: SocketAddr = ([127, 0, 0, 1], port).into();
            let mut idle = Vec::new();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while idle.is_empty() {
                match TcpStream::connect(addr).await {
                    Ok(stream) => idle.push(stream),
                    Err(error) if std::time::Instant::now() < deadline => {
                        let _ = error;
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    Err(error) => panic!("MCP HTTP listener never came up: {error}"),
                }
            }
            while idle.len() < 64 {
                idle.push(TcpStream::connect(addr).await.unwrap());
            }
            let mut extra = TcpStream::connect(addr).await.unwrap();
            let mut response = Vec::new();
            let read = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                extra.read_to_end(&mut response),
            )
            .await;
            let response = String::from_utf8_lossy(&response);
            assert!(
                read.is_ok() && response.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
                "connection 65 was not refused: {response:?}"
            );
            drop(idle);
        });
    }

    // DAEMON-4: the daemon must not keep the token in its environment, where
    // every app it launches would inherit it.
    #[test]
    fn mcp_http_token_leaves_the_environment_once_read() {
        if !crate::is_test_child() {
            crate::run_test_child(
                "mcp_http::tests::mcp_http_token_leaves_the_environment_once_read",
                &token_env(),
                &[],
                std::time::Duration::from_secs(60),
            );
            return;
        }
        assert_eq!(configured_auth_token().unwrap(), TEST_TOKEN);
        assert!(
            std::env::var_os(MCP_HTTP_TOKEN_ENV).is_none(),
            "{MCP_HTTP_TOKEN_ENV} is still in the daemon environment"
        );
        // A second read (for example a second listener) still sees the token.
        assert_eq!(configured_auth_token().unwrap(), TEST_TOKEN);
    }
}
