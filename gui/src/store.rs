//! The store: the shell's handle on the [`kage_client::Client`].
//!
//! The store owns the client, mirrors transport events into it, runs
//! the initialize gate, and drives the boot flow: handshake first,
//! then one session, then the replay prompt when playing the
//! recording. Commands it cannot carry out itself come back to the
//! shell, which owns the transport. Views read the store and call its
//! command methods; they never see frames.

use kage_client::wire::{ContentBlock, FsListResult, FsOp};
use kage_client::{Change, Client, Frame, PermissionAsk, PromptOutcome, Session, SteerError};

use gpui_kit::{App, Entity};

use crate::gate::{self, Report};
use crate::timing::{SessionTimes, Timings};
use crate::transport::{Link, State};

/// Store mutations that queue frames or move state go through
/// [`StoreHandle::act`], which notifies the store's observers: the
/// shell flushes the outgoing frames on that notify, and the views
/// redraw. A bare `update` that skips the notify leaves the frames in
/// the client until something unrelated notifies.
pub trait StoreHandle {
    /// Runs `f` on the store, then notifies its observers.
    fn act<R>(&self, cx: &mut App, f: impl FnOnce(&mut Store) -> R) -> R;
}

impl StoreHandle for Entity<Store> {
    fn act<R>(&self, cx: &mut App, f: impl FnOnce(&mut Store) -> R) -> R {
        self.update(cx, |store, cx| {
            let result = f(store);
            cx.notify();
            result
        })
    }
}

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
    /// What this client timed itself as frames arrived.
    timings: Timings,
    /// What the transport connects to.
    link: Link,
    /// The session the transcript view follows. `None` is the welcome
    /// state, where the next prompt opens the session it rides on.
    active: Option<String>,
    /// The prompt typed on the welcome pane, waiting for the session
    /// it opens.
    pending_prompt: Option<String>,
    /// The `session/new` request the user asked for, until it answers.
    /// Only its session becomes active; a session that opens on its own
    /// leaves the welcome pane where it is.
    opening: Option<u64>,
    /// A welcome prompt whose session failed to open, for the composer
    /// to take back.
    returned_prompt: Option<String>,
    /// The options a welcome card chose, set on the session the welcome
    /// prompt opens before the prompt goes out.
    held_options: Vec<(String, String)>,
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
            timings: Timings::default(),
            link: Link::serve(""),
            active: None,
            pending_prompt: None,
            opening: None,
            returned_prompt: None,
            held_options: Vec::new(),
            fs_listing: None,
            commands: Vec::new(),
        }
    }

    /// Names what the transport connects to.
    #[must_use]
    pub fn with_link(mut self, link: Link) -> Self {
        self.link = link;
        self
    }

    /// What the transport connects to.
    #[must_use]
    pub fn link(&self) -> &Link {
        &self.link
    }

    /// The directory a new session opens in: the shell's own, or the
    /// engine's when the shell has none, as in a browser.
    #[must_use]
    pub fn session_dir(&self) -> Option<&str> {
        if self.cwd.is_empty() {
            self.state().agent_cwd.as_deref()
        } else {
            Some(&self.cwd)
        }
    }

    /// What this client timed of session `id` as its frames arrived.
    #[must_use]
    pub fn timings(&self, id: &str) -> Option<&SessionTimes> {
        self.timings.session(id)
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

    /// The open asks of the active session and the subagents under it,
    /// oldest session first. Asks of other sessions wait for the user
    /// to open those.
    #[must_use]
    pub fn active_asks(&self) -> Vec<(&str, &PermissionAsk)> {
        let Some(active) = self.active.as_deref() else {
            return Vec::new();
        };
        let state = self.state();
        state
            .open_asks()
            .into_iter()
            .filter(|(id, _)| {
                let mut at = Some(*id);
                // A parent chain is short; the bound only guards a cycle.
                for _ in 0..32 {
                    match at {
                        Some(id) if id == active => return true,
                        Some(id) => {
                            at = state
                                .session(id)
                                .and_then(|session| session.parent.as_deref());
                        }
                        None => return false,
                    }
                }
                false
            })
            .collect()
    }

    /// The id of the session the transcript follows.
    #[must_use]
    pub fn active_id(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Follows another session. A session the state only knows from
    /// the directory is loaded first, so clicking a recorded row
    /// actually opens it.
    pub fn set_active(&mut self, id: impl Into<String>) {
        let id = id.into();
        let known = self.state().session(&id).is_some();
        if !known {
            let cwd = self
                .state()
                .directory
                .iter()
                .find(|info| info.session_id == id)
                .map(|info| info.cwd.clone())
                .unwrap_or_else(|| self.cwd.clone());
            self.client.load_session(&id, &cwd, &[]);
        }
        self.active = Some(id);
    }

    /// Leaves the active session: the welcome pane shows, and the
    /// next prompt opens the session it rides on. This is what the
    /// New session control does, as the web client's does.
    pub fn show_welcome(&mut self) {
        self.active = None;
        self.pending_prompt = None;
        self.opening = None;
        self.held_options.clear();
    }

    /// Holds config option `id` at `value` for the session the welcome
    /// prompt opens. A later hold of the same option replaces it.
    pub fn hold_option(&mut self, id: &str, value: &str) {
        self.held_options.retain(|(held, _)| held != id);
        self.held_options.push((id.to_owned(), value.to_owned()));
    }

    /// Accepts one incoming frame and reports what moved. The gate is
    /// rechecked on every initialize answer, and the boot flow reacts
    /// to the changes it has been waiting for.
    pub fn absorb(&mut self, frame: Frame) -> Vec<Change> {
        let answered = match &frame {
            Frame::Success { id, .. } | Frame::Failure { id, .. } => Some(*id),
            _ => None,
        };
        let changes = self.client.handle(frame);
        if answered.is_some() && answered == self.opening {
            self.opening = None;
            self.settle_opening(&changes);
        }
        if changes.contains(&Change::Connection) {
            self.gate = gate::check(self.client.state());
            self.gate_dismissed = false;
            if !self.booted {
                self.booted = true;
                if self.replay {
                    // The recording opens its session itself and its
                    // frame ids are fixed, so nothing else rides the
                    // link.
                    self.commands.push(Command::NewSession);
                } else {
                    // The recorded sessions fill the sidebar's project
                    // groups, and a live connection lands on the
                    // welcome pane, the way the web client boots.
                    // A browser knows no directory of its own, so it
                    // lists every recorded session the server holds.
                    let cwd = (!self.cwd.is_empty()).then_some(self.cwd.as_str());
                    self.client.list_sessions(cwd, None);
                }
            }
        }
        let mut touched: Vec<&str> = changes.iter().filter_map(Change::session_id).collect();
        touched.sort_unstable();
        touched.dedup();
        for id in touched {
            if let Some(session) = self.client.state().session(id) {
                self.timings.observe(session);
            }
        }
        for change in &changes {
            if let Change::Session { id } = change {
                let opened = self.state().session(id).is_some_and(|s| s.opened);
                if self.replay && !self.prompted && opened && self.active.as_deref() == Some(id) {
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

    /// Lands the answer to the user's `session/new`: the session it
    /// made becomes active and takes the welcome prompt, or, when the
    /// open failed, the prompt goes back to the composer.
    fn settle_opening(&mut self, changes: &[Change]) {
        let opened = changes.iter().find_map(|change| match change {
            Change::Session { id } => Some(id.clone()),
            _ => None,
        });
        match opened {
            Some(id) => {
                for (option, value) in std::mem::take(&mut self.held_options) {
                    let offered = self.state().session(&id).is_some_and(|session| {
                        session.config_options.iter().any(|offer| {
                            offer.id == option
                                && (offer.options.is_empty()
                                    || offer.options.iter().any(|choice| choice.value == value))
                        })
                    });
                    if offered {
                        self.client.set_config_option(&id, &option, &value);
                    }
                }
                if let Some(text) = self.pending_prompt.take() {
                    let _ = self.client.prompt(&id, vec![ContentBlock::text(text)]);
                }
                self.active = Some(id);
            }
            None => self.returned_prompt = self.pending_prompt.take(),
        }
    }

    /// The welcome prompt whose session failed to open, taken once.
    pub fn take_returned_prompt(&mut self) -> Option<String> {
        self.returned_prompt.take()
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
                // The app runs routine work like the TUI: tools without
                // a rule run, and configured asks and MCP tools ask.
                meta: Some(kage_client::wire::ClientMeta {
                    kage: Some(kage_client::wire::KageClientCapabilities {
                        unconfigured_tools: Some(kage_client::wire::UnconfiguredTools::Allow),
                    }),
                }),
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
        self.opening = Some(self.client.new_session(&self.cwd, &[]));
    }

    /// Opens a fresh session to carry `text`, the prompt a welcome
    /// pane took. The prompt goes out the moment the session is live;
    /// until then [`Store::pending_prompt`] tells whether one waits.
    pub fn open_with_prompt(&mut self, text: &str) {
        self.pending_prompt = Some(text.to_owned());
        self.new_session();
    }

    /// Whether a welcome-typed prompt waits for its session.
    #[must_use]
    pub fn pending_prompt(&self) -> bool {
        self.pending_prompt.is_some()
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
    use super::{Command, Store, StoreHandle as _};
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
    fn the_first_connect_handshakes_and_lands_on_the_welcome() {
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
        assert!(
            store.take_commands().is_empty(),
            "a live connection boots into the welcome, no session"
        );
        assert_eq!(store.active_id(), None);
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1, "the directory page is asked");
        assert!(matches!(&outgoing[0], Frame::Request { method, .. } if method == "session/list"));
        assert_eq!(store.take_outgoing(), vec![]);
    }

    #[test]
    fn a_browser_without_a_directory_lists_every_session() {
        let mut store = Store::new("", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let outgoing = store.take_outgoing();
        let Frame::Request { method, params, .. } = &outgoing[0] else {
            panic!("expected the list, got {:?}", outgoing[0]);
        };
        assert_eq!(method, "session/list");
        assert!(params.get("cwd").is_none(), "no cwd filter: {params}");
    }

    /// Opens a session the way the shell does, answering its
    /// `session/new`, so the tests have one to work with.
    fn open_session(store: &mut Store, reply_id: u64, id: &str) {
        store.new_session();
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: reply_id,
            result: serde_json::json!({ "sessionId": id }),
        });
        let _ = store.take_outgoing();
    }

    #[test]
    fn a_reconnect_handshakes_again_and_replays_open_sessions() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");

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
        let _ = store.take_outgoing();
        assert_eq!(
            store.take_commands(),
            vec![],
            "a dirty gate still boots into the welcome, no session"
        );

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
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");

        assert_eq!(
            store.submit("hello"),
            Some(PromptOutcome::Sent { request_id: 4 })
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

        assert_eq!(store.steer("hurry"), Ok(5), "ctrl-enter steers the run");
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
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");

        assert_eq!(
            store.submit("hello"),
            Some(PromptOutcome::Sent { request_id: 4 })
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
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");
        open_session(&mut store, 4, "s2");

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
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");
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
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");

        assert!(store.fs_listing("s1").is_none(), "nothing asked yet");
        assert!(store.fs_list(""));
        store.absorb(Frame::Success {
            id: 4,
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

    /// The shell flushes frames when the store notifies, so a welcome
    /// prompt that opens its session through `act` reaches the wire at
    /// once instead of waiting for an unrelated redraw.
    #[gpui_kit::test]
    fn act_notifies_so_queued_frames_flush(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;
        let store = cx.new(|_| Store::new("/w", false));
        let flushed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        cx.update(|cx| {
            let flushed = flushed.clone();
            cx.observe(&store, move |store, cx| {
                let frames = store.update(cx, |store, _| store.take_outgoing());
                flushed.borrow_mut().extend(frames);
            })
            .detach();
        });
        cx.update(|cx| store.act(cx, |store| store.open_with_prompt("fix the flake")));
        cx.run_until_parked();
        let flushed = flushed.borrow();
        assert_eq!(flushed.len(), 1, "the session/new went out on the notify");
        assert!(matches!(&flushed[0], Frame::Request { method, .. } if method == "session/new"));
    }

    #[test]
    fn a_prompt_without_a_session_sends_nothing() {
        let mut store = Store::new("/w", false);
        assert!(!store.prompt("hello"));
        assert!(store.take_outgoing().is_empty());
    }

    #[test]
    fn a_welcome_prompt_opens_its_session_and_sends_when_live() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        assert_eq!(store.active_id(), None, "the welcome state");

        store.open_with_prompt("fix the flake");
        assert!(store.pending_prompt(), "the prompt waits for its session");
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1);
        let Frame::Request { id, method, .. } = &outgoing[0] else {
            panic!("expected a request, got {:?}", outgoing[0]);
        };
        assert_eq!(method, "session/new");

        store.absorb(Frame::Success {
            id: *id,
            result: serde_json::json!({"sessionId": "s9"}),
        });
        assert_eq!(store.active_id(), Some("s9"));
        assert!(!store.pending_prompt(), "the held prompt went out");
        let outgoing = store.take_outgoing();
        assert!(matches!(&outgoing[0], Frame::Request { method, params, .. }
            if method == "session/prompt" && params["prompt"][0]["text"] == "fix the flake"));
    }

    #[test]
    fn the_welcome_clears_and_a_second_prompt_waits_again() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        open_session(&mut store, 3, "s1");

        store.show_welcome();
        assert_eq!(store.active_id(), None, "New session shows the welcome");

        store.open_with_prompt("again");
        let outgoing = store.take_outgoing();
        let Frame::Request { id, .. } = &outgoing[0] else {
            panic!("expected a request, got {:?}", outgoing[0]);
        };
        store.absorb(Frame::Success {
            id: *id,
            result: serde_json::json!({"sessionId": "s2"}),
        });
        assert_eq!(store.active_id(), Some("s2"));
        assert!(
            matches!(&store.take_outgoing()[0], Frame::Request { method, .. } if method == "session/prompt")
        );
    }

    #[test]
    fn following_a_directory_session_loads_it() {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();

        store.set_active("rec-1");
        let outgoing = store.take_outgoing();
        assert_eq!(outgoing.len(), 1, "an unknown row is loaded, not assumed");
        let Frame::Request { method, params, .. } = &outgoing[0] else {
            panic!("expected a request, got {:?}", outgoing[0]);
        };
        assert_eq!(method, "session/load");
        assert_eq!(params["sessionId"], "rec-1");
        assert_eq!(params["cwd"], "/w", "the store's directory carries it");
        assert_eq!(store.active_id(), Some("rec-1"));
    }

    /// A connected store on the welcome pane.
    fn welcome_store() -> Store {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        run_commands(&mut store);
        let _ = store.take_outgoing();
        store.absorb(init_answer(Some("0.1.0"), true, true));
        let _ = store.take_outgoing();
        store
    }

    #[test]
    fn a_session_moving_in_the_background_leaves_the_welcome_alone() {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        store.show_welcome();
        // A reconnect reopens every open session behind the welcome.
        store.handshake(true);
        let load = store
            .take_outgoing()
            .into_iter()
            .find_map(|frame| match frame {
                Frame::Request { id, method, .. } if method == "session/load" => Some(id),
                _ => None,
            })
            .expect("the open session reloads");
        store.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({}),
        });
        assert!(store.state().session("s1").unwrap().opened);
        assert_eq!(store.active_id(), None, "the welcome stays up");
    }

    #[test]
    fn a_failed_open_hands_the_welcome_prompt_back() {
        let mut store = welcome_store();
        store.open_with_prompt("fix the flake");
        let outgoing = store.take_outgoing();
        let Frame::Request { id, .. } = &outgoing[0] else {
            panic!("expected a request, got {:?}", outgoing[0]);
        };
        store.absorb(Frame::Failure {
            id: *id,
            error: kage_client::RpcError {
                code: -32603,
                message: "no provider".into(),
                data: None,
            },
        });
        assert!(!store.pending_prompt(), "nothing waits on a failed open");
        assert_eq!(store.active_id(), None);
        assert_eq!(
            store.take_returned_prompt().as_deref(),
            Some("fix the flake")
        );
        assert!(store.take_outgoing().is_empty(), "no prompt went out");
    }

    #[test]
    fn the_card_sees_the_asks_of_the_active_tree_only() {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        open_session(&mut store, 4, "s2");
        store.set_active("s1");
        store.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "update": {"sessionUpdate": "subagent_update", "subagentSessionId": "c1", "state": "running"},
            }),
        });
        let ask = |id: u64, session: &str| Frame::Request {
            id,
            method: "session/request_permission".into(),
            params: serde_json::json!({
                "sessionId": session,
                "toolCall": {"toolCallId": format!("call_{id}"), "title": "shell"},
                "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}],
            }),
        };
        store.absorb(ask(70, "s2"));
        store.absorb(ask(71, "c1"));
        store.absorb(ask(72, "s1"));
        let mut asked: Vec<&str> = store.active_asks().into_iter().map(|(id, _)| id).collect();
        asked.sort_unstable();
        assert_eq!(asked, ["c1", "s1"], "a child's ask shows on its parent");
        store.show_welcome();
        assert!(store.active_asks().is_empty(), "the welcome shows no card");
    }
}
