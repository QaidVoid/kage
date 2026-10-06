//! Parsing and authorizing the HTTP upgrade request head.
//!
//! [`read_head`] reads one request head straight off the socket with a
//! size cap and a deadline, keeping any bytes that follow the blank
//! line so they can seed the frame decoder after the upgrade.
//! [`authorize`] then checks the token in a fixed order: the
//! `Authorization: Bearer` header, then every token-bearing
//! `Sec-WebSocket-Protocol` entry, then the `token` query parameter.
//! [`respond`] writes plain HTTP replies for every rejection.

use std::fmt;
use std::io::{self, Read as _, Write as _};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::token::Token;
use crate::{HEAD_CAP, HEAD_TIMEOUT};

/// Prefix of the `Sec-WebSocket-Protocol` entry kage's own client
/// sends.
pub const TOKEN_SUBPROTOCOL_PREFIX: &str = "kage.";

/// Prefix of the `Sec-WebSocket-Protocol` entry the ACP browser UI
/// sends, where request headers are unavailable.
pub const UI_SUBPROTOCOL_PREFIX: &str = "acp.";

/// An upgrade request head: request line, headers, and any frame bytes
/// read past the blank line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// Request method, for example `GET`.
    pub method: String,
    /// Request path without the query string, for example `/acp`.
    pub path: String,
    /// Query pairs in arrival order. Values are not percent-decoded.
    pub query: Vec<(String, String)>,
    /// Headers as `(lowercase name, value)` pairs in arrival order.
    pub headers: Vec<(String, String)>,
    /// Bytes read past the end of the head.
    pub leftover: Vec<u8>,
}

impl Head {
    /// First value of the named header, matched case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Every value of the named header, matched case-insensitively.
    pub fn header_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Value of the named query parameter, if present.
    #[must_use]
    pub fn query_value(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Why a request head could not be read.
#[derive(Debug)]
pub enum HeadError {
    /// The head passed [`HEAD_CAP`] bytes without ending.
    TooLarge,
    /// The peer missed the deadline: the per-read timeout fired, or
    /// the whole head did not arrive in budget.
    Timeout(String),
    /// The peer hung up or sent garbage.
    Io(io::Error),
}

impl fmt::Display for HeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("request head exceeds the size cap"),
            Self::Timeout(why) => write!(f, "request head timed out: {why}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HeadError {}

impl From<io::Error> for HeadError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Reads one request head from `stream` with the default
/// [`HEAD_TIMEOUT`].
///
/// # Errors
///
/// Returns [`HeadError::TooLarge`] when the head passes [`HEAD_CAP`]
/// bytes without ending, and [`HeadError::Io`] when the peer hangs up,
/// sends a malformed head, or misses the deadline.
pub fn read_head(stream: &mut TcpStream) -> Result<Head, HeadError> {
    read_head_with_timeout(stream, HEAD_TIMEOUT)
}

/// [`read_head`] with a caller-chosen deadline. `timeout` bounds each
/// read and the whole head: a peer that keeps a read alive by
/// dripping bytes still misses the head's deadline once the first
/// read started it.
///
/// # Errors
///
/// Same as [`read_head`].
pub fn read_head_with_timeout(
    stream: &mut TcpStream,
    timeout: Duration,
) -> Result<Head, HeadError> {
    stream.set_read_timeout(Some(timeout))?;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let start = Instant::now();
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(HeadError::Timeout("a read missed its deadline".to_owned()));
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before the head ended",
            )
            .into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find_head_end(&buf) {
            if end + 4 > HEAD_CAP {
                return Err(HeadError::TooLarge);
            }
            let leftover = buf.split_off(end + 4);
            buf.truncate(end);
            return parse_head(&buf, leftover);
        }
        if buf.len() > HEAD_CAP {
            return Err(HeadError::TooLarge);
        }
        if start.elapsed() >= timeout {
            return Err(HeadError::Timeout(
                "the request head missed its deadline".to_owned(),
            ));
        }
    }
}

/// Writes a plain HTTP reply with a `text/plain` body, for every
/// non-upgrade answer.
///
/// # Errors
///
/// Fails when the reply cannot be written.
pub fn respond(stream: &mut TcpStream, status: u16, reason: &str, body: &str) -> io::Result<()> {
    let reply = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        body.len(),
    );
    stream.write_all(reply.as_bytes())?;
    stream.flush()
}

/// How an authorized request presented the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// The `Authorization: Bearer` header.
    Bearer,
    /// A token-bearing `Sec-WebSocket-Protocol` entry, to be echoed
    /// verbatim in the 101.
    Subprotocol(String),
    /// The `token` query parameter.
    Query,
}

