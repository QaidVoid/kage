//! The sans-IO client.
//!
//! A host feeds every frame the transport receives into
//! [`Client::handle`] and drains [`Client::take_outgoing`] to send
//! what the client produced. The client owns the request ids and the
//! pending requests, so answers land on the session that asked, and
//! it keeps [`State`] exactly as the frames delivered it. There is no
//! thread, no clock and no IO here, which makes the client testable
//! from recorded transcripts and usable on wasm.

use std::collections::BTreeMap;

use serde_json::Value;

use kage_acp_wire::{
    CancelNotification, ClientCapabilities, CloseSessionRequest, ConfigGetRequest, ContentBlock,
    FsOp, FsRequest, Implementation, InitializeRequest, KageMeta, ListSessionsRequest,
    LoadSessionRequest, McpServer, NewSessionRequest, PROTOCOL_VERSION, PermissionOptionKind,
    PermissionOutcome, PlanReview, PromptDelivery, PromptRequest, PromptResponse, RequestMeta,
    RequestPermissionRequest, RequestPermissionResult, ResumeSessionRequest, SelectedOption,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
};

use crate::change::Change;
use crate::frame::{Frame, RpcError};
use crate::state::{
    PermissionAsk, QueuedPrompt, Session, State, ToolCallItem, TranscriptItem, Usage,
};

/// The method an agent calls to ask for a tool call's verdict.
const ASK_METHOD: &str = "session/request_permission";

/// Why a steer did not go out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerError {
    /// No run is in flight on the session, so there is nothing to
    /// steer. Send the prompt plain instead.
    NotRunning,
    /// The agent did not advertise the steering capability, so the
    /// wire cannot join a run in flight. The prompt queues instead.
    NotAdvertised,
}

/// What [`Client::prompt`] did with a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptOutcome {
    /// Sent plain: the session was idle, so the run starts now.
    Sent {
        /// The id the stop reason comes back on.
        request_id: u64,
    },
    /// Sent with the steer marker to join the run already in flight
    /// at its next turn boundary. Only when the agent advertised the
    /// steering capability.
    Steered {
        /// The id the stop reason comes back on.
        request_id: u64,
    },
    /// Held in the session's queue because a run was in flight and
    /// the agent cannot steer. The client sends it itself, plain,
    /// when the run ends.
    Queued,
}

/// The verdict a user makes on a permission ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Pick the ask's `allow_once` option.
    Allow,
    /// Pick the ask's `allow_always` option.
    AllowAlways,
    /// Pick the ask's `reject_once` option.
    Reject,
    /// Pick the option with this id and carry `feedback` as the
    /// user's own words through the `_meta.kage.planReview` channel:
    /// the revise and reject-with-feedback answers of a plan review
    /// ride it.
    Feedback {
        /// The offered option the verdict picks, verbatim.
        option_id: String,
        /// The text the user typed.
        feedback: String,
    },
    /// Answer with the cancelled outcome, as when the dialog is
    /// dismissed without a choice.
    Cancel,
    /// Pick the option with this id verbatim, for asks whose options
    /// a host shows by id.
    Option(String),
}

/// What the client is still waiting to hear back about a request.
#[derive(Debug, Clone, PartialEq)]
enum Pending {
    Initialize,
    NewSession {
        cwd: String,
    },
    Open {
        session_id: String,
    },
    List {
        /// The directory filter of the first page, kept for the next.
        cwd: Option<String>,
    },
    Prompt {
        session_id: String,
        /// Whether the answer ends the run: false for a prompt that
        /// only steered the run another request owns.
        owns_run: bool,
    },
    SetConfigOption {
        session_id: String,
    },
    Close {
        session_id: String,
    },
    ConfigGet,
    Fs {
        session_id: String,
    },
}

/// The client side of an ACP connection.
#[derive(Debug, Default)]
pub struct Client {
    state: State,
    next_id: u64,
    pending: BTreeMap<u64, Pending>,
    outgoing: Vec<Frame>,
}

