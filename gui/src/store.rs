//! The store: the shell's handle on the [`kage_client::Client`].
//!
//! The store owns the client, mirrors transport events into it, runs
//! the initialize gate, and drives the boot flow: handshake first,
//! then one session, then the replay prompt when playing the
//! recording. Commands it cannot carry out itself come back to the
//! shell, which owns the transport. Views read the store and call its
//! command methods; they never see frames.

use kage_client::wire::{
    ContentBlock, FsListResult, FsOp, FsReadResult, NoticeTone, PermissionOption,
    PermissionOptionKind, SessionConfigOption,
};
use kage_client::{
    Change, Client, Frame, PermissionAsk, PromptOutcome, Session, SteerError, TranscriptItem,
};

use gpui_kit::{App, Entity};

use crate::gate::{self, Report};
use crate::prefs::Prefs;
use std::collections::HashMap;

use crate::timing::{SessionTimes, Timings};
use crate::transport::{Link, State};
use crate::views::composer::{PLAN_MODE, active_mode};

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

/// A message for the user the shell toasts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Severity.
    pub tone: NoticeTone,
    /// The message.
    pub text: String,
    /// The archived session a click on the toast restores.
    pub undo_archive: Option<String>,
}

impl Note {
    fn new(tone: NoticeTone, text: String) -> Self {
        Self {
            tone,
            text,
            undo_archive: None,
        }
    }
}

/// What a fork the user asked for does once its copy is made.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ForkPlan {
    /// Open the copy.
    Fork,
    /// Open the copy in place of the source: the copy takes the
    /// source's title, the source keeps the old state under a "(before
    /// rewind)" title, and the rewound prompt goes back to the composer.
    Rewind {
        /// The source's title, which the copy takes.
        title: Option<String>,
        /// The prompt the rewind discarded.
        prompt: String,
    },
}

/// The tool whose open ask is the plan mode review.
pub(crate) const EXIT_PLAN_TOOL: &str = "exit_plan";

/// The open plan review of a session: the exit plan ask and the
/// options it offers for each answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanReviewState {
    /// The id the decision answers.
    pub request_id: u64,
    /// The `exit_plan` call under review.
    pub call_id: String,
    /// The offered approve option, when the ask offers one.
    pub approve: Option<String>,
    /// The offered revise option, when the ask offers one.
    pub revise: Option<String>,
    /// The offered reject option, when the ask offers one.
    pub reject: Option<String>,
}

/// The first offered option whose id or name is `name`, case
/// insensitively. A review ask carries two reject-kind options, so
/// the kind alone cannot tell revise from reject; the option ids the
/// agent reads decisions from can.
fn option_named(options: &[PermissionOption], name: &str) -> Option<String> {
    options
        .iter()
        .find(|option| {
            option.option_id.eq_ignore_ascii_case(name) || option.name.eq_ignore_ascii_case(name)
        })
        .map(|option| option.option_id.clone())
}

/// The open plan review of a session, when one is pending.
#[must_use]
pub(crate) fn plan_review(session: &Session) -> Option<PlanReviewState> {
    let ask = session
        .permissions
        .iter()
        .find(|ask| ask.tool_call.title.as_deref() == Some(EXIT_PLAN_TOOL))?;
    let approve = match ask.option_of(PermissionOptionKind::AllowOnce) {
        Some(id) => Some(id.to_owned()),
        None => option_named(&ask.options, "approve"),
    };
    Some(PlanReviewState {
        request_id: ask.request_id,
        call_id: ask.tool_call.tool_call_id.clone(),
        approve,
        revise: option_named(&ask.options, "revise"),
        reject: option_named(&ask.options, "reject"),
    })
}

