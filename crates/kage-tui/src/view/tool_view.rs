//! One describer for tool calls.
//!
//! [`describe`] turns a tool name and its JSON input into a verb-first
//! [`ToolLabel`] (`Read README.md`, `Ran cargo test`,
//! `Edited src/lib.rs (+1 -1)`). Tool rows, the `Explored` group row,
//! the approval panel, F3 labels and the working row all read tool
//! calls through this module, so a tool reads the same everywhere.
//!
//! Everything here is a display transform. The text the model receives
//! is never touched: [`bash_output`] and [`agent_output`] strip the
//! model-facing labels and wrapper from a copy for painting only.

use serde_json::Value;

use super::modeline::format_token_count;
use super::{UnicodeWidthStr, pad_to_width};

/// Lifecycle of one tool call as the conversation shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolPhase {
    /// The model is still streaming the call's arguments.
    Streaming,
    /// The arguments are complete and the call waits its turn to run.
    Queued,
    /// The call waits for the user's approval.
    Waiting,
    /// The tool is executing.
    Running,
    /// The tool finished successfully.
    Done,
    /// The tool finished with an error.
    Failed,
    /// The user denied the call.
    Denied,
    /// The run ended before the tool finished.
    Interrupted,
}

/// What a folded tool row shows below its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolBody {
    /// Nothing: read-only tools only show their header.
    Hidden,
    /// The last lines of the output (`bash`).
    Tail,
    /// The change as `-` and `+` lines (`edit`).
    Diff,
    /// The first lines of the output.
    Head,
}

/// Verb-first description of one tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolLabel {
    /// Plain verb for a call that has not run or did not do its work,
    /// such as `Read` or `Edit`.
    pub verb: &'static str,
    /// Verb while the call runs, such as `Reading`.
    pub verb_live: &'static str,
    /// Verb once the call finished, such as `Read`.
    pub verb_done: &'static str,
    /// What the call acts on: a path, a command, a pattern, or the
    /// tool name for tools without a tailored summary. Always one line.
    pub target: String,
    /// Extra facts painted after the target, such as `(+1 -1)`. Empty
    /// when there are none.
    pub stats: String,
    /// Whether the tool only reads local state. Only read-only calls
    /// are grouped into an `Explored` row.
    pub read_only: bool,
    /// What the folded row shows below the header.
    pub body: ToolBody,
}

/// How a [`BodyLine`] is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// Plain output text.
    Text,
    /// An added line of a diff.
    Add,
    /// A removed line of a diff.
    Delete,
    /// A dim marker such as `stderr` or `lines 3-5`.
    Marker,
}

/// One display line of a tool body, without any `+`, `-` or indent
/// prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyLine {
    /// How the line is painted.
    pub kind: LineKind,
    /// The line text.
    pub text: String,
}

impl BodyLine {
    fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// The change an `edit` call makes, read from its input.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditDiff {
    /// Removed and added lines in change order.
    pub lines: Vec<BodyLine>,
    /// Number of added lines.
    pub added: usize,
    /// Number of removed lines.
    pub removed: usize,
}

impl EditDiff {
    fn push_change(&mut self, change: &Value) {
        if let Some(range) = change.get("range") {
            let bound = |key| range.get(key).and_then(Value::as_u64).unwrap_or(0);
            let (start, end) = (bound("start"), bound("end"));
            let marker = if start == end {
                format!("line {start}")
            } else {
                format!("lines {start}-{end}")
            };
            self.lines.push(BodyLine::new(LineKind::Marker, marker));
            if start > 0 && end >= start {
                self.removed += usize::try_from(end - start + 1).unwrap_or(usize::MAX);
            }
            self.push_lines(LineKind::Add, field(change, "text"));
        } else {
            self.push_lines(LineKind::Delete, field(change, "old_str"));
            self.push_lines(LineKind::Add, field(change, "new_str"));
        }
    }

    fn push_lines(&mut self, kind: LineKind, text: &str) {
        for line in text.lines() {
            self.lines.push(BodyLine::new(kind, line));
            if kind == LineKind::Add {
                self.added += 1;
            } else {
                self.removed += 1;
            }
        }
    }
}

/// How a `bash` command ended, read from its output text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BashExit {
    /// The command exited with this code.
    Code(i32),
    /// The command was killed by a signal.
    Signal,
}

