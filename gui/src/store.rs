//! The store: the shell's handle on the [`kage_client::Client`].
//!
//! The store owns the client, mirrors transport events into it, runs
//! the initialize gate, and drives the boot flow: handshake first,
//! then one session, then the replay prompt when playing the
//! recording. Commands it cannot carry out itself come back to the
//! shell, which owns the transport. Views read the store and call its
//! command methods; they never see frames.

use kage_client::wire::{ContentBlock, FsListResult, FsOp};
use kage_client::{Change, Client, Frame, PromptOutcome, Session, SteerError};

use crate::gate::{self, Report};
use crate::transport::State;

/// What the store asks the shell to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run the handshake: `initialize`, and `session/load` for every
    /// session already open when the link came back.
    Handshake {
        /// True when this is a reconnect, so open sessions replay.
        replay_sessions: bool,
    },
    /// Open the first session of a fresh connection.
    NewSession,
    /// Send the prompt the recording expects, once its session is
    /// open.
    ReplayPrompt,
}

/// The client state plus everything the shell and views read around
/// it.
#[derive(Debug)]
pub struct Store {
    client: Client,
    /// Where the connection stands, as the transport last reported.
    connect: State,
    /// What the last initialize answer fell short of.
    gate: Report,
    /// Whether the user dismissed the gate banner for this answer.
    gate_dismissed: bool,
    /// Whether a Connected was seen before, so the next one is a
    /// reconnect.
    had_link: bool,
    /// Whether the boot flow already opened its first session.
    booted: bool,
    /// Whether the connection plays the recording, which wants the
    /// scripted prompt.
    replay: bool,
    /// Whether the scripted prompt already went out.
    prompted: bool,
    /// The directory sessions open in.
    cwd: String,
    /// The session the transcript view follows.
    active: Option<String>,
    /// The last `_kage/fs` listing answer, held with the session it
    /// ran against for the picker that asked. A later answer replaces
    /// it.
    fs_listing: Option<(String, FsListResult)>,
    /// Commands waiting for the shell.
    commands: Vec<Command>,
}

impl Store {
    /// A store for a connection that opens sessions in `cwd`; `replay`
    /// selects the scripted prompt of the recording.
    #[must_use]
    pub fn new(cwd: impl Into<String>, replay: bool) -> Self {
        Self {
            client: Client::new(),
            connect: State::Connecting,
            gate: Report::default(),
            gate_dismissed: false,
            had_link: false,
            booted: false,
            replay,
            prompted: false,
            cwd: cwd.into(),
            active: None,
            fs_listing: None,
            commands: Vec::new(),
        }
    }

    /// The client state as the frames left it.
    #[must_use]
    pub fn state(&self) -> &kage_client::State {
        self.client.state()
    }

    /// Where the connection stands.
    #[must_use]
    pub fn connect(&self) -> &State {
        &self.connect
    }

    /// What the last initialize answer fell short of.
    #[must_use]
    pub fn gate(&self) -> &Report {
        &self.gate
    }

    /// Whether the banner for the current gate report was dismissed.
    #[must_use]
    pub fn gate_dismissed(&self) -> bool {
        self.gate_dismissed
    }

    /// The session the transcript follows.
    #[must_use]
    pub fn active_session(&self) -> Option<&Session> {
        let id = self.active.as_deref()?;
        self.state().session(id)
    }

