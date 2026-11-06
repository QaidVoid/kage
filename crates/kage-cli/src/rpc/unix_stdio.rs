//! Owned, interruptible pipe transport for a future ACP server.
//!
//! Both descriptors must have exclusively controlled open file descriptions.
//! Neither descriptor may have aliases that perform I/O or change status flags.
//! Descriptor type checks cannot establish this property. In particular, do not
//! duplicate inherited stdout and pass the duplicate here. The process must
//! handle or ignore SIGPIPE if it needs a closed output pipe to return an error
//! instead of terminating the process.

use std::io::{self, BufReader, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use kage_acp::agent::{Agent, AgentPeer, serve_agent_unix_connected};
use kage_jsonrpc::unix_peer::{self, ReadLimits, UnixPeer};
use kage_jsonrpc::{OrderedInbound, RpcError};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::stat::{SFlag, fstat};
use nix::unistd::read;
use std::time::Duration;

struct Input {
    fd: OwnedFd,
    stopped: Arc<AtomicBool>,
    #[cfg(test)]
    reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return Ok(0);
            }
            match read(&self.fd, buf) {
                Ok(n) => {
                    #[cfg(test)]
                    if n > 0 {
                        self.reads.fetch_add(1, Ordering::Release);
                    }
                    return Ok(n);
                }
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => {
                    let mut fds = [PollFd::new(self.fd.as_fd(), PollFlags::POLLIN)];
                    match poll(&mut fds, 10u16) {
                        Ok(_) | Err(Errno::EINTR) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// A cloneable forced-stop handle. It interrupts input and shuts down writes.
/// Application operations and their cleanup are not bounded by this handle.
/// Stopping partway through an input frame can be reported as a reader failure
/// rather than a clean cancellation.
#[derive(Clone)]
pub(super) struct InputStop {
    stopped: Arc<AtomicBool>,
    peer: UnixPeer,
}

impl InputStop {
    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.peer.shutdown();
    }
}

/// Owns the reader thread and its inbound queue as well as the write peer.
/// The receiver must be drained while serving. Shutdown drops it before joining
/// so a reader blocked on a full queue can terminate. Consumers must release
/// any messages or ordering barriers they retain independently.
pub(super) struct UnixStdio {
    peer: UnixPeer,
    inbound: Option<mpsc::Receiver<OrderedInbound>>,
    stopped: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    #[cfg(test)]
    reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl UnixStdio {
    /// Takes independent pipe ends. Both open file descriptions must be under
    /// exclusive control for the entire lifetime, including flag changes.
    pub(super) fn new(input: OwnedFd, output: OwnedFd, limits: ReadLimits) -> io::Result<Self> {
        let stat = fstat(&input).map_err(io::Error::from)?;
        let flags =
            OFlag::from_bits_truncate(fcntl(&input, FcntlArg::F_GETFL).map_err(io::Error::from)?);
        if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFIFO
            || flags & OFlag::O_ACCMODE != OFlag::O_RDONLY
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a readable pipe",
            ));
        }
        fcntl(&input, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::from)?;
        let stopped = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader = BufReader::new(Input {
            fd: input,
            stopped: Arc::clone(&stopped),
            #[cfg(test)]
            reads: Arc::clone(&reads),
        });
        let (peer, inbound, reader) =
            unix_peer::connect_ordered_server_with_limits(reader, output, limits)?;
        Ok(Self {
            peer,
            inbound: Some(inbound),
            stopped,
            reader: Some(reader),
            #[cfg(test)]
            reads,
        })
    }

    /// Returns a handle usable from another thread while the server is running.
    pub(super) fn stop_handle(&self) -> InputStop {
        InputStop {
            stopped: Arc::clone(&self.stopped),
            peer: self.peer.clone(),
        }
    }

    /// Runs ACP and then stops the transport and joins its input reader.
    /// Clean input EOF leaves output open until outstanding operations finish.
    /// Agent operations and cleanup can take an unbounded time.
    pub(super) fn serve<A, F>(
        mut self,
        write_timeout: Duration,
        make_agent: F,
    ) -> Result<(), RpcError>
    where
        A: Agent,
        F: FnOnce(AgentPeer) -> A,
    {
        let result = serve_agent_unix_connected(
            self.peer.clone(),
            self.inbound(),
            write_timeout,
            make_agent,
        );
        let joined = self.shutdown();
        match (result, joined) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(_)) => Err(RpcError::internal("Unix transport reader panicked")),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    pub(super) fn peer(&self) -> &UnixPeer {
        &self.peer
    }

    pub(super) fn inbound(&self) -> &mpsc::Receiver<OrderedInbound> {
        self.inbound.as_ref().expect("transport already shut down")
    }

    pub(super) fn shutdown(&mut self) -> std::thread::Result<()> {
        self.stopped.store(true, Ordering::Release);
        self.peer.shutdown();
        drop(self.inbound.take());
        self.reader.take().map_or(Ok(()), JoinHandle::join)
    }
}

impl Drop for UnixStdio {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::time::{Duration, Instant};

    struct TestAgent;

    impl Agent for TestAgent {
        fn initialize(
            &self,
            _: kage_acp::acp::InitializeRequest,
        ) -> kage_acp::acp::InitializeResponse {
            unreachable!()
        }
        fn new_session(
            &self,
            _: kage_acp::acp::NewSessionRequest,
        ) -> Result<kage_acp::acp::NewSessionResponse, RpcError> {
            unreachable!()
        }
        fn prompt(
            &self,
            _: kage_acp::acp::PromptRequest,
            _: &kage_acp::agent::PromptContext,
        ) -> Result<kage_acp::acp::PromptResponse, RpcError> {
            unreachable!()
        }
        fn cancel(&self, _: &str) {}
    }

    #[test]
    fn serve_preserves_output_after_clean_input_eof() {
        let (input, mut client, output, replies) = pipes();
        let transport = UnixStdio::new(input, output, ReadLimits::default()).unwrap();
        writeln!(
            client,
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"unknown\"}}"
        )
        .unwrap();
        drop(client);
        let server =
            std::thread::spawn(move || transport.serve(Duration::from_secs(1), |_| TestAgent));
        let mut line = String::new();
        BufReader::new(replies).read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["id"],
            1
        );
        server.join().unwrap().unwrap();
    }

    #[test]
    fn stop_interrupts_blocked_server_input() {
        let (input, _client, output, _replies) = pipes();
        let transport = UnixStdio::new(input, output, ReadLimits::default()).unwrap();
        let stop = transport.stop_handle();
        let (started, ready) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let result = transport.serve(Duration::from_secs(1), |_| {
                started.send(()).unwrap();
                TestAgent
            });
            done.send(result).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        stop.stop();
        finished
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
    }

    #[test]
    fn serve_reports_input_failure_and_joins() {
        let (input, mut client, output, _replies) = pipes();
        let transport = UnixStdio::new(
            input,
            output,
            ReadLimits {
                max_line_bytes: 16,
                inbound_capacity: 1,
            },
        )
        .unwrap();
        writeln!(client, "{{\"jsonrpc\":\"2.0\",\"method\":\"too_long\"}}").unwrap();
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            done.send(transport.serve(Duration::from_secs(1), |_| TestAgent))
                .unwrap();
        });
        assert!(
            finished
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
    }

    fn pipes() -> (OwnedFd, std::fs::File, OwnedFd, std::fs::File) {
        let (input, input_writer) = nix::unistd::pipe().unwrap();
        let (output_reader, output) = nix::unistd::pipe().unwrap();
        (input, input_writer.into(), output, output_reader.into())
    }

    #[test]
    fn exchanges_frames_and_joins_idle_reader() {
        let (input, mut client, output, replies) = pipes();
        let mut transport = UnixStdio::new(input, output, ReadLimits::default()).unwrap();
        writeln!(client, "{{\"jsonrpc\":\"2.0\",\"method\":\"hello\"}}").unwrap();
        assert!(matches!(
            transport
                .inbound()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            OrderedInbound::Message(_)
        ));
        transport
            .peer()
            .notify(
                "world",
                serde_json::json!({}),
                Some(Instant::now() + Duration::from_secs(2)),
                None,
            )
            .unwrap();
        let mut line = String::new();
        BufReader::new(replies).read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["method"],
            "world"
        );
        transport.shutdown().unwrap();
    }

    #[test]
    fn shutdown_drops_full_inbound_queue_before_join() {
        let (input, mut client, output, _replies) = pipes();
        let mut transport = UnixStdio::new(
            input,
            output,
            ReadLimits {
                max_line_bytes: 1024,
                inbound_capacity: 1,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        for expected in 1..=2 {
            writeln!(client, "{{\"jsonrpc\":\"2.0\",\"method\":\"hello\"}}").unwrap();
            while transport.reads.load(Ordering::Acquire) < expected {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        }
        // The second read cannot start until the first message has filled the queue.
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            done.send(transport.shutdown()).unwrap();
        });
        finished
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
    }

    #[test]
    fn serve_reports_output_failure_and_joins_blocked_input() {
        let (input, mut client, output, replies) = pipes();
        drop(replies);
        let transport = UnixStdio::new(input, output, ReadLimits::default()).unwrap();
        writeln!(
            client,
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"unknown\"}}"
        )
        .unwrap();
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            done.send(transport.serve(Duration::from_secs(1), |_| TestAgent))
                .unwrap();
        });
        assert!(
            finished
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn rejects_non_pipe_input() {
        let (_input, _client, output, _replies) = pipes();
        let file: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        assert!(UnixStdio::new(file, output, ReadLimits::default()).is_err());
    }
}
