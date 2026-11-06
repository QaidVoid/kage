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

use crate::unix_pipe::{GuardedWriteError, PipeWriter};
use crate::{
    InboundSender, OrderedInbound, Peer, Pending, PendingReply, PendingRequest, ResponseBarrier,
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
    shutdown_complete: AtomicBool,
    #[cfg(test)]
    admission_blocked: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct Admission<'a>(&'a AtomicUsize);

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

impl Drop for State {
    fn drop(&mut self) {
        if !self.shutdown_complete.load(Ordering::Acquire) {
            self.shutting_down.store(true, Ordering::Release);
            self.pipe.signal_shutdown();
            close_pending(&self.peer.pending);
            self.pipe.shutdown();
        }
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
///
/// Outgoing requests and notifications omit `params` when passed
/// [`serde_json::Value::Null`]. Other values are sent unchanged without
/// validation, including nulls nested inside objects or arrays.
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
    receiver: Receiver<PendingReply>,
}

/// The reason a request could not be committed.
///
/// Clean EOF and cancellation mean no request bytes were written. They do not
/// guarantee the write side remains open after this outcome is returned.
#[derive(Debug)]
pub enum DispatchError {
    /// Clean server EOF prevented the first request byte.
    CleanEof(RpcError),
    /// Cancellation prevented the first request byte.
    Cancelled(RpcError),
    /// Any other dispatch failure, including a partially written frame.
    Other(RpcError),
}

impl From<DispatchError> for RpcError {
    fn from(error: DispatchError) -> Self {
        match error {
            DispatchError::CleanEof(error)
            | DispatchError::Cancelled(error)
            | DispatchError::Other(error) => error,
        }
    }
}

/// The origin of a committed request's completion.
pub enum RequestWaitOutcome {
    /// A valid response from the remote peer, including a JSON-RPC error.
    Delivered {
        /// The remote result or error.
        result: Result<serde_json::Value, RpcError>,
        /// The ordering barrier, absent if the inbound receiver was dropped.
        barrier: Option<ResponseBarrier>,
    },
    /// A locally detected error, including a deadline or invalid response.
    LocalError(RpcError),
    /// The caller's cancellation flag was triggered.
    Cancelled,
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
        match self.wait_typed(deadline, cancel) {
            RequestWaitOutcome::Delivered { result, barrier } => (result, barrier),
            RequestWaitOutcome::LocalError(error) => (Err(error), None),
            RequestWaitOutcome::Cancelled => {
                (Err(RpcError::new(-32800, "request cancelled")), None)
            }
        }
    }

