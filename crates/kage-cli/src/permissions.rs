//! Tool permission gate (`[permissions]`) for the TUI and print runs.
//!
//! [`PermissionGate`] implements [`kage_loop::Hooks::before_tool_call`]
//! and is composed into the hook stack as the innermost layer, where
//! the ACP client gate sits in `kage rpc`. With no `[permissions]`
//! config every call returns `None` (allow), so unconfigured behavior
//! is unchanged. A configured tool resolves through
//! [`PermissionsConfig::check`]: deny synthesizes an error output,
//! allow passes through, and ask blocks the worker until the host
//! answers over the ask channel (TUI overlay) or, without one
//! (print mode), denies with a message pointing at the config.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kage_core::config::Config;
use kage_core::permissions::{PermissionAction, PermissionsConfig};
use kage_core::sync::lock;
use kage_core::{CancelFlag, ToolOutput};
use kage_loop::Hooks;
use kage_tui::{PermissionAsk, PermissionDecision};

/// How long to wait between cancel-flag checks while parked on an
/// ask. Same cadence the loop's cancelable sleeps use.
const ASK_POLL: Duration = Duration::from_millis(100);

/// Allow / ask / deny gate over tool calls, shared across the hook
/// instances of one session.
///
/// The rules live behind an `Arc<Mutex>` so an "always allow" answer
/// mutates every clone at once, and so the mutation can be persisted
/// to the user config before the parked call resumes. Clone it per
/// hook composition site; the ask channel and cancel flag are shared.
#[derive(Clone, Debug)]
pub(crate) struct PermissionGate {
    rules: Arc<Mutex<PermissionsConfig>>,
    ask: Option<Sender<PermissionAsk>>,
    cancel: CancelFlag,
    /// Session-scoped mode override. `Some(action)` short-circuits
    /// the per-tool rules for every call; `None` (the initial state)
    /// evaluates the configured rules, which default to allow. Never
    /// persisted: it lives and dies with this session.
    mode: Arc<Mutex<Option<PermissionAction>>>,
    /// Where "always allow" decisions are written. `None` (every
    /// production construction) resolves [`Config::default_path`] at
    /// write time; tests point it at a tempdir.
    config_path: Option<PathBuf>,
}

impl PermissionGate {
    /// Build a gate from the loaded `[permissions]` rules. No ask
    /// channel (print mode): an `ask` verdict denies with a
    /// non-interactive explanation. Attach the host's cancel flag
    /// with [`Self::with_cancel`] and the TUI channel with
    /// [`Self::with_ask`].
    pub(crate) fn new(rules: PermissionsConfig) -> Self {
        Self {
            rules: Arc::new(Mutex::new(rules)),
            ask: None,
            cancel: CancelFlag::new(),
            mode: Arc::new(Mutex::new(None)),
            config_path: None,
        }
    }

    /// Set the session mode override. `None` clears it, restoring
    /// the configured per-tool rules. Shared across every clone.
    pub(crate) fn set_mode(&self, mode: Option<PermissionAction>) {
        *lock(&self.mode) = mode;
    }

    /// Current session mode override, `None` when the configured
    /// rules decide.
    #[must_use]
    pub(crate) fn mode(&self) -> Option<PermissionAction> {
        *lock(&self.mode)
    }

    /// Attach the channel a TUI host listens on. While set, an `ask`
    /// verdict parks the worker on the host's decision instead of
    /// denying.
    #[must_use]
    pub(crate) fn with_ask(mut self, ask: Sender<PermissionAsk>) -> Self {
        self.ask = Some(ask);
        self
    }

    /// Point the gate at the run's real cancel flag so a Ctrl+C while
    /// parked on an ask unblocks the worker as a cancellation.
    #[must_use]
    pub(crate) fn with_cancel(mut self, cancel: CancelFlag) -> Self {
        self.cancel = cancel;
        self
    }

    /// Redirect "always allow" persistence to `path`. Tests use this
    /// so they never touch the real user config.
    #[cfg(test)]
    fn with_persist_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }

    /// Resolve an `ask` verdict: forward to the host and park until
    /// it answers, the channel dies, or the run is cancelled. Returns
    /// `None` to run the tool or `Some(output)` to short-circuit.
    fn ask_user(&self, tool: &str, subject: String) -> Option<ToolOutput> {
        let Some(ask) = self.ask.as_ref() else {
            return Some(non_interactive_output(tool));
        };
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if ask
            .send(PermissionAsk {
                tool: tool.to_owned(),
                subject,
                reply: reply_tx,
            })
            .is_err()
        {
            return Some(error_output(
                tool,
                "permission prompt unavailable (host went away); denied",
            ));
        }
        loop {
            match reply_rx.recv_timeout(ASK_POLL) {
                Ok(PermissionDecision::AllowOnce) => return None,
                Ok(PermissionDecision::AllowAlways) => {
                    self.persist_allow_always(tool);
                    return None;
                }
                Ok(PermissionDecision::Deny) => {
                    return Some(error_output(tool, "denied by user"));
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.cancel.is_cancelled() {
                        return Some(error_output(tool, "permission prompt cancelled"));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Some(error_output(
                        tool,
                        "permission prompt dropped (host went away); denied",
                    ));
                }
            }
        }
    }

    /// Record an "always allow" for `tool`: flip the shared rules so
    /// every clone stops asking, then persist the decision into the
    /// user config file. The user file (not the layered config) is
    /// the destination so project and env layers are not baked in.
    /// IO failures only eprintln: the in-memory flip already holds
    /// for this session.
    fn persist_allow_always(&self, tool: &str) {
        lock(&self.rules)
            .tools
            .entry(tool.to_owned())
            .or_default()
            .default = PermissionAction::Allow;
        let path = match &self.config_path {
            Some(p) => Some(p.clone()),
            None => Config::default_path(),
        };
        let Some(path) = path else {
            eprintln!("kage: persist permission: no home directory; not persisted");
            return;
        };
        if let Err(e) = save_allow_always(&path, tool) {
            eprintln!("kage: persist permission: {e}");
        }
    }
}

