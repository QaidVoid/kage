//! `kage serve` end to end on real sockets: a mock-provider host behind
//! the same accept loop `kage serve` runs, driven by WebSocket clients.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use kage_core::config::McpServer as McpSpec;
use kage_core::permissions::PermissionsConfig;
use kage_core::{CancelFlag, StopReason as CoreStopReason, TokenUsage};
use kage_jsonrpc::{Inbound, Peer};
use kage_loop::{AgentContext, LoopConfig};
use kage_provider::testing::MockProvider;
use kage_provider::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, ProviderModel,
    ProviderRegistry, StreamRequest,
};
use kage_remote::token::Token;
use kage_tools::builtin_registry;
use tungstenite::WebSocket;
use tungstenite::http::StatusCode;
use tungstenite::protocol::Role;

use super::*;
use crate::engine::SessionSpec;
use crate::permissions::PermissionGate;
use crate::rpc::host::Host;

/// Every wait in these tests is bounded by this.
const WAIT: Duration = Duration::from_secs(10);

type Script = Vec<Result<ProviderEvent, ProviderError>>;

/// One turn that replies with `text`.
fn text_turn(text: &str) -> Script {
    vec![
        Ok(ProviderEvent::MessageStart),
        Ok(ProviderEvent::TextDelta {
            delta: text.to_owned(),
        }),
        Ok(ProviderEvent::MessageEnd {
            stop_reason: CoreStopReason::EndTurn,
            usage: TokenUsage::default(),
        }),
    ]
}

/// The mock provider, offering `mock:m` and `mock:other` to pickers.
#[derive(Debug)]
struct Listed {
    mock: MockProvider,
}

impl Provider for Listed {
    fn metadata(&self) -> &ProviderMetadata {
        self.mock.metadata()
    }

    fn stream(
        &self,
        req: StreamRequest,
        cancel: &CancelFlag,
    ) -> Result<EventStream, ProviderError> {
        self.mock.stream(req, cancel)
    }

    fn models(&self) -> Vec<ProviderModel> {
        ["m", "other"]
            .map(|id| ProviderModel {
                id: id.into(),
                name: format!("Mock {id}"),
                ..ProviderModel::default()
            })
            .into()
    }
}

/// A host like the rpc tests build, on the mock provider, whose
/// sessions run in `workdir`.
fn test_host(scripts: Vec<Script>, workdir: &Path) -> Arc<Host> {
    let mock = MockProvider::sequence(scripts);
    let registry = Arc::new(ProviderRegistry::new().with(Arc::new(Listed { mock })));
    let spec_workdir = workdir.to_path_buf();
    let spec = Box::new(
        move |id, _cwd: &str, model: &str, _servers: BTreeMap<String, McpSpec>| {
            Ok(SessionSpec {
                id,
                model: model.to_owned(),
                cx: AgentContext::new(model, "").with_workdir(&spec_workdir),
                recorder: None,
                tools: builtin_registry(),
                gate: PermissionGate::new(PermissionsConfig::default()),
                loop_cfg: LoopConfig::default(),
                plugins: None,
                mcp: None,
                interactive: true,
                title: false,
                shell: None,
                agents: None,
            })
        },
    );
    Host::new(
        registry,
        "mock:m".to_owned(),
        workdir.to_path_buf(),
        spec,
        BTreeMap::new(),
    )
}