/// An answer to a plan review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanChoice {
    /// Build with the plan.
    Approve,
    /// Rework the plan with the given text.
    Revise,
    /// Drop the plan and leave plan mode.
    Reject,
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
    /// Sessions whose transcript moved while another one showed.
    unread: std::collections::HashSet<String>,
    /// The client's own preferences.
    prefs: Prefs,
    /// Bumped on every preferences change, so the shell stores each
    /// change once.
    prefs_rev: u64,
    /// Per session, the permission mode under plan mode. The wire shows
    /// only `plan` while plan mode is on; the engine keeps the
    /// permission mode underneath, and leaving plan mode restores it.
    permissions: HashMap<String, String>,
    /// The last `_kage/fs` listing answer, held with the session it
    /// ran against for the picker that asked. A later answer replaces
    /// it.
    fs_listing: Option<(String, FsListResult)>,
    /// The path the last `_kage/fs` read asked for, until it answers.
    fs_reading: Option<String>,
    /// The last file read: the session, the path and the answer.
    fs_preview: Option<(String, String, FsReadResult)>,
    /// The last `_kage/config/get` answer, raw as the wire carried it.
    config: Option<serde_json::Value>,
    /// The last `_kage/options` answer: the engine options in effect.
    engine_options: Option<Vec<kage_client::wire::OptionEntry>>,
    /// Forks waiting for their copy, by source session.
    forking: HashMap<String, ForkPlan>,
    /// Copies waiting to open, with their source, by copy.
    forked: HashMap<String, (String, ForkPlan)>,
    /// Messages for the user the shell toasts.
    notes: Vec<Note>,
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
            permissions: HashMap::new(),
            prefs: Prefs::default(),
            prefs_rev: 0,
            unread: std::collections::HashSet::new(),
            fs_listing: None,
            fs_reading: None,
            fs_preview: None,
            config: None,
            engine_options: None,
            forking: HashMap::new(),
            forked: HashMap::new(),
            notes: Vec::new(),
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
    /// to open those, and a plan review is answered on its plan card.
    #[must_use]
    pub fn active_asks(&self) -> Vec<(&str, &PermissionAsk)> {
        let Some(active) = self.active.as_deref() else {
            return Vec::new();
        };
        let state = self.state();
        state
            .open_asks()
            .into_iter()
            .filter(|(_, ask)| ask.tool_call.title.as_deref() != Some(EXIT_PLAN_TOOL))
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
        self.unread.remove(&id);
        self.active = Some(id);
    }

    /// Whether session `id` moved since the user last looked at it.
    #[must_use]
    pub fn is_unread(&self, id: &str) -> bool {
        self.unread.contains(id)
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
        for change in &changes {
            if let Change::Transcript { id } = change
                && self.active.as_deref() != Some(id.as_str())
                && self
                    .state()
                    .session(id)
                    .is_some_and(|session| session.parent.is_none() && session.opened)
            {
                self.unread.insert(id.clone());
            }
        }
        for id in touched {
            if let Some(session) = self.client.state().session(id) {
                self.timings.observe(session);
                if let Some(mode) = active_mode(session).filter(|mode| mode != PLAN_MODE) {
                    self.permissions.insert(id.to_owned(), mode);
                }
            }
        }
        if let Some(active) = self.active.clone()
            && changes.contains(&Change::Session { id: active.clone() })
        {
            self.keep_template(&active);
        }
        for change in &changes {
            if let Change::Session { id } = change {
                let opened = self.state().session(id).is_some_and(|s| s.opened);
                if self.replay && !self.prompted && opened && self.active.as_deref() == Some(id) {
                    self.prompted = true;
                    self.commands.push(Command::ReplayPrompt);
                }
            }
            match change {
                Change::Forked { from, to } => self.open_fork(from, to),
                Change::Config { config } => self.config = Some(config.clone()),
                Change::Options { options } => self.engine_options = Some(options.clone()),
                Change::Session { id } => self.settle_fork(id),
                Change::Fs {
                    session_id,
                    result: kage_client::wire::FsResult::List(listing),
                } => self.merge_listing(session_id, listing),
                Change::Fs {
                    session_id,
                    result: kage_client::wire::FsResult::Read(read),
                } => {
                    if let Some(path) = self.fs_reading.take() {
                        self.fs_preview = Some((session_id.clone(), path, read.clone()));
                    }
                }
                _ => {}
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

    /// Loads subagent `id`'s transcript when this client holds none of
    /// it: a child that ran before this client attached replays through
    /// `session/load`, while a live one already streams here.
    pub fn load_child(&mut self, id: &str) {
        if self
            .state()
            .session(id)
            .is_some_and(|session| !session.items.is_empty())
        {
            return;
        }
        let cwd = self
            .active_session()
            .and_then(|session| session.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        self.client.load_session(id, &cwd, &[]);
    }

    /// Stops session `id`: a subagent's stop button.
    pub fn cancel_session(&mut self, id: &str) {
        self.client.cancel(id);
    }

    /// Continues the given swarm children of the active session with
    /// their own task. Reports whether a request went out.
    pub fn resume_swarm(&mut self, members: &[String]) -> bool {
        let Some(session) = self.active.clone() else {
            return false;
        };
        if members.is_empty() {
            return false;
        }
        let members = members
            .iter()
            .map(|id| (id.clone(), String::new()))
            .collect();
        self.client.swarm_resume(&session, members);
        true
    }

    /// Copies the active session into a new one and opens it. With
    /// `through`, the copy keeps the transcript up to the end of that
    /// item's turn; without, all of it. Reports whether a request went
    /// out.
    pub fn fork(&mut self, through: Option<usize>) -> bool {
        let Some(session) = self.active_session() else {
            return false;
        };
        let before = through.and_then(|index| {
            let next = session.items[index + 1..]
                .iter()
                .position(|item| matches!(item, TranscriptItem::User { .. }))?;
            session.prompt_ref(index + 1 + next)
        });
        let id = session.id.clone();
        self.forking.insert(id.clone(), ForkPlan::Fork);
        self.client.fork_session(&id, before);
        true
    }

    /// Rewinds the active session to just before the prompt at item
    /// `prompt`: a copy without it and what followed opens in the
    /// session's place, the session stays as it was under a "(before
    /// rewind)" title, and the prompt text comes back to the composer.
    /// Refused while a run is in flight. Reports whether a request went
    /// out.
    pub fn rewind(&mut self, prompt: usize) -> bool {
        let Some(session) = self.active_session() else {
            return false;
        };
        if session.running || session.in_turn {
            return false;
        }
        let Some(before) = session.prompt_ref(prompt) else {
            return false;
        };
        let id = session.id.clone();
        let plan = ForkPlan::Rewind {
            title: session.title.clone(),
            prompt: before.text.clone(),
        };
        self.forking.insert(id.clone(), plan);
        self.client.fork_session(&id, Some(before));
        true
    }

    /// Opens the copy a fork made, refreshing the directory so it nests
    /// under its source.
    fn open_fork(&mut self, from: &str, to: &str) {
        let plan = self.forking.remove(from).unwrap_or(ForkPlan::Fork);
        self.forked.insert(to.to_owned(), (from.to_owned(), plan));
        let listed = (!self.cwd.is_empty()).then(|| self.cwd.clone());
        self.client.list_sessions(listed.as_deref(), None);
        let cwd = self
            .state()
            .session(from)
            .and_then(|session| session.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());
        self.client.load_session(to, &cwd, &[]);
        self.unread.remove(to);
        self.active = Some(to.to_owned());
    }

    /// Carries out the rest of a fork's plan once its copy opened.
    fn settle_fork(&mut self, id: &str) {
        if !self
            .state()
            .session(id)
            .is_some_and(|session| session.opened)
        {
            return;
        }
        let Some((from, plan)) = self.forked.remove(id) else {
            return;
        };
        let source = self
            .state()
            .session(&from)
            .and_then(|session| session.title.clone())
            .unwrap_or_else(|| "untitled session".to_owned());
        match plan {
            ForkPlan::Fork => self.notes.push(Note::new(
                NoticeTone::Success,
                format!("Forked \"{source}\""),
            )),
            ForkPlan::Rewind { title, prompt } => {
                let kept = format!("{} (before rewind)", title.as_deref().unwrap_or(&source));
                if let Some(title) = &title {
                    self.client.rename_session(id, title);
                }
                self.client.rename_session(&from, &kept);
                self.returned_prompt = Some(prompt);
                self.notes.push(Note::new(
                    NoticeTone::Success,
                    format!("Rewound; the old state is \"{kept}\""),
                ));
            }
        }
    }

    /// Names the active session. Blank titles are ignored. Reports
    /// whether a request went out.
    pub fn rename(&mut self, title: &str) -> bool {
        let title = title.trim();
        let Some(id) = self.active.clone().filter(|_| !title.is_empty()) else {
            return false;
        };
        self.client.rename_session(&id, title);
        true
    }

    /// Summarizes the active session's older turns now. Refused while a
    /// run is in flight. Reports whether a request went out.
    pub fn compact(&mut self) -> bool {
        let Some(session) = self.active_session() else {
            return false;
        };
        if session.running || session.in_turn {
            return false;
        }
        let id = session.id.clone();
        self.client.compact_session(&id);
        true
    }

    /// Asks for the active session as Markdown; the text arrives as
    /// [`Change::Exported`]. Reports whether a request went out.
    pub fn export(&mut self) -> bool {
        let Some(id) = self.active.clone() else {
            return false;
        };
        self.client.export_session(&id);
        true
    }

    /// Releases the active session and shows the welcome pane. The
    /// session stays recorded and listed.
    pub fn close_active(&mut self) {
        let Some(id) = self.active.clone() else {
            return;
        };
        self.client.close_session(&id);
        let cwd = (!self.cwd.is_empty()).then(|| self.cwd.clone());
        self.client.list_sessions(cwd.as_deref(), None);
        self.show_welcome();
    }

    /// The session `id` was forked from, as the directory lists it.
    #[must_use]
    pub fn fork_parent(&self, id: &str) -> Option<&str> {
        self.state()
            .directory
            .iter()
            .find(|info| info.session_id == id)?
            .meta
            .as_ref()?
            .kage
            .as_ref()?
            .parent_session_id
            .as_deref()
    }

    /// Asks the engine for its configuration snapshot; the answer
    /// replaces [`Store::config`].
    pub fn ask_config(&mut self) {
        self.client.config_get();
    }

    /// Asks the engine for its options; the answer replaces
    /// [`Store::engine_options`].
    pub fn ask_engine_options(&mut self) {
        self.client.options_list();
    }

    /// The engine options as the engine last answered.
    #[must_use]
    pub fn engine_options(&self) -> Option<&[kage_client::wire::OptionEntry]> {
        self.engine_options.as_deref()
    }

    /// Stores engine option `name` in the user config; the answer
    /// refreshes [`Store::engine_options`], a refusal toasts.
    pub fn set_engine_option(&mut self, name: &str, value: serde_json::Value) {
        self.client.set_engine_option(name, value);
    }

    /// The last configuration snapshot the engine answered with.
    #[must_use]
    pub fn config(&self) -> Option<&serde_json::Value> {
        self.config.as_ref()
    }

    /// The config options the composer shows for `id`: the active
    /// session's, or on the welcome pane the last session's with the
    /// choices held for the next session applied.
    #[must_use]
    pub fn composer_option(&self, id: &str) -> Option<SessionConfigOption> {
        if let Some(session) = self.active_session() {
            return session
                .config_options
                .iter()
                .find(|option| option.id == id)
                .cloned();
        }
        let mut option = self
            .prefs
            .template
            .iter()
            .find(|option| option.id == id)
            .cloned()?;
        if let Some((_, value)) = self.held_options.iter().find(|(held, _)| held == id) {
            option.current_value.clone_from(value);
        }
        Some(option)
    }

    /// Remembers the model, thinking and mode options of session `id`,
    /// the plan mode aside, for the welcome pane to offer.
    fn keep_template(&mut self, id: &str) {
        let Some(session) = self
            .state()
            .session(id)
            .filter(|s| s.opened && s.parent.is_none())
        else {
            return;
        };
        let mut template: Vec<SessionConfigOption> = session
            .config_options
            .iter()
            .filter(|option| matches!(option.id.as_str(), "model" | "thinking" | "mode"))
            .cloned()
            .collect();
        for option in &mut template {
            if option.id == "mode" && option.current_value == PLAN_MODE {
                option.current_value = self
                    .permissions
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| "default".to_owned());
            }
        }
        if !template.is_empty() {
            self.update_prefs(|prefs| prefs.template = template);
        }
    }

    /// The messages for the user since the last call, oldest first.
    pub fn take_notes(&mut self) -> Vec<Note> {
        std::mem::take(&mut self.notes)
    }

    /// The title session `id` shows under, from its state or the
    /// directory.
    #[must_use]
    pub fn session_title(&self, id: &str) -> Option<&str> {
        self.state()
            .session(id)
            .and_then(|session| session.title.as_deref())
            .or_else(|| {
                self.state()
                    .directory
                    .iter()
                    .find(|info| info.session_id == id)?
                    .title
                    .as_deref()
            })
    }

    /// Pins session `id` to the top of the sidebar, or unpins it.
    pub fn toggle_pin(&mut self, id: &str) {
        self.update_prefs(|prefs| {
            if !prefs.pinned.remove(id) {
                prefs.pinned.insert(id.to_owned());
            }
        });
    }

    /// Takes session `id` out of the sidebar until it is restored; the
    /// welcome shows when it was the active one. The toast that says so
    /// restores it on click.
    pub fn archive(&mut self, id: &str) {
        let title = self
            .session_title(id)
            .unwrap_or("untitled session")
            .to_owned();
        self.update_prefs(|prefs| {
            prefs.archived.insert(id.to_owned());
        });
        if self.active.as_deref() == Some(id) {
            self.show_welcome();
        }
        self.notes.push(Note {
            tone: NoticeTone::Info,
            text: format!("Archived \"{title}\"; click to undo"),
            undo_archive: Some(id.to_owned()),
        });
    }

    /// Brings archived session `id` back to the sidebar.
    pub fn restore(&mut self, id: &str) {
        self.update_prefs(|prefs| {
            prefs.archived.remove(id);
        });
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
            self.hold_option(id, value);
            return true;
        };
        if id == "mode" && value != PLAN_MODE {
            self.permissions.insert(session.clone(), value.to_owned());
        }
        self.client.set_config_option(&session, id, value);
        true
    }

    /// Answers the active session's plan review with the offered option
    /// `choice` names, exactly the id the ask offered. A revise carries
    /// its text through the client's feedback channel. Reports whether
    /// an answer went out.
    pub fn review_plan(&mut self, choice: PlanChoice, revision: Option<&str>) -> bool {
        let Some(session_id) = self.active.clone() else {
            return false;
        };
        let Some(review) = self.active_session().and_then(plan_review) else {
            return false;
        };
        let offered = match choice {
            PlanChoice::Approve => review.approve,
            PlanChoice::Revise => review.revise,
            PlanChoice::Reject => review.reject,
        };
        let Some(option_id) = offered else {
            return false;
        };
        let decision = match revision.map(str::trim).filter(|text| !text.is_empty()) {
            Some(feedback) => kage_client::PermissionDecision::Feedback {
                option_id,
                feedback: feedback.to_owned(),
            },
            None => kage_client::PermissionDecision::Option(option_id),
        };
        self.client
            .reply_permission(&session_id, review.request_id, &decision)
    }

    /// Whether swarm cards draw the constellation.
    #[must_use]
    pub fn constellation(&self) -> bool {
        self.prefs.constellation
    }

    /// Starts from stored preferences.
    #[must_use]
    pub fn with_prefs(mut self, prefs: Prefs) -> Self {
        self.prefs = prefs;
        self
    }

    /// The client's own preferences.
    #[must_use]
    pub fn prefs(&self) -> &Prefs {
        &self.prefs
    }

    /// Changes the preferences; the shell stores the result.
    pub fn update_prefs(&mut self, change: impl FnOnce(&mut Prefs)) {
        let before = self.prefs.clone();
        change(&mut self.prefs);
        if self.prefs != before {
            self.prefs_rev += 1;
        }
    }

    /// The preferences revision, bumped on every change.
    #[must_use]
    pub fn prefs_rev(&self) -> u64 {
        self.prefs_rev
    }

    /// Whether the active session is in plan mode.
    #[must_use]
    pub fn plan_on(&self) -> bool {
        self.active_session()
            .and_then(active_mode)
            .is_some_and(|mode| mode == PLAN_MODE)
    }

    /// The permission mode of the active session: its mode, or under
    /// plan mode the mode plan mode will return to.
    #[must_use]
    pub fn permission_mode(&self) -> Option<String> {
        let session = self.active_session()?;
        let mode = active_mode(session)?;
        if mode != PLAN_MODE {
            return Some(mode);
        }
        Some(
            self.permissions
                .get(&session.id)
                .cloned()
                .unwrap_or_else(|| "default".to_owned()),
        )
    }

    /// Sets the permission mode of the active session. Under plan mode
    /// the choice waits for plan mode to end, as the engine keeps one
    /// mode on the wire.
    pub fn set_permission(&mut self, value: &str) -> bool {
        let Some(id) = self.active.clone() else {
            self.hold_option("mode", value);
            return true;
        };
        if self.plan_on() {
            self.permissions.insert(id, value.to_owned());
            return true;
        }
        self.set_option("mode", value)
    }

    /// Turns plan mode on for the active session.
    pub fn enter_plan(&mut self) -> bool {
        if self.plan_on() {
            return false;
        }
        self.set_option("mode", PLAN_MODE)
    }

    /// Turns plan mode off, back to the permission mode it held.
    pub fn exit_plan(&mut self) -> bool {
        if !self.plan_on() {
            return false;
        }
        let mode = self
            .permission_mode()
            .unwrap_or_else(|| "default".to_owned());
        self.set_option("mode", &mode)
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

    /// The workdir entries `_kage/fs` listed for `session` so far, every
    /// answer merged, for the mention menu and the files pane. Truncated
    /// while a listed subtree was cut short and not yet continued.
    #[must_use]
    pub fn fs_listing(&self, session: &str) -> Option<&FsListResult> {
        let (id, listing) = self.fs_listing.as_ref()?;
        (id == session).then_some(listing)
    }

    /// Folds one listing answer into the session's merged listing.
    fn merge_listing(&mut self, session: &str, listing: &FsListResult) {
        match &mut self.fs_listing {
            Some((id, merged)) if id == session => {
                for entry in &listing.entries {
                    if !merged.entries.iter().any(|known| known.path == entry.path) {
                        merged.entries.push(entry.clone());
                    }
                }
                merged.entries.sort_by(|a, b| a.path.cmp(&b.path));
                merged.truncated = listing.truncated;
            }
            _ => self.fs_listing = Some((session.to_owned(), listing.clone())),
        }
    }

    /// Reads `path` under the active session's workdir through
    /// `_kage/fs`; the answer lands in [`Store::fs_preview`].
    pub fn fs_read(&mut self, path: &str) -> bool {
        let Some(session) = self.active.clone() else {
            return false;
        };
        self.fs_reading = Some(path.to_owned());
        self.client.fs(&session, FsOp::Read, path);
        true
    }

    /// The last file `_kage/fs` read for `session`: its path and the
    /// answer.
    #[must_use]
    pub fn fs_preview(&self, session: &str) -> Option<(&str, &FsReadResult)> {
        let (id, path, read) = self.fs_preview.as_ref()?;
        (id == session).then_some((path.as_str(), read))
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
    use kage_client::wire::NoticeTone;
    use kage_client::{Frame, PromptOutcome, SteerError, TranscriptItem};

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

        let sent_mode = |store: &mut Store| -> Vec<String> {
            store
                .take_outgoing()
                .into_iter()
                .filter_map(|frame| match frame {
                    Frame::Request { method, params, .. }
                        if method == "session/set_config_option"
                            && params["configId"] == "mode" =>
                    {
                        params["value"].as_str().map(str::to_owned)
                    }
                    _ => None,
                })
                .collect()
        };
        // Shift+Tab walks the permission modes; plan mode is its own chip.
        for expected in ["ask", "allow", "deny", "default", "ask"] {
            let current = store.permission_mode().unwrap();
            let next =
                crate::views::composer::next_mode_value(store.active_session().unwrap(), &current);
            assert_eq!(next.as_deref(), Some(expected));
            assert!(store.set_permission(expected));
            assert_eq!(sent_mode(&mut store), [expected], "one frame per step");
        }

        assert!(store.enter_plan());
        assert_eq!(sent_mode(&mut store), ["plan"]);
        assert!(store.plan_on());
        assert_eq!(
            store.permission_mode().as_deref(),
            Some("ask"),
            "kept under plan"
        );
        assert!(store.set_permission("allow"));
        assert!(
            sent_mode(&mut store).is_empty(),
            "a choice under plan waits"
        );
        assert!(store.exit_plan());
        assert_eq!(
            sent_mode(&mut store),
            ["allow"],
            "plan ends on the waiting choice"
        );
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

    /// The requests in `frames` as (id, method, params).
    fn requests(frames: Vec<Frame>) -> Vec<(u64, String, serde_json::Value)> {
        frames
            .into_iter()
            .filter_map(|frame| match frame {
                Frame::Request { id, method, params } => Some((id, method, params)),
                _ => None,
            })
            .collect()
    }

    /// A store on session `s1`, titled "Parser", that ran the prompts
    /// "again", "next" and "again".
    fn three_prompts() -> Store {
        let mut store = welcome_store();
        open_session(&mut store, 3, "s1");
        store.absorb(Frame::Notification {
            method: "session/update".into(),
            params: serde_json::json!({
                "sessionId": "s1",
                "update": {"sessionUpdate": "session_info_update", "title": "Parser"},
            }),
        });
        for text in ["again", "next", "again"] {
            store.prompt(text);
            let (id, ..) = requests(store.take_outgoing()).remove(0);
            store.absorb(Frame::Success {
                id,
                result: serde_json::json!({"stopReason": "end_turn"}),
            });
        }
        let _ = store.take_outgoing();
        store
    }

    /// Answers the fork request in `frames` with copy `to`, then the
    /// copy's load, and returns what the store sent meanwhile.
    fn land_fork(
        store: &mut Store,
        frames: Vec<Frame>,
        to: &str,
    ) -> Vec<(u64, String, serde_json::Value)> {
        let (fork, ..) = requests(frames)
            .into_iter()
            .find(|(_, method, _)| method == "_kage/session/fork")
            .expect("a fork request");
        store.absorb(Frame::Success {
            id: fork,
            result: serde_json::json!({"sessionId": to}),
        });
        let mut sent = requests(store.take_outgoing());
        let load = sent
            .iter()
            .find(|(_, method, _)| method == "session/load")
            .map(|(id, ..)| *id)
            .expect("the copy loads");
        store.absorb(Frame::Success {
            id: load,
            result: serde_json::json!({}),
        });
        sent.extend(requests(store.take_outgoing()));
        sent
    }

    #[test]
    fn a_fork_keeps_the_turn_it_starts_from_and_opens_the_copy() {
        let mut store = three_prompts();
        let first = store
            .active_session()
            .unwrap()
            .items
            .iter()
            .position(|item| matches!(item, TranscriptItem::User { .. }))
            .unwrap();

        assert!(store.fork(Some(first)));
        let frames = store.take_outgoing();
        let (_, _, params) = requests(frames.clone()).remove(0);
        assert_eq!(
            params["before"],
            serde_json::json!({"text": "next", "occurrence": 0})
        );
        let sent = land_fork(&mut store, frames, "s2");
        assert!(sent.iter().any(|(_, method, _)| method == "session/list"));
        assert_eq!(store.active_id(), Some("s2"));
        let notes = store.take_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            (notes[0].tone, notes[0].text.as_str()),
            (NoticeTone::Success, "Forked \"Parser\"")
        );
    }

    #[test]
    fn a_rewind_opens_the_copy_under_the_old_title_and_returns_the_prompt() {
        let mut store = three_prompts();
        let last = store
            .active_session()
            .unwrap()
            .items
            .iter()
            .rposition(|item| matches!(item, TranscriptItem::User { .. }))
            .unwrap();

        assert!(store.rewind(last));
        let frames = store.take_outgoing();
        let (_, _, params) = requests(frames.clone()).remove(0);
        assert_eq!(
            params["before"],
            serde_json::json!({"text": "again", "occurrence": 1})
        );
        let sent = land_fork(&mut store, frames, "s2");
        let renames: Vec<(String, String)> = sent
            .iter()
            .filter(|(_, method, _)| method == "_kage/session/rename")
            .map(|(_, _, params)| {
                (
                    params["sessionId"].as_str().unwrap().to_owned(),
                    params["title"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            renames,
            [
                ("s2".to_owned(), "Parser".to_owned()),
                ("s1".to_owned(), "Parser (before rewind)".to_owned())
            ]
        );
        assert_eq!(store.active_id(), Some("s2"));
        assert_eq!(store.take_returned_prompt().as_deref(), Some("again"));
    }

    #[test]
    fn compact_and_rewind_wait_for_the_run_and_close_keeps_the_listing() {
        let mut store = three_prompts();
        store.prompt("busy");
        let _ = store.take_outgoing();
        assert!(!store.compact(), "compact waits for the run");
        assert!(!store.rewind(0), "a rewind waits for the run");

        store.close_active();
        let methods: Vec<String> = requests(store.take_outgoing())
            .into_iter()
            .map(|(_, method, _)| method)
            .collect();
        assert_eq!(methods, ["session/close", "session/list"]);
        assert_eq!(store.active_id(), None);
    }

    #[test]
    fn the_welcome_offers_the_last_sessions_options_and_holds_a_choice() {
        let mut store = welcome_store();
        store.new_session();
        let (id, ..) = requests(store.take_outgoing()).remove(0);
        store.absorb(Frame::Success {
            id,
            result: serde_json::json!({
                "sessionId": "s1",
                "configOptions": [
                    {"id": "model", "name": "Model", "type": "select", "currentValue": "a:one",
                     "options": [{"value": "a:one", "name": "One"}, {"value": "a:two", "name": "Two"}]},
                    {"id": "swarm", "name": "Swarm", "type": "select", "currentValue": "off",
                     "options": [{"value": "off", "name": "Off"}, {"value": "on", "name": "On"}]},
                ],
            }),
        });
        let ids: Vec<&str> = store
            .prefs()
            .template
            .iter()
            .map(|o| o.id.as_str())
            .collect();
        assert_eq!(ids, ["model"], "only the options a new session starts from");

        store.show_welcome();
        assert_eq!(
            store.composer_option("model").unwrap().current_value,
            "a:one"
        );
        assert!(
            store.set_option("model", "a:two"),
            "the welcome holds the choice"
        );
        assert_eq!(
            store.composer_option("model").unwrap().current_value,
            "a:two"
        );
        assert!(
            requests(store.take_outgoing()).is_empty(),
            "nothing goes out yet"
        );
    }
}
