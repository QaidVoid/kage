//! The `send_message` tool: drop a message into another agent
//! session's mailbox. Delivery is fire-and-forget: the target runs the
//! message as its next prompt, and the caller keeps going.

use std::sync::mpsc;
use std::time::Duration;

use kage_core::protocol::Delivery;
use kage_core::{Content, Risk, SessionId, ToolOutput};
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
            "Send a message to another agent session's mailbox and continue without \
             waiting. The message becomes the target's next prompt: it runs at once \
             when the target is idle and the agent running limit allows, else after \
             its current run ends or a slot frees. The target's reply never comes back \
             to you; it lands in the target's own transcript, so ask it to report back \
             another way, such as a follow-up `swarm` resume or a message of its own.\n\n\
             `to` is `parent` (the session that started you) or the id of another \
             running session of this conversation, for example a sibling still at \
             work; an agent that has finished takes no messages. Your own id is {from}.\n\n\
             Use it to hand findings to a sibling, ask the parent a question mid-task \
             or answer the parent without being asked. For work you must wait on, \
             make an `agent` call instead."
        );
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "`parent` or the session id of a live agent session."
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
    /// Resolve and queue one mailbox message. `None` targets address
    /// the sender's parent, and a target must share the sender's main
    /// session and still be running: the main session or an agent
    /// whose result is not delivered yet. The message is wrapped so the
    /// target knows who sent it and where to answer. An idle main session runs it at once; an
    /// idle agent runs it once the running limit allows; a busy target
    /// runs it after its current run ends.
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
        let label = self
            .sessions
            .get(&from)
            .and_then(|s| s.link.as_ref())
            .map_or_else(
                || "the main session".to_owned(),
                |l| format!("the {} agent", l.agent),
            );
        // A finished agent already delivered its result, so a reply to
        // a later message, and anything it changed, would never reach
        // its parent.
        let finished = self.sessions.get(&to).is_none_or(|target| {
            target
                .link
                .as_ref()
                .is_some_and(|link| link.reply.is_none())
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
        let target = &self.sessions[&to];
        let content = vec![Content::Text {
            text: format!("[message from {label} session {from}]\n\n{message}"),
        }];
        let busy = target.idle.is_none() || self.waiting.contains(&to);
        let max = target.agents.as_ref().map_or(usize::MAX, |a| a.max_running);
        if busy || target.link.is_none() {
            self.prompt(to, content, Delivery::Queue);
            return Ok(if busy {
                format!("message queued for session {to}; it runs when its current work ends")
            } else {
                format!("message delivered to session {to}; it runs now")
            });
        }
        let starts = self.running_agents() < max;
        self.launch_agent(to, max, content);
        Ok(if starts {
            format!("message delivered to session {to}; it runs now")
        } else {
            format!("message queued for session {to}; it runs when an agent slot frees")
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
