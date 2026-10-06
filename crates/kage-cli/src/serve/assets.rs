//! Serving the web client bundle from the `--web-dir` directory.
//!
//! [`WebDir`] answers the `GET /` and `GET /<file>` routes of the
//! serve routing table: the page at `/`, and any file under the web
//! directory. Every response carries the web UI's security headers,
//! and a path is served only when it stays inside the directory, with
//! %-escapes decoded before the check. The assets hold no secret, so
//! the token still guards `/acp` only.
//!
//! The bundle makes zero non-self requests: the page loads `boot.js`,
//! the glue and the module from the same origin, and the icons are
//! fetched same-origin on first use. The cross-origin isolation
//! headers on the page and the module follow the measurement of the
//! web build (see `gui/SPIKE.md`): the page runs identically with and
//! without them, and they are sent so a future atomics build can use
//! `SharedArrayBuffer` without a serving change.

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The Content-Security-Policy of every asset response. The page, the
/// glue and the module are all same-origin (`script-src 'self'`), and
/// `'wasm-unsafe-eval'` is the minimum WebAssembly compilation needs;
/// styles come from same-origin sheets, images from same-origin or
/// data URLs, and the WebSocket dial is `connect-src 'self'`. The four
/// hashes admit the exact `style` attributes of the GPUI web
/// platform's safe-area probes, one per edge, and nothing else.
pub(crate) const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self' \
     'wasm-unsafe-eval'; style-src 'self' 'unsafe-hashes' \
     'sha256-f7k/zkE0F5tLJydN2gyZQ4AOODzhaXhLLMp11FaIh5k=' \
     'sha256-tUiyeK8Byfwc/oNxmI63Upo8EZy+Z4dQfKT3FPt4Qq4=' \
     'sha256-MnlFZhszarHl/R/A2EWXa+4M/Hs8g3j+PBNaIrrscZ8=' \
     'sha256-KVR0sJO9NDObqCUVDwC9L9fIyWNaULEbUQPAXIyMyx4='; img-src 'self' data:; \
     connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// How long a rejected asset connection may take to collect its reply
/// before the socket is dropped.
const REJECT_DRAIN: Duration = Duration::from_millis(200);

const BODY_404: &str = "not found; the web UI serves its bundle here and /acp elsewhere\n";
const BODY_405: &str = "the web UI is read-only; use GET\n";

/// The opened web bundle. Available only when the directory exists and
/// holds an `index.html`; anything else keeps `/acp` working without a
/// web UI.
#[derive(Clone)]
pub(crate) struct WebDir {
    /// Canonical root served at `/`.
    root: Option<PathBuf>,
}

impl WebDir {
    /// Opens `dir` as the web bundle directory.
    pub(crate) fn open(dir: &Path) -> Self {
        let root = std::fs::canonicalize(dir)
            .ok()
            .filter(|root| root.join("index.html").is_file());
        Self { root }
    }

    /// Whether a page is served at `/` at all.
    pub(crate) fn available(&self) -> bool {
        self.root.is_some()
    }

    /// Answers one asset request: `/` with the page, `/`-separated
    /// names with the file under the web directory, everything else
    /// with `404`. A path that decodes out of the directory is
    /// reported as [`Outcome::Traversal`] and never read, and a target
    /// that is itself a symlink is refused, so a file swapped for a
    /// link between the checks cannot escape the directory.
    pub(crate) fn serve(&self, raw_path: &str, stream: &mut TcpStream) -> Outcome {
        let Some(decoded) = percent_decode(raw_path) else {
            reject(stream, 404, "Not Found", BODY_404);
            return Outcome::NotFound;
        };
        let segments = match safe_segments(&decoded) {
            Ok(segments) => segments,
            Err(SegmentError::Traversal) => {
                reject(stream, 404, "Not Found", BODY_404);
                return Outcome::Traversal;
            }
            Err(SegmentError::Unsafe) => {
                reject(stream, 404, "Not Found", BODY_404);
                return Outcome::NotFound;
            }
        };
        let Some(root) = self.root.as_deref() else {
            reject(stream, 404, "Not Found", BODY_404);
            return Outcome::NotFound;
        };
        let mut target = root.to_path_buf();
        if segments.is_empty() {
            target.push("index.html");
        } else {
            target.extend(segments.iter().copied());
        }
        let body =
            if std::fs::symlink_metadata(&target).is_ok_and(|meta| meta.file_type().is_symlink()) {
                None
            } else {
                std::fs::canonicalize(&target)
                    .ok()
                    .filter(|path| path.starts_with(root) && path.is_file())
                    .and_then(|path| std::fs::read(path).ok())
            };
        let Some(body) = body else {
            reject(stream, 404, "Not Found", BODY_404);
            return Outcome::NotFound;
        };
        let name = segments.last().copied().unwrap_or("index.html");
        let kind = content_type(name);
        write_reply(stream, 200, "OK", kind, cache_control(kind), &body);
        Outcome::Served
    }
}

