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
//! Admitted requests except `initialize` run on their own threads. Prompt
//! workers are limited per connection, including their response writes. Excess
//! prompts are rejected rather than waiting for a worker slot. Other request
//! threads are not limited by this cap. The dispatch loop can continue to
//! receive cancellation while prompt slots are occupied.
//! When the input ends, every request but a prompt that is still in
//! flight is answered before [`serve_agent`] returns.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

pub use crate::agent_peer::AgentPeer;
use kage_core::CancelFlag;
use kage_jsonrpc::{CancelNotice, Inbound, RpcError, connect_with};

use crate::acp::{UiAttachRequest, UiCommandRequest, UiKeymapRequest, UiOutput, UiSnapshot};

use crate::acp::{
    AuthSetRequest, CloseSessionRequest, CloseSessionResponse, ConfigGetRequest, ConfigGetResult,
    ConfigSetRequest, ConfigTestRequest, ConfigTestResult, DirectoryRequest, DirectoryResult,
    FoldersRequest, FoldersResult, FsRequest, FsResult, ImageSaveRequest, InitializeRequest,
    InitializeResponse, KageMeta, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, McpRestartRequest, ModelsResponse, NewSessionRequest, NewSessionResponse,
    OptionSetRequest, OptionsResponse, PermissionOption, PermissionOptionKind, PermissionOutcome,
    PlanReview, PluginInstallRequest, PluginRemoveRequest, PromptRequest, PromptResponse,
    PromptWithdrawn, QuestionMeta, QuestionPrompt, RegistryInstallRequest, RegistryInstallResult,
    RegistrySearchRequest, RegistrySearchResult, RequestMeta, RequestPermissionRequest,
    RequestPermissionResponse, RequestPermissionResult, ResumeSessionRequest,
    ResumeSessionResponse, SessionExportResponse, SessionForkRequest, SessionForkResponse,
    SessionNotification, SessionPromptsResponse, SessionRenameRequest, SessionRequest,
    SessionRewindRequest, SessionUndoResponse, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, ShellRequest, SwarmResumeRequest, SwarmResumeResponse,
    ThemesRequest, ThemesResult, ToolCallUpdate, WithdrawRequest,
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
    peer: &AgentPeer,
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
    peer: &AgentPeer,
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
    peer: &AgentPeer,
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
    peer: AgentPeer,
    session_id: String,
    start: Option<Arc<Mutex<bool>>>,
}

impl PromptContext {
    /// Start a prompt by registering it with the backend while ordered against
    /// earlier and later `session/cancel` notifications. The closure must only
    /// perform the short registration, not wait for the prompt to finish.
    /// Returns `None` if cancellation arrived before registration. A later
    /// cancellation is still forwarded to [`Agent::cancel`]. Call this once
    /// per prompt. It does not coordinate prompts on other connections.
    #[must_use]
    pub fn start<T>(&self, register: impl FnOnce() -> T) -> Option<T> {
        let Some(start) = &self.start else {
            return Some(register());
        };
        let cancelled = start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *cancelled { None } else { Some(register()) }
    }

    /// Emit a `session/update` notification for this session.
    ///
    /// # Errors
    /// Returns an encoding or transport error.
    pub fn update(&self, update: SessionUpdate) -> Result<(), RpcError> {
        send_update(&self.peer, &self.session_id, update)
    }

    /// The underlying peer, for agent-initiated requests.
    #[must_use]
    pub fn peer(&self) -> &AgentPeer {
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
///
/// # Errors
/// Returns an encoding or transport error.
pub fn send_update(
    peer: &AgentPeer,
    session_id: &str,
    update: SessionUpdate,
) -> Result<(), RpcError> {
    let note = SessionNotification {
        session_id: session_id.to_owned(),
        update: update.into(),
    };
    let params = serde_json::to_value(&note)
        .map_err(|e| RpcError::internal(format!("encode update: {e}")))?;
    peer.notify("session/update", params)
}

/// A per-open action invoked once after the successful response is written.
/// If the response cannot be written, the receipt is dropped without invocation.
pub type OpenReceipt = Box<dyn FnOnce() + Send + 'static>;

/// An open response and its optional per-operation completion receipt.
/// A receipt replaces the call to [`Agent::session_announced`] for that open.
pub type OpenResult<T> = Result<(T, Option<OpenReceipt>), RpcError>;

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

    /// Create a session with an optional completion receipt. The default calls
    /// [`Agent::new_session`] and returns no receipt.
    ///
    /// # Errors
    /// Returns an error if the session cannot be created.
    fn new_session_with_receipt(&self, req: NewSessionRequest) -> OpenResult<NewSessionResponse> {
        self.new_session(req).map(|response| (response, None))
    }

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

    /// Load a session with an optional completion receipt. The default calls
    /// [`Agent::load_session`] and returns no receipt. History is still streamed
    /// before the response.
    ///
    /// # Errors
    /// Returns an error if the session cannot be loaded.
    fn load_session_with_receipt(
        &self,
        req: LoadSessionRequest,
        ctx: &PromptContext,
    ) -> OpenResult<LoadSessionResponse> {
        self.load_session(req, ctx).map(|response| (response, None))
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

    /// Resume a session with an optional completion receipt. The default calls
    /// [`Agent::resume_session`] and returns no receipt.
    ///
    /// # Errors
    /// Returns an error if the session cannot be resumed.
    fn resume_session_with_receipt(
        &self,
        req: ResumeSessionRequest,
        ctx: &PromptContext,
    ) -> OpenResult<ResumeSessionResponse> {
        self.resume_session(req, ctx)
            .map(|response| (response, None))
    }

    /// Prepare a load on the dispatch thread, so a later close can fence it.
    /// Keep this short and nonblocking. The returned token is passed to
    /// [`Agent::load_session_prepared`] on the worker thread. The default
    /// prepares nothing.
    fn prepare_load(&self, _req: &LoadSessionRequest) -> Option<Box<dyn std::any::Any + Send>> {
        None
    }

    /// Load using the token returned by [`Agent::prepare_load`]. The default
    /// ignores the token and calls [`Agent::load_session_with_receipt`].
    ///
    /// # Errors
    /// Returns an error if the session cannot be loaded.
    fn load_session_prepared(
        &self,
        req: LoadSessionRequest,
        ctx: &PromptContext,
        _prepared: Option<Box<dyn std::any::Any + Send>>,
    ) -> OpenResult<LoadSessionResponse> {
        self.load_session_with_receipt(req, ctx)
    }

    /// Prepare a resume on the dispatch thread, so a later close can fence
    /// it. Keep this short and nonblocking. The returned token is passed to
    /// [`Agent::resume_session_prepared`] on the worker thread. The default
    /// prepares nothing.
    fn prepare_resume(&self, _req: &ResumeSessionRequest) -> Option<Box<dyn std::any::Any + Send>> {
        None
    }

    /// Resume using the token returned by [`Agent::prepare_resume`]. The
    /// default ignores the token and calls
    /// [`Agent::resume_session_with_receipt`].
    ///
    /// # Errors
    /// Returns an error if the session cannot be resumed.
    fn resume_session_prepared(
        &self,
        req: ResumeSessionRequest,
        ctx: &PromptContext,
        _prepared: Option<Box<dyn std::any::Any + Send>>,
    ) -> OpenResult<ResumeSessionResponse> {
        self.resume_session_with_receipt(req, ctx)
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

    /// Fence a close on the dispatch thread before any later message is
    /// dispatched. Keep this short and nonblocking. The returned token is
    /// passed to [`Agent::close_session_fenced`] on the worker thread. The
    /// default fences nothing.
    fn fence_close(&self, _req: &CloseSessionRequest) -> Option<Box<dyn std::any::Any + Send>> {
        None
    }

    /// Finish a close using the token returned by [`Agent::fence_close`].
    /// The default ignores the token and calls [`Agent::close_session`].
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session id is unknown or cannot
    /// be released.
    fn close_session_fenced(
        &self,
        req: CloseSessionRequest,
        _fence: Option<Box<dyn std::any::Any + Send>>,
    ) -> Result<CloseSessionResponse, RpcError> {
        self.close_session(req)
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

    /// Drop the session's last prompt and what followed it
    /// (`_kage/session/undo`), answering with the dropped prompt. Every
    /// client showing the session gets `_kage/session_replaced`. The
    /// default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown, running, or has
    /// nothing to undo.
    fn session_undo(&self, _req: SessionRequest) -> Result<SessionUndoResponse, RpcError> {
        Err(RpcError::method_not_found("_kage/session/undo"))
    }

    /// Continue the session from the end of a past prompt's turn
    /// (`_kage/session/rewind`). Every client showing the session gets
    /// `_kage/session_replaced`. The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown, running, or has
    /// no such prompt.
    fn session_rewind(&self, _req: SessionRewindRequest) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::method_not_found("_kage/session/rewind"))
    }

    /// Every prompt the session can be rewound to
    /// (`_kage/session/prompts`). The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown.
    fn session_prompts(&self, _req: SessionRequest) -> Result<SessionPromptsResponse, RpcError> {
        Err(RpcError::method_not_found("_kage/session/prompts"))
    }

    /// Run a shell command the user typed in the session
    /// (`_kage/shell`). Its progress reaches clients as `_kage/shell`
    /// updates. The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown.
    fn session_shell(&self, _req: ShellRequest) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::method_not_found("_kage/shell"))
    }

