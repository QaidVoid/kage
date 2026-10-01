//! The WebSocket transport to `kage serve`.
//!
//! The token rides the `kage.<token>` `Sec-WebSocket-Protocol` entry,
//! never a query string. The `Acp-Connection-Id` of the 101 is read
//! and kept for logs and later epics. A dropped link flips the
//! transport to [`State::Reconnecting`] and retries on a backoff
//! ladder from one second to a thirty second cap; a refusal the
//! engine answered with an HTTP status that will not change (a wrong
//! token, a wrong path) stops the transport instead.

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kage_client::Frame;
use tungstenite::client::IntoClientRequest;
use tungstenite::error::UrlError;
use tungstenite::http::{HeaderValue, header::SEC_WEBSOCKET_PROTOCOL};
use tungstenite::protocol::WebSocket;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error as WsError, Message};

use super::{Backoff, Event, EventSender, Link, State, Transport};

/// The prefix of the subprotocol entry kage's own client sends.
const TOKEN_SUBPROTOCOL_PREFIX: &str = "kage.";

/// How long the connection thread blocks in a read before it looks at
/// the outgoing queue again.
const READ_POLL: Duration = Duration::from_millis(50);

/// What one failed dial leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    /// The endpoint said no for good.
    Refused(String),
    /// The endpoint is not there yet; wait `delay` and try again.
    Retry(Duration),
}

/// Everything the connection thread and the handle share.
#[derive(Default)]
struct Shared {
    /// The live link's outgoing queue, absent between links.
    outgoing: Mutex<Option<std::sync::mpsc::Sender<Frame>>>,
    /// Set by [`Transport::close`] to end every retry.
    closed: AtomicBool,
    /// A duplicate of the live socket, shut down to unblock the
    /// reader when the transport closes.
    socket: Mutex<Option<TcpStream>>,
}

/// The WebSocket transport to one `kage serve` endpoint.
#[derive(Clone)]
pub struct WsTransport {
    url: String,
    token: String,
    shared: Arc<Shared>,
    /// The `Acp-Connection-Id` the last successful 101 carried.
    connection_id: Arc<Mutex<Option<String>>>,
}

