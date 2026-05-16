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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::framing::{self, FramingError};
use crate::schema::{self, AcpCall, AcpRequest, PermissionResponse, PromptParams, error_code};

/// Shared output sink: a boxed writer behind a mutex so the reader
/// thread (control acks) and the dispatcher (responses, events,
/// permission prompts) can all write framed messages.
type SharedWriter = Arc<Mutex<dyn Write + Send>>;

/// Slot the dispatcher installs a one-shot response channel into for
/// the duration of a `permission/request`; the reader thread routes
/// the client's `permission/respond` here.
type PermSlot = Arc<Mutex<Option<mpsc::Sender<PermissionResponse>>>>;

/// The client's verdict for a tool call the agent asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// Run the tool call.
    Allow,
    /// Block the tool call, with an optional client-supplied reason.
    Deny {
        /// Why the call was denied, if the client said.
        reason: Option<String>,
    },
}

/// Handle the host's `before_tool_call` hook uses to ask the client
/// whether a tool may run. Cloning is cheap (all shared handles); the
/// agent backend keeps one and calls [`Permission::request`] per tool
/// call. The call blocks until the client answers, the run is
/// cancelled, or the client disconnects. The agent never auto-approves.
#[derive(Clone)]
pub struct Permission {
    writer: SharedWriter,
    slot: PermSlot,
    cancel: Cancel,
    next_id: Arc<AtomicU64>,
}

