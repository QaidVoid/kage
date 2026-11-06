//! A bounded-wait writer for Unix pipes.
//!
//! This low-level transport is independent of [`crate::Peer`]. It does not
//! settle JSON-RPC requests or interrupt a reader. Owners must perform those
//! lifecycle operations themselves. It is not a replacement for the generic
//! blocking `connect` constructors.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use kage_core::CancelFlag;
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::stat::{SFlag, fstat};
use nix::unistd::write;

const CHECK_INTERVAL: Duration = Duration::from_millis(10);

struct Shared {
    fd: Mutex<Option<OwnedFd>>,
    closed: AtomicBool,
    #[cfg(test)]
    after_request_write: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    writer_blocked: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// Cloneable, frame-serializing pipe writer with cooperative shutdown.
///
/// The owned descriptor must be the write end of a pipe or FIFO. Construction
/// enables `O_NONBLOCK` on its open file description, affecting all duplicated
/// descriptors referring to that description. The caller must have exclusive
/// control of the open file description: no duplicate may write or change its
/// flags while this writer exists. The constructor cannot verify exclusivity.
/// Shutdown and cancellation are checked at most every 10 ms while polling,
/// subject to OS scheduling. No worker thread is created. As with ordinary Unix
/// pipe writes, a process configured to use the default SIGPIPE disposition may
/// terminate when the pipe's reader closes.
#[derive(Clone)]
pub struct PipeWriter(Arc<Shared>);

pub(crate) enum GuardedWriteError {
    CleanEof,
    Cancelled,
    Other(io::Error),
}

impl GuardedWriteError {
    fn from_check(error: io::Error, written: usize) -> Self {
        if written == 0 && error.kind() == io::ErrorKind::Interrupted {
            Self::Cancelled
        } else {
            Self::Other(error)
        }
    }

