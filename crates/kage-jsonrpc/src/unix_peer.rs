//! Ordered JSON-RPC over an interruptible Unix pipe writer.
//!
//! The owner must continuously drain the inbound receiver and arrange for the
//! reader to terminate. Shutdown interrupts writes and settles pending requests,
//! but cannot interrupt an arbitrary `BufRead` or a full inbound queue.

use std::io::{self, BufRead, Write};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use crossbeam_channel::{Receiver, select_biased};
use kage_core::CancelFlag;
use kage_core::sync::lock;

use crate::unix_pipe::PipeWriter;
use crate::{
    InboundSender, OrderedInbound, OrderedReply, Peer, Pending, PendingRequest, ResponseBarrier,
    RpcError, close_pending, closed, connect_inner,
};

const MAX_ADMITTED: usize = 16;
const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_PENDING: usize = 256;
const ADMISSION_INTERVAL: Duration = Duration::from_millis(10);
const ERROR_REPLY_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_INBOUND_BUDGET: usize = 16 * 1024 * 1024;

/// Limits for inbound Unix peer messages, measured in encoded bytes including
/// the newline. Parsed JSON allocation overhead and messages retained by the
/// consumer are additional and are not bounded by this wire budget.
///
/// The default is a 1 MiB line limit and 8 queued items. Both values must be
/// nonzero, the line limit may not exceed 8 MiB, queue capacity may not exceed
/// 256, and their product may not exceed 16 MiB.
#[derive(Clone, Copy, Debug)]
pub struct ReadLimits {
    /// Maximum encoded size of one incoming line, including its newline.
    pub max_line_bytes: usize,
    /// Maximum number of queued incoming messages and ordering markers.
    pub inbound_capacity: usize,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: 1024 * 1024,
            inbound_capacity: 8,
        }
    }
}

/// Snapshot of the input reader's lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadStatus {
    /// The reader has not terminated.
    Open,
    /// The reader reached a clean end of input.
    CleanEof,
    /// The reader terminated due to a fatal framing, routing, or I/O error.
    /// Recoverable malformed JSON and invalid requests do not terminate it
    /// if their error replies can be written.
    Failed,
}

struct State {
    peer: Peer,
    pipe: Arc<PipeWriter>,
    admitted: AtomicUsize,
    read_eof: Arc<AtomicBool>,
    eof_gate: Arc<Mutex<bool>>,
    read_status: Arc<AtomicU8>,
    shutting_down: AtomicBool,
}

struct Admission<'a>(&'a AtomicUsize);

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.shutting_down.store(true, Ordering::Release);
        close_pending(&self.peer.pending);
        self.pipe.shutdown();
    }
}

/// Cloneable ordered peer with interruptible writes and explicit shutdown.
///
/// Dropping the last clone shuts down writes and settles pending requests, but
/// does not interrupt the reader. Pipe ownership and SIGPIPE caveats are
/// documented on [`PipeWriter`]. Deadlines do not bound JSON serialization,
/// thread scheduling, inbound queue draining, or response barrier waits.
/// At most 16 writes are admitted at once, and at most 256 requests may be
/// pending. Outbound frames are capped at 8 MiB including the newline, using a
/// bounded streaming serializer. Callers retain ownership of argument values
/// while waiting for admission, so their memory and thread count are not bounded
/// by this peer.
#[derive(Clone)]
pub struct UnixPeer(Arc<State>);

/// An ordered request whose entire frame has been written to the pipe.
///
/// Waiting or dropping this handle never sends a protocol cancellation notice.
/// The owner must coordinate any such notice with the application lifecycle.
/// Dropping it removes only this request from the pending table.
pub struct CommittedRequest {
    id: i64,
    method: String,
    pending: Pending,
    receiver: Receiver<OrderedReply>,
}

impl Drop for CommittedRequest {
    fn drop(&mut self) {
        lock(&self.pending).requests.remove(&self.id);
    }
}

