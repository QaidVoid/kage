//! LSP-style `Content-Length` framing over a byte stream.
//!
//! Each message is a JSON value preceded by a `Content-Length: N`
//! header and a blank line, both terminated with `\r\n`:
//!
//! ```text
//! Content-Length: 17\r\n
//! \r\n
//! {"jsonrpc":"2.0"}
//! ```
//!
//! This is the same framing LSP uses; editors that already speak LSP
//! (Zed, Neovim) reuse their transport code to drive `kage rpc`.

use std::io::{BufRead, Write};

/// A framing-layer failure: transport I/O, a malformed header, or a
/// body that is not valid JSON.
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    /// The underlying reader or writer failed.
    #[error("framing io: {0}")]
    Io(#[from] std::io::Error),
    /// The header block ended without a `Content-Length` field.
    #[error("framing: missing Content-Length header")]
    MissingContentLength,
    /// The `Content-Length` value was not a base-10 byte count.
    #[error("framing: invalid Content-Length value `{0}`")]
    InvalidContentLength(String),
    /// The stream ended in the middle of a header block or body.
    #[error("framing: unexpected end of stream mid-message")]
    UnexpectedEof,
    /// The body was not a JSON value.
    #[error("framing json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Write one framed JSON message and flush.
///
/// # Errors
///
/// Returns [`FramingError::Json`] if `value` cannot be serialized, or
/// [`FramingError::Io`] if the write or flush fails.
pub fn write_message<W: Write>(out: &mut W, value: &serde_json::Value) -> Result<(), FramingError> {
    let body = serde_json::to_vec(value)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

/// Read one framed JSON message.
///
/// Returns `Ok(None)` on a clean end of stream (no bytes before the
/// next header), so a caller can loop until the peer disconnects.
///
/// # Errors
///
/// Returns [`FramingError::MissingContentLength`] when the header
/// block carries no length, [`FramingError::InvalidContentLength`]
/// when it is not a number, [`FramingError::UnexpectedEof`] when the
/// stream ends mid-message, [`FramingError::Io`] on a transport
/// failure, or [`FramingError::Json`] when the body is not JSON.
pub fn read_message<R: BufRead>(input: &mut R) -> Result<Option<serde_json::Value>, FramingError> {
    let mut content_length: Option<usize> = None;
    let mut saw_any_header = false;

    loop {
        let mut line = String::new();
        let n = input.read_line(&mut line)?;
        if n == 0 {
            if saw_any_header {
                return Err(FramingError::UnexpectedEof);
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        saw_any_header = true;
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            let raw = value.trim();
            content_length = Some(
                raw.parse()
                    .map_err(|_| FramingError::InvalidContentLength(raw.to_owned()))?,
            );
        }
    }

    let len = content_length.ok_or(FramingError::MissingContentLength)?;
    let mut body = vec![0u8; len];
    input.read_exact(&mut body).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => FramingError::UnexpectedEof,
        _ => FramingError::Io(e),
    })?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("write");
        let mut cur = Cursor::new(buf);
        let got = read_message(&mut cur).expect("read").expect("some");
        assert_eq!(got, msg);
    }

    #[test]
    fn reads_two_messages_from_one_stream() {
        let a = serde_json::json!({"id": 1});
        let b = serde_json::json!({"id": 2});
        let mut buf = Vec::new();
        write_message(&mut buf, &a).unwrap();
        write_message(&mut buf, &b).unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(read_message(&mut cur).unwrap().unwrap(), a);
        assert_eq!(read_message(&mut cur).unwrap().unwrap(), b);
        assert_eq!(read_message(&mut cur).unwrap(), None);
    }

    #[test]
    fn clean_eof_returns_none() {
        let mut cur = Cursor::new(Vec::new());
        assert_eq!(read_message(&mut cur).unwrap(), None);
    }

    #[test]
    fn missing_content_length_errors() {
        let mut cur = Cursor::new(b"X-Other: 1\r\n\r\n{}".to_vec());
        assert!(matches!(
            read_message(&mut cur),
            Err(FramingError::MissingContentLength)
        ));
    }

    #[test]
    fn invalid_content_length_errors() {
        let mut cur = Cursor::new(b"Content-Length: abc\r\n\r\n{}".to_vec());
        assert!(matches!(
            read_message(&mut cur),
            Err(FramingError::InvalidContentLength(_))
        ));
    }

    #[test]
    fn truncated_body_errors() {
        let mut cur = Cursor::new(b"Content-Length: 50\r\n\r\n{}".to_vec());
        assert!(matches!(
            read_message(&mut cur),
            Err(FramingError::UnexpectedEof)
        ));
    }

    #[test]
    fn header_name_is_case_insensitive() {
        let mut cur = Cursor::new(b"content-length: 2\r\n\r\n{}".to_vec());
        assert_eq!(
            read_message(&mut cur).unwrap().unwrap(),
            serde_json::json!({})
        );
    }
}