impl Client {
    /// A client that has sent nothing and heard nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The state as the handled frames left it.
    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The frames to send, in the order they were produced, drained
    /// by the call.
    pub fn take_outgoing(&mut self) -> Vec<Frame> {
        std::mem::take(&mut self.outgoing)
    }

    /// Stores the composer draft of `session_id`. See
    /// [`State::set_draft`].
    pub fn set_draft(&mut self, session_id: &str, text: &str) -> bool {
        self.state.set_draft(session_id, text)
    }

    /// Applies one incoming message and reports what moved. Server
    /// requests are answered here too: an unknown method is refused
    /// with a method-not-found error, and a malformed permission ask
    /// with an invalid-params error, both into the outgoing queue.
    pub fn handle(&mut self, frame: Frame) -> Vec<Change> {
        match frame {
            Frame::Request { id, method, params } => {
                self.handle_server_request(id, &method, params)
            }
            Frame::Notification { method, params } => self.handle_notification(&method, params),
            Frame::Success { id, result } => self.handle_success(id, result),
            Frame::Failure { id, error } => self.handle_failure(id, error),
        }
    }

    /// Starts the handshake. The answer carries the protocol version,
    /// the agent identity and the capabilities every later command
    /// checks, steering among them.
    pub fn initialize(
        &mut self,
        client_capabilities: ClientCapabilities,
        client_info: Option<Implementation>,
    ) -> u64 {
        self.request(
            "initialize",
            params(&InitializeRequest {
                protocol_version: PROTOCOL_VERSION,
                client_capabilities,
                client_info,
            }),
            Pending::Initialize,
        )
    }

    /// Opens a new session in `cwd`.
    pub fn new_session(&mut self, cwd: &str, mcp_servers: &[McpServer]) -> u64 {
        self.request(
            "session/new",
            params(&NewSessionRequest {
                cwd: cwd.to_owned(),
                mcp_servers: mcp_servers.to_vec(),
            }),
            Pending::NewSession {
                cwd: cwd.to_owned(),
            },
        )
    }

    /// Opens a recorded session and replays its transcript, which
    /// streams as updates before this answer arrives.
    pub fn load_session(&mut self, session_id: &str, cwd: &str, mcp_servers: &[McpServer]) -> u64 {
        self.open("session/load", session_id, cwd, mcp_servers)
    }

    /// Opens a recorded session without replaying its history.
    pub fn resume_session(
        &mut self,
        session_id: &str,
        cwd: &str,
        mcp_servers: &[McpServer],
    ) -> u64 {
        self.open("session/resume", session_id, cwd, mcp_servers)
    }

    /// Asks for one page of recorded sessions. Pages land in
    /// [`State::directory`], entries a page repeats are refreshed.
    /// A page that names a next cursor asks for that page itself, so
    /// the directory ends up whole.
    pub fn list_sessions(&mut self, cwd: Option<&str>, cursor: Option<&str>) -> u64 {
        self.request(
            "session/list",
            params(&ListSessionsRequest {
                cwd: cwd.map(str::to_owned),
                cursor: cursor.map(str::to_owned),
            }),
            Pending::List {
                cwd: cwd.map(str::to_owned),
            },
        )
    }

    /// Runs one prompt on `session_id`. A prompt that arrives while a
    /// run is in flight is held in the session's queue and sent plain
    /// when the run ends; joining a run in flight on purpose goes
    /// through [`Client::steer`].
    pub fn prompt(&mut self, session_id: &str, prompt: Vec<ContentBlock>) -> PromptOutcome {
        let running = self
            .state
            .sessions
            .get(session_id)
            .is_some_and(|session| session.running);
        if running {
            self.session_mut(session_id)
                .queue
                .push(QueuedPrompt { prompt });
            return PromptOutcome::Queued;
        }
        let request_id = self.send_prompt(session_id, prompt, None, true);
        PromptOutcome::Sent { request_id }
    }