impl WsTransport {
    /// A transport for the `ws://` endpoint at `url`, authenticating
    /// with `token` through the subprotocol.
    #[must_use]
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            shared: Arc::default(),
            connection_id: Arc::default(),
        }
    }

    /// The connection thread: dial, serve one link, repeat.
    fn run(self, events: EventSender) {
        let mut backoff = Backoff::new();
        let mut attempts = 0u32;
        loop {
            if self.closed() {
                let _ = events.send_blocking(Event::State(State::Closed));
                return;
            }
            let _ = events.send_blocking(Event::State(State::Connecting));
            match dial(&self.url, &self.token) {
                Err(error) => match classify(&error, &mut backoff) {
                    Failure::Refused(why) => {
                        let _ = events.send_blocking(Event::State(State::Refused(why)));
                        let _ = events.send_blocking(Event::State(State::Closed));
                        return;
                    }
                    Failure::Retry(delay) => {
                        attempts += 1;
                        let _ = events.send_blocking(Event::State(State::Reconnecting {
                            attempt: attempts,
                            delay,
                        }));
                        if self.wait_out(&events, delay) {
                            return;
                        }
                    }
                },
                Ok((socket, response)) => {
                    if let Some(id) = response
                        .headers()
                        .get("acp-connection-id")
                        .and_then(|value| value.to_str().ok())
                    {
                        *self.connection_id.lock().unwrap() = Some(id.to_owned());
                    }
                    backoff.reset();
                    attempts = 0;
                    self.hold_socket(&socket);
                    let _ = events.send_blocking(Event::State(State::Connected));
                    if self.serve_link(socket, &events) {
                        let _ = events.send_blocking(Event::State(State::Closed));
                        return;
                    }
                    let delay = backoff.retry_delay();
                    attempts += 1;
                    let _ = events.send_blocking(Event::State(State::Reconnecting {
                        attempt: attempts,
                        delay,
                    }));
                    if self.wait_out(&events, delay) {
                        return;
                    }
                }
            }
        }
    }

    /// Whether the transport was closed for good.
    fn closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// Waits out `delay`, polling the closed flag. Returns true when
    /// the transport closed while waiting.
    fn wait_out(&self, events: &EventSender, delay: Duration) -> bool {
        let end = Instant::now() + delay;
        loop {
            if self.closed() {
                let _ = events.send_blocking(Event::State(State::Closed));
                return true;
            }
            let now = Instant::now();
            if now >= end {
                return false;
            }
            thread::sleep((end - now).min(READ_POLL));
        }
    }

    /// Keeps a duplicate of the live socket so [`Transport::close`]
    /// can unblock the reader.
    fn hold_socket(&self, socket: &WebSocket<MaybeTlsStream<TcpStream>>) {
        let duplicate = match socket.get_ref() {
            MaybeTlsStream::Plain(stream) => stream.try_clone().ok(),
            _ => None,
        };
        *self.shared.socket.lock().unwrap() = duplicate;
    }

    /// Serves one live link until it drops or the transport closes.
    /// Returns true when no further link may start: the transport was
    /// closed, or the shell that listened is gone.
    fn serve_link(
        &self,
        mut socket: WebSocket<MaybeTlsStream<TcpStream>>,
        events: &EventSender,
    ) -> bool {
        if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
            let _ = stream.set_read_timeout(Some(READ_POLL));
        }
        let (writer, outgoing) = std::sync::mpsc::channel::<Frame>();
        *self.shared.outgoing.lock().unwrap() = Some(writer);
        let mut live = true;
        while live {
            while let Ok(frame) = outgoing.try_recv() {
                let line = serde_json::to_string(&frame.to_value()).expect("frame serializes");
                if socket.send(Message::text(line)).is_err() {
                    live = false;
                    break;
                }
            }
            if !live {
                break;
            }
            match socket.read() {
                Ok(Message::Text(text)) => {
                    let value: serde_json::Value =
                        serde_json::from_str(text.as_str()).unwrap_or(serde_json::Value::Null);
                    if let Some(frame) = Frame::parse(&value)
                        && events.send_blocking(Event::Frame(frame)).is_err()
                    {
                        live = false;
                    }
                }
                Ok(Message::Close(_)) | Err(WsError::ConnectionClosed) => live = false,
                Ok(_) => {}
                Err(WsError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => live = false,
            }
        }
        *self.shared.outgoing.lock().unwrap() = None;
        *self.shared.socket.lock().unwrap() = None;
        self.closed()
    }
}

impl Transport for WsTransport {
    fn link(&self) -> Link {
        Link::serve(&self.url)
    }

    fn start(&mut self, events: EventSender) {
        let transport = self.clone();
        thread::Builder::new()
            .name("kage-ws".to_owned())
            .spawn(move || transport.run(events))
            .expect("connection thread spawns");
    }

    fn send(&self, frame: Frame) {
        let outgoing = self.shared.outgoing.lock().unwrap().clone();
        if let Some(outgoing) = outgoing {
            let _ = outgoing.send(frame);
        }
    }

    fn close(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        *self.shared.outgoing.lock().unwrap() = None;
        if let Some(socket) = self.shared.socket.lock().unwrap().take() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }

    fn connection_id(&self) -> Option<String> {
        self.connection_id.lock().unwrap().clone()
    }
}

impl Drop for WsTransport {
    fn drop(&mut self) {
        self.close();
    }
}

/// Dials `url` with the token as the `kage.<token>` subprotocol.
fn dial(
    url: &str,
    token: &str,
) -> tungstenite::Result<(
    WebSocket<MaybeTlsStream<TcpStream>>,
    tungstenite::handshake::client::Response,
)> {
    let mut request = url.into_client_request()?;
    let entry = format!("{TOKEN_SUBPROTOCOL_PREFIX}{token}");
    let value = HeaderValue::from_str(&entry)
        .map_err(|_| WsError::Utf8("token carries characters a header cannot hold".into()))?;
    request.headers_mut().insert(SEC_WEBSOCKET_PROTOCOL, value);
    tungstenite::connect(request)
}

