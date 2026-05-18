//! Shared Server-Sent-Events stream plumbing.
//!
//! Every HTTP provider streamed its response the same way: a blocking
//! line reader that skips blank and comment lines and surfaces
//! `event:` / `data:` frames, wrapped in an identical `Iterator::next`
//! shell that drains a pending queue, honors the cancel flag, reads a
//! frame, feeds it to a provider-specific state machine, and fuses on
//! EOF. That shell and the framing reader lived four times over.
//!
//! Here they live once. A provider keeps its own state machine and
//! pending queue and implements [`SseStreamCore`]; the framing reader
//! and the loop are shared. The reader does full SSE framing (the
//! Anthropic grammar), which is a behavioral superset of the
//! `data:`-only readers the other providers used: those send one
//! `data:` line per blank-line-terminated event, so the joined-data /
//! `event:`-name handling never changes their observed payloads, and
//! processors that don't care about the event name simply ignore it.

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
/// Blank lines terminate a frame, `:` lines are comments, `event:`
/// sets the name, and successive `data:` lines join with `\n`. A
/// frame with content still buffered at EOF is flushed before the
/// terminating `Ok(None)`.
pub(crate) fn read_sse_event<R: BufRead>(
    reader: &mut R,
) -> Result<Option<SseEvent>, ProviderError> {
    let mut name = String::new();
    let mut data = String::new();
    let mut have_content = false;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if n == 0 {
            if have_content {
                return Ok(Some(SseEvent { name, data }));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if have_content {
                return Ok(Some(SseEvent { name, data }));
            }
            continue;
        }
        if trimmed.starts_with(':') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("event:") {
            rest.trim_start().clone_into(&mut name);
            have_content = true;
        } else if let Some(rest) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
            have_content = true;
        }
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
