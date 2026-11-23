//! Transport facade for agent-originated ACP messages.

#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::{Duration, Instant};

use kage_core::CancelFlag;
#[cfg(unix)]
use kage_jsonrpc::unix_peer::RequestWaitOutcome;
use kage_jsonrpc::{Peer, RpcError};
use serde_json::Value;

/// A cloneable agent connection.
///
/// Generic connections cannot interrupt a blocked write. On Unix, the native
/// pipe transport applies the configured timeout to each write, including
/// request dispatch and cancellation notices. Waiting for a response remains
/// cancellable without imposing a deadline on the user's decision.
#[derive(Clone)]
pub struct AgentPeer {
    inner: Transport,
    #[cfg(unix)]
    failure: Option<Arc<Mutex<Option<RpcError>>>>,
    #[cfg(all(test, unix))]
    barrier_reached: Option<std::sync::mpsc::Sender<()>>,
}

#[derive(Clone)]
enum Transport {
    Generic(Peer),
    #[cfg(unix)]
    Unix(kage_jsonrpc::unix_peer::UnixPeer, Duration),
}

/// A committed Unix request that can be waited on independently of dispatch.
///
/// Dropping this handle removes the local pending request without blocking or
/// sending a cancellation notice. It does not cancel the remote operation.
/// Callers must settle the request with [`Self::wait`] or shut down the
/// connection when abandoning it.
#[cfg(unix)]
#[must_use = "wait for the request or shut down the connection"]
pub struct DispatchedRequest {
    peer: AgentPeer,
    request: kage_jsonrpc::unix_peer::CommittedRequest,
}

impl From<Peer> for AgentPeer {
    fn from(peer: Peer) -> Self {
        Self {
            inner: Transport::Generic(peer),
            #[cfg(unix)]
            failure: None,
            #[cfg(all(test, unix))]
            barrier_reached: None,
        }
    }
}

impl AgentPeer {
    /// Wrap a native Unix pipe peer with a timeout for each outgoing write.
    #[cfg(unix)]
    #[must_use]
    pub fn unix(peer: kage_jsonrpc::unix_peer::UnixPeer, write_timeout: Duration) -> Self {
        Self {
            inner: Transport::Unix(peer, write_timeout),
            failure: Some(Arc::new(Mutex::new(None))),
            #[cfg(test)]
            barrier_reached: None,
        }
    }

    /// Whether requests can be dispatched without waiting for their response,
    /// which only the native Unix pipe transport supports.
    #[cfg(unix)]
    #[must_use]
    pub fn splits_requests(&self) -> bool {
        matches!(self.inner, Transport::Unix(..))
    }