/// Load the user config at `path`, set `[permissions.tools.<tool>]
/// default = "allow"`, and save it back comment-preserving. Missing
/// files are created from defaults by [`Config::save`].
///
/// # Errors
///
/// A string describing the load or save failure, for the caller's
/// eprintln.
fn save_allow_always(path: &Path, tool: &str) -> Result<(), String> {
    let mut cfg = Config::load(path).map_err(|e| format!("config load: {e}"))?;
    cfg.permissions
        .tools
        .entry(tool.to_owned())
        .or_default()
        .default = PermissionAction::Allow;
    cfg.save(path).map_err(|e| format!("save: {e}"))
}

impl Hooks for PermissionGate {
    fn before_tool_call(&mut self, name: &str, input: &serde_json::Value) -> Option<ToolOutput> {
        let subject = PermissionsConfig::subject_for(input);
        if let Some(mode) = self.mode() {
            return match mode {
                PermissionAction::Allow => None,
                PermissionAction::Deny => Some(error_output(
                    name,
                    "permission mode is deny this session (`:permission default` restores rules)",
                )),
                PermissionAction::Ask => self.ask_user(name, subject),
            };
        }
        let action = lock(&self.rules).check(name, &subject);
        match action {
            PermissionAction::Allow => None,
            PermissionAction::Deny => Some(error_output(
                name,
                &format!("permission denied by [permissions.tools.{name}]"),
            )),
            PermissionAction::Ask => self.ask_user(name, subject),
        }
    }
}

/// Synthesized `is_error` output for a refused call. The text is what
/// the model sees, so it names the refusing rule.
fn error_output(tool: &str, reason: &str) -> ToolOutput {
    ToolOutput {
        is_error: true,
        text: format!("`{tool}`: {reason}"),
        structured: None,
        terminate: false,
    }
}