/// Checks the head against `token` in a fixed order and returns how
/// the token was presented.
///
/// The order is the `Authorization: Bearer` header first, then every
/// `Sec-WebSocket-Protocol` entry prefixed with
/// [`TOKEN_SUBPROTOCOL_PREFIX`] or [`UI_SUBPROTOCOL_PREFIX`], then the
/// `token` query parameter. Each candidate is compared in constant
/// time.
///
/// Returns `None` when the request carries no valid token and must be
/// answered with `401`.
#[must_use]
pub fn authorize(head: &Head, token: &Token) -> Option<Auth> {
    for value in head.header_all("authorization") {
        let Some((scheme, presented)) = value.split_once(' ') else {
            continue;
        };
        if scheme.eq_ignore_ascii_case("bearer") && token.matches(presented.trim()) {
            return Some(Auth::Bearer);
        }
    }
    for entry in head
        .header_all("sec-websocket-protocol")
        .flat_map(|value| value.split(','))
    {
        let entry = entry.trim();
        let presented = entry
            .strip_prefix(TOKEN_SUBPROTOCOL_PREFIX)
            .or_else(|| entry.strip_prefix(UI_SUBPROTOCOL_PREFIX));
        if let Some(presented) = presented
            && token.matches(presented)
        {
            return Some(Auth::Subprotocol(entry.to_owned()));
        }
    }
    if let Some(presented) = head.query_value("token")
        && token.matches(presented)
    {
        return Some(Auth::Query);
    }
    None
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_head(head: &[u8], leftover: Vec<u8>) -> Result<Head, HeadError> {
    let text = std::str::from_utf8(head)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let request = lines.next().unwrap_or_default();
    let mut parts = request.split(' ').filter(|part| !part.is_empty());
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty request line"))?
        .to_owned();
    let target = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request line has no target"))?;
    let (path, query) = parse_target(target);
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "header line has no colon").into(),
            );
        };
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(Head {
        method,
        path,
        query,
        headers,
        leftover,
    })
}

