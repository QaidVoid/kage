//! The upgraded connection: WebSocket frames in, JSON-RPC lines out.
//!
//! [`upgrade`] writes the `101` response and hands back the
//! newline-delimited reader and writer pair the agent serving loop
//! consumes. Two threads share one socket. The reader turns text
//! frames into lines on a bounded in-process stream, replacing raw newlines so
//! a frame always arrives as one message, dropping binary frames, and
//! closing on a message past the inbound cap. The writer drains a
//! byte-capped outgoing queue into text frames, pings on an interval,
//! and closes a connection that stops answering. Feeding the queue
//! never blocks: once the cap is passed, the flush fails and the
//! socket closes.

use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Select, Sender, TryRecvError, TrySendError, bounded};
use tungstenite::Error as WsError;
use tungstenite::Message;
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::{Role, WebSocket, WebSocketConfig};

use crate::head::Head;
use crate::{IDLE_TIMEOUT, MAX_MESSAGE, OUTGOING_CAP, PING_INTERVAL};

/// How often the reader wakes while the peer is silent, so a dying
/// connection is noticed without busy-looping.
const READER_TICK: Duration = Duration::from_secs(5);

/// How long one socket write may make no progress before the peer is
/// declared gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Write buffer allowed behind a stalled socket before sends fail.
/// Wide enough for several messages of the inbound cap, so a momentary
/// slow reader is absorbed while a truly stuck one is cut off.
const WIRE_BUFFER_CAP: usize = MAX_MESSAGE * 4 + 1024 * 1024;

/// Message-count backstop beside the [`OUTGOING_CAP`] byte limit.
const OUTGOING_MESSAGES: usize = 1024;

/// How many reader-produced byte blobs, pong and close echoes, may
/// queue for the writer thread.
const WIRE_BLOBS: usize = 64;

const INBOUND_MESSAGES: usize = 4;

const DRAIN_BATCH: usize = 16;

/// How long the reader waits after a peer close for the writer to
/// push the echo.
const CLOSE_ECHO_GRACE: Duration = Duration::from_millis(50);

