//! Shared Server-Sent-Events frame parsing.
//!
//! One reader serves both SSE consumers: HTTP providers stream model
//! events with event names, and the MCP HTTP transport forwards
//! JSON-RPC payloads under a per-frame byte cap so a server cannot
//! exhaust memory with one endless stream of lines. A frame is the
//! `data:` payload (successive `data:` lines joined with `\n`) plus
//! the optional `event:` name; blank lines terminate a frame and `:`
//! lines are comments.

use std::io::{self, BufRead, Read};

/// One decoded SSE frame: the `event:` name, when the stream sends
/// one, and the joined `data:` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseFrame {
    /// The `event:` field, `None` when the frame has none.
    pub name: Option<String>,
    /// The `data:` lines joined with `\n`.
    pub data: String,
}

/// Read one SSE frame from `reader`, or `None` at end of stream.
///
/// Blank lines terminate a frame, `:` lines are comments, `event:`
/// sets the name, and successive `data:` lines join with `\n`. A
/// frame still buffered at EOF is flushed before the terminating
/// `None`. An unnamed frame with empty data (a bare `data:` keepalive
/// line) is skipped rather than returned. When `max_bytes` is set, a
/// frame reading more than that many bytes fails with an
/// [`io::ErrorKind::InvalidData`] error.
///
/// # Errors
///
/// A line read fails, or `max_bytes` is set and the frame exceeds it.
pub fn read_frame<R: BufRead>(
    reader: &mut R,
    max_bytes: Option<u64>,
) -> io::Result<Option<SseFrame>> {
    let mut name = String::new();
    let mut data = String::new();
    let mut have_content = false;
    let mut frame_bytes = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        let n = match max_bytes {
            Some(limit) => reader.by_ref().take(limit).read_line(&mut line)?,
            None => reader.read_line(&mut line)?,
        };
        if let Some(limit) = max_bytes {
            frame_bytes = frame_bytes.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            if frame_bytes > limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sse frame exceeds size cap",
                ));
            }
        }
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if have_content && !(name.is_empty() && data.is_empty()) {
                return Ok(Some(take_frame(&mut name, &mut data)));
            }
            // An unnamed empty-data frame is a keepalive, not an
            // event: reset and keep reading instead of surfacing a
            // payload the consumers would fail to parse.
            frame_bytes = 0;
            name.clear();
            data.clear();
            have_content = false;
        } else if let Some(rest) = trimmed.strip_prefix("event:") {
            rest.trim_start().clone_into(&mut name);
            have_content = true;
        } else if let Some(rest) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
            have_content = true;
        }
        // Comment lines (`:` prefix) match no field and are ignored.
    }
    if have_content && !(name.is_empty() && data.is_empty()) {
        return Ok(Some(take_frame(&mut name, &mut data)));
    }
    Ok(None)
}

/// Move the accumulators into a frame, dropping an empty event name.
fn take_frame(name: &mut String, data: &mut String) -> SseFrame {
    SseFrame {
        name: (!name.is_empty()).then(|| std::mem::take(name)),
        data: std::mem::take(data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn frame(bytes: &[u8]) -> Option<SseFrame> {
        let mut reader = BufReader::new(bytes);
        read_frame(&mut reader, None).unwrap()
    }

    #[test]
    fn multi_line_data_joins_with_newlines() {
        let f = frame(b"data: a\ndata: b\ndata: c\n\n").unwrap();
        assert_eq!(f.name, None);
        assert_eq!(f.data, "a\nb\nc");
    }

    #[test]
    fn event_names_and_data_fields_are_kept() {
        let f = frame(b"event: message\ndata: {\"a\":1}\n\n").unwrap();
        assert_eq!(f.name.as_deref(), Some("message"));
        assert_eq!(f.data, "{\"a\":1}");
    }

    #[test]
    fn comments_and_unknown_fields_are_skipped() {
        let f = frame(b": keep-alive\nretry: 5\ndata: x\n\n").unwrap();
        assert_eq!(f.name, None);
        assert_eq!(f.data, "x");
        assert!(frame(b": only a comment\n\n").is_none());
    }

    #[test]
    fn a_bare_data_line_is_a_keepalive_between_frames() {
        let mut reader = BufReader::new(&b"data:\n\ndata: real\n\n"[..]);
        let f = read_frame(&mut reader, None).unwrap().unwrap();
        assert_eq!(f.name, None);
        assert_eq!(f.data, "real");
    }

    #[test]
    fn a_named_frame_with_empty_data_is_kept() {
        let f = frame(b"event: ping\ndata:\n\n").unwrap();
        assert_eq!(f.name.as_deref(), Some("ping"));
        assert!(f.data.is_empty());
    }

    #[test]
    fn crlf_endings_and_missing_space_after_the_colon_parse() {
        let f = frame(b"event:ping\r\ndata:x\r\n\r\n").unwrap();
        assert_eq!(f.name.as_deref(), Some("ping"));
        assert_eq!(f.data, "x");
    }

    #[test]
    fn an_unterminated_frame_flushes_at_eof() {
        assert_eq!(frame(b"data: tail").unwrap().data, "tail");
        assert!(frame(b"").is_none());
    }

    #[test]
    fn a_frame_over_the_cap_is_rejected() {
        let mut reader = BufReader::new(&b"data: 0123456789abcdef\n\n"[..]);
        let err = read_frame(&mut reader, Some(8)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "sse frame exceeds size cap");
    }

    #[test]
    fn the_cap_spans_the_whole_frame_but_resets_per_frame() {
        let mut reader = BufReader::new(&b"data: 12\ndata: 34\ndata: 56\n\ndata: ok\n\n"[..]);
        // Three data lines (27 bytes) exceed the 20-byte cap together.
        assert_eq!(
            read_frame(&mut reader, Some(20)).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut reader = BufReader::new(&b"data: 12\ndata: 34\n\ndata: ok\n\n"[..]);
        let f = read_frame(&mut reader, Some(20)).unwrap().unwrap();
        assert_eq!(f.data, "12\n34");
        let f = read_frame(&mut reader, Some(20)).unwrap().unwrap();
        assert_eq!(f.data, "ok");
    }
}