/// What one asset request ended as, for the serve log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The file (or the page) was served.
    Served,
    /// No such file, or a path with characters outside the bundle's
    /// naming.
    NotFound,
    /// The path tried to leave the web directory.
    Traversal,
}

/// Answers a non-`GET` on the asset routes. Like every asset response
/// it carries the security headers.
pub(crate) fn reject_method(stream: &mut TcpStream) {
    reject(stream, 405, "Method Not Allowed", BODY_405);
}

/// Writes a plain-text asset rejection with the security headers.
fn reject(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    write_reply(
        stream,
        status,
        reason,
        "text/plain; charset=utf-8",
        "no-store",
        body.as_bytes(),
    );
}

/// Writes one asset reply: the security headers on every response,
/// cross-origin isolation on the page and the module, then drains the
/// unread inbound so the peer receives the answer instead of a reset.
fn write_reply(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    cache_control: &str,
    body: &[u8],
) {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Content-Security-Policy: {CONTENT_SECURITY_POLICY}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Cache-Control: {cache_control}\r\n",
        body.len(),
    );
    if isolated(content_type) {
        head.push_str("Cross-Origin-Opener-Policy: same-origin\r\n");
        head.push_str("Cross-Origin-Embedder-Policy: require-corp\r\n");
    }
    head.push_str("Connection: close\r\n\r\n");
    let wrote = stream.write_all(head.as_bytes()).is_ok()
        && stream.write_all(body).is_ok()
        && stream.flush().is_ok();
    if !wrote {
        return;
    }
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(REJECT_DRAIN));
    let mut sink = [0u8; 4096];
    loop {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// Whether the response needs the cross-origin isolation headers: the
/// page and the WebAssembly module, the two the web build measured.
fn isolated(content_type: &str) -> bool {
    content_type.starts_with("text/html") || content_type == "application/wasm"
}

/// The Content-Type of a served file name. The extension matches
/// case-insensitively, so an uppercase request for an existing file
/// (`BOOT.JS` on a case-insensitive filesystem) still serves as its
/// real type instead of `application/octet-stream`, which
/// `X-Content-Type-Options: nosniff` keeps the browser from
/// executing. Unknown extensions get `application/octet-stream`.
fn content_type(name: &str) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The Cache-Control of a response. The page is never cached. The
/// bundle's file names carry no content hash, so everything else is
/// cached for five minutes: short enough that a rebuilt bundle is
/// picked up without action, long enough to reuse the 24 MiB module
/// across reloads.
fn cache_control(content_type: &str) -> &'static str {
    if content_type.starts_with("text/html") {
        "no-store"
    } else {
        "public, max-age=300"
    }
}

/// Decodes the %-escapes of a request path. `None` on a truncated or
/// malformed escape, or a path that is not UTF-8 after decoding.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 3 > bytes.len() {
            return None;
        }
        let high = hex_digit(bytes[index + 1])?;
        let low = hex_digit(bytes[index + 2])?;
        out.push(high.wrapping_shl(4) | low);
        index += 3;
    }
    String::from_utf8(out).ok()
}

/// One hexadecimal digit of a %-escape.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Why a decoded path has no segments to serve.
#[derive(Debug, PartialEq, Eq)]
enum SegmentError {
    /// A `..` segment tries to leave the web directory.
    Traversal,
    /// A segment carries characters outside the bundle's naming.
    Unsafe,
}

