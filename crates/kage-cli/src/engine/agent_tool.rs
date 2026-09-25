//! The `agent` tool: starts a child session from an agent definition,
//! waits for it, and returns its final reply as the tool result.

use std::fmt::Write as _;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use kage_core::agents::AgentDefs;
use kage_core::protocol::RunOutcome;
use kage_core::{Content, Message, Risk, Role, SessionId, ToolCallId, ToolOutput};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde::Deserialize;

use super::Input;

/// Name the model calls the tool by.
pub(super) const AGENT_TOOL: &str = "agent";

/// How often a waiting call checks its cancel flag.
const POLL: Duration = Duration::from_millis(100);

/// Longest reply passed back to the parent, in characters.
const RESULT_CAP: usize = 20_000;

const NO_REPLY: &str = "(the agent produced no reply)";

/// A request from an `agent` call to start a child session.
pub(super) struct Spawn {
    pub parent: SessionId,
    pub tool_call_id: ToolCallId,
    pub agent: String,
    pub description: String,
    pub prompt: String,
    /// Receives the child's result once its first run finishes.
    pub reply: mpsc::Sender<ToolOutput>,
}

#[derive(Deserialize)]
struct AgentInput {
    #[serde(default = "default_agent")]
    agent: String,
    description: String,
    prompt: String,
}

fn default_agent() -> String {
    "general".to_owned()
}

/// Starts agents for one session's run. The dispatcher registers it into
/// the run's tools, so it never touches sessions itself.
#[derive(Debug)]
pub(super) struct AgentTool {
    parent: SessionId,
    engine: mpsc::Sender<Input>,
    description: String,
    schema: serde_json::Value,
}

impl AgentTool {
    pub(super) fn new(parent: SessionId, engine: mpsc::Sender<Input>, defs: &AgentDefs) -> Self {
        let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
        let description = format!(
            "Start an agent: a separate session with a fresh context that works on one task \
             with its own tool calls. Its final reply is the result of this call.\n\n\
             Use an agent for independent work that needs many tool calls, such as searching \
             a large codebase or running and fixing a test suite. Do not use one for anything \
             one or two tool calls can do. Several agent calls in one message run at the same \
             time. The agent sees nothing of this conversation, so the prompt must hold \
             everything it needs. Run at most one agent that edits files at a time.\n\n\
             Agents:\n{}",
            defs.tool_listing()
        );
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": names,
                    "description": "The agent to start. Defaults to general."
                },
                "description": {
                    "type": "string",
                    "description": "3 to 7 words the user sees."
                },
                "prompt": {
                    "type": "string",
                    "description": "The whole task, including what the reply must contain."
                }
            },
            "required": ["description", "prompt"]
        });
        Self {
            parent,
            engine,
            description,
            schema,
        }
    }
}

