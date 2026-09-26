//! Remote MCP transport over Streamable HTTP.
//!
//! Both directions talk to one endpoint. kage POSTs each JSON-RPC
//! message and the reply rides on that POST's response: a JSON body,
//! a `text/event-stream` of frames, or a bare `202 Accepted` when the
//! server answers later. A GET on the same endpoint opens the optional
//! server-initiated stream; kage opens it once, after the
//! `initialize` reply, and forwards any JSON frames it produces into
//! the same pipe the POST responses feed. The session id the server hands
//! back is echoed as `mcp-session-id` on later requests.
//!
//! This maps onto the byte stream [`kage_jsonrpc::connect`] expects, so
//! the HTTP transport reuses the exact same [`Peer`], request routing,
//! and cancellation as the stdio transport.
//!
//! The transport learns the negotiated protocol version from the first
//! response whose `result` carries a string `protocolVersion` (the
//! `initialize` reply) and sends it as `MCP-Protocol-Version` on every
//! later POST and on the GET.
//!
//! With a [`TokenSource`] and no configured `authorization` header,
//! every POST and the GET carry `Authorization: Bearer <token>` for the
//! endpoint URL. A 401 asks the source for a fresh token once and
//! retries that POST once. A 401 that stays fails the request with
//! [`UNAUTHORIZED`], or [`REFUSED`] when kage sends no stored token (a
//! configured header wins, or there is no token source), which the
//! connection reports as
//! [`McpError::Unauthorized`](crate::McpError::Unauthorized). Redirects
//! are never followed, so configured headers and bearer tokens never
//! reach another host than the configured endpoint.
//!
//! One simplification over the spec: a POST failure closes the
//! transport only when the server could not have routed the request at
//! all (404, which the spec defines as an expired session, or an
//! unreachable host); any other HTTP status fails that one request and
//! leaves the transport open.
//!
//! Each outgoing request is sent as a POST on its own detached
//! thread, so the peer's writer lock is released as soon as the
//! message is handed off. A long `tools/call` can therefore stream for as long as it
//! likes while the server sends `roots/list` or `ping` mid-call and
//! kage answers it, and a local cancel returns immediately. When that
//! POST fails, the thread feeds a synthetic error response for the
//! request id into the pipe so the waiting caller fails fast.
//! Notifications and responses are posted inline: servers answer them
//! with a quick 202, and it keeps `notifications/initialized` ordered
//! after the `initialize` reply.
//!
//! Synchronous throughout: blocking `ureq` calls and `std::thread`, no
//! async, matching the rest of the workspace. The GET pump and request
//! threads are detached and may stay parked on an open remote stream
//! until the server closes it, the 600 s idle deadline fires, or the
//! process exits; closing the transport only takes away the pipe
//! writer they forward into.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use kage_jsonrpc::{Inbound, Peer, connect_with};

use crate::oauth::TokenSource;
use crate::server::cancel_notice;

/// JSON-RPC code of the synthetic error for a POST the server answered
/// with 401 even after a token refresh. It sits outside the range
/// JSON-RPC reserves, and servers never send it over HTTP 401.
pub(crate) const UNAUTHORIZED: i64 = -33401;

/// JSON-RPC code of the synthetic error for a POST the server answered
/// with 401 when kage sends no stored token, so a login would not help.
pub(crate) const REFUSED: i64 = -33403;

/// JSON-RPC code of the synthetic error for any other failed POST.
const INTERNAL: i64 = -32603;

/// How long the GET pump waits for the negotiated version before
/// giving up on the server-initiated stream. Generous: this only
/// trips on a server that never answers.
const STREAM_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Cap on a single SSE frame, so a server cannot exhaust memory with
/// one endless stream of lines.
const MAX_SSE_LINE: u64 = 8 * 1024 * 1024;

/// Cap on a single non-SSE POST response body.
const MAX_JSON_BODY: u64 = 8 * 1024 * 1024;

/// Session state shared between the POST writer and the GET pump:
/// the server-assigned session id and the negotiated protocol version.
#[derive(Default)]
struct Shared {
    session_id: Option<String>,
    protocol_version: Option<String>,
}

/// Session state behind the condvar the GET pump waits on.
type SharedState = Arc<(Mutex<Shared>, Condvar)>;

/// The transport's write end of the pipe feeding the jsonrpc reader.
/// Taken away by [`HttpPoster::close`] to end the stream.
type OutSlot = Arc<Mutex<Option<io::PipeWriter>>>;

/// Where and how every request of one connection goes: the agent, the
/// endpoint URL, the configured headers, and the token source (`None`
/// when there is none or a configured `authorization` header wins).
#[derive(Clone)]
struct Endpoint {
    agent: ureq::Agent,
    url: String,
    headers: BTreeMap<String, String>,
    tokens: Option<Arc<dyn TokenSource>>,
}

impl Endpoint {
    fn bearer(&self) -> Option<String> {
        self.tokens.as_ref()?.bearer(&self.url)
    }
}

/// Open a Streamable HTTP connection to `url`, sending `headers` and a
/// bearer token from `tokens` on every request, and hand the adapted
/// pipe to [`kage_jsonrpc::connect_with`] with the MCP cancel notice.
///
/// # Errors
///
/// Returns a message when the local pipe cannot be created.
pub(crate) fn connect_http(
    url: &str,
    headers: &BTreeMap<String, String>,
    tokens: Option<Arc<dyn TokenSource>>,
) -> Result<(Peer, Receiver<Inbound>, JoinHandle<()>), String> {
    open_http(transport_agent(), url, headers, tokens)
}

