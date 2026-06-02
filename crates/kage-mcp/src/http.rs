//! Remote MCP transport over HTTP + Server-Sent Events.
//!
//! kage opens the server's SSE stream with a GET, the server announces a
//! POST endpoint via an `endpoint` event, kage POSTs each JSON-RPC
//! message to that endpoint, and responses (plus any server-initiated
//! requests/notifications) arrive back over the SSE stream. That shape
//! maps onto the byte-stream the shared [`kage_jsonrpc::connect`]
//! expects, so the HTTP transport reuses the exact same [`Peer`],
//! request routing, and cancellation as the stdio transport:
//!
//! * the read half ([`SseToJsonLines`]) strips SSE framing and yields
//!   one newline-delimited JSON-RPC message per `data` event, capturing
//!   the announced endpoint out of band;
//! * the write half ([`HttpPoster`]) buffers each message the peer
//!   writes and POSTs it to the announced endpoint on flush, blocking
//!   until the endpoint is known.
//!
//! Synchronous throughout: blocking `ureq` calls and `std::thread`, no
//! async, matching the rest of the workspace.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use kage_jsonrpc::{Inbound, Peer, connect};

/// How long [`HttpPoster::flush`] waits for the server to announce its
/// POST endpoint before giving up. Generous: the endpoint event is the
/// first SSE frame, so this only trips on a misbehaving server.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on a single SSE line, so a server cannot exhaust memory with one
/// unterminated frame.
const MAX_SSE_LINE: u64 = 8 * 1024 * 1024;

/// Lazily-discovered POST endpoint announced by the server's `endpoint`
/// SSE event. The reader fills it; the writer blocks on the condvar
/// until it is set.
type Endpoint = Arc<(Mutex<Option<String>>, Condvar)>;

/// Open an HTTP+SSE connection to `url`, sending `headers` on both the
/// SSE GET and every POST, and hand the adapted byte streams to
/// [`kage_jsonrpc::connect`].
///
/// # Errors
///
/// Returns a message when the SSE GET cannot be opened.
pub(crate) fn connect_http(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<(Peer, Receiver<Inbound>, JoinHandle<()>), String> {
    let agent = ureq::Agent::new_with_defaults();
    let mut req = agent.get(url).header("accept", "text/event-stream");
    for (key, value) in headers {
        req = req.header(key.as_str(), value.as_str());
    }
    let response = req.call().map_err(|e| format!("open SSE {url}: {e}"))?;
    let body: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());

    let endpoint: Endpoint = Arc::new((Mutex::new(None), Condvar::new()));
    let reader = BufReader::new(SseToJsonLines::new(
        BufReader::new(body),
        Arc::clone(&endpoint),
        url.to_owned(),
    ));
    let writer = HttpPoster::new(agent, endpoint, headers.clone());
    Ok(connect(reader, writer))
}

/// One parsed SSE frame: its `event` name (empty when unset) and the
/// joined `data` payload.
struct SseFrame {
    event: String,
    data: String,
}

/// Read the next SSE frame from `reader`, returning `None` at EOF.
///
/// Comment lines (`:` prefix) are ignored, multiple `data:` lines are
/// joined with `\n`, and a blank line terminates the frame.
fn read_sse_frame<R: BufRead>(reader: &mut R) -> io::Result<Option<SseFrame>> {
    let mut event = String::new();
    let mut data: Vec<String> = Vec::new();
    let mut saw_any = false;
    loop {
        let mut line = String::new();
        let n = reader.by_ref().take(MAX_SSE_LINE).read_line(&mut line)?;
        if n == 0 {
            if saw_any && !data.is_empty() {
                return Ok(Some(SseFrame {
                    event,
                    data: data.join("\n"),
                }));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if saw_any && !data.is_empty() {
                return Ok(Some(SseFrame {
                    event,
                    data: data.join("\n"),
                }));
            }
            continue;
        }
        saw_any = true;
        if let Some(rest) = trimmed.strip_prefix("event:") {
            rest.trim_start().clone_into(&mut event);
        } else if let Some(rest) = trimmed.strip_prefix("data:") {
            data.push(rest.trim_start().to_owned());
        }
        // Comments (':' prefix) and unknown fields are ignored.
    }
}

/// Resolve an endpoint announced by the server (often a relative path)
/// against the SSE `base` URL.
fn resolve_endpoint(base: &str, announced: &str) -> Result<String, String> {
    let base = url::Url::parse(base).map_err(|e| format!("base url {base}: {e}"))?;
    let joined = base
        .join(announced)
        .map_err(|e| format!("endpoint {announced}: {e}"))?;
    Ok(joined.to_string())
}

/// `Read` adapter that turns an SSE byte stream into newline-delimited
/// JSON-RPC messages, capturing the announced endpoint out of band.
struct SseToJsonLines<R: BufRead> {
    inner: R,
    endpoint: Endpoint,
    base: String,
    pending: Vec<u8>,
    pos: usize,
}

impl<R: BufRead> SseToJsonLines<R> {
    fn new(inner: R, endpoint: Endpoint, base: String) -> Self {
        Self {
            inner,
            endpoint,
            base,
            pending: Vec::new(),
            pos: 0,
        }
    }

    /// Pull SSE frames until a JSON-RPC message is ready in `pending`,
    /// routing `endpoint` events to the shared slot. Returns `false` at
    /// stream end.
    fn refill(&mut self) -> io::Result<bool> {
        loop {
            let Some(frame) = read_sse_frame(&mut self.inner)? else {
                return Ok(false);
            };
            if frame.event == "endpoint" {
                let resolved = resolve_endpoint(&self.base, &frame.data)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let (lock, cv) = &*self.endpoint;
                *lock.lock().expect("mcp endpoint mutex poisoned") = Some(resolved);
                cv.notify_all();
                continue;
            }
            if frame.data.is_empty() {
                continue;
            }
            self.pending = frame.data.into_bytes();
            self.pending.push(b'\n');
            self.pos = 0;
            return Ok(true);
        }
    }
}

impl<R: BufRead> Read for SseToJsonLines<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.pending.len() && !self.refill()? {
            return Ok(0);
        }
        let n = (&self.pending[self.pos..]).read(buf)?;
        self.pos += n;
        Ok(n)
    }
}