/// How an agent's run ended, read from its `agent` call result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentEnd {
    /// The agent completed its task.
    Done,
    /// The agent was stopped before it finished.
    Stopped,
    /// The agent's run failed.
    Failed,
}

impl AgentEnd {
    /// The state word a finished row paints on the right.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// Describe a tool call from its name and (possibly partial) input.
///
/// Built-in tools get a tailored verb and target. MCP tools
/// (`server__tool`) read as `Called server.tool`, and any other tool
/// as `Called <tool>` with its arguments summarized on one line in
/// [`ToolLabel::stats`].
#[must_use]
pub fn describe(name: &str, input: &Value) -> ToolLabel {
    let path = || field(input, "path").to_owned();
    let mut described = match name {
        "read" => {
            label(["Read", "Reading", "Read"], path(), read_range(input)).body(ToolBody::Hidden)
        }
        "write" => {
            let lines = field(input, "content").lines().count();
            let stats = if lines == 0 {
                String::new()
            } else {
                format!("({lines} {})", plural(lines, "line", "lines"))
            };
            label(["Write", "Writing", "Wrote"], path(), stats)
        }
        "edit" => {
            let diff = edit_diff(input);
            let stats = if diff.lines.is_empty() {
                String::new()
            } else {
                format!("(+{} -{})", diff.added, diff.removed)
            };
            label(["Edit", "Editing", "Edited"], path(), stats).body(ToolBody::Diff)
        }
        "bash" => label(
            ["Run", "Running", "Ran"],
            one_line(field(input, "command")),
            String::new(),
        )
        .body(ToolBody::Tail),
        "ls" => {
            let dir = input.get("path").and_then(Value::as_str).unwrap_or(".");
            label(["List", "Listing", "Listed"], dir.to_owned(), String::new())
                .body(ToolBody::Hidden)
        }
        "find" | "grep" => label(
            ["Search", "Searching", "Searched"],
            search_target(input),
            String::new(),
        )
        .body(ToolBody::Hidden),
        "web_fetch" => label(
            ["Fetch", "Fetching", "Fetched"],
            field(input, "url").to_owned(),
            String::new(),
        ),
        "agent" => {
            let target = match one_line(field(input, "description")) {
                description if description.is_empty() => agent_name(input).to_owned(),
                description => format!("{}: {description}", agent_name(input)),
            };
            label(["Agent", "Agent", "Agent"], target, String::new())
        }
        _ => label(
            ["Call", "Calling", "Called"],
            display_name(name),
            arg_summary(input),
        ),
    };
    described.read_only = is_read_only(name);
    described
}

/// Whether tool `name` only reads local state, so its calls may be
/// grouped into an `Explored` row.
#[must_use]
pub fn is_read_only(name: &str) -> bool {
    matches!(name, "read" | "ls" | "find" | "grep")
}

/// The `-` and `+` lines of an `edit` call with their counts. Reads the
/// shorthand `old_str`/`new_str` form and the `changes` list, where a
/// line-range change shows a `lines S-E` marker and its new text.
#[must_use]
pub fn edit_diff(input: &Value) -> EditDiff {
    let mut diff = EditDiff::default();
    match input.get("changes").and_then(Value::as_array) {
        Some(changes) => changes.iter().for_each(|c| diff.push_change(c)),
        None => diff.push_change(input),
    }
    diff
}

/// Split `bash` output text into display lines and the exit status.
///
/// Drops the `stdout:` label and the trailing `exit: N` line, which are
/// meant for the model. A dim `stderr` marker separates the streams
/// only when both have output. `(no output)` gives no lines. Text that
/// does not end in an `exit:` line comes back whole with no status.
#[must_use]
pub fn bash_output(text: &str) -> (Vec<BodyLine>, Option<BashExit>) {
    let (rest, exit) = match text.rsplit_once("\nexit: ") {
        Some((rest, "signal")) => (rest, Some(BashExit::Signal)),
        Some((rest, code)) => match code.parse() {
            Ok(code) => (rest, Some(BashExit::Code(code))),
            Err(_) => (text, None),
        },
        None => (text, None),
    };
    let mut lines = Vec::new();
    if rest == "(no output)" {
        return (lines, exit);
    }
    let mut push = |kind, s: &str| lines.extend(s.lines().map(|l| BodyLine::new(kind, l)));
    if let Some(out) = rest.strip_prefix("stdout:\n") {
        match out.split_once("\nstderr:\n") {
            Some((out, err)) => {
                push(LineKind::Text, out);
                push(LineKind::Marker, "stderr");
                push(LineKind::Text, err);
            }
            None => push(LineKind::Text, out),
        }
    } else {
        push(
            LineKind::Text,
            rest.strip_prefix("stderr:\n").unwrap_or(rest),
        );
    }
    (lines, exit)
}

/// The body the engine writes for an agent that ended without a reply.
const NO_REPLY: &str = "(the agent produced no reply)";

/// Split an `agent` call result into display lines and how the agent
/// ended.
///
/// Drops the `<agent ...>` wrapper the model reads. A stopped agent's
/// first line says so before its partial reply. Text without the
/// wrapper, such as a refused start, comes back whole with no end.
#[must_use]
pub fn agent_output(text: &str) -> (Vec<BodyLine>, Option<AgentEnd>) {
    let unwrapped = text.strip_prefix("<agent ").and_then(|rest| {
        let (attrs, rest) = rest.split_once(">\n")?;
        let body = rest.strip_suffix("\n</agent>")?;
        let state = attrs.split_once("state=\"")?.1.split_once('"')?.0;
        let end = match state {
            "completed" => AgentEnd::Done,
            "cancelled" => AgentEnd::Stopped,
            "failed" => AgentEnd::Failed,
            _ => return None,
        };
        Some((body, end))
    });
    let Some((body, end)) = unwrapped else {
        let lines = text.lines().map(|l| BodyLine::new(LineKind::Text, l));
        return (lines.collect(), None);
    };
    let mut lines: Vec<BodyLine> = body
        .lines()
        .map(|l| BodyLine::new(LineKind::Text, l))
        .collect();
    if end == AgentEnd::Stopped {
        match lines.first_mut() {
            Some(first) if body != NO_REPLY => {
                first.text = format!("Stopped by you. Partial reply: {}", first.text);
            }
            _ => {
                let text = "Stopped by you before it replied.";
                lines = vec![BodyLine::new(LineKind::Text, text)];
            }
        }
    }
    (lines, Some(end))
}

/// The stat line of an agent card: the tool count and, once the agent
/// used any, its tokens (`14 tools`, a middle dot, `22k tok`).
#[must_use]
pub fn agent_stats(tool_calls: u32, tokens: u64) -> String {
    let tools = usize::try_from(tool_calls).unwrap_or(usize::MAX);
    let tools = format!("{tools} {}", plural(tools, "tool", "tools"));
    if tokens == 0 {
        return tools;
    }
    format!("{tools} \u{b7} {} tok", format_token_count(tokens))
}

/// The approval title for a call, such as `Run this command?` or
/// `Edit src/lib.rs?`.
#[must_use]
pub fn question(name: &str, input: &Value) -> String {
    if name == "bash" {
        return "Run this command?".to_owned();
    }
    if name == "agent" {
        return format!("Start agent {}?", agent_name(input));
    }
    let label = describe(name, input);
    if label.target.is_empty() {
        return format!("Allow {}?", display_name(name));
    }
    let verb = if label.verb == "Call" {
        "Allow"
    } else {
        label.verb
    };
    format!("{verb} {}?", label.target)
}

/// The arguments of a call as `(key, value)` rows. Keys are padded to
/// one width so values line up. Values are cut to their first line with
/// a `(+N lines)` note, and non-string values are compact JSON.
#[must_use]
pub fn arg_rows(input: &Value) -> Vec<(String, String)> {
    let Some(args) = input.as_object() else {
        return Vec::new();
    };
    let width = args.keys().map(|k| k.width()).max().unwrap_or(0);
    args.iter()
        .map(|(k, v)| (pad_to_width(k, width), one_line(&value_text(v))))
        .collect()
}

/// The counts an `Explored` row shows for its read-only calls, such as
/// `2 files, 1 search`.
#[must_use]
pub fn group_summary(labels: &[ToolLabel]) -> String {
    [
        ("Read", "file", "files"),
        ("Listed", "directory", "directories"),
        ("Searched", "search", "searches"),
    ]
    .into_iter()
    .filter_map(|(verb, one, many)| {
        let n = labels.iter().filter(|l| l.verb_done == verb).count();
        (n > 0).then(|| format!("{n} {}", plural(n, one, many)))
    })
    .collect::<Vec<_>>()
    .join(", ")
}

/// Format elapsed time in whole seconds (`0s`, `14s`), then minutes and
/// seconds (`1m 05s`). The working row and the thinking block share it,
/// so their clocks read alike.
#[must_use]
pub fn format_seconds(ms: u64) -> String {
    if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else {
        format_elapsed(ms)
    }
}

/// Format a duration for a tool row: tenths below ten seconds (`0.1s`,
/// `4.9s`), whole seconds below a minute (`14s`), then minutes and
/// seconds (`1m 05s`).
#[must_use]
pub fn format_elapsed(ms: u64) -> String {
    let tenths = ms.saturating_add(50) / 100;
    if tenths < 100 {
        return format!("{}.{}s", tenths / 10, tenths % 10);
    }
    let secs = ms.saturating_add(500) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// A label from its `[plain, live, done]` verbs.
fn label(
    [verb, verb_live, verb_done]: [&'static str; 3],
    target: String,
    stats: String,
) -> ToolLabel {
    ToolLabel {
        verb,
        verb_live,
        verb_done,
        target,
        stats,
        read_only: false,
        body: ToolBody::Head,
    }
}

impl ToolLabel {
    fn body(mut self, body: ToolBody) -> Self {
        self.body = body;
        self
    }

    /// The verb for a call in `phase`: live while it runs, past once it
    /// did its work, plain otherwise. `ran` says a failed call still did
    /// its work, like a command that exited non-zero.
    #[must_use]
    pub fn verb_for(&self, phase: ToolPhase, ran: bool) -> &'static str {
        match phase {
            ToolPhase::Running => self.verb_live,
            ToolPhase::Done => self.verb_done,
            ToolPhase::Failed if ran => self.verb_done,
            _ => self.verb,
        }
    }

    /// The stats for a call in `phase`. A call that failed, was denied
    /// or was interrupted shows none, since it made no such change.
    #[must_use]
    pub fn stats_for(&self, phase: ToolPhase) -> &str {
        match phase {
            ToolPhase::Failed | ToolPhase::Denied | ToolPhase::Interrupted => "",
            _ => &self.stats,
        }
    }
}

fn field<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The definition an `agent` call starts. The tool defaults to
/// `general`.
fn agent_name(input: &Value) -> &str {
    match field(input, "agent") {
        "" => "general",
        name => name,
    }
}

fn read_range(input: &Value) -> String {
    let line = |key| input.get(key).and_then(Value::as_u64);
    match (line("start_line"), line("end_line")) {
        (Some(start), Some(end)) => format!("lines {start}-{end}"),
        (Some(start), None) => format!("from line {start}"),
        (None, Some(end)) => format!("lines 1-{end}"),
        (None, None) => String::new(),
    }
}

fn search_target(input: &Value) -> String {
    let pattern = format!("\"{}\"", field(input, "pattern"));
    match field(input, "path") {
        "" | "." => pattern,
        path => format!("{pattern} in {path}"),
    }
}

fn display_name(name: &str) -> String {
    match name.split_once("__") {
        Some((server, tool)) if !server.is_empty() && !tool.is_empty() => {
            format!("{server}.{tool}")
        }
        _ => name.to_owned(),
    }
}

fn arg_summary(input: &Value) -> String {
    match input.as_object() {
        Some(args) => args
            .iter()
            .map(|(k, v)| format!("{k}: {}", one_line(&value_text(v))))
            .collect::<Vec<_>>()
            .join(", "),
        None if input.is_null() => String::new(),
        None => one_line(&value_text(input)),
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn one_line(s: &str) -> String {
    let mut lines = s.trim().lines();
    let first = lines.next().unwrap_or_default();
    match lines.count() {
        0 => first.to_owned(),
        more => format!("{first} (+{more} {})", plural(more, "line", "lines")),
    }
}

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn verbs_and_target(name: &str, input: &Value) -> (&'static str, &'static str, String) {
        let l = describe(name, input);
        (l.verb_live, l.verb_done, l.target)
    }

    fn texts(lines: &[BodyLine]) -> Vec<(LineKind, &str)> {
        lines.iter().map(|l| (l.kind, l.text.as_str())).collect()
    }

    #[test]
    fn builtin_tools_get_verbs_and_targets() {
        let cases = [
            (
                "read",
                json!({"path": "README.md"}),
                "Reading",
                "Read",
                "README.md",
            ),
            (
                "write",
                json!({"path": "a.rs", "content": "x"}),
                "Writing",
                "Wrote",
                "a.rs",
            ),
            (
                "edit",
                json!({"path": "a.rs", "old_str": "x", "new_str": "y"}),
                "Editing",
                "Edited",
                "a.rs",
            ),
            (
                "bash",
                json!({"command": "cargo test"}),
                "Running",
                "Ran",
                "cargo test",
            ),
            ("ls", json!({"path": "src"}), "Listing", "Listed", "src"),
            ("ls", json!({}), "Listing", "Listed", "."),
            (
                "find",
                json!({"pattern": "**/*.rs"}),
                "Searching",
                "Searched",
                "\"**/*.rs\"",
            ),
            (
                "grep",
                json!({"pattern": "fn main", "path": "crates/"}),
                "Searching",
                "Searched",
                "\"fn main\" in crates/",
            ),
            (
                "grep",
                json!({"pattern": "foo", "path": "."}),
                "Searching",
                "Searched",
                "\"foo\"",
            ),
            (
                "web_fetch",
                json!({"url": "https://x.dev"}),
                "Fetching",
                "Fetched",
                "https://x.dev",
            ),
        ];
        for (name, input, live, done, target) in cases {
            assert_eq!(
                verbs_and_target(name, &input),
                (live, done, target.to_owned()),
                "{name}"
            );
        }
    }

    #[test]
    fn builtin_stats_bodies_and_read_only() {
        let read = describe(
            "read",
            &json!({"path": "a", "start_line": 2, "end_line": 4}),
        );
        assert_eq!(read.stats, "lines 2-4");
        assert!(read.read_only);
        assert_eq!(read.body, ToolBody::Hidden);

        let edit = describe(
            "edit",
            &json!({"path": "a", "old_str": "x", "new_str": "y\nz"}),
        );
        assert_eq!(edit.stats, "(+2 -1)");
        assert_eq!(edit.body, ToolBody::Diff);
        assert!(!edit.read_only);

        let write = describe("write", &json!({"path": "a", "content": "one\ntwo\n"}));
        assert_eq!(write.stats, "(2 lines)");

        assert_eq!(
            describe("bash", &json!({"command": "ls"})).body,
            ToolBody::Tail
        );
        assert!(describe("grep", &json!({"pattern": "x"})).read_only);
        assert!(describe("ls", &json!({})).read_only);
        assert!(!describe("web_fetch", &json!({"url": "u"})).read_only);
    }

    #[test]
    fn multiline_bash_command_reads_as_one_line() {
        let l = describe("bash", &json!({"command": "cd x\ncargo build\ncargo test"}));
        assert_eq!(l.target, "cd x (+2 lines)");
    }

    #[test]
    fn partial_input_describes_without_panicking() {
        assert_eq!(describe("edit", &Value::Null).target, "");
        assert_eq!(describe("edit", &json!({"path": "a"})).stats, "");
        assert_eq!(describe("bash", &json!({})).target, "");
    }

    #[test]
    fn mcp_tools_read_as_server_dot_tool() {
        let l = describe("github__create_issue", &json!({"repo": "qaidvoid/kage"}));
        assert_eq!((l.verb_live, l.verb_done), ("Calling", "Called"));
        assert_eq!(l.target, "github.create_issue");
        assert_eq!(l.stats, "repo: qaidvoid/kage");
        assert!(!l.read_only);
        assert_eq!(l.body, ToolBody::Head);
        assert_eq!(describe("__odd", &json!({})).target, "__odd");
    }

    #[test]
    fn unknown_tool_falls_back_to_called_with_a_summary() {
        let l = describe(
            "custom_tool",
            &json!({"count": 3, "note": "first\nsecond", "tags": ["a"]}),
        );
        assert_eq!((l.verb_done, l.target.as_str()), ("Called", "custom_tool"));
        assert_eq!(l.stats, "count: 3, note: first (+1 line), tags: [\"a\"]");
        assert_eq!(describe("custom_tool", &Value::Null).stats, "");
    }

    #[test]
    fn shorthand_edit_diff_lists_old_then_new() {
        let diff = edit_diff(&json!({"path": "a", "old_str": "a\nb", "new_str": "c"}));
        assert_eq!(
            texts(&diff.lines),
            [
                (LineKind::Delete, "a"),
                (LineKind::Delete, "b"),
                (LineKind::Add, "c"),
            ]
        );
        assert_eq!((diff.added, diff.removed), (1, 2));
    }

    #[test]
    fn changes_edit_diff_covers_substring_and_range_changes() {
        let diff = edit_diff(&json!({
            "path": "a",
            "changes": [
                {"old_str": "x", "new_str": "y"},
                {"range": {"start": 3, "end": 5}, "text": "one\ntwo\n"},
                {"range": {"start": 9, "end": 9}, "text": ""},
            ],
        }));
        assert_eq!(
            texts(&diff.lines),
            [
                (LineKind::Delete, "x"),
                (LineKind::Add, "y"),
                (LineKind::Marker, "lines 3-5"),
                (LineKind::Add, "one"),
                (LineKind::Add, "two"),
                (LineKind::Marker, "line 9"),
            ]
        );
        assert_eq!((diff.added, diff.removed), (3, 5));
    }

    #[test]
    fn bash_output_stdout_only() {
        let (lines, exit) = bash_output("stdout:\nhello\nworld\n\nexit: 0");
        assert_eq!(
            texts(&lines),
            [(LineKind::Text, "hello"), (LineKind::Text, "world")]
        );
        assert_eq!(exit, Some(BashExit::Code(0)));
    }

    #[test]
    fn bash_output_stderr_only_has_no_marker() {
        let (lines, exit) = bash_output("stderr:\noops\n\nexit: 0");
        assert_eq!(texts(&lines), [(LineKind::Text, "oops")]);
        assert_eq!(exit, Some(BashExit::Code(0)));
    }

    #[test]
    fn bash_output_both_streams_get_a_marker() {
        let (lines, _) = bash_output("stdout:\nout\n\nstderr:\nerr\n\nexit: 0");
        assert_eq!(
            texts(&lines),
            [
                (LineKind::Text, "out"),
                (LineKind::Marker, "stderr"),
                (LineKind::Text, "err"),
            ]
        );
    }

    #[test]
    fn bash_output_no_output() {
        let (lines, exit) = bash_output("(no output)\nexit: 0");
        assert!(lines.is_empty());
        assert_eq!(exit, Some(BashExit::Code(0)));
    }

    #[test]
    fn bash_output_non_zero_exit_and_signal() {
        let (lines, exit) = bash_output("stderr:\nboom\n\nexit: 101");
        assert_eq!(texts(&lines), [(LineKind::Text, "boom")]);
        assert_eq!(exit, Some(BashExit::Code(101)));

        let (lines, exit) = bash_output("(no output)\nexit: signal");
        assert!(lines.is_empty());
        assert_eq!(exit, Some(BashExit::Signal));
    }

    #[test]
    fn bash_output_without_exit_line_is_kept_whole() {
        let (lines, exit) = bash_output("bash timed out after 150ms");
        assert_eq!(
            texts(&lines),
            [(LineKind::Text, "bash timed out after 150ms")]
        );
        assert_eq!(exit, None);
    }

    #[test]
    fn questions_name_the_action() {
        assert_eq!(
            question("bash", &json!({"command": "ls"})),
            "Run this command?"
        );
        assert_eq!(
            question("edit", &json!({"path": "src/a.rs"})),
            "Edit src/a.rs?"
        );
        assert_eq!(question("write", &json!({"path": "b"})), "Write b?");
        assert_eq!(
            question("github__create_issue", &json!({})),
            "Allow github.create_issue?"
        );
        assert_eq!(question("edit", &json!({})), "Allow edit?");
    }

    #[test]
    fn agent_calls_read_as_the_agent_and_its_task() {
        let input = json!({"agent": "explore", "description": "map exports", "prompt": "p"});
        let l = describe("agent", &input);
        assert_eq!((l.verb_live, l.verb_done), ("Agent", "Agent"));
        assert_eq!(l.target, "explore: map exports");
        assert_eq!(l.body, ToolBody::Head);
        assert!(!l.read_only);
        assert_eq!(describe("agent", &json!({})).target, "general");
        assert_eq!(question("agent", &input), "Start agent explore?");
        assert_eq!(question("agent", &json!({})), "Start agent general?");
    }

    #[test]
    fn agent_output_strips_the_wrapper() {
        let wrap = |state: &str, body: &str| {
            format!("<agent name=\"explore\" session=\"01K\" state=\"{state}\">\n{body}\n</agent>")
        };
        let (lines, end) = agent_output(&wrap("completed", "one\ntwo"));
        assert_eq!(
            texts(&lines),
            [(LineKind::Text, "one"), (LineKind::Text, "two")]
        );
        assert_eq!(end, Some(AgentEnd::Done));

        let (lines, end) = agent_output(&wrap("cancelled", "so far"));
        assert_eq!(
            texts(&lines),
            [(LineKind::Text, "Stopped by you. Partial reply: so far")]
        );
        assert_eq!(end, Some(AgentEnd::Stopped));

        let (lines, _) = agent_output(&wrap("cancelled", NO_REPLY));
        assert_eq!(
            texts(&lines),
            [(LineKind::Text, "Stopped by you before it replied.")]
        );

        let (lines, end) = agent_output(&wrap("failed", "rate limited"));
        assert_eq!(texts(&lines), [(LineKind::Text, "rate limited")]);
        assert_eq!(end, Some(AgentEnd::Failed));

        let (lines, end) = agent_output("unknown agent x (valid agents: explore)");
        assert_eq!(lines.len(), 1);
        assert_eq!(end, None);
    }

    #[test]
    fn agent_stats_show_tokens_once_used() {
        assert_eq!(agent_stats(0, 0), "0 tools");
        assert_eq!(agent_stats(1, 0), "1 tool");
        assert_eq!(agent_stats(14, 22_000), "14 tools \u{b7} 22k tok");
    }

    #[test]
    fn arg_rows_align_keys_and_cut_values() {
        let rows = arg_rows(&json!({
            "repo": "qaidvoid/kage",
            "title": "Palette leads with quit",
            "body": "The slash palette lists quit first.\na\nb\nc",
            "n": 2,
        }));
        assert_eq!(
            rows,
            [
                (
                    "body ".to_owned(),
                    "The slash palette lists quit first. (+3 lines)".to_owned()
                ),
                ("n    ".to_owned(), "2".to_owned()),
                ("repo ".to_owned(), "qaidvoid/kage".to_owned()),
                ("title".to_owned(), "Palette leads with quit".to_owned()),
            ]
        );
        assert!(arg_rows(&Value::Null).is_empty());
    }

    #[test]
    fn group_summary_pluralizes() {
        let read = describe("read", &json!({"path": "a"}));
        let grep = describe("grep", &json!({"pattern": "x"}));
        let ls = describe("ls", &json!({}));
        assert_eq!(
            group_summary(&[read.clone(), grep.clone()]),
            "1 file, 1 search"
        );
        assert_eq!(
            group_summary(&[read.clone(), read, grep.clone(), grep, ls.clone(), ls]),
            "2 files, 2 directories, 2 searches"
        );
        assert_eq!(group_summary(&[]), "");
    }

    #[test]
    fn seconds_floor_then_switch_to_minutes() {
        assert_eq!(format_seconds(900), "0s");
        assert_eq!(format_seconds(1_700), "1s");
        assert_eq!(format_seconds(59_999), "59s");
        assert_eq!(format_seconds(65_000), "1m 05s");
    }

    #[test]
    fn verbs_follow_the_phase() {
        let edit = describe(
            "edit",
            &json!({"path": "a.rs", "old_str": "x", "new_str": "y"}),
        );
        let bash = describe("bash", &json!({"command": "make"}));
        for phase in [ToolPhase::Streaming, ToolPhase::Queued, ToolPhase::Waiting] {
            assert_eq!(bash.verb_for(phase, false), "Run");
        }
        assert_eq!(bash.verb_for(ToolPhase::Running, false), "Running");
        assert_eq!(bash.verb_for(ToolPhase::Done, false), "Ran");
        assert_eq!(bash.verb_for(ToolPhase::Failed, true), "Ran");
        assert_eq!(edit.verb_for(ToolPhase::Failed, false), "Edit");
        assert_eq!(edit.verb_for(ToolPhase::Denied, false), "Edit");
        assert_eq!(edit.verb_for(ToolPhase::Interrupted, false), "Edit");
        assert_eq!(edit.stats_for(ToolPhase::Done), "(+1 -1)");
        assert_eq!(edit.stats_for(ToolPhase::Failed), "");
        assert_eq!(edit.stats_for(ToolPhase::Denied), "");
    }

    #[test]
    fn elapsed_formatting() {
        assert_eq!(format_elapsed(50), "0.1s");
        assert_eq!(format_elapsed(4_900), "4.9s");
        assert_eq!(format_elapsed(9_950), "10s");
        assert_eq!(format_elapsed(14_000), "14s");
        assert_eq!(format_elapsed(65_000), "1m 05s");
    }
}
