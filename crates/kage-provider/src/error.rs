//! Error type returned by [`Provider`](crate::Provider) implementations.

use std::time::Duration;

/// Failure modes shared by all providers.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ProviderError {
    /// Authentication failed (bad or missing API key).
    #[error("authentication failed: {0}")]
    Auth(String),

    /// Provider rate limited the request.
    #[error("rate limited")]
    RateLimited {
        /// Hint from the provider about when to retry.
        retry_after: Option<Duration>,
    },

    /// HTTP error returned by the provider.
    #[error("provider returned status {status}: {body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Response body (truncated).
        body: String,
    },

    /// Network or transport-level error.
    #[error("transport: {0}")]
    Transport(String),

    /// Could not decode the provider's response.
    #[error("malformed response: {0}")]
    Decode(String),

    /// Request was cancelled by the caller before completion.
    #[error("cancelled")]
    Cancelled,

    /// The requested model id is not supported by this provider.
    #[error("model not supported: {0}")]
    UnknownModel(String),
}

impl ProviderError {
    /// Whether re-issuing the identical request might succeed.
    ///
    /// True for failures that are about the pipe, not the request:
    /// any transport error (timeouts, resets, a stream that dropped
    /// mid-body), rate limiting, and server-side 5xx / 408 / 429. A
    /// stalled stream surfaces here as [`Self::Transport`], so this is
    /// the gate the loop's auto-retry consults. Auth, decode,
    /// unknown-model, an explicit cancel, and other 4xx are the
    /// request's fault and never retried - resending only repeats the
    /// failure.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Transport(_) | Self::RateLimited { .. } => true,
            Self::Http { status, .. } => {
                *status == 408 || *status == 429 || (500..=599).contains(status)
            }
            Self::Auth(_) | Self::Decode(_) | Self::Cancelled | Self::UnknownModel(_) => false,
        }
    }

    /// A provider-supplied backoff hint, when one exists. Only
    /// [`Self::RateLimited`] carries one; everything else lets the
    /// caller pick its own backoff.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }

    /// Classify an in-band provider error event (kind + message) so a
    /// mid-stream failure gets the same retryability treatment as the
    /// equivalent HTTP status: rate limits and 5xx-class kinds are
    /// transient, everything else stays a permanent decode failure.
    pub(crate) fn from_stream_error(kind: &str, message: &str) -> Self {
        let kind = kind.trim().to_ascii_lowercase();
        if kind.contains("rate_limit") || kind.contains("resource_exhausted") || kind == "429" {
            return Self::RateLimited { retry_after: None };
        }
        if kind.contains("overloaded")
            || kind.contains("server_error")
            || kind.contains("unavailable")
            || kind == "api_error"
            || kind
                .parse::<u16>()
                .is_ok_and(|status| (500..=599).contains(&status))
        {
            return Self::Transport(format!("provider stream error ({kind}): {message}"));
        }
        Self::Decode(format!("provider stream error ({kind}): {message}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_message_includes_detail() {
        let err = ProviderError::Auth("missing api key".into());
        assert!(err.to_string().contains("missing api key"));
    }

    #[test]
    fn http_message_includes_status_and_body() {
        let err = ProviderError::Http {
            status: 503,
            body: "service unavailable".into(),
        };
        let s = err.to_string();
        assert!(s.contains("503"));
        assert!(s.contains("service unavailable"));
    }

    #[test]
    fn cancelled_displays_cleanly() {
        assert_eq!(ProviderError::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn transient_classification_targets_the_pipe_not_the_request() {
        assert!(ProviderError::Transport("timeout: receive response".into()).is_transient());
        assert!(ProviderError::RateLimited { retry_after: None }.is_transient());
        for status in [408, 429, 500, 502, 503, 504] {
            assert!(
                ProviderError::Http {
                    status,
                    body: String::new()
                }
                .is_transient(),
                "{status} should be transient"
            );
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(
                !ProviderError::Http {
                    status,
                    body: String::new()
                }
                .is_transient(),
                "{status} must not be transient"
            );
        }
        assert!(!ProviderError::Auth("no key".into()).is_transient());
        assert!(!ProviderError::Decode("bad json".into()).is_transient());
        assert!(!ProviderError::Cancelled.is_transient());
        assert!(!ProviderError::UnknownModel("m".into()).is_transient());
    }

    #[test]
    fn retry_after_only_from_rate_limit() {
        assert_eq!(
            ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
            .retry_after(),
            Some(Duration::from_secs(7))
        );
        assert_eq!(ProviderError::Transport("x".into()).retry_after(), None);
    }

    #[test]
    fn stream_error_classifier_maps_rate_limit_kinds() {
        for kind in [
            "rate_limit_error",
            "rate_limit_exceeded",
            "RESOURCE_EXHAUSTED",
            "429",
        ] {
            assert!(
                matches!(
                    ProviderError::from_stream_error(kind, "slow down"),
                    ProviderError::RateLimited { retry_after: None }
                ),
                "{kind} should classify as RateLimited"
            );
        }
    }

    #[test]
    fn stream_error_classifier_maps_transient_kinds_to_transport() {
        for kind in [
            "overloaded_error",
            "server_error",
            "api_error",
            "UNAVAILABLE",
            "500",
            "503",
        ] {
            let err = ProviderError::from_stream_error(kind, "upstream trouble");
            assert!(err.is_transient(), "{kind} should be transient");
            assert!(
                err.to_string().contains("upstream trouble"),
                "{kind} should keep the provider message"
            );
        }
    }

    #[test]
    fn stream_error_classifier_keeps_other_kinds_permanent() {
        for kind in [
            "invalid_request_error",
            "authentication_error",
            "permission_error",
            "not_found",
        ] {
            let err = ProviderError::from_stream_error(kind, "the request is bad");
            assert!(!err.is_transient(), "{kind} must not be transient");
            assert!(
                matches!(err, ProviderError::Decode(_)),
                "{kind} stays Decode"
            );
        }
    }
}