/// Writes the `101` response for an authorized upgrade request and
/// returns the connection as the reader and writer pair the agent
/// serving loop consumes.
///
/// The reader yields one line per text frame, with raw newlines
/// replaced. The writer buffers bytes through [`Write`] and publishes
/// each [`Write::flush`] as one text frame. `subprotocol` is the
/// accepted `Sec-WebSocket-Protocol` entry, echoed when present.
/// Dropping the reader aborts the connection, shuts down the socket, and joins
/// the transport workers. Queued outgoing messages may be discarded.
/// `connection_id` goes out as `Acp-Connection-Id`. The frame bytes
/// read past the request head, [`Head::leftover`], seed the decoder so
/// an early frame is never lost.
///
/// # Errors
///
/// Fails when the request carries no `Sec-WebSocket-Key`, when the
/// response cannot be written, or when the socket cannot be cloned or
/// the worker threads spawned.
pub fn upgrade(
    stream: &mut TcpStream,
    head: Head,
    connection_id: &str,
    subprotocol: Option<&str>,
) -> io::Result<(BufReader<Incoming>, Outgoing)> {
    send_upgrade_response(stream, &head, connection_id, subprotocol)?;

    stream.set_read_timeout(Some(READER_TICK))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;

    let sock = Arc::new(stream.try_clone()?);
    let (blob_tx, blob_rx) = bounded::<Vec<u8>>(WIRE_BLOBS);
    let (cancel_tx, cancel_rx) = bounded::<()>(0);
    let stop = Arc::new(Stop {
        sock: Arc::clone(&sock),
        cancel: Mutex::new(Some(cancel_tx)),
    });
    let mut ws_read = WebSocket::from_partially_read(
        ReaderWire {
            sock: Arc::clone(&sock),
            blobs: blob_tx,
            cancel: cancel_rx.clone(),
        },
        head.leftover,
        Role::Server,
        Some(config()),
    );
    let ws_write = WebSocket::from_raw_socket(
        WriterWire {
            sock: Arc::clone(&sock),
        },
        Role::Server,
        Some(config()),
    );

    let (inbound_tx, inbound_rx) = bounded(INBOUND_MESSAGES);
    let (tx, rx) = bounded::<Vec<u8>>(OUTGOING_MESSAGES);
    let bytes = Arc::new(Mutex::new(0));
    let queue = Queue {
        tx,
        bytes: Arc::clone(&bytes),
    };
    let epoch = Instant::now();
    let last_inbound = Arc::new(AtomicU64::new(0));

    let reader = {
        let worker_stop = Arc::clone(&stop);
        let cancel = cancel_rx.clone();
        let last_inbound = Arc::clone(&last_inbound);
        match thread::Builder::new()
            .name("kage-remote-read".to_owned())
            .spawn(move || {
                read_loop(
                    &mut ws_read,
                    &inbound_tx,
                    &worker_stop,
                    &cancel,
                    epoch,
                    &last_inbound,
                );
            }) {
            Ok(handle) => handle,
            Err(error) => {
                stop.shutdown();
                return Err(error);
            }
        }
    };
    let writer = {
        let worker_stop = Arc::clone(&stop);
        let cancel = cancel_rx.clone();
        let last_inbound = Arc::clone(&last_inbound);
        match thread::Builder::new()
            .name("kage-remote-write".to_owned())
            .spawn(move || {
                write_loop(
                    ws_write,
                    &blob_rx,
                    &rx,
                    &bytes,
                    &worker_stop,
                    &cancel,
                    (epoch, &last_inbound),
                );
            }) {
            Ok(handle) => handle,
            Err(error) => {
                stop.shutdown();
                let _ = reader.join();
                return Err(error);
            }
        }
    };

    let outgoing = Outgoing {
        buffer: Vec::new(),
        queue,
        stop: Arc::clone(&stop),
        failed: false,
    };
    Ok((
        BufReader::new(Incoming {
            rx: inbound_rx,
            current: Vec::new(),
            offset: 0,
            stop,
            workers: vec![reader, writer],
        }),
        outgoing,
    ))
}

fn send_upgrade_response(
    stream: &mut TcpStream,
    head: &Head,
    connection_id: &str,
    subprotocol: Option<&str>,
) -> io::Result<()> {
    let key = head.header("sec-websocket-key").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "request has no Sec-WebSocket-Key",
        )
    })?;
    let accept = derive_accept_key(key.as_bytes());
    let mut response = String::with_capacity(160);
    response.push_str(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: ",
    );
    response.push_str(&accept);
    response.push_str("\r\n");
    if let Some(entry) = subprotocol {
        response.push_str("Sec-WebSocket-Protocol: ");
        response.push_str(entry);
        response.push_str("\r\n");
    }
    response.push_str("Acp-Connection-Id: ");
    response.push_str(connection_id);
    response.push_str("\r\n\r\n");
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

struct Stop {
    sock: Arc<TcpStream>,
    cancel: Mutex<Option<Sender<()>>>,
}

