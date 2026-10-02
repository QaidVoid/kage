//! ACP agent server.
//!
//! Drives an injected [`Agent`] over the [`kage_jsonrpc`] peer, conformant
//! with the published ACP spec: it answers `initialize`, `session/new`,
//! `session/load`, `session/list`, `session/resume`,
//! `session/set_config_option`, `session/prompt` and `session/close`,
//! forwards the `session/cancel`
//! notification, and lets the agent stream `session/update`
//! notifications and issue `session/request_permission` requests. A
//! request the agent abandons (a permission ask outlived by its run) is
//! withdrawn with `$/cancel_request`, so the client can close its dialog.
//!
//! Every request except `initialize` runs on its own thread, so the
//! dispatch loop keeps draining inbound messages: a `session/cancel`
//! always lands, and a slow `session/new` never blocks a running prompt.
//! When the input ends, every request but a prompt that is still in
//! flight is answered before [`serve_agent`] returns.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::thread;

use kage_core::CancelFlag;
use kage_jsonrpc::{CancelNotice, Inbound, Peer, RpcError, connect_with};

use crate::acp::{
    AuthSetRequest, CloseSessionRequest, CloseSessionResponse, ConfigGetRequest, ConfigGetResult,
    ConfigSetRequest, ConfigTestRequest, ConfigTestResult, DirectoryRequest, DirectoryResult,
    FoldersRequest, FoldersResult, FsRequest, FsResult, InitializeRequest, InitializeResponse,
    KageMeta, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    ModelsResponse, NewSessionRequest, NewSessionResponse, OptionSetRequest, OptionsResponse,
    PermissionOption, PermissionOptionKind, PermissionOutcome, PlanReview, PluginInstallRequest,
    PluginRemoveRequest, PromptRequest, PromptResponse, QuestionMeta, QuestionPrompt, RequestMeta,
    RequestPermissionRequest, RequestPermissionResponse, RequestPermissionResult,
    ResumeSessionRequest, ResumeSessionResponse, SessionExportResponse, SessionForkRequest,
    SessionForkResponse, SessionNotification, SessionRenameRequest, SessionRequest, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SwarmResumeRequest,
    SwarmResumeResponse, ToolCallUpdate,
};

/// The client's answer to a `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Run the tool call.
    Allow,
    /// Run the tool call and allow the tool for the rest of the session.
    AllowSession,
    /// Block it, with an optional reason for the model.
    Deny(Option<String>),
    /// The ask was withdrawn or the connection closed before an answer
    /// arrived. No decision is sent, so the request the ask came from
    /// stays open for another client to answer.
    Unanswered,
}

/// Ask the client to allow `tool_call` once or for the session, or to
/// deny it. Blocks until the client answers, `cancel` withdraws the ask,
/// or the connection closes. A rejection and a decode failure resolve to
/// [`PermissionDecision::Deny`]. A withdrawn ask and a closed connection
/// resolve to [`PermissionDecision::Unanswered`], which sends no
/// decision. Never auto-approves.
#[must_use]
pub fn request_permission(
    peer: &Peer,
    session_id: &str,
    tool_call: ToolCallUpdate,
    title: &str,
    cancel: &CancelFlag,
) -> PermissionDecision {
    let req = RequestPermissionRequest {
        session_id: session_id.to_owned(),
        tool_call,
        options: vec![
            PermissionOption {
                option_id: "allow".to_owned(),
                name: format!("Allow {title}"),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionOption {
                option_id: "allow_session".to_owned(),
                name: format!("Allow {title} for this session"),
                kind: PermissionOptionKind::AllowAlways,
            },
            PermissionOption {
                option_id: "reject".to_owned(),
                name: format!("Reject {title}"),
                kind: PermissionOptionKind::RejectOnce,
            },
        ],
        meta: None,
    };
    let Ok(params) = serde_json::to_value(&req) else {
        return PermissionDecision::Deny(Some("encode permission request".to_owned()));
    };
    match peer.request_cancellable("session/request_permission", params, cancel) {
        Ok(value) => match serde_json::from_value::<RequestPermissionResponse>(value) {
            Ok(resp) => match resp.outcome {
                PermissionOutcome::Selected(sel) => match sel.option_id.as_str() {
                    "allow" => PermissionDecision::Allow,
                    "allow_session" => PermissionDecision::AllowSession,
                    _ => PermissionDecision::Deny(Some("rejected by client".to_owned())),
                },
                PermissionOutcome::Cancelled => {
                    PermissionDecision::Deny(Some("cancelled".to_owned()))
                }
            },
            Err(e) => PermissionDecision::Deny(Some(format!("decode outcome: {e}"))),
        },
        Err(e) if e.code == -32800 || e.message == "connection closed" => {
            PermissionDecision::Unanswered
        }
        Err(e) => PermissionDecision::Deny(Some(e.message)),
    }
}

/// The client's answer to a plan-mode review raised through
/// [`request_plan_review`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanReviewDecision {
    /// The plan was approved: run the `exit_plan` call, which ends plan
    /// mode and resumes the run.
    Approve,
    /// The user asked for changes, in their own words. The `exit_plan`
    /// call is denied, which ends the run with plan mode still on, and
    /// the text is delivered to the model as its next prompt.
    Revise(String),
    /// The plan was rejected: deny the call, ending the run like any
    /// other denial.
    Reject,
    /// The ask was withdrawn or the connection closed before an answer
    /// arrived. No decision is sent.
    Unanswered,
}