/// Transport deadlines, mirroring `kage_provider::http::build_agent`:
/// without them every phase is unbounded, so a server that accepts the
/// connection and never answers parks a thread and socket forever.
/// `recv_body` is an idle bound recomputed on every read, so long
/// tool calls and the GET stream are never capped in total;
/// `recv_response` stays unset so a streaming response cannot die
/// mid-flight while data keeps arriving. Redirects are never
/// followed: an MCP endpoint has no reason to redirect, and not
/// following keeps configured headers and bearer tokens on the
/// configured host.
fn transport_config() -> ureq::config::Config {
    ureq::Agent::config_builder()
        .timeout_resolve(Some(Duration::from_secs(15)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_send_request(Some(Duration::from_secs(600)))
        .timeout_send_body(Some(Duration::from_secs(600)))
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .max_redirects(0)
        .build()
}

fn transport_agent() -> ureq::Agent {
    transport_config().new_agent()
}

/// [`connect_http`] against an explicit agent, so tests can run the
/// whole transport against an in-process fake server.
fn open_http(
    agent: ureq::Agent,
    url: &str,
    headers: &BTreeMap<String, String>,
    tokens: Option<Arc<dyn TokenSource>>,
) -> Result<(Peer, Receiver<Inbound>, JoinHandle<()>), String> {
    let (pipe_reader, pipe_writer) = io::pipe().map_err(|e| format!("open mcp pipe: {e}"))?;
    let out: OutSlot = Arc::new(Mutex::new(Some(pipe_writer)));
    let shared: SharedState = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
    let configured = headers
        .keys()
        .any(|key| key.eq_ignore_ascii_case("authorization"));
    let endpoint = Endpoint {
        agent,
        url: url.to_owned(),
        headers: headers.clone(),
        tokens: tokens.filter(|_| !configured),
    };
    spawn_get_pump(endpoint.clone(), Arc::clone(&shared), Arc::clone(&out));
    let writer = HttpPoster {
        endpoint,
        shared,
        out,
        buf: Vec::new(),
    };
    Ok(connect_with(
        BufReader::new(pipe_reader),
        writer,
        Some(cancel_notice()),
    ))
}

/// Write one newline-terminated JSON-RPC message into the pipe, or
/// fail once the transport has been closed.
fn forward(out: &OutSlot, msg: &[u8]) -> io::Result<()> {
    let mut guard = kage_core::sync::lock(out);
    let Some(sink) = guard.as_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "mcp http connection closed",
        ));
    };
    sink.write_all(msg)?;
    sink.write_all(b"\n")
}

/// Read the next SSE frame's joined `data` payload from `reader`,
/// returning `None` at EOF. Comment lines (`:` prefix) and unknown
/// fields (including `event:`) are ignored, multiple `data:` lines
/// are joined with `\n`, and a blank line terminates the frame. Both
/// a single line and the frame as a whole are bounded by `limit`.
fn read_sse_frame<R: BufRead>(reader: &mut R, limit: u64) -> io::Result<Option<String>> {
    let mut data: Vec<String> = Vec::new();
    let mut saw_any = false;
    let mut frame_bytes = 0u64;
    loop {
        let mut line = String::new();
        let n = reader.by_ref().take(limit).read_line(&mut line)?;
        frame_bytes += u64::try_from(n).unwrap_or(u64::MAX);
        if frame_bytes > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sse frame exceeds size cap",
            ));
        }
        if n == 0 {
            if saw_any && !data.is_empty() {
                return Ok(Some(data.join("\n")));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if saw_any && !data.is_empty() {
                return Ok(Some(data.join("\n")));
            }
            continue;
        }
        saw_any = true;
        if let Some(rest) = trimmed.strip_prefix("data:") {
            data.push(rest.trim_start().to_owned());
        }
        // Comments (':' prefix) and unknown fields are ignored.
    }
}

/// Pull SSE frames from `body` and forward every JSON payload into
/// the pipe. Keep-alive comments, empty frames, and non-JSON data
/// (legacy `endpoint` events, bare strings) are dropped.
fn pump_sse<R: Read>(body: R, shared: &SharedState, out: &OutSlot) -> io::Result<()> {
    let mut reader = BufReader::new(body);
    while let Some(data) = read_sse_frame(&mut reader, MAX_SSE_LINE)? {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        record_version(shared, &message);
        forward(out, data.as_bytes())?;
    }
    Ok(())
}

/// Remember the version from the first `result` that names one, and
/// wake the GET pump that waits for it. It is recorded before the
/// message is forwarded, so the next POST already carries it.
fn record_version(shared: &SharedState, message: &serde_json::Value) {
    let Some(version) = message
        .get("result")
        .and_then(|r| r.get("protocolVersion"))
        .and_then(|v| v.as_str())
    else {
        return;
    };
    let mut guard = kage_core::sync::lock(&shared.0);
    if guard.protocol_version.is_none() {
        guard.protocol_version = Some(version.to_owned());
        shared.1.notify_all();
    }
}

/// Take away the pipe writer: the reader side sees EOF, the jsonrpc
/// drain ends, and the connection reports itself dead.
fn close(out: &OutSlot) {
    *kage_core::sync::lock(out) = None;
}

/// The `id` of `body` when it is a JSON-RPC request, that is, a
/// message carrying both `method` and a non-null `id`.
fn request_id(body: &[u8]) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value.get("method")?;
    value.get("id").filter(|id| !id.is_null()).cloned()
}