impl Stop {
    fn shutdown(&self) {
        self.cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

/// A bounded stream of newline-delimited messages that owns the transport workers.
///
/// Dropping this reader aborts the transport and joins both worker threads.
/// Queued outgoing messages may be discarded, including messages whose writer
/// has already been flushed or dropped.
pub struct Incoming {
    rx: Receiver<Vec<u8>>,
    current: Vec<u8>,
    offset: usize,
    stop: Arc<Stop>,
    workers: Vec<JoinHandle<()>>,
}

impl Read for Incoming {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.offset == self.current.len() {
            match self.rx.recv() {
                Ok(bytes) => {
                    self.current = bytes;
                    self.offset = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let count = buf.len().min(self.current.len() - self.offset);
        buf[..count].copy_from_slice(&self.current[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        self.stop.shutdown();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// The write half handed to the agent: bytes buffer in memory and each
/// [`Write::flush`] becomes one text frame.
///
/// Feeding the queue never blocks. When the byte cap is passed or the
/// connection is gone, the flush fails and the socket closes, so a
/// stalled reader is cut off instead of wedging the agent. A successful flush
/// means the message entered the queue, not that it reached the peer. Dropping
/// this writer lets the transport drain queued messages before shutting down,
/// unless the reader is dropped or another transport failure aborts it first.
pub struct Outgoing {
    buffer: Vec<u8>,
    queue: Queue,
    stop: Arc<Stop>,
    failed: bool,
}

impl Write for Outgoing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection is closed",
            ));
        }
        let current = u64::try_from(self.buffer.len()).unwrap_or(u64::MAX);
        let added = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if current
            .checked_add(added)
            .is_none_or(|total| total > OUTGOING_CAP)
        {
            self.buffer.clear();
            return Err(self.cut(io::ErrorKind::WriteZero, "outgoing frame is too large"));
        }
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection is closed",
            ));
        }
        if self.buffer.is_empty() {
            return Ok(());
        }
        let message = std::mem::take(&mut self.buffer);
        match self.queue.push(message) {
            Ok(()) => Ok(()),
            Err(PushError::Full) => {
                Err(self.cut(io::ErrorKind::WriteZero, "outgoing queue is full"))
            }
            Err(PushError::Closed) => {
                Err(self.cut(io::ErrorKind::BrokenPipe, "connection is closed"))
            }
        }
    }
}

impl Outgoing {
    fn cut(&mut self, kind: io::ErrorKind, message: &str) -> io::Error {
        self.failed = true;
        self.stop.shutdown();
        io::Error::new(kind, message)
    }
}

struct Queue {
    tx: Sender<Vec<u8>>,
    bytes: Arc<Mutex<u64>>,
}

enum PushError {
    Full,
    Closed,
}

impl Queue {
    fn push(&self, message: Vec<u8>) -> Result<(), PushError> {
        let len = u64::try_from(message.len()).unwrap_or(u64::MAX);
        let mut queued = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
        if *queued + len > OUTGOING_CAP {
            return Err(PushError::Full);
        }
        match self.tx.try_send(message) {
            Ok(()) => {
                *queued += len;
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(PushError::Full),
            Err(TrySendError::Disconnected(_)) => Err(PushError::Closed),
        }
    }
}

/// The reader thread's view of the socket.
///
/// Reads come straight off the socket. Writes never touch it: the
/// bytes tungstenite produces on its own, an automatic pong or a close
/// echo, are handed to the writer thread, so one thread owns every
/// byte on the wire and frames can never interleave.
struct ReaderWire {
    sock: Arc<TcpStream>,
    blobs: Sender<Vec<u8>>,
    cancel: Receiver<()>,
}

impl Read for ReaderWire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.sock).read(buf)
    }
}

impl Write for ReaderWire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        crossbeam_channel::select! {
            send(self.blobs, buf.to_vec()) -> result => result.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer thread is gone"))?,
            recv(self.cancel) -> _ => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection is closed")),
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The writer thread's view of the socket: the only writer.
struct WriterWire {
    sock: Arc<TcpStream>,
}

impl Read for WriterWire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.sock).read(buf)
    }
}

impl Write for WriterWire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.sock).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self.sock).flush()
    }
}

fn config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
        .max_write_buffer_size(WIRE_BUFFER_CAP)
}

