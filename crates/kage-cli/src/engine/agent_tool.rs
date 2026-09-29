//! The `agent` tool: starts a child session from an agent definition,
//! waits for it, and returns its final reply as the tool result.

use std::fmt::Write as _;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crossbeam_channel::select_biased;
use kage_core::agents::AgentDefs;
use kage_core::event::AGENT_NO_REPLY_TEXT as NO_REPLY;
use kage_core::protocol::{RunOutcome, Usage};
use kage_core::{Content, Message, Risk, Role, SessionId, ToolCallId, ToolOutput};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde::Deserialize;

use super::{Input, swarm_tool::SwarmInfo};

/// Name the model calls the tool by.
pub(super) const AGENT_TOOL: &str = "agent";

/// How long a cancelled call waits for the child's cancelled result, which
/// carries its session id and partial reply.
const CANCEL_GRACE: Duration = Duration::from_millis(300);

/// Longest reply passed back to the parent, in characters.
const RESULT_CAP: usize = 20_000;

/// A request from an `agent` call to start a child session.
pub(super) struct Spawn {
    pub parent: SessionId,
    pub tool_call_id: ToolCallId,
    pub agent: String,
    pub description: String,
    pub prompt: String,
    /// Receives the child's result once its first run finishes.
    pub reply: crossbeam_channel::Sender<ToolOutput>,
    /// Start the child from a snapshot of the parent's conversation
    /// instead of zero context. A plain `agent` call leaves it false.
    pub fork: bool,
    /// Set when a `swarm` call spawned this child. A plain `agent`
    /// call leaves it `None`.
    pub swarm: Option<SwarmInfo>,
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
             everything it needs. Agents may edit in parallel while each touches its own \
             files; files several agents need belong to one of them.\n\n\
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
        let (reply, result) = crossbeam_channel::bounded(1);
        let spawn = Spawn {
            parent: self.parent,
            tool_call_id,
            agent: input.agent,
            description: input.description,
            prompt: input.prompt,
            reply,
            fork: false,
            swarm: None,
        };
        if self.engine.send(Input::Spawn(Box::new(spawn))).is_err() {
            return Ok(engine_stopped());
        }
        let watch = cx.cancel_flag().watch();
        select_biased! {
            recv(result) -> output => Ok(output.unwrap_or_else(|_| engine_stopped())),
            recv(watch.receiver()) -> _ => result
                .recv_timeout(CANCEL_GRACE)
                .map_err(|_| ToolError::Cancelled),
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
/// A child's result: the `<agent>` wrapper the model reads and the
/// tree restores from. The header records the child's tool count,
/// usage totals and run time, so the agents list survives a session
/// restart; the tool count is the calls in the child's history.
pub(super) fn agent_result(
    session: SessionId,
    agent: &str,
    outcome: &RunOutcome,
    history: &[Arc<Message>],
    usage: &Usage,
    run_time: Duration,
) -> ToolOutput {
    let reply = || {
        history
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| text_of(m))
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| NO_REPLY.to_owned())
    };
    let (state, body, is_error) = match outcome {
        RunOutcome::Completed => ("completed", reply(), false),
        RunOutcome::Cancelled => ("cancelled", reply(), true),
        RunOutcome::Failed { error } => ("failed", error.to_string(), true),
    };
    let tool_calls = history
        .iter()
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, Content::ToolCall { .. }))
        .count();
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
    let run_ms = u64::try_from(run_time.as_millis()).unwrap_or(u64::MAX);
    ToolOutput {
        text: format!(
            "<agent name=\"{agent}\" session=\"{session}\" state=\"{state}\" \
             tools=\"{tool_calls}\" in=\"{}\" out=\"{}\" cache_read=\"{}\" \
             cache_write=\"{}\" cost=\"{:.4}\" ctx=\"{}\" win=\"{}\" run_ms=\"{run_ms}\">\n\
             {body}\n</agent>",
            usage.total.input,
            usage.total.output,
            usage.total.cache_read,
            usage.total.cache_write,
            usage.cost,
            usage.context_used,
            usage.context_window,
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
            Arc::new(Message::new(
                Role::User,
                vec![Content::Text { text: "q".into() }],
                None,
            )),
            Arc::new(assistant("first")),
            Arc::new(assistant("the answer")),
        ];
        let out = agent_result(
            id,
            "explore",
            &RunOutcome::Completed,
            &history,
            &Usage::default(),
            Duration::ZERO,
        );
        assert_eq!(
            out.text,
            format!(
                "<agent name=\"explore\" session=\"{id}\" state=\"completed\" tools=\"0\" \
                 in=\"0\" out=\"0\" cache_read=\"0\" cache_write=\"0\" cost=\"0.0000\" \
                 ctx=\"0\" win=\"0\" run_ms=\"0\">\n\
                 the answer\n</agent>"
            )
        );
        assert!(!out.is_error);
    }

    #[test]
    fn the_header_records_the_stats_of_the_run() {
        let id = SessionId::new();
        let call = |text: &str| Content::ToolCall {
            id: ToolCallId(text.into()),
            name: "read".into(),
            input: serde_json::json!({ "path": "a.rs" }),
        };
        let history = [
            Arc::new(Message::new(
                Role::User,
                vec![Content::Text { text: "q".into() }],
                None,
            )),
            Arc::new(Message::new(
                Role::Assistant,
                vec![Content::Text { text: "h".into() }, call("c1")],
                None,
            )),
            Arc::new(Message::new(
                Role::User,
                vec![Content::ToolResultBlock {
                    call_id: ToolCallId("c1".into()),
                    output: "hi".into(),
                    is_error: false,
                }],
                None,
            )),
            Arc::new(Message::new(Role::Assistant, vec![call("c2")], None)),
        ];
        let usage = Usage {
            total: kage_core::event::TokenUsage {
                input: 1_200,
                output: 40,
                cache_read: 800,
                cache_write: 120,
            },
            cost: 0.5,
            context_used: 2_160,
            context_window: 200_000,
        };
        let out = agent_result(
            id,
            "general",
            &RunOutcome::Completed,
            &history,
            &usage,
            Duration::from_millis(4_200),
        );
        let header = out.text.split_once('\n').unwrap().0;
        assert!(
            header.ends_with(
                "tools=\"2\" in=\"1200\" out=\"40\" cache_read=\"800\" \
                 cache_write=\"120\" cost=\"0.5000\" ctx=\"2160\" win=\"200000\" \
                 run_ms=\"4200\">"
            ),
            "{header}"
        );
    }

    #[test]
    fn states_and_missing_reply() {
        let id = SessionId::new();
        let cancelled = agent_result(
            id,
            "general",
            &RunOutcome::Cancelled,
            &[],
            &Usage::default(),
            Duration::ZERO,
        );
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
            &[Arc::new(assistant("partial"))],
            &Usage::default(),
            Duration::ZERO,
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
            &[Arc::new(assistant("a </agent> b"))],
            &Usage::default(),
            Duration::ZERO,
        );
        assert!(out.text.contains("a <\\/agent> b"));
        assert_eq!(out.text.matches("</agent>").count(), 1);
    }

    #[test]
    fn long_replies_are_cut_with_a_trailer() {
        let id = SessionId::new();
        let long = "\u{e9}".repeat(RESULT_CAP + 5);
        let out = agent_result(
            id,
            "general",
            &RunOutcome::Completed,
            &[Arc::new(assistant(&long))],
            &Usage::default(),
            Duration::ZERO,
        );
        assert!(out.text.contains(&format!(
            "[truncated: 5 more characters. The full transcript is session {id}.]"
        )));
        assert_eq!(out.text.matches('\u{e9}').count(), RESULT_CAP);
    }

    #[test]
    fn cancel_ends_a_call_the_engine_never_answers() {
        let (engine, spawns) = mpsc::channel();
        let tool = AgentTool::new(SessionId::new(), engine, &AgentDefs::builtin());
        let cancel = kage_core::CancelFlag::new();
        let flag = cancel.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let workdir = std::env::temp_dir();
            let id = ToolCallId::new("call");
            let cx = ToolContext::new(&workdir, &flag).with_call_id(&id);
            let input = serde_json::json!({"description": "d", "prompt": "p"});
            let _ = done_tx.send(tool.execute(input, &cx));
        });
        let spawn = spawns.recv_timeout(Duration::from_secs(5)).unwrap();
        cancel.cancel();
        let result = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(result, Err(ToolError::Cancelled)), "{result:?}");
        handle.join().unwrap();
        drop(spawn);
    }
}