/// Splits a decoded request path into segments the web directory may
/// hold. Empty and `.` segments are dropped, `..` is a traversal, and
/// any other segment may name any file: only control characters and
/// `\` are refused. Segments are already split on `/` and the
/// canonicalize-then-`starts_with` check confines reads to the
/// directory, so names with spaces, quotes, `+`, or non-ASCII bytes
/// are safe to serve (custom `--web-dir` content uses them today).
fn safe_segments(decoded: &str) -> Result<Vec<&str>, SegmentError> {
    let mut segments = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => return Err(SegmentError::Traversal),
            other if is_safe_segment(other) => segments.push(other),
            _ => return Err(SegmentError::Unsafe),
        }
    }
    Ok(segments)
}

/// Whether a segment can name a file under the web directory:
/// anything without control characters (C0 and C1, NUL included) or
/// a backslash.
fn is_safe_segment(segment: &str) -> bool {
    segment.chars().all(|c| !c.is_control() && c != '\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_segments_accept_spaces_quotes_plus_and_unicode() {
        for name in ["my file.svg", "we+ird.svg", "caf\u{e9}.svg", "\"q\".svg"] {
            assert_eq!(safe_segments(name).unwrap(), vec![name]);
        }
    }

    #[test]
    fn safe_segments_still_rejects_traversal_backslash_and_controls() {
        assert_eq!(safe_segments("a/.."), Err(SegmentError::Traversal));
        assert_eq!(safe_segments("a\\b"), Err(SegmentError::Unsafe));
        assert_eq!(safe_segments("\u{7f}.svg"), Err(SegmentError::Unsafe));
        assert_eq!(safe_segments("\u{9f}.svg"), Err(SegmentError::Unsafe));
        assert_eq!(safe_segments("a/\u{0}b"), Err(SegmentError::Unsafe));
    }

    #[test]
    fn a_percent_encoded_dot_dot_path_is_traversal_after_decoding() {
        let decoded = percent_decode("/%2e%2e/secret").unwrap();
        assert_eq!(decoded, "/../secret");
        assert_eq!(safe_segments(&decoded), Err(SegmentError::Traversal));
    }

    #[test]
    fn content_type_matches_extensions_case_insensitively() {
        assert_eq!(content_type("BOOT.JS"), "application/javascript");
        assert!(content_type("INDEX.HTML").starts_with("text/html"));
        assert_eq!(content_type("BOOT.WASM"), "application/wasm");
        assert_eq!(content_type("boot.js"), "application/javascript");
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("unknown.bin"), "application/octet-stream");
    }

    /// One request round trip over a real loopback socket: the
    /// accepted side is served, the connecting side reads the reply.
    fn serve_get(web: &WebDir, raw_path: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        web.serve(raw_path, &mut stream);
        let mut reply = String::new();
        std::io::BufReader::new(client)
            .read_to_string(&mut reply)
            .unwrap();
        reply
    }

    #[test]
    fn serves_a_file_with_a_space_in_its_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"page").unwrap();
        std::fs::write(dir.path().join("my file.svg"), b"<svg/>").unwrap();
        let web = WebDir::open(dir.path());
        assert!(web.available());
        let reply = serve_get(&web, "/my%20file.svg");
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(reply.contains("image/svg+xml"), "{reply}");
        assert!(reply.ends_with("<svg/>"), "{reply}");
    }

    #[test]
    fn a_traversal_request_is_reported_not_served() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"page").unwrap();
        let web = WebDir::open(dir.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(web.serve("/..%2fsecret", &mut stream), Outcome::Traversal);
        let mut reply = String::new();
        std::io::BufReader::new(client)
            .read_to_string(&mut reply)
            .unwrap();
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
    }

    /// A link inside the web directory must not serve its target,
    /// inside or outside the directory: the bundle ships plain files.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_asset_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"page").unwrap();
        std::fs::write(dir.path().join("plain.txt"), b"plain").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("leak.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink("plain.txt", dir.path().join("inside.txt")).unwrap();
        let web = WebDir::open(dir.path());

        let reply = serve_get(&web, "/leak.txt");
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
        assert!(!reply.contains("secret"), "{reply}");

        let reply = serve_get(&web, "/inside.txt");
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");

        let reply = serve_get(&web, "/plain.txt");
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.ends_with("plain"), "{reply}");
    }
}