/// `Write` adapter that POSTs each JSON-RPC message the peer writes to
/// the server's announced endpoint.
struct HttpPoster {
    agent: ureq::Agent,
    endpoint: Endpoint,
    headers: BTreeMap<String, String>,
    buf: Vec<u8>,
}

impl HttpPoster {
    fn new(agent: ureq::Agent, endpoint: Endpoint, headers: BTreeMap<String, String>) -> Self {
        Self {
            agent,
            endpoint,
            headers,
            buf: Vec::new(),
        }
    }

    /// Block until the server has announced its POST endpoint, or time
    /// out. Returns the resolved endpoint URL.
    fn wait_endpoint(&self) -> io::Result<String> {
        let (lock, cv) = &*self.endpoint;
        let guard = lock.lock().expect("mcp endpoint mutex poisoned");
        let (guard, timeout) = cv
            .wait_timeout_while(guard, ENDPOINT_TIMEOUT, |e| e.is_none())
            .expect("mcp endpoint mutex poisoned");
        if timeout.timed_out() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "mcp server did not announce an SSE endpoint",
            ));
        }
        Ok(guard.clone().expect("endpoint set once wait returns"))
    }
}

impl Write for HttpPoster {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let url = self.wait_endpoint()?;
        let body = std::mem::take(&mut self.buf);
        let mut req = self
            .agent
            .post(&url)
            .header("content-type", "application/json");
        for (key, value) in &self.headers {
            req = req.header(key.as_str(), value.as_str());
        }
        req.send(&body[..])
            .map_err(|e| io::Error::other(format!("mcp post {url}: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use super::*;

    fn sse_reader(bytes: &'static [u8]) -> (SseToJsonLines<BufReader<&'static [u8]>>, Endpoint) {
        let endpoint: Endpoint = Arc::new((Mutex::new(None), Condvar::new()));
        let reader = SseToJsonLines::new(
            BufReader::new(bytes),
            Arc::clone(&endpoint),
            "https://example.com/sse".to_owned(),
        );
        (reader, endpoint)
    }

    #[test]
    fn captures_endpoint_and_emits_message_lines() {
        let stream: &[u8] = b"event: endpoint\ndata: /messages?session=abc\n\n\
            event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let (reader, endpoint) = sse_reader(stream);
        let mut buf = BufReader::new(reader);
        let mut line = String::new();
        buf.read_line(&mut line).unwrap();
        assert_eq!(
            line.trim_end(),
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}"
        );
        assert_eq!(
            endpoint.0.lock().unwrap().as_deref(),
            Some("https://example.com/messages?session=abc")
        );
    }

    #[test]
    fn frame_joins_multi_line_data_and_skips_comments() {
        let mut reader = BufReader::new(&b": keep-alive\nevent: message\ndata: a\ndata: b\n\n"[..]);
        let frame = read_sse_frame(&mut reader).unwrap().unwrap();
        assert_eq!(frame.event, "message");
        assert_eq!(frame.data, "a\nb");
    }

    #[test]
    fn skips_comment_then_emits_message_line() {
        let stream: &[u8] = b": keep-alive\nevent: message\ndata: {\"id\":2}\n\n";
        let (reader, _ep) = sse_reader(stream);
        let mut buf = BufReader::new(reader);
        let mut line = String::new();
        buf.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"id\":2}\n");
    }

    #[test]
    fn stream_end_yields_eof() {
        let (reader, _ep) = sse_reader(b"");
        let mut buf = BufReader::new(reader);
        let mut line = String::new();
        assert_eq!(buf.read_line(&mut line).unwrap(), 0);
    }

    #[test]
    fn resolve_endpoint_handles_relative_and_absolute() {
        assert_eq!(
            resolve_endpoint("https://h/sse", "/messages?s=1").unwrap(),
            "https://h/messages?s=1"
        );
        assert_eq!(
            resolve_endpoint("https://h/sse", "https://other/post").unwrap(),
            "https://other/post"
        );
    }

    #[test]
    fn poster_waits_for_endpoint_then_returns_it() {
        let endpoint: Endpoint = Arc::new((Mutex::new(None), Condvar::new()));
        let poster = HttpPoster::new(
            ureq::Agent::new_with_defaults(),
            Arc::clone(&endpoint),
            BTreeMap::new(),
        );
        let setter = Arc::clone(&endpoint);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let (lock, cv) = &*setter;
            *lock.lock().unwrap() = Some("https://h/post".to_owned());
            cv.notify_all();
        });
        assert_eq!(poster.wait_endpoint().unwrap(), "https://h/post");
    }
}