    /// Send a notification.
    ///
    /// # Errors
    /// Returns the transport error, including a native write timeout.
    pub fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        let result = match &self.inner {
            Transport::Generic(peer) => peer.notify(method, params),
            #[cfg(unix)]
            Transport::Unix(peer, timeout) => deadline(*timeout)
                .and_then(|deadline| peer.notify(method, params, Some(deadline), None)),
        };
        self.record(result)
    }

    /// Send a response. One too large to send goes as an error response
    /// instead, so the request does not wait forever.
    ///
    /// # Errors
    /// Returns the transport error, including a native write timeout.
    pub fn respond(&self, id: &Value, result: Result<Value, RpcError>) -> Result<(), RpcError> {
        let write = |result| match &self.inner {
            Transport::Generic(peer) => peer.respond(id, result),
            #[cfg(unix)]
            Transport::Unix(peer, timeout) => deadline(*timeout)
                .and_then(|deadline| peer.respond(id, result, Some(deadline), None)),
        };
        let outcome = match write(result) {
            Err(error) if error.is_not_sent() => write(Err(RpcError::internal(format!(
                "the answer could not be sent: {}",
                error.message
            )))),
            other => other,
        };
        self.record(outcome)
    }

    /// Send a request and wait for the response with no deadline.
    ///
    /// # Errors
    /// Returns the remote error or a transport error.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match &self.inner {
            Transport::Generic(peer) => peer.request(method, params),
            #[cfg(unix)]
            Transport::Unix(_, _) => self.request_cancellable(method, params, &CancelFlag::new()),
        }
    }

    /// Send a request and give up after `timeout` without an answer.
    ///
    /// # Errors
    /// Returns the remote error, [`RpcError::timed_out`] when the deadline
    /// passes, or a transport error.
    pub fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: std::time::Duration,
    ) -> Result<Value, RpcError> {
        match &self.inner {
            Transport::Generic(peer) => peer.request_timeout(method, params, timeout),
            #[cfg(unix)]
            Transport::Unix(_, _) => {
                let until = deadline(timeout)?;
                let cancel = CancelFlag::new();
                self.dispatch_cancellable(method, params, &cancel)?
                    .wait_until(&cancel, Some(until))
            }
        }
    }

    /// Send a request and wait for a response or cancellation.
    ///
    /// Native cancellation sends ACP's cancellation notice after a committed
    /// request. A failure to send that notice is returned to the caller.
    /// The generic peer's cancellation notice depends on its constructor.
    ///
    /// # Errors
    /// Returns the remote error, cancellation, or a transport error.
    pub fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancel: &CancelFlag,
    ) -> Result<Value, RpcError> {
        match &self.inner {
            Transport::Generic(peer) => peer.request_cancellable(method, params, cancel),
            #[cfg(unix)]
            Transport::Unix(_, _) => self
                .dispatch_cancellable(method, params, cancel)?
                .wait(cancel),
        }
    }
    /// Commit a request to a native Unix pipe without waiting for its response.
    ///
    /// The configured write timeout and cancellation apply to dispatch. A
    /// successful return means the entire request frame was written. Generic
    /// transports do not support this operation.
    ///
    /// # Errors
    /// Returns an error if dispatch fails or the transport is generic.
    #[cfg(unix)]
    pub fn dispatch_cancellable(
        &self,
        method: &str,
        params: Value,
        cancel: &CancelFlag,
    ) -> Result<DispatchedRequest, RpcError> {
        let Transport::Unix(peer, timeout) = &self.inner else {
            return Err(RpcError::internal(
                "split dispatch requires a Unix transport",
            ));
        };
        let deadline = match deadline(*timeout) {
            Ok(deadline) => deadline,
            Err(error) => return self.record(Err(error)),
        };
        let request = match peer.dispatch_typed(method, params, Some(deadline), Some(cancel)) {
            Ok(request) => request,
            Err(
                kage_jsonrpc::unix_peer::DispatchError::CleanEof(error)
                | kage_jsonrpc::unix_peer::DispatchError::Cancelled(error),
            ) => return Err(error),
            Err(kage_jsonrpc::unix_peer::DispatchError::Other(error)) => {
                return self.record(Err(error));
            }
        };
        Ok(DispatchedRequest {
            peer: self.clone(),
            request,
        })
    }

    /// Remembers the first error that broke the connection and shuts it
    /// down, so later writes fail at once rather than each waiting out
    /// a stalled client. A message refused before any byte of it was
    /// written leaves the connection as it is.
    fn record<T>(&self, result: Result<T, RpcError>) -> Result<T, RpcError> {
        #[cfg(unix)]
        if let (Some(failure), Err(error)) = (&self.failure, &result)
            && !error.is_not_sent()
        {
            let mut first = failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if first.is_none() {
                *first = Some(error.clone());
                if let Transport::Unix(peer, _) = &self.inner {
                    peer.shutdown();
                }
            }
        }
        result
    }

    #[cfg(unix)]
    pub(crate) fn failure(&self) -> Option<RpcError> {
        let failure = self.failure.as_ref().and_then(|failure| {
            failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        });
        failure.or_else(|| match &self.inner {
            Transport::Unix(peer, _)
                if peer.read_status() == kage_jsonrpc::unix_peer::ReadStatus::Failed =>
            {
                Some(RpcError::internal("Unix transport reader failed"))
            }
            _ => None,
        })
    }
}

