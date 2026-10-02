//! The `send_message` tool: drop a message into another session's
//! inbox. Delivery is fire-and-forget: a running target reads the
//! message at its next turn boundary, and the caller keeps going.

use std::sync::mpsc;
use std::time::Duration;

use kage_core::agent_report::AgentMail;
use kage_core::sync::lock;
use kage_core::{Risk, SessionId, ToolOutput};
use kage_tools::{Tool, ToolContext, ToolError};
use serde::Deserialize;
use ulid::Ulid;

use super::Input;

/// Name the model calls the tool by.
pub(super) const MAILBOX_TOOL: &str = "send_message";

/// How long the tool waits for the engine's delivery ack. The engine
/// answers on its own thread, so this only guards against a stopped
/// or wedged engine.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct MailInput {
    /// `parent` or the session id of a live agent session.
    to: String,
    message: String,
}

/// Sends mailbox messages for one session's run. The dispatcher
/// registers it into the run's tools next to the `agent` tool.
#[derive(Debug)]
pub(super) struct MailboxTool {
    from: SessionId,
    engine: mpsc::Sender<Input>,
    description: String,
    schema: serde_json::Value,
}

impl MailboxTool {
    pub(super) fn new(from: SessionId, engine: mpsc::Sender<Input>) -> Self {
        let description = format!(
            "Send a message to another session of this conversation and continue without \
             waiting. A running target reads it at its next turn boundary. The answer is \
             not this call's result: an agent you started covers it in its own result, \
             and any other session can message you back.\n\n\
             `to` is `parent` (the session that started you) or the session id of a \
             running agent, such as one you started in the background or a sibling still \
             at work. An agent that has finished takes no messages; start a new agent \
             instead. Your own id is {from}.\n\n\
             Use it to steer an agent you started, hand findings to a sibling or ask the \
             parent a question mid-task. For work you must wait on, make an `agent` call \
             instead."
        );
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "`parent` or the session id of a running agent."
                },
                "message": {
                    "type": "string",
                    "description": "What to send. Self-contained: the target reads it without your context."
                }
            },
            "required": ["to", "message"]
        });
        Self {
            from,
            engine,
            description,
            schema,
        }
    }
}

impl Tool for MailboxTool {
    fn name(&self) -> &str {
        MAILBOX_TOOL
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn risk(&self) -> Risk {
        Risk::Exec
    }

    fn execute(
        &self,
        input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: MailInput = serde_json::from_value(input)?;
        if input.message.trim().is_empty() {
            return Ok(super::agent_tool::error_output(
                "the message is empty".to_owned(),
            ));
        }
        let to = if input.to.trim() == "parent" {
            None
        } else {
            match Ulid::from_string(input.to.trim()) {
                Ok(id) => Some(SessionId(id)),
                Err(_) => {
                    return Ok(super::agent_tool::error_output(format!(
                        "`to` must be `parent` or a session id, got `{}`",
                        input.to
                    )));
                }
            }
        };
        let (reply, ack) = crossbeam_channel::bounded(1);
        if self
            .engine
            .send(Input::Deliver {
                from: self.from,
                to,
                message: input.message,
                reply,
            })
            .is_err()
        {
            return Ok(super::agent_tool::error_output(
                "the engine stopped".to_owned(),
            ));
        }
        match ack.recv_timeout(ACK_TIMEOUT) {
            Ok(Ok(text)) => Ok(ToolOutput {
                text,
                ..ToolOutput::default()
            }),
            Ok(Err(text)) => Ok(super::agent_tool::error_output(text)),
            Err(_) => Ok(super::agent_tool::error_output(
                "the engine did not answer the delivery".to_owned(),
            )),
        }
    }
}

impl super::Dispatcher {
    /// Resolve one message and leave it in the target's inbox. `None`
    /// targets address the sender's parent, and a target must share the
    /// sender's main session and still be running: the main session or
    /// an agent whose result is not delivered yet. The message travels
    /// as an [`AgentMail`], so the target knows who sent it. A busy
    /// target reads it at its next turn boundary; an idle one wakes to
    /// read it, unless it is a main session that holds agent text for
    /// its next prompt.
    pub(super) fn deliver_message(
        &mut self,
        from: SessionId,
        to: Option<SessionId>,
        message: &str,
    ) -> Result<String, String> {
        let to = match to {
            Some(to) => to,
            None => self
                .parent_of(from)
                .ok_or_else(|| "this session has no parent to message".to_owned())?,
        };
        if to == from {
            return Err("cannot send a message to yourself".to_owned());
        }
        let sender = self
            .sessions
            .get(&from)
            .and_then(|s| s.link.as_ref())
            .map_or_else(|| "kage".to_owned(), |l| l.agent.clone());
        // A finished agent already delivered its result, so a reply to
        // a later message, and anything it changed, would never reach
        // its parent.
        let finished = self.sessions.get(&to).is_none_or(|target| {
            target
                .link
                .as_ref()
                .is_some_and(|link| link.report.is_none())
        });
        if finished {
            return Err(format!(
                "session {to} is not running. A finished agent takes no messages; start a \
                 new agent, or continue a swarm child with a swarm resume"
            ));
        }
        if self.root_of(to) != self.root_of(from) {
            return Err(format!(
                "session {to} belongs to another conversation; message sessions of your own"
            ));
        }
        let mail = AgentMail {
            from: sender,
            session: from,
            body: message.to_owned(),
        };
        let target = &self.sessions[&to];
        lock(&target.inbox).push_back(mail.to_text());
        if target.idle.is_none() {
            return Ok(format!(
                "Delivered to session {to}. It reads the message at its next turn boundary."
            ));
        }
        if self.waiting.contains(&to) {
            return Ok(format!(
                "Delivered to session {to}. It reads the message when it starts."
            ));
        }
        Ok(if self.wake(to) {
            format!("Delivered to session {to}. It starts a run to read the message.")
        } else {
            format!("Delivered to session {to}. It reads the message with its next prompt.")
        })
    }

    /// The main session `id` hangs under, or `id` itself.
    fn root_of(&self, mut id: SessionId) -> SessionId {
        while let Some(parent) = self.parent_of(id) {
            id = parent;
        }
        id
    }
}
