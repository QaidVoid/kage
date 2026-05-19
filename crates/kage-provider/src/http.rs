//! Shared `ureq` plumbing used by every HTTP provider.
//!
//! [`HttpClient`], [`send`], [`read_error_body`], and the error
//! mapping are provider-agnostic: they were living in `openai` and
//! reached into sideways by the Anthropic / Gemini / Responses
//! providers, which muddied the module layering. They belong here so
//! each provider is a peer that depends on a shared util, not on a
//! sibling.

use std::sync::{Arc, RwLock};

use kage_core::CancelFlag;

use crate::ProviderError;

/// Construct a [`ureq::Agent`] with status-code-as-error disabled so we
/// can surface the upstream response body in [`ProviderError::Http`]
/// instead of throwing it away.
///
/// Timeout values, given ureq 3.x's chained deadlines: a phase's
/// deadline is the minimum over the phase itself, its preceding phases,
/// and `Global`/`PerCall` (`CallTimings::next_timeout`). An unset
/// timeout drops out of the chain - it is not "infinite for the phase".
///
/// `recv_response` is left unset on purpose. `RecvBody`'s chain
/// includes `RecvResponse`, whose deadline is anchored at the start of
/// response receipt, so a set `recv_response` also caps the body: a
/// generation streaming longer than it dies mid-stream with
/// `Timeout(RecvResponse)` while data flows nonstop. Time-to-first-
/// response is still bounded by the send deadlines, which precede
/// `RecvResponse` in its chain.
///
/// `recv_body` is an idle timeout, not a total cap: the *current*
/// phase is measured from "now" and recomputed every read, so 600s is
/// the max silence between bytes, not a ceiling on a long answer.
/// Providers emit deltas or pings well inside that; the loop's
/// auto-retry backstops a genuine stall, so the grace is generous on
/// purpose to avoid killing a slow-but-alive generation. `global`
/// stays unset so an active generation is never capped by total time.
fn build_agent() -> ureq::Agent {
    use std::time::Duration;
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_resolve(Some(Duration::from_secs(15)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_send_request(Some(Duration::from_secs(600)))
        .timeout_send_body(Some(Duration::from_secs(600)))
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .build()
        .new_agent()
}

/// A pooled [`ureq::Agent`] that can swap its connection pool out from
/// under itself when a connection turns out to be dead.
///
/// ureq keeps idle keep-alive sockets in a per-host pool and hands one
/// back to the next request. If that socket is half-dead - the server
/// silently dropped it, or it is wedged behind backpressure - the
/// reused connection stalls on connect/send and the request fails with
/// a transport timeout. ureq 3.x exposes no way to evict a single
/// pooled connection, so [`recycle`](Self::recycle) replaces the whole
/// agent with a fresh one; the old pool (and every socket in it) is
/// dropped, and the next [`agent`](Self::agent) snapshot dials a brand
/// new connection. Keep-alive reuse is otherwise preserved, so this
/// only pays the reconnect cost when a connection actually went bad.
///
/// Cloning shares the same swappable slot, so a clone moved onto a
/// worker thread recycles the pool the foreground will see next.
#[derive(Debug, Clone)]
pub(crate) struct HttpClient {
    agent: Arc<RwLock<ureq::Agent>>,
}

impl HttpClient {
    /// Build a client with a fresh pooled agent.
    pub(crate) fn new() -> Self {
        Self {
            agent: Arc::new(RwLock::new(build_agent())),
        }
    }

    /// Snapshot the current agent for one request. The clone shares the
    /// live pool, so it still benefits from keep-alive; a concurrent
    /// [`recycle`](Self::recycle) only affects *later* snapshots.
    fn agent(&self) -> ureq::Agent {
        // A poisoned lock means some thread panicked while holding the
        // guard. The agent handle behind it is still a valid, usable
        // value (recycle, the only writer, swaps it in one move and
        // cannot panic mid-update), so recovering the guard is correct
        // here - not a swallowed failure.
        self.agent
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Drop the pooled connections by swapping in a fresh agent so the
    /// next [`agent`](Self::agent) snapshot dials a new connection.
    fn recycle(&self) {
        let fresh = build_agent();
        let mut guard = self
            .agent
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = fresh;
    }

    /// Map a transport-time [`ureq::Error`] onto a [`ProviderError`],
    /// first recycling the pool when the failure implicates a dead or
    /// stale connection so the next attempt dials fresh.
    fn on_transport_error(&self, err: ureq::Error) -> ProviderError {
        if is_stale_connection_error(&err) {
            self.recycle();
        }
        map_ureq_error(err)
    }
}

/// True for transport failures that point at a dead or stalled
/// connection - the DNS/connect/send phases, an outright connect
/// failure, or a reset/aborted socket - rather than a slow but live
/// server still streaming a response. On these the pooled keep-alive
/// socket is suspect, so the caller recycles the agent to force a fresh
/// dial; a `RecvResponse`/`RecvBody` timeout is *not* included because
/// that is a slow generation, where reconnecting would only restart it.
fn is_stale_connection_error(err: &ureq::Error) -> bool {
    match err {
        ureq::Error::Timeout(phase) => matches!(
            phase,
            ureq::Timeout::Connect | ureq::Timeout::SendRequest | ureq::Timeout::SendBody
        ),
        ureq::Error::ConnectionFailed => true,
        ureq::Error::Io(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

/// Run a provider's request-and-headers call on `client`, polling
/// `cancel` from the foreground so a slow provider does not block the
/// cancel flag (see [`crate::cancelable::cancellable_call`]).
///
/// `build` receives a pooled agent snapshot and issues the POST; on a
/// stale-connection failure the client's pool is recycled before the
/// error is returned, so the *next* call dials a fresh connection
/// rather than reusing the wedged socket.
pub(crate) fn send<F>(
    client: &HttpClient,
    cancel: &CancelFlag,
    build: F,
) -> Result<ureq::http::Response<ureq::Body>, ProviderError>
where
    F: FnOnce(&ureq::Agent) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>
        + Send
        + 'static,
{
    let client = client.clone();
    crate::cancelable::cancellable_call(cancel, move || {
        let agent = client.agent();
        build(&agent).map_err(|e| client.on_transport_error(e))
    })
}

/// Issue a synchronous (non-streaming) request through `client`,
/// recycling the pool on a stale-connection failure exactly like
/// [`send`] but without the cancel-polling worker thread.
pub(crate) fn send_blocking<F>(
    client: &HttpClient,
    build: F,
) -> Result<ureq::http::Response<ureq::Body>, ProviderError>
where
    F: FnOnce(&ureq::Agent) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
{
    let agent = client.agent();
    build(&agent).map_err(|e| client.on_transport_error(e))
}

/// Read the body of a non-2xx response into [`ProviderError::Http`].
///
/// Caps the body at 8 KiB so a misbehaving upstream cannot blow up our
/// error strings; what we keep is enough to surface the JSON error
/// payload that every major provider returns for 4xx/5xx.
pub(crate) fn read_error_body(
    status: u16,
    response: ureq::http::Response<ureq::Body>,
) -> ProviderError {
    use std::io::Read as _;
    let mut buf = Vec::new();
    let _ = response
        .into_body()
        .into_reader()
        .take(8 * 1024)
        .read_to_end(&mut buf);
    let body = String::from_utf8_lossy(&buf).into_owned();
    ProviderError::Http { status, body }
}

/// Map a transport-time [`ureq::Error`] (from sending the request or
/// reading headers) onto a [`ProviderError`]. A bare status code keeps
/// an empty body; the caller reads the real body separately via
/// [`read_error_body`].
fn map_ureq_error(err: ureq::Error) -> ProviderError {
    match err {
        ureq::Error::StatusCode(code) => ProviderError::Http {
            status: code,
            body: String::new(),
        },
        ureq::Error::Io(e) => ProviderError::Transport(e.to_string()),
        other => ProviderError::Transport(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_classifier_flags_send_phase_timeouts_only() {
        assert!(is_stale_connection_error(&ureq::Error::Timeout(
            ureq::Timeout::SendRequest
        )));
        assert!(is_stale_connection_error(&ureq::Error::Timeout(
            ureq::Timeout::SendBody
        )));
        assert!(is_stale_connection_error(&ureq::Error::Timeout(
            ureq::Timeout::Connect
        )));
        assert!(is_stale_connection_error(&ureq::Error::ConnectionFailed));
        // A slow generation (response/body recv) is a live server, not
        // a dead socket: reconnecting would only restart the work.
        assert!(!is_stale_connection_error(&ureq::Error::Timeout(
            ureq::Timeout::RecvResponse
        )));
        assert!(!is_stale_connection_error(&ureq::Error::Timeout(
            ureq::Timeout::RecvBody
        )));
    }

    #[test]
    fn on_transport_error_recycles_then_maps() {
        let client = HttpClient::new();
        // A send-phase timeout is a stale-connection error: this drives
        // the recycle path (must not panic) and still maps to a
        // transport error so the caller surfaces it unchanged.
        let mapped = client.on_transport_error(ureq::Error::Timeout(ureq::Timeout::SendRequest));
        assert!(matches!(mapped, ProviderError::Transport(_)));
        // A bare status code is not a connection problem; it must map
        // to an HTTP error with an empty body for the caller to fill.
        let mapped = client.on_transport_error(ureq::Error::StatusCode(503));
        assert!(matches!(
            mapped,
            ProviderError::Http {
                status: 503,
                body
            } if body.is_empty()
        ));
        // The client is still usable after a recycle.
        let _ = client.agent();
    }
}