/// A failed POST, whether it leaves the transport unusable, and the
/// JSON-RPC code its synthetic error reply carries.
struct PostFailure {
    error: io::Error,
    fatal: bool,
    code: i64,
}

impl PostFailure {
    fn fatal(error: io::Error) -> Self {
        Self {
            error,
            fatal: true,
            code: INTERNAL,
        }
    }
}

/// Send one POST with the session headers, `bearer` when given, and the
/// configured headers.
fn send_post(
    endpoint: &Endpoint,
    shared: &SharedState,
    bearer: Option<&str>,
    body: &[u8],
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let mut req = endpoint
        .agent
        .post(&endpoint.url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json");
    {
        let guard = kage_core::sync::lock(&shared.0);
        if let Some(session) = &guard.session_id {
            req = req.header("mcp-session-id", session.as_str());
        }
        if let Some(version) = &guard.protocol_version {
            req = req.header("MCP-Protocol-Version", version.as_str());
        }
    }
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    // Configured headers go last so they can override the defaults.
    for (key, value) in &endpoint.headers {
        req = req.header(key.as_str(), value.as_str());
    }
    req.send(body)
}

/// POST one JSON-RPC message and absorb the response. A 401 to a
/// bearer token asks the token source for a fresh one and retries once.
/// A successful response records the session id (first one wins) before
/// any body is forwarded, so the next POST already carries the session;
/// an SSE response or a JSON body is forwarded into the pipe, a 202 or
/// empty body is a bare success.
fn post_and_forward(
    endpoint: &Endpoint,
    shared: &SharedState,
    out: &OutSlot,
    body: &[u8],
) -> Result<(), PostFailure> {
    let url = &endpoint.url;
    let bearer = endpoint.bearer();
    let mut sent = send_post(endpoint, shared, bearer.as_deref(), body);
    if let (Some(token), Some(source)) = (bearer.as_deref(), endpoint.tokens.as_ref())
        && matches!(sent, Err(ureq::Error::StatusCode(401)))
        && let Some(fresh) = source.rejected(url, token)
    {
        sent = send_post(endpoint, shared, Some(&fresh), body);
    }
    let response = match sent {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(401)) => {
            return Err(PostFailure {
                error: io::Error::other(format!("mcp post {url}: status 401 unauthorized")),
                fatal: false,
                code: if endpoint.tokens.is_some() {
                    UNAUTHORIZED
                } else {
                    REFUSED
                },
            });
        }
        // The spec defines 404 as a terminated session.
        Err(ureq::Error::StatusCode(404)) => {
            return Err(PostFailure::fatal(io::Error::other(
                "mcp http session expired (404)",
            )));
        }
        Err(ureq::Error::StatusCode(code)) => {
            return Err(PostFailure {
                error: io::Error::other(format!("mcp post {url}: status {code}")),
                fatal: false,
                code: INTERNAL,
            });
        }
        Err(e) => {
            return Err(PostFailure::fatal(io::Error::other(format!(
                "mcp post {url}: {e}"
            ))));
        }
    };
    {
        let mut guard = kage_core::sync::lock(&shared.0);
        if guard.session_id.is_none()
            && let Some(session) = response
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
        {
            guard.session_id = Some(session.to_owned());
        }
    }
    let status = response.status().as_u16();
    // With redirects never followed (see `transport_config`), a 3xx
    // arrives here as a plain response. There is nothing to act on:
    // fail the request instead of forwarding an empty success.
    if !response.status().is_success() {
        return Err(PostFailure {
            error: io::Error::other(format!("mcp post {url}: status {status}")),
            fatal: false,
            code: INTERNAL,
        });
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    // A failure past this point may leave the response part-consumed,
    // so the transport can no longer be trusted.
    if content_type.starts_with("text/event-stream") {
        return pump_sse(response.into_body().into_reader(), shared, out)
            .map_err(PostFailure::fatal);
    }
    let mut payload = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_JSON_BODY + 1)
        .read_to_end(&mut payload)
        .map_err(PostFailure::fatal)?;
    if u64::try_from(payload.len()).unwrap_or(u64::MAX) > MAX_JSON_BODY {
        return Err(PostFailure::fatal(io::Error::new(
            io::ErrorKind::InvalidData,
            "mcp http body exceeds size cap",
        )));
    }
    if status == 202 || payload.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    if kage_core::sync::lock(&shared.0).protocol_version.is_none()
        && let Ok(message) = serde_json::from_slice::<serde_json::Value>(&payload)
    {
        record_version(shared, &message);
    }
    forward(out, &payload).map_err(PostFailure::fatal)
}

/// `Write` adapter that POSTs each buffered JSON-RPC message to the
/// server endpoint on flush and absorbs the response into the pipe.
struct HttpPoster {
    endpoint: Endpoint,
    shared: SharedState,
    out: OutSlot,
    buf: Vec<u8>,
}