/// Ask the client to review `plan`, presented by the plan-mode
/// `exit_plan` tool as `tool_call`. Offers exactly approve, revise and
/// reject, with the document under `_meta.kage.planReview`; a revise
/// answer brings the user's text back in the same field. Blocks until
/// the client answers, `cancel` withdraws the ask, or the connection
/// closes. Any unexpected answer, rejection or decode failure resolves
/// to [`PlanReviewDecision::Reject`]; a withdrawn ask and a closed
/// connection resolve to [`PlanReviewDecision::Unanswered`]. Never
/// auto-approves.
#[must_use]
pub fn request_plan_review(
    peer: &Peer,
    session_id: &str,
    tool_call: ToolCallUpdate,
    plan: &str,
    cancel: &CancelFlag,
) -> PlanReviewDecision {
    let req = RequestPermissionRequest {
        session_id: session_id.to_owned(),
        tool_call,
        options: vec![
            PermissionOption {
                option_id: "approve".to_owned(),
                name: "Approve".to_owned(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionOption {
                option_id: "revise".to_owned(),
                name: "Revise".to_owned(),
                kind: PermissionOptionKind::RejectOnce,
            },
            PermissionOption {
                option_id: "reject".to_owned(),
                name: "Reject".to_owned(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ],
        meta: Some(RequestMeta {
            kage: KageMeta {
                plan_review: Some(PlanReview {
                    plan: Some(plan.to_owned()),
                    revision: None,
                }),
                ..KageMeta::default()
            },
        }),
    };
    let Ok(params) = serde_json::to_value(&req) else {
        return PlanReviewDecision::Reject;
    };
    match peer.request_cancellable("session/request_permission", params, cancel) {
        Ok(value) => match serde_json::from_value::<RequestPermissionResult>(value) {
            Ok(resp) => match resp.outcome {
                PermissionOutcome::Selected(sel) => match sel.option_id.as_str() {
                    "approve" => PlanReviewDecision::Approve,
                    "revise" => PlanReviewDecision::Revise(
                        resp.meta
                            .and_then(|meta| meta.kage.plan_review)
                            .and_then(|review| review.revision)
                            .unwrap_or_default(),
                    ),
                    _ => PlanReviewDecision::Reject,
                },
                PermissionOutcome::Cancelled => PlanReviewDecision::Reject,
            },
            Err(_) => PlanReviewDecision::Reject,
        },
        Err(e) if e.code == -32800 || e.message == "connection closed" => {
            PlanReviewDecision::Unanswered
        }
        Err(_) => PlanReviewDecision::Reject,
    }
}

/// How the user answered the questions of an `ask_user_question` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionsAnswer {
    /// Per question, the labels picked or the user's own words.
    Answered(Vec<Vec<String>>),
    /// The user skipped a question, which declines them all.
    Declined,
    /// The ask was withdrawn or the connection closed before an answer
    /// arrived. No answer is sent.
    Unanswered,
}

/// Ask the client `questions` of an `ask_user_question` call, shown as
/// `tool_call`, one `session/request_permission` at a time. Each offers
/// option `choice-<n>` per choice and `skip`, so any client can pick
/// one choice. The question rides `_meta.kage.question.prompt`, and a
/// kage client may answer with several choices or the user's own words
/// in `_meta.kage.question.answer`. Skipping or cancelling any question
/// declines them all.
#[must_use]
pub fn request_questions(
    peer: &Peer,
    session_id: &str,
    tool_call: &ToolCallUpdate,
    questions: &[QuestionPrompt],
    cancel: &CancelFlag,
) -> QuestionsAnswer {
    let mut answers = Vec::with_capacity(questions.len());
    for question in questions {
        let mut options: Vec<PermissionOption> = question
            .options
            .iter()
            .enumerate()
            .map(|(n, choice)| PermissionOption {
                option_id: format!("choice-{n}"),
                name: choice.label.clone(),
                kind: PermissionOptionKind::AllowOnce,
            })
            .collect();
        options.push(PermissionOption {
            option_id: "skip".to_owned(),
            name: "Skip".to_owned(),
            kind: PermissionOptionKind::RejectOnce,
        });
        let req = RequestPermissionRequest {
            session_id: session_id.to_owned(),
            tool_call: ToolCallUpdate {
                title: Some(format!("{}: {}", question.header, question.question)),
                ..tool_call.clone()
            },
            options,
            meta: Some(RequestMeta {
                kage: KageMeta {
                    question: Some(Box::new(QuestionMeta {
                        prompt: Some(question.clone()),
                        answer: None,
                    })),
                    ..KageMeta::default()
                },
            }),
        };
        let Ok(params) = serde_json::to_value(&req) else {
            return QuestionsAnswer::Declined;
        };
        let resp = match peer.request_cancellable("session/request_permission", params, cancel) {
            Ok(value) => serde_json::from_value::<RequestPermissionResult>(value),
            Err(e) if e.code == -32800 || e.message == "connection closed" => {
                return QuestionsAnswer::Unanswered;
            }
            Err(_) => return QuestionsAnswer::Declined,
        };
        let Ok(resp) = resp else {
            return QuestionsAnswer::Declined;
        };
        let PermissionOutcome::Selected(selected) = resp.outcome else {
            return QuestionsAnswer::Declined;
        };
        let written = resp
            .meta
            .and_then(|meta| meta.kage.question)
            .and_then(|meta| meta.answer);
        let picked = selected
            .option_id
            .strip_prefix("choice-")
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| question.options.get(n))
            .map(|choice| vec![choice.label.clone()]);
        match written.or(picked) {
            Some(answer) => answers.push(answer),
            None => return QuestionsAnswer::Declined,
        }
    }
    QuestionsAnswer::Answered(answers)
}

/// Handed to [`Agent::prompt`] and [`Agent::load_session`]: streams
/// updates for one session.
pub struct PromptContext {
    peer: Peer,
    session_id: String,
}

impl PromptContext {
    /// Emit a `session/update` notification for this session.
    pub fn update(&self, update: SessionUpdate) {
        send_update(&self.peer, &self.session_id, update);
    }

    /// The underlying peer, for agent-initiated requests.
    #[must_use]
    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    /// The session this call belongs to.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// Emit a `session/update` notification for `session_id` on `peer`.
/// The update is converted into the wire union, its serde form.
pub fn send_update(peer: &Peer, session_id: &str, update: SessionUpdate) {
    let note = SessionNotification {
        session_id: session_id.to_owned(),
        update: update.into(),
    };
    if let Ok(params) = serde_json::to_value(&note) {
        let _ = peer.notify("session/update", params);
    }
}

/// The host-supplied agent the server drives. The server owns the
/// protocol; this trait owns the agent. Methods take `&self` and may run
/// concurrently on different sessions.
pub trait Agent: Send + Sync + 'static {
    /// Handshake. Return the agent's capabilities and identity.
    fn initialize(&self, req: InitializeRequest) -> InitializeResponse;

    /// Create a session for `cwd`.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if a session cannot be created.
    fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError>;

    /// Resume a previously recorded session, streaming its history
    /// back through `ctx` as `session/update` notifications. The
    /// default rejects: only agents that advertise
    /// `agentCapabilities.loadSession` override it.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session id is unknown or its
    /// history cannot be replayed.
    fn load_session(
        &self,
        _req: LoadSessionRequest,
        _ctx: &PromptContext,
    ) -> Result<LoadSessionResponse, RpcError> {
        Err(RpcError::method_not_found("session/load"))
    }

    /// List recorded sessions, one page at a time. The default rejects:
    /// only agents that advertise `sessionCapabilities.list` override
    /// it.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the sessions cannot be listed.
    fn list_sessions(&self, _req: ListSessionsRequest) -> Result<ListSessionsResponse, RpcError> {
        Err(RpcError::method_not_found("session/list"))
    }

    /// Reopen a recorded session without replaying its history. `ctx`
    /// streams updates for the session, such as the turn in flight
    /// when the agent still hosts it. The default rejects: only agents
    /// that advertise `sessionCapabilities.resume` override it.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session id is unknown or cannot
    /// be reopened.
    fn resume_session(
        &self,
        _req: ResumeSessionRequest,
        _ctx: &PromptContext,
    ) -> Result<ResumeSessionResponse, RpcError> {
        Err(RpcError::method_not_found("session/resume"))
    }

    /// Release a session the connection loaded, created or resumed: it
    /// no longer watches it, and the agent may stop hosting it once no
    /// other connection does. The default rejects: only agents that
    /// advertise `sessionCapabilities.close` override it.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session id is unknown or cannot
    /// be released.
    fn close_session(&self, _req: CloseSessionRequest) -> Result<CloseSessionResponse, RpcError> {
        Err(RpcError::method_not_found("session/close"))
    }

    /// Change one config option of a session. The default rejects:
    /// only agents that return config options override it.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session, option or value is
    /// unknown.
    fn set_config_option(
        &self,
        _req: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, RpcError> {
        Err(RpcError::method_not_found("session/set_config_option"))
    }

    /// The read-only configuration sections (`_kage/config/get`). The
    /// default rejects: only agents that serve the host config answer.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the config cannot be read.
    fn config_get(&self, _req: ConfigGetRequest) -> Result<ConfigGetResult, RpcError> {
        Err(RpcError::method_not_found("_kage/config/get"))
    }

    /// The confined file operations (`_kage/fs`): list and read under
    /// the session workdir. The default rejects: only agents with a
    /// session workdir answer.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown or a path
    /// escapes the workdir.
    fn fs(&self, _req: FsRequest) -> Result<FsResult, RpcError> {
        Err(RpcError::method_not_found("_kage/fs"))
    }

    /// Continue children of an earlier `swarm` call of a session
    /// (`_kage/swarm/resume`). The default rejects: only agents with a
    /// swarm engine answer.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown or a member is
    /// not a session id.
    fn swarm_resume(&self, _req: SwarmResumeRequest) -> Result<SwarmResumeResponse, RpcError> {
        Err(RpcError::method_not_found("_kage/swarm/resume"))
    }

    /// Copy a recorded session, whole or up to a prompt
    /// (`_kage/session/fork`). The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session or the prompt is unknown,
    /// or the copy fails.
    fn session_fork(&self, _req: SessionForkRequest) -> Result<SessionForkResponse, RpcError> {
        Err(RpcError::method_not_found("_kage/session/fork"))
    }

    /// Render a recorded session as Markdown (`_kage/session/export`).
    /// The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown or unreadable.
    fn session_export(&self, _req: SessionRequest) -> Result<SessionExportResponse, RpcError> {
        Err(RpcError::method_not_found("_kage/session/export"))
    }

    /// Summarize a session's older turns now (`_kage/session/compact`).
    /// The compaction reaches the client as its update. The default
    /// rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown.
    fn session_compact(&self, _req: SessionRequest) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::method_not_found("_kage/session/compact"))
    }

    /// Name a session (`_kage/session/rename`). The new title reaches
    /// every client as a `session_info_update`. The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown.
    fn session_rename(&self, _req: SessionRenameRequest) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::method_not_found("_kage/session/rename"))
    }

    /// Every model the engine can run now, with what the catalog knows
    /// of each (`_kage/models/list`). The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the agent offers no catalog.
    fn models_list(&self) -> Result<ModelsResponse, RpcError> {
        Err(RpcError::method_not_found("_kage/models/list"))
    }

    /// The engine options a client can change (`_kage/options/list`),
    /// read for the request's session when it names one. The default
    /// rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the configuration cannot be read.
    fn options_list(&self, _req: ConfigGetRequest) -> Result<OptionsResponse, RpcError> {
        Err(RpcError::method_not_found("_kage/options/list"))
    }

    /// Validates and stores one engine option in the user config
    /// (`_kage/options/set`), answering with every option after the
    /// write. The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the option is unknown, the value does
    /// not fit it, or the config cannot be written.
    fn option_set(&self, _req: OptionSetRequest) -> Result<OptionsResponse, RpcError> {
        Err(RpcError::method_not_found("_kage/options/set"))
    }

    /// Replaces or removes one entry of the user config
    /// (`_kage/config/set`), answering with the snapshot after the
    /// write. The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the entry is outside the settable
    /// sections, the edited config does not load or validate, or the
    /// config cannot be written.
    fn config_set(&self, _req: ConfigSetRequest) -> Result<ConfigGetResult, RpcError> {
        Err(RpcError::method_not_found("_kage/config/set"))
    }

    /// Asks a provider for its model list (`_kage/config/test`), as
    /// saved or as a form holds it, so a client can test a connection
    /// and fill a model table without a socket of its own. The default
    /// rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the request names no provider. An
    /// unreachable or refusing endpoint is a result, not an error.
    fn config_test(&self, _req: ConfigTestRequest) -> Result<ConfigTestResult, RpcError> {
        Err(RpcError::method_not_found("_kage/config/test"))
    }

    /// Saves or removes a provider's API key in the credential store
    /// (`_kage/auth/set`), never in `config.toml`. The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the store cannot be written.
    fn auth_set(&self, _req: AuthSetRequest) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::method_not_found("_kage/auth/set"))
    }

    /// Lists the providers of a directory in the models.dev `api.json`
    /// shape (`_kage/providers/directory`), fetched by the agent. The
    /// default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the directory cannot be fetched or
    /// read.
    fn providers_directory(&self, _req: DirectoryRequest) -> Result<DirectoryResult, RpcError> {
        Err(RpcError::method_not_found("_kage/providers/directory"))
    }

    /// Lists the folders inside one folder on the machine the agent runs
    /// on (`_kage/folders`), for a client choosing where a session
    /// opens. The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the folder cannot be read.
    fn folders(&self, _req: FoldersRequest) -> Result<FoldersResult, RpcError> {
        Err(RpcError::method_not_found("_kage/folders"))
    }

    /// Installs a plugin file into the plugin directory
    /// (`_kage/plugins/install`). The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the source cannot be read, is not a
    /// Lua plugin, or the plugin directory cannot be written.
    fn plugin_install(&self, _req: PluginInstallRequest) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::method_not_found("_kage/plugins/install"))
    }

    /// Removes a plugin file from the plugin directory
    /// (`_kage/plugins/remove`). The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if no such plugin is installed or the
    /// file cannot be removed.
    fn plugin_remove(&self, _req: PluginRemoveRequest) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::method_not_found("_kage/plugins/remove"))
    }

    /// Run one prompt turn to completion, streaming `session/update`
    /// notifications through `ctx`.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the turn cannot start or fails
    /// irrecoverably (streamed progress already reached the client).
    fn prompt(&self, req: PromptRequest, ctx: &PromptContext) -> Result<PromptResponse, RpcError>;

    /// The client asked to cancel the running prompt of `session_id`.
    fn cancel(&self, session_id: &str);

    /// The response naming `session_id` to a `session/new`,
    /// `session/load` or `session/resume` was written, so updates for
    /// the session sent from now on cannot overtake it. The default does
    /// nothing.
    fn session_announced(&self, _session_id: &str) {}

    /// The connection ended and every request but a prompt that is
    /// still running is answered. The default does nothing.
    fn detached(&self) {}
}

