//! The `swarm` tool: starts one child agent per item from a prompt
//! template and waits for every child, then returns one aggregated
//! result.

use std::fmt::Write as _;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossbeam_channel::select_biased;
use kage_core::agents::AgentDefs;
use kage_core::protocol::{Command, CommandKind};
use kage_core::{Risk, SessionId, ToolCallId, ToolOutput};
use kage_tools::{ExecMode, Tool, ToolContext, ToolError};
use serde::Deserialize;
use ulid::Ulid;

use super::Input;
use super::agent_tool::{self, Spawn};

/// Name the model calls the tool by.
pub(super) const SWARM_TOOL: &str = "swarm";

/// Placeholder every item replaces in the prompt template.
const PLACEHOLDER: &str = "{{item}}";

/// How long a timed-out swarm still waits for the children the engine
/// is cancelling, so their states land in the aggregate.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// Longest whole swarm result, in characters. Per-child replies are
/// already capped at the `agent` tool's limit; when the aggregate is
/// still over this, bodies are cut before any status line.
const RESULT_CAP: usize = 100_000;

/// Marks a body the result cap cut.
const BODY_CUT: &str = "\n[body truncated to fit the result cap]";

/// Identifies one child of a `swarm` call, recorded in the child's
/// session marker.
#[derive(Clone, Debug)]
pub(super) struct SwarmInfo {
    /// The child's session id, assigned by the tool so the swarm can
    /// cancel children that never reported.
    pub id: SessionId,
    /// Id of the swarm call, shared by every child of one call.
    pub batch_id: ToolCallId,
    /// Position of this child's item, 0-based.
    pub index: usize,
    /// The item this child was spawned for.
    pub item: String,
}

#[derive(Deserialize)]
struct SwarmInput {
    description: String,
    #[serde(default = "default_agent")]
    agent: String,
    prompt_template: String,
    items: Vec<String>,
    /// Reserved for a later phase; rejected.
    resume: Option<serde_json::Value>,
    /// Reserved for a later phase; rejected.
    fork: Option<serde_json::Value>,
}

fn default_agent() -> String {
    "general".to_owned()
}

/// Starts a swarm for one session's run. The dispatcher registers it
/// into the run's tools next to the `agent` tool.
#[derive(Debug)]
pub(super) struct SwarmTool {
    parent: SessionId,
    engine: mpsc::Sender<Input>,
    defs: AgentDefs,
    description: String,
    schema: serde_json::Value,
    max_items: usize,
    timeout: Duration,
}

impl SwarmTool {
    pub(super) fn new(
        parent: SessionId,
        engine: mpsc::Sender<Input>,
        defs: &AgentDefs,
        max_items: usize,
        timeout: Duration,
    ) -> Self {
        let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
        let description = format!(
            "Start a swarm: one call spawns the same agent once per item and waits for \
             every child. Its final reply is one aggregated result with a summary line \
             and each child's session id, state and reply.\n\n\
             Use a swarm when one task shape repeats over many inputs, such as the same \
             fix across many crates or a review of many files. For a few different \
             tasks, make several `agent` calls in one message instead.\n\n\
             Coordination rules: explore the code yourself first and delegate only the \
             repeated work. Every child starts with zero context, so each expanded \
             prompt must be self-contained, holding the paths and details the child \
             needs. Give each child a distinct scope: no duplicated work, no \
             conflicting edits, at most one agent that edits files at a time.\n\n\
             Agents:\n{}",
            defs.tool_listing()
        );
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short label for the whole swarm."
                },
                "agent": {
                    "type": "string",
                    "enum": names,
                    "description": "The agent every child runs. Defaults to general."
                },
                "prompt_template": {
                    "type": "string",
                    "description":
                        format!("Task template; every {PLACEHOLDER} is replaced with the item.")
                },
                "items": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 2,
                    "maxItems": max_items,
                    "description": "One child per entry, substituted into the template."
                },
                "resume": {
                    "type": "object",
                    "description": "Reserved. Not supported yet."
                },
                "fork": {
                    "type": "boolean",
                    "description": "Reserved. Not supported yet."
                }
            },
            "required": ["description", "prompt_template", "items"]
        });
        Self {
            parent,
            engine,
            defs: defs.clone(),
            description,
            schema,
            max_items,
            timeout,
        }
    }
}