    /// Steers the run in flight on `session_id` with `prompt`, joining
    /// it at its next turn boundary. Only when a run is in flight and
    /// the agent advertised the steering capability.
    pub fn steer(
        &mut self,
        session_id: &str,
        prompt: Vec<ContentBlock>,
    ) -> Result<u64, SteerError> {
        let running = self
            .state
            .sessions
            .get(session_id)
            .is_some_and(|session| session.running);
        if !running {
            return Err(SteerError::NotRunning);
        }
        if !self.state.steer_available() {
            return Err(SteerError::NotAdvertised);
        }
        let request_id = self.send_prompt(session_id, prompt, Some(PromptDelivery::Steer), false);
        Ok(request_id)
    }

    /// Removes the queued prompt at `index` of `session_id` before it
    /// ever went on the wire, reporting whether there was one.
    ///
    /// The ACP surface has no withdraw method: the engine-side prompt
    /// queue is only reachable through `session/prompt` deliveries, so
    /// a client that holds the queue, as this one does, withdraws by
    /// dropping the held prompt.
    pub fn withdraw_queued(&mut self, session_id: &str, index: usize) -> bool {
        let queue = &mut self.session_mut(session_id).queue;
        if index >= queue.len() {
            return false;
        }
        queue.remove(index);
        true
    }

    /// Sends the queued prompt at `index` of `session_id` as a steer
    /// on the run in flight, removing it from the queue. The same wire
    /// limits as [`Client::steer`] apply, and a rejected steer leaves
    /// the queue untouched.
    pub fn steer_queued(&mut self, session_id: &str, index: usize) -> Result<u64, SteerError> {
        let Some(prompt) = self
            .state
            .session(session_id)
            .and_then(|session| session.queue.get(index).map(|queued| queued.prompt.clone()))
        else {
            return Err(SteerError::NotRunning);
        };
        let sent = self.steer(session_id, prompt);
        if sent.is_ok() {
            self.withdraw_queued(session_id, index);
        }
        sent
    }

    /// Asks the agent to stop the run of `session_id`.
    pub fn cancel(&mut self, session_id: &str) {
        self.outgoing.push(Frame::Notification {
            method: "session/cancel".into(),
            params: params(&CancelNotification {
                session_id: session_id.to_owned(),
            }),
        });
    }

    /// Changes one config option of a session. The answer replaces the
    /// session's config options.
    pub fn set_config_option(&mut self, session_id: &str, config_id: &str, value: &str) -> u64 {
        self.request(
            "session/set_config_option",
            params(&SetSessionConfigOptionRequest {
                session_id: session_id.to_owned(),
                config_id: config_id.to_owned(),
                value: value.to_owned(),
            }),
            Pending::SetConfigOption {
                session_id: session_id.to_owned(),
            },
        )
    }