/// The server under test: the real accept loop on an ephemeral port,
/// with the log lines collected instead of printed.
struct Server {
    _dir: tempfile::TempDir,
    addr: SocketAddr,
    token: Arc<Token>,
    stop: Arc<AtomicBool>,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Server {
    /// The collected `kage serve:` lines, without the prefix.
    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    /// Stops the accept loop. The serving threads outlive the test,
    /// like the pipe threads of the rpc tests.
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Serves `scripts` from a mock host with a fresh token.
fn spawn_server(scripts: Vec<Script>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let host = test_host(scripts, dir.path());
    let token = Arc::new(Token::load_or_create(&dir.path().join("remote-token")).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let sink = Arc::clone(&lines);
    let log: Log = Arc::new(move |line| sink.lock().unwrap().push(line.to_owned()));
    let served_token = Arc::clone(&token);
    let served_stop = Arc::clone(&stop);
    let served_log = Arc::clone(&log);
    thread::spawn(move || {
        accept_until(&listener, &host, &served_token, &served_stop, &served_log);
    });
    Server {
        _dir: dir,
        addr,
        token,
        stop,
        lines,
    }
}

/// A WebSocket client of the server, as a JSON-RPC peer. Dropping it
/// shuts the socket down, because the peer's reader thread would
/// otherwise hold it open forever.
struct Client {
    peer: Peer,
    inbox: mpsc::Receiver<Inbound>,
    sock: Arc<TcpStream>,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

/// Connects a WebSocket client with the token in the query.
fn connect_client(server: &Server) -> Client {
    let uri = format!("ws://{}/acp?token={}", server.addr, server.token.as_str());
    let (socket, response) = tungstenite::connect(uri).unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    let tungstenite::stream::MaybeTlsStream::Plain(sock) = socket.get_ref() else {
        panic!("expected a plain socket");
    };
    let reader_sock = sock.try_clone().unwrap();
    let writer_sock = sock.try_clone().unwrap();
    let sock = Arc::new(sock.try_clone().unwrap());
    drop(socket);
    let frames_in = WebSocket::from_partially_read(reader_sock, Vec::new(), Role::Client, None);
    let frames_out = WebSocket::from_raw_socket(writer_sock, Role::Client, None);
    let (peer, inbox, _thread) = kage_jsonrpc::connect(
        BufReader::new(FrameReader {
            ws: frames_in,
            buffer: Vec::new(),
        }),
        FrameWriter {
            ws: frames_out,
            buffer: Vec::new(),
        },
    );
    Client { peer, inbox, sock }
}

/// Reads WebSocket text frames as JSON-RPC lines: one frame, one line,
/// like the server's pipe does in reverse.
struct FrameReader {
    ws: WebSocket<TcpStream>,
    buffer: Vec<u8>,
}

impl Read for FrameReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.buffer.is_empty() {
            match self.ws.read() {
                Ok(tungstenite::Message::Text(text)) => {
                    self.buffer.extend_from_slice(text.as_bytes());
                    self.buffer.push(b'\n');
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => return Ok(0),
                Ok(_) => {}
            }
        }
        let n = buf.len().min(self.buffer.len());
        buf[..n].copy_from_slice(&self.buffer[..n]);
        self.buffer.drain(..n);
        Ok(n)
    }
}

/// Writes one text frame per flush.
struct FrameWriter {
    ws: WebSocket<TcpStream>,
    buffer: Vec<u8>,
}

impl Write for FrameWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let message = String::from_utf8_lossy(&self.buffer).into_owned();
        self.buffer.clear();
        self.ws
            .send(tungstenite::Message::text(message))
            .map_err(io::Error::other)
    }
}

impl Client {
    /// Sends `method` and waits up to [`WAIT`] for the answer.
    fn request(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.peer
            .request_timeout(method, params, WAIT)
            .unwrap_or_else(|e| panic!("{method}: {e}"))
    }

    /// The text of the first `agent_message_chunk`, once it arrives.
    fn reply_text(&self) -> String {
        loop {
            match self.inbox.recv_timeout(WAIT).expect("no message chunk") {
                Inbound::Notification { params, .. }
                    if params["update"]["sessionUpdate"] == "agent_message_chunk" =>
                {
                    return params["update"]["content"]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                }
                Inbound::Notification { .. } => {}
                other @ Inbound::Request { .. } => {
                    panic!("expected notifications, got {other:?}")
                }
            }
        }
    }
}

/// The ACP initialize handshake.
fn initialize(client: &Client) {
    let response = client.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": kage_acp::acp::PROTOCOL_VERSION,
            "clientCapabilities": {},
        }),
    );
    assert_eq!(response["agentInfo"]["name"], "kage");
}

/// Opens a session and returns its id.
fn new_session(client: &Client) -> String {
    let response = client.request(
        "session/new",
        serde_json::json!({"cwd": "/tmp", "mcpServers": []}),
    );
    response["sessionId"].as_str().unwrap().to_owned()
}

/// Starts a prompt on `session`.
fn prompt(client: &Client, session: &str) -> serde_json::Value {
    client.request(
        "session/prompt",
        serde_json::json!({
            "sessionId": session,
            "prompt": [{"type": "text", "text": "hi"}],
        }),
    )
}

/// Sends a raw HTTP request and returns the client address together
/// with the whole reply.
fn http_request(addr: SocketAddr, request: &str) -> (SocketAddr, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let local = stream.local_addr().unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    (local, reply)
}

fn request_with_token(addr: SocketAddr, request_line: &str, token: &Token) -> (SocketAddr, String) {
    http_request(
        addr,
        &format!(
            "{request_line} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\n\r\n",
            token.as_str()
        ),
    )
}

