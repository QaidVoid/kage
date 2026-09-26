//! Interruptible TCP transport for cancellable provider requests.
//!
//! ureq 3.x offers no way to abort an in-flight request: the socket
//! lives inside its pooled `TcpTransport` behind a private field, so
//! nothing outside can wake a thread blocked in a read. The
//! per-provider streams contain the blocking by running reads on worker
//! threads (see [`crate::cancelable`]), but cancelling a turn still
//! left the worker blocked until the 600s idle deadline expired,
//! holding a thread and a live LLM connection for minutes per cancel.
//!
//! This module dials the TCP socket itself, so kage holds a
//! [`TcpStream::try_clone`] handle for every connection.
//! [`KillRegistry::shutdown_all`] shuts those handles down, which wakes
//! the blocked read with a connection error, lets the worker exit, and
//! closes the connection instead of draining it to the deadline.
//!
//! The agent built on this transport is per-request (see
//! [`crate::http`]). Connection-pool reuse hands out a socket without
//! passing through any connector, so a reused socket could never be
//! registered for shutdown; dialing fresh per request means every
//! socket kage opens is covered. HTTP CONNECT proxies keep working by
//! delegating the tunnel hop to ureq's `ConnectProxyConnector`, which
//! re-enters this connector to dial the proxy itself, so the tunneled
//! socket is registered too. SOCKS proxies cannot be registered this
//! way and are rejected loudly rather than silently bypassed.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kage_core::sync::lock;
use ureq::ProxyProtocol;
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout,
    RustlsConnector, Transport,
};
use ureq::{Error, Timeout};

/// Shutdown handles for every socket dialed on behalf of one request.
///
/// The connector registers each socket it opens; cancelling the request
/// shuts them down so a worker blocked in a read wakes at once. Held
/// behind [`Arc`] by `send` and the cancelable stream; dropped when the
/// request's stream is dropped.
#[derive(Debug, Default)]
pub struct KillRegistry {
    sockets: Mutex<Vec<TcpStream>>,
}

impl KillRegistry {
    /// An empty registry.
    ///
    /// HTTP requests create their own; this is for streams with no
    /// sockets to tear down, where cancel is purely cooperative.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep a handle that can shut `stream` down from another thread.
    ///
    /// A `try_clone` failure only means cancel degrades to the idle
    /// timeout for this socket, so it is ignored.
    fn register(&self, stream: &TcpStream) {
        if let Ok(dup) = stream.try_clone() {
            lock(&self.sockets).push(dup);
        }
    }