impl Tool for SwarmTool {
    fn name(&self) -> &str {
        SWARM_TOOL
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
        Some(ExecMode::Sequential)
    }

    fn runs_alone(&self) -> bool {
        true
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: SwarmInput = serde_json::from_value(input)?;
        let prompts = match expand(&input, &self.defs, self.max_items) {
            Ok(prompts) => prompts,
            Err(text) => return Ok(agent_tool::error_output(text)),
        };
        let call_id = cx
            .call_id()
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput("the swarm tool needs a call id".into()))?;
        let total = prompts.len();
        let batch_id = ToolCallId::new(format!("swarm_{}", Ulid::generate()));
        // One channel for the whole batch: every child delivers its
        // result once, so the tool receives exactly `total` results.
        let (reply, results) = crossbeam_channel::bounded(total);
        let mut children = Vec::with_capacity(total);
        for (index, (item, prompt)) in input.items.iter().zip(prompts).enumerate() {
            let id = SessionId::new();
            let spawn = Spawn {
                parent: self.parent,
                tool_call_id: call_id.clone(),
                agent: input.agent.clone(),
                description: input.description.clone(),
                prompt,
                reply: reply.clone(),
                swarm: Some(SwarmInfo {
                    id,
                    batch_id: batch_id.clone(),
                    index,
                    item: item.clone(),
                }),
            };
            if self.engine.send(Input::Spawn(Box::new(spawn))).is_err() {
                break;
            }
            children.push(id);
        }
        drop(reply);
        if children.is_empty() {
            return Ok(agent_tool::error_output("the engine stopped".to_owned()));
        }
        let arrived = collect(total, &children, self.timeout, &results, &self.engine, cx);
        Ok(render(
            RESULT_CAP,
            &input.description,
            &input.agent,
            &input.items,
            &children,
            &arrived,
        ))
    }
}

/// Wait for every child, with an overall deadline. On deadline the
/// children that never reported are cancelled and the results that
/// still land within a short grace are kept. When the parent is
/// cancelled, whatever arrived is kept and the rest renders as
/// cancelled.
fn collect(
    total: usize,
    children: &[SessionId],
    timeout: Duration,
    results: &crossbeam_channel::Receiver<ToolOutput>,
    engine: &mpsc::Sender<Input>,
    cx: &ToolContext<'_>,
) -> Vec<ToolOutput> {
    let watch = cx.cancel_flag().watch();
    let deadline = Instant::now() + timeout;
    let mut arrived: Vec<ToolOutput> = Vec::with_capacity(total);
    let cancelled = loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break false;
        };
        select_biased! {
            recv(results) -> output => match output {
                Ok(output) => arrived.push(output),
                Err(_) => break false,
            },
            recv(watch.receiver()) -> _ => break true,
            recv(crossbeam_channel::after(left)) -> _ => break false,
        }
    };
    if cancelled {
        while let Ok(output) = results.try_recv() {
            arrived.push(output);
        }
        return arrived;
    }
    if arrived.len() < total {
        for id in children {
            let _ = engine.send(Input::Command(Command::to(*id, CommandKind::Cancel)));
        }
        let grace = Instant::now() + CANCEL_GRACE;
        while arrived.len() < total {
            let Some(left) = grace.checked_duration_since(Instant::now()) else {
                break;
            };
            match results.recv_timeout(left) {
                Ok(output) => arrived.push(output),
                Err(_) => break,
            }
        }
    }
    arrived
}

