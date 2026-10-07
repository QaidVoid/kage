//! The upgraded connection: WebSocket frames in, JSON-RPC lines out.
//!
//! [`upgrade`] writes the `101` response and hands back the
//! newline-delimited reader and writer pair the agent serving loop
//! consumes. Two threads share one socket. The reader turns text
//! frames into lines on an in-process pipe, replacing raw newlines so
//! a frame always arrives as one message, dropping binary frames, and
//! closing on a message past the inbound cap. The writer drains a
//! byte-capped outgoing queue into text frames, pings on an interval,
//! and closes a connection that stops answering. Feeding the queue
//! never blocks: once the cap is passed, the flush fails and the
//! socket closes.

use std::io::{self, BufReader, PipeReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
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
) -> io::Result<(BufReader<PipeReader>, Outgoing)> {
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
    stream.flush()?;

    stream.set_read_timeout(Some(READER_TICK))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;

    let sock = Arc::new(stream.try_clone()?);
    let (blob_tx, blob_rx) = bounded::<Vec<u8>>(WIRE_BLOBS);
    let mut ws_read = WebSocket::from_partially_read(
        ReaderWire {
            sock: Arc::clone(&sock),
            blobs: blob_tx,
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

    let (pipe_read, pipe_write) = io::pipe()?;
    let (tx, rx) = bounded::<Vec<u8>>(OUTGOING_MESSAGES);
    let bytes = Arc::new(Mutex::new(0));
    let queue = Queue {
        tx,
        bytes: Arc::clone(&bytes),
    };
    let epoch = Instant::now();
    let last_inbound = Arc::new(AtomicU64::new(0));

    {
        let sock = Arc::clone(&sock);
        let last_inbound = Arc::clone(&last_inbound);
        thread::Builder::new()
            .name("kage-remote-read".to_owned())
            .spawn(move || read_loop(&mut ws_read, pipe_write, &sock, epoch, &last_inbound))?;
    }
    {
        let sock = Arc::clone(&sock);
        let last_inbound = Arc::clone(&last_inbound);
        thread::Builder::new()
            .name("kage-remote-write".to_owned())
            .spawn(move || {
                write_loop(ws_write, &blob_rx, &rx, &bytes, &sock, epoch, &last_inbound);
            })?;
    }

    let outgoing = Outgoing {
        buffer: Vec::new(),
        queue,
        sock,
    };
    Ok((BufReader::new(pipe_read), outgoing))
}

/// The write half handed to the agent: bytes buffer in memory and each
/// [`Write::flush`] becomes one text frame.
///
/// Feeding the queue never blocks. When the byte cap is passed or the
/// connection is gone, the flush fails and the socket closes, so a
/// stalled reader is cut off instead of wedging the agent.
pub struct Outgoing {
    buffer: Vec<u8>,
    queue: Queue,
    sock: Arc<TcpStream>,
}

impl Write for Outgoing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
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
    fn cut(&self, kind: io::ErrorKind, message: &str) -> io::Error {
        let _ = self.sock.shutdown(Shutdown::Both);
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
}

impl Read for ReaderWire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.sock).read(buf)
    }
}

impl Write for ReaderWire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.blobs
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer thread is gone"))?;
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
    mut pipe: io::PipeWriter,
    sock: &TcpStream,
    epoch: Instant,
    last_inbound: &AtomicU64,
) {
    loop {
        let received = ws.read();
        match received {
            Ok(Message::Text(text)) => {
                last_inbound.store(millis_since(epoch), Ordering::Release);
                if text.contains('\n') {
                    eprintln!(
                        "kage-remote: frame carried a raw newline; replaced so the frame stays \
                         one message"
                    );
                }
                let mut line = text.as_str().replace('\n', " ");
                line.push('\n');
                if pipe.write_all(line.as_bytes()).is_err() {
                    break;
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
    let _ = sock.shutdown(Shutdown::Both);
}

fn write_loop(
    mut ws: WebSocket<WriterWire>,
    blobs: &Receiver<Vec<u8>>,
    rx: &Receiver<Vec<u8>>,
    bytes: &Mutex<u64>,
    sock: &TcpStream,
    epoch: Instant,
    last_inbound: &AtomicU64,
) {
    let mut last_ping = Instant::now();
    let mut select = Select::new();
    let blobs_op = select.recv(blobs);
    let queue_op = select.recv(rx);
    loop {
        // Bytes the reader thread produced, pong and close echoes,
        // always go out first.
        let mut blobs_dead = false;
        while let Ok(blob) = blobs.try_recv() {
            if (&*sock).write_all(&blob).is_err() {
                blobs_dead = true;
                break;
            }
        }
        if blobs_dead || drain(&mut ws, rx, bytes) {
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
            Ok(op) if op.index() == blobs_op => {
                if let Ok(blob) = op.recv(blobs)
                    && (&*sock).write_all(&blob).is_err()
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
    let _ = sock.shutdown(Shutdown::Both);
}

/// Sends every queued message. Returns true when the loop must stop:
/// either the agent dropped the connection or a send failed.
fn drain(ws: &mut WebSocket<WriterWire>, rx: &Receiver<Vec<u8>>, bytes: &Mutex<u64>) -> bool {
    loop {
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
    use std::io::BufRead as _;
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
        let loop_sock = Arc::clone(&sock);
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
                &loop_sock,
                Instant::now(),
                &last_inbound,
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
        let (pipe_read, pipe_write) = io::pipe().unwrap();
        let sock = Arc::new(server);
        let epoch = Instant::now();
        let last_inbound = Arc::new(AtomicU64::new(0));
        let reader_sock = Arc::clone(&sock);
        let wire_sock = Arc::clone(&sock);
        let reader = thread::spawn(move || {
            let mut ws = WebSocket::from_raw_socket(
                ReaderWire {
                    sock: wire_sock,
                    blobs,
                },
                Role::Server,
                Some(config()),
            );
            read_loop(&mut ws, pipe_write, &reader_sock, epoch, &last_inbound);
        });
        let mut peer = WebSocket::from_raw_socket(client, Role::Client, None);
        peer.send(Message::text("line one\nline two")).unwrap();
        let mut line = String::new();
        BufReader::new(pipe_read).read_line(&mut line).unwrap();
        assert_eq!(line, "line one line two\n");
        let _ = peer.close(None);
        reader.join().unwrap();
        drop(blob_rx);
    }
}