#[cfg(unix)]
impl DispatchedRequest {
    /// Wait for the response, its ordering acknowledgement, or cancellation.
    ///
    /// Cancellation after dispatch sends a bounded cancellation notice unless
    /// a response barrier was already received. Waiting for a response or its
    /// acknowledgement has no write deadline. Any remote response without a
    /// barrier fails the transport closed. Locally detected errors are returned
    /// unchanged.
    ///
    /// # Errors
    /// Returns the remote error, cancellation, or a transport error.
    pub fn wait(self, cancel: &CancelFlag) -> Result<Value, RpcError> {
        self.wait_until(cancel, None)
    }

    /// [`Self::wait`] that gives up at `until`, telling the peer to drop
    /// the request the way a cancellation does.
    fn wait_until(self, cancel: &CancelFlag, until: Option<Instant>) -> Result<Value, RpcError> {
        let Self {
            peer: agent,
            request,
        } = self;
        let Transport::Unix(peer, timeout) = &agent.inner else {
            unreachable!("dispatched requests require a Unix transport");
        };
        let id = request.id();
        let abandon = || {
            agent.record(deadline(*timeout).and_then(|deadline| {
                peer.notify(
                    "$/cancel_request",
                    serde_json::json!({"requestId": id}),
                    Some(deadline),
                    None,
                )
            }))
        };

        match request.wait_typed(until, Some(cancel)) {
            RequestWaitOutcome::Delivered {
                result,
                barrier: Some(barrier),
            } => {
                #[cfg(test)]
                if let Some(reached) = &agent.barrier_reached {
                    let _ = reached.send(());
                }
                let acknowledged = agent.record(
                    barrier
                        .wait_cancellable_until(cancel, || !peer.is_write_open())
                        .map_err(|_| RpcError::internal("response ordering barrier was dropped")),
                )?;
                if acknowledged && !cancel.is_cancelled() {
                    result
                } else if cancel.is_cancelled() {
                    Err(RpcError::new(-32800, "request cancelled"))
                } else {
                    agent.record(Err(RpcError::internal(
                        "connection closed before response ordering acknowledgement",
                    )))
                }
            }
            RequestWaitOutcome::Delivered { barrier: None, .. } => agent.record(Err(
                RpcError::internal("response ordering barrier was not delivered"),
            )),
            RequestWaitOutcome::LocalError(error) => {
                if until.is_some_and(|until| Instant::now() >= until) {
                    abandon()?;
                }
                Err(error)
            }
            RequestWaitOutcome::Cancelled => {
                abandon()?;
                Err(RpcError::new(-32800, "request cancelled"))
            }
        }
    }
}