/// Expand the call into one prompt per item, or the reason it is
/// invalid. The rules are whole-call: nothing spawns unless every
/// check passes.
fn expand(input: &SwarmInput, defs: &AgentDefs, max_items: usize) -> Result<Vec<String>, String> {
    if input.resume.is_some() {
        return Err("resume is not supported yet".into());
    }
    if input.fork.is_some() {
        return Err("fork is not supported yet".into());
    }
    if defs.get(&input.agent).is_none() {
        let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
        return Err(format!(
            "unknown agent `{}`. Available agents: {}",
            input.agent,
            names.join(", ")
        ));
    }
    if input.items.len() < 2 {
        return Err(format!(
            "a swarm needs at least 2 items (got {})",
            input.items.len()
        ));
    }
    if input.items.len() > max_items {
        return Err(format!(
            "a swarm is capped at {max_items} items (swarm_max_items), \
             the call lists {}",
            input.items.len()
        ));
    }
    if !input.prompt_template.contains(PLACEHOLDER) {
        return Err(format!(
            "prompt_template must contain {PLACEHOLDER}, so every item has \
             its place in the prompt"
        ));
    }
    let mut prompts = Vec::with_capacity(input.items.len());
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (index, item) in input.items.iter().enumerate() {
        if item.trim().is_empty() {
            return Err(format!("items[{}] is empty", index + 1));
        }
        let prompt = input.prompt_template.replace(PLACEHOLDER, item);
        if let Some(first) = seen.get(&prompt) {
            return Err(format!(
                "items {} and {} expand to the same prompt (\"{}\"); \
                 drop one or make them differ",
                first + 1,
                index + 1,
                item
            ));
        }
        seen.insert(prompt.clone(), index);
        prompts.push(prompt);
    }
    Ok(prompts)
}

/// One child's slot in the aggregate result: the lines around its
/// reply and the reply itself, so a result over the cap loses body
/// before it loses status.
struct Block {
    open: String,
    body: String,
    close: &'static str,
}

impl Block {
    /// The characters the block costs beyond its body.
    fn overhead(&self) -> usize {
        self.open.chars().count() + self.close.chars().count() + 1
    }

    fn render(&self, body: &str) -> String {
        format!("{}\n{body}{}", self.open, self.close)
    }
}

/// The aggregate result: a summary line, then one `<swarm>` element
/// per item wrapping the child's `<agent>` element. Children that
/// never reported render as cancelled, session id included. The call
/// is an error only when every child failed.
fn render(
    cap: usize,
    description: &str,
    agent: &str,
    items: &[String],
    children: &[SessionId],
    results: &[ToolOutput],
) -> ToolOutput {
    // Place each result on the child it names; a result naming no
    // known child lands on the first free slot, in arrival order.
    let mut slots: Vec<Option<&ToolOutput>> = vec![None; children.len()];
    let mut spare = 0;
    for result in results {
        let at = session_in(result).and_then(|id| children.iter().position(|child| *child == id));
        if let Some(at) = at {
            slots[at] = Some(result);
        } else {
            while spare < slots.len() && slots[spare].is_some() {
                spare += 1;
            }
            if spare < slots.len() {
                slots[spare] = Some(result);
                spare += 1;
            }
        }
    }
    let mut completed = 0;
    let mut failed = 0;
    let mut cancelled = 0;
    let mut blocks = Vec::with_capacity(children.len());
    for (index, child) in children.iter().enumerate() {
        let item = items.get(index).map_or("<unknown>", String::as_str);
        let swarm_open = format!(
            "<swarm description=\"{}\" item=\"{}\">",
            escape(description),
            escape(item)
        );
        let (open, body) = match slots[index] {
            Some(result) if is_agent(result) => {
                let (header, rest) = result.text.split_once('\n').unwrap_or((&result.text, ""));
                (
                    format!("{swarm_open}\n{header}"),
                    rest.strip_suffix("\n</agent>").unwrap_or(rest).to_owned(),
                )
            }
            Some(result) => (
                format!(
                    "{swarm_open}\n<agent name=\"{agent}\" session=\"{child}\" state=\"failed\">"
                ),
                result.text.clone(),
            ),
            None => (
                format!(
                    "{swarm_open}\n<agent name=\"{agent}\" session=\"{child}\" \
                     state=\"cancelled\">"
                ),
                "no result arrived".to_owned(),
            ),
        };
        match slots[index].and_then(state_in) {
            Some("completed") => completed += 1,
            Some("failed") => failed += 1,
            _ => cancelled += 1,
        }
        blocks.push(Block {
            open,
            body,
            close: "\n</agent>\n</swarm>",
        });
    }
    let summary = format!("completed: {completed}, failed: {failed}, cancelled: {cancelled}");
    let mut out = summary.clone();
    let mut budget = cap.saturating_sub(summary.chars().count());
    for block in &blocks {
        budget = budget.saturating_sub(block.overhead());
    }
    let mut left = blocks.len();
    for block in &blocks {
        let allowance = budget / left.max(1);
        let (body, used) = if block.body.chars().count() > allowance {
            let body_cap = allowance.saturating_sub(BODY_CUT.chars().count());
            (
                format!("{}{BODY_CUT}", truncate(&block.body, body_cap)),
                allowance,
            )
        } else {
            (block.body.clone(), block.body.chars().count())
        };
        budget -= used;
        let _ = write!(out, "\n{}", block.render(&body));
        left -= 1;
    }
    ToolOutput {
        text: out,
        is_error: !children.is_empty() && failed == children.len(),
        ..ToolOutput::default()
    }
}