/// The print-mode `ask` outcome: there is no one to ask, so the call
/// is refused with the remedy in the message.
fn non_interactive_output(tool: &str) -> ToolOutput {
    error_output(
        tool,
        &format!(
            "permission is `ask` ([permissions.tools.{tool}]) and this mode is non-interactive; \
             add an allow rule or run the interactive TUI"
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use kage_core::permissions::{PermissionAction, ToolPermissionRules};
    use kage_loop::NoopHooks;

    use super::*;

    fn rules_for(default: PermissionAction) -> PermissionsConfig {
        PermissionsConfig {
            confine_paths: false,
            tools: [(
                "bash".to_owned(),
                ToolPermissionRules {
                    default,
                    allow: Vec::new(),
                    deny: Vec::new(),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    fn bash_input() -> serde_json::Value {
        serde_json::json!({"command": "ls"})
    }

    #[test]
    fn no_config_allows_every_call() {
        let mut gate = PermissionGate::new(PermissionsConfig::default());
        assert!(gate.before_tool_call("bash", &bash_input()).is_none());
        assert!(
            gate.before_tool_call("write", &serde_json::json!({}))
                .is_none()
        );
    }

    #[test]
    fn mode_deny_overrides_allow_rules() {
        let mut gate = PermissionGate::new(rules_for(PermissionAction::Allow));
        gate.set_mode(Some(PermissionAction::Deny));
        let out = gate.before_tool_call("bash", &bash_input()).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("permission mode is deny"), "{}", out.text);
        gate.set_mode(None);
        assert!(gate.before_tool_call("bash", &bash_input()).is_none());
    }

    #[test]
    fn mode_ask_overrides_allow_rules_and_uses_channel() {
        let (ask_tx, ask_rx) = mpsc::channel::<PermissionAsk>();
        let gate = PermissionGate::new(rules_for(PermissionAction::Allow)).with_ask(ask_tx);
        gate.set_mode(Some(PermissionAction::Ask));
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let out = gate.before_tool_call("bash", &bash_input());
            let _ = done_tx.send(out.is_none());
        });
        let ask = ask_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        ask.reply.send(PermissionDecision::Deny).unwrap();
        handle.join().unwrap();
        assert!(!done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn mode_allow_overrides_deny_rules_and_shares_across_clones() {
        let gate = PermissionGate::new(rules_for(PermissionAction::Deny));
        let mut clone = gate.clone();
        gate.set_mode(Some(PermissionAction::Allow));
        assert!(clone.before_tool_call("bash", &bash_input()).is_none());
        assert_eq!(clone.mode(), Some(PermissionAction::Allow));
    }

    #[test]
    fn deny_by_rule_synthesizes_error_output() {
        let mut gate = PermissionGate::new(rules_for(PermissionAction::Deny));
        let out = gate.before_tool_call("bash", &bash_input()).unwrap();
        assert!(out.is_error);
        assert_eq!(
            out.text,
            "`bash`: permission denied by [permissions.tools.bash]"
        );
    }

    #[test]
    fn ask_without_channel_denies_non_interactively() {
        let mut gate = PermissionGate::new(rules_for(PermissionAction::Ask));
        let out = gate.before_tool_call("bash", &bash_input()).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("non-interactive"), "{}", out.text);
        assert!(
            out.text.contains("[permissions.tools.bash]"),
            "{}",
            out.text
        );
    }

    #[test]
    fn ask_with_channel_returns_allow_once() {
        let (ask_tx, ask_rx) = mpsc::channel::<PermissionAsk>();
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask)).with_ask(ask_tx);
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let out = gate.before_tool_call("bash", &bash_input());
            let _ = done_tx.send(out.is_none());
        });
        let ask = ask_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(ask.tool, "bash");
        assert_eq!(ask.subject, "ls");
        ask.reply.send(PermissionDecision::AllowOnce).unwrap();
        handle.join().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn ask_with_channel_returns_user_deny() {
        let (ask_tx, ask_rx) = mpsc::channel::<PermissionAsk>();
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask)).with_ask(ask_tx);
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let out = gate.before_tool_call("bash", &bash_input());
            let _ = done_tx.send(out.map(|o| o.text));
        });
        let ask = ask_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        ask.reply.send(PermissionDecision::Deny).unwrap();
        handle.join().unwrap();
        let text = done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(text, "`bash`: denied by user");
    }

    #[test]
    fn ask_with_channel_allow_always_flips_shared_rules_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (ask_tx, ask_rx) = mpsc::channel::<PermissionAsk>();
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask))
            .with_ask(ask_tx)
            .with_persist_path(path.clone());
        let gate2 = gate.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let allowed = gate.before_tool_call("bash", &bash_input()).is_none();
            let _ = done_tx.send(allowed);
        });
        let ask = ask_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        ask.reply.send(PermissionDecision::AllowAlways).unwrap();
        handle.join().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        // The in-memory rules flipped for every clone; no further ask.
        let mut gate2 = gate2;
        assert!(gate2.before_tool_call("bash", &bash_input()).is_none());
        assert_eq!(
            lock(&gate2.rules).check("bash", "anything"),
            PermissionAction::Allow
        );
        // The decision landed in the user config file.
        let saved = Config::load(&path).unwrap();
        assert_eq!(
            saved.permissions.check("bash", "anything"),
            PermissionAction::Allow
        );
    }

    #[test]
    fn cancel_during_ask_denies() {
        let (ask_tx, ask_rx) = mpsc::channel::<PermissionAsk>();
        let cancel = CancelFlag::new();
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask))
            .with_ask(ask_tx)
            .with_cancel(cancel.clone());
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let out = gate.before_tool_call("bash", &bash_input());
            let _ = done_tx.send(out.map(|o| o.text));
        });
        let ask = ask_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Drop our sender handle so only the parked worker holds the
        // receiver; then trip the cancel flag the gate watches.
        cancel.cancel();
        handle.join().unwrap();
        let text = done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(text, "`bash`: permission prompt cancelled");
        drop(ask);
    }

    #[test]
    fn dropped_host_denies() {
        let (ask_tx, ask_rx) = mpsc::channel::<PermissionAsk>();
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask)).with_ask(ask_tx);
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let out = gate.before_tool_call("bash", &bash_input());
            let _ = done_tx.send(out.map(|o| o.text));
        });
        // Receive the ask and drop its reply sender entirely.
        let ask = ask_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(ask);
        drop(ask_rx);
        handle.join().unwrap();
        let text = done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(text.contains("denied"), "{text}");
    }

    #[test]
    fn gate_composes_as_plain_hooks() {
        // The gate must work wherever a `dyn Hooks` is placed, like
        // the innermost layer under TuiHooks.
        let mut gate = PermissionGate::new(PermissionsConfig::default());
        let hooks: &mut dyn Hooks = &mut gate;
        assert!(hooks.before_tool_call("bash", &bash_input()).is_none());
        let noop: &mut dyn Hooks = &mut NoopHooks;
        assert!(noop.before_tool_call("bash", &bash_input()).is_none());
    }
}