impl CommittedRequest {
    /// The JSON-RPC request ID, for protocols whose cancel message needs it.
    #[must_use]
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Wait for the response, cancellation, or absolute deadline.
    ///
    /// Returns the response and its ordering barrier. Cancellation and timeout
    /// abandon the request locally without writing a notice. If multiple
    /// conditions are ready, cancellation takes priority over the deadline,
    /// and both take priority over a buffered response. The barrier, when
    /// present, requires separate acknowledgement by the inbound consumer.
    pub fn wait(
        self,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> (Result<serde_json::Value, RpcError>, Option<ResponseBarrier>) {
        let watch = cancel.map(CancelFlag::watch);
        let cancelled = watch
            .as_ref()
            .map_or_else(crossbeam_channel::never, |w| w.receiver().clone());
        let timer = deadline.map_or_else(crossbeam_channel::never, |d| {
            crossbeam_channel::after(d.saturating_duration_since(Instant::now()))
        });
        select_biased! {
            recv(cancelled) -> _ => (Err(RpcError::new(-32800, "request cancelled")), None),
            recv(timer) -> _ => (Err(RpcError::timed_out(&self.method)), None),
            recv(self.receiver) -> result => result.unwrap_or_else(|_| (Err(closed()), None)),
        }
    }
}

struct CappedFrame(Vec<u8>);

impl Write for CappedFrame {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (MAX_FRAME - 1).saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "outbound frame exceeds size cap",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ReaderWriter(Arc<PipeWriter>);

impl Write for ReaderWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .write_frame(bytes, Some(Instant::now() + ERROR_REPLY_TIMEOUT), None)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Start an ordered peer using the owned write end of a Unix pipe or FIFO.
///
/// The reader thread closes the writer and settles pending requests at EOF or
/// read failure. The caller must continuously drain the inbound receiver. If
/// it is full, the reader cannot observe EOF until the queue is drained.
///
/// # Errors
/// Returns an error when the descriptor is not a writable pipe or cannot be
/// configured as nonblocking. See [`PipeWriter::new`] for ownership constraints.
pub fn connect_ordered<R: BufRead + Send + 'static>(
    reader: R,
    writer: OwnedFd,
) -> io::Result<(UnixPeer, mpsc::Receiver<OrderedInbound>, JoinHandle<()>)> {
    connect_ordered_with_limits(reader, writer, ReadLimits::default())
}

/// Start an ordered Unix peer with explicit bounded inbound limits.
/// Oversized frames receive a parse error when possible and close the peer.
/// An unterminated final frame is discarded and the connection closes.
///
/// # Errors
/// Returns an error for invalid limits or an unsupported pipe descriptor.
pub fn connect_ordered_with_limits<R: BufRead + Send + 'static>(
    reader: R,
    writer: OwnedFd,
    limits: ReadLimits,
) -> io::Result<(UnixPeer, mpsc::Receiver<OrderedInbound>, JoinHandle<()>)> {
    connect_with_mode(reader, writer, limits, false)
}

/// Start a server peer that preserves its write side on clean read EOF.
///
/// EOF settles outgoing requests and prohibits new dispatches. Notifications
/// and responses remain available until explicit shutdown or the last peer is
/// dropped. I/O errors, invalid UTF-8, oversized or unterminated frames, and
/// failed error replies close the transport. Malformed JSON and invalid requests
/// are recoverable when their error replies succeed. The inbound stream must be drained for EOF to be seen.
///
/// # Errors
/// Returns an error for an unsupported pipe descriptor.
pub fn connect_ordered_server<R: BufRead + Send + 'static>(
    reader: R,
    writer: OwnedFd,
) -> io::Result<(UnixPeer, mpsc::Receiver<OrderedInbound>, JoinHandle<()>)> {
    connect_ordered_server_with_limits(reader, writer, ReadLimits::default())
}

/// Start a server peer with explicit bounded inbound limits.
///
/// # Errors
/// Returns an error for invalid limits or an unsupported pipe descriptor.
pub fn connect_ordered_server_with_limits<R: BufRead + Send + 'static>(
    reader: R,
    writer: OwnedFd,
    limits: ReadLimits,
) -> io::Result<(UnixPeer, mpsc::Receiver<OrderedInbound>, JoinHandle<()>)> {
    connect_with_mode(reader, writer, limits, true)
}

