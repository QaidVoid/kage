//! Shared `ureq` plumbing used by every HTTP provider.
//!
//! The pooled `HttpClient`, `send`, `read_error_body` and the
//! status-to-error mapping are provider-agnostic, so each provider
//! depends on this module rather than on a sibling provider.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use kage_core::{
    CancelFlag,
    sync::{read, write},
};

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
        read(&self.agent).clone()
    }

    /// Drop the pooled connections by swapping in a fresh agent so the
    /// next [`agent`](Self::agent) snapshot dials a new connection.
    fn recycle(&self) {
        let fresh = build_agent();
        let mut guard = write(&self.agent);
        *guard = fresh;
    }

    /// Map a transport-time [`ureq::Error`] onto a [`ProviderError`],
    /// first recycling the pool when the failure implicates a dead or
    /// stale connection so the next attempt dials fresh.
    fn on_transport_error(&self, err: ureq::Error, url: &str) -> ProviderError {
        if is_stale_connection_error(&err) {
            self.recycle();
        }
        map_ureq_error(err, url)
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

/// Run a provider's request-and-headers call on `client`, waiting on the
/// response and a watch on `cancel` together so a slow provider does not
/// delay cancellation (see [`crate::cancelable::cancellable_call`]).
///
/// `build` receives a pooled agent snapshot and `url`, and issues the
/// POST; on a stale-connection failure the client's pool is recycled
/// before the error is returned, so the *next* call dials a fresh
/// connection rather than reusing the wedged socket. A transport error
/// names the host and port of `url`, never its path or query.
pub(crate) fn send<F>(
    client: &HttpClient,
    cancel: &CancelFlag,
    url: String,
    build: F,
) -> Result<ureq::http::Response<ureq::Body>, ProviderError>
where
    F: FnOnce(&ureq::Agent, &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>
        + Send
        + 'static,
{
    let client = client.clone();
    crate::cancelable::cancellable_call(cancel, move || {
        let agent = client.agent();
        build(&agent, &url).map_err(|e| client.on_transport_error(e, &url))
    })
}

/// Read the body of a non-2xx response into a [`ProviderError`].
///
/// A 429 becomes [`ProviderError::RateLimited`], carrying the
/// provider's `Retry-After` hint (delta-seconds or HTTP-date) when one
/// is present. A 401 or 403 becomes [`ProviderError::Auth`] with a
/// short detail pulled from the body. Every other status stays
/// [`ProviderError::Http`] with the body capped at 8 KiB so a
/// misbehaving upstream cannot blow up our error strings.
pub(crate) fn read_error_body(
    status: u16,
    response: ureq::http::Response<ureq::Body>,
) -> ProviderError {
    use std::io::Read as _;
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut buf = Vec::new();
    let _ = response
        .into_body()
        .into_reader()
        .take(8 * 1024)
        .read_to_end(&mut buf);
    let body = String::from_utf8_lossy(&buf).into_owned();
    classify_http_error(
        status,
        parse_retry_after(retry_after.as_deref(), chrono::Utc::now()),
        body,
    )
}

/// Pure status-to-error mapping, split out of [`read_error_body`] so
/// tests can pin it without a live response.
fn classify_http_error(status: u16, retry_after: Option<Duration>, body: String) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited { retry_after },
        401 | 403 => ProviderError::Auth(auth_detail(status, &body)),
        _ => ProviderError::Http { status, body },
    }
}

/// Maximum characters of a non-JSON auth error body kept as detail.
const AUTH_DETAIL_MAX_CHARS: usize = 200;

/// Human-readable detail for an auth failure: the JSON body's
/// `error.message` or top-level `message`, else the raw body cut to
/// [`AUTH_DETAIL_MAX_CHARS`], else the bare status.
fn auth_detail(status: u16, body: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
    let message = parsed.as_ref().and_then(|json| {
        json.pointer("/error/message")
            .or_else(|| json.get("message"))
            .and_then(serde_json::Value::as_str)
    });
    if let Some(message) = message.map(str::trim).filter(|m| !m.is_empty()) {
        return message.to_owned();
    }
    let body = body.trim();
    if body.is_empty() {
        return format!("status {status}");
    }
    if body.chars().count() <= AUTH_DETAIL_MAX_CHARS {
        return body.to_owned();
    }
    let cut: String = body.chars().take(AUTH_DETAIL_MAX_CHARS).collect();
    format!("{cut}...")
}

/// Parse an HTTP `Retry-After` value: a delta in seconds, or an
/// HTTP-date (IMF-fixdate). A date in the past, or anything
/// unparseable, yields `None` so the caller falls back to its own
/// backoff.
fn parse_retry_after(value: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    (date.to_utc() - now).to_std().ok()
}

/// Map a transport-time [`ureq::Error`] (from sending the request or
/// reading headers) onto a [`ProviderError`]. A bare status code keeps
/// an empty body; the caller reads the real body separately via
/// [`read_error_body`]. A transport error names the host of `url`.
fn map_ureq_error(err: ureq::Error, url: &str) -> ProviderError {
    let detail = match err {
        ureq::Error::StatusCode(code) => {
            return ProviderError::Http {
                status: code,
                body: String::new(),
            };
        }
        ureq::Error::Io(e) => e.to_string(),
        other => other.to_string(),
    };
    match url_host(url) {
        Some(host) => ProviderError::Transport(format!("{detail} (host {host})")),
        None => ProviderError::Transport(detail),
    }
}

