//! Threaded JSON-RPC server.
//!
//! Concurrency follows the workspace rule: `std::thread` plus
//! `std::sync::mpsc`, no async. A reader thread pulls framed messages
//! off the input and special-cases out-of-band control methods
//! (`cancel`, `permission/respond`) so they take effect even while a
//! prompt is mid-flight; everything else is forwarded to the
//! dispatcher, which runs on the calling thread and drives the
//! injected [`AcpBackend`].
//!
//! The agent itself is *not* in this crate. The host (the `kage`
//! binary) implements [`AcpBackend`] with its provider registry,
//! tool registry, and the agent loop; the server only speaks the
//! protocol. That keeps `kage-acp` transport-only and unit-testable
//! with a mock backend.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::framing::{self, FramingError};
use crate::schema::{self, AcpCall, AcpRequest, PermissionResponse, PromptParams, error_code};

/// A cooperative cancellation flag shared between the reader thread
/// (which flips it on a `cancel` request) and the running prompt
/// (which polls it).
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A fresh, un-cancelled flag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Clear the flag before a new prompt.
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The host-supplied agent backend the server drives. The server
/// owns the protocol; this trait owns the agent.
pub trait AcpBackend {
    /// Capabilities/info returned by `initialize`.
    fn server_info(&self) -> serde_json::Value;

    /// Run one prompt to completion. `cancel` is polled cooperatively;
    /// `emit` receives each loop event already serialized as a JSON
    /// value (the same alphabet as `kage -p --json`), which the server
    /// wraps in an `event` notification.
    ///
    /// # Errors
    ///
    /// Returns an error string (sent as a JSON-RPC error) when the
    /// model is unresolvable or the run fails to start.
    fn prompt(
        &mut self,
        params: &PromptParams,
        cancel: &Cancel,
        emit: &mut dyn FnMut(serde_json::Value),
    ) -> Result<serde_json::Value, String>;

    /// List recorded sessions.
    ///
    /// # Errors
    ///
    /// Returns an error string when the session store cannot be read.
    fn list_sessions(&mut self) -> Result<serde_json::Value, String>;

    /// Replay a recorded session into the agent context.
    ///
    /// # Errors
    ///
    /// Returns an error string when the id is unknown or the file
    /// cannot be replayed.
    fn load_session(&mut self, id: &str) -> Result<serde_json::Value, String>;
}

fn send<W: Write>(writer: &Mutex<W>, value: &serde_json::Value) -> Result<(), FramingError> {
    let mut guard = writer.lock().expect("acp writer mutex poisoned");
    framing::write_message(&mut *guard, value)
}

/// Serve the ACP protocol over `reader`/`writer` until the peer
/// disconnects (clean EOF on the reader).
///
/// # Errors
///
/// Returns a [`FramingError`] only for a fatal transport failure on
/// the dispatch path. A clean EOF is `Ok(())`. Malformed individual
/// messages are answered with a JSON-RPC error and do not stop the
/// server.
pub fn serve<R, W, B>(reader: R, writer: W, mut backend: B) -> Result<(), FramingError>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
    B: AcpBackend,
{
    let writer = Arc::new(Mutex::new(writer));
    let cancel = Cancel::new();
    let (tx, rx) = mpsc::channel::<AcpRequest>();
    let perm: Arc<Mutex<Option<mpsc::Sender<PermissionResponse>>>> = Arc::new(Mutex::new(None));

    let reader_handle = {
        let writer = Arc::clone(&writer);
        let cancel = cancel.clone();
        let perm = Arc::clone(&perm);
        thread::spawn(move || {
            let mut reader = reader;
            loop {
                match framing::read_message(&mut reader) {
                    Ok(None) | Err(_) => break,
                    Ok(Some(value)) => match schema::parse_request(&value) {
                        Ok(req) => match req.call {
                            AcpCall::Cancel => {
                                cancel.cancel();
                                if let Some(id) = &req.id {
                                    let _ = send(
                                        &writer,
                                        &schema::response_result(
                                            id,
                                            serde_json::json!({ "cancelled": true }),
                                        ),
                                    );
                                }
                            }
                            AcpCall::PermissionRespond(resp) => {
                                let routed = perm
                                    .lock()
                                    .expect("acp permission mutex poisoned")
                                    .as_ref()
                                    .is_some_and(|s| s.send(resp).is_ok());
                                if let Some(id) = &req.id {
                                    let _ = send(
                                        &writer,
                                        &schema::response_result(
                                            id,
                                            serde_json::json!({ "routed": routed }),
                                        ),
                                    );
                                }
                            }
                            _ => {
                                if tx.send(req).is_err() {
                                    break;
                                }
                            }
                        },
                        Err(e) => {
                            let _ = send(
                                &writer,
                                &schema::response_error(
                                    &serde_json::Value::Null,
                                    error_code::INVALID_REQUEST,
                                    &e.to_string(),
                                ),
                            );
                        }
                    },
                }
            }
        })
    };

    let result = dispatch(&rx, &writer, &cancel, &mut backend);
    drop(rx);
    reader_handle.join().ok();
    result
}

