//! Tool permission gate (`[permissions]`).
//!
//! [`PermissionGate`] implements [`kage_loop::Hooks::before_tool_call`].
//! A configured tool resolves through [`PermissionsConfig::check`]; an MCP
//! tool (`<server>__<tool>`) of a known server without an entry resolves
//! through [`PermissionsConfig::mcp_action`], which asks for unlisted
//! servers; any other tool gets the gate's fallback action (allow by
//! default, ask for editor sessions). Deny synthesizes an error output, allow
//! passes through, and ask blocks the run until an [`Asker`] delivers the
//! answer, or, when there is none (print mode), denies with a message
//! pointing at the config. A session mode short-circuits the rules, but
//! a configured deny still denies. Tools approved for the session skip the
//! ask, but never a deny mode or a configured deny.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, select_biased};
use kage_core::config::Config;
use kage_core::options::OptionValue;
use kage_core::permissions::{PermissionAction, PermissionsConfig};
use kage_core::protocol::PermissionDecision;
use kage_core::sync::lock;
use kage_core::{CancelFlag, ToolCallId, ToolOutput};
use kage_loop::Hooks;

/// A tool call waiting for an interactive decision.
pub(crate) struct PermissionPrompt {
    pub call_id: ToolCallId,
    pub tool: String,
    pub subject: String,
    pub input: serde_json::Value,
}

/// Forwards a prompt to whoever can answer it and returns the channel the
/// answer arrives on, or `None` when nobody is listening anymore.
pub(crate) type Asker =
    Arc<dyn Fn(PermissionPrompt) -> Option<Receiver<PermissionDecision>> + Send + Sync>;

/// Allow / ask / deny gate over tool calls, shared across the hook
/// instances of one session.
///
/// The rules live behind an `Arc<Mutex>` so an "always allow" answer
/// mutates every clone at once, and so the mutation can be persisted
/// to the user config before the parked call resumes. Clone it per
/// hook composition site; the ask channel and cancel flag are shared.
#[derive(Clone)]
pub(crate) struct PermissionGate {
    rules: Arc<Mutex<PermissionsConfig>>,
    ask: Option<Asker>,
    /// Action for tools with no `[permissions.tools.<name>]` entry
    /// that do not belong to a known MCP server.
    fallback: PermissionAction,
    /// Names of the MCP servers whose tools are registered, used to
    /// recognize `<server>__<tool>` names.
    mcp_servers: Arc<[String]>,
    cancel: CancelFlag,
    /// Session-scoped mode override. `Some(action)` short-circuits
    /// the per-tool rules for every call except a configured deny;
    /// `None` (the initial state)
    /// evaluates the configured rules, which default to allow. Never
    /// persisted: it lives and dies with this session.
    mode: Arc<Mutex<Option<PermissionAction>>>,
    /// Tools the user allowed for this session. They skip an ask, but
    /// not a deny mode or a configured deny. Never persisted.
    session_allowed: Arc<Mutex<BTreeSet<String>>>,
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
    /// [`Self::with_asker`].
    pub(crate) fn new(rules: PermissionsConfig) -> Self {
        Self {
            rules: Arc::new(Mutex::new(rules)),
            ask: None,
            fallback: PermissionAction::Allow,
            mcp_servers: Arc::from([]),
            cancel: CancelFlag::new(),
            mode: Arc::new(Mutex::new(None)),
            session_allowed: Arc::new(Mutex::new(BTreeSet::new())),
            config_path: None,
        }
    }

    /// Set the session mode override. `None` clears it, restoring
    /// the configured per-tool rules. Shared across every clone.
    pub(crate) fn set_mode(&self, mode: Option<PermissionAction>) {
        *lock(&self.mode) = mode;
    }

    /// Drop the session mode override and the session approvals, for a
    /// new or switched session. The configured rules stay, including
    /// the in-memory flips of "always allow".
    pub(crate) fn reset_session(&self) {
        *lock(&self.mode) = None;
        lock(&self.session_allowed).clear();
    }

    /// Current session mode override, `None` when the configured
    /// rules decide.
    #[must_use]
    pub(crate) fn mode(&self) -> Option<PermissionAction> {
        *lock(&self.mode)
    }