/// Sorts one failed dial into a refusal the user must fix or a retry
/// worth waiting for.
fn classify(error: &WsError, backoff: &mut Backoff) -> Failure {
    match error {
        WsError::Http(response) => match response.status().as_u16() {
            503 => Failure::Retry(backoff.retry_delay()),
            status => Failure::Refused(format!(
                "the endpoint answered HTTP {status}; check the URL and the token"
            )),
        },
        WsError::Url(UrlError::TlsFeatureNotEnabled) => {
            Failure::Refused("wss:// needs TLS, which this build leaves out; use ws://".to_owned())
        }
        WsError::Url(UrlError::UnableToConnect(_)) => Failure::Retry(backoff.retry_delay()),
        WsError::Url(error) => Failure::Refused(format!("bad endpoint: {error}")),
        WsError::Utf8(why) => Failure::Refused(format!("the handshake was rejected: {why}")),
        WsError::HttpFormat(error) => {
            Failure::Refused(format!("the handshake was rejected: {error}"))
        }
        WsError::Protocol(error) => Failure::Refused(format!("handshake failed: {error}")),
        WsError::Io(_) => Failure::Retry(backoff.retry_delay()),
        WsError::Tls(_) => Failure::Retry(backoff.retry_delay()),
        WsError::Capacity(_)
        | WsError::WriteBufferFull(_)
        | WsError::AttackAttempt
        | WsError::ConnectionClosed
        | WsError::AlreadyClosed => Failure::Retry(backoff.retry_delay()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use kage_client::{Client, Frame};
    use tungstenite::handshake::derive_accept_key;
    use tungstenite::protocol::Role;

    use super::{TOKEN_SUBPROTOCOL_PREFIX, WsTransport};
    use crate::transport::{Event, State, Transport};

    const TOKEN: &str = "sekret";

    /// The next state event, skipping frames, with a deadline.
    fn next_state(rx: &async_channel::Receiver<Event>) -> State {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "no state event arrived");
            match rx.try_recv() {
                Ok(Event::State(state)) => return state,
                Ok(Event::Frame(_)) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(other) => panic!("event stream broke: {other}"),
            }
        }
    }

    /// The next frame event, skipping states, with a deadline.
    fn next_frame(rx: &async_channel::Receiver<Event>) -> Frame {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "no frame arrived");
            match rx.try_recv() {
                Ok(Event::Frame(frame)) => return frame,
                Ok(Event::State(_)) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(other) => panic!("event stream broke: {other}"),
            }
        }
    }

    /// Reads a request head off the fake server's socket.
    fn read_head(stream: &mut std::net::TcpStream) -> String {
        let mut head = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let read = stream.read(&mut chunk).expect("fake server reads");
            assert!(read > 0, "the client hung up mid-head");
            head.extend_from_slice(&chunk[..read]);
            if head.windows(4).any(|window| window == b"\r\n\r\n") {
                return String::from_utf8(head).expect("the head is UTF-8");
            }
        }
    }

    /// Completes the server half of the handshake and returns a
    /// server-side WebSocket for frame traffic.
    fn upgrade(
        mut stream: std::net::TcpStream,
        echo_protocol: bool,
    ) -> tungstenite::WebSocket<std::net::TcpStream> {
        let head = read_head(&mut stream);
        let entry = format!("{TOKEN_SUBPROTOCOL_PREFIX}{TOKEN}");
        assert!(
            head.contains(&format!("Sec-WebSocket-Protocol: {entry}")),
            "the token rides the subprotocol: {head}"
        );
        assert!(
            !head.contains("token="),
            "the token must not ride the URL: {head}"
        );
        let key = head
            .lines()
            .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
            .map(str::trim)
            .expect("the client sends a websocket key");
        let accept = derive_accept_key(key.as_bytes());
        let protocol = echo_protocol.then(|| format!("Sec-WebSocket-Protocol: {entry}\r\n"));
        write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             Acp-Connection-Id: conn-7\r\n\
             {}\
             \r\n",
            protocol.as_deref().unwrap_or("")
        )
        .unwrap();
        stream.flush().unwrap();
        tungstenite::WebSocket::from_raw_socket(stream, Role::Server, None)
    }

    #[test]
    fn a_wrong_token_is_refused_once_and_stays_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (events, rx) = async_channel::unbounded();
        let mut transport = WsTransport::new(format!("ws://{addr}/acp"), "wrong-token");
        transport.start(events);

        let (mut stream, _) = listener.accept().unwrap();
        let _ = read_head(&mut stream);
        write!(
            stream,
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();

        assert_eq!(next_state(&rx), State::Connecting);
        let refused = next_state(&rx);
        let State::Refused(reason) = refused else {
            panic!("expected a refusal, got {refused:?}");
        };
        assert!(reason.contains("401"), "{reason}");
        assert_eq!(next_state(&rx), State::Closed);
    }

    #[test]
    fn frames_move_both_ways_and_the_connection_id_is_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (events, rx) = async_channel::unbounded();
        let mut transport = WsTransport::new(format!("ws://{addr}/acp"), TOKEN);
        transport.start(events);
        assert_eq!(next_state(&rx), State::Connecting);

        let (stream, _) = listener.accept().unwrap();
        let mut server = upgrade(stream, true);
        assert_eq!(next_state(&rx), State::Connected);
        assert_eq!(transport.connection_id().as_deref(), Some("conn-7"));

        let mut client = Client::new();
        client.initialize(Default::default(), None);
        for frame in client.take_outgoing() {
            transport.send(frame);
        }
        let sent = server.read().expect("the initialize frame arrives");
        let value: serde_json::Value = serde_json::from_str(sent.to_text().unwrap()).unwrap();
        client.handle(Frame::parse(&value).expect("the initialize frame parses"));

        server
            .send(tungstenite::Message::text(
                serde_json::to_string(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "protocolVersion": 1,
                        "agentCapabilities": {"steer": true},
                        "agentInfo": {"name": "kage", "version": "0.1.0"},
                    },
                }))
                .unwrap(),
            ))
            .unwrap();
        client.handle(next_frame(&rx));
        assert_eq!(
            client
                .state()
                .agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("kage"),
            "a server frame landed in the client"
        );

        transport.close();
        assert_eq!(next_state(&rx), State::Closed);
    }

    #[test]
    fn a_dropped_link_reconnects_and_reports_the_backoff() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (events, rx) = async_channel::unbounded();
        let mut transport = WsTransport::new(format!("ws://{addr}/acp"), TOKEN);
        transport.start(events);
        assert_eq!(next_state(&rx), State::Connecting);

        let (stream, _) = listener.accept().unwrap();
        let server = upgrade(stream, true);
        assert_eq!(next_state(&rx), State::Connected);

        drop(server);
        assert_eq!(
            next_state(&rx),
            State::Reconnecting {
                attempt: 1,
                delay: Duration::from_secs(1)
            },
            "the first retry waits one second"
        );
        assert_eq!(next_state(&rx), State::Connecting);

        let (stream, _) = listener.accept().unwrap();
        let _server = upgrade(stream, true);
        assert_eq!(next_state(&rx), State::Connected);
        assert_eq!(transport.connection_id().as_deref(), Some("conn-7"));

        transport.close();
        assert_eq!(next_state(&rx), State::Closed);
    }

    #[test]
    fn a_missing_server_retries_with_growing_delays() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let (events, rx) = async_channel::unbounded();
        let mut transport = WsTransport::new(format!("ws://{addr}/acp"), TOKEN);
        transport.start(events);

        assert_eq!(next_state(&rx), State::Connecting);
        assert_eq!(
            next_state(&rx),
            State::Reconnecting {
                attempt: 1,
                delay: Duration::from_secs(1)
            }
        );
        assert_eq!(next_state(&rx), State::Connecting);
        assert_eq!(
            next_state(&rx),
            State::Reconnecting {
                attempt: 2,
                delay: Duration::from_secs(2)
            }
        );
        transport.close();
        assert_eq!(next_state(&rx), State::Closed);
    }
}