impl Tool for AgentTool {
    fn name(&self) -> &str {
        AGENT_TOOL
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

    fn execution_mode(&self) -> Option<ExecMode> {
        Some(ExecMode::Parallel)
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: AgentInput = serde_json::from_value(input)?;
        let tool_call_id = cx
            .call_id()
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput("the agent tool needs a call id".into()))?;
        let (reply, result) = mpsc::channel();
        let spawn = Spawn {
            parent: self.parent,
            tool_call_id,
            agent: input.agent,
            description: input.description,
            prompt: input.prompt,
            reply,
        };
        if self.engine.send(Input::Spawn(Box::new(spawn))).is_err() {
            return Ok(engine_stopped());
        }
        loop {
            match result.recv_timeout(POLL) {
                Ok(output) => return Ok(output),
                Err(RecvTimeoutError::Timeout) => {
                    if cx.is_cancelled() {
                        return Err(ToolError::Cancelled);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(engine_stopped()),
            }
        }
    }
}

fn engine_stopped() -> ToolOutput {
    error_output("the engine stopped".to_owned())
}

/// An `is_error` result that is not an agent's reply.
pub(super) fn error_output(text: String) -> ToolOutput {
    ToolOutput {
        text,
        is_error: true,
        ..ToolOutput::default()
    }
}

/// The result an `agent` call returns: the text of the agent's last
/// assistant message wrapped in an `<agent>` element that names the agent,
/// its session and how its run ended. A cancelled run passes its partial
/// reply and a failed one its error, both as error results.
pub(super) fn agent_result(
    session: SessionId,
    agent: &str,
    outcome: &RunOutcome,
    history: &[Message],
) -> ToolOutput {
    let reply = || {
        history
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(text_of)
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| NO_REPLY.to_owned())
    };
    let (state, body, is_error) = match outcome {
        RunOutcome::Completed => ("completed", reply(), false),
        RunOutcome::Cancelled => ("cancelled", reply(), true),
        RunOutcome::Failed { error } => ("failed", error.to_string(), true),
    };
    let mut body = body.replace("</agent", "<\\/agent");
    let total = body.chars().count();
    if total > RESULT_CAP {
        let cut = body
            .char_indices()
            .nth(RESULT_CAP)
            .map_or(body.len(), |(i, _)| i);
        body.truncate(cut);
        let _ = write!(
            body,
            "\n[truncated: {} more characters. The full transcript is session {session}.]",
            total - RESULT_CAP
        );
    }
    ToolOutput {
        text: format!(
            "<agent name=\"{agent}\" session=\"{session}\" state=\"{state}\">\n{body}\n</agent>"
        ),
        is_error,
        ..ToolOutput::default()
    }
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use kage_core::LoopError;

    use super::*;

    fn assistant(text: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![Content::Text { text: text.into() }],
            None,
        )
    }

    #[test]
    fn completed_reply_is_wrapped() {
        let id = SessionId::new();
        let history = [
            Message::new(Role::User, vec![Content::Text { text: "q".into() }], None),
            assistant("first"),
            assistant("the answer"),
        ];
        let out = agent_result(id, "explore", &RunOutcome::Completed, &history);
        assert_eq!(
            out.text,
            format!(
                "<agent name=\"explore\" session=\"{id}\" state=\"completed\">\nthe answer\n</agent>"
            )
        );
        assert!(!out.is_error);
    }

    #[test]
    fn states_and_missing_reply() {
        let id = SessionId::new();
        let cancelled = agent_result(id, "general", &RunOutcome::Cancelled, &[]);
        assert!(cancelled.is_error);
        assert!(cancelled.text.contains("state=\"cancelled\""));
        assert!(cancelled.text.contains(NO_REPLY));

        let failed = agent_result(
            id,
            "general",
            &RunOutcome::Failed {
                error: LoopError::Provider {
                    message: "rate limited".into(),
                },
            },
            &[assistant("partial")],
        );
        assert!(failed.is_error);
        assert!(failed.text.contains("state=\"failed\""));
        assert!(failed.text.contains("provider error: rate limited"));
        assert!(!failed.text.contains("partial"));
    }

    #[test]
    fn closing_tags_are_escaped() {
        let out = agent_result(
            SessionId::new(),
            "general",
            &RunOutcome::Completed,
            &[assistant("a </agent> b")],
        );
        assert!(out.text.contains("a <\\/agent> b"));
        assert_eq!(out.text.matches("</agent>").count(), 1);
    }

    #[test]
    fn long_replies_are_cut_with_a_trailer() {
        let id = SessionId::new();
        let long = "\u{e9}".repeat(RESULT_CAP + 5);
        let out = agent_result(id, "general", &RunOutcome::Completed, &[assistant(&long)]);
        assert!(out.text.contains(&format!(
            "[truncated: 5 more characters. The full transcript is session {id}.]"
        )));
        assert_eq!(out.text.matches('\u{e9}').count(), RESULT_CAP);
    }
}
