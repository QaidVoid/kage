//! Plan mode: the reminders the engine injects when it turns on or off,
//! and the `exit_plan` tool the agent presents its plan with.
//!
//! The permission gate asks the user about every `exit_plan` call, so
//! reaching [`Tool::execute`] means the plan was approved. A plan the
//! user did not approve never runs the tool: the gate ends the run.

use std::sync::mpsc;

use kage_core::protocol::EXIT_PLAN_TOOL;
use kage_core::{Risk, SessionId, ToolOutput};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde::Deserialize;

use super::Input;
use crate::permissions::PermissionGate;

/// Context block injected once when a session's plan mode turns on.
pub(crate) const PLAN_MODE_ON: &str = "[plan mode on] Plan before changing anything. \
Investigate with read-only tools such as read, grep, find, ls, web_search and web_fetch; write \
and edit are refused, and commands wait for the user's approval. When you understand the task, call \
`exit_plan` once with the whole plan in Markdown: a `#` title, the goal, the steps with the \
files each one touches, and how to verify the result. The user approves it, asks for changes or \
rejects it. Do not start the work until the plan is approved. The user leaves plan mode with \
`/plan off`.";

/// Context block injected once when a session's plan mode turns off.
pub(crate) const PLAN_MODE_OFF: &str = "[plan mode off] You may change files and run commands \
again under the normal permission rules.";

const DESCRIPTION: &str = "Present your plan for the user's review. Call it once, alone, when \
your investigation is done. `plan` is the whole plan in Markdown: start with a `# ` title, then \
the goal, the steps with the files each one touches, and how to verify the result. When the user \
approves, plan mode turns off and you carry out the plan. When they do not, the run ends and \
their next message says what to change.";

#[derive(Deserialize)]
struct PlanInput {
    plan: String,
}

/// Presents one session's plan. The dispatcher registers it into runs
/// that start in plan mode.
pub(super) struct ExitPlanTool {
    session: SessionId,
    engine: mpsc::Sender<Input>,
    gate: PermissionGate,
}

impl ExitPlanTool {
    pub(super) fn new(
        session: SessionId,
        engine: mpsc::Sender<Input>,
        gate: PermissionGate,
    ) -> Self {
        Self {
            session,
            engine,
            gate,
        }
    }
}

impl std::fmt::Debug for ExitPlanTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExitPlanTool")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Tool for ExitPlanTool {
    fn name(&self) -> &str {
        EXIT_PLAN_TOOL
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "The whole plan in Markdown, starting with a `# ` title."
                }
            },
            "required": ["plan"]
        })
    }

    fn risk(&self) -> Risk {
        Risk::Read
    }

    fn execution_mode(&self) -> Option<ExecMode> {
        Some(ExecMode::Sequential)
    }

    fn runs_alone(&self) -> bool {
        true
    }

    fn execute(
        &self,
        input: serde_json::Value,
        _cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: PlanInput = serde_json::from_value(input)?;
        if input.plan.trim().is_empty() {
            return Ok(super::agent_tool::error_output(
                "the plan is empty".to_owned(),
            ));
        }
        if !self.gate.plan() {
            return Ok(ToolOutput {
                text: "Plan mode is already off; go ahead with the work.".to_owned(),
                ..ToolOutput::default()
            });
        }
        // Off before the next call is judged, so the approved work is
        // not refused by the mode it just left.
        self.gate.set_plan(false);
        let _ = self.engine.send(Input::PlanApproved(self.session));
        Ok(ToolOutput {
            text: "The user approved the plan. Plan mode is off: carry out the plan now."
                .to_owned(),
            ..ToolOutput::default()
        })
    }
}
