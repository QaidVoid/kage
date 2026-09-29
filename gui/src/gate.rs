//! The initialize-time gate: is the agent new enough and does it
//! carry the capabilities the shell gates on?
//!
//! [`check`] reads a [`kage_client::State`] after the `initialize`
//! answer landed and reports what falls short. The shell renders the
//! report as a dismissible banner; nothing here blocks the session.

use kage_client::State;

/// The oldest engine the shell accepts, the current engine version.
pub const MINIMUM_KAGE_VERSION: &str = "0.1.0";

/// A capability the shell gates a feature on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Steering a run in flight at its turn boundary.
    Steer,
    /// Releasing a session with `session/close`.
    CloseSession,
}

impl Capability {
    /// The name the banner shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Steer => "steering",
            Self::CloseSession => "session close",
        }
    }
}

/// What the initialize answer fell short of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The version the agent reported when it is below
    /// [`MINIMUM_KAGE_VERSION`], or when it reported none or an
    /// unreadable one.
    pub too_old: Option<String>,
    /// The advertised capabilities the shell misses.
    pub missing: Vec<Capability>,
}

impl Report {
    /// Whether the agent passes every gate.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.too_old.is_none() && self.missing.is_empty()
    }

    /// One human line per problem, in banner order.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(found) = &self.too_old {
            lines.push(format!(
                "agent version {found} is below the required {MINIMUM_KAGE_VERSION}"
            ));
        }
        for capability in &self.missing {
            lines.push(format!(
                "agent does not advertise {}; the related controls stay dark",
                capability.label()
            ));
        }
        lines
    }
}

/// Checks the agent identity and capabilities an initialize answer
/// delivered. An agent that reported nothing yet passes: there is
/// nothing to judge before the handshake.
#[must_use]
pub fn check(state: &State) -> Report {
    let mut report = Report::default();
    let Some(capabilities) = &state.capabilities else {
        return report;
    };
    let reported = state
        .agent
        .as_ref()
        .and_then(|agent| agent.version.as_deref());
    match reported {
        Some(version) if is_below(version, MINIMUM_KAGE_VERSION) => {
            report.too_old = Some(version.to_owned());
        }
        None => report.too_old = Some("none".to_owned()),
        _ => {}
    }
    if !capabilities.steer {
        report.missing.push(Capability::Steer);
    }
    if capabilities.session_capabilities.close.is_none() {
        report.missing.push(Capability::CloseSession);
    }
    report
}

/// Whether `version` sorts before `minimum`. A version that does not
/// parse as dotted numbers counts as below.
fn is_below(version: &str, minimum: &str) -> bool {
    match (parse(version), parse(minimum)) {
        (Some(found), Some(floor)) => found < floor,
        _ => true,
    }
}

/// Reads up to three dotted numbers, filling absent parts with zero.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = [0u64; 3];
    for (ix, part) in version.split('.').enumerate() {
        if ix >= 3 {
            return None;
        }
        parts[ix] = part.parse().ok()?;
    }
    Some((parts[0], parts[1], parts[2]))
}

#[cfg(test)]
mod tests {
    use super::{Capability, Report, check, is_below, parse};
    use kage_client::wire::{AgentCapabilities, Implementation};
    use kage_client::{Client, Frame};

    fn state_with(version: Option<&str>, steer: bool, close: bool) -> kage_client::State {
        let mut client = Client::new();
        client.initialize(Default::default(), None);
        let _ = client.take_outgoing();
        let mut capabilities = AgentCapabilities {
            steer,
            ..AgentCapabilities::default()
        };
        capabilities.session_capabilities.close = close.then_some(kage_client::wire::Supported {});
        let agent = version.map(|version| Implementation {
            name: "kage".into(),
            title: None,
            version: Some(version.into()),
        });
        client
            .handle(Frame::Success {
                id: 1,
                result: serde_json::json!({
                    "protocolVersion": 1,
                    "agentCapabilities": capabilities,
                    "agentInfo": agent,
                }),
            })
            .is_empty();
        client.state().clone()
    }

    #[test]
    fn dotted_versions_compare_part_wise() {
        assert_eq!(parse("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse("1.2"), Some((1, 2, 0)));
        assert_eq!(parse("x"), None);
        assert!(is_below("0.0.9", "0.1.0"));
        assert!(is_below("0.0.99", "0.1.0"));
        assert!(!is_below("0.1.0", "0.1.0"));
        assert!(!is_below("0.2.0", "0.1.0"));
        assert!(!is_below("1.0.0", "0.1.0"));
        assert!(is_below("nonsense", "0.1.0"));
    }

    #[test]
    fn a_current_engine_passes_every_gate() {
        let report = check(&state_with(Some("0.1.0"), true, true));
        assert_eq!(report, Report::default());
        assert!(report.is_clean());
        assert!(report.lines().is_empty());
    }

    #[test]
    fn a_newer_engine_passes_too() {
        assert!(check(&state_with(Some("0.9.2"), true, true)).is_clean());
        assert!(check(&state_with(Some("1.0.0"), true, true)).is_clean());
    }

    #[test]
    fn a_below_minimum_version_is_named() {
        let report = check(&state_with(Some("0.0.9"), true, true));
        assert_eq!(report.too_old.as_deref(), Some("0.0.9"));
        assert!(report.missing.is_empty());
        assert_eq!(
            report.lines(),
            vec!["agent version 0.0.9 is below the required 0.1.0"]
        );
    }

    #[test]
    fn a_missing_or_unreadable_version_is_flagged() {
        let report = check(&state_with(None, true, true));
        assert_eq!(report.too_old.as_deref(), Some("none"));
        assert!(
            check(&state_with(Some("dev"), true, true))
                .too_old
                .is_some()
        );
    }

    #[test]
    fn missing_capabilities_are_flagged_by_label() {
        let report = check(&state_with(Some("0.1.0"), false, false));
        assert_eq!(
            report.missing,
            vec![Capability::Steer, Capability::CloseSession]
        );
        assert_eq!(
            report.lines(),
            vec![
                "agent does not advertise steering; the related controls stay dark",
                "agent does not advertise session close; the related controls stay dark",
            ]
        );
    }

    #[test]
    fn an_unanswered_handshake_checks_clean() {
        assert!(check(&kage_client::State::default()).is_clean());
    }
}