impl Permission {
    /// Ask the client to allow or deny a tool call. Emits a
    /// `permission/request` notification and blocks for the matching
    /// `permission/respond`. A cancelled run or a disconnected client
    /// resolves to [`PermissionOutcome::Deny`].
    #[must_use]
    pub fn request(&self, name: &str, input: &serde_json::Value) -> PermissionOutcome {
        let id = format!("perm-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = mpsc::channel();
        *self.slot.lock().expect("acp permission mutex poisoned") = Some(tx);
        let _ = send(
            &self.writer,
            &schema::notification(
                "permission/request",
                serde_json::json!({ "id": id, "name": name, "input": input }),
            ),
        );
        loop {
            if self.cancel.is_cancelled() {
                return PermissionOutcome::Deny {
                    reason: Some("cancelled".to_owned()),
                };
            }
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(resp) if resp.id == id => {
                    return if resp.allow {
                        PermissionOutcome::Allow
                    } else {
                        PermissionOutcome::Deny {
                            reason: resp.reason,
                        }
                    };
                }
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return PermissionOutcome::Deny {
                        reason: Some("client disconnected".to_owned()),
                    };
                }
            }
        }
    }
}

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
    /// `permission` is the human-in-the-loop gate the agent must call
    /// before any tool runs (it never auto-approves); `emit` receives
    /// each loop event already serialized as a JSON value (the same
    /// alphabet as `kage -p --json`), which the server wraps in an
    /// `event` notification.
    ///
    /// # Errors
    ///
    /// Returns an error string (sent as a JSON-RPC error) when the
    /// model is unresolvable or the run fails to start.
    fn prompt(
        &mut self,
        params: &PromptParams,
        cancel: &Cancel,
        permission: &Permission,
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

fn send(writer: &Mutex<dyn Write + Send>, value: &serde_json::Value) -> Result<(), FramingError> {
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
    let writer: SharedWriter = Arc::new(Mutex::new(writer));
    let cancel = Cancel::new();
    let (tx, rx) = mpsc::channel::<AcpRequest>();
    let perm: PermSlot = Arc::new(Mutex::new(None));

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

    let result = dispatch(&rx, &writer, &cancel, &perm, &mut backend);
    drop(rx);
    reader_handle.join().ok();
    result
}

fn dispatch<B: AcpBackend>(
    rx: &mpsc::Receiver<AcpRequest>,
    writer: &SharedWriter,
    cancel: &Cancel,
    perm: &PermSlot,
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
                let permission = Permission {
                    writer: Arc::clone(writer),
                    slot: Arc::clone(perm),
                    cancel: cancel.clone(),
                    next_id: Arc::new(AtomicU64::new(0)),
                };
                let mut emit = |event: serde_json::Value| {
                    let _ = send(writer, &schema::notification("event", event));
                };
                let outcome = backend.prompt(&p, cancel, &permission, &mut emit);
                *perm.lock().expect("acp permission mutex poisoned") = None;
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

fn reply(
    writer: &Mutex<dyn Write + Send>,
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
            _permission: &Permission,
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

    fn make_permission() -> (Permission, PermSlot) {
        let writer: SharedWriter = Arc::new(Mutex::new(Vec::<u8>::new()));
        let slot: PermSlot = Arc::new(Mutex::new(None));
        let perm = Permission {
            writer,
            slot: Arc::clone(&slot),
            cancel: Cancel::new(),
            next_id: Arc::new(AtomicU64::new(0)),
        };
        (perm, slot)
    }

    fn route_after_install(slot: &PermSlot, resp: PermissionResponse) {
        let tx = loop {
            if let Some(s) = slot.lock().unwrap().clone() {
                break s;
            }
            thread::yield_now();
        };
        tx.send(resp).unwrap();
    }

    #[test]
    fn permission_request_resolves_on_allow() {
        let (perm, slot) = make_permission();
        let h = thread::spawn(move || perm.request("bash", &serde_json::json!({"cmd": "ls"})));
        route_after_install(
            &slot,
            PermissionResponse {
                id: "perm-0".to_owned(),
                allow: true,
                reason: None,
            },
        );
        assert_eq!(h.join().unwrap(), PermissionOutcome::Allow);
    }

    #[test]
    fn permission_request_resolves_on_deny_with_reason() {
        let (perm, slot) = make_permission();
        let h = thread::spawn(move || perm.request("write", &serde_json::json!({})));
        route_after_install(
            &slot,
            PermissionResponse {
                id: "perm-0".to_owned(),
                allow: false,
                reason: Some("not allowed".to_owned()),
            },
        );
        assert_eq!(
            h.join().unwrap(),
            PermissionOutcome::Deny {
                reason: Some("not allowed".to_owned())
            }
        );
    }

    #[test]
    fn permission_request_denies_when_cancelled() {
        let (perm, _slot) = make_permission();
        perm.cancel.cancel();
        assert_eq!(
            perm.request("bash", &serde_json::json!({})),
            PermissionOutcome::Deny {
                reason: Some("cancelled".to_owned())
            }
        );
    }

    #[derive(Default)]
    struct PermBackend;

    impl AcpBackend for PermBackend {
        fn server_info(&self) -> serde_json::Value {
            serde_json::json!({ "name": "perm" })
        }

        fn prompt(
            &mut self,
            _params: &PromptParams,
            _cancel: &Cancel,
            permission: &Permission,
            emit: &mut dyn FnMut(serde_json::Value),
        ) -> Result<serde_json::Value, String> {
            emit(serde_json::json!({ "type": "message_start" }));
            match permission.request("bash", &serde_json::json!({ "cmd": "ls" })) {
                PermissionOutcome::Allow => {
                    emit(serde_json::json!({ "type": "message_end" }));
                    Ok(serde_json::json!({ "status": "completed", "permission": "allowed" }))
                }
                PermissionOutcome::Deny { .. } => {
                    Ok(serde_json::json!({ "status": "completed", "permission": "denied" }))
                }
            }
        }

        fn list_sessions(&mut self) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!([]))
        }

        fn load_session(&mut self, _id: &str) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({}))
        }
    }

    fn permission_round_trip(allow: bool) -> Vec<serde_json::Value> {
        use std::io::BufReader;

        let (c2s_r, mut c2s_w) = std::io::pipe().expect("client->server pipe");
        let (s2c_r, s2c_w) = std::io::pipe().expect("server->client pipe");

        let server = thread::spawn(move || serve(BufReader::new(c2s_r), s2c_w, PermBackend));

        framing::write_message(
            &mut c2s_w,
            &serde_json::json!({"id": 1, "method": "prompt", "params": {"prompt": "hi"}}),
        )
        .unwrap();

        let mut client = BufReader::new(s2c_r);
        let mut frames = Vec::new();
        loop {
            let v = framing::read_message(&mut client)
                .expect("read")
                .expect("frame before EOF");
            if v.get("method").and_then(|m| m.as_str()) == Some("permission/request") {
                let pid = v["params"]["id"].as_str().unwrap().to_owned();
                framing::write_message(
                    &mut c2s_w,
                    &serde_json::json!({
                        "id": 2,
                        "method": "permission/respond",
                        "params": {"id": pid, "allow": allow},
                    }),
                )
                .unwrap();
            }
            let done = v.get("id") == Some(&serde_json::json!(1)) && v.get("result").is_some();
            frames.push(v);
            if done {
                break;
            }
        }

        drop(c2s_w);
        server.join().unwrap().expect("serve ok");
        frames
    }

    #[test]
    fn acp_permission_round_trip_allow() {
        let frames = permission_round_trip(true);
        let req = frames
            .iter()
            .find(|f| f.get("method").and_then(|m| m.as_str()) == Some("permission/request"))
            .expect("a permission/request was sent");
        assert_eq!(req["params"]["name"], "bash");
        let resp = frames
            .iter()
            .find(|f| f.get("id") == Some(&serde_json::json!(1)) && f.get("result").is_some())
            .unwrap();
        assert_eq!(resp["result"]["permission"], "allowed");
        assert!(
            frames.iter().any(|f| f["params"]["type"] == "message_end"),
            "an event must follow an allowed call"
        );
    }

    #[test]
    fn acp_permission_round_trip_deny() {
        let frames = permission_round_trip(false);
        let resp = frames
            .iter()
            .find(|f| f.get("id") == Some(&serde_json::json!(1)) && f.get("result").is_some())
            .unwrap();
        assert_eq!(resp["result"]["permission"], "denied");
        assert!(
            !frames.iter().any(|f| f["params"]["type"] == "message_end"),
            "a denied call must not emit the post-tool event"
        );
    }

    #[test]
    fn permission_ignores_stale_id_then_accepts_match() {
        let (perm, slot) = make_permission();
        let h = thread::spawn(move || perm.request("bash", &serde_json::json!({})));
        let tx = loop {
            if let Some(s) = slot.lock().unwrap().clone() {
                break s;
            }
            thread::yield_now();
        };
        tx.send(PermissionResponse {
            id: "perm-99".to_owned(),
            allow: true,
            reason: None,
        })
        .unwrap();
        tx.send(PermissionResponse {
            id: "perm-0".to_owned(),
            allow: false,
            reason: None,
        })
        .unwrap();
        assert_eq!(h.join().unwrap(), PermissionOutcome::Deny { reason: None });
    }
}