impl Write for HttpPoster {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    /// POST the buffered message. A request is posted on its own
    /// thread and `flush` returns at once, so a long call does not
    /// hold the peer's writer lock while the server sends requests of
    /// its own; a failed POST is answered with a synthetic error
    /// response so the caller does not wait forever. Notifications
    /// and responses are posted inline, which keeps
    /// `notifications/initialized` ordered after the `initialize`
    /// reply.
    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let body = std::mem::take(&mut self.buf);
        let Some(id) = request_id(&body) else {
            return post_and_forward(&self.endpoint, &self.shared, &self.out, &body).map_err(
                |failure| {
                    if failure.fatal {
                        close(&self.out);
                    }
                    failure.error
                },
            );
        };
        let endpoint = self.endpoint.clone();
        let shared = Arc::clone(&self.shared);
        let out = Arc::clone(&self.out);
        std::thread::spawn(move || {
            let Err(failure) = post_and_forward(&endpoint, &shared, &out, &body) else {
                return;
            };
            let reply = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": failure.code, "message": failure.error.to_string() },
            });
            let _ = forward(&out, reply.to_string().as_bytes());
            if failure.fatal {
                close(&out);
            }
        });
        Ok(())
    }
}

/// Spawn the detached GET pump: wait for the negotiated version (the
/// `initialize` reply), then open the optional server-initiated stream and forward its
/// JSON frames into the pipe. A refused or non-SSE answer (the spec
/// allows a plain 405, and a 401 is not retried here) ends the pump
/// silently.
fn spawn_get_pump(endpoint: Endpoint, shared: SharedState, out: OutSlot) {
    std::thread::spawn(move || {
        let (lock, cv) = &*shared;
        let guard = kage_core::sync::lock(lock);
        let (guard, waited) = cv
            .wait_timeout_while(guard, STREAM_READY_TIMEOUT, |s| {
                s.protocol_version.is_none()
            })
            .expect("mcp http state mutex poisoned");
        if waited.timed_out() {
            return;
        }
        let session = guard.session_id.clone();
        let version = guard.protocol_version.clone().unwrap_or_default();
        drop(guard);
        let mut req = endpoint
            .agent
            .get(&endpoint.url)
            .header("accept", "text/event-stream");
        if let Some(session) = &session {
            req = req.header("mcp-session-id", session.as_str());
        }
        req = req.header("MCP-Protocol-Version", version.as_str());
        if let Some(token) = endpoint.bearer() {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        for (key, value) in &endpoint.headers {
            req = req.header(key.as_str(), value.as_str());
        }
        let Ok(response) = req.call() else {
            return;
        };
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("text/event-stream") {
            return;
        }
        let _ = pump_sse(response.into_body().into_reader(), &shared, &out);
    });
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::server::{McpConnection, McpError};
    use crate::test_support::{HttpRequest, StaticTokens, read_request, wait_until};
    use ureq::config::Config;
    use ureq::http::Uri;
    use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
    use ureq::unversioned::transport::{
        Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
    };

    #[test]
    fn frame_joins_multi_line_data_and_skips_comments() {
        let mut reader = BufReader::new(&b": keep-alive\nevent: message\ndata: a\ndata: b\n\n"[..]);
        let frame = read_sse_frame(&mut reader, MAX_SSE_LINE).unwrap().unwrap();
        assert_eq!(frame, "a\nb");
    }

    #[test]
    fn sse_frame_size_cap_rejects_an_oversized_frame() {
        let mut reader = BufReader::new(&b"data: 0123456789abcdef\n\n"[..]);
        let err = read_sse_frame(&mut reader, 8).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn pump_sse_forwards_json_and_drops_noise() {
        let (mut pipe_rx, pipe_tx) = io::pipe().unwrap();
        let out: OutSlot = Arc::new(Mutex::new(Some(pipe_tx)));
        let shared = SharedState::default();
        let stream: &[u8] = b": keep-alive\nevent: endpoint\ndata: /mcp\n\n\
            event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7}\n\n\
            data: not json\n\n";
        pump_sse(stream, &shared, &out).unwrap();
        *kage_core::sync::lock(&out) = None;
        let mut got = String::new();
        BufReader::new(&mut pipe_rx).read_line(&mut got).unwrap();
        assert_eq!(got.trim_end(), "{\"jsonrpc\":\"2.0\",\"id\":7}");
        let mut eof = [0u8; 1];
        assert_eq!(pipe_rx.read(&mut eof).unwrap(), 0);
    }

    /// The handler each test server call goes through: one recorded
    /// request in, the HTTP response written to the connection.
    type Handler = Arc<dyn Fn(&HttpRequest, &mut UnixStream) + Send + Sync>;

    /// A [`Handler`] answering every request with the full response
    /// `respond` builds.
    fn fixed(respond: impl Fn(&HttpRequest) -> Vec<u8> + Send + Sync + 'static) -> Handler {
        Arc::new(move |request, stream| {
            let _ = stream.write_all(&respond(request));
        })
    }

    /// Build a full HTTP/1.1 response with a fixed body.
    fn response_bytes(
        status_line: &str,
        content_type: Option<&str>,
        extra: &[(&str, &str)],
        body: &str,
    ) -> Vec<u8> {
        let mut head = format!(
            "{status_line}\r\ncontent-length: {}\r\nconnection: close\r\n",
            body.len()
        );
        if let Some(ct) = content_type {
            head.push_str("content-type: ");
            head.push_str(ct);
            head.push_str("\r\n");
        }
        for (name, value) in extra {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(body.as_bytes());
        out
    }

    /// Client end of one fake connection: the socket to the in-process
    /// server thread plus ureq's lazy buffers, mirroring
    /// ureq's own `TcpTransport`.
    struct FakeTransport {
        stream: UnixStream,
        buffers: LazyBuffers,
        timeout_read: Option<Duration>,
        timeout_write: Option<Duration>,
    }

    impl fmt::Debug for FakeTransport {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("FakeTransport").finish_non_exhaustive()
        }
    }

    /// Map a socket error the way ureq's own transports do: an
    /// elapsed timeout surfaces as EAGAIN (`WouldBlock`) on a
    /// `UnixStream`, everything else stays an io error.
    fn transport_error(e: io::Error, timeout: NextTimeout) -> ureq::Error {
        if e.kind() == io::ErrorKind::WouldBlock {
            ureq::Error::Timeout(timeout.reason)
        } else {
            ureq::Error::Io(e)
        }
    }

    /// Apply `timeout` to `stream` only when it changed, like ureq's
    /// `maybe_update_timeout`.
    fn apply_timeout(
        timeout: NextTimeout,
        previous: &mut Option<Duration>,
        stream: &UnixStream,
        set: fn(&UnixStream, Option<Duration>) -> io::Result<()>,
    ) -> Result<(), ureq::Error> {
        let wanted = timeout.not_zero().map(|d| *d);
        if wanted != *previous {
            set(stream, wanted).map_err(ureq::Error::Io)?;
            *previous = wanted;
        }
        Ok(())
    }

    impl Transport for FakeTransport {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }

        fn transmit_output(
            &mut self,
            amount: usize,
            timeout: NextTimeout,
        ) -> Result<(), ureq::Error> {
            apply_timeout(
                timeout,
                &mut self.timeout_write,
                &self.stream,
                UnixStream::set_write_timeout,
            )?;
            let output = &self.buffers.output()[..amount];
            self.stream
                .write_all(output)
                .map_err(|e| transport_error(e, timeout))
        }

        fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
            apply_timeout(
                timeout,
                &mut self.timeout_read,
                &self.stream,
                UnixStream::set_read_timeout,
            )?;
            let input = self.buffers.input_append_buf();
            match self.stream.read(input) {
                Ok(n) => {
                    self.buffers.input_appended(n);
                    Ok(n > 0)
                }
                Err(e) => Err(transport_error(e, timeout)),
            }
        }

        fn is_open(&mut self) -> bool {
            // Responses are sent with `connection: close`, so the
            // transport is never pooled.
            false
        }
    }

    /// Connector standing in for the remote endpoint: every connection
    /// is a `UnixStream::pair`, the server side served by a thread that
    /// parses one request and answers through the test handler. The
    /// first `quota` non-GET requests get responses; any later one gets
    /// a dropped connection (no response), which the client reads as a
    /// transport failure.
    struct FakeServerConnector {
        handler: Handler,
        served: Arc<AtomicUsize>,
        quota: usize,
    }

    impl fmt::Debug for FakeServerConnector {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("FakeServerConnector")
                .finish_non_exhaustive()
        }
    }

    impl Connector<()> for FakeServerConnector {
        type Out = FakeTransport;

        fn connect(
            &self,
            details: &ConnectionDetails,
            _: Option<()>,
        ) -> Result<Option<FakeTransport>, ureq::Error> {
            let (client, mut remote) = UnixStream::pair().map_err(ureq::Error::Io)?;
            let handler = Arc::clone(&self.handler);
            let served = Arc::clone(&self.served);
            let quota = self.quota;
            std::thread::spawn(move || {
                let Some(request) = read_request(&mut remote) else {
                    return;
                };
                if request.method != "GET" && served.fetch_add(1, Ordering::SeqCst) >= quota {
                    // Over quota: stop answering entirely; the client
                    // sees EOF and reports a transport failure.
                    return;
                }
                handler(&request, &mut remote);
                let _ = remote.flush();
            });
            let buffers = LazyBuffers::new(
                details.config.input_buffer_size(),
                details.config.output_buffer_size(),
            );
            Ok(Some(FakeTransport {
                stream: client,
                buffers,
                timeout_read: None,
                timeout_write: None,
            }))
        }
    }

    /// Resolver that answers every host with a fixed address without
    /// touching DNS, so the fake agent never reaches the network.
    #[derive(Debug)]
    struct NoResolver;

    impl Resolver for NoResolver {
        fn resolve(
            &self,
            _: &Uri,
            _: &Config,
            _: NextTimeout,
        ) -> Result<ResolvedSocketAddrs, ureq::Error> {
            let mut addrs = self.empty();
            addrs.push("10.0.0.1:1".parse().unwrap());
            Ok(addrs)
        }
    }

    /// An agent whose HTTP traffic is answered by `handler` in-process.
    /// The first `quota` POST requests get responses, later ones get a
    /// dropped connection; GET requests always get through.
    fn fake_agent(handler: Handler, quota: usize) -> ureq::Agent {
        fake_agent_with(Config::default(), handler, quota)
    }

    /// [`fake_agent`] against an explicit config, so tests can run the
    /// production transport config against the in-process fake server.
    fn fake_agent_with(config: Config, handler: Handler, quota: usize) -> ureq::Agent {
        ureq::Agent::with_parts(
            config,
            FakeServerConnector {
                handler,
                served: Arc::new(AtomicUsize::new(0)),
                quota,
            },
            NoResolver,
        )
    }

    /// Run `f` on its own thread and return its result, or `None` when
    /// it has not finished within 5 s.
    fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(5)).ok()
    }

    /// The URL tests hand to [`open_http`]; never dialed, the fake
    /// connector ignores it.
    const TEST_URL: &str = "http://mcp.test/mcp";

    /// The `initialize` answer the fake servers send.
    fn initialize_response() -> Vec<u8> {
        response_bytes(
            "HTTP/1.1 200 OK",
            Some("application/json"),
            &[],
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} }
                }
            })
            .to_string(),
        )
    }

    #[test]
    fn http_round_trip_json_with_session_echo() {
        let log: Arc<Mutex<Vec<HttpRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&log);
        let handler = fixed(move |request: &HttpRequest| -> Vec<u8> {
            recorded.lock().unwrap().push(request.clone());
            if request.method == "GET" {
                return response_bytes("HTTP/1.1 405 Method Not Allowed", None, &[], "");
            }
            let body: serde_json::Value =
                serde_json::from_str(&request.body).unwrap_or(serde_json::Value::Null);
            match body.get("method").and_then(|m| m.as_str()) {
                Some("initialize") => response_bytes(
                    "HTTP/1.1 200 OK",
                    Some("application/json"),
                    &[("mcp-session-id", "sess-1")],
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": { "tools": {} }
                        }
                    })
                    .to_string(),
                ),
                Some("notifications/initialized") => {
                    response_bytes("HTTP/1.1 202 Accepted", None, &[], "")
                }
                Some("tools/list") => response_bytes(
                    "HTTP/1.1 200 OK",
                    Some("application/json"),
                    &[],
                    &serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "result": { "tools": [ { "name": "t", "inputSchema": {} } ] }
                    })
                    .to_string(),
                ),
                _ => response_bytes("HTTP/1.1 500 Internal Server Error", None, &[], ""),
            }
        });
        let (peer, inbound, _reader) = open_http(
            fake_agent(handler, usize::MAX),
            TEST_URL,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        let conn = McpConnection::initialize("srv", peer, inbound, &[], None).unwrap();
        assert_eq!(conn.protocol_version(), "2025-06-18");
        let tools = conn.list_tools().unwrap();
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["t"]
        );
        assert!(wait_until(|| log
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.method == "GET")));
        let log = log.lock().unwrap();
        assert_eq!(log.iter().filter(|r| r.method == "GET").count(), 1);
        let init = log
            .iter()
            .find(|r| r.method == "POST" && r.body.contains("\"initialize\""))
            .unwrap();
        assert!(!init.headers.contains_key("mcp-session-id"));
        let listed = log
            .iter()
            .find(|r| r.method == "POST" && r.body.contains("\"tools/list\""))
            .unwrap();
        assert_eq!(
            listed.headers.get("mcp-session-id").map(String::as_str),
            Some("sess-1")
        );
        assert!(listed.headers.contains_key("mcp-protocol-version"));
    }

    #[test]
    fn http_round_trip_sse_response() {
        let handler = fixed(|request: &HttpRequest| -> Vec<u8> {
            if request.method == "GET" {
                return response_bytes("HTTP/1.1 405 Method Not Allowed", None, &[], "");
            }
            let body: serde_json::Value =
                serde_json::from_str(&request.body).unwrap_or(serde_json::Value::Null);
            match body.get("method").and_then(|m| m.as_str()) {
                Some("initialize") => response_bytes(
                    "HTTP/1.1 200 OK",
                    Some("text/event-stream"),
                    &[("mcp-session-id", "sess-2")],
                    &format!(
                        "event: message\ndata: {}\n\n",
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "result": {
                                "protocolVersion": "2025-06-18",
                                "capabilities": {}
                            }
                        })
                    ),
                ),
                Some("notifications/initialized") => {
                    response_bytes("HTTP/1.1 202 Accepted", None, &[], "")
                }
                _ => response_bytes("HTTP/1.1 500 Internal Server Error", None, &[], ""),
            }
        });
        let (peer, inbound, _reader) = open_http(
            fake_agent(handler, usize::MAX),
            TEST_URL,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        let conn = McpConnection::initialize("srv", peer, inbound, &[], None).unwrap();
        assert_eq!(conn.protocol_version(), "2025-06-18");
    }

    #[test]
    fn later_requests_carry_the_negotiated_version() {
        for sse in [false, true] {
            let log: Arc<Mutex<Vec<HttpRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&log);
            let handler = fixed(move |request: &HttpRequest| -> Vec<u8> {
                recorded.lock().unwrap().push(request.clone());
                if request.method == "GET" {
                    return response_bytes("HTTP/1.1 405 Method Not Allowed", None, &[], "");
                }
                if !request.body.contains("\"initialize\"") {
                    return response_bytes("HTTP/1.1 202 Accepted", None, &[], "");
                }
                let reply = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": { "protocolVersion": "2025-03-26", "capabilities": {} }
                });
                if sse {
                    let body = format!("data: {reply}\n\n");
                    response_bytes("HTTP/1.1 200 OK", Some("text/event-stream"), &[], &body)
                } else {
                    let body = reply.to_string();
                    response_bytes("HTTP/1.1 200 OK", Some("application/json"), &[], &body)
                }
            });
            let (peer, inbound, _reader) = open_http(
                fake_agent(handler, usize::MAX),
                TEST_URL,
                &BTreeMap::new(),
                None,
            )
            .unwrap();
            let conn = McpConnection::initialize("srv", peer, inbound, &[], None).unwrap();
            assert_eq!(conn.protocol_version(), "2025-03-26");
            assert!(wait_until(|| log
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.method == "GET")));
            let log = log.lock().unwrap();
            let version = |r: &HttpRequest| r.headers.get("mcp-protocol-version").cloned();
            assert_eq!(version(&log[0]), None, "sse: {sse}");
            let initialized = log
                .iter()
                .find(|r| r.body.contains("notifications/initialized"))
                .expect("the second post");
            assert_eq!(
                version(initialized).as_deref(),
                Some("2025-03-26"),
                "sse: {sse}"
            );
            let get = log.iter().find(|r| r.method == "GET").expect("the stream");
            assert_eq!(version(get).as_deref(), Some("2025-03-26"), "sse: {sse}");
        }
    }

    #[test]
    fn transport_failure_fails_the_request_and_marks_connection_dead() {
        let handler = fixed(|request: &HttpRequest| -> Vec<u8> {
            if request.method == "GET" {
                return response_bytes("HTTP/1.1 405 Method Not Allowed", None, &[], "");
            }
            let body: serde_json::Value =
                serde_json::from_str(&request.body).unwrap_or(serde_json::Value::Null);
            match body.get("method").and_then(|m| m.as_str()) {
                Some("initialize") => initialize_response(),
                Some("notifications/initialized") => {
                    response_bytes("HTTP/1.1 202 Accepted", None, &[], "")
                }
                _ => response_bytes("HTTP/1.1 500 Internal Server Error", None, &[], ""),
            }
        });
        // Two responses cover the initialize handshake; the tools/list
        // POST is the dropped third request.
        let (peer, inbound, _reader) =
            open_http(fake_agent(handler, 2), TEST_URL, &BTreeMap::new(), None).unwrap();
        let conn = Arc::new(McpConnection::initialize("srv", peer, inbound, &[], None).unwrap());
        assert!(!conn.is_dead());
        let caller = Arc::clone(&conn);
        let err = within(move || caller.list_tools())
            .expect("a failed POST must not leave the request hanging")
            .unwrap_err();
        match err {
            McpError::Rpc { source, .. } => {
                assert!(source.message.contains("mcp post"), "got {source:?}");
            }
            other => panic!("expected an rpc error, got {other:?}"),
        }
        assert!(wait_until(|| conn.is_dead()));
    }

    /// A handler answering `initialize` and 202 to everything else when
    /// `accept` passes the request, and 401 otherwise. GETs get 405.
    fn guarded(accept: impl Fn(&HttpRequest) -> bool + Send + Sync + 'static) -> (Handler, Log) {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&log);
        let handler = fixed(move |request: &HttpRequest| -> Vec<u8> {
            recorded.lock().unwrap().push(request.clone());
            if request.method == "GET" {
                return response_bytes("HTTP/1.1 405 Method Not Allowed", None, &[], "");
            }
            if !accept(request) {
                return response_bytes("HTTP/1.1 401 Unauthorized", None, &[], "");
            }
            if request.body.contains("\"initialize\"") {
                initialize_response()
            } else {
                response_bytes("HTTP/1.1 202 Accepted", None, &[], "")
            }
        });
        (handler, log)
    }

    type Log = Arc<Mutex<Vec<HttpRequest>>>;

    fn authorization(request: &HttpRequest) -> Option<&str> {
        request.headers.get("authorization").map(String::as_str)
    }

    #[test]
    fn the_bearer_is_sent_and_a_configured_header_wins() {
        let (handler, log) = guarded(|r| authorization(r) == Some("Bearer tok-a"));
        let tokens = StaticTokens::new("tok-a", None);
        let (peer, inbound, _reader) = open_http(
            fake_agent(handler, usize::MAX),
            TEST_URL,
            &BTreeMap::new(),
            Some(tokens),
        )
        .unwrap();
        McpConnection::initialize("srv", peer, inbound, &[], None).unwrap();
        assert!(wait_until(|| log
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.method == "GET")));
        let log = log.lock().unwrap();
        assert!(log.len() >= 3, "initialize, initialized and the stream");
        for request in log.iter() {
            assert_eq!(
                authorization(request),
                Some("Bearer tok-a"),
                "{}",
                request.method
            );
        }

        let (handler, log) = guarded(|r| authorization(r) == Some("Bearer configured"));
        let headers =
            BTreeMap::from([("Authorization".to_owned(), "Bearer configured".to_owned())]);
        let tokens = StaticTokens::new("tok-a", Some("tok-b"));
        let (peer, inbound, _reader) = open_http(
            fake_agent(handler, usize::MAX),
            TEST_URL,
            &headers,
            Some(Arc::clone(&tokens) as Arc<dyn TokenSource>),
        )
        .unwrap();
        McpConnection::initialize("srv", peer, inbound, &[], None).unwrap();
        assert_eq!(tokens.refreshes(), 0);
        let log = log.lock().unwrap();
        assert!(
            log.iter()
                .all(|r| authorization(r) == Some("Bearer configured"))
        );
    }

    #[test]
    fn one_401_refreshes_the_token_and_retries() {
        let (handler, log) = guarded(|r| authorization(r) == Some("Bearer fresh"));
        let tokens = StaticTokens::new("stale", Some("fresh"));
        let (peer, inbound, _reader) = open_http(
            fake_agent(handler, usize::MAX),
            TEST_URL,
            &BTreeMap::new(),
            Some(Arc::clone(&tokens) as Arc<dyn TokenSource>),
        )
        .unwrap();
        McpConnection::initialize("srv", peer, inbound, &[], None).unwrap();
        assert_eq!(tokens.refreshes(), 1);
        let log = log.lock().unwrap();
        let initialize: Vec<Option<&str>> = log
            .iter()
            .filter(|r| r.body.contains("\"initialize\""))
            .map(authorization)
            .collect();
        assert_eq!(initialize, [Some("Bearer stale"), Some("Bearer fresh")]);
    }

    #[test]
    fn a_second_401_fails_with_unauthorized() {
        let (handler, log) = guarded(|_| false);
        let tokens = StaticTokens::new("stale-secret", Some("fresh-secret"));
        let (peer, inbound, _reader) = open_http(
            fake_agent(handler, usize::MAX),
            TEST_URL,
            &BTreeMap::new(),
            Some(Arc::clone(&tokens) as Arc<dyn TokenSource>),
        )
        .unwrap();
        let err = McpConnection::initialize("srv", peer, inbound, &[], None)
            .err()
            .expect("a refused token fails the handshake");
        assert!(
            matches!(&err, McpError::Unauthorized { server, login: true } if server == "srv"),
            "{err:?}"
        );
        assert!(err.to_string().contains("kage mcp login srv"), "{err}");
        assert!(!format!("{err} {err:?}").contains("secret"), "{err:?}");
        assert_eq!(tokens.refreshes(), 1);
        assert_eq!(log.lock().unwrap().len(), 2, "one retry, no more");

        let (handler, _log) = guarded(|_| false);
        let (peer, inbound, _reader) = open_http(
            fake_agent(handler, usize::MAX),
            TEST_URL,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        let err = McpConnection::initialize("srv", peer, inbound, &[], None)
            .err()
            .expect("a server asking for a token fails the handshake");
        assert!(
            matches!(err, McpError::Unauthorized { login: false, .. }),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "server `srv` needs authorization");
    }

    #[test]
    fn a_refused_configured_header_suggests_no_login() {
        let (handler, _log) = guarded(|_| false);
        let headers = BTreeMap::from([("authorization".to_owned(), "Bearer fake".to_owned())]);
        let tokens = StaticTokens::new("tok-a", None);
        let (peer, inbound, _reader) = open_http(
            fake_agent(handler, usize::MAX),
            TEST_URL,
            &headers,
            Some(Arc::clone(&tokens) as Arc<dyn TokenSource>),
        )
        .unwrap();
        let err = McpConnection::initialize("srv", peer, inbound, &[], None)
            .err()
            .expect("a refused header fails the handshake");
        assert_eq!(err.to_string(), "server `srv` needs authorization");
        assert_eq!(tokens.refreshes(), 0);
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&log);
        let handler = fixed(move |request: &HttpRequest| -> Vec<u8> {
            recorded.lock().unwrap().push(request.clone());
            response_bytes(
                "HTTP/1.1 303 See Other",
                None,
                &[("location", "http://other.test/mcp")],
                "",
            )
        });
        let (peer, inbound, _reader) = open_http(
            fake_agent_with(transport_config(), handler, usize::MAX),
            TEST_URL,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        let err = McpConnection::initialize_with_timeout(
            "srv",
            peer,
            inbound,
            &[],
            None,
            Duration::from_secs(5),
        )
        .err()
        .expect("a redirect without following fails the request");
        assert!(err.to_string().contains("status 303"), "{err}");
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1, "the redirect target is never contacted");
        assert_eq!(
            log[0].headers.get("host").map(String::as_str),
            Some("mcp.test")
        );
    }

    #[test]
    fn server_request_during_a_streaming_call_is_answered() {
        let (roots_tx, roots_rx) = std::sync::mpsc::channel::<serde_json::Value>();
        let roots_rx = Mutex::new(roots_rx);
        let handler: Handler = Arc::new(move |request, stream| {
            if request.method == "GET" {
                let _ = stream.write_all(&response_bytes(
                    "HTTP/1.1 405 Method Not Allowed",
                    None,
                    &[],
                    "",
                ));
                return;
            }
            let body: serde_json::Value =
                serde_json::from_str(&request.body).unwrap_or(serde_json::Value::Null);
            let response = match body.get("method").and_then(|m| m.as_str()) {
                Some("initialize") => initialize_response(),
                Some("notifications/initialized") => {
                    response_bytes("HTTP/1.1 202 Accepted", None, &[], "")
                }
                Some("tools/call") => {
                    let ask = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": "roots-1",
                        "method": "roots/list",
                    });
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                         connection: close\r\n\r\ndata: {ask}\n\n"
                    );
                    let _ = stream.flush();
                    let Ok(roots) = roots_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                    else {
                        return;
                    };
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": body["id"],
                        "result": {
                            "content": [ { "type": "text", "text": roots["roots"][0]["uri"] } ]
                        }
                    });
                    let _ = write!(stream, "data: {reply}\n\n");
                    return;
                }
                None if body["id"] == "roots-1" => {
                    let _ = roots_tx.send(body["result"].clone());
                    response_bytes("HTTP/1.1 202 Accepted", None, &[], "")
                }
                _ => response_bytes("HTTP/1.1 500 Internal Server Error", None, &[], ""),
            };
            let _ = stream.write_all(&response);
        });
        let (peer, inbound, _reader) = open_http(
            fake_agent(handler, usize::MAX),
            TEST_URL,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        let roots = [std::path::PathBuf::from("/work/project")];
        let conn = McpConnection::initialize("srv", peer, inbound, &roots, None).unwrap();
        let result = within(move || {
            conn.request(
                "tools/call",
                serde_json::json!({ "name": "t", "arguments": {} }),
            )
        })
        .expect("the call must finish while the server asks for roots")
        .unwrap();
        assert_eq!(result["content"][0]["text"], "file:///work/project");
    }
}