#[cfg(unix)]
fn deadline(timeout: Duration) -> Result<Instant, RpcError> {
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| RpcError::internal("write timeout exceeds clock range"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use kage_jsonrpc::{Inbound, OrderedInbound, connect};
    use std::io::BufReader;
    use std::thread;

    #[test]
    fn clean_eof_rejects_dispatch_without_recording_failure() {
        use std::io::BufRead;
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, _inbound, reader_thread) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        drop(input);
        reader_thread.join().unwrap();
        assert!(
            peer.dispatch_cancellable("ask", Value::Null, &CancelFlag::new())
                .is_err()
        );
        assert!(peer.failure().is_none());
        peer.notify("after", Value::Null).unwrap();
        peer.respond(&serde_json::json!(42), Ok(Value::Null))
            .unwrap();
        let mut output = BufReader::new(output);
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["method"],
            "after"
        );
        line.clear();
        output.read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["id"], 42);
        assert!(peer.failure().is_none());
    }

    #[test]
    fn dispatch_commits_before_notification_and_waits_independently() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let (client, inbox, _) = connect(BufReader::new(output), input);
        let request = peer
            .dispatch_cancellable("ask", Value::Null, &CancelFlag::new())
            .unwrap();
        peer.notify("after", Value::Null).unwrap();
        let Inbound::Request { id, method, .. } =
            inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        assert_eq!(method, "ask");
        assert!(
            matches!(inbox.recv_timeout(Duration::from_secs(1)).unwrap(), Inbound::Notification { method, .. } if method == "after")
        );
        let (done, result) = std::sync::mpsc::channel();
        thread::spawn(move || done.send(request.wait(&CancelFlag::new())).unwrap());
        client.respond(&id, Ok(serde_json::json!(42))).unwrap();
        let OrderedInbound::Barrier(marker) = inbound.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected barrier");
        };
        assert!(result.try_recv().is_err());
        marker.acknowledge();
        assert_eq!(
            result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap(),
            42
        );
    }

    #[test]
    fn split_cancellation_sends_notice_after_committed_request() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, _, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let (_client, inbox, _) = connect(BufReader::new(output), input);
        let cancel = CancelFlag::new();
        let request = peer
            .dispatch_cancellable("ask", Value::Null, &cancel)
            .unwrap();
        let Inbound::Request { id, .. } = inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        cancel.cancel();
        assert_eq!(request.wait(&cancel).unwrap_err().code, -32800);
        assert!(
            matches!(inbox.recv_timeout(Duration::from_secs(1)).unwrap(), Inbound::Notification { method, params } if method == "$/cancel_request" && params == serde_json::json!({"requestId": id}))
        );
        assert!(matches!(
            inbox.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(peer.failure().is_none());
    }

    /// A Unix request that gets no answer in time fails as timed out
    /// and tells the peer to drop it, with no timer thread involved.
    #[test]
    fn an_unanswered_unix_request_times_out_and_is_withdrawn() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, _, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let (_client, inbox, _) = connect(BufReader::new(output), input);
        let started = Instant::now();
        let error = peer
            .request_timeout("ask", Value::Null, Duration::from_millis(100))
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(error, RpcError::timed_out("ask"));
        let Inbound::Request { id, .. } = inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        assert!(
            matches!(inbox.recv_timeout(Duration::from_secs(1)).unwrap(), Inbound::Notification { method, params } if method == "$/cancel_request" && params == serde_json::json!({"requestId": id}))
        );
        assert!(peer.failure().is_none());
    }

    #[test]
    fn split_cancellation_takes_priority_over_buffered_response() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let (client, inbox, _) = connect(BufReader::new(output), input);
        let cancel = CancelFlag::new();
        let request = peer
            .dispatch_cancellable("ask", Value::Null, &cancel)
            .unwrap();
        let Inbound::Request { id, .. } = inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        client.respond(&id, Ok(Value::Null)).unwrap();
        client.notify("response_processed", Value::Null).unwrap();
        let OrderedInbound::Barrier(marker) = inbound.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected barrier");
        };
        assert!(matches!(
            inbound.recv_timeout(Duration::from_secs(1)).unwrap(),
            OrderedInbound::Message(Inbound::Notification { method, .. }) if method == "response_processed"
        ));
        cancel.cancel();
        assert_eq!(request.wait(&cancel).unwrap_err().code, -32800);
        assert!(
            matches!(inbox.recv_timeout(Duration::from_secs(1)).unwrap(), Inbound::Notification { method, params } if method == "$/cancel_request" && params == serde_json::json!({"requestId": id}))
        );
        drop(marker);
        assert!(peer.failure().is_none());
    }

    #[test]
    fn split_rejects_generic_transport() {
        let (reader, input) = std::io::pipe().unwrap();
        let (_output, writer) = std::io::pipe().unwrap();
        let (generic, _, _) = connect(BufReader::new(reader), writer);
        let peer = AgentPeer::from(generic);
        let result = peer.dispatch_cancellable("ask", Value::Null, &CancelFlag::new());
        assert!(
            matches!(result, Err(error) if error.message.contains("requires a Unix transport"))
        );
        drop(input);
    }

    #[test]
    fn split_response_without_barrier_fails_closed() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let (client, inbox, _) = connect(BufReader::new(output), input);
        let request = peer
            .dispatch_cancellable("ask", Value::Null, &CancelFlag::new())
            .unwrap();
        let Inbound::Request { id, .. } = inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        drop(inbound);
        client.respond(&id, Ok(Value::Null)).unwrap();
        let error = request.wait(&CancelFlag::new()).unwrap_err();
        assert_eq!(error.message, "response ordering barrier was not delivered");
        assert_eq!(peer.failure(), Some(error));
    }

    /// A message too large to send fails alone: the connection stays
    /// up for the next one, and a too large answer reaches the asker as
    /// an error rather than never.
    #[test]
    fn an_oversized_message_spares_the_connection() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let (client, inbox, _) = connect(BufReader::new(output), input);
        let huge = Value::String("x".repeat(9 * 1024 * 1024));
        let error = peer.notify("big", huge.clone()).unwrap_err();
        assert!(error.is_not_sent(), "{error:?}");
        assert!(peer.failure().is_none());
        peer.notify("small", Value::Null).unwrap();
        assert!(
            matches!(inbox.recv_timeout(Duration::from_secs(1)).unwrap(), Inbound::Notification { method, .. } if method == "small")
        );
        let (done, answer) = std::sync::mpsc::channel();
        thread::spawn(move || done.send(client.request("ask", Value::Null)).unwrap());
        let OrderedInbound::Message(Inbound::Request { id, .. }) =
            inbound.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected the request");
        };
        peer.respond(&id, Ok(huge)).unwrap();
        let answer = answer.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            answer
                .as_ref()
                .is_err_and(|e| e.message.contains("could not be sent")),
            "{answer:?}"
        );
        assert!(peer.failure().is_none());
    }

    #[test]
    fn remote_cancellation_error_requires_and_obeys_barrier() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let (client, inbox, _) = connect(BufReader::new(output), input);
        let request = peer
            .dispatch_cancellable("ask", Value::Null, &CancelFlag::new())
            .unwrap();
        let Inbound::Request { id, .. } = inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        let remote = RpcError::new(-32800, "remote cancellation");
        client.respond(&id, Err(remote.clone())).unwrap();
        let OrderedInbound::Barrier(marker) = inbound.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected barrier");
        };
        let (done, result) = std::sync::mpsc::channel();
        thread::spawn(move || done.send(request.wait(&CancelFlag::new())).unwrap());
        assert!(result.try_recv().is_err());
        marker.acknowledge();
        assert_eq!(
            result.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(remote)
        );
        assert!(matches!(
            inbox.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(peer.failure().is_none());
    }

    #[test]
    fn remote_cancellation_error_without_barrier_fails_closed() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let (client, inbox, _) = connect(BufReader::new(output), input);
        let request = peer
            .dispatch_cancellable("ask", Value::Null, &CancelFlag::new())
            .unwrap();
        let Inbound::Request { id, .. } = inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        drop(inbound);
        client
            .respond(&id, Err(RpcError::new(-32800, "remote cancellation")))
            .unwrap();
        let error = request.wait(&CancelFlag::new()).unwrap_err();
        assert_eq!(error.message, "response ordering barrier was not delivered");
        assert_eq!(peer.failure(), Some(error));
        assert!(
            !matches!(inbox.recv_timeout(Duration::from_millis(100)), Ok(Inbound::Notification { method, .. }) if method == "$/cancel_request")
        );
    }

    #[test]
    fn malformed_response_remains_a_local_error() {
        use std::io::{BufRead, Write};

        let (reader, mut input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, _inbound, reader_thread) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let request = peer
            .dispatch_cancellable("ask", Value::Null, &CancelFlag::new())
            .unwrap();
        let mut output = BufReader::new(output);
        let mut frame = String::new();
        output.read_line(&mut frame).unwrap();
        let id = serde_json::from_str::<Value>(&frame).unwrap()["id"].clone();
        writeln!(input, "{}", serde_json::json!({"jsonrpc": "2.0", "id": id})).unwrap();
        let error = request.wait(&CancelFlag::new()).unwrap_err();
        assert_eq!(error.code, -32600);
        reader_thread.join().unwrap();
        assert!(peer.failure.as_ref().unwrap().lock().unwrap().is_none());
        frame.clear();
        assert_eq!(output.read_line(&mut frame).unwrap(), 0);
    }

    #[test]
    fn split_dropped_barrier_records_failure() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let (client, inbox, _) = connect(BufReader::new(output), input);
        let request = peer
            .dispatch_cancellable("ask", Value::Null, &CancelFlag::new())
            .unwrap();
        let Inbound::Request { id, .. } = inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        client.respond(&id, Ok(Value::Null)).unwrap();
        let OrderedInbound::Barrier(marker) = inbound.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected barrier");
        };
        drop(marker);
        let error = request.wait(&CancelFlag::new()).unwrap_err();
        assert_eq!(error.message, "response ordering barrier was dropped");
        assert_eq!(peer.failure(), Some(error));
    }

    #[test]
    fn cancellation_interrupts_an_unacknowledged_response_barrier() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let mut peer = AgentPeer::unix(unix, Duration::from_secs(1));
        let (reached, barrier_started) = std::sync::mpsc::channel();
        peer.barrier_reached = Some(reached);
        let check = peer.clone();
        let (client, inbox, _) = connect(BufReader::new(output), input);
        let cancel = CancelFlag::new();
        let flag = cancel.clone();
        let (done, result) = std::sync::mpsc::channel();
        thread::spawn(move || {
            done.send(peer.request_cancellable("ask", serde_json::json!({}), &flag))
                .unwrap();
        });
        let Inbound::Request { id, .. } = inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        client.respond(&id, Ok(serde_json::json!(true))).unwrap();
        let OrderedInbound::Barrier(marker) = inbound.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected barrier");
        };
        barrier_started
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        cancel.cancel();
        assert_eq!(
            result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err()
                .code,
            -32800
        );
        assert!(check.failure().is_none());
        drop(marker);
    }

    #[test]
    fn shutdown_interrupts_barrier_even_when_marker_is_retained() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, reader_thread) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix.clone(), Duration::from_secs(1));
        let (client, inbox, _) = connect(BufReader::new(output), input);
        let (done, result) = std::sync::mpsc::channel();
        thread::spawn(move || {
            done.send(peer.request_cancellable("ask", serde_json::json!({}), &CancelFlag::new()))
                .unwrap();
        });
        let Inbound::Request { id, .. } = inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        client.respond(&id, Ok(serde_json::json!(true))).unwrap();
        let OrderedInbound::Barrier(marker) = inbound.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected barrier");
        };
        unix.shutdown();
        assert!(
            result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_err()
        );
        drop(marker);
        drop(client);
        reader_thread.join().unwrap();
    }

    #[test]
    fn response_barrier_is_not_limited_by_write_timeout() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, _) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, Duration::from_millis(50));
        let check = peer.clone();
        let (client, inbox, _) = connect(BufReader::new(output), input);
        let task = thread::spawn(move || {
            peer.request_cancellable("ask", serde_json::json!({}), &CancelFlag::new())
        });
        let Inbound::Request { id, .. } = inbox.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected request");
        };
        client.respond(&id, Ok(serde_json::json!(true))).unwrap();
        let OrderedInbound::Barrier(marker) = inbound.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("expected barrier");
        };
        thread::sleep(Duration::from_millis(100));
        assert!(!task.is_finished());
        marker.acknowledge();
        assert_eq!(task.join().unwrap().unwrap(), serde_json::json!(true));
        assert!(check.failure().is_none());
    }
}