    /// The id of the session the transcript follows.
    #[must_use]
    pub fn active_id(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Follows another session.
    pub fn set_active(&mut self, id: impl Into<String>) {
        self.active = Some(id.into());
    }

    /// Accepts one incoming frame and reports what moved. The gate is
    /// rechecked on every initialize answer, and the boot flow reacts
    /// to the changes it has been waiting for.
    pub fn absorb(&mut self, frame: Frame) -> Vec<Change> {
        let changes = self.client.handle(frame);
        if changes.contains(&Change::Connection) {
            self.gate = gate::check(self.client.state());
            self.gate_dismissed = false;
            if !self.booted {
                self.booted = true;
                self.commands.push(Command::NewSession);
            }
        }
        for change in &changes {
            if let Change::Session { id } = change {
                let opened = self.state().session(id).is_some_and(|s| s.opened);
                if self.active.is_none() && opened {
                    self.active = Some(id.clone());
                }
                if self.replay && !self.prompted && opened {
                    self.prompted = true;
                    self.commands.push(Command::ReplayPrompt);
                }
            }
            if let Change::Fs {
                session_id,
                result: kage_client::wire::FsResult::List(listing),
            } = change
            {
                self.fs_listing = Some((session_id.clone(), listing.clone()));
            }
        }
        changes
    }

    /// Records a connect-state move and returns what the shell must
    /// carry out for it.
    pub fn set_connect(&mut self, state: State) {
        if state.is_connected() && !self.connect.is_connected() {
            let replay_sessions = self.had_link;
            self.had_link = true;
            self.commands.push(Command::Handshake { replay_sessions });
        }
        self.connect = state;
    }

    /// Runs the handshake: `initialize` always, `session/load` for
    /// every open session when the link came back.
    pub fn handshake(&mut self, replay_sessions: bool) {
        self.client.initialize(
            kage_client::wire::ClientCapabilities {
                subagents: Some(serde_json::json!({})),
                ..kage_client::wire::ClientCapabilities::default()
            },
            Some(kage_client::wire::Implementation {
                name: env!("CARGO_PKG_NAME").to_owned(),
                title: None,
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        );
        if replay_sessions {
            let open: Vec<(String, Option<String>)> = self
                .state()
                .sessions
                .values()
                .filter(|session| session.opened)
                .map(|session| (session.id.clone(), session.cwd.clone()))
                .collect();
            for (id, cwd) in open {
                self.client
                    .load_session(&id, cwd.as_deref().unwrap_or(&self.cwd), &[]);
            }
        }
    }

    /// Opens a fresh session in the store's directory.
    pub fn new_session(&mut self) {
        self.client.new_session(&self.cwd, &[]);
    }

    /// Sends one text prompt to the session the transcript follows.
    /// Returns false when there is no session to prompt.
    pub fn prompt(&mut self, text: &str) -> bool {
        let Some(id) = self.active.clone() else {
            return false;
        };
        let _ = self.client.prompt(&id, vec![ContentBlock::text(text)]);
        true
    }

    /// Sends or queues `text` on the active session: plain when idle,
    /// queued while a run is in flight. Reports what became of it.
    #[must_use]
    pub fn submit(&mut self, text: &str) -> Option<PromptOutcome> {
        let id = self.active.clone()?;
        let outcome = self.client.prompt(&id, vec![ContentBlock::text(text)]);
        Some(outcome)
    }

    /// Steers the run in flight on the active session with `text`.
    /// Reports why nothing went out: no session, no run, or the agent
    /// never advertised steering.
    pub fn steer(&mut self, text: &str) -> Result<u64, SteerError> {
        let Some(id) = self.active.clone() else {
            return Err(SteerError::NotRunning);
        };
        self.client.steer(&id, vec![ContentBlock::text(text)])
    }

    /// Cancels the run of the active session. Reports whether there
    /// was a session to cancel.
    pub fn cancel(&mut self) -> bool {
        let Some(id) = self.active.clone() else {
            return false;
        };
        self.client.cancel(&id);
        true
    }

    /// Answers the open permission ask of `session`, by request id.
    /// Reports whether the ask was found and the answer queued.
    pub fn reply_permission(
        &mut self,
        session: &str,
        request_id: u64,
        decision: &kage_client::PermissionDecision,
    ) -> bool {
        self.client.reply_permission(session, request_id, decision)
    }

    /// Stores the composer draft of `session`, when the state knows
    /// it.
    pub fn set_draft(&mut self, session: Option<&str>, text: &str) {
        if let Some(id) = session {
            self.client.set_draft(id, text);
        }
    }

    /// The stored draft of `session`.
    #[must_use]
    pub fn draft(&self, session: &str) -> Option<&str> {
        self.state().draft(session)
    }

    /// Sets one config option of the active session, by option id.
    /// Reports whether a session is open to carry it.
    pub fn set_option(&mut self, id: &str, value: &str) -> bool {
        let Some(session) = self.active.clone() else {
            return false;
        };
        self.client.set_config_option(&session, id, value);
        true
    }

    /// Lists the active session's workdir through `_kage/fs`. The
    /// answer lands in [`Store::fs_listing`]. Reports whether a
    /// session is open to ask.
    pub fn fs_list(&mut self, path: &str) -> bool {
        let Some(session) = self.active.clone() else {
            return false;
        };
        self.client.fs(&session, FsOp::List, path);
        true
    }

    /// The last `_kage/fs` listing answer for `session`, for the
    /// picker that asked.
    #[must_use]
    pub fn fs_listing(&self, session: &str) -> Option<&FsListResult> {
        let (id, listing) = self.fs_listing.as_ref()?;
        (id == session).then_some(listing)
    }

    /// Removes the queued prompt at `index` of the active session
    /// before it went on the wire.
    pub fn withdraw_queued(&mut self, index: usize) -> bool {
        let Some(session) = self.active.clone() else {
            return false;
        };
        self.client.withdraw_queued(&session, index)
    }

    /// Sends the queued prompt at `index` of the active session as a
    /// steer on the run in flight.
    pub fn steer_queued(&mut self, index: usize) -> Result<u64, SteerError> {
        let Some(session) = self.active.clone() else {
            return Err(SteerError::NotRunning);
        };
        self.client.steer_queued(&session, index)
    }

    /// Dismisses the gate banner until the next initialize answer.
    pub fn dismiss_gate(&mut self) {
        self.gate_dismissed = true;
    }

    /// The commands the shell must carry out, drained.
    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.commands)
    }

    /// The frames to send, in order, drained from the client.
    pub fn take_outgoing(&mut self) -> Vec<Frame> {
        self.client.take_outgoing()
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Store};
    use crate::transport::State;
    use kage_client::{Frame, PromptOutcome, SteerError};

    /// An initialize answer with the given version and capabilities.
    fn init_answer(version: Option<&str>, steer: bool, close: bool) -> Frame {
        init_answer_on(1, version, steer, close)
    }

    /// The same answer with an explicit id, as a re-handshake lands on a
    /// fresh one.
    fn init_answer_on(id: u64, version: Option<&str>, steer: bool, close: bool) -> Frame {
        let agent = version.map(|version| {
            serde_json::json!({
                "name": "kage", "version": version,
            })
        });
        let mut capabilities = serde_json::json!({ "steer": steer });
        if close {
            capabilities["sessionCapabilities"] = serde_json::json!({ "close": {} });
        }
        Frame::Success {
            id,
            result: serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": capabilities,
                "agentInfo": agent,
            }),
        }
    }

    /// Carries out `commands` the way the shell does, so the
    /// client's requests are actually sent.
    fn run(store: &mut Store, commands: Vec<Command>) {
        for command in commands {
            match command {
                Command::Handshake { replay_sessions } => store.handshake(replay_sessions),
                Command::NewSession => store.new_session(),
                Command::ReplayPrompt => {
                    store.prompt("fix the null check");
                }
            }
        }
    }

    /// Drains and carries out the store's pending commands the way
    /// the shell does.
    fn run_commands(store: &mut Store) {
        let commands = store.take_commands();
        run(store, commands);
    }

    #[test]
    fn the_first_connect_handshakes_and_boots_one_session() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connecting);
        assert!(store.take_commands().is_empty());
        store.set_connect(State::Connected);
        assert_eq!(
            store.take_commands(),
            vec![Command::Handshake {
                replay_sessions: false
            }]
        );
        store.handshake(false);
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1, "only initialize goes out first");
        assert!(matches!(&outgoing[0], Frame::Request { method, .. } if method == "initialize"));

        store.absorb(init_answer(Some("0.1.0"), true, true));
        assert!(store.gate().is_clean());
        assert_eq!(store.take_commands(), vec![Command::NewSession]);
        store.new_session();
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        assert!(matches!(&outgoing[0], Frame::Request { method, .. } if method == "session/new"));

        store.absorb(Frame::Success {
            id: 2,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        assert_eq!(store.active_id(), Some("s1"));
        assert!(
            store.take_commands().is_empty(),
            "no scripted prompt off replay"
        );
        assert_eq!(store.take_outgoing(), vec![]);
    }

    #[test]
    fn a_reconnect_handshakes_again_and_replays_open_sessions() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        for command in store.take_commands() {
            match command {
                Command::Handshake { replay_sessions } => store.handshake(replay_sessions),
                Command::NewSession => store.new_session(),
                Command::ReplayPrompt => {
                    store.prompt("fix the null check");
                }
            }
        }
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        for command in store.take_commands() {
            if let Command::NewSession = command {
                store.new_session();
            }
        }
        store.absorb(Frame::Success {
            id: 2,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        store.handshake(false);
        let _ = store.take_outgoing();

        store.set_connect(State::Closed);
        store.set_connect(State::Connecting);
        store.set_connect(State::Connected);
        assert_eq!(
            store.take_commands(),
            vec![Command::Handshake {
                replay_sessions: true
            }]
        );
        store.handshake(true);
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 2, "initialize then session/load");
        assert!(matches!(&outgoing[0], Frame::Request { method, .. } if method == "initialize"));
        assert!(matches!(&outgoing[1], Frame::Request { method, .. } if method == "session/load"));
        match &outgoing[1] {
            Frame::Request { params, .. } => {
                assert_eq!(params["sessionId"], "s1");
            }
            other => panic!("expected the load, got {other:?}"),
        }
    }

    #[test]
    fn the_replay_flow_prompts_once_when_the_session_opens() {
        let mut store = Store::new("/w", true);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let commands = store.take_commands();
        assert_eq!(commands, vec![Command::NewSession]);
        run(&mut store, commands);
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 2,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        assert_eq!(store.take_commands(), vec![Command::ReplayPrompt]);
        assert!(store.prompt("fix the null check"));
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        match &outgoing[0] {
            Frame::Request { method, params, .. } => {
                assert_eq!(method, "session/prompt");
                assert_eq!(params["prompt"][0]["text"], "fix the null check");
            }
            other => panic!("expected the prompt, got {other:?}"),
        }
        assert_eq!(store.take_commands(), vec![], "the prompt goes out once");
    }

    #[test]
    fn a_too_old_agent_raises_the_gate_but_not_the_boot() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.0.9"), false, false));
        assert!(!store.gate().is_clean());
        assert_eq!(store.gate().too_old.as_deref(), Some("0.0.9"));
        assert_eq!(store.gate().missing.len(), 2);
        assert!(!store.gate_dismissed());
        store.dismiss_gate();
        assert!(store.gate_dismissed());
        let commands = store.take_commands();
        assert_eq!(commands, vec![Command::NewSession]);
        run(&mut store, commands);
        let _ = store.take_outgoing();

        store.set_connect(State::Closed);
        store.set_connect(State::Connecting);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let frames = store.take_outgoing();
        let init_id = frames
            .iter()
            .find_map(|frame| match frame {
                Frame::Request { id, method, .. } if method == "initialize" => Some(*id),
                _ => None,
            })
            .expect("a re-handshake sends initialize");
        store.absorb(init_answer_on(init_id, Some("0.1.0"), true, true));
        assert!(store.gate().is_clean(), "a new answer rechecks the gate");
        assert!(!store.gate_dismissed());
    }

    #[test]
    fn a_running_enter_queues_and_ctrl_enter_steers_with_the_capability() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 2,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        let _ = store.take_outgoing();

        assert_eq!(
            store.submit("hello"),
            Some(PromptOutcome::Sent { request_id: 3 })
        );
        let outgoing = store.take_outgoing();
        assert!(matches!(&outgoing[0], Frame::Request { method, params, .. }
            if method == "session/prompt" && params.get("delivery").is_none()));

        assert_eq!(
            store.submit("later"),
            Some(PromptOutcome::Queued),
            "a running turn holds plain submits in the queue"
        );
        assert!(store.take_outgoing().is_empty(), "queuing sends no frame");
        assert_eq!(store.state().session("s1").unwrap().queue.len(), 1);

        assert_eq!(store.steer("hurry"), Ok(4), "ctrl-enter steers the run");
        let outgoing = store.take_outgoing();
        assert!(matches!(&outgoing[0], Frame::Request { method, params, .. }
            if method == "session/prompt" && params["delivery"] == "steer"));
    }

    #[test]
    fn ctrl_enter_without_the_capability_and_double_esc_send_the_wire_frames() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), false, true));
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 2,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        let _ = store.take_outgoing();

        assert_eq!(
            store.submit("hello"),
            Some(PromptOutcome::Sent { request_id: 3 })
        );
        let _ = store.take_outgoing();
        assert_eq!(
            store.steer("hurry"),
            Err(SteerError::NotAdvertised),
            "no capability, no steer"
        );
        assert!(store.take_outgoing().is_empty());

        assert!(store.cancel());
        let outgoing = store.take_outgoing();
        assert!(matches!(&outgoing[0], Frame::Notification { method, .. }
            if method == "session/cancel"));
    }

    #[test]
    fn the_draft_survives_a_switch_away_and_back() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 2,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        let _ = store.take_outgoing();
        store.new_session();
        store.absorb(Frame::Success {
            id: 3,
            result: serde_json::json!({"sessionId": "s2"}),
        });
        let _ = store.take_outgoing();

        store.set_active("s1");
        let active = store.active_id().map(str::to_owned);
        store.set_draft(active.as_deref(), "typed and never sent");
        store.set_active("s2");
        let active = store.active_id().map(str::to_owned);
        store.set_draft(active.as_deref(), "another session, another draft");
        store.set_active("s1");
        assert_eq!(
            store.draft("s1"),
            Some("typed and never sent"),
            "switching back restores the draft"
        );
        assert_eq!(store.draft("s2"), Some("another session, another draft"));
    }

    #[test]
    fn mode_cycles_exactly_the_advertised_values() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 2,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        let _ = store.take_outgoing();
        store.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "config_option_update",
                    "configOptions": [{
                        "id": "mode", "name": "Mode", "category": "mode",
                        "type": "select", "currentValue": "default",
                        "options": [
                            {"value": "default", "name": "Default"},
                            {"value": "ask", "name": "Ask"},
                            {"value": "allow", "name": "Allow"},
                            {"value": "deny", "name": "Deny"},
                            {"value": "plan", "name": "Plan"}
                        ]
                    }]
                },
            }),
        });

        let mut sent = 0usize;
        for expected in ["ask", "allow", "deny", "plan", "default"] {
            assert!(
                crate::views::composer::next_mode_value(store.active_session().unwrap(),)
                    .is_some_and(|next| next == expected)
            );
            assert!(store.set_option("mode", expected));
            let outgoing = store.take_outgoing();
            assert_eq!(outgoing.len(), 1, "one set_config_option per step");
            let Frame::Request { id, method, params } = &outgoing[0] else {
                panic!("expected a request, got {:?}", outgoing[0]);
            };
            assert_eq!(method, "session/set_config_option");
            assert_eq!(params["configId"], "mode");
            sent += 1;
            store.absorb(Frame::Success {
                id: *id,
                result: serde_json::json!({"configOptions": [{
                    "id": "mode", "name": "Mode", "category": "mode",
                    "type": "select", "currentValue": expected,
                    "options": [
                        {"value": "default", "name": "Default"},
                        {"value": "ask", "name": "Ask"},
                        {"value": "allow", "name": "Allow"},
                        {"value": "deny", "name": "Deny"},
                        {"value": "plan", "name": "Plan"}
                    ]
                }]}),
            });
        }
        assert_eq!(sent, 5, "one cycle through every advertised value");
        assert!(store.take_outgoing().is_empty(), "everything was drained");
    }

    #[test]
    fn the_fs_listing_holds_for_the_mention_menu() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: 2,
            result: serde_json::json!({"sessionId": "s1"}),
        });
        let _ = store.take_outgoing();

        assert!(store.fs_listing("s1").is_none(), "nothing asked yet");
        assert!(store.fs_list(""));
        store.absorb(Frame::Success {
            id: 3,
            result: serde_json::json!({
                "op": "list",
                "entries": [
                    {"path": "src", "kind": "directory", "size": 0},
                    {"path": "src/main.rs", "kind": "file", "size": 12}
                ],
                "truncated": false
            }),
        });
        let listing = store.fs_listing("s1").unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert!(
            store.fs_listing("other").is_none(),
            "the listing is per session"
        );
    }

    #[test]
    fn a_prompt_without_a_session_sends_nothing() {
        let mut store = Store::new("/w", false);
        assert!(!store.prompt("hello"));
        assert!(store.take_outgoing().is_empty());
    }
}