fn parse_target(target: &str) -> (String, Vec<(String, String)>) {
    let Some((path, query)) = target.split_once('?') else {
        return (target.to_owned(), Vec::new());
    };
    let pairs = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (name.to_owned(), value.to_owned()),
            None => (pair.to_owned(), String::new()),
        })
        .collect();
    (path.to_owned(), pairs)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    use super::*;
    use crate::token::with_value;

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    fn head_of(raw: &str) -> Head {
        let (mut client, mut server) = pair();
        client.write_all(raw.as_bytes()).unwrap();
        read_head(&mut server).unwrap()
    }

    #[test]
    fn parses_request_line_headers_query_and_leftover() {
        let head = head_of(
            "GET /acp?token=abc&x=%20 HTTP/1.1\r\n\
             HOST: example.test\r\n\
             Sec-WebSocket-Protocol: kage.tok, acp\r\n\
             \r\nFRAME",
        );
        assert_eq!(head.method, "GET");
        assert_eq!(head.path, "/acp");
        assert_eq!(
            head.query,
            vec![
                ("token".to_owned(), "abc".to_owned()),
                ("x".to_owned(), "%20".to_owned())
            ]
        );
        assert_eq!(head.header("host"), Some("example.test"));
        assert_eq!(head.header("HOST"), Some("example.test"));
        assert_eq!(
            head.header_all("sec-websocket-protocol")
                .collect::<Vec<_>>(),
            vec!["kage.tok, acp"]
        );
        assert_eq!(head.leftover, b"FRAME");
    }

    #[test]
    fn target_without_query_has_no_pairs() {
        let head = head_of("GET /acp HTTP/1.1\r\n\r\n");
        assert_eq!(head.path, "/acp");
        assert!(head.query.is_empty());
        assert!(head.leftover.is_empty());
    }

    #[test]
    fn oversized_head_is_too_large() {
        let (mut client, mut server) = pair();
        let pad = "x".repeat(HEAD_CAP * 2);
        let _ = client.write_all(format!("GET /acp HTTP/1.1\r\nX-Pad: {pad}").as_bytes());
        let error = read_head(&mut server).unwrap_err();
        assert!(matches!(error, HeadError::TooLarge), "{error}");
    }

    #[test]
    fn head_at_the_cap_still_parses() {
        let (mut client, mut server) = pair();
        let line = format!(
            "GET /acp HTTP/1.1\r\nX-Pad: {}\r\n\r\n",
            "y".repeat(HEAD_CAP - 40)
        );
        client.write_all(line.as_bytes()).unwrap();
        let head = read_head(&mut server).unwrap();
        assert_eq!(head.path, "/acp");
    }

    #[test]
    fn silent_peer_times_out() {
        let (_client, mut server) = pair();
        let error = read_head_with_timeout(&mut server, Duration::from_millis(300)).unwrap_err();
        assert!(
            matches!(error, HeadError::Timeout(_)),
            "expected a timeout, got {error}"
        );
    }

    #[test]
    fn a_dripping_peer_misses_the_whole_head_deadline() {
        let (client, mut server) = pair();
        let budget = Duration::from_millis(900);
        let gap = Duration::from_millis(500);
        // One byte per read window keeps the per-read timeout quiet
        // while the whole head passes its budget.
        let writer = std::thread::spawn(move || {
            let mut client = client;
            client.write_all(b"G").unwrap();
            std::thread::sleep(gap);
            client.write_all(b"E").unwrap();
            std::thread::sleep(gap);
            client.write_all(b"T").unwrap();
        });
        let error = read_head_with_timeout(&mut server, budget).unwrap_err();
        writer.join().unwrap();
        match error {
            HeadError::Timeout(why) => assert!(
                why.contains("deadline"),
                "the whole-head deadline fired: {why}"
            ),
            other => panic!("expected the whole-head deadline, got {other:?}"),
        }
    }

    #[test]
    fn respond_writes_status_body_and_length() {
        let (mut client, mut server) = pair();
        respond(&mut server, 401, "Unauthorized", "no token\n").unwrap();
        drop(server);
        let mut reply = String::new();
        std::io::Read::read_to_string(&mut client, &mut reply).unwrap();
        assert!(
            reply.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "{reply}"
        );
        assert!(reply.contains("Content-Length: 9\r\n"), "{reply}");
        assert!(reply.ends_with("\r\n\r\nno token\n"), "{reply}");
    }

    #[test]
    fn authorize_checks_bearer_then_subprotocol_then_query() {
        let token = with_value("ab".repeat(32));
        let head = |query: &[(String, String)], headers: Vec<(String, String)>| Head {
            method: "GET".to_owned(),
            path: "/acp".to_owned(),
            query: query.to_vec(),
            headers,
            leftover: vec![],
        };
        let query_token = [("token".to_owned(), token.as_str().to_owned())];
        let entry = format!("kage.{}", token.as_str());
        let bearer = vec![(
            "authorization".to_owned(),
            format!("Bearer {}", token.as_str()),
        )];

        assert_eq!(
            authorize(&head(&[], bearer.clone()), &token),
            Some(Auth::Bearer)
        );

        for scheme in ["bearer", "BEARER", "BeArEr"] {
            let lowered = vec![(
                "authorization".to_owned(),
                format!("{scheme} {}", token.as_str()),
            )];
            assert_eq!(
                authorize(&head(&[], lowered), &token),
                Some(Auth::Bearer),
                "the auth scheme is case-insensitive per RFC 9110"
            );
        }

        let scheme_only = vec![("authorization".to_owned(), "Bearer".to_owned())];
        assert_eq!(authorize(&head(&[], scheme_only), &token), None);
        let wrong_scheme = vec![(
            "authorization".to_owned(),
            format!("Basic {}", token.as_str()),
        )];
        assert_eq!(authorize(&head(&[], wrong_scheme), &token), None);

        let protocols = vec![("sec-websocket-protocol".to_owned(), entry.clone())];
        assert_eq!(
            authorize(&head(&[], protocols), &token),
            Some(Auth::Subprotocol(entry))
        );

        assert_eq!(
            authorize(&head(&query_token, vec![]), &token),
            Some(Auth::Query)
        );

        let wrong_bearer = vec![("authorization".to_owned(), "Bearer wrong".to_owned())];
        assert_eq!(authorize(&head(&[], wrong_bearer), &token), None);

        assert_eq!(authorize(&head(&[], vec![]), &token), None);
    }

    #[test]
    fn authorize_accepts_both_subprotocol_forms() {
        let token = with_value("cd".repeat(32));
        for prefix in [TOKEN_SUBPROTOCOL_PREFIX, UI_SUBPROTOCOL_PREFIX] {
            let head = Head {
                method: "GET".to_owned(),
                path: "/acp".to_owned(),
                query: vec![],
                headers: vec![(
                    "sec-websocket-protocol".to_owned(),
                    format!("chat, {prefix}{}", token.as_str()),
                )],
                leftover: vec![],
            };
            let accepted = format!("{prefix}{}", token.as_str());
            assert_eq!(authorize(&head, &token), Some(Auth::Subprotocol(accepted)));
        }
    }
}