    /// Answers the ask `request_id` of `session_id`. The ask leaves
    /// the queue once answered, a decision record joins the session's
    /// transcript so a redraw shows what was chosen, and the answer
    /// goes out. A [`PermissionDecision::Feedback`] verdict rides the
    /// `_meta.kage.planReview` channel. Returns false when no such
    /// ask is open, in which case nothing is sent.
    pub fn reply_permission(
        &mut self,
        session_id: &str,
        request_id: u64,
        decision: &PermissionDecision,
    ) -> bool {
        let (outcome, meta) = {
            let Some(session) = self.state.sessions.get_mut(session_id) else {
                return false;
            };
            let Some(index) = session
                .permissions
                .iter()
                .position(|ask| ask.request_id == request_id)
            else {
                return false;
            };
            let ask = &session.permissions[index];
            let mut meta = None;
            let option_id = match decision {
                PermissionDecision::Cancel => None,
                PermissionDecision::Option(option_id) => Some(option_id.clone()),
                PermissionDecision::Allow => ask
                    .option_of(PermissionOptionKind::AllowOnce)
                    .map(str::to_owned),
                PermissionDecision::AllowAlways => ask
                    .option_of(PermissionOptionKind::AllowAlways)
                    .map(str::to_owned),
                PermissionDecision::Reject => ask
                    .option_of(PermissionOptionKind::RejectOnce)
                    .map(str::to_owned),
                PermissionDecision::Feedback {
                    option_id,
                    feedback,
                } => {
                    meta = Some(RequestMeta {
                        kage: KageMeta {
                            plan_review: Some(PlanReview {
                                revision: Some(feedback.clone()),
                                plan: None,
                            }),
                            ..KageMeta::default()
                        },
                    });
                    Some(option_id.clone())
                }
            };
            let outcome = if let Some(ref option_id) = option_id {
                PermissionOutcome::Selected(SelectedOption {
                    option_id: option_id.clone(),
                })
            } else {
                if *decision != PermissionDecision::Cancel {
                    return false;
                }
                PermissionOutcome::Cancelled
            };
            let record = decision_record(ask, decision, option_id.as_deref());
            session.permissions.remove(index);
            session.items.push(record);
            (outcome, meta)
        };
        self.outgoing.push(Frame::Success {
            id: request_id,
            result: params(&RequestPermissionResult { outcome, meta }),
        });
        true
    }

    /// Releases the session: the answer removes it from the state.
    pub fn close_session(&mut self, session_id: &str) -> u64 {
        self.request(
            "session/close",
            params(&CloseSessionRequest {
                session_id: session_id.to_owned(),
            }),
            Pending::Close {
                session_id: session_id.to_owned(),
            },
        )
    }

    /// Asks for the running process's read-only configuration. The
    /// answer arrives as [`Change::Config`].
    pub fn config_get(&mut self) -> u64 {
        self.request(
            "_kage/config/get",
            params(&ConfigGetRequest {}),
            Pending::ConfigGet,
        )
    }

    /// Lists or reads a path under the session workdir. The answer
    /// arrives as [`Change::Fs`].
    pub fn fs(&mut self, session_id: &str, op: FsOp, path: &str) -> u64 {
        self.request(
            "_kage/fs",
            params(&FsRequest {
                session_id: session_id.to_owned(),
                op,
                path: path.to_owned(),
            }),
            Pending::Fs {
                session_id: session_id.to_owned(),
            },
        )
    }

    fn request(&mut self, method: &str, params: Value, pending: Pending) -> u64 {
        let id = self.take_id();
        self.pending.insert(id, pending);
        self.outgoing.push(Frame::Request {
            id,
            method: method.to_owned(),
            params,
        });
        id
    }

    fn take_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn open(
        &mut self,
        method: &str,
        session_id: &str,
        cwd: &str,
        mcp_servers: &[McpServer],
    ) -> u64 {
        let request = if method == "session/load" {
            params(&LoadSessionRequest {
                session_id: session_id.to_owned(),
                cwd: cwd.to_owned(),
                mcp_servers: mcp_servers.to_vec(),
            })
        } else {
            params(&ResumeSessionRequest {
                session_id: session_id.to_owned(),
                cwd: cwd.to_owned(),
                mcp_servers: mcp_servers.to_vec(),
            })
        };
        // The replay that follows streams the whole history again, so
        // what it carries starts over; what only this client holds (the
        // draft, the held queue, the place in the forest) stays. A run
        // that was in flight answers on the old connection, which is
        // gone, so the session reads idle until the replay says
        // otherwise.
        let session = self.session_mut(session_id);
        let kept = Session {
            id: session_id.to_owned(),
            cwd: if cwd.is_empty() {
                session.cwd.take()
            } else {
                Some(cwd.to_owned())
            },
            title: session.title.take(),
            parent: session.parent.take(),
            draft: session.draft.take(),
            queue: std::mem::take(&mut session.queue),
            ..Session::default()
        };
        *session = kept;
        self.request(
            method,
            request,
            Pending::Open {
                session_id: session_id.to_owned(),
            },
        )
    }