/// The `$/cancel_request` notice ACP expects for an abandoned request.
fn cancel_notice() -> CancelNotice {
    Arc::new(|id, _method| {
        Some((
            "$/cancel_request".to_owned(),
            serde_json::json!({"requestId": id}),
        ))
    })
}

fn parse<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))
}

/// Serve the ACP agent protocol over `reader`/`writer` until the peer
/// disconnects. `make_agent` receives the peer so the agent can send
/// notifications and requests outside a prompt call.
///
/// # Errors
///
/// Returns an [`RpcError`] only for a fatal transport failure; a
/// clean disconnect is `Ok(())`.
pub fn serve_agent<R, W, A, F>(reader: R, writer: W, make_agent: F) -> Result<(), RpcError>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
    A: Agent,
    F: FnOnce(Peer) -> A,
{
    let (peer, inbound, _reader) = connect_with(reader, writer, Some(cancel_notice()));
    let agent = Arc::new(make_agent(peer.clone()));

    let mut ops = Vec::new();
    for message in inbound {
        match message {
            Inbound::Notification { method, params } => {
                if method == "session/cancel"
                    && let Ok(c) = parse::<crate::acp::CancelNotification>(params)
                {
                    agent.cancel(&c.session_id);
                }
            }
            Inbound::Request { id, method, params } => {
                if let Some(op) = handle_request(&peer, &agent, id, &method, params) {
                    ops.retain(|op: &thread::JoinHandle<()>| !op.is_finished());
                    ops.push(op);
                }
            }
        }
    }
    for op in ops {
        let _ = op.join();
    }
    agent.detached();
    Ok(())
}

