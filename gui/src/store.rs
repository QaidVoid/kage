//! The store: the shell's handle on the [`kage_client::Client`].
//!
//! The store owns the client, mirrors transport events into it, runs
//! the initialize gate, and drives the boot flow: handshake first,
//! then one session, then the replay prompt when playing the
//! recording. Commands it cannot carry out itself come back to the
//! shell, which owns the transport. Views read the store and call its
//! command methods; they never see frames.

use kage_client::wire::ContentBlock;
use kage_client::{Change, Client, Frame, Session};

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
    use kage_client::Frame;

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
    fn a_prompt_in_flight_steers_when_the_agent_advertises_it() {
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

        assert!(store.prompt("hello"));
        let outgoing = store.take_outgoing();
        assert!(matches!(&outgoing[0], Frame::Request { method, params, .. }
            if method == "session/prompt" && params.get("delivery").is_none()));

        assert!(store.prompt("again"), "steered prompts still send");
        let outgoing = store.take_outgoing();
        assert!(matches!(&outgoing[0], Frame::Request { method, params, .. }
            if method == "session/prompt" && params["delivery"] == "steer"));
    }

    #[test]
    fn a_prompt_without_a_session_sends_nothing() {
        let mut store = Store::new("/w", false);
        assert!(!store.prompt("hello"));
        assert!(store.take_outgoing().is_empty());
    }
}
