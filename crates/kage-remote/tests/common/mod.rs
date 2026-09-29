//! Helpers shared by the transport tests: a fresh token, a listener on
//! an ephemeral port, the accept flow the serve command wires, raw
//! upgrade requests, and a WebSocket client.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kage_remote::head::{self, Auth, HeadError};
use kage_remote::pipe::{self, Outgoing};
use kage_remote::token::Token;

use tungstenite::WebSocket;
use tungstenite::http::StatusCode;

/// The connection id the test accept flow puts in the 101.
pub const CONNECTION_ID: &str = "test-connection-1";

/// The RFC 6455 sample key and the accept value derived from it, used
/// to check the 101 independently of tungstenite.
pub const SAMPLE_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
pub const SAMPLE_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// Every wait in these tests is bounded by this.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// A freshly generated token; the temp dir keeps its file alive.
pub fn fresh_token() -> (tempfile::TempDir, Arc<Token>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote-token");
    let token = Token::load_or_create(&path).unwrap();
    (dir, Arc::new(token))
}

/// Binds a listener on `127.0.0.1:0`.
pub fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// The accept flow the serve command wires up: read the head,
/// authorize, then upgrade or answer 401 and 431.
pub fn accept(stream: &mut TcpStream, token: &Token) -> Accepted {
    let rejected = |stream: &mut TcpStream, status: u16| {
        let reason = match status {
            431 => "Request Header Fields Too Large",
            _ => "Unauthorized",
        };
        let body = match status {
            431 => "request head is too large\n",
            _ => "a valid token is required\n",
        };
        head::respond(stream, status, reason, body).ok();
        // Flush the unread inbound so the close carries the reply
        // instead of a reset.
        let _ = stream.shutdown(Shutdown::Write);
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        let mut sink = [0u8; 4096];
        while let Ok(n) = stream.read(&mut sink) {
            if n == 0 {
                break;
            }
        }
        Accepted::Rejected(status)
    };
    match head::read_head_with_timeout(stream, TIMEOUT) {
        Err(HeadError::TooLarge) => rejected(stream, 431),
        Err(_) => Accepted::Rejected(0),
        Ok(head) => match head::authorize(&head, token) {
            None => rejected(stream, 401),
            Some(auth) => {
                let subprotocol = match &auth {
                    Auth::Subprotocol(entry) => Some(entry.clone()),
                    _ => None,
                };
                match pipe::upgrade(&mut *stream, head, CONNECTION_ID, subprotocol.as_deref()) {
                    Ok((reader, writer)) => Accepted::Upgraded(reader, writer),
                    Err(_) => Accepted::Rejected(0),
                }
            }
        },
    }
}

/// The outcome of [`accept`].
pub enum Accepted {
    Upgraded(BufReader<std::io::PipeReader>, Outgoing),
    Rejected(u16),
}

/// Serves one connection, handing an upgraded pair to `run` on its own
/// thread, and returns the bound address.
pub fn serve_upgraded(
    token: Arc<Token>,
    run: impl FnOnce(BufReader<std::io::PipeReader>, Outgoing) + Send + 'static,
) -> SocketAddr {
    let (listener, addr) = listener();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        if let Accepted::Upgraded(reader, writer) = accept(&mut stream, &token) {
            run(reader, writer);
        }
    });
    addr
}

/// Sends a `GET` upgrade request with the fixed sample key plus extra
/// headers, and returns the raw response head.
pub fn upgrade_response(addr: SocketAddr, target: &str, headers: &[(&str, &str)]) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut request = format!(
        "GET {target} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {SAMPLE_KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n"
    );
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut reply = String::new();
    let mut buffered = BufReader::new(stream);
    loop {
        let mut line = String::new();
        let read = buffered.read_line(&mut line).unwrap();
        let done = read == 0 || line == "\r\n";
        reply.push_str(&line);
        if done {
            break;
        }
    }
    reply
}

/// Connects a real WebSocket client with the token in the query.
pub fn connect_client(
    addr: SocketAddr,
    token: &Token,
) -> WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>> {
    let uri = format!("ws://{addr}/acp?token={}", token.as_str());
    let (client, response) = tungstenite::connect(uri).unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    if let tungstenite::stream::MaybeTlsStream::Plain(sock) = client.get_ref() {
        sock.set_read_timeout(Some(TIMEOUT)).unwrap();
    }
    client
}