    /// Take back the session's newest pending prompt of one queue
    /// (`_kage/prompt/withdraw`). The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown.
    fn prompt_withdraw(&self, _req: WithdrawRequest) -> Result<PromptWithdrawn, RpcError> {
        Err(RpcError::method_not_found("_kage/prompt/withdraw"))
    }

    /// Restart one of the session's MCP servers (`_kage/mcp/restart`).
    /// The result reaches clients as MCP updates. The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown.
    fn mcp_restart(&self, _req: McpRestartRequest) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::method_not_found("_kage/mcp/restart"))
    }

    /// The plugin UI of the project at `req.cwd` (`_kage/ui/attach`). The
    /// connection keeps the project's plugins loaded until it ends. The
    /// default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the project's plugins cannot load.
    fn ui_attach(&self, _req: UiAttachRequest) -> Result<UiSnapshot, RpcError> {
        Err(RpcError::method_not_found("_kage/ui/attach"))
    }

    /// Run a plugin command for the client (`_kage/ui/command`). Dialogs
    /// it opens reach the client as `_kage/ui/dialog` requests. The
    /// default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if no plugin registered the command.
    fn ui_command(&self, _req: UiCommandRequest) -> Result<UiOutput, RpcError> {
        Err(RpcError::method_not_found("_kage/ui/command"))
    }

    /// Run a key mapping's Lua handler for the client
    /// (`_kage/ui/keymap`), dialogs included. The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if no mapping has the handler.
    fn ui_keymap(&self, _req: UiKeymapRequest) -> Result<UiOutput, RpcError> {
        Err(RpcError::method_not_found("_kage/ui/keymap"))
    }

    /// Delete a recorded session no client has open
    /// (`_kage/session/delete`). The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown or open.
    fn session_delete(&self, _req: SessionRequest) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::method_not_found("_kage/session/delete"))
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

    /// Lists the user themes on the machine the agent runs on
    /// (`_kage/themes`), for a client that draws its own palette. The
    /// default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the themes cannot be listed.
    fn themes(&self, _req: ThemesRequest) -> Result<ThemesResult, RpcError> {
        Err(RpcError::method_not_found("_kage/themes"))
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

    /// Saves an image the model produced into the session's directory
    /// (`_kage/image/save`). The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the session is unknown.
    fn image_save(&self, _req: ImageSaveRequest) -> Result<serde_json::Value, RpcError> {
        Err(RpcError::method_not_found("_kage/image/save"))
    }

    /// Lists the packages of the plugin registry
    /// (`_kage/registry/search`). The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the registry cannot be reached and no
    /// index is cached.
    fn registry_search(
        &self,
        _req: RegistrySearchRequest,
    ) -> Result<RegistrySearchResult, RpcError> {
        Err(RpcError::method_not_found("_kage/registry/search"))
    }

    /// Installs a package from the plugin registry into the plugin
    /// directory (`_kage/registry/install`). The default rejects.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if no release fits, a download fails or
    /// does not match the index, or something of the same name is in
    /// the way.
    fn registry_install(
        &self,
        _req: RegistryInstallRequest,
    ) -> Result<RegistryInstallResult, RpcError> {
        Err(RpcError::method_not_found("_kage/registry/install"))
    }

    /// Prepare a prompt on the dispatch thread after a worker slot is reserved.
    /// Keep this operation short and nonblocking. The returned owned token is
    /// passed to [`Agent::prompt_prepared`] on the worker thread. It is dropped
    /// if the worker cannot be spawned.
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the prompt cannot be prepared.
    fn prepare_prompt(
        &self,
        _req: &PromptRequest,
    ) -> Result<Option<Box<dyn std::any::Any + Send>>, RpcError> {
        Ok(None)
    }

    /// Run a prompt using the token returned by [`Agent::prepare_prompt`].
    /// The default ignores the token and calls [`Agent::prompt`].
    ///
    /// # Errors
    ///
    /// Returns an [`RpcError`] if the turn cannot start or fails irrecoverably.
    fn prompt_prepared(
        &self,
        req: PromptRequest,
        ctx: &PromptContext,
        _prepared: Option<Box<dyn std::any::Any + Send>>,
    ) -> Result<PromptResponse, RpcError> {
        self.prompt(req, ctx)
    }

    /// Run one prompt turn to completion, streaming `session/update`
    /// notifications through `ctx`. Implementations that register work before
    /// cancellation can affect it must use [`PromptContext::start`] for that
    /// registration and return a cancelled response if it returns `None`.
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

    /// The input ended. Called before waiting for outstanding requests,
    /// which may still be running concurrently, including session opens
    /// and prompts. Implementations may interrupt connection-bound work
    /// here, but must synchronize with those requests and prevent any
    /// resources created by a concurrent open from escaping cleanup.
    /// The default does nothing.
    fn disconnecting(&self) {}

    /// The connection ended and every request but a prompt that is
    /// still running is answered. Called after [`Agent::disconnecting`]
    /// and after the other requests have finished. The default does nothing.
    fn detached(&self) {}

    /// The client sent its environment in a `_kage/environment`
    /// notification. Requests read after it see it. The default ignores
    /// it.
    fn environment(&self, _vars: BTreeMap<String, String>) {}
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
    F: FnOnce(AgentPeer) -> A,
{
    let (peer, inbound, _reader) = connect_with(reader, writer, Some(cancel_notice()));
    let peer = AgentPeer::from(peer);
    serve_inbound(&peer, inbound, make_agent)
}

/// Serve an ACP agent over an exclusively owned Unix pipe writer.
///
/// Every outgoing write has `write_timeout`. The inbound reader must be
/// terminated by its owner. A write failure cannot interrupt a blocked reader.
/// The reader must be drained even while agent requests are outstanding.
///
/// # Errors
/// Returns an error if the pipe cannot be initialized or an outgoing write fails.
#[cfg(unix)]
pub fn serve_agent_unix<R, A, F>(
    reader: R,
    writer: std::os::fd::OwnedFd,
    write_timeout: std::time::Duration,
    make_agent: F,
) -> Result<(), RpcError>
where
    R: BufRead + Send + 'static,
    A: Agent,
    F: FnOnce(AgentPeer) -> A,
{
    let (peer, inbound, _reader) = kage_jsonrpc::unix_peer::connect_ordered_server(reader, writer)
        .map_err(|e| RpcError::internal(format!("initialize Unix transport: {e}")))?;
    serve_agent_unix_connected(peer, &inbound, write_timeout, make_agent)
}

/// Serve an ACP agent over a preconnected Unix peer and its ordered inbound receiver.
///
/// The peer and receiver must come from the same ordered server connection. The
/// receiver remains owned by the caller, as does the reader thread and its
/// lifecycle. A generic reader cannot be interrupted by this function, even
/// after a write failure, so its owner must arrange for it to terminate. Every
/// outgoing write has `write_timeout`, but request execution, cleanup, and the
/// overall serving operation are unbounded. Callbacks invoked on the inbound
/// consumer thread, including `make_agent`, `initialize`, `cancel`,
/// `disconnecting`, and `detached`, must not synchronously wait for outbound
/// requests. Their response barriers can only be acknowledged by that same
/// consumer, which is no longer draining during disconnect callbacks.
///
/// A clean input EOF finishes outstanding non-prompt requests before returning.
/// Ordered barriers are acknowledged as inbound messages are drained.
///
/// # Errors
///
/// Returns an error if the Unix transport records a reader or writer failure.
#[cfg(unix)]
pub fn serve_agent_unix_connected<A, F>(
    peer: kage_jsonrpc::unix_peer::UnixPeer,
    inbound: &std::sync::mpsc::Receiver<kage_jsonrpc::OrderedInbound>,
    write_timeout: std::time::Duration,
    make_agent: F,
) -> Result<(), RpcError>
where
    A: Agent,
    F: FnOnce(AgentPeer) -> A,
{
    use kage_jsonrpc::OrderedInbound;
    let peer = AgentPeer::unix(peer, write_timeout);
    let monitor = peer.clone();
    let messages = std::iter::from_fn(move || {
        loop {
            if monitor.failure().is_some() {
                return None;
            }
            match inbound.recv_timeout(std::time::Duration::from_millis(25)) {
                Ok(OrderedInbound::Message(message)) => return Some(message),
                Ok(OrderedInbound::Barrier(marker)) => marker.acknowledge(),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
    });
    serve_inbound(&peer, messages, make_agent)
}

// This bounds queued as well as executing prompts, for each connection.
const MAX_PROMPT_WORKERS: usize = 32;

struct PromptPermit(Arc<AtomicUsize>);

impl PromptPermit {
    fn acquire(count: &Arc<AtomicUsize>) -> Option<Self> {
        count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current < MAX_PROMPT_WORKERS).then_some(current + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(count)))
    }
}

impl Drop for PromptPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn serve_inbound<A, F>(
    peer: &AgentPeer,
    inbound: impl IntoIterator<Item = Inbound>,
    make_agent: F,
) -> Result<(), RpcError>
where
    A: Agent,
    F: FnOnce(AgentPeer) -> A,
{
    let agent = Arc::new(make_agent(peer.clone()));

    let mut ops = Vec::new();
    let prompt_workers = Arc::new(AtomicUsize::new(0));
    let mut starts: HashMap<String, Vec<Weak<Mutex<bool>>>> = HashMap::new();
    for message in inbound {
        starts.retain(|_, entries| {
            entries.retain(|entry| entry.strong_count() > 0);
            !entries.is_empty()
        });
        match message {
            Inbound::Notification { method, params } => match method.as_str() {
                "session/cancel" => {
                    if let Ok(c) = parse::<crate::acp::CancelNotification>(params) {
                        if let Some(entries) = starts.get(&c.session_id) {
                            for entry in entries {
                                if let Some(entry) = entry.upgrade() {
                                    *entry
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                                }
                            }
                        }
                        agent.cancel(&c.session_id);
                    }
                }
                "_kage/environment" => {
                    if let Ok(e) = parse::<crate::acp::EnvironmentNotification>(params) {
                        agent.environment(e.vars);
                    }
                }
                _ => {}
            },
            Inbound::Request { id, method, params } => {
                if let Some(op) = handle_request(
                    peer,
                    &agent,
                    &mut starts,
                    &prompt_workers,
                    id,
                    &method,
                    params,
                ) {
                    ops.retain(|op: &thread::JoinHandle<()>| !op.is_finished());
                    ops.push(op);
                }
            }
        }
    }
    agent.disconnecting();
    for op in ops {
        let _ = op.join();
    }
    agent.detached();
    #[cfg(unix)]
    if let Some(error) = peer.failure() {
        return Err(error);
    }
    Ok(())
}

fn jval<T: serde::Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// Answer request `id` with `op`, run on its own thread.
fn spawn_op<A, F>(
    peer: &AgentPeer,
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

fn spawn_prompt<A: Agent>(
    peer: &AgentPeer,
    agent: &Arc<A>,
    id: &serde_json::Value,
    req: PromptRequest,
    ctx: PromptContext,
    permit: PromptPermit,
    prepared: Option<Box<dyn std::any::Any + Send>>,
) {
    let agent = Arc::clone(agent);
    let worker_peer = peer.clone();
    let worker_id = id.clone();
    if let Err(error) = thread::Builder::new().spawn(move || {
        let _permit = permit;
        let outcome = agent.prompt_prepared(req, &ctx, prepared).map(jval);
        let _ = worker_peer.respond(&worker_id, outcome);
    }) {
        let _ = peer.respond(
            id,
            Err(RpcError::internal(format!("spawn prompt worker: {error}"))),
        );
    }
}

/// Answer request `id` with `op`, which opens the session it returns
/// the id of, on its own thread, then tell the agent the client has the
/// response.
fn spawn_open<A, F, T>(
    peer: &AgentPeer,
    agent: &Arc<A>,
    id: serde_json::Value,
    op: F,
) -> thread::JoinHandle<()>
where
    A: Agent,
    F: FnOnce(&A) -> Result<(String, T, Option<OpenReceipt>), RpcError> + Send + 'static,
    T: serde::Serialize,
{
    let agent = Arc::clone(agent);
    let peer = peer.clone();
    thread::spawn(move || match op(&agent).and_then(encode_open) {
        Ok((session, response, receipt)) => {
            if peer.respond(&id, Ok(response)).is_ok() {
                if let Some(receipt) = receipt {
                    receipt();
                } else {
                    agent.session_announced(&session);
                }
            }
        }
        Err(e) => {
            let _ = peer.respond(&id, Err(e));
        }
    })
}

fn encode_open<T: serde::Serialize>(
    (session, response, receipt): (String, T, Option<OpenReceipt>),
) -> Result<(String, serde_json::Value, Option<OpenReceipt>), RpcError> {
    let response = serde_json::to_value(response)
        .map_err(|e| RpcError::internal(format!("encode open response: {e}")))?;
    Ok((session, response, receipt))
}

/// Fence a close on the dispatch thread, then finish it on its own thread.
fn spawn_close<A: Agent>(
    peer: &AgentPeer,
    agent: &Arc<A>,
    id: serde_json::Value,
    req: CloseSessionRequest,
) -> thread::JoinHandle<()> {
    let fence = agent.fence_close(&req);
    spawn_op(peer, agent, id, move |a| {
        a.close_session_fenced(req, fence).map(jval)
    })
}

/// Answers `id` with the params parse error `e`.
fn parse_failed(
    peer: &AgentPeer,
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
    peer: &AgentPeer,
    agent: &Arc<A>,
    starts: &mut HashMap<String, Vec<Weak<Mutex<bool>>>>,
    prompt_workers: &Arc<AtomicUsize>,
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
                a.new_session_with_receipt(req)
                    .map(|(resp, receipt)| (resp.session_id.clone(), resp, receipt))
            }),
        },
        "session/prompt" => match parse::<PromptRequest>(params) {
            Err(e) => return parse_failed(peer, &id, e),
            Ok(req) => {
                let Some(permit) = PromptPermit::acquire(prompt_workers) else {
                    return parse_failed(
                        peer,
                        &id,
                        RpcError::new(-32000, "prompt worker limit reached"),
                    );
                };
                let prepared = match agent.prepare_prompt(&req) {
                    Ok(prepared) => prepared,
                    Err(error) => return parse_failed(peer, &id, error),
                };
                let start = Arc::new(Mutex::new(false));
                starts
                    .entry(req.session_id.clone())
                    .or_default()
                    .push(Arc::downgrade(&start));
                let ctx = PromptContext {
                    peer: peer.clone(),
                    session_id: req.session_id.clone(),
                    start: Some(start),
                };
                spawn_prompt(peer, agent, &id, req, ctx, permit, prepared);
                return None;
            }
        },
        "session/load" => match parse::<LoadSessionRequest>(params) {
            Err(e) => return parse_failed(peer, &id, e),
            Ok(req) => {
                let ctx = PromptContext {
                    peer: peer.clone(),
                    session_id: req.session_id.clone(),
                    start: None,
                };
                let prepared = agent.prepare_load(&req);
                spawn_open(peer, agent, id, move |a| {
                    let session = req.session_id.clone();
                    a.load_session_prepared(req, &ctx, prepared)
                        .map(|(resp, receipt)| (session, resp, receipt))
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
                    start: None,
                };
                let prepared = agent.prepare_resume(&req);
                spawn_open(peer, agent, id, move |a| {
                    let session = req.session_id.clone();
                    a.resume_session_prepared(req, &ctx, prepared)
                        .map(|(resp, receipt)| (session, resp, receipt))
                })
            }
        },
        "session/set_config_option" => match parse::<SetSessionConfigOptionRequest>(params) {
            Err(e) => return parse_failed(peer, &id, e),
            Ok(req) => spawn_op(peer, agent, id, move |a| a.set_config_option(req).map(jval)),
        },
        "session/close" => match parse::<CloseSessionRequest>(params) {
            Err(e) => return parse_failed(peer, &id, e),
            Ok(req) => spawn_close(peer, agent, id, req),
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
    peer: &AgentPeer,
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
        "_kage/themes" => match parse::<ThemesRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.themes(req).map(jval)),
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
        "_kage/image/save" => match parse::<ImageSaveRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.image_save(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/registry/search" => match parse::<RegistrySearchRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.registry_search(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/registry/install" => match parse::<RegistryInstallRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.registry_install(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/session/rename" => match parse::<SessionRenameRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.session_rename(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        other => return handle_session_request(peer, agent, id, other, params),
    };
    Some(op)
}

/// The requests that act on one session: undo, rewind, its prompts,
/// deleting it, a user shell command, withdrawing a pending prompt and
/// restarting an MCP server.
fn handle_session_request<A: Agent>(
    peer: &AgentPeer,
    agent: &Arc<A>,
    id: serde_json::Value,
    method: &str,
    params: serde_json::Value,
) -> Option<thread::JoinHandle<()>> {
    let op = match method {
        "_kage/session/undo" => match parse::<SessionRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.session_undo(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/session/rewind" => match parse::<SessionRewindRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.session_rewind(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/session/prompts" => match parse::<SessionRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.session_prompts(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/session/delete" => match parse::<SessionRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.session_delete(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/shell" => match parse::<ShellRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.session_shell(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/prompt/withdraw" => match parse::<WithdrawRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.prompt_withdraw(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/mcp/restart" => match parse::<McpRestartRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.mcp_restart(req)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/ui/attach" => match parse::<UiAttachRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.ui_attach(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/ui/command" => match parse::<UiCommandRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.ui_command(req).map(jval)),
            Err(e) => return parse_failed(peer, &id, e),
        },
        "_kage/ui/keymap" => match parse::<UiKeymapRequest>(params) {
            Ok(req) => spawn_op(peer, agent, id, move |a| a.ui_keymap(req).map(jval)),
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
            }))?;
            Ok(PromptResponse {
                stop_reason: StopReason::EndTurn,
            })
        }

        fn cancel(&self, _session_id: &str) {}
    }

    #[cfg(unix)]
    #[test]
    fn prompt_preparation_errors_and_transfers_token_to_worker() {
        use std::io::{BufRead, Write};
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;
        use std::time::Duration;

        struct PreparedAgent(mpsc::Sender<(thread::ThreadId, thread::ThreadId, String)>);
        impl Agent for PreparedAgent {
            fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
                MockAgent.initialize(req)
            }
            fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
                MockAgent.new_session(req)
            }
            fn prepare_prompt(
                &self,
                req: &PromptRequest,
            ) -> Result<Option<Box<dyn std::any::Any + Send>>, RpcError> {
                if req.session_id == "reject" {
                    return Err(RpcError::new(-32001, "preparation rejected"));
                }
                Ok(Some(Box::new((
                    thread::current().id(),
                    req.session_id.clone(),
                ))))
            }
            fn prompt_prepared(
                &self,
                _: PromptRequest,
                _: &PromptContext,
                prepared: Option<Box<dyn std::any::Any + Send>>,
            ) -> Result<PromptResponse, RpcError> {
                let (dispatch, session) = *prepared
                    .unwrap()
                    .downcast::<(thread::ThreadId, String)>()
                    .unwrap();
                self.0
                    .send((dispatch, thread::current().id(), session))
                    .unwrap();
                Ok(PromptResponse {
                    stop_reason: StopReason::EndTurn,
                })
            }
            fn prompt(
                &self,
                _: PromptRequest,
                _: &PromptContext,
            ) -> Result<PromptResponse, RpcError> {
                panic!("prepared prompt must be used")
            }
            fn cancel(&self, _: &str) {}
        }

        let (server_io, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let server_read = server_io.try_clone().unwrap();
        let (tx, rx) = mpsc::channel();
        let server = thread::spawn(move || {
            serve_agent(BufReader::new(server_read), server_io, |_| {
                PreparedAgent(tx)
            })
        });
        let mut reader = BufReader::new(client.try_clone().unwrap());
        for (id, session) in [(1, "reject"), (2, "accept")] {
            writeln!(client, "{}", serde_json::json!({"jsonrpc":"2.0", "id":id, "method":"session/prompt", "params":{"sessionId":session, "prompt":[]}})).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let response: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["id"], id);
            if id == 1 {
                assert_eq!(response["error"]["message"], "preparation rejected");
                assert!(rx.try_recv().is_err());
            } else {
                assert_eq!(response["result"]["stopReason"], "end_turn");
            }
        }
        let (dispatch, worker, session) = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_ne!(dispatch, worker);
        assert_eq!(session, "accept");
        client.shutdown(std::net::Shutdown::Both).unwrap();
        server.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn prompt_limit_rejects_without_starting_and_cancellation_stays_responsive() {
        use std::io::{BufRead, Write};
        use std::os::unix::net::UnixStream;
        use std::sync::{Condvar, mpsc};
        use std::time::Duration;

        struct BlockingAgent {
            entered: mpsc::Sender<()>,
            cancelled: mpsc::Sender<()>,
            gate: Arc<(Mutex<bool>, Condvar)>,
        }
        impl Agent for BlockingAgent {
            fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
                MockAgent.initialize(req)
            }
            fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
                MockAgent.new_session(req)
            }
            fn prompt(
                &self,
                _: PromptRequest,
                ctx: &PromptContext,
            ) -> Result<PromptResponse, RpcError> {
                self.entered.send(()).unwrap();
                let (lock, wake) = &*self.gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
                let started = ctx.start(|| ());
                Ok(PromptResponse {
                    stop_reason: if started.is_some() {
                        StopReason::EndTurn
                    } else {
                        StopReason::Cancelled
                    },
                })
            }
            fn cancel(&self, _: &str) {
                self.cancelled.send(()).unwrap();
            }
        }
        let (server_io, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let server_read = server_io.try_clone().unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let server_gate = Arc::clone(&gate);
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let result = serve_agent(BufReader::new(server_read), server_io, |_| BlockingAgent {
                entered: entered_tx,
                cancelled: cancel_tx,
                gate: server_gate,
            });
            done_tx.send(result).unwrap();
        });
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let send_prompt = |client: &mut UnixStream, id: usize| {
            writeln!(client, "{}", serde_json::json!({"jsonrpc":"2.0", "id":id, "method":"session/prompt", "params":{"sessionId":"s", "prompt":[]}})).unwrap();
        };
        for id in 0..MAX_PROMPT_WORKERS {
            send_prompt(&mut client, id);
        }
        for _ in 0..MAX_PROMPT_WORKERS {
            entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        send_prompt(&mut client, MAX_PROMPT_WORKERS);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], MAX_PROMPT_WORKERS);
        assert_eq!(response["error"]["code"], -32000);
        assert_eq!(response["error"]["message"], "prompt worker limit reached");
        assert!(entered_rx.try_recv().is_err());
        writeln!(client, "{}", serde_json::json!({"jsonrpc":"2.0", "method":"session/cancel", "params":{"sessionId":"s"}})).unwrap();
        cancel_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        for _ in 0..MAX_PROMPT_WORKERS {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let response: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["result"]["stopReason"], "cancelled");
        }
        // Response bytes can be read just before the worker drops its permit.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            assert!(std::time::Instant::now() < deadline);
            send_prompt(&mut client, MAX_PROMPT_WORKERS + 1);
            line.clear();
            reader.read_line(&mut line).unwrap();
            let response: serde_json::Value = serde_json::from_str(&line).unwrap();
            if response.get("result").is_some() {
                assert_eq!(response["result"]["stopReason"], "end_turn");
                break;
            }
            thread::yield_now();
        }
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        client.shutdown(std::net::Shutdown::Both).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
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

    struct DisconnectAgent {
        events: std::sync::mpsc::Sender<&'static str>,
        unblock: std::sync::mpsc::Sender<()>,
        blocked: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl Agent for DisconnectAgent {
        fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
            MockAgent.initialize(req)
        }

        fn new_session(&self, _req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            self.events.send("started").unwrap();
            kage_core::sync::lock(&self.blocked).recv().unwrap();
            self.events.send("finished").unwrap();
            Err(RpcError::new(-32000, "interrupted"))
        }

        fn prompt(
            &self,
            _req: PromptRequest,
            _ctx: &PromptContext,
        ) -> Result<PromptResponse, RpcError> {
            unreachable!("no prompt is sent")
        }

        fn cancel(&self, _session_id: &str) {}

        fn disconnecting(&self) {
            self.events.send("disconnecting").unwrap();
            self.unblock.send(()).unwrap();
        }

        fn detached(&self) {
            self.events.send("detached").unwrap();
        }
    }

    #[test]
    fn disconnecting_unblocks_request_before_detached() {
        use std::io::Read as _;
        use std::sync::mpsc;
        use std::time::Duration;

        let (srv_r, mut cli_w) = std::io::pipe().unwrap();
        let (mut cli_r, srv_w) = std::io::pipe().unwrap();
        let (events, observed) = mpsc::channel();
        let (unblock, blocked) = mpsc::channel();
        let agent = DisconnectAgent {
            events,
            unblock: unblock.clone(),
            blocked: std::sync::Mutex::new(blocked),
        };
        let server = thread::spawn(move || serve_agent(BufReader::new(srv_r), srv_w, |_| agent));
        writeln!(
            cli_w,
            r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{{"cwd":"/tmp"}}}}"#
        )
        .unwrap();
        let timeout = Duration::from_secs(5);
        assert_eq!(observed.recv_timeout(timeout).unwrap(), "started");
        drop(cli_w);
        let first = observed.recv_timeout(timeout);
        if first.is_err() {
            let _ = unblock.send(());
        }
        assert_eq!(first.unwrap(), "disconnecting");
        assert_eq!(observed.recv_timeout(timeout).unwrap(), "finished");
        assert_eq!(observed.recv_timeout(timeout).unwrap(), "detached");
        server.join().unwrap().unwrap();

        let mut out = String::new();
        cli_r.read_to_string(&mut out).unwrap();
        assert!(out.contains("interrupted"), "{out}");
    }

    #[derive(Clone)]
    struct ReceiptWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        fail: bool,
    }

    impl Write for ReceiptWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail {
                return Err(std::io::Error::other("failed writer"));
            }
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct DropSignal(std::sync::mpsc::Sender<&'static str>);
    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.send("dropped").unwrap();
        }
    }

    struct ReceiptAgent {
        writer: ReceiptWriter,
        events: std::sync::mpsc::Sender<String>,
        drops: std::sync::mpsc::Sender<&'static str>,
        receipts: bool,
        barrier: Option<Arc<std::sync::Barrier>>,
    }

    impl ReceiptAgent {
        fn receipt(&self, label: String) -> Option<OpenReceipt> {
            if !self.receipts {
                return None;
            }
            let writer = self.writer.clone();
            let events = self.events.clone();
            let guard = DropSignal(self.drops.clone());
            Some(Box::new(move || {
                let _guard = guard;
                let bytes = writer.bytes.lock().unwrap();
                let lines = String::from_utf8(bytes.clone()).unwrap();
                let expected_id: i64 = label.parse().unwrap();
                assert!(
                    lines.lines().any(|line| {
                        let response: serde_json::Value = serde_json::from_str(line).unwrap();
                        response["id"] == expected_id && response.get("result").is_some()
                    }),
                    "receipt before response: {lines}"
                );
                events.send(label).unwrap();
            }))
        }
    }

    impl Agent for ReceiptAgent {
        fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
            MockAgent.initialize(req)
        }
        fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
            MockAgent.new_session(req)
        }
        fn load_session(
            &self,
            req: LoadSessionRequest,
            ctx: &PromptContext,
        ) -> Result<LoadSessionResponse, RpcError> {
            LoadAgent.load_session(req, ctx)
        }
        fn resume_session(
            &self,
            req: ResumeSessionRequest,
            ctx: &PromptContext,
        ) -> Result<ResumeSessionResponse, RpcError> {
            LoadAgent.resume_session(req, ctx)
        }
        fn new_session_with_receipt(
            &self,
            req: NewSessionRequest,
        ) -> OpenResult<NewSessionResponse> {
            let label = req.cwd.clone();
            if label == "error" {
                let _receipt = self.receipt("9".into());
                return Err(RpcError::internal("open failed"));
            }
            let response = self.new_session(req)?;
            let receipt = self.receipt(label);
            if let Some(barrier) = &self.barrier {
                barrier.wait();
            }
            Ok((response, receipt))
        }
        fn load_session_with_receipt(
            &self,
            req: LoadSessionRequest,
            ctx: &PromptContext,
        ) -> OpenResult<LoadSessionResponse> {
            let label = req.session_id.clone();
            self.load_session(req, ctx)
                .map(|response| (response, self.receipt(label)))
        }
        fn resume_session_with_receipt(
            &self,
            req: ResumeSessionRequest,
            ctx: &PromptContext,
        ) -> OpenResult<ResumeSessionResponse> {
            let label = req.session_id.clone();
            self.resume_session(req, ctx)
                .map(|response| (response, self.receipt(label)))
        }
        fn prompt(&self, _: PromptRequest, _: &PromptContext) -> Result<PromptResponse, RpcError> {
            unreachable!()
        }
        fn cancel(&self, _: &str) {}
        fn session_announced(&self, session: &str) {
            self.events.send(format!("default:{session}")).unwrap();
        }
    }

    fn open_input() -> String {
        [
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"1"}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"2","cwd":"/","mcpServers":[]}}),
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"session/resume","params":{"sessionId":"3","cwd":"/","mcpServers":[]}}),
        ].iter().fold(String::new(), |mut output, value| {
            use std::fmt::Write as _;
            writeln!(output, "{value}").unwrap();
            output
        })
    }

    #[test]
    fn open_receipts_follow_responses_for_all_wrappers() {
        let writer = ReceiptWriter {
            bytes: Arc::default(),
            fail: false,
        };
        let (events, received) = std::sync::mpsc::channel();
        let (drops, dropped) = std::sync::mpsc::channel();
        let agent = ReceiptAgent {
            writer: writer.clone(),
            events,
            drops,
            receipts: true,
            barrier: None,
        };
        serve_agent(
            BufReader::new(std::io::Cursor::new(open_input())),
            writer.clone(),
            |_| agent,
        )
        .unwrap();
        let mut observed: Vec<_> = received.try_iter().collect();
        observed.sort();
        assert_eq!(observed, ["1", "2", "3"]);
        assert_eq!(dropped.try_iter().count(), 3);
        let lines = String::from_utf8(writer.bytes.lock().unwrap().clone()).unwrap();
        let messages: Vec<serde_json::Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let history = messages
            .iter()
            .position(|m| m["method"] == "session/update")
            .unwrap();
        let response = messages.iter().position(|m| m["id"] == 2).unwrap();
        assert!(history < response);
    }

    #[test]
    fn open_encoding_failure_returns_error_without_announcement() {
        struct FailingSerialize;
        impl serde::Serialize for FailingSerialize {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("deliberate failure"))
            }
        }

        for with_receipt in [true, false] {
            let writer = ReceiptWriter {
                bytes: Arc::default(),
                fail: false,
            };
            let (events, received) = std::sync::mpsc::channel();
            let (drops, dropped) = std::sync::mpsc::channel();
            let agent = Arc::new(ReceiptAgent {
                writer: writer.clone(),
                events,
                drops,
                receipts: with_receipt,
                barrier: None,
            });
            let (peer, _inbound, reader) = connect(
                BufReader::new(std::io::Cursor::new(Vec::new())),
                writer.clone(),
            );
            let peer = AgentPeer::from(peer);
            let worker = spawn_open(&peer, &agent, serde_json::json!(9), |a| {
                Ok(("session".into(), FailingSerialize, a.receipt("9".into())))
            });
            worker.join().unwrap();
            reader.join().unwrap();
            assert!(received.try_iter().next().is_none());
            assert_eq!(dropped.try_iter().count(), usize::from(with_receipt));
            let bytes = writer.bytes.lock().unwrap();
            let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(response["id"], 9);
            assert_eq!(response["error"]["code"], -32603);
            assert_eq!(
                response["error"]["message"],
                "encode open response: deliberate failure"
            );
            assert!(response.get("result").is_none());
        }
    }

    #[test]
    fn open_receipt_is_dropped_on_failed_response_or_open() {
        for (fail, input) in [(true, "1"), (false, "error")] {
            let writer = ReceiptWriter {
                bytes: Arc::default(),
                fail,
            };
            let (events, received) = std::sync::mpsc::channel();
            let (drops, dropped) = std::sync::mpsc::channel();
            let agent = ReceiptAgent {
                writer: writer.clone(),
                events,
                drops,
                receipts: true,
                barrier: None,
            };
            let request = format!(
                "{}\n",
                serde_json::json!({"jsonrpc":"2.0","id":9,"method":"session/new","params":{"cwd":input}})
            );
            serve_agent(
                BufReader::new(std::io::Cursor::new(request)),
                writer,
                |_| agent,
            )
            .unwrap();
            assert!(received.try_iter().next().is_none());
            assert_eq!(dropped.try_iter().collect::<Vec<_>>(), ["dropped"]);
        }
    }

    #[test]
    fn concurrent_opens_keep_independent_receipts_for_same_session() {
        let writer = ReceiptWriter {
            bytes: Arc::default(),
            fail: false,
        };
        let (events, received) = std::sync::mpsc::channel();
        let (drops, dropped) = std::sync::mpsc::channel();
        let agent = ReceiptAgent {
            writer: writer.clone(),
            events,
            drops,
            receipts: true,
            barrier: Some(Arc::new(std::sync::Barrier::new(2))),
        };
        let input = (1..=2).fold(String::new(), |mut output, id| {
            use std::fmt::Write as _;
            writeln!(output, "{}", serde_json::json!({"jsonrpc":"2.0","id":id,"method":"session/new","params":{"cwd":id.to_string()}})).unwrap();
            output
        });
        serve_agent(BufReader::new(std::io::Cursor::new(input)), writer, |_| {
            agent
        })
        .unwrap();
        let mut observed: Vec<_> = received.try_iter().collect();
        observed.sort();
        assert_eq!(observed, ["1", "2"]);
        assert_eq!(dropped.try_iter().count(), 2);
    }

    #[test]
    fn default_open_wrappers_announce_each_session() {
        struct DefaultAgent(std::sync::mpsc::Sender<String>);
        impl Agent for DefaultAgent {
            fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
                MockAgent.initialize(req)
            }
            fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
                MockAgent.new_session(req)
            }
            fn load_session(
                &self,
                req: LoadSessionRequest,
                ctx: &PromptContext,
            ) -> Result<LoadSessionResponse, RpcError> {
                LoadAgent.load_session(req, ctx)
            }
            fn resume_session(
                &self,
                req: ResumeSessionRequest,
                ctx: &PromptContext,
            ) -> Result<ResumeSessionResponse, RpcError> {
                LoadAgent.resume_session(req, ctx)
            }
            fn prompt(
                &self,
                _: PromptRequest,
                _: &PromptContext,
            ) -> Result<PromptResponse, RpcError> {
                unreachable!()
            }
            fn cancel(&self, _: &str) {}
            fn session_announced(&self, session: &str) {
                self.0.send(session.to_owned()).unwrap();
            }
        }
        let writer = ReceiptWriter {
            bytes: Arc::default(),
            fail: false,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        serve_agent(
            BufReader::new(std::io::Cursor::new(open_input())),
            writer,
            |_| DefaultAgent(tx),
        )
        .unwrap();
        let mut observed: Vec<_> = rx.try_iter().collect();
        observed.sort();
        assert_eq!(observed, ["2", "3", "sess-1"]);
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
            }))?;
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

    #[test]
    fn cancellation_before_registration_is_not_lost_or_inherited() {
        use std::sync::mpsc;
        use std::time::Duration;

        struct DelayedAgent {
            entered: mpsc::Sender<()>,
            proceed: Mutex<mpsc::Receiver<()>>,
            cancelled: mpsc::Sender<String>,
        }
        impl Agent for DelayedAgent {
            fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
                MockAgent.initialize(req)
            }
            fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
                MockAgent.new_session(req)
            }
            fn prompt(
                &self,
                _: PromptRequest,
                ctx: &PromptContext,
            ) -> Result<PromptResponse, RpcError> {
                self.entered.send(()).unwrap();
                self.proceed.lock().unwrap().recv().unwrap();
                Ok(PromptResponse {
                    stop_reason: if ctx.start(|| ()).is_some() {
                        StopReason::EndTurn
                    } else {
                        StopReason::Cancelled
                    },
                })
            }
            fn cancel(&self, session: &str) {
                self.cancelled.send(session.to_owned()).unwrap();
            }
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (proceed_tx, proceed_rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || {
            serve_agent(BufReader::new(srv_r), srv_w, |_| DelayedAgent {
                entered: entered_tx,
                proceed: Mutex::new(proceed_rx),
                cancelled: cancel_tx,
            })
        });
        let (client, inbox, _) = connect(BufReader::new(cli_r), cli_w);
        let timeout = Duration::from_secs(5);
        let launch = |session: &'static str| {
            let client = client.clone();
            thread::spawn(move || {
                client
                    .request(
                        "session/prompt",
                        serde_json::json!({"sessionId": session, "prompt": []}),
                    )
                    .unwrap()
            })
        };
        let first = launch("a");
        entered_rx.recv_timeout(timeout).unwrap();
        client
            .notify("session/cancel", serde_json::json!({"sessionId": "a"}))
            .unwrap();
        assert_eq!(cancel_rx.recv_timeout(timeout).unwrap(), "a");
        proceed_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap()["stopReason"], "cancelled");

        let second = launch("a");
        entered_rx.recv_timeout(timeout).unwrap();
        client
            .notify("session/cancel", serde_json::json!({"sessionId": "b"}))
            .unwrap();
        assert_eq!(cancel_rx.recv_timeout(timeout).unwrap(), "b");
        proceed_tx.send(()).unwrap();
        assert_eq!(second.join().unwrap()["stopReason"], "end_turn");
        drop(client);
        drop(inbox);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn cancellation_after_registration_is_forwarded() {
        use std::sync::mpsc;
        use std::time::Duration;
        struct RegisteredAgent {
            registered: mpsc::Sender<()>,
            cancelled: Mutex<mpsc::Receiver<()>>,
            cancel: mpsc::Sender<()>,
        }
        impl Agent for RegisteredAgent {
            fn initialize(&self, req: InitializeRequest) -> InitializeResponse {
                MockAgent.initialize(req)
            }
            fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, RpcError> {
                MockAgent.new_session(req)
            }
            fn prompt(
                &self,
                _: PromptRequest,
                ctx: &PromptContext,
            ) -> Result<PromptResponse, RpcError> {
                ctx.start(|| self.registered.send(()).unwrap()).unwrap();
                self.cancelled
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                Ok(PromptResponse {
                    stop_reason: StopReason::Cancelled,
                })
            }
            fn cancel(&self, _: &str) {
                self.cancel.send(()).unwrap();
            }
        }
        let (registered_tx, registered_rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || {
            serve_agent(BufReader::new(srv_r), srv_w, |_| RegisteredAgent {
                registered: registered_tx,
                cancelled: Mutex::new(cancel_rx),
                cancel: cancel_tx,
            })
        });
        let (client, inbox, _) = connect(BufReader::new(cli_r), cli_w);
        let prompt = {
            let client = client.clone();
            thread::spawn(move || {
                client
                    .request(
                        "session/prompt",
                        serde_json::json!({"sessionId": "a", "prompt": []}),
                    )
                    .unwrap()
            })
        };
        registered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        client
            .notify("session/cancel", serde_json::json!({"sessionId": "a"}))
            .unwrap();
        assert_eq!(prompt.join().unwrap()["stopReason"], "cancelled");
        drop(client);
        drop(inbox);
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
    #[cfg(unix)]
    #[test]
    fn unix_server_dispatches_requests() {
        let (srv_r, cli_w) = std::io::pipe().unwrap();
        let (cli_r, srv_w) = std::io::pipe().unwrap();
        let server = thread::spawn(move || {
            serve_agent_unix(
                BufReader::new(srv_r),
                srv_w.into(),
                std::time::Duration::from_secs(1),
                |_| MockAgent,
            )
        });
        let (client, _inbox, _h) = connect(BufReader::new(cli_r), cli_w);
        assert_eq!(
            client
                .request("bogus/method", serde_json::Value::Null)
                .unwrap_err()
                .code,
            -32601
        );
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_connected_server_borrows_receiver_and_drains_eof() {
        use std::io::Read as _;
        let (srv_r, mut cli_w) = std::io::pipe().unwrap();
        let (mut cli_r, srv_w) = std::io::pipe().unwrap();
        let (peer, inbound, reader) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(srv_r), srv_w.into())
                .unwrap();
        writeln!(cli_w, r#"{{"jsonrpc":"2.0","id":1,"method":"unknown"}}"#).unwrap();
        drop(cli_w);
        serve_agent_unix_connected(peer, &inbound, std::time::Duration::from_secs(1), |_| {
            MockAgent
        })
        .unwrap();
        reader.join().unwrap();
        assert!(matches!(
            inbound.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        let mut response = String::new();
        cli_r.read_to_string(&mut response).unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["error"]["code"], -32601);
    }

    #[cfg(unix)]
    #[test]
    fn unix_blocked_write_is_returned_and_recorded() {
        let (reader, input) = std::io::pipe().unwrap();
        let (_output, writer) = std::io::pipe().unwrap();
        let (unix, _inbound, _reader) =
            kage_jsonrpc::unix_peer::connect_ordered(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, std::time::Duration::from_millis(30));
        let error = peer
            .notify("large", serde_json::json!({"data": "x".repeat(128 * 1024)}))
            .unwrap_err();
        assert_eq!(error.code, -32000);
        assert_eq!(peer.failure(), Some(error));
        assert_eq!(
            peer.notify("after-failure", serde_json::json!({}))
                .unwrap_err()
                .message,
            "connection closed"
        );
        drop(input);
    }

    #[cfg(unix)]
    #[test]
    fn unix_cancelled_committed_request_sends_notice() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, _inbound, _reader) =
            kage_jsonrpc::unix_peer::connect_ordered(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, std::time::Duration::from_secs(1));
        let (client, inbox, _reader) = connect(BufReader::new(output), input);
        let cancel = CancelFlag::new();
        let flag = cancel.clone();
        let task =
            thread::spawn(move || peer.request_cancellable("ask", serde_json::json!({}), &flag));
        let Inbound::Request { id, .. } = inbox
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
        else {
            panic!("expected request")
        };
        cancel.cancel();
        match inbox
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
        {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "$/cancel_request");
                assert_eq!(params["requestId"], id);
            }
            Inbound::Request { .. } => panic!("expected cancellation"),
        }
        assert_eq!(task.join().unwrap().unwrap_err().code, -32800);
        drop(client);
    }
    #[cfg(unix)]
    #[test]
    fn unix_server_exits_after_write_failure_without_input_eof() {
        use std::io::Write as _;
        let (srv_r, mut cli_w) = std::io::pipe().unwrap();
        let (_cli_r, srv_w) = std::io::pipe().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let result = serve_agent_unix(
                BufReader::new(srv_r),
                srv_w.into(),
                std::time::Duration::ZERO,
                |_| MockAgent,
            );
            done_tx.send(result).unwrap();
        });
        cli_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"unknown\"}\n")
            .unwrap();
        let error = done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, -32000);
        drop(cli_w);
    }
    #[cfg(unix)]
    #[test]
    fn unix_half_closed_input_still_answers_outstanding_open() {
        use std::io::Read as _;
        struct AnnouncingAgent(AgentPeer);
        impl Agent for AnnouncingAgent {
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
                unreachable!()
            }
            fn cancel(&self, _session_id: &str) {}
            fn session_announced(&self, session_id: &str) {
                self.0
                    .notify(
                        "session/update",
                        serde_json::json!({"sessionId": session_id}),
                    )
                    .unwrap();
            }
        }
        let (srv_r, mut cli_w) = std::io::pipe().unwrap();
        let (mut cli_r, srv_w) = std::io::pipe().unwrap();
        writeln!(
            cli_w,
            r#"{{"jsonrpc":"2.0","id":1,"method":"session/new","params":{{"cwd":"/tmp"}}}}"#
        )
        .unwrap();
        drop(cli_w);
        serve_agent_unix(
            BufReader::new(srv_r),
            srv_w.into(),
            std::time::Duration::from_secs(1),
            AnnouncingAgent,
        )
        .unwrap();
        let mut out = String::new();
        cli_r.read_to_string(&mut out).unwrap();
        let messages: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["result"]["sessionId"], "sess-1");
        assert_eq!(messages[1]["method"], "session/update");
        assert_eq!(messages[1]["params"]["sessionId"], "sess-1");
    }

    #[cfg(unix)]
    #[test]
    fn unix_approval_waits_for_prior_inbound_work() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, _reader) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, std::time::Duration::from_secs(1));
        let (client, inbox, _reader) = connect(BufReader::new(output), input);
        let cancel = CancelFlag::new();
        let flag = cancel.clone();
        let task =
            thread::spawn(move || peer.request_cancellable("ask", serde_json::json!({}), &flag));
        let Inbound::Request { id, .. } = inbox
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
        else {
            panic!("expected request")
        };
        client
            .notify("session/cancel", serde_json::json!({}))
            .unwrap();
        client
            .respond(&id, Ok(serde_json::json!({"approved": true})))
            .unwrap();
        assert!(matches!(
            inbound
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            kage_jsonrpc::OrderedInbound::Message(_)
        ));
        let kage_jsonrpc::OrderedInbound::Barrier(marker) = inbound
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
        else {
            panic!("expected barrier")
        };
        thread::sleep(std::time::Duration::from_millis(30));
        assert!(!task.is_finished());
        cancel.cancel();
        marker.acknowledge();
        assert_eq!(task.join().unwrap().unwrap_err().code, -32800);
    }

    #[cfg(unix)]
    #[test]
    fn unix_half_closed_request_does_not_close_output() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, inbound, reader_task) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, std::time::Duration::from_secs(1));
        drop(input);
        reader_task.join().unwrap();
        drop(inbound);
        assert!(
            peer.request_cancellable("ask", serde_json::json!({}), &CancelFlag::new())
                .is_err()
        );
        assert!(peer.failure().is_none());
        peer.respond(&serde_json::json!(7), Ok(serde_json::json!(true)))
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut BufReader::new(output), &mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["id"],
            7
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_partial_request_cancellation_records_transport_failure() {
        use std::io::Read as _;
        let (reader, input) = std::io::pipe().unwrap();
        let (mut output, writer) = std::io::pipe().unwrap();
        let (unix, _inbound, _reader) =
            kage_jsonrpc::unix_peer::connect_ordered_server(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, std::time::Duration::from_secs(2));
        let worker_peer = peer.clone();
        let cancel = CancelFlag::new();
        let flag = cancel.clone();
        let task = thread::spawn(move || {
            worker_peer.request_cancellable(
                "ask",
                serde_json::json!({"data": "x".repeat(1024 * 1024)}),
                &flag,
            )
        });
        let mut byte = [0];
        output.read_exact(&mut byte).unwrap();
        cancel.cancel();
        assert_eq!(task.join().unwrap().unwrap_err().code, -32800);
        assert!(peer.failure().is_some());
        drop(input);
    }

    #[cfg(unix)]
    #[test]
    fn unix_server_reports_truncated_input() {
        let (srv_r, mut cli_w) = std::io::pipe().unwrap();
        let (_cli_r, srv_w) = std::io::pipe().unwrap();
        write!(cli_w, "{{\"jsonrpc\":").unwrap();
        drop(cli_w);
        let error = serve_agent_unix(
            BufReader::new(srv_r),
            srv_w.into(),
            std::time::Duration::from_secs(1),
            |_| MockAgent,
        )
        .unwrap_err();
        assert_eq!(error.message, "Unix transport reader failed");
    }

    #[cfg(unix)]
    #[test]
    fn unix_write_failure_settles_pending_requests() {
        let (reader, input) = std::io::pipe().unwrap();
        let (output, writer) = std::io::pipe().unwrap();
        let (unix, _inbound, _reader) =
            kage_jsonrpc::unix_peer::connect_ordered(BufReader::new(reader), writer.into())
                .unwrap();
        let peer = AgentPeer::unix(unix, std::time::Duration::from_millis(50));
        let (client, inbox, _reader) = connect(BufReader::new(output), input);
        let request_peer = peer.clone();
        let task = thread::spawn(move || {
            request_peer.request_cancellable("ask", serde_json::json!({}), &CancelFlag::new())
        });
        assert!(matches!(
            inbox
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            Inbound::Request { .. }
        ));
        // Stall the client reader with a notification larger than its inbound queue.
        // The receiver is intentionally kept alive but is not drained.
        for _ in 0..300 {
            if peer
                .notify("fill", serde_json::json!({"data": "x".repeat(8192)}))
                .is_err()
            {
                break;
            }
        }
        let err = task.join().unwrap().unwrap_err();
        assert_eq!(err.message, "connection closed");
        assert!(peer.failure().is_some());
        drop(client);
    }
}