/// Completes a WebSocket upgrade on a raw socket and keeps it open.
fn open_upgrade(addr: SocketAddr, token: &Token) -> (TcpStream, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let request = format!(
        "GET /acp HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Authorization: Bearer {}\r\n\r\n",
        token.as_str()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut reply = String::new();
    let mut buffered = BufReader::new(stream.try_clone().unwrap());
    loop {
        let mut line = String::new();
        let read = buffered.read_line(&mut line).unwrap();
        let done = read == 0 || line == "\r\n";
        reply.push_str(&line);
        if done {
            break;
        }
    }
    (stream, reply)
}

#[test]
fn two_clients_initialize_open_sessions_and_prompt() {
    let server = spawn_server(vec![text_turn("one"), text_turn("two")]);

    let first = connect_client(&server);
    initialize(&first);
    let mine = new_session(&first);
    let second = connect_client(&server);
    initialize(&second);
    let theirs = new_session(&second);
    assert_ne!(mine, theirs);

    let answer = prompt(&first, &mine);
    assert_eq!(answer["stopReason"], "end_turn");
    let answer = prompt(&second, &theirs);
    assert_eq!(answer["stopReason"], "end_turn");
    assert_eq!(first.reply_text(), "one");
    assert_eq!(second.reply_text(), "two");

    let lines = server.lines();
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("connect ")).count(),
        2,
        "{lines:?}"
    );
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("attach ")).count(),
        2,
        "{lines:?}"
    );

    drop(first);
    drop(second);
    let deadline = std::time::Instant::now() + WAIT;
    let lines = loop {
        let lines = server.lines();
        if lines.iter().any(|l| l.starts_with("disconnect ")) {
            break lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no disconnect line: {lines:?}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("disconnect ") && l.contains("connection 0")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains(server.token.as_str())),
        "the token must never be logged: {lines:?}"
    );
    server.stop();
}

#[test]
fn non_upgrade_acp_traffic_gets_405_and_other_paths_get_404() {
    let server = spawn_server(vec![]);

    let (_, post) = request_with_token(server.addr, "POST /acp", &server.token);
    assert!(post.starts_with("HTTP/1.1 405 "), "{post}");
    assert!(post.contains("WebSocket"), "{post}");

    let (_, get_acp) = request_with_token(server.addr, "GET /acp", &server.token);
    assert!(get_acp.starts_with("HTTP/1.1 405 "), "{get_acp}");

    let (_, delete) = request_with_token(server.addr, "DELETE /acp", &server.token);
    assert!(delete.starts_with("HTTP/1.1 405 "), "{delete}");

    let (_, root) = request_with_token(server.addr, "GET /", &server.token);
    assert!(root.starts_with("HTTP/1.1 404 "), "{root}");

    server.stop();
}

#[test]
fn wrong_token_gets_401_and_is_never_logged() {
    let server = spawn_server(vec![]);
    let presented = "de".repeat(32);
    let host = format!("Host: {}\r\n", server.addr);

    let (local, wrong) = http_request(
        server.addr,
        &format!("GET /acp HTTP/1.1\r\n{host}Authorization: Bearer {presented}\r\n\r\n"),
    );
    assert!(wrong.starts_with("HTTP/1.1 401 "), "{wrong}");

    let (_, missing) = http_request(server.addr, &format!("GET /acp HTTP/1.1\r\n{host}\r\n"));
    assert!(missing.starts_with("HTTP/1.1 401 "), "{missing}");

    let lines = server.lines();
    let peer = local.to_string();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("refuse ") && l.contains(peer.as_str())),
        "the refusal must name the peer address: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains(&presented)),
        "the presented value must never be logged: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains(server.token.as_str())),
        "the token must never be logged: {lines:?}"
    );
    server.stop();
}

#[test]
fn the_seventeenth_concurrent_connection_gets_503() {
    let server = spawn_server(vec![]);
    let mut held = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        let (stream, reply) = open_upgrade(server.addr, &server.token);
        assert!(reply.starts_with("HTTP/1.1 101 "), "{reply}");
        held.push(stream);
    }

    let (_, reply) = open_upgrade(server.addr, &server.token);
    assert!(reply.starts_with("HTTP/1.1 503 "), "{reply}");

    let lines = server.lines();
    assert!(lines.iter().any(|l| l.contains("(503)")), "{lines:?}");
    server.stop();
}

#[test]
fn non_loopback_hosts_are_warned_about_in_plain_text() {
    for (host, warned) in [
        ("127.0.0.1", false),
        ("::1", false),
        ("localhost", false),
        ("0.0.0.0", true),
        ("192.168.1.10", true),
        ("box.example.com", true),
    ] {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&lines);
        let log: Log = Arc::new(move |line| sink.lock().unwrap().push(line.to_owned()));
        warn_non_loopback(host, &log);
        let printed = lines.lock().unwrap().join("\n");
        assert_eq!(printed.contains("no TLS"), warned, "{host}: {printed}");
    }
}