    /// Shut every registered socket down, in both directions.
    ///
    /// Idempotent; a socket that already served its request shuts down
    /// harmlessly.
    pub(crate) fn shutdown_all(&self) {
        let mut sockets = lock(&self.sockets);
        for socket in sockets.drain(..) {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

/// Connector that dials TCP directly so the socket can be interrupted.
///
/// Output is boxed to satisfy `Agent::with_parts`, which requires
/// `Out = Box<dyn Transport>`.
#[derive(Debug)]
pub(crate) struct InterruptibleConnector {
    registry: Arc<KillRegistry>,
}

impl InterruptibleConnector {
    pub(crate) fn new(registry: Arc<KillRegistry>) -> Self {
        Self { registry }
    }

    /// Dial one target (or proxy) address and register the socket.
    fn dial(&self, details: &ConnectionDetails<'_>) -> Result<InterruptibleTcpTransport, Error> {
        let budget: Option<Duration> = details.timeout.not_zero().map(|d| *d);
        let started = Instant::now();
        let total = details.addrs.len();
        let mut last: Option<Error> = None;
        for (index, addr) in details.addrs.iter().enumerate() {
            let attempt = match budget {
                None => None,
                Some(total_budget) => {
                    let Some(remaining) = total_budget.checked_sub(started.elapsed()) else {
                        break;
                    };
                    // Share the budget evenly across the remaining
                    // addresses, with a floor so each attempt is
                    // meaningful (same floor ureq uses).
                    let spread = u32::try_from(total - index).unwrap_or(u32::MAX);
                    Some((remaining / spread).max(MIN_PER_ADDRESS_TIMEOUT))
                }
            };
            match Self::connect_one(*addr, attempt, details.config.no_delay()) {
                Ok(stream) => {
                    self.registry.register(&stream);
                    let buffers = LazyBuffers::new(
                        details.config.input_buffer_size(),
                        details.config.output_buffer_size(),
                    );
                    return Ok(InterruptibleTcpTransport {
                        stream,
                        buffers,
                        timeout_write: None,
                        timeout_read: None,
                    });
                }
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or(Error::ConnectionFailed))
    }

    /// Open one socket, honoring the per-attempt connect budget.
    fn connect_one(
        addr: SocketAddr,
        timeout: Option<Duration>,
        no_delay: bool,
    ) -> Result<TcpStream, Error> {
        let stream = match timeout {
            Some(attempt) => TcpStream::connect_timeout(&addr, attempt),
            None => TcpStream::connect(addr),
        }
        .map_err(|e| {
            if e.kind() == io::ErrorKind::TimedOut {
                Error::Timeout(Timeout::Connect)
            } else {
                Error::Io(e)
            }
        })?;
        if no_delay {
            stream.set_nodelay(true).map_err(Error::Io)?;
        }
        Ok(stream)
    }
}

/// Floor for one address's share of the connect budget.
const MIN_PER_ADDRESS_TIMEOUT: Duration = Duration::from_millis(10);

impl Connector<()> for InterruptibleConnector {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        _chained: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        // A SOCKS proxy would be dialed by ureq's own connector, out of
        // reach of the kill registry. Fail instead of silently bypassing
        // the proxy. (The socks-proxy feature is not enabled, so this is
        // an unlikely configuration.)
        if let Some(proxy) = details.config.proxy()
            && matches!(
                proxy.protocol(),
                ProxyProtocol::Socks4
                    | ProxyProtocol::Socks4A
                    | ProxyProtocol::Socks5
                    | ProxyProtocol::Socks5h
            )
        {
            return Err(Error::Other(
                "SOCKS proxies are not supported for cancelable provider connections".into(),
            ));
        }

        // Run ureq's CONNECT-proxy hop first. With a proxy configured it
        // dials the proxy by re-entering this connector through
        // `run_connector`, so the tunneled socket is registered; without
        // one it reports "nothing to do" and we dial the target directly.
        let proxy = ConnectProxyConnector::default();
        let tcp: Box<dyn Transport> = match proxy.connect(details, None::<()>)? {
            Some(Either::B(tunneled)) => tunneled,
            _ => Box::new(self.dial(details)?),
        };

        // TLS stays ureq's: its rustls connector wraps whatever
        // transport it is given, ours included.
        let tls = RustlsConnector::default();
        Ok(Some(match tls.connect(details, Some(tcp))? {
            Some(Either::A(tcp)) => Box::new(tcp),
            Some(Either::B(tls)) => Box::new(tls),
            None => return Err(Error::ConnectionFailed),
        }))
    }
}

/// TCP transport identical in behavior to ureq's own, minus pooling.
///
/// Modeled on ureq 3.3's `TcpTransport` (which keeps its socket
/// private): the same per-phase socket timeouts and the same probe for
/// pool liveness. Cancel support comes from the connector registering
/// the socket before this transport is ever handed out.
pub(crate) struct InterruptibleTcpTransport {
    stream: TcpStream,
    buffers: LazyBuffers,
    timeout_write: Option<Duration>,
    timeout_read: Option<Duration>,
}

impl Transport for InterruptibleTcpTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        apply_timeout(
            timeout,
            &mut self.timeout_write,
            &self.stream,
            TcpStream::set_write_timeout,
        )?;

        let output = &self.buffers.output()[..amount];
        match self.stream.write_all(output) {
            Ok(()) => Ok(()),
            Err(e) if is_timeout(&e) => Err(Error::Timeout(timeout.reason)),
            Err(e) => Err(e.into()),
        }
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        apply_timeout(
            timeout,
            &mut self.timeout_read,
            &self.stream,
            TcpStream::set_read_timeout,
        )?;

        let input = self.buffers.input_append_buf();
        let amount = match self.stream.read(input) {
            Ok(amount) => amount,
            Err(e) if is_timeout(&e) => return Err(Error::Timeout(timeout.reason)),
            Err(e) => return Err(e.into()),
        };
        self.buffers.input_appended(amount);

        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        probe_tcp_stream(&mut self.stream).unwrap_or(false)
    }
}

/// A blocking socket read or write that hit its configured timeout.
///
/// On Linux a `SO_RCVTIMEO`/`SO_SNDTIMEO` expiry surfaces as
/// `WouldBlock`, which ureq normalizes to `TimedOut` internally; this
/// accepts both.
fn is_timeout(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Only reset the socket timeout when the requested value changed, to
/// avoid a syscall per operation (same scheme as ureq's transport).
fn apply_timeout(
    timeout: NextTimeout,
    previous: &mut Option<Duration>,
    stream: &TcpStream,
    set: impl Fn(&TcpStream, Option<Duration>) -> io::Result<()>,
) -> io::Result<()> {
    let wanted: Option<Duration> = timeout.not_zero().map(|d| *d);
    if wanted != *previous {
        set(stream, wanted)?;
        *previous = wanted;
    }
    Ok(())
}

/// Peek non-blockingly: `WouldBlock` means the connection is idle and
/// alive, anything else means it is not usable (same probe as ureq's).
fn probe_tcp_stream(stream: &mut TcpStream) -> Result<bool, Error> {
    stream.set_nonblocking(true)?;
    let mut buf = [0];
    match stream.read(&mut buf) {
        // WouldBlock is the healthy answer: idle and alive.
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        _ => return Ok(false),
    }
    stream.set_nonblocking(false)?;
    Ok(true)
}

impl fmt::Debug for InterruptibleTcpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InterruptibleTcpTransport")
            .field("addr", &self.stream.peer_addr().ok())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use kage_core::CancelFlag;

    use super::*;
    use crate::cancelable::make_cancelable;
    use crate::{EventStream, ProviderError, ProviderEvent};

    /// A reader blocked on a socket registered in the registry must
    /// return promptly once `shutdown_all` runs; this is the mechanism
    /// that frees a cancelled turn's connection.
    #[test]
    fn shutdown_all_unblocks_a_reader_blocked_on_a_registered_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let registry = Arc::new(KillRegistry::new());
        let handles = Arc::clone(&registry);

        let reader = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).expect("connect");
            handles.register(&stream);
            let mut buf = [0u8; 64];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => return "eof",
                    Ok(_) => {}
                    Err(_) => return "error",
                }
            }
        });

        // Give the reader time to block, then tear the socket down.
        std::thread::sleep(Duration::from_millis(100));
        registry.shutdown_all();

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(reader.join()).expect("send"));
        let outcome = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader must not stay blocked after shutdown");
        assert!(
            matches!(outcome, Ok("eof" | "error")),
            "unexpected reader outcome: {outcome:?}"
        );
    }

    /// A request through an agent built on this connector must fail
    /// promptly when the registry shuts the socket down, even though
    /// the server never answers.
    #[test]
    fn shutdown_unblocks_a_request_waiting_for_a_response() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let registry = Arc::new(KillRegistry::new());
        let agent = agent_for_test(Arc::clone(&registry));

        let waiter = std::thread::spawn(move || {
            agent
                .get(format!("http://{addr}/v1/stall"))
                .call()
                .map(|_| ())
        });

        // The dial goes through the connector; once the server sees the
        // connection, shut the socket down while ureq waits for a
        // response that will never come. The server socket is kept open
        // so only the shutdown can end the wait.
        let (server, _peer) = listener.accept().expect("accept");
        std::thread::sleep(Duration::from_millis(100));
        registry.shutdown_all();

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(waiter.join()).expect("send"));
        let outcome = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("request must not stay blocked after shutdown");
        let call = outcome.expect("request thread must not panic");
        assert!(call.is_err(), "expected a transport error, got {call:?}");
        drop(server);
    }

    /// Build an agent over the interruptible connector with the test's
    /// timeouts (the real one lives in `crate::http`).
    fn agent_for_test(registry: Arc<KillRegistry>) -> ureq::Agent {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_send_request(Some(Duration::from_secs(5)))
            .timeout_recv_body(Some(Duration::from_secs(5)))
            .build();
        ureq::Agent::with_parts(
            config,
            InterruptibleConnector::new(registry),
            ureq::unversioned::resolver::DefaultResolver::default(),
        )
    }

    /// A stream that yields `MessageStart`, then drains the response
    /// body as text deltas until the body errors or ends.
    struct BodyEvents {
        reader: Box<dyn Read + Send>,
        started: bool,
    }

    impl Iterator for BodyEvents {
        type Item = Result<ProviderEvent, ProviderError>;

        fn next(&mut self) -> Option<Self::Item> {
            if !self.started {
                self.started = true;
                return Some(Ok(ProviderEvent::MessageStart));
            }
            let mut buf = [0u8; 64];
            match self.reader.read(&mut buf) {
                Ok(0) => None,
                Ok(n) => Some(Ok(ProviderEvent::TextDelta {
                    delta: String::from_utf8_lossy(&buf[..n]).into_owned(),
                })),
                Err(e) => Some(Err(ProviderError::Transport(e.to_string()))),
            }
        }
    }

    /// The M-N1 guarantee end to end: a turn cancelled mid-stream must
    /// close the connection promptly. The server answers with headers
    /// plus a partial body, then goes quiet; the cancelled stream
    /// returns `Cancelled` at once and the server observes the
    /// disconnect within seconds. Without teardown the worker would
    /// hold the connection until the 600s idle deadline.
    #[test]
    fn cancel_tears_down_a_streaming_connection() {
        use std::io::Write as _;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        let cancel = CancelFlag::new();

        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            // Consume the request so the later read observes the
            // disconnect, not leftover request bytes.
            let mut seen = Vec::new();
            let mut chunk = [0u8; 256];
            loop {
                let n = conn.read(&mut chunk).expect("read request");
                seen.extend_from_slice(&chunk[..n]);
                if n == 0 || seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\ndata")
                .expect("write headers");
            conn.flush().expect("flush");
            let mut buf = [0u8; 16];
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => true,
                Ok(_) => false,
            }
        });

        let url = format!("http://{addr}/v1/chat");
        let (response, kill) = crate::http::send(&cancel, url, |agent, url| agent.get(url).call())
            .expect("request should reach the response phase");
        assert_eq!(response.status().as_u16(), 200);

        let reader: Box<dyn Read + Send> = Box::new(response.into_body().into_reader());
        let inner: EventStream = Box::new(BodyEvents {
            reader,
            started: false,
        });
        let mut stream = make_cancelable(inner, cancel.clone(), kill);

        // Let the worker drain the partial body and block for more.
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        cancel.cancel();
        let item = stream.next().expect("a final item");
        assert!(
            matches!(item, Err(ProviderError::Cancelled)),
            "expected Cancelled, got {item:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancel must be observed immediately"
        );
        drop(stream);

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(server.join()).expect("send"));
        let saw_disconnect = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server must not wait out the idle deadline");
        assert!(
            saw_disconnect.expect("server thread"),
            "server must see the disconnect"
        );
    }
}