    fn send_prompt(
        &mut self,
        session_id: &str,
        prompt: Vec<ContentBlock>,
        delivery: Option<PromptDelivery>,
        owns_run: bool,
    ) -> u64 {
        let id = self.take_id();
        self.pending.insert(
            id,
            Pending::Prompt {
                session_id: session_id.to_owned(),
                owns_run,
            },
        );
        let session = self.session_mut(session_id);
        session.running = true;
        // The agent echoes a prompt only to the other clients, so the
        // transcript gets its own copy here.
        session.items.push(TranscriptItem::User {
            content: prompt.clone(),
            steered: delivery == Some(PromptDelivery::Steer),
        });
        self.outgoing.push(Frame::Request {
            id,
            method: "session/prompt".into(),
            params: params(&PromptRequest {
                session_id: session_id.to_owned(),
                prompt,
                delivery,
            }),
        });
        id
    }

    /// Sends the oldest queued prompt of a run that just ended, if
    /// there is one.
    fn flush_queue(&mut self, session_id: &str) {
        if self.session_mut(session_id).queue.is_empty() {
            return;
        }
        let queued = self.session_mut(session_id).queue.remove(0);
        self.send_prompt(session_id, queued.prompt, None, true);
    }

    fn session_mut(&mut self, session_id: &str) -> &mut Session {
        self.state
            .sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| Session::new(session_id))
    }

    fn handle_server_request(&mut self, id: u64, method: &str, params: Value) -> Vec<Change> {
        if method != ASK_METHOD {
            self.outgoing.push(Frame::Failure {
                id,
                error: RpcError::method_not_found(method),
            });
            return Vec::new();
        }
        let Ok(request) = serde_json::from_value::<RequestPermissionRequest>(params) else {
            self.outgoing.push(Frame::Failure {
                id,
                error: RpcError::invalid_params(ASK_METHOD),
            });
            return Vec::new();
        };
        let session_id = request.session_id;
        let session = self.session_mut(&session_id);
        let plan = request
            .meta
            .and_then(|meta| meta.kage.plan_review)
            .and_then(|review| review.plan);
        let ask = PermissionAsk {
            request_id: id,
            tool_call: request.tool_call,
            options: request.options,
            plan,
        };
        // A re-attach makes the agent raise its open asks again, so a
        // frame whose id is already queued replaces its echo instead
        // of joining it: one ask per request id, in the latest offer's
        // shape, position kept.
        match session
            .permissions
            .iter()
            .position(|open| open.request_id == id)
        {
            Some(index) => session.permissions[index] = ask,
            None => session.permissions.push(ask),
        }
        vec![Change::Permission { id: session_id }]
    }

    fn handle_notification(&mut self, method: &str, params: Value) -> Vec<Change> {
        match method {
            "session/update" => match serde_json::from_value::<SessionNotification>(params) {
                Ok(note) => self.apply_update(&note.session_id, note.update),
                Err(_) => Vec::new(),
            },
            "$/cancel_request" => {
                let Some(request_id) = params.get("requestId").and_then(Value::as_u64) else {
                    return Vec::new();
                };
                self.withdraw_ask(request_id)
            }
            _ => Vec::new(),
        }
    }

    /// Removes the open ask `request_id`: it was answered elsewhere
    /// or the agent withdrew it through `$/cancel_request`. The ask
    /// leaves the queue with no reply of ours and no decision record;
    /// [`Change::AnsweredElsewhere`] is the host's cue to close the
    /// card.
    fn withdraw_ask(&mut self, request_id: u64) -> Vec<Change> {
        let mut moved = Vec::new();
        let sessions = self.state.sessions.keys().cloned().collect::<Vec<_>>();
        for session_id in sessions {
            let Some(session) = self.state.sessions.get_mut(&session_id) else {
                continue;
            };
            let Some(index) = session
                .permissions
                .iter()
                .position(|ask| ask.request_id == request_id)
            else {
                continue;
            };
            session.permissions.remove(index);
            moved.push(Change::Permission {
                id: session_id.clone(),
            });
            moved.push(Change::AnsweredElsewhere {
                id: session_id,
                request_id,
            });
        }
        moved
    }

    fn handle_success(&mut self, id: u64, result: Value) -> Vec<Change> {
        let Some(pending) = self.pending.remove(&id) else {
            return Vec::new();
        };
        match pending {
            Pending::Initialize => {
                match answer::<kage_acp_wire::InitializeResponse>(id, result, "initialize result") {
                    Err(failed) => failed,
                    Ok(response) => {
                        self.state.protocol_version = Some(response.protocol_version);
                        self.state.capabilities = Some(response.agent_capabilities);
                        self.state.agent = response.agent_info;
                        self.state.agent_cwd = response
                            .meta
                            .and_then(|meta| meta.kage)
                            .and_then(|kage| kage.cwd);
                        vec![Change::Connection]
                    }
                }
            }
            Pending::NewSession { cwd } => self.apply_new_session(id, cwd, result),
            Pending::Open { session_id } => {
                match answer::<kage_acp_wire::LoadSessionResponse>(
                    id,
                    result,
                    "session open result",
                ) {
                    Err(failed) => failed,
                    Ok(response) => {
                        let session = self.session_mut(&session_id);
                        session.opened = true;
                        session.config_options = response.config_options;
                        vec![Change::Session { id: session_id }]
                    }
                }
            }
            Pending::List { cwd } => match answer::<kage_acp_wire::ListSessionsResponse>(
                id,
                result,
                "session/list result",
            ) {
                Err(failed) => failed,
                Ok(page) => {
                    for info in page.sessions {
                        if let Some(listed) = self
                            .state
                            .directory
                            .iter_mut()
                            .find(|listed| listed.session_id == info.session_id)
                        {
                            *listed = info;
                        } else {
                            self.state.directory.push(info);
                        }
                    }
                    if let Some(cursor) = page.next_cursor {
                        self.list_sessions(cwd.as_deref(), Some(&cursor));
                    }
                    vec![Change::Directory]
                }
            },
            Pending::Prompt {
                session_id,
                owns_run,
            } => self.apply_prompt_answer(session_id, owns_run, result),
            Pending::SetConfigOption { session_id } => {
                match answer::<kage_acp_wire::SetSessionConfigOptionResponse>(
                    id,
                    result,
                    "session/set_config_option result",
                ) {
                    Err(failed) => failed,
                    Ok(response) => {
                        self.session_mut(&session_id).config_options = response.config_options;
                        vec![Change::Session { id: session_id }]
                    }
                }
            }
            Pending::Close { session_id } => {
                self.state.sessions.remove(&session_id);
                vec![Change::Session { id: session_id }]
            }
            Pending::ConfigGet => vec![Change::Config { config: result }],
            Pending::Fs { session_id } => {
                match answer::<kage_acp_wire::FsResult>(id, result, "_kage/fs result") {
                    Err(failed) => failed,
                    Ok(fs_result) => vec![Change::Fs {
                        session_id,
                        result: fs_result,
                    }],
                }
            }
        }
    }

    /// Records the session a `session/new` answer created.
    fn apply_new_session(&mut self, id: u64, cwd: String, result: Value) -> Vec<Change> {
        match answer::<kage_acp_wire::NewSessionResponse>(id, result, "session/new result") {
            Err(failed) => failed,
            Ok(response) => {
                let mut session = Session::new(&response.session_id);
                session.opened = true;
                // An empty cwd ran in the agent's own directory.
                session.cwd = if cwd.is_empty() {
                    self.state.agent_cwd.clone()
                } else {
                    Some(cwd)
                };
                session.config_options = response.config_options;
                let session_id = session.id.clone();
                self.state.sessions.insert(session_id.clone(), session);
                vec![Change::Session { id: session_id }]
            }
        }
    }

    /// Records a prompt answer and, when it ended the run, sends the
    /// oldest queued prompt.
    fn apply_prompt_answer(
        &mut self,
        session_id: String,
        owns_run: bool,
        result: Value,
    ) -> Vec<Change> {
        let stop = serde_json::from_value::<PromptResponse>(result)
            .ok()
            .map(|response| response.stop_reason);
        {
            let session = self.session_mut(&session_id);
            session.last_stop = stop;
            if owns_run {
                session.running = false;
            }
        }
        if owns_run {
            self.flush_queue(&session_id);
        }
        vec![Change::Session { id: session_id }]
    }

    fn handle_failure(&mut self, id: u64, error: RpcError) -> Vec<Change> {
        let Some(pending) = self.pending.remove(&id) else {
            return Vec::new();
        };
        if let Pending::Prompt {
            session_id,
            owns_run: true,
        } = pending
        {
            self.session_mut(&session_id).running = false;
            self.flush_queue(&session_id);
        }
        vec![Change::Failed { request: id, error }]
    }

    fn apply_update(&mut self, session_id: &str, update: SessionUpdate) -> Vec<Change> {
        let transcript = || Change::Transcript {
            id: session_id.into(),
        };
        match update {
            SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::AgentMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::ToolCall(_)
            | SessionUpdate::ToolCallUpdate(_)
            | SessionUpdate::Plan(_)
            | SessionUpdate::Notice(_)
            | SessionUpdate::Compaction(_) => {
                let session = self.session_mut(session_id);
                apply_item(session, update)
                    .then(transcript)
                    .into_iter()
                    .collect()
            }
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.session_mut(session_id).commands = update.available_commands;
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::CurrentModeUpdate(update) => {
                self.session_mut(session_id).mode = Some(update.current_mode_id);
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::UsageUpdate(update) => {
                self.session_mut(session_id).usage = Usage {
                    used: update.used,
                    size: update.size,
                    cost: update.cost,
                };
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::SessionInfoUpdate(update) => {
                let session = self.session_mut(session_id);
                if update.title.is_some() {
                    session.title = update.title;
                }
                if update.updated_at.is_some() {
                    session.updated_at = update.updated_at;
                }
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                self.session_mut(session_id).config_options = update.config_options;
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::SubagentUpdate(update) => {
                let child = update.subagent_session_id.clone();
                let session = self.session_mut(session_id);
                let agent = session.agents.entry(child.clone()).or_default();
                agent.merge(&update);
                self.session_mut(&child).parent = Some(session_id.to_owned());
                vec![Change::Agents {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::Turn(turn) => match turn.phase {
                kage_acp_wire::TurnPhase::Start => {
                    self.session_mut(session_id).in_turn = true;
                    vec![Change::Session {
                        id: session_id.into(),
                    }]
                }
                kage_acp_wire::TurnPhase::End => {
                    let session = self.session_mut(session_id);
                    session.in_turn = false;
                    session.items.push(TranscriptItem::TurnEnd {
                        reason: turn.reason,
                    });
                    vec![
                        Change::Session {
                            id: session_id.into(),
                        },
                        transcript(),
                    ]
                }
            },
            SessionUpdate::McpStatus(status) => {
                self.session_mut(session_id)
                    .mcp
                    .insert(status.name, status.status);
                vec![Change::Session {
                    id: session_id.into(),
                }]
            }
            SessionUpdate::Unknown => Vec::new(),
        }
    }
}

/// Applies the updates that land in the transcript, reporting whether
/// anything changed. An update for a tool call the transcript never
/// announced changes nothing.
fn apply_item(session: &mut Session, update: SessionUpdate) -> bool {
    match update {
        SessionUpdate::UserMessageChunk(chunk) => {
            // A prompt arrives as one chunk per block, text first, and
            // the wire names no message: a text chunk starts a prompt,
            // and an image or resource chunk belongs to the prompt
            // before it.
            match session.items.last_mut() {
                Some(TranscriptItem::User { content, .. }) if chunk.content.as_text().is_none() => {
                    content.push(chunk.content);
                }
                _ => session.items.push(TranscriptItem::User {
                    content: vec![chunk.content],
                    steered: false,
                }),
            }
            true
        }
        SessionUpdate::AgentMessageChunk(chunk) => {
            session.append_chunk(&chunk.content, true);
            true
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            session.append_chunk(&chunk.content, false);
            true
        }
        SessionUpdate::ToolCall(call) => {
            session.items.push(TranscriptItem::ToolCall(ToolCallItem {
                tool_call_id: call.tool_call_id,
                title: call.title,
                kind: call.kind,
                status: call.status,
                input: call.raw_input,
                swarm: call.meta.and_then(|meta| meta.kage.swarm),
                content: call.content,
                raw_output: None,
            }));
            true
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let call_id = update.tool_call_id.clone();
            session
                .items
                .iter_mut()
                .rev()
                .find_map(|item| match item {
                    TranscriptItem::ToolCall(call) if call.tool_call_id == call_id => Some(call),
                    _ => None,
                })
                .is_some_and(|call| {
                    call.merge(&update);
                    true
                })
        }
        SessionUpdate::Plan(plan) => {
            session.plan = Some(plan.entries.clone());
            match session.items.last_mut() {
                Some(TranscriptItem::Plan { entries }) => *entries = plan.entries,
                _ => session.items.push(TranscriptItem::Plan {
                    entries: plan.entries,
                }),
            }
            true
        }
        SessionUpdate::Notice(notice) => {
            session.items.push(TranscriptItem::Notice {
                tone: notice.tone,
                text: notice.text,
            });
            true
        }
        SessionUpdate::Compaction(compaction) => {
            session.items.push(TranscriptItem::Compaction {
                kept: compaction.kept,
                before: compaction.before,
                after: compaction.after,
            });
            true
        }
        _ => false,
    }
}

/// The transcript record of a decision: the subject the ask named,
/// the chosen option's label as offered, whether it lets the call
/// proceed, and any feedback text. An answer without a chosen option
/// records `cancelled`. A label for an option id the ask never
/// offered is the id itself, never a made-up name.
fn decision_record(
    ask: &PermissionAsk,
    decision: &PermissionDecision,
    option_id: Option<&str>,
) -> TranscriptItem {
    let subject = ask.subject();
    let chosen = option_id.and_then(|option_id| {
        ask.options
            .iter()
            .find(|option| option.option_id == option_id)
    });
    let label = match chosen {
        Some(option) => option.name.clone(),
        None => option_id.map_or_else(|| "cancelled".to_owned(), str::to_owned),
    };
    let allowed = chosen.is_some_and(|option| {
        matches!(
            option.kind,
            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
        )
    });
    let feedback = match decision {
        PermissionDecision::Feedback { feedback, .. } => Some(feedback.clone()),
        _ => None,
    };
    TranscriptItem::Decision {
        subject,
        label,
        allowed,
        feedback,
    }
}

/// Decodes a request answer, or the change that reports it undecodable.
fn answer<T: serde::de::DeserializeOwned>(
    id: u64,
    result: Value,
    what: &str,
) -> Result<T, Vec<Change>> {
    serde_json::from_value(result).map_err(|_| {
        vec![Change::Failed {
            request: id,
            error: RpcError::invalid_params(what),
        }]
    })
}

/// The parameters of an outgoing frame. Every wire type serializes
/// infallibly, so a failure here is a bug in the schema, not in a
/// caller's input.
fn params<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("wire type serializes")
}
