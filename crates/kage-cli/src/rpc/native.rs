//! Native serving to any writer through a kage-owned pipe.
//!
//! The native peer needs an exclusively owned pipe to make its writes
//! interruptible and deadline bound. Inherited stdout and a WebSocket cannot
//! provide one, so the peer writes into a pipe this module creates, and one
//! pump thread copies each line to the real writer and flushes it, which is
//! one message per line on a WebSocket. A writer that blocks only stalls the
//! pump. The pipe then fills and the peer's writes time out instead of
//! blocking the host. If the writer fails, the pump discards the rest until
//! the peer closes the pipe, so the pipe never raises SIGPIPE.

use std::io::{self, BufRead, BufReader, Write};
use std::thread::JoinHandle;

/// A pipe whose write end the native peer owns and whose read end is pumped
/// to `sink`. Join the pump after the peer is dropped so every line is out.
pub(super) fn pumped(
    sink: impl Write + Send + 'static,
) -> io::Result<(std::os::fd::OwnedFd, JoinHandle<()>)> {
    let (reader, writer) = std::io::pipe()?;
    let pump = std::thread::Builder::new()
        .name("kage-native-output".into())
        .spawn(move || pump(BufReader::new(reader), sink))?;
    Ok((writer.into(), pump))
}

fn pump(mut lines: impl BufRead, mut sink: impl Write) {
    let mut line = Vec::new();
    let mut failed = false;
    loop {
        line.clear();
        match lines.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // A frame the peer abandoned partway ends without a newline.
        if !line.ends_with(b"\n") {
            break;
        }
        if !failed && sink.write_all(&line).and_then(|()| sink.flush()).is_err() {
            failed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // Bytes written since the last flush, and each flushed message.
    type Written = (Vec<u8>, Vec<Vec<u8>>);

    #[derive(Clone, Default)]
    struct Messages(Arc<Mutex<Written>>);

    impl Write for Messages {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            kage_core::sync::lock(&self.0).0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            let mut messages = kage_core::sync::lock(&self.0);
            let message = std::mem::take(&mut messages.0);
            if !message.is_empty() {
                messages.1.push(message);
            }
            Ok(())
        }
    }

    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn each_line_is_flushed_as_one_message() {
        let messages = Messages::default();
        let (fd, pump) = pumped(messages.clone()).unwrap();
        let mut writer = std::fs::File::from(fd);
        writer.write_all(b"{\"a\":1}\n{\"b\":").unwrap();
        writer.write_all(b"2}\n{\"torn\"").unwrap();
        drop(writer);
        pump.join().unwrap();
        let messages = kage_core::sync::lock(&messages.0);
        assert_eq!(
            messages.1,
            [b"{\"a\":1}\n".to_vec(), b"{\"b\":2}\n".to_vec()]
        );
    }

    #[test]
    fn a_failed_writer_is_drained_until_the_peer_closes() {
        let (fd, pump) = pumped(Broken).unwrap();
        let mut writer = std::fs::File::from(fd);
        for _ in 0..1024 {
            writer.write_all(&[b'x'; 1023]).unwrap();
            writer.write_all(b"\n").unwrap();
        }
        drop(writer);
        pump.join().unwrap();
    }
}