    /// Wait for a response with explicit provenance.
    ///
    /// Valid remote successes and errors are delivered even when no barrier
    /// could be queued. Invalid responses, closure and deadlines are local
    /// errors. Cancellation takes priority over the deadline and a buffered
    /// response, and the deadline takes priority over a buffered response.
    /// Like [`Self::wait`], this never sends a cancellation notice.
    pub fn wait_typed(
        self,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> RequestWaitOutcome {
        let watch = cancel.map(CancelFlag::watch);
        let cancelled = watch
            .as_ref()
            .map_or_else(crossbeam_channel::never, |w| w.receiver().clone());
        let timer = deadline.map_or_else(crossbeam_channel::never, |d| {
            crossbeam_channel::after(d.saturating_duration_since(Instant::now()))
        });
        select_biased! {
            recv(cancelled) -> _ => RequestWaitOutcome::Cancelled,
            recv(timer) -> _ => RequestWaitOutcome::LocalError(RpcError::timed_out(&self.method)),
            recv(self.receiver) -> result => match result {
                Ok(PendingReply::Delivered((result, barrier))) => RequestWaitOutcome::Delivered { result, barrier },
                Ok(PendingReply::LocalError(error)) => RequestWaitOutcome::LocalError(error),
                Err(_) => RequestWaitOutcome::LocalError(closed()),
            },
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
                    reader_status.store(1, Ordering::Release);
                    *gate = true;
                    reader_eof.store(true, Ordering::Release);
                }
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
            shutdown_complete: AtomicBool::new(false),
            #[cfg(test)]
            admission_blocked: Mutex::new(None),
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

    /// Signal terminal shutdown using only atomic operations, without waiting
    /// for the writer or pending request locks.
    ///
    /// Writes stop cooperatively, but an in-flight final write may succeed.
    /// This signal alone does not settle pending responses, close an idle
    /// descriptor, or interrupt the reader or inbound queue. An active write
    /// may independently close the descriptor and settle pending requests.
    /// Call [`Self::shutdown`] to complete cleanup.
    pub fn signal_shutdown(&self) {
        self.0.shutting_down.store(true, Ordering::Release);
        self.0.pipe.signal_shutdown();
    }

    /// Permanently shut down the write side and settle all pending requests.
    /// This does not interrupt the separately owned reader or inbound queue.
    pub fn shutdown(&self) {
        self.signal_shutdown();
        close_pending(&self.0.peer.pending);
        self.0.pipe.shutdown();
        self.0.shutdown_complete.store(true, Ordering::Release);
    }

    fn ensure_writable(&self, allow_after_eof: bool) -> Result<(), DispatchError> {
        let pending_closed = lock(&self.0.peer.pending).closed;
        let eof = self.0.read_eof.load(Ordering::Acquire);
        if self.0.shutting_down.load(Ordering::Acquire)
            || self.0.pipe.is_closed()
            || (!eof && pending_closed)
        {
            return Err(DispatchError::Other(closed()));
        }
        if eof && !allow_after_eof {
            return Err(DispatchError::CleanEof(closed()));
        }
        Ok(())
    }

    fn admit(
        &self,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
        allow_after_eof: bool,
    ) -> Result<Admission<'_>, DispatchError> {
        loop {
            self.ensure_writable(allow_after_eof)?;
            if cancel.is_some_and(CancelFlag::is_cancelled) {
                return Err(DispatchError::Cancelled(RpcError::new(
                    -32800,
                    "write cancelled",
                )));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(DispatchError::Other(RpcError::new(
                    -32000,
                    "write timed out",
                )));
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
            #[cfg(test)]
            if let Some(hook) = lock(&self.0.admission_blocked).take() {
                hook();
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
    ) -> Result<(), DispatchError> {
        self.ensure_writable(allow_after_eof)?;
        let mut frame = CappedFrame(Vec::new());
        serde_json::to_writer(&mut frame, value)
            .map_err(|e| DispatchError::Other(RpcError::internal(format!("encode: {e}"))))?;
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
                let clean_eof = matches!(e, GuardedWriteError::CleanEof);
                let cancelled = matches!(e, GuardedWriteError::Cancelled);
                let e = e.into_io();
                let error = match e.kind() {
                    io::ErrorKind::Interrupted => RpcError::new(-32800, "write cancelled"),
                    io::ErrorKind::TimedOut => RpcError::new(-32000, "write timed out"),
                    _ => RpcError::internal(format!("write: {e}")),
                };
                Err(if clean_eof {
                    DispatchError::CleanEof(error)
                } else if cancelled {
                    DispatchError::Cancelled(error)
                } else {
                    DispatchError::Other(error)
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
            .map_err(Into::into)
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
            .map_err(Into::into)
    }

    /// Commit a request frame and return a handle for its ordered response.
    ///
    /// The deadline and cancellation cover writer admission and the complete
    /// write. If cancellation or timeout occurs after a partial frame, the
    /// transport closes and all pending requests fail. Cancellation or timeout
    /// before the first byte leaves the writer usable, but I/O errors close it.
    /// Pass the same absolute
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
        self.dispatch_typed(method, params, deadline, cancel)
            .map_err(Into::into)
    }

    /// Commit a request while preserving the origin of a dispatch failure.
    ///
    /// # Errors
    /// Returns a typed failure if the request frame was not committed.
    pub fn dispatch_typed(
        &self,
        method: &str,
        params: serde_json::Value,
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> Result<CommittedRequest, DispatchError> {
        let _admission = self.admit(deadline, cancel, false)?;
        check_method(method).map_err(DispatchError::Other)?;
        let id = self
            .0
            .peer
            .next_id
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .map_err(|_| DispatchError::Other(RpcError::internal("request IDs exhausted")))?;
        let (tx, receiver) = crossbeam_channel::bounded(1);
        {
            let mut pending = lock(&self.0.peer.pending);
            if self.0.shutting_down.load(Ordering::Acquire) || self.0.pipe.is_closed() {
                return Err(DispatchError::Other(closed()));
            }
            if self.0.read_eof.load(Ordering::Acquire) {
                return Err(DispatchError::CleanEof(closed()));
            }
            if pending.closed {
                return Err(DispatchError::Other(closed()));
            }
            if pending.requests.len() >= MAX_PENDING {
                return Err(DispatchError::Other(RpcError::internal(
                    "too many pending requests",
                )));
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
    if !params.is_null() {
        object.insert("params".into(), params);
    }
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
    fn signal_is_lock_free_and_shutdown_completes_cleanup() {
        let (peer, _incoming, reader, mut agent_in, agent_out) = fixture();
        let pending = peer
            .dispatch("pending", serde_json::Value::Null, None, None)
            .unwrap();
        read_message(&mut agent_in);
        let writer_guard = peer.0.pipe.lock_for_test();
        let pending_guard = lock(&peer.0.peer.pending);
        let signal_peer = peer.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            signal_peer.signal_shutdown();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(2))
            .expect("signal waited for a lock");
        assert!(!peer.is_write_open());
        assert!(writer_guard.is_some());
        assert!(!pending_guard.closed);
        assert!(pending.receiver.is_empty());
        drop(pending_guard);
        drop(writer_guard);
        worker.join().unwrap();
        assert!(
            peer.notify("after", serde_json::Value::Null, None, None)
                .is_err()
        );
        peer.shutdown();
        assert_eq!(pending.wait(None, None).0.unwrap_err(), closed());
        let mut rest = Vec::new();
        agent_in.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn last_drop_after_signal_still_cleans_up() {
        let (peer, incoming, reader, mut agent_in, agent_out) = fixture();
        let pending = peer
            .dispatch("pending", serde_json::Value::Null, None, None)
            .unwrap();
        read_message(&mut agent_in);
        peer.signal_shutdown();
        assert!(!peer.0.shutdown_complete.load(Ordering::Acquire));
        drop(peer);
        assert_eq!(pending.wait(None, None).0.unwrap_err(), closed());
        let mut rest = Vec::new();
        agent_in.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        drop(incoming);
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn completed_shutdown_last_drop_avoids_transport_locks() {
        let (peer, incoming, reader, _agent_in, agent_out) = fixture();
        peer.shutdown();
        let pending = Arc::clone(&peer.0.peer.pending);
        let pipe = Arc::clone(&peer.0.pipe);
        let pending_guard = lock(&pending);
        let writer_guard = pipe.lock_for_test();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            drop(peer);
            tx.send(()).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(2));
        drop(writer_guard);
        drop(pending_guard);
        worker.join().unwrap();
        result.expect("last drop waited for a transport lock");
        drop(incoming);
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn concurrent_shutdown_waits_for_full_cleanup() {
        let (peer, incoming, reader, _agent_in, agent_out) = fixture();
        let pipe = Arc::clone(&peer.0.pipe);
        let guard = pipe.lock_for_test();
        let first = peer.clone();
        let (tx, rx) = mpsc::channel();
        let tx2 = tx.clone();
        let one = thread::spawn(move || {
            first.shutdown();
            tx.send(()).unwrap();
        });
        let limit = Instant::now() + Duration::from_secs(2);
        while !lock(&peer.0.peer.pending).closed {
            assert!(Instant::now() < limit);
            thread::yield_now();
        }
        assert!(!peer.0.shutdown_complete.load(Ordering::Acquire));
        let second = peer.clone();
        let two = thread::spawn(move || {
            second.shutdown();
            tx2.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(!peer.0.shutdown_complete.load(Ordering::Acquire));
        drop(guard);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        one.join().unwrap();
        two.join().unwrap();
        assert!(peer.0.shutdown_complete.load(Ordering::Acquire));
        drop(incoming);
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn typed_wait_distinguishes_remote_errors_from_local_failures() {
        let (peer, incoming, reader, mut agent_in, mut agent_out) = fixture();
        for code in [-32800, -32000, -32600, -32603] {
            let request = peer
                .dispatch("test", serde_json::Value::Null, None, None)
                .unwrap();
            let id = read_message(&mut agent_in)["id"].clone();
            writeln!(agent_out, "{}", serde_json::json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":"remote"}})).unwrap();
            let OrderedInbound::Barrier(marker) =
                incoming.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("expected barrier")
            };
            marker.acknowledge();
            match request.wait_typed(Some(Instant::now() + Duration::from_secs(2)), None) {
                RequestWaitOutcome::Delivered {
                    result: Err(error),
                    barrier: Some(barrier),
                } => {
                    assert_eq!(error.code, code);
                    barrier.wait_timeout(Duration::from_secs(2)).unwrap();
                }
                _ => panic!("expected remote error"),
            }
        }
        let request = peer
            .dispatch("test", serde_json::Value::Null, None, None)
            .unwrap();
        let id = read_message(&mut agent_in)["id"].clone();
        writeln!(agent_out, "{}", serde_json::json!({"id":id, "result":true})).unwrap();
        assert!(
            matches!(request.wait_typed(Some(Instant::now() + Duration::from_secs(2)), None), RequestWaitOutcome::LocalError(error) if error.code == -32600)
        );
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn typed_wait_reports_cancellation_deadline_and_close() {
        let (peer, _incoming, reader, mut agent_in, agent_out) = fixture();
        let cancel = CancelFlag::new();
        cancel.cancel();
        let request = peer
            .dispatch("cancel", serde_json::Value::Null, None, None)
            .unwrap();
        read_message(&mut agent_in);
        assert!(matches!(
            request.wait_typed(Some(Instant::now()), Some(&cancel)),
            RequestWaitOutcome::Cancelled
        ));
        let request = peer
            .dispatch("timeout", serde_json::Value::Null, None, None)
            .unwrap();
        read_message(&mut agent_in);
        assert!(
            matches!(request.wait_typed(Some(Instant::now()), None), RequestWaitOutcome::LocalError(error) if error == RpcError::timed_out("timeout"))
        );
        let request = peer
            .dispatch("close", serde_json::Value::Null, None, None)
            .unwrap();
        read_message(&mut agent_in);
        peer.shutdown();
        assert!(
            matches!(request.wait_typed(Some(Instant::now() + Duration::from_secs(2)), None), RequestWaitOutcome::LocalError(error) if error == closed())
        );
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn typed_wait_delivers_without_barrier_when_receiver_is_dropped() {
        let (peer, incoming, reader, mut agent_in, mut agent_out) = fixture();
        let request = peer
            .dispatch("test", serde_json::Value::Null, None, None)
            .unwrap();
        let id = read_message(&mut agent_in)["id"].clone();
        drop(incoming);
        writeln!(
            agent_out,
            "{}",
            serde_json::json!({"jsonrpc":"2.0", "id":id, "result":42})
        )
        .unwrap();
        assert!(
            matches!(request.wait_typed(Some(Instant::now() + Duration::from_secs(2)), None), RequestWaitOutcome::Delivered { result: Ok(value), barrier: None } if value == 42)
        );
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn typed_wait_prioritizes_cancellation_and_deadline_over_buffered_response() {
        let (peer, incoming, reader, mut agent_in, mut agent_out) = fixture();
        for cancelled in [true, false] {
            let request = peer
                .dispatch("priority", serde_json::Value::Null, None, None)
                .unwrap();
            let id = read_message(&mut agent_in)["id"].clone();
            writeln!(
                agent_out,
                "{}",
                serde_json::json!({"jsonrpc":"2.0", "id":id, "result":42})
            )
            .unwrap();
            let OrderedInbound::Barrier(marker) =
                incoming.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("expected barrier")
            };
            let limit = Instant::now() + Duration::from_secs(2);
            while request.receiver.is_empty() {
                assert!(Instant::now() < limit, "response was not buffered");
                thread::yield_now();
            }
            marker.acknowledge();
            let cancel = CancelFlag::new();
            if cancelled {
                cancel.cancel();
            }
            let outcome = request.wait_typed(Some(Instant::now()), Some(&cancel));
            if cancelled {
                assert!(matches!(outcome, RequestWaitOutcome::Cancelled));
            } else {
                assert!(
                    matches!(outcome, RequestWaitOutcome::LocalError(error) if error == RpcError::timed_out("priority"))
                );
            }
        }
        peer.shutdown();
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn tuple_wait_preserves_remote_error_and_local_cancellation() {
        let (peer, incoming, reader, mut agent_in, mut agent_out) = fixture();
        let request = peer
            .dispatch("remote", serde_json::Value::Null, None, None)
            .unwrap();
        let id = read_message(&mut agent_in)["id"].clone();
        writeln!(agent_out, "{}", serde_json::json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32800, "message":"remote"}})).unwrap();
        let OrderedInbound::Barrier(marker) =
            incoming.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("expected barrier")
        };
        marker.acknowledge();
        let (result, barrier) = request.wait(Some(Instant::now() + Duration::from_secs(2)), None);
        assert_eq!(result.unwrap_err(), RpcError::new(-32800, "remote"));
        barrier
            .unwrap()
            .wait_timeout(Duration::from_secs(2))
            .unwrap();

        let request = peer
            .dispatch("cancel", serde_json::Value::Null, None, None)
            .unwrap();
        read_message(&mut agent_in);
        let cancel = CancelFlag::new();
        cancel.cancel();
        let (result, barrier) =
            request.wait(Some(Instant::now() + Duration::from_secs(2)), Some(&cancel));
        assert_eq!(
            result.unwrap_err(),
            RpcError::new(-32800, "request cancelled")
        );
        assert!(barrier.is_none());
        peer.shutdown();
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn outbound_params_and_null_result_wire_frames() {
        let (peer, incoming, reader, mut agent_in, mut agent_out) = fixture();
        for params in [
            serde_json::Value::Null,
            serde_json::json!({"nested": null}),
            serde_json::json!([null, {"nested": null}]),
            serde_json::json!(42),
        ] {
            peer.notify("notice", params.clone(), None, None).unwrap();
            let notice = read_message(&mut agent_in);
            let mut expected = serde_json::json!({"jsonrpc": "2.0", "method": "notice"});
            if !params.is_null() {
                expected["params"] = params.clone();
            }
            assert_eq!(notice, expected);

            let request = peer.dispatch("call", params.clone(), None, None).unwrap();
            let frame = read_message(&mut agent_in);
            let id = frame["id"].clone();
            let mut expected = serde_json::json!({"jsonrpc": "2.0", "method": "call", "id": id});
            if !params.is_null() {
                expected["params"] = params;
            }
            assert_eq!(frame, expected);
            writeln!(
                agent_out,
                "{}",
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null})
            )
            .unwrap();
            let OrderedInbound::Barrier(marker) =
                incoming.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("expected barrier");
            };
            marker.acknowledge();
            let (result, barrier) =
                request.wait(Some(Instant::now() + Duration::from_secs(2)), None);
            assert_eq!(result.unwrap(), serde_json::Value::Null);
            barrier
                .unwrap()
                .wait_timeout(Duration::from_secs(2))
                .unwrap();
        }
        peer.respond(
            &serde_json::json!(7),
            Ok(serde_json::Value::Null),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            read_message(&mut agent_in),
            serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": null})
        );
        peer.shutdown();
        drop(agent_out);
        reader.join().unwrap();
    }

    #[test]
    fn cancellation_and_deadline_take_priority_over_buffered_response() {
        let (peer, incoming, reader, mut agent_in, mut agent_out) = fixture();
        for cancelled in [false, true] {
            let request = peer
                .dispatch("test", serde_json::Value::Null, None, None)
                .unwrap();
            let id = read_message(&mut agent_in)["id"].clone();
            writeln!(
                agent_out,
                "{}",
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": 42})
            )
            .unwrap();
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
            serde_json::json!({"jsonrpc":false, "id": first_id, "result": "ignored"})
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
            serde_json::json!({"id":1, "result":true}),
            serde_json::json!({"jsonrpc":null, "id":1, "result":true}),
            serde_json::json!({"jsonrpc":2, "id":1, "result":true}),
            serde_json::json!({"jsonrpc":"1.0", "id":1, "result":true}),
            serde_json::json!({"jsonrpc":"2.0", "id":1}),
            serde_json::json!({"jsonrpc":"2.0", "id":1, "result":true, "error":{"code":-1, "message":"bad"}}),
            serde_json::json!({"jsonrpc":"2.0", "id":1, "error":{"code":"bad", "message":"bad"}}),
        ] {
            let (peer, _incoming, reader, mut agent_in, mut agent_out) = fixture();
            let pending = peer
                .dispatch("pending", serde_json::Value::Null, None, None)
                .unwrap();
            assert_eq!(read_message(&mut agent_in)["id"], 1);
            let other = peer
                .dispatch("other", serde_json::Value::Null, None, None)
                .unwrap();
            assert_eq!(read_message(&mut agent_in)["id"], 2);
            writeln!(agent_out, "{response}").unwrap();
            let (result, barrier) =
                pending.wait(Some(Instant::now() + Duration::from_secs(2)), None);
            assert_eq!(result.unwrap_err().code, -32600);
            assert!(barrier.is_none());
            reader.join().unwrap();
            assert_eq!(other.wait(None, None).0.unwrap_err(), closed());
            assert_eq!(peer.read_status(), ReadStatus::Failed);
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
            thread::spawn(move || p.dispatch_typed("queued", serde_json::Value::Null, None, None));
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
        assert!(matches!(
            dispatch.join().unwrap(),
            Err(DispatchError::CleanEof(_))
        ));
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
            done.send(p.dispatch_typed(
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
        assert!(matches!(
            outcome.expect("request did not stop after EOF"),
            Err(DispatchError::Other(_))
        ));
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
            thread::spawn(move || p.dispatch_typed("test", serde_json::Value::Null, None, None));
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut BufReader::new(&mut output), &mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        writeln!(
            input,
            "{}",
            serde_json::json!({"jsonrpc":"2.0", "id": request["id"], "result": 42})
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
    fn signal_after_server_eof_stops_preserved_output() {
        let (peer, _rx, reader, mut output, input) = server();
        drop(input);
        reader.join().unwrap();
        assert_eq!(peer.read_status(), ReadStatus::CleanEof);
        assert!(matches!(
            peer.dispatch_typed("before", serde_json::Value::Null, None, None),
            Err(DispatchError::CleanEof(_))
        ));
        peer.signal_shutdown();
        assert!(matches!(
            peer.dispatch_typed("after", serde_json::Value::Null, None, None),
            Err(DispatchError::Other(_))
        ));
        assert!(
            peer.notify("after", serde_json::Value::Null, None, None)
                .is_err()
        );
        peer.shutdown();
        let mut rest = Vec::new();
        output.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    }

    #[test]
    fn signal_stops_admission_waiter() {
        let (peer, _rx, reader, mut output, input) = server();
        let permits: Vec<_> = (0..MAX_ADMITTED)
            .map(|_| peer.admit(None, None, false).unwrap())
            .collect();
        let (entered, waiting) = mpsc::channel();
        *lock(&peer.0.admission_blocked) = Some(Box::new(move || {
            entered.send(()).unwrap();
        }));
        let worker_peer = peer.clone();
        let (done, result) = mpsc::channel();
        let worker = thread::spawn(move || {
            done.send(worker_peer.dispatch_typed("blocked", serde_json::Value::Null, None, None))
                .unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        peer.signal_shutdown();
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(DispatchError::Other(_))
        ));
        worker.join().unwrap();
        drop(permits);
        peer.shutdown();
        let mut rest = Vec::new();
        output.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        drop(input);
        reader.join().unwrap();
    }

    #[test]
    fn completed_final_write_can_commit_after_signal() {
        let (peer, _rx, reader, mut output, input) = server();
        let (reached, waiting) = mpsc::channel();
        let (resume, proceed) = mpsc::channel();
        peer.0.pipe.after_request_write(move || {
            reached.send(()).unwrap();
            proceed.recv().unwrap();
        });
        let worker_peer = peer.clone();
        let worker = thread::spawn(move || {
            worker_peer.dispatch_typed("test", serde_json::Value::Null, None, None)
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        peer.signal_shutdown();
        resume.send(()).unwrap();
        let request = worker.join().unwrap().unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut BufReader::new(&mut output), &mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["method"],
            "test"
        );
        assert!(request.receiver.is_empty());
        peer.shutdown();
        assert_eq!(request.wait(None, None).0.unwrap_err(), closed());
        drop(input);
        reader.join().unwrap();
    }

    #[test]
    fn cancellation_while_admission_is_full_is_nonfatal() {
        let (peer, _rx, reader, mut output, input) = server();
        let permits: Vec<_> = (0..MAX_ADMITTED)
            .map(|_| peer.admit(None, None, false).unwrap())
            .collect();
        let (entered, waiting) = mpsc::channel();
        *lock(&peer.0.admission_blocked) = Some(Box::new(move || {
            entered.send(()).unwrap();
        }));
        let cancel = CancelFlag::new();
        let worker_cancel = cancel.clone();
        let worker_peer = peer.clone();
        let (done, result) = mpsc::channel();
        let worker = thread::spawn(move || {
            done.send(worker_peer.dispatch_typed(
                "blocked",
                serde_json::Value::Null,
                Some(Instant::now() + Duration::from_secs(2)),
                Some(&worker_cancel),
            ))
            .unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        cancel.cancel();
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(DispatchError::Cancelled(_))
        ));
        worker.join().unwrap();
        drop(permits);
        peer.notify(
            "after",
            serde_json::Value::Null,
            Some(Instant::now() + Duration::from_secs(1)),
            None,
        )
        .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut BufReader::new(&mut output), &mut line).unwrap();
        assert!(line.contains("after"));
        peer.shutdown();
        drop(input);
        reader.join().unwrap();
    }

    #[test]
    fn eof_while_admission_is_full_is_nonfatal() {
        let (peer, _rx, reader, mut output, input) = server();
        let permits: Vec<_> = (0..MAX_ADMITTED)
            .map(|_| peer.admit(None, None, false).unwrap())
            .collect();
        let (entered, waiting) = mpsc::channel();
        *lock(&peer.0.admission_blocked) = Some(Box::new(move || {
            entered.send(()).unwrap();
        }));
        let worker_peer = peer.clone();
        let (done, result) = mpsc::channel();
        let worker = thread::spawn(move || {
            done.send(worker_peer.dispatch_typed(
                "blocked",
                serde_json::Value::Null,
                Some(Instant::now() + Duration::from_secs(2)),
                None,
            ))
            .unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(input);
        reader.join().unwrap();
        assert_eq!(peer.read_status(), ReadStatus::CleanEof);
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(DispatchError::CleanEof(_))
        ));
        worker.join().unwrap();
        drop(permits);
        peer.notify(
            "after",
            serde_json::Value::Null,
            Some(Instant::now() + Duration::from_secs(1)),
            None,
        )
        .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut BufReader::new(&mut output), &mut line).unwrap();
        assert!(line.contains("after"));
        peer.shutdown();
    }

    #[test]
    fn cancellation_queued_for_writer_is_nonfatal() {
        let (peer, _rx, reader, mut output, input) = server();
        let guard = peer.0.pipe.lock_for_test();
        let cancel = CancelFlag::new();
        let worker_cancel = cancel.clone();
        let worker_peer = peer.clone();
        let (done, result) = mpsc::channel();
        let worker = thread::spawn(move || {
            done.send(worker_peer.dispatch_typed(
                "queued",
                serde_json::Value::Null,
                Some(Instant::now() + Duration::from_secs(2)),
                Some(&worker_cancel),
            ))
            .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while lock(&peer.0.peer.pending).requests.is_empty() {
            assert!(Instant::now() < deadline, "dispatch did not register");
            thread::yield_now();
        }
        cancel.cancel();
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(DispatchError::Cancelled(_))
        ));
        worker.join().unwrap();
        drop(guard);
        peer.notify(
            "after",
            serde_json::Value::Null,
            Some(Instant::now() + Duration::from_secs(1)),
            None,
        )
        .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut BufReader::new(&mut output), &mut line).unwrap();
        assert!(line.contains("after"));
        peer.shutdown();
        drop(input);
        reader.join().unwrap();
    }

    #[test]
    fn dispatch_classifies_cancellation_and_timeout() {
        let (peer, _rx, reader, _output, input) = server();
        let cancel = CancelFlag::new();
        cancel.cancel();
        assert!(matches!(
            peer.dispatch_typed("cancel", serde_json::Value::Null, None, Some(&cancel)),
            Err(DispatchError::Cancelled(_))
        ));
        assert!(peer.is_write_open());
        assert!(matches!(
            peer.dispatch_typed(
                "timeout",
                serde_json::Value::Null,
                Some(Instant::now()),
                None
            ),
            Err(DispatchError::Other(_))
        ));
        assert!(peer.is_write_open());
        peer.shutdown();
        drop(input);
        reader.join().unwrap();
    }

    #[test]
    fn dispatch_classifies_partial_cancellation() {
        use nix::errno::Errno;
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        use nix::poll::{PollFd, PollFlags, poll};
        use std::os::fd::AsFd;

        let (out_read, out_write) = nix::unistd::pipe().unwrap();
        let flags = OFlag::from_bits_truncate(fcntl(&out_write, FcntlArg::F_GETFL).unwrap());
        fcntl(&out_write, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).unwrap();
        let mut filled = 0;
        for bytes in [&[b'x'; 4096][..], &[b'x'][..]] {
            loop {
                match nix::unistd::write(&out_write, bytes) {
                    Ok(n) => filled += n,
                    Err(Errno::EAGAIN) => break,
                    Err(error) => panic!("fill pipe: {error}"),
                }
            }
        }
        let mut output = File::from(out_read);
        let drained = filled.min(4096);
        assert!(drained > 0);
        output.read_exact(&mut vec![0; drained]).unwrap();
        let probe = nix::unistd::dup(&out_write).unwrap();
        let writable = || {
            let mut fds = [PollFd::new(probe.as_fd(), PollFlags::POLLOUT)];
            poll(&mut fds, 0u8).unwrap() > 0
        };
        assert!(writable(), "draining did not make the pipe writable");
        let (in_read, in_write) = nix::unistd::pipe().unwrap();
        let (peer, _rx, reader) =
            connect_ordered_server(BufReader::new(File::from(in_read)), out_write).unwrap();
        let input = File::from(in_write);
        let cancel = CancelFlag::new();
        let cancel_write = cancel.clone();
        let p = peer.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = tx.send(p.dispatch_typed(
                "large",
                serde_json::Value::String("x".repeat(256 * 1024)),
                Some(Instant::now() + Duration::from_secs(3)),
                Some(&cancel_write),
            ));
        });
        let limit = Instant::now() + Duration::from_secs(2);
        let observed = loop {
            if !writable() {
                break true;
            }
            if Instant::now() >= limit {
                break false;
            }
            thread::yield_now();
        };
        cancel.cancel();
        let outcome = rx.recv_timeout(Duration::from_secs(2));
        if !observed || outcome.is_err() {
            peer.shutdown();
        }
        drop(input);
        reader.join().unwrap();
        assert!(observed, "request did not write into the pipe");
        assert!(matches!(outcome.unwrap(), Err(DispatchError::Other(_))));
        assert!(!peer.is_write_open());
        worker.join().unwrap();
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