fn connect_with_mode<R: BufRead + Send + 'static>(
    reader: R,
    writer: OwnedFd,
    limits: ReadLimits,
    server: bool,
) -> io::Result<(UnixPeer, mpsc::Receiver<OrderedInbound>, JoinHandle<()>)> {
    if limits.max_line_bytes == 0
        || limits.max_line_bytes > 8 * 1024 * 1024
        || limits.inbound_capacity == 0
        || limits.inbound_capacity > 256
        || limits
            .max_line_bytes
            .checked_mul(limits.inbound_capacity)
            .is_none_or(|n| n > MAX_INBOUND_BUDGET)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid inbound limits",
        ));
    }
    let pipe = Arc::new(PipeWriter::new(writer)?);
    let weak_pipe = Arc::downgrade(&pipe);
    let read_eof = Arc::new(AtomicBool::new(false));
    let reader_eof = Arc::clone(&read_eof);
    let eof_gate = Arc::new(Mutex::new(false));
    let reader_gate = Arc::clone(&eof_gate);
    let read_status = Arc::new(AtomicU8::new(0));
    let reader_status = Arc::clone(&read_status);
    let (tx, rx) = mpsc::sync_channel(limits.inbound_capacity);
    let (peer, handle) = connect_inner(
        reader,
        ReaderWriter(Arc::clone(&pipe)),
        None,
        InboundSender::Ordered(tx),
        true,
        u64::try_from(limits.max_line_bytes).expect("validated inbound limit"),
        Some(MAX_FRAME),
        Some(Box::new(move |clean_eof| {
            if server && clean_eof {
                {
                    let mut gate = lock(&reader_gate);
                    *gate = true;
                    reader_eof.store(true, Ordering::Release);
                }
                reader_status.store(1, Ordering::Release);
            } else {
                reader_status.store(if clean_eof { 1 } else { 2 }, Ordering::Release);
                if let Some(pipe) = weak_pipe.upgrade() {
                    pipe.shutdown();
                }
            }
        })),
    );
    Ok((
        UnixPeer(Arc::new(State {
            peer,
            pipe,
            admitted: AtomicUsize::new(0),
            read_eof,
            eof_gate,
            read_status,
            shutting_down: AtomicBool::new(false),
        })),
        rx,
        handle,
    ))
}

impl UnixPeer {
    /// Return a snapshot of the input reader's lifecycle.
    #[must_use]
    pub fn read_status(&self) -> ReadStatus {
        match self.0.read_status.load(Ordering::Acquire) {
            1 => ReadStatus::CleanEof,
            2 => ReadStatus::Failed,
            _ => ReadStatus::Open,
        }
    }

    /// Return whether the write side remains open at this instant.
    #[must_use]
    pub fn is_write_open(&self) -> bool {
        !self.0.shutting_down.load(Ordering::Acquire) && !self.0.pipe.is_closed()
    }

    /// Permanently shut down the write side and settle all pending requests.
    /// This does not interrupt the separately owned reader or inbound queue.
    pub fn shutdown(&self) {
        self.0.shutting_down.store(true, Ordering::Release);
        close_pending(&self.0.peer.pending);
        self.0.pipe.shutdown();
    }

    fn ensure_writable(&self, allow_after_eof: bool) -> Result<(), RpcError> {
        let pending_closed = lock(&self.0.peer.pending).closed;
        let eof = self.0.read_eof.load(Ordering::Acquire);
        if self.0.shutting_down.load(Ordering::Acquire)
            || self.0.pipe.is_closed()
            || (eof && !allow_after_eof)
            || (!eof && pending_closed)
        {
            return Err(closed());
        }
        Ok(())
    }

