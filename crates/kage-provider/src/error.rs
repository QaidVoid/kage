//! Error type returned by [`Provider`](crate::Provider) implementations.

use std::time::Duration;

/// Failure modes shared by all providers.
#[derive(Debug, thiserror::Error)]
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
}