fn jval<T: serde::Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// Answer request `id` with `op`, run on its own thread.
fn spawn_op<A, F>(
    peer: &Peer,
    agent: &Arc<A>,
    id: serde_json::Value,
    op: F,
) -> thread::JoinHandle<()>
where
    A: Agent,
    F: FnOnce(&A) -> Result<serde_json::Value, RpcError> + Send + 'static,
{
    let agent = Arc::clone(agent);
    let peer = peer.clone();
    thread::spawn(move || {
        let outcome = op(&agent);
        let _ = peer.respond(&id, outcome);
    })
}

/// Answer request `id` with `op`, which opens the session it returns
/// the id of, on its own thread, then tell the agent the client has the
/// response.
fn spawn_open<A, F>(
    peer: &Peer,
    agent: &Arc<A>,
    id: serde_json::Value,
    op: F,
) -> thread::JoinHandle<()>
where
    A: Agent,
    F: FnOnce(&A) -> Result<(String, serde_json::Value), RpcError> + Send + 'static,
{
    let agent = Arc::clone(agent);
    let peer = peer.clone();
    thread::spawn(move || match op(&agent) {
        Ok((session, response)) => {
            let _ = peer.respond(&id, Ok(response));
            agent.session_announced(&session);
        }
        Err(e) => {
            let _ = peer.respond(&id, Err(e));
        }
    })
}