    fn admit(
        &self,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
        allow_after_eof: bool,
    ) -> Result<Admission<'_>, RpcError> {
        loop {
            self.ensure_writable(allow_after_eof)?;
            if cancel.is_some_and(CancelFlag::is_cancelled) {
                return Err(RpcError::new(-32800, "write cancelled"));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(RpcError::new(-32000, "write timed out"));
            }
            if self
                .0
                .admitted
                .fetch_update(Ordering::Acquire, Ordering::Relaxed, |n| {
                    (n < MAX_ADMITTED).then_some(n + 1)
                })
                .is_ok()
            {
                return Ok(Admission(&self.0.admitted));
            }
            let delay = deadline.map_or(ADMISSION_INTERVAL, |d| {
                d.saturating_duration_since(Instant::now())
                    .min(ADMISSION_INTERVAL)
            });
            std::thread::sleep(delay);
        }
    }

    fn write_value(
        &self,
        value: &serde_json::Value,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
        allow_after_eof: bool,
    ) -> Result<(), RpcError> {
        self.ensure_writable(allow_after_eof)?;
        let mut frame = CappedFrame(Vec::new());
        serde_json::to_writer(&mut frame, value)
            .map_err(|e| RpcError::internal(format!("encode: {e}")))?;
        frame.0.push(b'\n');
        match self.0.pipe.write_frame_guarded(
            &frame.0,
            deadline,
            cancel,
            (!allow_after_eof).then_some(&self.0.eof_gate),
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                if self.0.pipe.is_closed() {
                    close_pending(&self.0.peer.pending);
                }
                Err(match e.kind() {
                    io::ErrorKind::Interrupted => RpcError::new(-32800, "write cancelled"),
                    io::ErrorKind::TimedOut => RpcError::new(-32000, "write timed out"),
                    _ => RpcError::internal(format!("write: {e}")),
                })
            }
        }
    }

    /// Write a notification. Deadline and cancellation cover writer admission
    /// and the pipe write, but not JSON encoding.
    ///
    /// # Errors
    /// Returns an error on cancellation, deadline, closure, or encoding failure.
    pub fn notify(
        &self,
        method: &str,
        params: serde_json::Value,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> Result<(), RpcError> {
        let _admission = self.admit(deadline, cancel, true)?;
        check_method(method)?;
        self.write_value(&message(method, params, None), deadline, cancel, true)
    }

    /// Write a response to an inbound request with bounded writer admission.
    ///
    /// # Errors
    /// Returns an error on cancellation, deadline, closure, or encoding failure.
    pub fn respond(
        &self,
        id: &serde_json::Value,
        result: Result<serde_json::Value, RpcError>,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> Result<(), RpcError> {
        let _admission = self.admit(deadline, cancel, true)?;
        serde_json::to_writer(&mut CappedFrame(Vec::new()), id)
            .map_err(|e| RpcError::internal(format!("encode: {e}")))?;
        self.write_value(&native_response(id, result), deadline, cancel, true)
    }

    /// Commit a request frame and return a handle for its ordered response.
    ///
    /// The deadline and cancellation cover writer admission and the complete
    /// write. If cancellation or timeout occurs after a partial frame, the
    /// transport closes and all pending requests fail. An error before any
    /// bytes are written leaves the connection usable. Pass the same absolute
    /// deadline to [`CommittedRequest::wait`] to bound the full operation.
    ///
    /// Response correlation assumes a cooperative peer. The request ID is
    /// registered before the frame is fully written, and a peer that guesses
    /// or observes that ID in a partial frame can send a matching response
    /// before dispatch commits. A successful dispatch does not prove that a
    /// response was produced after the peer received the complete request.
    /// JSON-RPC IDs do not authenticate a peer or its claimed results.
    ///
    /// # Errors
    /// Returns an error if the request could not be committed.
    pub fn dispatch(
        &self,
        method: &str,
        params: serde_json::Value,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> Result<CommittedRequest, RpcError> {
        let _admission = self.admit(deadline, cancel, false)?;
        check_method(method)?;
        let id = self
            .0
            .peer
            .next_id
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .map_err(|_| RpcError::internal("request IDs exhausted"))?;
        let (tx, receiver) = crossbeam_channel::bounded(1);
        {
            let mut pending = lock(&self.0.peer.pending);
            if pending.closed || self.0.read_eof.load(Ordering::Acquire) {
                return Err(closed());
            }
            if pending.requests.len() >= MAX_PENDING {
                return Err(RpcError::internal("too many pending requests"));
            }
            pending
                .requests
                .insert(id, PendingRequest { tx, ordered: true });
        }
        let handle = CommittedRequest {
            id,
            method: method.to_owned(),
            pending: Arc::clone(&self.0.peer.pending),
            receiver,
        };
        self.write_value(&message(method, params, Some(id)), deadline, cancel, false)?;
        Ok(handle)
    }
}

fn check_method(method: &str) -> Result<(), RpcError> {
    if method.len() >= MAX_FRAME {
        return Err(RpcError::internal("outbound frame exceeds size cap"));
    }
    Ok(())
}

fn native_response(
    id: &serde_json::Value,
    result: Result<serde_json::Value, RpcError>,
) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert("jsonrpc".into(), "2.0".into());
    object.insert("id".into(), id.clone());
    match result {
        Ok(value) => {
            object.insert("result".into(), value);
        }
        Err(error) => {
            let mut obj = serde_json::Map::new();
            obj.insert("code".into(), error.code.into());
            obj.insert("message".into(), error.message.into());
            object.insert("error".into(), serde_json::Value::Object(obj));
        }
    }
    serde_json::Value::Object(object)
}

fn message(method: &str, params: serde_json::Value, id: Option<i64>) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert("jsonrpc".into(), "2.0".into());
    object.insert("method".into(), method.into());
    object.insert("params".into(), params);
    if let Some(id) = id {
        object.insert("id".into(), id.into());
    }
    serde_json::Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::thread;
    use std::time::Duration;

    type Fixture = (
        UnixPeer,
        mpsc::Receiver<OrderedInbound>,
        JoinHandle<()>,
        BufReader<File>,
        File,
    );

    fn fixture() -> Fixture {
        let (agent_read, client_write) = nix::unistd::pipe().unwrap();
        let (client_read, agent_write) = nix::unistd::pipe().unwrap();
        let (peer, incoming, reader) =
            connect_ordered(BufReader::new(File::from(client_read)), client_write).unwrap();
        (
            peer,
            incoming,
            reader,
            BufReader::new(File::from(agent_read)),
            File::from(agent_write),
        )
    }

    fn read_message(reader: &mut BufReader<File>) -> serde_json::Value {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn cancellation_and_deadline_take_priority_over_buffered_response() {
        let (peer, incoming, reader, mut agent_in, mut agent_out) = fixture();
        for cancelled in [false, true] {
            let request = peer
                .dispatch("test", serde_json::Value::Null, None, None)
                .unwrap();
            let id = read_message(&mut agent_in)["id"].clone();
            writeln!(agent_out, "{}", serde_json::json!({"id": id, "result": 42})).unwrap();
            let OrderedInbound::Barrier(marker) =
                incoming.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("expected barrier");
            };
            marker.acknowledge();
            let limit = Instant::now() + Duration::from_secs(2);
            while request.receiver.is_empty() {
                assert!(Instant::now() < limit, "response was not routed");
                thread::yield_now();
            }
            let flag = CancelFlag::new();
            if cancelled {
                flag.cancel();
            }
            let error = request
                .wait(Some(Instant::now()), Some(&flag))
                .0
                .unwrap_err();
            assert_eq!(error.code, if cancelled { -32800 } else { -32000 });
        }
        peer.shutdown();
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn committed_handles_route_independently_and_drop_does_not_write() {
        let (peer, incoming, reader, mut agent_in, mut agent_out) = fixture();
        let first = peer
            .dispatch("first", serde_json::Value::Null, None, None)
            .unwrap();
        let second = peer
            .dispatch("second", serde_json::Value::Null, None, None)
            .unwrap();
        assert_ne!(first.id(), second.id());
        let first_id = read_message(&mut agent_in)["id"].as_i64().unwrap();
        let second_id = read_message(&mut agent_in)["id"].as_i64().unwrap();
        drop(first);
        writeln!(
            agent_out,
            "{}",
            serde_json::json!({"jsonrpc":"2.0", "id": first_id, "result": "ignored"})
        )
        .unwrap();
        writeln!(
            agent_out,
            "{}",
            serde_json::json!({"jsonrpc":"2.0", "id": second_id, "result": 42})
        )
        .unwrap();
        let OrderedInbound::Barrier(marker) =
            incoming.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected barrier");
        };
        marker.acknowledge();
        let (result, barrier) = second.wait(Some(Instant::now() + Duration::from_secs(2)), None);
        assert_eq!(result.unwrap(), 42);
        barrier
            .unwrap()
            .wait_timeout(Duration::from_secs(2))
            .unwrap();
        peer.shutdown();
        let mut rest = Vec::new();
        agent_in.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn cancelled_partial_dispatch_settles_other_requests() {
        let (peer, _incoming, reader, mut agent_in, agent_out) = fixture();
        let pending = peer
            .dispatch("pending", serde_json::Value::Null, None, None)
            .unwrap();
        read_message(&mut agent_in);
        let cancel = CancelFlag::new();
        let worker_cancel = cancel.clone();
        let worker_peer = peer.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            tx.send(
                worker_peer
                    .dispatch(
                        "large",
                        serde_json::Value::String("x".repeat(4 * 1024 * 1024)),
                        None,
                        Some(&worker_cancel),
                    )
                    .map(|_| ()),
            )
            .unwrap();
        });
        let mut one = [0];
        agent_in.read_exact(&mut one).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        cancel.cancel();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .code,
            -32800
        );
        worker.join().unwrap();
        assert!(
            pending
                .wait(Some(Instant::now() + Duration::from_secs(2)), None)
                .0
                .is_err()
        );
        assert!(
            peer.notify("after", serde_json::Value::Null, None, None)
                .is_err()
        );
        let mut rest = Vec::new();
        agent_in.read_to_end(&mut rest).unwrap();
        assert!(!rest.is_empty());
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn server_clean_eof_retains_output_for_responses_and_notifications() {
        let (agent_read, client_write) = nix::unistd::pipe().unwrap();
        let (client_read, agent_write) = nix::unistd::pipe().unwrap();
        let (peer, incoming, reader) =
            connect_ordered_server(BufReader::new(File::from(client_read)), client_write).unwrap();
        let mut agent_in = BufReader::new(File::from(agent_read));
        let mut agent_out = File::from(agent_write);
        let pending = peer
            .dispatch("outgoing", serde_json::Value::Null, None, None)
            .unwrap();
        assert_eq!(read_message(&mut agent_in)["method"], "outgoing");
        writeln!(
            agent_out,
            "{}",
            serde_json::json!({"jsonrpc":"2.0", "method":"incoming", "id":7})
        )
        .unwrap();
        drop(agent_out);
        assert!(matches!(
            incoming.recv_timeout(Duration::from_secs(2)).unwrap(),
            OrderedInbound::Message(crate::Inbound::Request { .. })
        ));
        reader.join().unwrap();
        assert!(
            pending
                .wait(Some(Instant::now() + Duration::from_secs(1)), None)
                .0
                .is_err()
        );
        assert!(
            peer.dispatch("after", serde_json::Value::Null, None, None)
                .is_err()
        );
        peer.respond(&serde_json::json!(7), Ok(serde_json::json!(42)), None, None)
            .unwrap();
        peer.notify("update", serde_json::json!(1), None, None)
            .unwrap();
        assert_eq!(read_message(&mut agent_in)["result"], 42);
        assert_eq!(read_message(&mut agent_in)["method"], "update");
        peer.shutdown();
        let mut rest = Vec::new();
        agent_in.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert!(
            peer.notify("after", serde_json::Value::Null, None, None)
                .is_err()
        );
    }

    #[test]
    fn server_unterminated_eof_closes_output() {
        let (agent_read, client_write) = nix::unistd::pipe().unwrap();
        let (client_read, agent_write) = nix::unistd::pipe().unwrap();
        let (peer, _incoming, reader) =
            connect_ordered_server(BufReader::new(File::from(client_read)), client_write).unwrap();
        let mut agent_out = File::from(agent_write);
        write!(agent_out, "{{\"id\":1,\"method\":\"unfinished\"}}").unwrap();
        drop(agent_out);
        reader.join().unwrap();
        assert!(
            peer.notify("after", serde_json::Value::Null, None, None)
                .is_err()
        );
        let mut rest = Vec::new();
        File::from(agent_read).read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }

    #[test]
    fn shutdown_and_reader_eof_settle_pending_and_close_writer() {
        for eof in [false, true] {
            let (peer, _incoming, reader, mut agent_in, agent_out) = fixture();
            let pending = peer
                .dispatch("pending", serde_json::Value::Null, None, None)
                .unwrap();
            read_message(&mut agent_in);
            if eof {
                drop(agent_out);
            } else {
                peer.shutdown();
                drop(agent_out);
            }
            assert!(
                pending
                    .wait(Some(Instant::now() + Duration::from_secs(2)), None)
                    .0
                    .is_err()
            );
            reader.join().unwrap();
            let mut rest = Vec::new();
            agent_in.read_to_end(&mut rest).unwrap();
            assert!(rest.is_empty());
            assert!(
                peer.dispatch("after", serde_json::Value::Null, None, None)
                    .is_err()
            );
        }
    }

    #[test]
    fn admission_deadline_and_streaming_frame_cap_preserve_transport() {
        let (peer, _incoming, reader, mut agent_in, agent_out) = fixture();
        let permits: Vec<_> = (0..MAX_ADMITTED)
            .map(|_| peer.admit(None, None, false).unwrap())
            .collect();
        assert_eq!(
            peer.notify(
                "blocked",
                serde_json::Value::Null,
                Some(Instant::now() + Duration::from_millis(30)),
                None
            )
            .unwrap_err()
            .code,
            -32000
        );
        drop(permits);
        assert!(
            peer.notify(
                "oversize",
                serde_json::Value::String("x".repeat(MAX_FRAME + 1)),
                None,
                None
            )
            .is_err()
        );
        peer.notify("ok", serde_json::Value::Null, None, None)
            .unwrap();
        assert_eq!(read_message(&mut agent_in)["method"], "ok");
        peer.shutdown();
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn shutdown_settles_pending_while_barrier_queue_is_full() {
        let (peer, incoming, reader, mut agent_in, mut agent_out) = fixture();
        let pending = peer
            .dispatch("pending", serde_json::Value::Null, None, None)
            .unwrap();
        let id = read_message(&mut agent_in)["id"].clone();
        for _ in 0..ReadLimits::default().inbound_capacity {
            writeln!(
                agent_out,
                "{}",
                serde_json::json!({"jsonrpc":"2.0", "method":"event"})
            )
            .unwrap();
        }
        writeln!(
            agent_out,
            "{}",
            serde_json::json!({"jsonrpc":"2.0", "id":id, "result":true})
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || tx.send(pending.wait(None, None).0).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(60)).is_err());
        peer.shutdown();
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        worker.join().unwrap();
        drop(incoming);
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn malformed_response_fails_without_barrier_and_closes_transport() {
        for response in [
            serde_json::json!({"jsonrpc":"2.0", "id":1}),
            serde_json::json!({"jsonrpc":"2.0", "id":1, "result":true, "error":{"code":-1, "message":"bad"}}),
            serde_json::json!({"jsonrpc":"2.0", "id":1, "error":{"code":"bad", "message":"bad"}}),
        ] {
            let (peer, _incoming, reader, mut agent_in, mut agent_out) = fixture();
            let pending = peer
                .dispatch("pending", serde_json::Value::Null, None, None)
                .unwrap();
            assert_eq!(read_message(&mut agent_in)["id"], 1);
            writeln!(agent_out, "{response}").unwrap();
            let (result, barrier) =
                pending.wait(Some(Instant::now() + Duration::from_secs(2)), None);
            assert_eq!(result.unwrap_err().code, -32600);
            assert!(barrier.is_none());
            reader.join().unwrap();
            assert!(
                peer.dispatch("after", serde_json::Value::Null, None, None)
                    .is_err()
            );
        }
    }

    #[test]
    fn eof_does_not_deliver_unterminated_valid_frame() {
        let (peer, incoming, reader, _agent_in, mut agent_out) = fixture();
        write!(
            agent_out,
            "{{\"jsonrpc\":\"2.0\",\"method\":\"unfinished\"}}"
        )
        .unwrap();
        drop(agent_out);
        reader.join().unwrap();
        assert!(incoming.try_recv().is_err());
        assert!(
            peer.notify("after", serde_json::Value::Null, None, None)
                .is_err()
        );
    }

    #[test]
    fn error_reply_to_full_pipe_is_bounded() {
        use nix::errno::Errno;
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        let (agent_read, client_write) = nix::unistd::pipe().unwrap();
        let flags = OFlag::from_bits_truncate(fcntl(&client_write, FcntlArg::F_GETFL).unwrap());
        fcntl(&client_write, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).unwrap();
        loop {
            match nix::unistd::write(&client_write, &[b'x'; 4096]) {
                Ok(_) => {}
                Err(Errno::EAGAIN) => break,
                Err(error) => panic!("fill pipe: {error}"),
            }
        }
        let (client_read, agent_write) = nix::unistd::pipe().unwrap();
        let (peer, _incoming, reader) =
            connect_ordered(BufReader::new(File::from(client_read)), client_write).unwrap();
        let mut agent_out = File::from(agent_write);
        writeln!(agent_out, "not json").unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(reader.join()).unwrap());
        rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        assert!(
            peer.notify("after", serde_json::Value::Null, None, None)
                .is_err()
        );
        drop(agent_read);
    }

    #[test]
    fn configured_line_limit_rejects_without_echoing_payload() {
        let (agent_read, client_write) = nix::unistd::pipe().unwrap();
        let (client_read, agent_write) = nix::unistd::pipe().unwrap();
        let limits = ReadLimits {
            max_line_bytes: 64,
            inbound_capacity: 1,
        };
        let (peer, incoming, reader) = connect_ordered_with_limits(
            BufReader::new(File::from(client_read)),
            client_write,
            limits,
        )
        .unwrap();
        let mut agent_out = File::from(agent_write);
        writeln!(agent_out, "{{\"method\":\"{}\"}}", "secret".repeat(20)).unwrap();
        let mut agent_in = BufReader::new(File::from(agent_read));
        let response = read_message(&mut agent_in);
        assert_eq!(response["error"]["code"], -32700);
        assert!(!response.to_string().contains("secret"));
        reader.join().unwrap();
        assert!(incoming.try_recv().is_err());
        assert!(
            peer.notify("after", serde_json::Value::Null, None, None)
                .is_err()
        );
    }

    #[test]
    fn waiting_deadline_and_handle_drop_do_not_close_transport() {
        let (peer, _incoming, reader, mut agent_in, agent_out) = fixture();
        let pending = peer
            .dispatch("pending", serde_json::Value::Null, None, None)
            .unwrap();
        read_message(&mut agent_in);
        assert_eq!(
            pending.wait(Some(Instant::now()), None).0.unwrap_err().code,
            -32000
        );
        let next = peer
            .dispatch("next", serde_json::Value::Null, None, None)
            .unwrap();
        assert_eq!(read_message(&mut agent_in)["method"], "next");
        drop(next);
        peer.shutdown();
        drop(agent_out);
        reader.join().unwrap();
    }
}

