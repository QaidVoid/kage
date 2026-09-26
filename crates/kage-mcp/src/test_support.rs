//! Fixtures shared by this crate's tests: scripted MCP servers over
//! in-process pipes, a fake HTTP server, and a static token source.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kage_jsonrpc::{Inbound, Peer, RpcError};
use serde_json::{Value, json};

use crate::oauth::TokenSource;
use crate::server::{McpConnection, PROTOCOL_VERSION};

/// Poll `done` for up to five seconds. Returns whether it came true, so
/// the caller's assert can report the real values.
pub(crate) fn wait_until(done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    true
}

/// Requests a scripted server received, as `(method, params)`.
pub(crate) type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// Answer the requests on `inbound` from a thread until the client
/// hangs up: `initialize` with `capabilities`, and every other request
/// with `answer`, recorded in the returned [`Seen`].
pub(crate) fn answer_requests(
    peer: Peer,
    inbound: Receiver<Inbound>,
    capabilities: Value,
    answer: impl Fn(&str, &Value) -> Result<Value, RpcError> + Send + 'static,
) -> Seen {
    let seen = Seen::default();
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for msg in inbound {
            let Inbound::Request { id, method, params } = msg else {
                continue;
            };
            let outcome = if method == "initialize" {
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": capabilities,
                }))
            } else {
                log.lock().unwrap().push((method.clone(), params.clone()));
                answer(&method, &params)
            };
            let _ = peer.respond(&id, outcome);
        }
    });
    seen
}

/// An in-process server named `name` that advertises `capabilities`
/// and answers every other request with `answer`. Returns the client
/// connection, the server's peer (for notifications) and the requests
/// it saw after `initialize`.
pub(crate) fn scripted(
    name: &str,
    capabilities: Value,
    answer: impl Fn(&str, &Value) -> Result<Value, RpcError> + Send + 'static,
) -> (Arc<McpConnection>, Peer, Seen) {
    let ((cli_peer, cli_in), (srv_peer, srv_in)) = kage_jsonrpc::testing::pair();
    let seen = answer_requests(srv_peer.clone(), srv_in, capabilities, answer);
    let conn = McpConnection::initialize(name, cli_peer, cli_in, &[], None).unwrap();
    (Arc::new(conn), srv_peer, seen)
}

/// One HTTP request a fake server saw, header names lowercased.
#[derive(Clone, Debug)]
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: String,
}

impl HttpRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub(crate) fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
}

/// Parse one HTTP/1.1 request off `stream`: the request line, the
/// headers, and a content-length-delimited body.
pub(crate) fn read_request(stream: impl Read) -> Option<HttpRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(HttpRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// The fake server's answer to one request.
pub(crate) struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    pub(crate) fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.to_string(),
        }
    }

    pub(crate) fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

/// A fake HTTP server on 127.0.0.1: `base` is its origin and `seen`
/// every request it answered, in order.
pub(crate) struct FakeServer {
    pub(crate) base: String,
    pub(crate) seen: Arc<Mutex<Vec<HttpRequest>>>,
}

impl FakeServer {
    pub(crate) fn paths(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap();
        seen.iter()
            .map(|s| format!("{} {}", s.method, s.path))
            .collect()
    }

    pub(crate) fn find(&self, method: &str, path: &str) -> Option<HttpRequest> {
        let seen = self.seen.lock().unwrap();
        seen.iter()
            .find(|s| s.method == method && s.path == path)
            .cloned()
    }
}

/// Serve every connection with `handler(request, base)`, one request
/// per connection, on a thread per connection.
pub(crate) fn serve(
    handler: impl Fn(&HttpRequest, &str) -> Reply + Send + Sync + 'static,
) -> FakeServer {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = Arc::new(handler);
    let (log, origin) = (Arc::clone(&seen), base.clone());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (handler, log, origin) = (Arc::clone(&handler), Arc::clone(&log), origin.clone());
            thread::spawn(move || {
                let Some(request) = read_request(&stream) else {
                    return;
                };
                log.lock().unwrap().push(request.clone());
                let reply = handler(&request, &origin);
                let mut head = format!(
                    "HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (name, value) in &reply.headers {
                    let _ = write!(head, "{name}: {value}\r\n");
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.body.as_bytes());
            });
        }
    });
    FakeServer { base, seen }
}

/// A token source with one stored token and, optionally, the token a
/// refresh yields, which is stored from then on. Counts the refreshes
/// asked for.
pub(crate) struct StaticTokens {
    bearer: Mutex<String>,
    fresh: Option<String>,
    rejected: AtomicUsize,
}

impl StaticTokens {
    pub(crate) fn new(bearer: &str, fresh: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            bearer: Mutex::new(bearer.to_owned()),
            fresh: fresh.map(str::to_owned),
            rejected: AtomicUsize::new(0),
        })
    }

    pub(crate) fn refreshes(&self) -> usize {
        self.rejected.load(Ordering::SeqCst)
    }

    /// Drop the stored token, like a logout.
    pub(crate) fn forget(&self) {
        self.bearer.lock().unwrap().clear();
    }
}

impl TokenSource for StaticTokens {
    fn bearer(&self, _url: &str) -> Option<String> {
        Some(self.bearer.lock().unwrap().clone()).filter(|token| !token.is_empty())
    }

    fn rejected(&self, _url: &str, _token: &str) -> Option<String> {
        self.rejected.fetch_add(1, Ordering::SeqCst);
        let fresh = self.fresh.clone()?;
        fresh.clone_into(&mut self.bearer.lock().unwrap());
        Some(fresh)
    }
}