fn dispatch<W: Write, B: AcpBackend>(
    rx: &mpsc::Receiver<AcpRequest>,
    writer: &Mutex<W>,
    cancel: &Cancel,
    backend: &mut B,
) -> Result<(), FramingError> {
    for req in rx {
        let id = req.id.clone();
        match req.call {
            AcpCall::Initialize(_) => {
                if let Some(id) = &id {
                    send(writer, &schema::response_result(id, backend.server_info()))?;
                }
            }
            AcpCall::SessionList => {
                reply(writer, id.as_ref(), backend.list_sessions())?;
            }
            AcpCall::SessionLoad(p) => {
                reply(writer, id.as_ref(), backend.load_session(&p.id))?;
            }
            AcpCall::Prompt(p) => {
                cancel.reset();
                let mut emit = |event: serde_json::Value| {
                    let _ = send(writer, &schema::notification("event", event));
                };
                let outcome = backend.prompt(&p, cancel, &mut emit);
                reply(writer, id.as_ref(), outcome)?;
            }
            // Handled out-of-band on the reader thread; reaching here
            // means the client sent one with no prompt in flight.
            AcpCall::Cancel | AcpCall::PermissionRespond(_) => {
                if let Some(id) = &id {
                    send(
                        writer,
                        &schema::response_result(id, serde_json::json!({ "noop": true })),
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn reply<W: Write>(
    writer: &Mutex<W>,
    id: Option<&schema::RequestId>,
    outcome: Result<serde_json::Value, String>,
) -> Result<(), FramingError> {
    let Some(id) = id else { return Ok(()) };
    let msg = match outcome {
        Ok(value) => schema::response_result(id, value),
        Err(e) => schema::response_error(id, error_code::INTERNAL_ERROR, &e),
    };
    send(writer, &msg)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[derive(Default)]
    struct MockBackend {
        prompts: Vec<String>,
    }

    impl AcpBackend for MockBackend {
        fn server_info(&self) -> serde_json::Value {
            serde_json::json!({ "name": "mock", "version": "0" })
        }

        fn prompt(
            &mut self,
            params: &PromptParams,
            cancel: &Cancel,
            emit: &mut dyn FnMut(serde_json::Value),
        ) -> Result<serde_json::Value, String> {
            self.prompts.push(params.prompt.clone());
            emit(serde_json::json!({ "type": "message_start" }));
            if cancel.is_cancelled() {
                return Err("cancelled".to_owned());
            }
            emit(serde_json::json!({ "type": "message_end" }));
            Ok(serde_json::json!({ "ok": true }))
        }

        fn list_sessions(&mut self) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!([{ "id": "s1" }]))
        }

        fn load_session(&mut self, id: &str) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "loaded": id }))
        }
    }

    fn framed(value: &serde_json::Value) -> Vec<u8> {
        let mut buf = Vec::new();
        framing::write_message(&mut buf, value).unwrap();
        buf
    }

    fn drain(bytes: Vec<u8>) -> Vec<serde_json::Value> {
        let mut cur = Cursor::new(bytes);
        let mut out = Vec::new();
        while let Some(v) = framing::read_message(&mut cur).unwrap() {
            out.push(v);
        }
        out
    }

    #[derive(Clone)]
    struct SharedWrite(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWrite {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn initialize_prompt_and_session_calls_round_trip() {
        let mut input = Vec::new();
        input.extend(framed(
            &serde_json::json!({"id": 1, "method": "initialize"}),
        ));
        input.extend(framed(&serde_json::json!({
            "id": 2, "method": "prompt", "params": {"prompt": "hello"}
        })));
        input.extend(framed(
            &serde_json::json!({"id": 3, "method": "session/list"}),
        ));
        input.extend(framed(&serde_json::json!({
            "id": 4, "method": "session/load", "params": {"id": "abc"}
        })));

        let sink = Arc::new(Mutex::new(Vec::new()));
        serve(
            Cursor::new(input),
            SharedWrite(Arc::clone(&sink)),
            MockBackend::default(),
        )
        .expect("serve");

        let out = drain(sink.lock().unwrap().clone());
        assert_eq!(out[0]["id"], 1);
        assert_eq!(out[0]["result"]["name"], "mock");
        // prompt: two event notifications then the prompt response.
        assert_eq!(out[1]["method"], "event");
        assert_eq!(out[1]["params"]["type"], "message_start");
        assert_eq!(out[2]["params"]["type"], "message_end");
        assert_eq!(out[3]["id"], 2);
        assert_eq!(out[3]["result"]["ok"], true);
        assert_eq!(out[4]["id"], 3);
        assert_eq!(out[4]["result"][0]["id"], "s1");
        assert_eq!(out[5]["id"], 4);
        assert_eq!(out[5]["result"]["loaded"], "abc");
    }

    #[test]
    fn unknown_method_gets_an_error_response() {
        let input = framed(&serde_json::json!({"id": 9, "method": "bogus"}));
        let sink = Arc::new(Mutex::new(Vec::new()));
        serve(
            Cursor::new(input),
            SharedWrite(Arc::clone(&sink)),
            MockBackend::default(),
        )
        .expect("serve");
        let out = drain(sink.lock().unwrap().clone());
        assert_eq!(out[0]["error"]["code"], error_code::INVALID_REQUEST);
    }
}