    /// Route `ask` verdicts to `asker` instead of denying them.
    #[must_use]
    pub(crate) fn with_asker(mut self, asker: Asker) -> Self {
        self.ask = Some(asker);
        self
    }

    /// Use `action` for tools the configured rules do not mention.
    #[must_use]
    pub(crate) fn with_fallback(mut self, action: PermissionAction) -> Self {
        self.fallback = action;
        self
    }

    /// Treat tools named `<server>__<tool>` for any of `servers` as MCP
    /// tools, resolved through `[permissions.mcp]`.
    #[must_use]
    pub(crate) fn with_mcp_servers(mut self, servers: Vec<String>) -> Self {
        self.mcp_servers = servers.into();
        self
    }

    /// The known MCP server `tool` belongs to. Matching by prefix
    /// against the known names keeps servers whose names contain `__`
    /// correct; the longest match wins.
    fn mcp_server_of(&self, tool: &str) -> Option<&str> {
        self.mcp_servers
            .iter()
            .filter(|server| {
                tool.strip_prefix(server.as_str())
                    .is_some_and(|rest| rest.len() > 2 && rest.starts_with("__"))
            })
            .max_by_key(|server| server.len())
            .map(String::as_str)
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
    fn ask_user(&self, prompt: PermissionPrompt, rule: &Rule) -> Option<ToolOutput> {
        let Some(ask) = self.ask.as_ref() else {
            return Some(non_interactive_output(&prompt.tool, rule));
        };
        let tool = prompt.tool.clone();
        let tool = tool.as_str();
        let Some(reply_rx) = ask(prompt) else {
            return Some(error_output(
                tool,
                "permission prompt unavailable (host went away); denied",
            ));
        };
        let watch = self.cancel.watch();
        select_biased! {
            recv(reply_rx) -> decision => match decision {
                Ok(PermissionDecision::AllowOnce) => None,
                Ok(PermissionDecision::AllowSession) => {
                    self.allow_for_session(tool);
                    None
                }
                Ok(PermissionDecision::AllowAlways) => {
                    self.allow_for_session(tool);
                    self.persist_allow_always(tool);
                    None
                }
                Ok(PermissionDecision::Deny) => Some(error_output(tool, "denied by user")),
                Err(_) => Some(error_output(
                    tool,
                    "permission prompt dropped (host went away); denied",
                )),
            },
            recv(watch.receiver()) -> _ => {
                Some(error_output(tool, "permission prompt cancelled"))
            }
        }
    }

    /// What the configured rules say about `name` on `subject`.
    fn configured_action(&self, name: &str, subject: &str) -> (PermissionAction, Rule) {
        let rules = lock(&self.rules);
        if rules.tools.contains_key(name) {
            (rules.check(name, subject), Rule::Tool)
        } else if let Some(server) = self.mcp_server_of(name) {
            (rules.mcp_action(server), Rule::Mcp(server.to_owned()))
        } else {
            (self.fallback, Rule::Tool)
        }
    }

    fn allow_for_session(&self, tool: &str) {
        lock(&self.session_allowed).insert(tool.to_owned());
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

/// Set `[permissions.tools.<tool>] default = "allow"` in the user
/// config at `path`, leaving every other line of the file as written.
/// A missing file is created with just that key.
///
/// # Errors
///
/// A string describing the save failure, for the caller's eprintln.
fn save_allow_always(path: &Path, tool: &str) -> Result<(), String> {
    Config::save_keys(
        path,
        &[(
            vec!["permissions", "tools", tool, "default"],
            OptionValue::Str("allow".to_owned()),
        )],
    )
    .map_err(|e| format!("save: {e}"))
}

impl Hooks for PermissionGate {
    fn before_tool_call(
        &mut self,
        id: &ToolCallId,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<ToolOutput> {
        let mode = self.mode();
        let subject = PermissionsConfig::subject_for(input);
        let configured = self.configured_action(name, &subject);
        if mode != Some(PermissionAction::Deny)
            && configured.0 != PermissionAction::Deny
            && lock(&self.session_allowed).contains(name)
        {
            return None;
        }
        // Deny wins both ways: a deny mode denies everything, and a
        // configured deny survives an allow or ask session mode.
        let (action, rule) = match mode {
            None => configured,
            Some(PermissionAction::Deny) => (PermissionAction::Deny, Rule::Mode),
            Some(_) if configured.0 == PermissionAction::Deny => configured,
            Some(action) => (action, Rule::Mode),
        };
        match (action, &rule) {
            (PermissionAction::Allow, _) => None,
            (PermissionAction::Deny, Rule::Mode) => Some(error_output(
                name,
                "permission mode is deny this session (`/permission default` restores rules)",
            )),
            (PermissionAction::Deny, Rule::Mcp(server)) => Some(error_output(
                name,
                &format!("permission denied by [permissions.mcp] {server}"),
            )),
            (PermissionAction::Deny, Rule::Tool) => Some(error_output(
                name,
                &format!("permission denied by [permissions.tools.{name}]"),
            )),
            (PermissionAction::Ask, _) => self.ask_user(
                PermissionPrompt {
                    call_id: id.clone(),
                    tool: name.to_owned(),
                    subject,
                    input: input.clone(),
                },
                &rule,
            ),
        }
    }
}

/// Where a verdict came from, so refusals name the right remedy.
enum Rule {
    /// The session mode override.
    Mode,
    /// A `[permissions.tools.<name>]` entry or the gate's fallback.
    Tool,
    /// The `[permissions.mcp]` action for this server.
    Mcp(String),
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
fn non_interactive_output(tool: &str, rule: &Rule) -> ToolOutput {
    let text = match rule {
        Rule::Mcp(server) => format!(
            "permission is `ask` (MCP tools ask by default) and this mode is non-interactive; \
             allow the server with `{server} = \"allow\"` under [permissions.mcp], \
             allow the tool with `default = \"allow\"` under [permissions.tools.{tool}], \
             or run the interactive TUI"
        ),
        Rule::Mode | Rule::Tool => format!(
            "permission is `ask` ([permissions.tools.{tool}]) and this mode is non-interactive; \
             add an allow rule or run the interactive TUI"
        ),
    };
    error_output(tool, &text)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use kage_core::permissions::{PermissionAction, ToolPermissionRules};
    use kage_loop::NoopHooks;

    use super::*;

    struct Ask {
        tool: String,
        subject: String,
        reply: crossbeam_channel::Sender<PermissionDecision>,
    }

    /// An asker that hands each prompt to the test over a channel.
    fn channel_asker() -> (Asker, mpsc::Receiver<Ask>) {
        let (tx, rx) = mpsc::channel();
        let asker: Asker = Arc::new(move |prompt: PermissionPrompt| {
            let (reply, answer) = crossbeam_channel::bounded(1);
            tx.send(Ask {
                tool: prompt.tool,
                subject: prompt.subject,
                reply,
            })
            .ok()
            .map(|()| answer)
        });
        (asker, rx)
    }

    fn rules_for(default: PermissionAction) -> PermissionsConfig {
        PermissionsConfig {
            confine_paths: false,
            mcp: std::collections::BTreeMap::new(),
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
        assert!(
            gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
                .is_none()
        );
        assert!(
            gate.before_tool_call(
                &kage_core::ToolCallId::new("call"),
                "write",
                &serde_json::json!({})
            )
            .is_none()
        );
    }

    #[test]
    fn mode_deny_overrides_allow_rules() {
        let mut gate = PermissionGate::new(rules_for(PermissionAction::Allow));
        gate.set_mode(Some(PermissionAction::Deny));
        let out = gate
            .before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
            .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("permission mode is deny"), "{}", out.text);
        gate.set_mode(None);
        assert!(
            gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
                .is_none()
        );
    }

    #[test]
    fn mode_ask_overrides_allow_rules_and_uses_channel() {
        let (ask_tx, ask_rx) = channel_asker();
        let gate = PermissionGate::new(rules_for(PermissionAction::Allow)).with_asker(ask_tx);
        gate.set_mode(Some(PermissionAction::Ask));
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let out =
                gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input());
            let _ = done_tx.send(out.is_none());
        });
        let ask = ask_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        ask.reply.send(PermissionDecision::Deny).unwrap();
        handle.join().unwrap();
        assert!(!done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn mode_allow_does_not_override_configured_deny() {
        let gate = PermissionGate::new(rules_for(PermissionAction::Deny));
        let mut clone = gate.clone();
        gate.set_mode(Some(PermissionAction::Allow));
        let out = clone
            .before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            out.text,
            "`bash`: permission denied by [permissions.tools.bash]"
        );
        assert_eq!(clone.mode(), Some(PermissionAction::Allow));
    }

    #[test]
    fn mode_ask_does_not_override_configured_deny() {
        let mut gate = PermissionGate::new(rules_for(PermissionAction::Deny));
        gate.set_mode(Some(PermissionAction::Ask));
        let out = gate
            .before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
            .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("permission denied by"), "{}", out.text);
    }

    #[test]
    fn deny_by_rule_synthesizes_error_output() {
        let mut gate = PermissionGate::new(rules_for(PermissionAction::Deny));
        let out = gate
            .before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            out.text,
            "`bash`: permission denied by [permissions.tools.bash]"
        );
    }

    #[test]
    fn ask_without_channel_denies_non_interactively() {
        let mut gate = PermissionGate::new(rules_for(PermissionAction::Ask));
        let out = gate
            .before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
            .unwrap();
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
        let (ask_tx, ask_rx) = channel_asker();
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask)).with_asker(ask_tx);
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let out =
                gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input());
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
        let (ask_tx, ask_rx) = channel_asker();
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask)).with_asker(ask_tx);
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let out =
                gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input());
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
        let (ask_tx, ask_rx) = channel_asker();
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask))
            .with_asker(ask_tx)
            .with_persist_path(path.clone());
        let gate2 = gate.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let allowed = gate
                .before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
                .is_none();
            let _ = done_tx.send(allowed);
        });
        let ask = ask_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        ask.reply.send(PermissionDecision::AllowAlways).unwrap();
        handle.join().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        // The in-memory rules flipped for every clone; no further ask.
        let mut gate2 = gate2;
        assert!(
            gate2
                .before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
                .is_none()
        );
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

    /// Run one bash call on `gate` under ask mode, answer the ask with
    /// `decision`, and return whether the call was allowed.
    fn answer_ask(gate: &PermissionGate, decision: PermissionDecision) -> bool {
        let (ask_tx, ask_rx) = channel_asker();
        let mut gate = gate.clone().with_asker(ask_tx);
        gate.set_mode(Some(PermissionAction::Ask));
        let handle = std::thread::spawn(move || {
            gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
                .is_none()
        });
        let ask = ask_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        ask.reply.send(decision).unwrap();
        handle.join().unwrap()
    }

    fn panicking_asker() -> Asker {
        Arc::new(|_| panic!("a session-allowed tool must not ask"))
    }

    #[test]
    fn allow_session_stops_asking_under_ask_mode_and_persists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let gate =
            PermissionGate::new(rules_for(PermissionAction::Ask)).with_persist_path(path.clone());
        assert!(answer_ask(&gate, PermissionDecision::AllowSession));
        let mut gate = gate.with_asker(panicking_asker());
        assert!(
            gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
                .is_none()
        );
        assert_eq!(lock(&gate.rules).check("bash", "ls"), PermissionAction::Ask);
        assert!(!path.exists());
    }

    #[test]
    fn allow_always_stops_asking_under_ask_mode_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let gate =
            PermissionGate::new(rules_for(PermissionAction::Ask)).with_persist_path(path.clone());
        assert!(answer_ask(&gate, PermissionDecision::AllowAlways));
        let mut gate = gate.with_asker(panicking_asker());
        assert!(
            gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
                .is_none()
        );
        let saved = Config::load(&path).unwrap();
        assert_eq!(
            saved.permissions.check("bash", "anything"),
            PermissionAction::Allow
        );
    }

    #[test]
    fn allow_always_on_an_unconfigured_edit_stops_asking_under_ask_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let edit = |file: &str| serde_json::json!({"path": file, "old_str": "a", "new_str": "b"});
        let gate =
            PermissionGate::new(PermissionsConfig::default()).with_persist_path(path.clone());
        gate.set_mode(Some(PermissionAction::Ask));
        let (ask_tx, ask_rx) = channel_asker();
        let mut first = gate.clone().with_asker(ask_tx);
        let first_input = edit("src/main.rs");
        let handle = std::thread::spawn(move || {
            first
                .before_tool_call(&kage_core::ToolCallId::new("call_1"), "edit", &first_input)
                .is_none()
        });
        let ask = ask_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(ask.tool, "edit");
        ask.reply.send(PermissionDecision::AllowAlways).unwrap();
        assert!(handle.join().unwrap());
        let mut second = gate.with_asker(panicking_asker());
        assert!(
            second
                .before_tool_call(
                    &kage_core::ToolCallId::new("call_2"),
                    "edit",
                    &edit("src/lib.rs")
                )
                .is_none()
        );
        let saved = Config::load(&path).unwrap();
        assert_eq!(
            saved.permissions.check("edit", "anything"),
            PermissionAction::Allow
        );
    }

    #[test]
    fn allow_always_adds_only_its_rule_to_a_hand_written_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# mine\n\n[ui]\ntheme = \"default\"  # by hand\n\n\
                        [providers.custom.fake]\nbase_url = \"http://127.0.0.1:1/v1\"\n\
                        api_key_env = \"\"\n\n[[providers.custom.fake.models]]\n\
                        id = \"small\"\nname = \"Small\"\n\n\
                        [mcp.servers.files]\ncommand = \"mcp-files\"\n";
        std::fs::write(&path, original).unwrap();
        let gate =
            PermissionGate::new(PermissionsConfig::default()).with_persist_path(path.clone());
        gate.persist_allow_always("write");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{original}\n[permissions.tools.write]\ndefault = \"allow\"\n")
        );
    }

    #[test]
    fn reset_session_drops_the_mode_and_approvals_but_keeps_always_allow() {
        let dir = tempfile::tempdir().unwrap();
        let mut rules = rules_for(PermissionAction::Ask);
        rules.tools.insert(
            "write".to_owned(),
            ToolPermissionRules {
                default: PermissionAction::Ask,
                allow: Vec::new(),
                deny: Vec::new(),
            },
        );
        let gate = PermissionGate::new(rules).with_persist_path(dir.path().join("config.toml"));
        assert!(answer_ask(&gate, PermissionDecision::AllowSession));
        gate.persist_allow_always("write");
        gate.reset_session();
        assert_eq!(gate.mode(), None);
        let mut gate = gate;
        let out = gate
            .before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
            .unwrap();
        assert!(out.text.contains("non-interactive"), "{}", out.text);
        assert!(call(&mut gate, "write").is_none());
    }

    #[test]
    fn a_configured_deny_pattern_outlives_a_session_approval() {
        let mut rules = rules_for(PermissionAction::Ask);
        rules.tools.get_mut("bash").unwrap().deny = vec!["rm *".to_owned()];
        let gate = PermissionGate::new(rules);
        assert!(answer_ask(&gate, PermissionDecision::AllowSession));
        let mut gate = gate.with_asker(panicking_asker());
        gate.set_mode(None);
        assert!(
            gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
                .is_none()
        );
        let out = gate
            .before_tool_call(
                &kage_core::ToolCallId::new("call"),
                "bash",
                &serde_json::json!({"command": "rm -rf target"}),
            )
            .unwrap();
        assert!(out.text.contains("permission denied"), "{}", out.text);
    }

    #[test]
    fn deny_mode_denies_a_session_allowed_tool() {
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask));
        assert!(answer_ask(&gate, PermissionDecision::AllowSession));
        let mut gate = gate;
        gate.set_mode(Some(PermissionAction::Deny));
        let out = gate
            .before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
            .unwrap();
        assert!(out.text.contains("permission mode is deny"), "{}", out.text);
    }

    #[test]
    fn cancel_during_ask_denies() {
        let (ask_tx, ask_rx) = channel_asker();
        let cancel = CancelFlag::new();
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask))
            .with_asker(ask_tx)
            .with_cancel(cancel.clone());
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let out =
                gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input());
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
        let (ask_tx, ask_rx) = channel_asker();
        let gate = PermissionGate::new(rules_for(PermissionAction::Ask)).with_asker(ask_tx);
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut gate = gate;
            let out =
                gate.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input());
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

    fn call(gate: &mut PermissionGate, tool: &str) -> Option<ToolOutput> {
        gate.before_tool_call(
            &kage_core::ToolCallId::new("call"),
            tool,
            &serde_json::json!({}),
        )
    }

    fn github_gate(rules: PermissionsConfig) -> PermissionGate {
        PermissionGate::new(rules).with_mcp_servers(vec!["github".to_owned()])
    }

    #[test]
    fn unconfigured_mcp_tool_asks_and_builtins_stay_allowed() {
        let mut gate = github_gate(PermissionsConfig::default());
        let out = call(&mut gate, "github__create_issue").unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("non-interactive"), "{}", out.text);
        assert!(call(&mut gate, "bash").is_none());
        assert!(call(&mut gate, "other__tool").is_none());
    }

    #[test]
    fn mcp_server_allow_and_deny_apply() {
        let mut rules = PermissionsConfig::default();
        rules
            .mcp
            .insert("github".to_owned(), PermissionAction::Allow);
        let mut gate = github_gate(rules.clone());
        assert!(call(&mut gate, "github__create_issue").is_none());

        rules
            .mcp
            .insert("github".to_owned(), PermissionAction::Deny);
        let mut gate = github_gate(rules);
        let out = call(&mut gate, "github__create_issue").unwrap();
        assert_eq!(
            out.text,
            "`github__create_issue`: permission denied by [permissions.mcp] github"
        );
    }

    #[test]
    fn per_tool_entry_beats_server_default() {
        let mut rules = PermissionsConfig::default();
        rules
            .mcp
            .insert("github".to_owned(), PermissionAction::Deny);
        rules.tools.insert(
            "github__list_issues".to_owned(),
            ToolPermissionRules::default(),
        );
        let mut gate = github_gate(rules);
        assert!(call(&mut gate, "github__list_issues").is_none());
        assert!(call(&mut gate, "github__create_issue").is_some());
    }

    #[test]
    fn server_names_containing_separator_match_by_prefix() {
        let mut rules = PermissionsConfig::default();
        rules.mcp.insert("a__b".to_owned(), PermissionAction::Allow);
        let mut gate =
            PermissionGate::new(rules).with_mcp_servers(vec!["a".to_owned(), "a__b".to_owned()]);
        assert!(call(&mut gate, "a__b__tool").is_none());
        assert!(call(&mut gate, "a__tool").is_some());
    }

    #[test]
    fn allowed_server_skips_the_asker_under_ask_fallback() {
        let mut rules = PermissionsConfig::default();
        rules
            .mcp
            .insert("github".to_owned(), PermissionAction::Allow);
        let asker: Asker = Arc::new(|_| panic!("allowed server must not ask"));
        let mut gate = github_gate(rules)
            .with_fallback(PermissionAction::Ask)
            .with_asker(asker);
        assert!(call(&mut gate, "github__create_issue").is_none());
    }

    #[test]
    fn print_mode_refusal_names_both_remedies() {
        let mut gate = github_gate(PermissionsConfig::default());
        let out = call(&mut gate, "github__create_issue").unwrap();
        assert!(
            out.text
                .contains(r#"`github = "allow"` under [permissions.mcp]"#),
            "{}",
            out.text
        );
        assert!(
            out.text
                .contains("[permissions.tools.github__create_issue]"),
            "{}",
            out.text
        );
    }

    #[test]
    fn gate_composes_as_plain_hooks() {
        // The gate must work wherever a `dyn Hooks` is placed, like
        // the innermost layer under TuiHooks.
        let mut gate = PermissionGate::new(PermissionsConfig::default());
        let hooks: &mut dyn Hooks = &mut gate;
        assert!(
            hooks
                .before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
                .is_none()
        );
        let noop: &mut dyn Hooks = &mut NoopHooks;
        assert!(
            noop.before_tool_call(&kage_core::ToolCallId::new("call"), "bash", &bash_input())
                .is_none()
        );
    }
}