/// The host of `url`, with its port when one is given. The scheme,
/// credentials, path and query are left out.
fn url_host(url: &str) -> Option<String> {
    let uri = url.parse::<ureq::http::Uri>().ok()?;
    let host = uri.host()?;
    Some(match uri.port_u16() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
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
        let mapped = client.on_transport_error(
            ureq::Error::Timeout(ureq::Timeout::SendRequest),
            "http://localhost/v1",
        );
        assert!(matches!(mapped, ProviderError::Transport(_)));
        // A bare status code is not a connection problem; it must map
        // to an HTTP error with an empty body for the caller to fill.
        let mapped = client.on_transport_error(ureq::Error::StatusCode(503), "http://localhost/v1");
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

    #[test]
    fn transport_error_names_the_host_but_not_the_path_or_credentials() {
        let url = "http://user:secret@localhost:11434/v1/chat?key=abc";
        let err = map_ureq_error(ureq::Error::ConnectionFailed, url).to_string();
        assert!(err.contains("(host localhost:11434)"), "got {err}");
        for hidden in ["user", "secret", "/v1", "chat", "key=abc"] {
            assert!(!err.contains(hidden), "{hidden} leaked into {err}");
        }
        assert_eq!(
            url_host("https://api.anthropic.com/v1/messages").as_deref(),
            Some("api.anthropic.com")
        );
        assert_eq!(url_host("not a url"), None);
    }

    #[test]
    fn retry_after_delta_seconds_parses() {
        let now = chrono::Utc::now();
        assert_eq!(
            parse_retry_after(Some("12"), now),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            parse_retry_after(Some(" 3 "), now),
            Some(Duration::from_secs(3))
        );
    }

    #[test]
    fn retry_after_http_date_parses() {
        let now = chrono::DateTime::parse_from_rfc2822("Sun, 06 Nov 1994 08:49:37 GMT")
            .expect("fixed date")
            .to_utc();
        let minute_later = parse_retry_after(Some("Sun, 06 Nov 1994 08:50:37 GMT"), now);
        assert_eq!(minute_later, Some(Duration::from_secs(60)));
    }

    #[test]
    fn retry_after_past_date_and_garbage_are_none() {
        let now = chrono::Utc::now();
        assert_eq!(parse_retry_after(None, now), None);
        assert_eq!(parse_retry_after(Some(""), now), None);
        assert_eq!(parse_retry_after(Some("   "), now), None);
        assert_eq!(
            parse_retry_after(Some("Sun, 06 Nov 1994 08:49:37 GMT"), now),
            None,
            "a date in the past must not produce a wait"
        );
        assert_eq!(parse_retry_after(Some("soon"), now), None);
    }

    #[test]
    fn classify_maps_429_to_rate_limited_and_keeps_other_statuses_http() {
        assert!(matches!(
            classify_http_error(429, Some(Duration::from_secs(9)), String::new()),
            ProviderError::RateLimited {
                retry_after: Some(d)
            } if d == Duration::from_secs(9)
        ));
        assert!(matches!(
            classify_http_error(429, None, String::new()),
            ProviderError::RateLimited { retry_after: None }
        ));
        assert!(matches!(
            classify_http_error(500, None, "boom".into()),
            ProviderError::Http {
                status: 500,
                body
            } if body == "boom"
        ));
    }

    #[test]
    fn classify_maps_401_and_403_to_auth_with_the_json_message() {
        let body = r#"{"error":{"message":"token expired"}}"#;
        for status in [401, 403] {
            assert!(matches!(
                classify_http_error(status, None, body.into()),
                ProviderError::Auth(detail) if detail == "token expired"
            ));
        }
        assert!(matches!(
            classify_http_error(401, None, r#"{"message":"bad key"}"#.into()),
            ProviderError::Auth(detail) if detail == "bad key"
        ));
        assert!(matches!(
            classify_http_error(400, None, body.into()),
            ProviderError::Http { status: 400, .. }
        ));
    }

    #[test]
    fn auth_detail_falls_back_to_a_truncated_body_or_status() {
        let long = "x".repeat(500);
        let detail = auth_detail(401, &long);
        assert_eq!(detail.chars().count(), AUTH_DETAIL_MAX_CHARS + 3);
        assert!(detail.ends_with("..."));
        assert_eq!(auth_detail(401, " denied \n"), "denied");
        assert_eq!(auth_detail(403, ""), "status 403");
    }

    #[test]
    fn read_error_body_carries_retry_after_header_into_rate_limit() {
        let response = ureq::http::Response::builder()
            .status(429)
            .header("retry-after", "7")
            .body(ureq::Body::builder().data("slow down"))
            .expect("static response");
        let err = read_error_body(429, response);
        assert!(
            matches!(
                &err,
                ProviderError::RateLimited {
                    retry_after: Some(d)
                } if *d == Duration::from_secs(7)
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn read_error_body_keeps_non_429_as_http() {
        let response = ureq::http::Response::builder()
            .status(503)
            .body(ureq::Body::builder().data("unavailable"))
            .expect("static response");
        let err = read_error_body(503, response);
        assert!(
            matches!(
                &err,
                ProviderError::Http { status: 503, body } if body == "unavailable"
            ),
            "got {err:?}"
        );
    }
}