fn read_loop(
    ws: &mut WebSocket<ReaderWire>,
    inbound: &Sender<Vec<u8>>,
    stop: &Stop,
    cancel: &Receiver<()>,
    epoch: Instant,
    last_inbound: &AtomicU64,
) {
    loop {
        let received = ws.read();
        match received {
            Ok(Message::Text(text)) => {
                last_inbound.store(millis_since(epoch), Ordering::Release);
                // The JSON-RPC line limit includes the appended newline.
                if text.len() >= MAX_MESSAGE {
                    break;
                }
                if text.contains('\n') {
                    eprintln!(
                        "kage-remote: frame carried a raw newline; replaced so the frame stays \
                         one message"
                    );
                }
                let mut line = text.as_str().replace('\n', " ");
                line.push('\n');
                crossbeam_channel::select! {
                    send(inbound, line.into_bytes()) -> result => if result.is_err() { break; },
                    recv(cancel) -> _ => break,
                }
            }
            Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {
                last_inbound.store(millis_since(epoch), Ordering::Release);
            }
            Ok(Message::Close(_)) => {
                let _ = ws.flush();
                // Give the writer thread a moment to push the close
                // echo before the socket goes down.
                thread::sleep(CLOSE_ECHO_GRACE);
                break;
            }
            Err(WsError::Io(e))
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            }
            Err(_) => break,
        }
    }
    stop.shutdown();
}