/// Answers `id` with the params parse error `e`.
fn parse_failed(
    peer: &Peer,
    id: &serde_json::Value,
    e: RpcError,
) -> Option<thread::JoinHandle<()>> {
    let _ = peer.respond(id, Err(e));
    None
}

/// Answer one request, on its own thread for all but `initialize`.
/// Returns the thread to wait for at the end of the input, which is
/// every one but a prompt's.
fn handle_request<A: Agent>(
    peer: &Peer,
    agent: &Arc<A>,
    id: serde_json::Value,
    method: &str,
    params: serde_json::Value,
) -> Option<thread::JoinHandle<()>> {
    let op = match method {
        "initialize" => {
            let outcome = parse::<InitializeRequest>(params).map(|req| jval(agent.initialize(req)));
            let _ = peer.respond(&id, outcome);
            return None;
        }
        "session/new" => match parse::<NewSessionRequest>(params) {
            Err(e) => return parse_failed(peer, &id, e),
            Ok(req) => spawn_open(peer, agent, id, move |a| {
                a.new_session(req)
                    .map(|resp| (resp.session_id.clone(), jval(resp)))
            }),
        },
        "session/prompt" => match parse::<PromptRequest>(params) {
            Err(e) => return parse_failed(peer, &id, e),
            Ok(req) => {
                let ctx = PromptContext {
                    peer: peer.clone(),
                    session_id: req.session_id.clone(),
                };
                spawn_op(peer, agent, id, move |a| a.prompt(req, &ctx).map(jval));
                return None;
            }
        },
        "session/load" => match parse::<LoadSessionRequest>(params) {
            Err(e) => return parse_failed(peer, &id, e),
            Ok(req) => {
                let ctx = PromptContext {
                    peer: peer.clone(),
                    session_id: req.session_id.clone(),
                };
                spawn_open(peer, agent, id, move |a| {
                    let session = req.session_id.clone();
                    a.load_session(req, &ctx).map(|resp| (session, jval(resp)))
                })
            }
        },
        "session/list" => match parse::<ListSessionsRequest>(params) {
            Err(e) => {
                let _ = peer.respond(&id, Err(e));
                return None;
            }
            Ok(req) => spawn_op(peer, agent, id, move |a| a.list_sessions(req).map(jval)),
        },
        "session/resume" => match parse::<ResumeSessionRequest>(params) {
            Err(e) => return parse_failed(peer, &id, e),
            Ok(req) => {
                let ctx = PromptContext {
                    peer: peer.clone(),
                    session_id: req.session_id.clone(),
                };
                spawn_open(peer, agent, id, move |a| {
                    let session = req.session_id.clone();
                    a.resume_session(req, &ctx)
                        .map(|resp| (session, jval(resp)))
                })
            }
        },
        "session/set_config_option" => match parse::<SetSessionConfigOptionRequest>(params) {
            Err(e) => return parse_failed(peer, &id, e),
            Ok(req) => spawn_op(peer, agent, id, move |a| a.set_config_option(req).map(jval)),
        },
        "session/close" => match parse::<CloseSessionRequest>(params) {
            Err(e) => return parse_failed(peer, &id, e),
            Ok(req) => spawn_op(peer, agent, id, move |a| a.close_session(req).map(jval)),
        },
        other if other.starts_with("_kage/") => {
            return handle_kage_request(peer, agent, id, other, params);
        }
        other => {
            let _ = peer.respond(&id, Err(RpcError::method_not_found(other)));
            return None;
        }
    };
    Some(op)
}

