//! Shared Server-Sent-Events stream plumbing.
//!
//! Every HTTP provider streams its response the same way: a blocking
//! line reader that skips blank and comment lines and surfaces
//! `event:` and `data:` frames, wrapped in an `Iterator::next` shell
//! that drains a pending queue, honors the cancel flag, reads a frame,
//! feeds it to a provider-specific state machine, and fuses on EOF.
//!
//! A provider keeps its own state machine and pending queue and
//! implements `SseStreamCore`. The framing reader and the loop are
//! shared. The reader does full SSE framing (the Anthropic grammar).
//! Providers that send one `data:` line per blank-line-terminated event
//! see the same payloads, and processors that do not care about the
//! event name simply ignore it.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};

use kage_core::CancelFlag;

use crate::{ProviderError, ProviderEvent};

/// One decoded SSE frame: the `event:` name (empty when the provider
/// sends only `data:` lines) and the joined `data:` payload.
#[derive(Debug)]
pub(crate) struct SseEvent {
    pub(crate) name: String,
    pub(crate) data: String,
}

/// Read the next SSE frame, or `Ok(None)` at end of stream.
///
/// Framing is shared with the MCP transport
/// ([`kage_core::sse::read_frame`]), uncapped: blank lines terminate
/// a frame, `:` lines are comments, `event:` sets the name, and
/// successive `data:` lines join with `\n`. A frame with content
/// still buffered at EOF is flushed before the terminating `Ok(None)`.
/// A frame whose name and data are both empty (a bare `data:`
/// keepalive line) is skipped rather than surfaced.
pub(crate) fn read_sse_event<R: BufRead>(
    reader: &mut R,
) -> Result<Option<SseEvent>, ProviderError> {
    match kage_core::sse::read_frame(reader, None) {
        Ok(Some(frame)) => Ok(Some(SseEvent {
            name: frame.name.unwrap_or_default(),
            data: frame.data,
        })),
        Ok(None) => Ok(None),
        Err(e) => Err(ProviderError::Transport(e.to_string())),
    }
}

/// A provider's streaming state machine, driven by [`sse_next`].
///
/// The provider owns its pending-event queue, its `done` flag, and
/// whatever assembly state it needs; this trait exposes just enough
/// for the shared loop to drive it.
pub(crate) trait SseStreamCore {
    /// The buffered byte source the framing reader pulls from.
    fn reader(&mut self) -> &mut BufReader<Box<dyn Read + Send>>;
    /// The caller's cancellation flag.
    fn cancel(&self) -> &CancelFlag;
    /// Events the state machine has produced but not yet yielded.
    fn pending(&mut self) -> &mut VecDeque<Result<ProviderEvent, ProviderError>>;
    /// Whether the stream is finished (no more frames will be read).
    fn is_done(&self) -> bool;
    /// Fuse the stream: no further frames are read after this.
    fn set_done(&mut self);
    /// Feed one decoded frame into the state machine, pushing any
    /// resulting [`ProviderEvent`]s onto [`Self::pending`].
    fn process(&mut self, name: &str, data: &str);
    /// Called once at clean end of stream, before fusing, so a
    /// provider that ends without an explicit terminal frame can
    /// synthesize a final `MessageEnd`. Default: nothing to do.
    fn on_eof(&mut self) {}
}

/// The shared `Iterator::next` body: drain pending, honor cancel,
/// read a frame and feed it to the state machine, fuse on EOF /
/// transport error. Behavior matches the four hand-written loops it
/// replaces, including draining queued events before observing a
/// late cancel and Gemini's EOF-synthesized `MessageEnd` (via
/// [`SseStreamCore::on_eof`]).
pub(crate) fn sse_next<S: SseStreamCore>(
    s: &mut S,
) -> Option<Result<ProviderEvent, ProviderError>> {
    loop {
        if let Some(ev) = s.pending().pop_front() {
            return Some(ev);
        }
        if s.is_done() {
            return None;
        }
        if s.cancel().is_cancelled() {
            s.set_done();
            return Some(Err(ProviderError::Cancelled));
        }
        match read_sse_event(s.reader()) {
            Ok(Some(ev)) => s.process(&ev.name, &ev.data),
            Ok(None) => {
                s.on_eof();
                s.set_done();
            }
            Err(e) => {
                s.set_done();
                return Some(Err(e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(bytes: &'static [u8]) -> Vec<SseEvent> {
        let mut reader = BufReader::new(std::io::Cursor::new(bytes));
        let mut out = Vec::new();
        while let Some(frame) = read_sse_event(&mut reader).unwrap() {
            out.push(frame);
        }
        out
    }

    #[test]
    fn empty_data_keepalives_yield_no_frames() {
        let fs = frames(b"data:\n\ndata: {\"a\":1}\n\n");
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].data, "{\"a\":1}");
    }

    #[test]
    fn empty_data_keepalive_at_eof_yields_no_frames() {
        assert!(frames(b"data:\n\n").is_empty());
        assert!(frames(b"data:").is_empty());
    }

    #[test]
    fn named_frame_with_empty_data_is_kept() {
        let fs = frames(b"event: ping\ndata:\n\n");
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].name, "ping");
        assert!(fs[0].data.is_empty());
    }
}
