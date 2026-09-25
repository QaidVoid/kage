//! Remote MCP transport over Streamable HTTP.
//!
//! Both directions talk to one endpoint. kage POSTs each JSON-RPC
//! message and the reply rides on that POST's response: a JSON body,
//! a `text/event-stream` of frames, or a bare `202 Accepted` when the
//! server answers later. A GET on the same endpoint opens the optional
//! server-initiated stream; kage opens it once, after the first
//! successful POST, and forwards any JSON frames it produces into the
//! same pipe the POST responses feed. The session id the server hands
//! back is echoed as `mcp-session-id` on later requests.
//!
//! This maps onto the byte stream [`kage_jsonrpc::connect`] expects, so
//! the HTTP transport reuses the exact same [`Peer`], request routing,
//! and cancellation as the stdio transport.
//!
//! Two simplifications over the spec: `MCP-Protocol-Version` carries
//! the version kage advertises rather than the negotiated one, and a
//! POST failure closes the transport only when the server could not
//! have routed the request at all (404, which the spec defines as an
//! expired session, or an unreachable host); any other HTTP status
//! fails that one request and leaves the transport open.
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
//! until the server closes it or the process exits; closing the
//! transport only takes away the pipe writer they forward into.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use kage_jsonrpc::{Inbound, Peer, connect_with};

use crate::server::{PROTOCOL_VERSION, cancel_notice};

/// How long the GET pump waits for the first successful POST before
/// giving up on the server-initiated stream. Generous: this only
/// trips on a server that never answers.
const STREAM_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Cap on a single SSE frame, so a server cannot exhaust memory with
/// one endless stream of lines.
const MAX_SSE_LINE: u64 = 8 * 1024 * 1024;

/// Cap on a single non-SSE POST response body.
const MAX_JSON_BODY: u64 = 8 * 1024 * 1024;

/// Session state shared between the POST writer and the GET pump:
/// the server-assigned session id and whether any POST has succeeded.
#[derive(Default)]
struct Shared {
    session_id: Option<String>,
    ready: bool,
}

/// Session state behind the condvar the GET pump waits on.
type SharedState = Arc<(Mutex<Shared>, Condvar)>;

/// The transport's write end of the pipe feeding the jsonrpc reader.
/// Taken away by [`HttpPoster::close`] to end the stream.
type OutSlot = Arc<Mutex<Option<io::PipeWriter>>>;

