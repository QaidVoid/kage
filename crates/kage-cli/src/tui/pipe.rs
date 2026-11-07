//! An in-memory byte pipe for transports inside one process.
//!
//! A private TUI host serves its ACP client over one of these where a
//! [`std::os::unix::net::UnixStream`] pair is unavailable, such as
//! Windows. The two halves exchange byte chunks through channels:
//! what one writes, the other reads, and dropping a writer ends the
//! opposite reader at EOF like a hung-up stream would.

// Wired only where a `UnixStream` pair is unavailable, but compiled
// and tested everywhere so the logic cannot rot unseen.
#![allow(dead_code)]

use std::io;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};

/// Creates a connected pair: bytes written to the first half are read
/// from the second, and the other way round.
pub(crate) fn channel_pair() -> (PipeWriter, PipeReader) {
    let (one, two) = mpsc::channel();
    (
        PipeWriter { queue: one },
        PipeReader {
            inner: Mutex::new(ReaderInner {
                queue: two,
                pending: Vec::new(),
                offset: 0,
            }),
        },
    )
}

/// The writing half. Its drop ends the opposite reader at EOF.
pub(crate) struct PipeWriter {
    queue: Sender<Vec<u8>>,
}

impl io::Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.queue.send(buf.to_vec()).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the reader is gone",
            ));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The reading half, buffered across the pipe's chunks.
pub(crate) struct PipeReader {
    inner: Mutex<ReaderInner>,
}

struct ReaderInner {
    queue: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    offset: usize,
}

impl io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.offset < inner.pending.len() {
                let left = inner.pending.len() - inner.offset;
                let take = left.min(buf.len());
                buf[..take].copy_from_slice(&inner.pending[inner.offset..inner.offset + take]);
                inner.offset += take;
                return Ok(take);
            }
            match inner.queue.recv() {
                Ok(chunk) if chunk.is_empty() => {}
                Ok(chunk) => {
                    inner.pending = chunk;
                    inner.offset = 0;
                }
                // Every writer is gone: the peer hung up.
                Err(_) => return Ok(0),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    #[test]
    fn bytes_written_on_one_half_are_read_from_the_other() {
        let (mut out, mut in_) = channel_pair();
        out.write_all(b"hello ").unwrap();
        out.write_all(b"world").unwrap();
        let mut got = [0u8; 11];
        in_.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello world");
    }

    #[test]
    fn a_read_splits_across_the_writer_s_chunks_at_the_caller_s_size() {
        let (mut out, mut in_) = channel_pair();
        out.write_all(b"abcdef").unwrap();
        let mut got = [0u8; 2];
        in_.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ab");
        drop(out);
        let mut rest = Vec::new();
        in_.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"cdef");
    }

    #[test]
    fn dropping_the_writer_ends_the_reader_at_eof() {
        let (out, mut in_) = channel_pair();
        drop(out);
        let mut got = Vec::new();
        in_.read_to_end(&mut got).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn a_write_to_a_dropped_reader_fails() {
        let (mut out, in_) = channel_pair();
        drop(in_);
        assert!(out.write_all(b"nobody listens").is_err());
    }

    #[test]
    fn a_reader_keeps_whole_bytes_when_reads_stop_mid_chunk() {
        let (mut out, mut in_) = channel_pair();
        out.write_all(b"1234567890").unwrap();
        let mut first = [0u8; 3];
        in_.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"123");
        let mut second = [0u8; 3];
        in_.read_exact(&mut second).unwrap();
        assert_eq!(&second, b"456");
    }
}