#[cfg(test)]
mod eof_race_tests {
    use super::*;
    use std::fs::File;
    use std::io::{BufReader, Read, Write};
    use std::thread;

    fn server() -> (
        UnixPeer,
        mpsc::Receiver<OrderedInbound>,
        JoinHandle<()>,
        File,
        File,
    ) {
        let (out_read, out_write) = nix::unistd::pipe().unwrap();
        let (in_read, in_write) = nix::unistd::pipe().unwrap();
        let (peer, rx, handle) =
            connect_ordered_server(BufReader::new(File::from(in_read)), out_write).unwrap();
        (peer, rx, handle, File::from(out_read), File::from(in_write))
    }

    #[test]
    fn eof_before_queued_request_preserves_output() {
        let (peer, _rx, reader, mut output, input) = server();
        let p = peer.clone();
        let notify = thread::spawn(move || {
            p.notify(
                "hold",
                serde_json::Value::String("x".repeat(256 * 1024)),
                None,
                None,
            )
        });
        let mut first = [0];
        output.read_exact(&mut first).unwrap();
        let p = peer.clone();
        let dispatch =
            thread::spawn(move || p.dispatch("queued", serde_json::Value::Null, None, None));
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock(&peer.0.peer.pending).requests.is_empty() {
            assert!(Instant::now() < deadline, "request was not registered");
            thread::yield_now();
        }
        drop(input);
        reader.join().unwrap();
        let mut notification = vec![first[0]];
        loop {
            let mut b = [0; 4096];
            let n = output.read(&mut b).unwrap();
            notification.extend_from_slice(&b[..n]);
            if notification.last() == Some(&b'\n') {
                break;
            }
        }
        notify.join().unwrap().unwrap();
        assert!(dispatch.join().unwrap().is_err());
        peer.notify("after", serde_json::Value::Null, None, None)
            .unwrap();
        peer.shutdown();
        let mut rest = String::new();
        output.read_to_string(&mut rest).unwrap();
        assert!(rest.contains("after"));
        assert!(!rest.contains("queued"));
    }