/// Answer one `_kage/*` extension request on its own thread. Returns
/// the thread to wait for at the end of the input.
fn handle_kage_request<A: Agent>(
    peer: &Peer,
    agent: &Arc<A>,
    id: serde_json::Value,
    method: &str,
    params: serde_json::Value,
) -> Option<thread::JoinHandle<()>> {
    let op = match method {
        "_kage/config/get" => match parse::<ConfigGetRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.config_get(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/fs" => match parse::<FsRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.fs(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/swarm/resume" => match parse::<SwarmResumeRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.swarm_resume(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/session/fork" => match parse::<SessionForkRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.session_fork(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/session/export" => match parse::<SessionRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.session_export(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/session/compact" => match parse::<SessionRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.session_compact(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/options/list" => match parse::<ConfigGetRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.options_list(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/models/list" => spawn_op(peer, agent, id, |a| a.models_list().map(jval)),
        "_kage/options/set" => match parse::<OptionSetRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.option_set(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/config/set" => match parse::<ConfigSetRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.config_set(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/config/test" => match parse::<ConfigTestRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.config_test(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/auth/set" => match parse::<AuthSetRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.auth_set(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/folders" => match parse::<FoldersRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.folders(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/providers/directory" => match parse::<DirectoryRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| {
                a.providers_directory(req).map(jval)
            }),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/plugins/install" => match parse::<PluginInstallRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.plugin_install(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/plugins/remove" => match parse::<PluginRemoveRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.plugin_remove(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/session/rename" => match parse::<SessionRenameRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.session_rename(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        other => {
            let _ = peer.respond(&id, Err(RpcError::method_not_found(other)));
            return None;
        }
    };
    Some(op)
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use super::*;
    use kage_jsonrpc::connect;

    use crate::acp::{
        AgentCapabilities, ContentBlock, Implementation, MessageChunk, PromptCapabilities,
        SessionInfo, StopReason,
    };

    struct MockAgent;

    impl Agent for MockAgent {
        fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
            assert_eq!(req.protocol_version, crate::acp::PROTOCOL_VERSION);
            InitializeResponse {
                protocol_version: crate::acp::PROTOCOL_VERSION,
                agent_capabilities: AgentCapabilities {
                    load_session: false,
                    prompt_capabilities: PromptCapabilities::default(),
                    ..AgentCapabilities::default()
                },
                agent_info: Some(Implementation {
                    name: "mock".into(),
                    title: None,
                    version: None,
                }),
                auth_methods: vec![],
                meta: None,
            }
        }

        fn new_session(&self, _req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            Ok(NewSessionResponse {
                session_id: "sess-1".into(),
                config_options: vec![],
            })
        }

        fn prompt(
            &self,
            req: PromptRequest,
            ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            assert_eq!(ctx.session_id(), "sess-1");
            let echoed = req
                .prompt
                .first()
                .and_then(ContentBlock::as_text)
                .unwrap_or_default()
                .to_owned();
            ctx.update(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(format!("echo: {echoed}")),
                meta: None,
            }));
            Ok(PromptResponse {
                stop_reason: StopReason::EndTurn,
            })
        }

        fn cancel(&self, _session_id: &str) {}
    }

    #[test]
    fn initialize_new_prompt_round_trip() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| MockAgent));

        let (client, inbox, _h) = connect(BufReader::new(cli_r), cli_w);

        let init = client
            .request(
                "initialize",
                serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
            )
            .unwrap();
        assert_eq!(init["protocolVersion"], 1);
        assert_eq!(init["agentInfo"]["name"], "mock");

        let new = client
            .request("session/new", serde_json::json!({"cwd": "/tmp"}))
            .unwrap();
        assert_eq!(new["sessionId"], "sess-1");

        let res = client
            .request(
                "session/prompt",
                serde_json::json!({
                    "sessionId": "sess-1",
                    "prompt": [{"type": "text", "text": "hi"}]
                }),
            )
            .unwrap();
        assert_eq!(res["stopReason"], "end_turn");

        let note = inbox.recv().unwrap();
        match note {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "session/update");
                assert_eq!(params["sessionId"], "sess-1");
                assert_eq!(params["update"]["sessionUpdate"], "agent_message_chunk");
                assert_eq!(params["update"]["content"]["text"], "echo: hi");
            }
            Inbound::Request { .. } => panic!("expected a notification"),
        }

        drop(client);
        drop(inbox);
        server.join().unwrap().unwrap();
    }

    /// Opens sessions slowly and notes which ones it announced.
    struct SlowAgent(Arc<std::sync::Mutex<Vec<String>>>);

    impl Agent for SlowAgent {
        fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
            MockAgent.initialize(req)
        }

        fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            thread::sleep(std::time::Duration::from_millis(100));
            MockAgent.new_session(req)
        }

        fn prompt(
            &self,
            _req: PromptRequest,
            _ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            unreachable!("no prompt is sent")
        }

        fn cancel(&self, _session_id: &str) {}

        fn session_announced(&self, session_id: &str) {
            kage_core::sync::lock(&self.0).push(session_id.to_owned());
        }
    }

    #[test]
    fn a_new_session_is_answered_after_the_input_ends() {
        use std::io::Read as _;

        let (srv_r, mut cli_w) = std::io::pipe().unwrap();
        let (mut cli_r, srv_w) = std::io::pipe().unwrap();
        writeln!(
            cli_w,
            r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{{"cwd":"/tmp"}}}}"#
        )
        .unwrap();
        drop(cli_w);
        let announced = Arc::default();
        let agent = SlowAgent(Arc::clone(&announced));
        serve_agent(BufReader::new(srv_r), srv_w, |_| agent).unwrap();
        assert_eq!(*kage_core::sync::lock(&announced), ["sess-1"]);

        let mut out = String::new();
        cli_r.read_to_string(&mut out).unwrap();
        assert!(out.contains(r#""sessionId":"sess-1""#), "{out}");
    }

    struct LoadAgent;

    impl Agent for LoadAgent {
        fn initialize(&self, _req: InitializeRequest) -> InitializeResponse {
            InitializeResponse {
                protocol_version: crate::acp::PROTOCOL_VERSION,
                agent_capabilities: AgentCapabilities {
                    load_session: true,
                    prompt_capabilities: PromptCapabilities::default(),
                    ..AgentCapabilities::default()
                },
                agent_info: None,
                auth_methods: vec![],
                meta: None,
            }
        }

        fn new_session(&self, _r: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            Ok(NewSessionResponse {
                session_id: "s1".into(),
                config_options: vec![],
            })
        }

        fn prompt(
            &self,
            _r: PromptRequest,
            _c: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            Ok(PromptResponse {
                stop_reason: StopReason::EndTurn,
            })
        }

        fn cancel(&self, _session_id: &str) {}

        fn load_session(
            &self,
            req: LoadSessionRequest,
            ctx: &PromptContext,
        ) -> Result<LoadSessionResponse, RpcError> {
            ctx.update(SessionUpdate::AgentMessageChunk(MessageChunk {
                content: ContentBlock::text(format!("history of {}", req.session_id)),
                meta: None,
            }));
            Ok(LoadSessionResponse::default())
        }

        fn list_sessions(
            &self,
            req: ListSessionsRequest,
        ) -> Result<ListSessionsResponse, RpcError> {
            Ok(ListSessionsResponse {
                sessions: vec![SessionInfo {
                    session_id: "s1".into(),
                    cwd: req.cwd.unwrap_or_default(),
                    title: None,
                    updated_at: None,
                    meta: None,
                }],
                next_cursor: None,
            })
        }

        fn resume_session(
            &self,
            _req: ResumeSessionRequest,
            _ctx: &PromptContext,
        ) -> Result<ResumeSessionResponse, RpcError> {
            Ok(ResumeSessionResponse::default())
        }

        fn close_session(
            &self,
            req: CloseSessionRequest,
        ) -> Result<CloseSessionResponse, RpcError> {
            if req.session_id == "s1" {
                Ok(CloseSessionResponse {})
            } else {
                Err(RpcError::new(
                    -32602,
                    format!("unknown session {}", req.session_id),
                ))
            }
        }

        fn set_config_option(
            &self,
            req: SetSessionConfigOptionRequest,
        ) -> Result<SetSessionConfigOptionResponse, RpcError> {
            Err(RpcError::new(
                -32602,
                format!("unknown option {}", req.config_id),
            ))
        }
    }

    #[test]
    fn session_load_streams_history_then_resolves() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| LoadAgent));
        let (client, inbox, _h) = connect(BufReader::new(cli_r), cli_w);

        let res = client
            .request(
                "session/load",
                serde_json::json!({"sessionId": "s9", "cwd": "/tmp", "mcpServers": []}),
            )
            .unwrap();
        assert_eq!(res, serde_json::json!({}));

        match inbox.recv().unwrap() {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "session/update");
                assert_eq!(params["update"]["content"]["text"], "history of s9");
            }
            Inbound::Request { .. } => panic!("expected a notification"),
        }
        drop(client);
        drop(inbox);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn session_methods_dispatch_to_the_agent() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| LoadAgent));
        let (client, _inbox, _h) = connect(BufReader::new(cli_r), cli_w);

        let list = client
            .request("session/list", serde_json::json!({"cwd": "/w"}))
            .unwrap();
        assert_eq!(
            list,
            serde_json::json!({"sessions": [{"sessionId": "s1", "cwd": "/w"}]})
        );
        let resume = client
            .request(
                "session/resume",
                serde_json::json!({"sessionId": "s1", "cwd": "/w", "mcpServers": []}),
            )
            .unwrap();
        assert_eq!(resume, serde_json::json!({}));
        let close = client
            .request("session/close", serde_json::json!({"sessionId": "s1"}))
            .unwrap();
        assert_eq!(close, serde_json::json!({}));
        let err = client
            .request("session/close", serde_json::json!({"sessionId": "x"}))
            .unwrap_err();
        assert_eq!(err.code, -32602);
        let err = client
            .request(
                "session/set_config_option",
                serde_json::json!({"sessionId": "s1", "configId": "nope", "value": "x"}),
            )
            .unwrap_err();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "unknown option nope");
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn new_session_methods_default_to_method_not_found() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| MockAgent));
        let (client, _inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        for (method, params) in [
            ("session/list", serde_json::json!({})),
            (
                "session/resume",
                serde_json::json!({"sessionId": "x", "cwd": "/", "mcpServers": []}),
            ),
            (
                "session/set_config_option",
                serde_json::json!({"sessionId": "x", "configId": "model", "value": "m"}),
            ),
        ] {
            let err = client.request(method, params).unwrap_err();
            assert_eq!(err.code, -32601, "{method}");
        }
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn session_load_default_is_method_not_found() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| MockAgent));
        let (client, _inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        let err = client
            .request(
                "session/load",
                serde_json::json!({"sessionId": "x", "cwd": "/", "mcpServers": []}),
            )
            .unwrap_err();
        assert_eq!(err.code, -32601);
        drop(client);
        server.join().unwrap().unwrap();
    }

    /// Asks permission on every prompt and gives up once cancelled.
    #[derive(Default)]
    struct AskAgent {
        cancel: CancelFlag,
    }

    impl Agent for AskAgent {
        fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
            MockAgent.initialize(req)
        }

        fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            MockAgent.new_session(req)
        }

        fn prompt(
            &self,
            req: PromptRequest,
            ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            let tool_call = ToolCallUpdate {
                tool_call_id: "call-1".into(),
                ..ToolCallUpdate::default()
            };
            let decision = request_permission(
                ctx.peer(),
                &req.session_id,
                tool_call,
                "shell",
                &self.cancel,
            );
            assert!(matches!(decision, PermissionDecision::Unanswered));
            Ok(PromptResponse {
                stop_reason: StopReason::Cancelled,
            })
        }

        fn cancel(&self, _session_id: &str) {
            self.cancel.cancel();
        }
    }

    #[test]
    fn abandoned_permission_ask_sends_cancel_request() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || {
            serve_agent(BufReader::new(srv_r), srv_w, |_| AskAgent::default())
        });
        let (client, inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        let prompt = {
            let client = client.clone();
            thread::spawn(move || {
                client.request(
                    "session/prompt",
                    serde_json::json!({"sessionId": "sess-1", "prompt": []}),
                )
            })
        };
        let timeout = std::time::Duration::from_secs(5);
        let Ok(Inbound::Request { id, method, .. }) = inbox.recv_timeout(timeout) else {
            panic!("expected the permission request");
        };
        assert_eq!(method, "session/request_permission");
        client
            .notify("session/cancel", serde_json::json!({"sessionId": "sess-1"}))
            .unwrap();
        match inbox.recv_timeout(timeout) {
            Ok(Inbound::Notification { method, params }) => {
                assert_eq!(method, "$/cancel_request");
                assert_eq!(params, serde_json::json!({"requestId": id}));
            }
            other => panic!("expected $/cancel_request, got {other:?}"),
        }
        let res = prompt.join().unwrap().unwrap();
        assert_eq!(res["stopReason"], "cancelled");
        drop(client);
        drop(inbox);
        server.join().unwrap().unwrap();
    }

    /// Asks once and records the decision the ask came back with.
    #[derive(Default)]
    struct UnansweredAgent {
        seen: std::sync::Arc<std::sync::Mutex<Vec<PermissionDecision>>>,
    }

    impl Agent for UnansweredAgent {
        fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
            MockAgent.initialize(req)
        }

        fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            MockAgent.new_session(req)
        }

        fn prompt(
            &self,
            req: PromptRequest,
            ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            let tool_call = ToolCallUpdate {
                tool_call_id: "call-1".into(),
                ..ToolCallUpdate::default()
            };
            let decision = request_permission(
                ctx.peer(),
                &req.session_id,
                tool_call,
                "shell",
                &CancelFlag::new(),
            );
            kage_core::sync::lock(&self.seen).push(decision);
            Ok(PromptResponse {
                stop_reason: StopReason::Cancelled,
            })
        }

        fn cancel(&self, _session_id: &str) {}
    }
    #[test]
    fn a_connection_that_closes_while_asked_answers_unanswered() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let agent = UnansweredAgent::default();
        let seen = std::sync::Arc::clone(&agent.seen);
        let server = thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| agent));
        let (client, inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        let prompt = {
            let client = client.clone();
            thread::spawn(move || {
                client.request(
                    "session/prompt",
                    serde_json::json!({"sessionId": "sess-1", "prompt": []}),
                )
            })
        };
        let timeout = std::time::Duration::from_secs(5);
        let Ok(Inbound::Request { .. }) = inbox.recv_timeout(timeout) else {
            panic!("expected the permission request");
        };
        // One line over the input cap ends the connection even though
        // the prompt request still pins the write half. The write may
        // fail with EPIPE once the server stops reading; the cap was
        // passed by then.
        let _ = client.notify(
            "flood",
            serde_json::Value::String("x".repeat(9 * 1024 * 1024)),
        );
        drop(client);
        drop(inbox);
        server.join().unwrap().unwrap();
        let deadline = std::time::Instant::now() + timeout;
        while kage_core::sync::lock(&seen).is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the ask never resolved"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            *kage_core::sync::lock(&seen),
            [PermissionDecision::Unanswered]
        );
        let _ = prompt.join();
    }

    /// Raises a plan review on every prompt and records the decision.
    #[derive(Default)]
    struct ReviewAgent {
        seen: std::sync::Arc<std::sync::Mutex<Vec<PlanReviewDecision>>>,
    }

    impl Agent for ReviewAgent {
        fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
            MockAgent.initialize(req)
        }

        fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            MockAgent.new_session(req)
        }

        fn prompt(
            &self,
            req: PromptRequest,
            ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            let tool_call = ToolCallUpdate {
                tool_call_id: "call-1".into(),
                ..ToolCallUpdate::default()
            };
            let decision = request_plan_review(
                ctx.peer(),
                &req.session_id,
                tool_call,
                "# P",
                &CancelFlag::new(),
            );
            kage_core::sync::lock(&self.seen).push(decision);
            Ok(PromptResponse {
                stop_reason: StopReason::Cancelled,
            })
        }

        fn cancel(&self, _session_id: &str) {}
    }

    /// Answers one plan review with `answer` and returns the decision
    /// the ask came back with.
    fn review_answer(answer: serde_json::Value) -> PlanReviewDecision {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let agent = ReviewAgent::default();
        let seen = std::sync::Arc::clone(&agent.seen);
        let server = thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| agent));
        let (client, inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        let prompt = {
            let client = client.clone();
            thread::spawn(move || {
                client.request(
                    "session/prompt",
                    serde_json::json!({"sessionId": "sess-1", "prompt": []}),
                )
            })
        };
        let Ok(Inbound::Request { id, method, params }) =
            inbox.recv_timeout(std::time::Duration::from_secs(5))
        else {
            panic!("expected the permission request");
        };
        assert_eq!(method, "session/request_permission");
        let options: Vec<&str> = params["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["optionId"].as_str().unwrap())
            .collect();
        assert_eq!(options, ["approve", "revise", "reject"]);
        assert_eq!(params["_meta"]["kage"]["planReview"]["plan"], "# P");
        client.respond(&id, Ok(answer)).unwrap();
        prompt.join().unwrap().unwrap();
        drop(client);
        drop(inbox);
        server.join().unwrap().unwrap();
        let mut seen = kage_core::sync::lock(&seen);
        seen.pop().unwrap()
    }

    #[test]
    fn a_plan_review_maps_approve_revise_and_reject() {
        assert_eq!(
            review_answer(serde_json::json!({
                "outcome": {"outcome": "selected", "optionId": "approve"}
            })),
            PlanReviewDecision::Approve
        );
        assert_eq!(
            review_answer(serde_json::json!({
                "outcome": {"outcome": "selected", "optionId": "revise"},
                "_meta": {"kage": {"planReview": {"revision": "add tests"}}}
            })),
            PlanReviewDecision::Revise("add tests".to_owned())
        );
        assert_eq!(
            review_answer(serde_json::json!({
                "outcome": {"outcome": "selected", "optionId": "revise"}
            })),
            PlanReviewDecision::Revise(String::new())
        );
        assert_eq!(
            review_answer(serde_json::json!({
                "outcome": {"outcome": "selected", "optionId": "reject"}
            })),
            PlanReviewDecision::Reject
        );
        assert_eq!(
            review_answer(serde_json::json!({"outcome": {"outcome": "cancelled"}})),
            PlanReviewDecision::Reject
        );
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server =
            thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| MockAgent));
        let (client, _inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        let err = client
            .request("bogus/method", serde_json::Value::Null)
            .unwrap_err();
        assert_eq!(err.code, -32601);
        drop(client);
        server.join().unwrap().unwrap();
    }
}
