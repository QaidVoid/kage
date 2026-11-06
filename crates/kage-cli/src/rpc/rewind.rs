//! Undo, rewind and the prompts they target, withdrawing a pending prompt,
//! and deleting a recorded session: the session operations the engine
//! answers with an event.

use std::sync::mpsc;
use std::time::Duration;

use kage_acp::acp::{
    ContentBlock, PastPrompt, PromptDelivery, PromptWithdrawn, SessionPromptsResponse,
    SessionRewindRequest, SessionUndoResponse, WithdrawRequest,
};
use kage_core::protocol::{Command, CommandKind, Delivery, Event, HostEvent, NoticeLevel};
use kage_core::sync::lock;
use kage_core::{Content, SessionId};
use kage_jsonrpc::RpcError;

use super::CliAcpAgent;
use super::content::source_block;

// How long a request waits for the engine to answer its command.
const ENGINE_ANSWER: Duration = Duration::from_secs(30);

impl CliAcpAgent {
    /// Drops the session's last prompt and what followed it, answering
    /// with the dropped prompt.
    pub(super) fn undo(&self, client_id: &str) -> Result<SessionUndoResponse, RpcError> {
        let id = self.engine_id(client_id)?;
        let prompt = self.ask_engine(id, CommandKind::Undo, |event| match event {
            HostEvent::Undone { prompt } => Some(prompt.clone().unwrap_or_default()),
            _ => None,
        })?;
        Ok(SessionUndoResponse {
            prompt: blocks(&prompt),
        })
    }

    /// Continues the session from the end of the turn of the prompt
    /// `req.at` names.
    pub(super) fn rewind(&self, req: SessionRewindRequest) -> Result<serde_json::Value, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        self.ask_engine(id, CommandKind::Rewind { at: req.at }, |event| {
            matches!(event, HostEvent::Undone { .. }).then_some(())
        })?;
        Ok(serde_json::json!({}))
    }

    /// Every prompt of the session's branch tree, oldest first.
    pub(super) fn prompts(&self, client_id: &str) -> Result<SessionPromptsResponse, RpcError> {
        let id = self.engine_id(client_id)?;
        let prompts = self.ask_engine(id, CommandKind::ListPrompts, |event| match event {
            HostEvent::PromptList { prompts } => Some(prompts.clone()),
            _ => None,
        })?;
        Ok(SessionPromptsResponse {
            prompts: prompts
                .into_iter()
                .map(|prompt| PastPrompt {
                    at: prompt.at,
                    prompt: blocks(&prompt.prompt),
                    depth: prompt.depth,
                    active: prompt.active,
                })
                .collect(),
        })
    }

    /// Takes back the session's newest pending prompt of `req.delivery`.
    pub(super) fn withdraw(&self, req: &WithdrawRequest) -> Result<PromptWithdrawn, RpcError> {
        let id = self.engine_id(&req.session_id)?;
        let delivery = match req.delivery {
            PromptDelivery::Steer => Delivery::Steer,
            PromptDelivery::Queue => Delivery::Queue,
        };
        let content = self.ask_engine(
            id,
            CommandKind::WithdrawPrompt { delivery },
            move |event| match event {
                HostEvent::PromptWithdrawn {
                    delivery: from,
                    content,
                } if *from == delivery => Some(content.clone().unwrap_or_default()),
                _ => None,
            },
        )?;
        Ok(PromptWithdrawn {
            delivery: req.delivery,
            content: blocks(&content),
        })
    }

    /// Deletes the recorded session `client_id` names in full, which must
    /// not be open, closing, running, an agent of a live session, or held
    /// by any other kage process.
    pub(super) fn delete(&self, client_id: &str) -> Result<serde_json::Value, RpcError> {
        let id = client_id
            .parse::<ulid::Ulid>()
            .map(SessionId)
            .map_err(|_| RpcError::new(-32602, format!("unknown session {client_id}")))?;
        let path = self.recorded_path(client_id)?;
        if path.file_stem().and_then(|stem| stem.to_str()) != Some(client_id) {
            return Err(RpcError::new(
                -32602,
                format!("unknown session {client_id}"),
            ));
        }
        let in_use = self.host.workdir(id).is_some()
            || lock(&self.host.live).in_use(id)
            || kage_session::is_locked(&path);
        if in_use {
            return Err(RpcError::new(-32602, "the session is in use"));
        }
        std::fs::remove_file(&path)
            .map_err(|e| RpcError::internal(format!("{}: {e}", path.display())))?;
        Ok(serde_json::json!({}))
    }

    /// Sends `kind` to engine session `id` and waits for the first event
    /// `answer` accepts among those the engine publishes while handling
    /// this very command. A warning or error notice among them is the
    /// command's refusal; events from anything else are never taken.
    fn ask_engine<T: Send + 'static>(
        &self,
        id: SessionId,
        kind: CommandKind,
        answer: impl Fn(&HostEvent) -> Option<T> + Send + 'static,
    ) -> Result<T, RpcError> {
        let command = self.host.command_id();
        let (tx, rx) = mpsc::channel();
        let subscription = self.host.engine.subscribe(Box::new(move |envelope| {
            if crate::engine::answering() != Some(command) {
                return;
            }
            let Event::Host(event) = &envelope.event else {
                return;
            };
            let reply = match event {
                HostEvent::Notice {
                    level: NoticeLevel::Warning | NoticeLevel::Error,
                    text,
                    ..
                } => Some(Err(text.clone())),
                event => answer(event).map(Ok),
            };
            if let Some(reply) = reply {
                let _ = tx.send(reply);
            }
        }));
        self.host
            .engine
            .send(Command::to(id, kind).with_id(command));
        let reply = rx.recv_timeout(ENGINE_ANSWER);
        self.host.engine.unsubscribe(subscription);
        match reply {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(refused)) => Err(RpcError::new(-32603, refused)),
            Err(_) => Err(RpcError::internal("the engine did not answer")),
        }
    }
}

/// A prompt's text, images and clips as ACP blocks.
pub(super) fn blocks(content: &[Content]) -> Vec<ContentBlock> {
    content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(ContentBlock::text(text)),
            Content::Image { source, mime } => {
                Some(source_block(source, mime, ContentBlock::Image))
            }
            Content::Audio { source, mime } => {
                Some(source_block(source, mime, ContentBlock::Audio))
            }
            _ => None,
        })
        .collect()
}
