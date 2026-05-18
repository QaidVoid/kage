//! Shared `ureq` plumbing used by every HTTP provider.
//!
//! `build_agent`, `read_error_body`, and `map_ureq_error` are
//! provider-agnostic: they were living in `openai` and reached into
//! sideways by the Anthropic / Gemini / Responses providers, which
//! muddied the module layering. They belong here so each provider is
//! a peer that depends on a shared util, not on a sibling.

use crate::ProviderError;

/// Construct a [`ureq::Agent`] with status-code-as-error disabled so we
/// can surface the upstream response body in [`ProviderError::Http`]
/// instead of throwing it away.
///
/// The timeouts here are a backstop for the "provider never even starts
/// answering" hangs: DNS, connect/TLS, sending the request, and waiting
/// for the response *headers*. They are deliberately generous so a
/// slow-but-alive provider is never killed.
///
/// `recv_body` and `global` are intentionally left unset: for a
/// streaming completion the body *is* the whole (possibly multi-minute)
/// generation, so a total-body cap would abort legitimate long answers.
/// The case where a provider sends headers, streams part of an answer,
/// then goes silent forever is a mid-stream *idle* stall - bounding
/// that without false-positives needs an inactivity watchdog on the
/// event stream, not a blunt total-time cap here.
pub(crate) fn build_agent() -> ureq::Agent {
    use std::time::Duration;
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_resolve(Some(Duration::from_secs(15)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_send_request(Some(Duration::from_secs(30)))
        .timeout_send_body(Some(Duration::from_secs(60)))
        .timeout_recv_response(Some(Duration::from_secs(120)))
        .build()
        .new_agent()
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
pub(crate) fn map_ureq_error(err: ureq::Error) -> ProviderError {
    match err {
        ureq::Error::StatusCode(code) => ProviderError::Http {
            status: code,
            body: String::new(),
        },
        ureq::Error::Io(e) => ProviderError::Transport(e.to_string()),
        other => ProviderError::Transport(other.to_string()),
    }
}