    #[test]
    fn eof_after_partial_request_closes_output() {
        let (peer, _rx, reader, mut output, input) = server();
        let p = peer.clone();
        let (done, result) = mpsc::channel();
        let dispatch = thread::spawn(move || {
            done.send(p.dispatch(
                "large",
                serde_json::Value::String("x".repeat(256 * 1024)),
                None,
                None,
            ))
            .unwrap();
        });
        let mut byte = [0];
        output.read_exact(&mut byte).unwrap();
        drop(input);
        reader.join().unwrap();
        let outcome = result.recv_timeout(Duration::from_secs(2));
        if outcome.is_err() {
            peer.shutdown();
        }
        assert!(outcome.expect("request did not stop after EOF").is_err());
        dispatch.join().unwrap();
        assert!(!peer.is_write_open());
        let mut rest = Vec::new();
        output.read_to_end(&mut rest).unwrap();
        assert!(rest.len() < 256 * 1024);
    }

    #[test]
    fn final_write_wins_even_with_early_response_and_eof() {
        let (peer, incoming, reader, mut output, mut input) = server();
        let (reached, wait) = mpsc::channel();
        let (resume, proceed) = mpsc::channel();
        peer.0.pipe.after_request_write(move || {
            reached.send(()).unwrap();
            proceed.recv().unwrap();
        });
        let p = peer.clone();
        let dispatch =
            thread::spawn(move || p.dispatch("test", serde_json::Value::Null, None, None));
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut BufReader::new(&mut output), &mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        writeln!(
            input,
            "{}",
            serde_json::json!({"id": request["id"], "result": 42})
        )
        .unwrap();
        let OrderedInbound::Barrier(barrier) =
            incoming.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected barrier")
        };
        barrier.acknowledge();
        drop(input);
        reader.join().unwrap();
        resume.send(()).unwrap();
        let handle = dispatch.join().unwrap().unwrap();
        assert_eq!(handle.wait(None, None).0.unwrap(), 42);
        peer.shutdown();
    }

    #[test]
    fn reader_error_reply_cap_counts_escaped_id_and_newline() {
        let (out_read, out_write) = nix::unistd::pipe().unwrap();
        let (in_read, in_write) = nix::unistd::pipe().unwrap();
        let (peer, _rx, reader) = connect_ordered_server_with_limits(
            BufReader::new(File::from(in_read)),
            out_write,
            ReadLimits {
                max_line_bytes: MAX_FRAME,
                inbound_capacity: 1,
            },
        )
        .unwrap();
        let mut output = File::from(out_read);
        let mut input = File::from(in_write);
        let text = format!(
            "{{\"id\":\"{}\",\"method\":0}}\n",
            "\\\\".repeat(4 * 1024 * 1024 - 32)
        );
        input.write_all(text.as_bytes()).unwrap();
        drop(input);
        reader.join().unwrap();
        assert_eq!(peer.read_status(), ReadStatus::Failed);
        let mut data = Vec::new();
        output.read_to_end(&mut data).unwrap();
        assert!(data.is_empty());
    }
}