    pub(crate) fn into_io(self) -> io::Error {
        match self {
            Self::CleanEof => closed(),
            Self::Cancelled => io::Error::new(io::ErrorKind::Interrupted, "write cancelled"),
            Self::Other(error) => error,
        }
    }
}

impl PipeWriter {
    /// Take ownership of a writable pipe or FIFO descriptor.
    ///
    /// # Errors
    /// Returns an error for unsupported descriptors or failed system calls.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        let stat = fstat(&fd).map_err(io::Error::from)?;
        if SFlag::from_bits_truncate(stat.st_mode) != SFlag::S_IFIFO {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a pipe or FIFO",
            ));
        }
        let flags =
            OFlag::from_bits_truncate(fcntl(&fd, FcntlArg::F_GETFL).map_err(io::Error::from)?);
        if flags & OFlag::O_ACCMODE == OFlag::O_RDONLY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a writable pipe",
            ));
        }
        fcntl(&fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::from)?;
        Ok(Self(Arc::new(Shared {
            fd: Mutex::new(Some(fd)),
            closed: AtomicBool::new(false),
            #[cfg(test)]
            after_request_write: Mutex::new(None),
            #[cfg(test)]
            writer_blocked: Mutex::new(None),
        })))
    }

    /// Signal terminal shutdown without acquiring a lock or closing the descriptor.
    ///
    /// Active and queued writes observe the signal cooperatively. An in-flight
    /// final write may still succeed, and an active write may close the descriptor
    /// while stopping. This alone does not close an idle descriptor, settle
    /// pending responses, or interrupt a reader. Call [`Self::shutdown`] to
    /// complete descriptor cleanup.
    pub fn signal_shutdown(&self) {
        self.0.closed.store(true, Ordering::Release);
    }

    /// Signal shutdown and close the descriptor, waiting for any active writer.
    /// Shutdown does not interrupt an independently owned reader.
    pub fn shutdown(&self) {
        self.signal_shutdown();
        let mut guard = self
            .0
            .fd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.take();
    }

    /// Whether shutdown or a fatal write failure has closed the transport.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    /// Write one complete frame without interleaving it with other frames.
    ///
    /// The caller supplies framing, including the trailing newline. If a frame
    /// is only partially written when an error, cancellation, or deadline
    /// occurs, the transport closes permanently. Any pipe I/O error closes it
    /// even if no bytes were sent. Cancellation or deadline before the first
    /// byte leaves it usable. Cancellation can race with a completed write.
    ///
    /// # Errors
    /// Returns `Interrupted` on cancellation, `TimedOut` at the deadline, or an
    /// I/O error on shutdown or pipe failure.
    pub fn write_frame(
        &self,
        frame: &[u8],
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
    ) -> io::Result<()> {
        self.write_frame_guarded(frame, deadline, cancel, None)
            .map_err(GuardedWriteError::into_io)
    }

    pub(crate) fn write_frame_guarded(
        &self,
        frame: &[u8],
        deadline: Option<Instant>,
        cancel: Option<&CancelFlag>,
        eof: Option<&Mutex<bool>>,
    ) -> Result<(), GuardedWriteError> {
        let mut guard = loop {
            self.check(deadline, cancel)
                .map_err(|e| GuardedWriteError::from_check(e, 0))?;
            match self.0.fd.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(err)) => break err.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    #[cfg(test)]
                    if let Some(hook) = self.0.writer_blocked.lock().unwrap().take() {
                        hook();
                    }
                    std::thread::sleep(wait_interval(deadline));
                }
            }
        };
        self.check(deadline, cancel)
            .map_err(|e| GuardedWriteError::from_check(e, 0))?;
        let mut written = 0;
        loop {
            if written == frame.len() {
                return Ok(());
            }
            if let Err(error) = self.check(deadline, cancel) {
                if written > 0 || self.0.closed.load(Ordering::Acquire) {
                    self.0.closed.store(true, Ordering::Release);
                    guard.take();
                }
                return Err(GuardedWriteError::from_check(error, written));
            }
            let Some(fd) = guard.as_ref() else {
                return Err(GuardedWriteError::Other(closed()));
            };
            let result = if let Some(eof) = eof {
                let eof = eof
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *eof {
                    drop(eof);
                    if written > 0 {
                        self.0.closed.store(true, Ordering::Release);
                        guard.take();
                    }
                    return Err(if written == 0 {
                        GuardedWriteError::CleanEof
                    } else {
                        GuardedWriteError::Other(closed())
                    });
                }
                write(fd, &frame[written..])
            } else {
                write(fd, &frame[written..])
            };
            #[cfg(test)]
            if eof.is_some() && matches!(result, Ok(n) if n == frame.len() - written) {
                let hook = self.0.after_request_write.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook();
                }
            }
            match result {
                Ok(0) => {
                    self.0.closed.store(true, Ordering::Release);
                    guard.take();
                    return Err(GuardedWriteError::Other(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "pipe write returned zero",
                    )));
                }
                Ok(n) => written += n,
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => {
                    let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLOUT)];
                    let ms =
                        u16::try_from(wait_interval(deadline).as_millis().max(1)).unwrap_or(10);
                    match poll(&mut fds, ms) {
                        Ok(_) | Err(Errno::EINTR) => {}
                        Err(error) => {
                            self.0.closed.store(true, Ordering::Release);
                            guard.take();
                            return Err(GuardedWriteError::Other(error.into()));
                        }
                    }
                }
                Err(error) => {
                    self.0.closed.store(true, Ordering::Release);
                    guard.take();
                    return Err(GuardedWriteError::Other(error.into()));
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn lock_for_test(&self) -> std::sync::MutexGuard<'_, Option<OwnedFd>> {
        self.0.fd.lock().unwrap()
    }

    #[cfg(test)]
    pub(crate) fn after_request_write(&self, hook: impl FnOnce() + Send + 'static) {
        *self.0.after_request_write.lock().unwrap() = Some(Box::new(hook));
    }

    fn check(&self, deadline: Option<Instant>, cancel: Option<&CancelFlag>) -> io::Result<()> {
        if self.0.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        if cancel.is_some_and(CancelFlag::is_cancelled) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "write cancelled",
            ));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "write deadline expired",
            ));
        }
        Ok(())
    }
}