fn write_loop(
    mut ws: WebSocket<WriterWire>,
    blobs: &Receiver<Vec<u8>>,
    rx: &Receiver<Vec<u8>>,
    bytes: &Mutex<u64>,
    stop: &Stop,
    cancel: &Receiver<()>,
    clock: (Instant, &AtomicU64),
) {
    let (epoch, last_inbound) = clock;
    let mut last_ping = Instant::now();
    let mut select = Select::new();
    let cancel_op = select.recv(cancel);
    let blobs_op = select.recv(blobs);
    let queue_op = select.recv(rx);
    loop {
        if cancel
            .try_recv()
            .is_err_and(|e| e == TryRecvError::Disconnected)
        {
            break;
        }
        // Give control frames priority without starving messages or cancellation.
        let mut blobs_dead = false;
        for _ in 0..DRAIN_BATCH {
            let Ok(blob) = blobs.try_recv() else { break };
            if (&*stop.sock).write_all(&blob).is_err() {
                blobs_dead = true;
                break;
            }
        }
        if blobs_dead
            || cancel.try_recv() == Err(TryRecvError::Disconnected)
            || drain(&mut ws, rx, bytes)
        {
            break;
        }
        let quiet = millis_since(epoch).saturating_sub(last_inbound.load(Ordering::Acquire));
        if Duration::from_millis(quiet) >= IDLE_TIMEOUT {
            let _ = ws.close(None);
            break;
        }
        if last_ping.elapsed() >= PING_INTERVAL {
            let alive = ws.send(Message::Ping(tungstenite::Bytes::new())).is_ok();
            last_ping = Instant::now();
            if !alive {
                break;
            }
        }
        let wait = PING_INTERVAL
            .saturating_sub(last_ping.elapsed())
            .max(Duration::from_millis(1));
        match select.select_timeout(wait) {
            Ok(op) if op.index() == cancel_op => {
                let _ = op.recv(cancel);
                break;
            }
            Ok(op) if op.index() == blobs_op => {
                if let Ok(blob) = op.recv(blobs)
                    && (&*stop.sock).write_all(&blob).is_err()
                {
                    break;
                }
            }
            Ok(op) if op.index() == queue_op => {
                if let Ok(message) = op.recv(rx) {
                    release(bytes, message.len());
                    if !send_frame(&mut ws, message) {
                        break;
                    }
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
    stop.shutdown();
}

/// Sends a bounded batch of queued messages. Returns true when the loop
/// must stop because the agent disconnected or a send failed.
fn drain(ws: &mut WebSocket<WriterWire>, rx: &Receiver<Vec<u8>>, bytes: &Mutex<u64>) -> bool {
    for _ in 0..DRAIN_BATCH {
        match rx.try_recv() {
            Ok(message) => {
                release(bytes, message.len());
                if !send_frame(ws, message) {
                    return true;
                }
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
    false
}

/// Sends every queued line as one text frame. A message that is not
/// UTF-8 is corrupt JSON from the local agent: the connection is torn
/// down (false) so the breakage surfaces locally instead of reaching
/// the remote client as mangled text.
fn send_frame(ws: &mut WebSocket<WriterWire>, message: Vec<u8>) -> bool {
    let Ok(text) = String::from_utf8(message) else {
        eprintln!("kage-remote: dropped a non-UTF8 message; closing the connection");
        return false;
    };
    ws.send(Message::text(text)).is_ok()
}

fn release(bytes: &Mutex<u64>, len: usize) {
    let Ok(len) = u64::try_from(len) else {
        return;
    };
    let mut queued = bytes.lock().unwrap_or_else(PoisonError::into_inner);
    *queued = queued.saturating_sub(len);
}

fn millis_since(epoch: Instant) -> u64 {
    u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    /// One loopback socket pair.
    fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    /// The writer half of [`upgrade`]'s plumbing, driven through a
    /// channel so a test controls what goes into the queue.
    struct WriterHarness {
        queue: Sender<Vec<u8>>,
        _blobs: Sender<Vec<u8>>,
        thread: thread::JoinHandle<()>,
        /// Held so the write half outlives the loop.
        _sock: Arc<TcpStream>,
    }

    /// Starts [`write_loop`] against one end of a socket pair and
    /// hands back the harness and the client end.
    fn start_writer() -> (WriterHarness, TcpStream) {
        let (client, server) = socket_pair();
        let (queue, rx) = bounded(4);
        let (blobs, blob_rx) = bounded::<Vec<u8>>(8);
        let bytes = Arc::new(Mutex::new(0));
        let last_inbound = Arc::new(AtomicU64::new(0));
        let sock = Arc::new(server);
        let (cancel_tx, cancel_rx) = bounded(0);
        let stop = Arc::new(Stop {
            sock: Arc::clone(&sock),
            cancel: Mutex::new(Some(cancel_tx)),
        });
        let loop_stop = Arc::clone(&stop);
        let wire_sock = Arc::clone(&sock);
        let thread = thread::spawn(move || {
            let ws = WebSocket::from_raw_socket(
                WriterWire { sock: wire_sock },
                Role::Server,
                Some(config()),
            );
            write_loop(
                ws,
                &blob_rx,
                &rx,
                &bytes,
                &loop_stop,
                &cancel_rx,
                (Instant::now(), &last_inbound),
            );
        });
        (
            WriterHarness {
                queue,
                _blobs: blobs,
                thread,
                _sock: sock,
            },
            client,
        )
    }

    #[test]
    fn final_flush_survives_writer_drop_and_reader_wakes() {
        let (client, server) = socket_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let sock = Arc::new(server);
        let (cancel_tx, cancel_rx) = bounded(0);
        let stop = Arc::new(Stop {
            sock: Arc::clone(&sock),
            cancel: Mutex::new(Some(cancel_tx)),
        });
        let (inbound_tx, inbound_rx) = bounded(1);
        let inbound_cancel = cancel_rx.clone();
        let reader = thread::spawn(move || {
            let _inbound_tx = inbound_tx;
            let _ = inbound_cancel.recv();
        });
        let (queue_tx, queue_rx) = bounded(1);
        let (_blob_tx, blob_rx) = bounded(1);
        let bytes = Arc::new(Mutex::new(0));
        let mut outgoing = Outgoing {
            buffer: Vec::new(),
            queue: Queue {
                tx: queue_tx,
                bytes: Arc::clone(&bytes),
            },
            stop: Arc::clone(&stop),
            failed: false,
        };
        let (start_tx, start_rx) = bounded::<()>(0);
        let worker_stop = Arc::clone(&stop);
        let writer = thread::spawn(move || {
            start_rx.recv().unwrap();
            let ws = WebSocket::from_raw_socket(WriterWire { sock }, Role::Server, Some(config()));
            write_loop(
                ws,
                &blob_rx,
                &queue_rx,
                &bytes,
                &worker_stop,
                &cancel_rx,
                (Instant::now(), &AtomicU64::new(0)),
            );
        });
        let incoming = Incoming {
            rx: inbound_rx,
            current: Vec::new(),
            offset: 0,
            stop,
            workers: vec![reader, writer],
        };
        let (done_tx, done_rx) = bounded(0);
        thread::spawn(move || {
            let mut reader = incoming;
            done_tx.send(reader.read(&mut [0; 1])).unwrap();
        });
        outgoing.write_all(b"final response").unwrap();
        outgoing.flush().unwrap();
        drop(outgoing);
        start_tx.send(()).unwrap();
        let mut peer = WebSocket::from_raw_socket(client, Role::Client, None);
        assert_eq!(peer.read().unwrap(), Message::text("final response"));
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[test]
    fn drain_yields_with_messages_still_queued() {
        let (_client, server) = socket_pair();
        let (tx, rx) = bounded(DRAIN_BATCH * 2);
        for _ in 0..DRAIN_BATCH * 2 {
            tx.send(b"x".to_vec()).unwrap();
        }
        let bytes = Mutex::new((DRAIN_BATCH * 2) as u64);
        let mut ws = WebSocket::from_raw_socket(
            WriterWire {
                sock: Arc::new(server),
            },
            Role::Server,
            Some(config()),
        );
        assert!(!drain(&mut ws, &rx, &bytes));
        assert_eq!(rx.len(), DRAIN_BATCH);
        assert_eq!(*bytes.lock().unwrap(), DRAIN_BATCH as u64);
    }

    #[test]
    fn shutdown_interrupts_full_control_channel() {
        let (_client, server) = socket_pair();
        let sock = Arc::new(server);
        let (cancel_tx, cancel_rx) = bounded(0);
        let stop = Stop {
            sock: Arc::clone(&sock),
            cancel: Mutex::new(Some(cancel_tx)),
        };
        let (blobs, _rx) = bounded(1);
        blobs.send(vec![1]).unwrap();
        let (started_tx, started_rx) = bounded(0);
        let (done_tx, done_rx) = bounded(0);
        let worker = thread::spawn(move || {
            let mut wire = ReaderWire {
                sock,
                blobs,
                cancel: cancel_rx,
            };
            started_tx.send(()).unwrap();
            done_tx.send(wire.write(b"pong")).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        stop.shutdown();
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        worker.join().unwrap();
    }

    #[test]
    fn dropping_incoming_interrupts_full_inbound_channel() {
        let (client, server) = socket_pair();
        let sock = Arc::new(server);
        let (cancel_tx, cancel_rx) = bounded(0);
        let stop = Arc::new(Stop {
            sock: Arc::clone(&sock),
            cancel: Mutex::new(Some(cancel_tx)),
        });
        let (blobs, _blob_rx) = bounded(1);
        let (tx, rx) = bounded(1);
        let (ready_tx, ready_rx) = bounded(0);
        let worker_stop = Arc::clone(&stop);
        let processed = Arc::new(AtomicU64::new(0));
        let worker_processed = Arc::clone(&processed);
        let worker = thread::spawn(move || {
            let mut ws = WebSocket::from_raw_socket(
                ReaderWire {
                    sock,
                    blobs,
                    cancel: cancel_rx.clone(),
                },
                Role::Server,
                Some(config()),
            );
            tx.send(b"occupied".to_vec()).unwrap();
            ready_tx.send(()).unwrap();
            read_loop(
                &mut ws,
                &tx,
                &worker_stop,
                &cancel_rx,
                Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
                &worker_processed,
            );
        });
        ready_rx.recv().unwrap();
        let mut peer = WebSocket::from_raw_socket(client, Role::Client, None);
        peer.send(Message::text("blocked")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while processed.load(Ordering::Acquire) == 0 {
            assert!(
                Instant::now() < deadline,
                "reader did not process the frame"
            );
            thread::yield_now();
        }
        let incoming = Incoming {
            rx,
            current: Vec::new(),
            offset: 0,
            stop,
            workers: vec![worker],
        };
        let (done_tx, done_rx) = bounded(0);
        thread::spawn(move || {
            drop(incoming);
            done_tx.send(()).unwrap();
        });
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn oversized_unflushed_frame_closes_connection() {
        let (mut client, server) = socket_pair();
        let (tx, _rx) = bounded(1);
        let mut outgoing = Outgoing {
            buffer: vec![0; usize::try_from(OUTGOING_CAP).unwrap() - 1],
            queue: Queue {
                tx,
                bytes: Arc::new(Mutex::new(0)),
            },
            stop: Arc::new(Stop {
                sock: Arc::new(server),
                cancel: Mutex::new(None),
            }),
            failed: false,
        };
        assert_eq!(outgoing.write(b"x").unwrap(), 1);
        assert_eq!(
            outgoing.write(b"x").unwrap_err().kind(),
            io::ErrorKind::WriteZero
        );
        assert!(outgoing.buffer.is_empty());
        assert_eq!(
            outgoing.write(b"y").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            outgoing.flush().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(client.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn a_non_utf8_message_tears_the_connection_down_without_a_frame() {
        let (writer, mut client) = start_writer();
        writer.queue.send(vec![0xff, 0xfe, 0x00]).unwrap();
        writer.thread.join().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut rest = Vec::new();
        let read = client.read_to_end(&mut rest).unwrap();
        assert_eq!(read, 0, "no frame may reach the peer: {rest:?}");
    }

    #[test]
    fn a_utf8_message_reaches_the_peer_as_one_text_frame() {
        let (writer, client) = start_writer();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writer.queue.send(b"{\"ok\":true}".to_vec()).unwrap();
        let mut peer = WebSocket::from_raw_socket(client, Role::Client, None);
        loop {
            match peer.read().unwrap() {
                Message::Text(text) => {
                    assert_eq!(text.as_str(), "{\"ok\":true}");
                    break;
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_frame_with_a_raw_newline_still_arrives_as_one_line() {
        let (client, server) = socket_pair();
        server.set_read_timeout(Some(READER_TICK)).unwrap();
        let (blobs, blob_rx) = bounded::<Vec<u8>>(8);
        let (pipe_write, pipe_read) = bounded(4);
        let (cancel_tx, cancel_rx) = bounded(0);
        let sock = Arc::new(server);
        let epoch = Instant::now();
        let last_inbound = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(Stop {
            sock: Arc::clone(&sock),
            cancel: Mutex::new(Some(cancel_tx)),
        });
        let reader_stop = Arc::clone(&stop);
        let wire_sock = Arc::clone(&sock);
        let reader = thread::spawn(move || {
            let mut ws = WebSocket::from_raw_socket(
                ReaderWire {
                    sock: wire_sock,
                    blobs,
                    cancel: cancel_rx.clone(),
                },
                Role::Server,
                Some(config()),
            );
            read_loop(
                &mut ws,
                &pipe_write,
                &reader_stop,
                &cancel_rx,
                epoch,
                &last_inbound,
            );
        });
        let mut peer = WebSocket::from_raw_socket(client, Role::Client, None);
        peer.send(Message::text("line one\nline two")).unwrap();
        let line = String::from_utf8(pipe_read.recv().unwrap()).unwrap();
        assert_eq!(line, "line one line two\n");
        let _ = peer.close(None);
        reader.join().unwrap();
        drop(blob_rx);
    }
}