/// Cut `text` to at most `cap` characters, on a char boundary.
fn truncate(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    text.char_indices()
        .nth(cap)
        .map_or(text.to_owned(), |(at, _)| text[..at].to_owned())
}

/// Whether a result is an `<agent>` element the `agent` tool rendered.
fn is_agent(result: &ToolOutput) -> bool {
    result
        .text
        .lines()
        .next()
        .is_some_and(|line| line.starts_with("<agent "))
}

/// The session id in a result's `<agent ...>` header line.
fn session_in(result: &ToolOutput) -> Option<SessionId> {
    let line = result.text.lines().next()?;
    let rest = line.split("session=\"").nth(1)?;
    Ulid::from_string(rest.split('"').next()?)
        .ok()
        .map(SessionId)
}

/// The run state in a result's `<agent ...>` header line.
fn state_in(result: &ToolOutput) -> Option<&'static str> {
    let line = result.text.lines().next()?;
    match line.split("state=\"").nth(1)?.split('"').next()? {
        "completed" => Some("completed"),
        "failed" => Some("failed"),
        "cancelled" => Some("cancelled"),
        _ => None,
    }
}

/// Make `value` safe inside a double-quoted attribute.
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::AgentSetup;

    fn input(items: &[&str]) -> SwarmInput {
        SwarmInput {
            description: "a swarm".into(),
            agent: "general".into(),
            prompt_template: "handle {{item}}".into(),
            items: items.iter().map(|item| (*item).to_owned()).collect(),
            resume: None,
            fork: None,
        }
    }

    #[test]
    fn expand_replaces_every_placeholder() {
        let mut call = input(&["a", "b"]);
        call.prompt_template = "{{item}} then {{item}} again".into();
        assert_eq!(
            expand(&call, &AgentDefs::builtin(), 32).unwrap(),
            ["a then a again", "b then b again"]
        );
    }

    #[test]
    fn expand_rejects_a_bad_call() {
        let defs = AgentDefs::builtin();
        let mut call = input(&["a"]);
        assert!(expand(&call, &defs, 32).unwrap_err().contains("at least 2"));

        call = input(&["a", "b"]);
        call.prompt_template = "no placeholder".into();
        assert!(
            expand(&call, &defs, 32)
                .unwrap_err()
                .contains("must contain {{item}}")
        );

        call = input(&["a", "a"]);
        let err = expand(&call, &defs, 32).unwrap_err();
        assert!(err.contains("items 1 and 2"), "{err}");
        assert!(err.contains("\"a\""), "{err}");

        call = input(&["a", "b"]);
        assert!(expand(&call, &defs, 2).is_ok());
        let err = expand(&call, &defs, 1).unwrap_err();
        assert!(err.contains("capped at 1 items"), "{err}");

        call = input(&["a", " "]);
        assert!(expand(&call, &defs, 32).unwrap_err().contains("empty"));

        call = input(&["a", "b"]);
        call.agent = "nope".into();
        let err = expand(&call, &defs, 32).unwrap_err();
        assert!(err.contains("explore, general"), "{err}");
    }

    #[test]
    fn expand_rejects_the_reserved_fields() {
        let defs = AgentDefs::builtin();
        let mut call = input(&["a", "b"]);
        call.resume = Some(serde_json::json!({}));
        assert!(
            expand(&call, &defs, 32)
                .unwrap_err()
                .contains("resume is not supported yet")
        );
        call.resume = None;
        call.fork = Some(serde_json::json!(true));
        assert!(
            expand(&call, &defs, 32)
                .unwrap_err()
                .contains("fork is not supported yet")
        );
    }

    #[test]
    fn config_defaults_reach_the_setup() {
        let defaults =
            AgentSetup::from_config(AgentDefs::builtin(), &kage_core::config::Config::default());
        assert_eq!(defaults.swarm_max_items, 32);
        assert_eq!(defaults.swarm_timeout_ms, 7_200_000);
    }

    /// One `<agent>` element like `agent_result` renders it.
    fn agent_block(session: SessionId, state: &str, body: &str) -> ToolOutput {
        ToolOutput {
            text: format!(
                "<agent name=\"general\" session=\"{session}\" state=\"{state}\">\n{body}\n</agent>"
            ),
            is_error: state != "completed",
            ..ToolOutput::default()
        }
    }

    fn two_children() -> (Vec<SessionId>, Vec<String>) {
        (
            vec![SessionId::new(), SessionId::new()],
            vec!["a".into(), "b".into()],
        )
    }

    #[test]
    fn render_counts_and_wraps_each_child() {
        let (children, items) = two_children();
        let results = vec![
            agent_block(children[0], "completed", "did a"),
            agent_block(children[1], "failed", "boom"),
        ];
        let out = render(
            RESULT_CAP,
            "fix things",
            "general",
            &items,
            &children,
            &results,
        );
        assert!(
            out.text
                .starts_with("completed: 1, failed: 1, cancelled: 0\n"),
            "{}",
            out.text
        );
        assert!(out.text.contains(&format!(
            "<swarm description=\"fix things\" item=\"a\">\n<agent name=\"general\" \
             session=\"{}\" state=\"completed\">\ndid a\n</agent>\n</swarm>",
            children[0]
        )));
        assert!(out.text.contains("item=\"b\""));
        assert!(!out.is_error, "not every child failed");
    }

    #[test]
    fn render_marks_missing_children_cancelled() {
        let (children, items) = two_children();
        let out = render(RESULT_CAP, "d", "general", &items, &children, &[]);
        assert!(out.text.contains("completed: 0, failed: 0, cancelled: 2"));
        assert!(out.text.contains(&format!(
            "<agent name=\"general\" session=\"{}\" state=\"cancelled\">",
            children[1]
        )));
        assert!(out.text.contains("no result arrived"));
        assert!(!out.is_error);
    }

    #[test]
    fn render_is_error_only_when_every_child_failed() {
        let (children, items) = two_children();
        let all_failed = vec![
            agent_block(children[0], "failed", "x"),
            agent_block(children[1], "failed", "y"),
        ];
        assert!(render(RESULT_CAP, "d", "general", &items, &children, &all_failed).is_error);
        let mixed = vec![
            agent_block(children[0], "completed", "x"),
            agent_block(children[1], "failed", "y"),
        ];
        assert!(!render(RESULT_CAP, "d", "general", &items, &children, &mixed).is_error);
    }

    #[test]
    fn truncation_keeps_every_status_line_and_cuts_bodies() {
        let (children, items) = two_children();
        let body = "x".repeat(500);
        let results = vec![
            agent_block(children[0], "completed", &body),
            agent_block(children[1], "completed", &body),
        ];
        let out = render(600, "d", "general", &items, &children, &results);
        let count = |needle: &str| out.text.matches(needle).count();
        assert_eq!(count("item=\"a\""), 1, "{}", out.text);
        assert_eq!(count("item=\"b\""), 1);
        assert_eq!(count("state=\"completed\""), 2);
        assert_eq!(count(BODY_CUT.trim_start()), 2);
        assert!(out.text.chars().count() < 1_000, "cut to the cap");
    }

    #[test]
    fn a_result_without_an_agent_header_still_renders() {
        let (children, items) = two_children();
        let odd = ToolOutput {
            text: "the engine stopped".into(),
            is_error: true,
            ..ToolOutput::default()
        };
        let out = render(RESULT_CAP, "d", "general", &items, &children, &[odd]);
        assert!(out.text.contains("state=\"failed\""));
        assert!(out.text.contains("the engine stopped"));
    }
}