/// Open a Streamable HTTP connection to `url`, sending `headers` on
/// every request, and hand the adapted pipe to
/// [`kage_jsonrpc::connect_with`] with the MCP cancel notice.
///
/// # Errors
///
/// Returns a message when the local pipe cannot be created.
pub(crate) fn connect_http(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<(Peer, Receiver<Inbound>, JoinHandle<()>), String> {
    open_http(ureq::Agent::new_with_defaults(), url, headers)
}

/// [`connect_http`] against an explicit agent, so tests can run the
/// whole transport against an in-process fake server.
fn open_http(
    agent: ureq::Agent,
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<(Peer, Receiver<Inbound>, JoinHandle<()>), String> {
    let (pipe_reader, pipe_writer) = io::pipe().map_err(|e| format!("open mcp pipe: {e}"))?;
    let out: OutSlot = Arc::new(Mutex::new(Some(pipe_writer)));
    let shared: SharedState = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
    spawn_get_pump(
        agent.clone(),
        Arc::clone(&shared),
        Arc::clone(&out),
        url.to_owned(),
        headers.clone(),
    );
    let writer = HttpPoster {
        agent,
        url: url.to_owned(),
        headers: headers.clone(),
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
fn pump_sse<R: Read>(body: R, out: &OutSlot) -> io::Result<()> {
    let mut reader = BufReader::new(body);
    while let Some(data) = read_sse_frame(&mut reader, MAX_SSE_LINE)? {
        if data.is_empty() || serde_json::from_str::<serde_json::Value>(&data).is_err() {
            continue;
        }
        forward(out, data.as_bytes())?;
    }
    Ok(())
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

/// A failed POST, and whether it leaves the transport unusable.
struct PostFailure {
    error: io::Error,
    fatal: bool,
}

impl PostFailure {
    fn fatal(error: io::Error) -> Self {
        Self { error, fatal: true }
    }
}

/// POST one JSON-RPC message and absorb the response. A successful
/// response records the session id (first one wins) and unblocks the
/// GET pump before any body is forwarded, so the next POST already
/// carries the session; an SSE response or a JSON body is forwarded
/// into the pipe, a 202 or empty body is a bare success.
fn post_and_forward(
    agent: &ureq::Agent,
    url: &str,
    headers: &BTreeMap<String, String>,
    shared: &SharedState,
    out: &OutSlot,
    body: &[u8],
) -> Result<(), PostFailure> {
    let mut req = agent
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json");
    {
        let guard = kage_core::sync::lock(&shared.0);
        if let Some(session) = &guard.session_id {
            req = req.header("mcp-session-id", session.as_str());
        }
        if guard.ready {
            req = req.header("MCP-Protocol-Version", PROTOCOL_VERSION);
        }
    }
    // Configured headers go last so they can override the defaults.
    for (key, value) in headers {
        req = req.header(key.as_str(), value.as_str());
    }
    let response = match req.send(body) {
        Ok(response) => response,
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
        guard.ready = true;
    }
    shared.1.notify_all();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    // A failure past this point may leave the response part-consumed,
    // so the transport can no longer be trusted.
    if content_type.starts_with("text/event-stream") {
        return pump_sse(response.into_body().into_reader(), out).map_err(PostFailure::fatal);
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
    forward(out, &payload).map_err(PostFailure::fatal)
}

/// `Write` adapter that POSTs each buffered JSON-RPC message to the
/// server endpoint on flush and absorbs the response into the pipe.
struct HttpPoster {
    agent: ureq::Agent,
    url: String,
    headers: BTreeMap<String, String>,
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
            return post_and_forward(
                &self.agent,
                &self.url,
                &self.headers,
                &self.shared,
                &self.out,
                &body,
            )
            .map_err(|failure| {
                if failure.fatal {
                    close(&self.out);
                }
                failure.error
            });
        };
        let agent = self.agent.clone();
        let url = self.url.clone();
        let headers = self.headers.clone();
        let shared = Arc::clone(&self.shared);
        let out = Arc::clone(&self.out);
        std::thread::spawn(move || {
            let Err(failure) = post_and_forward(&agent, &url, &headers, &shared, &out, &body)
            else {
                return;
            };
            let reply = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": failure.error.to_string() },
            });
            let _ = forward(&out, reply.to_string().as_bytes());
            if failure.fatal {
                close(&out);
            }
        });
        Ok(())
    }
}

/// Spawn the detached GET pump: wait for the first successful POST,
/// then open the optional server-initiated stream and forward its
/// JSON frames into the pipe. A refused or non-SSE answer (the spec
/// allows a plain 405) ends the pump silently.
fn spawn_get_pump(
    agent: ureq::Agent,
    shared: SharedState,
    out: OutSlot,
    url: String,
    headers: BTreeMap<String, String>,
) {
    std::thread::spawn(move || {
        let (lock, cv) = &*shared;
        let guard = kage_core::sync::lock(lock);
        let (guard, waited) = cv
            .wait_timeout_while(guard, STREAM_READY_TIMEOUT, |s| !s.ready)
            .expect("mcp http state mutex poisoned");
        if waited.timed_out() {
            return;
        }
        let session = guard.session_id.clone();
        drop(guard);
        let mut req = agent.get(&url).header("accept", "text/event-stream");
        if let Some(session) = &session {
            req = req.header("mcp-session-id", session.as_str());
        }
        req = req.header("MCP-Protocol-Version", PROTOCOL_VERSION);
        for (key, value) in &headers {
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
        let _ = pump_sse(response.into_body().into_reader(), &out);
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fmt;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::server::{McpConnection, McpError};
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
        let stream: &[u8] = b": keep-alive\nevent: endpoint\ndata: /mcp\n\n\
            event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7}\n\n\
            data: not json\n\n";
        pump_sse(stream, &out).unwrap();
        *kage_core::sync::lock(&out) = None;
        let mut got = String::new();
        BufReader::new(&mut pipe_rx).read_line(&mut got).unwrap();
        assert_eq!(got.trim_end(), "{\"jsonrpc\":\"2.0\",\"id\":7}");
        let mut eof = [0u8; 1];
        assert_eq!(pipe_rx.read(&mut eof).unwrap(), 0);
    }

    /// One request seen by the test server.
    #[derive(Clone)]
    struct Recorded {
        method: String,
        headers: HashMap<String, String>,
        body: String,
    }

    /// The handler each test server call goes through: one recorded
    /// request in, the HTTP response written to the connection.
    type Handler = Arc<dyn Fn(&Recorded, &mut UnixStream) + Send + Sync>;

    /// A [`Handler`] answering every request with the full response
    /// `respond` builds.
    fn fixed(respond: impl Fn(&Recorded) -> Vec<u8> + Send + Sync + 'static) -> Handler {
        Arc::new(move |request, stream| {
            let _ = stream.write_all(&respond(request));
        })
    }

    /// Parse one HTTP/1.1 request off `stream`: request line, headers
    /// (keys lowercased), and a content-length-delimited body.
    fn read_request(stream: &mut UnixStream) -> io::Result<Recorded> {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let method = request_line
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
            }
        }
        let length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        Ok(Recorded {
            method,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
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
                let Ok(request) = read_request(&mut remote) else {
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
        ureq::Agent::with_parts(
            Config::default(),
            FakeServerConnector {
                handler,
                served: Arc::new(AtomicUsize::new(0)),
                quota,
            },
            NoResolver,
        )
    }

    /// Poll `cond` for up to 5 s; the caller's assert reports the
    /// failure with the real values when it never becomes true.
    fn wait_for(cond: impl Fn() -> bool) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
        let log: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&log);
        let handler = fixed(move |request: &Recorded| -> Vec<u8> {
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
        let (peer, inbound, _reader) =
            open_http(fake_agent(handler, usize::MAX), TEST_URL, &BTreeMap::new()).unwrap();
        let conn = McpConnection::initialize("srv", peer, inbound, &[], None).unwrap();
        assert_eq!(conn.protocol_version(), "2025-06-18");
        let tools = conn.list_tools().unwrap();
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["t"]
        );
        wait_for(|| log.lock().unwrap().iter().any(|r| r.method == "GET"));
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
        let handler = fixed(|request: &Recorded| -> Vec<u8> {
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
        let (peer, inbound, _reader) =
            open_http(fake_agent(handler, usize::MAX), TEST_URL, &BTreeMap::new()).unwrap();
        let conn = McpConnection::initialize("srv", peer, inbound, &[], None).unwrap();
        assert_eq!(conn.protocol_version(), "2025-06-18");
    }

    #[test]
    fn transport_failure_fails_the_request_and_marks_connection_dead() {
        let handler = fixed(|request: &Recorded| -> Vec<u8> {
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
            open_http(fake_agent(handler, 2), TEST_URL, &BTreeMap::new()).unwrap();
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
        wait_for(|| conn.is_dead());
        assert!(conn.is_dead());
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
        let (peer, inbound, _reader) =
            open_http(fake_agent(handler, usize::MAX), TEST_URL, &BTreeMap::new()).unwrap();
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