fn wait_interval(deadline: Option<Instant>) -> Duration {
    deadline.map_or(CHECK_INTERVAL, |d| {
        d.saturating_duration_since(Instant::now())
            .min(CHECK_INTERVAL)
    })
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "transport closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn signal_stops_active_and_queued_writes() {
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let writer = PipeWriter::new(write_fd).unwrap();
        let active_writer = writer.clone();
        let active = thread::spawn(move || {
            active_writer.write_frame(&vec![b'x'; 4 * 1024 * 1024], None, None)
        });
        let mut reader = std::fs::File::from(read_fd);
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        let (entered, waiting) = mpsc::channel();
        *writer.0.writer_blocked.lock().unwrap() = Some(Box::new(move || {
            entered.send(()).unwrap();
        }));
        let queued_writer = writer.clone();
        let queued = thread::spawn(move || queued_writer.write_frame(b"queued", None, None));
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        writer.signal_shutdown();
        assert_eq!(
            active.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            queued.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        writer.shutdown();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert!(rest.len() < 4 * 1024 * 1024 - 1);
    }

    #[test]
    fn empty_frame_respects_shutdown_and_cancellation() {
        let (_read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let writer = PipeWriter::new(write_fd).unwrap();
        assert!(writer.write_frame(b"", None, None).is_ok());
        let cancel = CancelFlag::new();
        cancel.cancel();
        assert_eq!(
            writer
                .write_frame(b"", None, Some(&cancel))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        writer.shutdown();
        assert_eq!(
            writer.write_frame(b"", None, None).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn cancelling_partial_real_pipe_frame_closes_it() {
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let writer = PipeWriter::new(write_fd).unwrap();
        let cancel = CancelFlag::new();
        let worker_writer = writer.clone();
        let worker_cancel = cancel.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result =
                worker_writer.write_frame(&vec![b'x'; 4 * 1024 * 1024], None, Some(&worker_cancel));
            done_tx.send(result).unwrap();
        });
        let mut reader = std::fs::File::from(read_fd);
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte[0], b'x');
        assert!(done_rx.recv_timeout(Duration::from_millis(30)).is_err());
        cancel.cancel();
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        worker.join().unwrap();
        assert_eq!(
            writer
                .write_frame(b"next\n", None, None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert!(!rest.is_empty());
        assert!(rest.len() < 4 * 1024 * 1024 - 1);
    }

    #[test]
    fn shutdown_interrupts_blocked_write_and_closes_descriptor() {
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let writer = PipeWriter::new(write_fd).unwrap();
        let worker_writer = writer.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx
                .send(worker_writer.write_frame(&vec![b'x'; 4 * 1024 * 1024], None, None))
                .unwrap();
        });
        let mut reader = std::fs::File::from(read_fd);
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(30)).is_err());
        writer.shutdown();
        assert_eq!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        worker.join().unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
    }

    #[test]
    fn deadline_interrupts_partial_blocked_frame() {
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let writer = PipeWriter::new(write_fd).unwrap();
        let started = Instant::now();
        let error = writer
            .write_frame(
                &vec![b'x'; 4 * 1024 * 1024],
                Some(started + Duration::from_millis(80)),
                None,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            writer
                .write_frame(b"next\n", None, None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        let mut data = Vec::new();
        std::fs::File::from(read_fd).read_to_end(&mut data).unwrap();
        assert!(!data.is_empty());
        assert!(data.len() < 4 * 1024 * 1024);
    }

    #[test]
    fn deadline_before_writing_preserves_pipe() {
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let writer = PipeWriter::new(write_fd).unwrap();
        assert_eq!(
            writer
                .write_frame(b"no\n", Some(Instant::now()), None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        writer
            .write_frame(
                b"yes\n",
                Some(Instant::now() + Duration::from_secs(1)),
                None,
            )
            .unwrap();
        writer.shutdown();
        let mut data = String::new();
        std::fs::File::from(read_fd)
            .read_to_string(&mut data)
            .unwrap();
        assert_eq!(data, "yes\n");
    }
}
