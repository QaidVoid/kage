//! OAuth flow dispatcher and per-provider implementations.
//!
//! Today this module exposes a single public surface, [`refresh`],
//! which the registry builder calls before handing a credential to a
//! [`kage_provider::Provider`] impl. The body is a `match` over
//! provider id; each arm wires the per-provider refresh endpoint.
//!
//! PP.B.1 only ships the dispatcher and the placeholder arms so the
//! v2 credential shape can land in isolation. The Anthropic, `OpenAI`
//! Codex, and `GitHub` Copilot flows arrive in later sub-tasks.

use crate::auth::OAuthCredential;

/// Slack window used when deciding whether to refresh an OAuth
/// credential proactively. A token whose deadline falls inside this
/// window from "now" is treated as expired so a request that is about
/// to fire doesn't race the expiry.
pub const REFRESH_SLACK: chrono::Duration = chrono::Duration::seconds(60);

/// Dispatch an OAuth refresh for `provider`. Returns the refreshed
/// credential on success.
///
/// The current implementation is a placeholder for PP.B.1: every
/// provider id returns an error indicating no refresh handler is
/// wired yet. PP.B.2 / PP.B.3 plug in the real flows and the public
/// signature stays stable.
pub fn refresh(provider: &str, _creds: &OAuthCredential) -> Result<OAuthCredential, String> {
    match provider {
        "anthropic" | "openai-codex" | "copilot" => Err(format!(
            "auth: oauth refresh for '{provider}' is not yet implemented; \
             rerun `kage auth login {provider}` to mint a fresh token"
        )),
        _ => Err(format!(
            "auth: provider '{provider}' has no oauth refresh handler"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_returns_a_helpful_error_for_known_oauth_providers() {
        let creds = OAuthCredential::default();
        let err = refresh("anthropic", &creds).unwrap_err();
        assert!(err.contains("anthropic"));
        assert!(err.contains("not yet implemented"));
    }

    #[test]
    fn refresh_returns_a_distinct_error_for_unknown_provider() {
        let creds = OAuthCredential::default();
        let err = refresh("acme", &creds).unwrap_err();
        assert!(err.contains("acme"));
        assert!(err.contains("no oauth refresh handler"));
    }
}
