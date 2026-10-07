//! The text an agent's run reports back to the session that started
//! it: one `<agent>` element naming the agent, its session and how the
//! run ended, with the reply as its body.
//!
//! The same element is a foreground `agent` call's result, a background
//! agent's report delivered as a message, and what a resumed session's
//! agent list is rebuilt from, so it is written and read in one place.
//!
//! ```text
//! <agent name="general" session="01K6..." state="completed" model="anthropic/claude"
//!   tools="3" in="1200" out="300" cache_read="0" cache_write="0" cost="0.0120" ctx="4000"
//!   win="200000" run_ms="4200">
//! the reply
//! </agent>
//! ```
//!
//! A report without stats (a refusal, or a background agent that just
//! started) carries only the name, session and state.
//!
//! [`AgentMail`] is the `<message>` element one session sends another
//! through `send_message`.

use std::fmt::Write as _;

use crate::TokenUsage;
use crate::protocol::{SessionId, Usage};

/// How an agent's run stands, as its report says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    /// A background agent started; its result comes later.
    Started,
    /// The run finished.
    Completed,
    /// The run was stopped.
    Cancelled,
    /// The run failed, or never started.
    Failed,
}

impl ReportState {
    /// The `state` attribute's value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "started" => Self::Started,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// Why an agent's run stopped before it was done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentLimit {
    /// It reached its turn limit.
    Turns,
    /// It ran out of time.
    Time,
    /// The agents of its session spent their token budget.
    Budget,
}

impl AgentLimit {
    /// The `limit` attribute's value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turns => "turns",
            Self::Time => "time",
            Self::Budget => "budget",
        }
    }

    /// How a client names the limit: `turn limit`, `timed out`,
    /// `over budget`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Turns => "turn limit",
            Self::Time => "timed out",
            Self::Budget => "over budget",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "turns" => Self::Turns,
            "time" => Self::Time,
            "budget" => Self::Budget,
            _ => return None,
        })
    }
}

/// What a finished run recorded about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReportStats {
    /// The model it ran, as `provider/model`.
    pub model: String,
    /// The tool calls it made.
    pub tool_calls: u32,
    /// Its token totals, context fill and cost.
    pub usage: Usage,
    /// How long it ran, in milliseconds, when recorded.
    pub run_ms: Option<u64>,
}

/// One agent's report.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentReport {
    /// The agent definition's name.
    pub name: String,
    /// The agent's session.
    pub session: SessionId,
    /// How its run stands.
    pub state: ReportState,
    /// Why the run stopped early, when a limit stopped it.
    pub limit: Option<AgentLimit>,
    /// What the run recorded; `None` for a refusal or a start.
    pub stats: Option<ReportStats>,
    /// The reply, the error, or what a start tells the model.
    pub body: String,
}

impl AgentReport {
    /// The report as the `<agent>` element, its body escaped so it
    /// cannot close the element early.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "<agent name=\"{}\" session=\"{}\" state=\"{}\"",
            self.name,
            self.session,
            self.state.as_str()
        );
        if let Some(limit) = self.limit {
            let _ = write!(text, " limit=\"{}\"", limit.as_str());
        }
        if let Some(stats) = &self.stats {
            let usage = &stats.usage;
            let _ = write!(
                text,
                " model=\"{}\" tools=\"{}\" in=\"{}\" out=\"{}\" cache_read=\"{}\" \
                 cache_write=\"{}\" cost=\"{:.4}\" ctx=\"{}\" win=\"{}\"",
                stats.model,
                stats.tool_calls,
                usage.total.input,
                usage.total.output,
                usage.total.cache_read,
                usage.total.cache_write,
                usage.cost,
                usage.context_used,
                usage.context_window,
            );
            if let Some(run_ms) = stats.run_ms {
                let _ = write!(text, " run_ms=\"{run_ms}\"");
            }
        }
        let _ = write!(
            text,
            ">\n{}\n</agent>",
            self.body
                .replace('\\', "\\\\")
                .replace("</agent", "<\\/agent")
        );
        text
    }

    /// The report `text` holds, when it is one `<agent>` element, with
    /// its body unescaped. Plain text and other elements are not.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (header, rest) = text.split_once('\n')?;
        let mut report = Self::header(header)?;
        let body = rest.strip_suffix("</agent>")?;
        let body = body.strip_suffix('\n').unwrap_or(body);
        report.body = body.replace("<\\/agent", "</agent").replace("\\\\", "\\");
        Some(report)
    }

    /// Every report `text` holds: a delivered message joins several
    /// with blank lines. Text around and between them is skipped.
    #[must_use]
    pub fn all_in(text: &str) -> Vec<Self> {
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find("<agent ") {
            let from = &rest[start..];
            let Some(end) = from.find("\n</agent>") else {
                break;
            };
            let element = &from[..end + "\n</agent>".len()];
            found.extend(Self::parse(element));
            rest = &from[element.len()..];
        }
        found
    }

    /// The report an `<agent ...>` header line describes, without a
    /// body. Attributes an older transcript lacks read as zero.
    /// Leading padding on the line is ignored.
    #[must_use]
    pub fn header(line: &str) -> Option<Self> {
        let attrs = line
            .trim_start()
            .strip_prefix("<agent ")?
            .split_once('>')?
            .0;
        let attr = |key: &str| attr_value(attrs, key);
        let num =
            |key: &str| -> u64 { attr(key).and_then(|value| value.parse().ok()).unwrap_or(0) };
        let session = ulid::Ulid::from_string(attr("session")?).ok()?;
        let recorded = ["tools", "model", "in", "out", "cost", "run_ms"]
            .iter()
            .any(|key| attr(key).is_some());
        let stats = recorded.then(|| ReportStats {
            model: attr("model")
                .map(crate::canonical_model)
                .unwrap_or_default(),
            tool_calls: u32::try_from(num("tools")).unwrap_or(u32::MAX),
            usage: Usage {
                total: TokenUsage {
                    input: num("in"),
                    output: num("out"),
                    cache_read: num("cache_read"),
                    cache_write: num("cache_write"),
                },
                context_used: num("ctx"),
                context_window: num("win"),
                cost: attr("cost")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0.0),
            },
            run_ms: attr("run_ms").and_then(|value| value.parse().ok()),
        });
        Some(Self {
            name: attr("name")?.to_owned(),
            session: SessionId(session),
            state: ReportState::parse(attr("state")?)?,
            limit: attr("limit").and_then(AgentLimit::parse),
            stats,
            body: String::new(),
        })
    }
}

/// A message one session of a conversation sends another, as the
/// `<message>` element the target reads.
///
/// ```text
/// <message from="kage" session="01K6...">
/// Also run the doc tests.
/// </message>
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMail {
    /// Who sent it: `kage` for a main session, else the agent's name.
    pub from: String,
    /// The sender's session.
    pub session: SessionId,
    /// What it says.
    pub body: String,
}

impl AgentMail {
    /// The mail as the `<message>` element, its body escaped so it
    /// cannot close the element early.
    #[must_use]
    pub fn to_text(&self) -> String {
        format!(
            "<message from=\"{}\" session=\"{}\">\n{}\n</message>",
            self.from,
            self.session,
            self.body
                .replace('\\', "\\\\")
                .replace("</message", "<\\/message")
        )
    }

    /// The mail `text` holds, when it is one `<message>` element, with
    /// its body unescaped.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (header, rest) = text.split_once('\n')?;
        let attrs = header.strip_prefix("<message ")?.strip_suffix('>')?;
        let session = ulid::Ulid::from_string(attr_value(attrs, "session")?).ok()?;
        let body = rest.strip_suffix("</message>")?;
        let body = body.strip_suffix('\n').unwrap_or(body);
        Some(Self {
            from: attr_value(attrs, "from")?.to_owned(),
            session: SessionId(session),
            body: body
                .replace("<\\/message", "</message")
                .replace("\\\\", "\\"),
        })
    }
}

/// One element of agent text in a user message.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentText {
    /// An agent's report.
    Report(AgentReport),
    /// A message from another session.
    Mail(AgentMail),
}

/// A burst of agent text in a user message, split into the person's
/// own words in front, the reports and messages, and the human words
/// interleaved between and after the elements. A run reads a burst of
/// agent text as one message, after any steering the user typed.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentBurst<'a> {
    /// The words before the first element, empty when the burst opens
    /// with one.
    pub words: &'a str,
    /// The human words between and after the elements, each trimmed,
    /// in order. Empty when the elements run back to back.
    pub prose: Vec<&'a str>,
    /// The reports and messages, in order.
    pub parts: Vec<AgentText>,
}

/// Splits a user message into its agent elements and the words around
/// them. Elements start at the front of the text or on their own
/// line; human words between them come back in [`AgentBurst::prose`]
/// instead of refusing the burst. `None` when the text holds no
/// agent element, or one that does not parse: it is plain user text
/// then.
#[must_use]
pub fn split_agent_text(text: &str) -> Option<AgentBurst<'_>> {
    let at_line_start = |rest: &str, at: usize| at == 0 || rest[..at].ends_with('\n');
    let opens = ["<agent ", "<message "];
    let next_open = |rest: &str| {
        opens
            .iter()
            .filter_map(|open| {
                rest.match_indices(open)
                    .map(|(at, _)| at)
                    .find(|at| at_line_start(rest, *at))
            })
            .min()
    };
    let first = next_open(text)?;
    let mut burst = AgentBurst {
        words: text[..first].trim(),
        prose: Vec::new(),
        parts: Vec::new(),
    };
    let mut rest = &text[first..];
    while !rest.is_empty() {
        let (close, report) = if rest.starts_with("<agent ") {
            ("\n</agent>", true)
        } else if rest.starts_with("<message ") {
            ("\n</message>", false)
        } else if let Some(next) = next_open(rest) {
            let words = rest[..next].trim();
            if !words.is_empty() {
                burst.prose.push(words);
            }
            rest = &rest[next..];
            continue;
        } else {
            let words = rest.trim();
            if !words.is_empty() {
                burst.prose.push(words);
            }
            break;
        };
        let end = rest.find(close)? + close.len();
        let element = &rest[..end];
        burst.parts.push(if report {
            AgentText::Report(AgentReport::parse(element)?)
        } else {
            AgentText::Mail(AgentMail::parse(element)?)
        });
        rest = &rest[end..];
    }
    Some(burst)
}

/// The value of `key="..."` in an attribute list, matched as a whole
/// attribute name so `xkey="..."` never matches a `key` lookup. Shared
/// by the report parser and the swarm aggregate parser.
pub(crate) fn attr_value<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=\"");
    let padded = format!(" {attrs}");
    let start = padded.find(&needle)? + needle.len();
    let value = &padded[start..];
    let end = value.find('"')?;
    // `padded` is `attrs` with one leading space, so the slice maps
    // back into `attrs` one byte earlier.
    let from = start - 1;
    Some(&attrs[from..from + end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionId {
        SessionId(ulid::Ulid::from_string("01K62W8Q3T9V5M2C7X4B1N0R6S").unwrap())
    }

    fn finished(state: ReportState) -> AgentReport {
        AgentReport {
            name: "general".into(),
            session: session(),
            state,
            limit: None,
            stats: Some(ReportStats {
                model: "anthropic/claude".into(),
                tool_calls: 3,
                usage: Usage {
                    total: TokenUsage {
                        input: 1200,
                        output: 300,
                        cache_read: 5,
                        cache_write: 6,
                    },
                    context_used: 4000,
                    context_window: 200_000,
                    cost: 0.012,
                },
                run_ms: Some(4200),
            }),
            body: "the reply".into(),
        }
    }

    #[test]
    fn a_finished_report_reads_as_the_engine_has_always_written_it() {
        assert_eq!(
            finished(ReportState::Completed).to_text(),
            "<agent name=\"general\" session=\"01K62W8Q3T9V5M2C7X4B1N0R6S\" state=\"completed\" \
             model=\"anthropic/claude\" tools=\"3\" in=\"1200\" out=\"300\" cache_read=\"5\" \
             cache_write=\"6\" cost=\"0.0120\" ctx=\"4000\" win=\"200000\" run_ms=\"4200\">\n\
             the reply\n</agent>"
        );
    }

    #[test]
    fn every_state_and_limit_round_trips() {
        for state in [
            ReportState::Started,
            ReportState::Completed,
            ReportState::Cancelled,
            ReportState::Failed,
        ] {
            for limit in [
                None,
                Some(AgentLimit::Turns),
                Some(AgentLimit::Time),
                Some(AgentLimit::Budget),
            ] {
                let mut report = finished(state);
                report.limit = limit;
                assert_eq!(AgentReport::parse(&report.to_text()), Some(report));
            }
        }
        let bare = AgentReport {
            stats: None,
            ..finished(ReportState::Failed)
        };
        assert_eq!(
            bare.to_text(),
            "<agent name=\"general\" session=\"01K62W8Q3T9V5M2C7X4B1N0R6S\" state=\"failed\">\nthe reply\n</agent>"
        );
        assert_eq!(AgentReport::parse(&bare.to_text()), Some(bare));
    }

    #[test]
    fn a_closing_tag_in_the_body_is_escaped_and_restored() {
        let mut report = finished(ReportState::Completed);
        report.body = "before </agent> after".into();
        let text = report.to_text();
        assert_eq!(text.matches("</agent>").count(), 1, "{text}");
        assert_eq!(
            AgentReport::parse(&text).unwrap().body,
            "before </agent> after"
        );
    }

    #[test]
    fn several_reports_in_one_message_are_all_found() {
        let one = finished(ReportState::Completed);
        let mut two = finished(ReportState::Cancelled);
        two.limit = Some(AgentLimit::Time);
        let text = format!("{}\n\n{}", one.to_text(), two.to_text());
        assert_eq!(AgentReport::all_in(&text), vec![one, two]);
        assert!(AgentReport::all_in("no reports here").is_empty());
    }

    #[test]
    fn plain_text_and_other_elements_are_not_reports() {
        assert_eq!(AgentReport::parse("hello"), None);
        assert_eq!(
            AgentReport::parse("<message from=\"kage\">\nhi\n</message>"),
            None
        );
        assert_eq!(
            AgentReport::parse(
                "<agent name=\"x\" session=\"bad\" state=\"completed\">\nhi\n</agent>"
            ),
            None
        );
    }

    #[test]
    fn mail_round_trips_with_its_closing_tag_escaped() {
        let mail = AgentMail {
            from: "kage".into(),
            session: session(),
            body: "run the doc tests </message> too".into(),
        };
        let text = mail.to_text();
        assert!(
            text.starts_with(
                "<message from=\"kage\" session=\"01K62W8Q3T9V5M2C7X4B1N0R6S\">\nrun the doc"
            ),
            "{text}"
        );
        assert_eq!(text.matches("</message>").count(), 1, "{text}");
        assert_eq!(AgentMail::parse(&text), Some(mail));
        assert_eq!(AgentMail::parse("plain"), None);
        assert_eq!(
            AgentMail::parse(&finished(ReportState::Completed).to_text()),
            None
        );
    }

    #[test]
    fn agent_text_splits_from_the_words_in_front() {
        let report = finished(ReportState::Completed);
        let mail = AgentMail {
            from: "kage".into(),
            session: session(),
            body: "hi\n\nthere".into(),
        };
        let burst = format!("{}\n\n{}", report.to_text(), mail.to_text());
        assert_eq!(
            split_agent_text(&burst),
            Some(AgentBurst {
                words: "",
                prose: Vec::new(),
                parts: vec![AgentText::Report(report.clone()), AgentText::Mail(mail)]
            })
        );
        let steered = format!("stop after this\n\n{}", report.to_text());
        assert_eq!(
            split_agent_text(&steered),
            Some(AgentBurst {
                words: "stop after this",
                prose: Vec::new(),
                parts: vec![AgentText::Report(report.clone())]
            })
        );
        assert_eq!(split_agent_text("plain words"), None);
        let broken = format!("{} and more", report.to_text());
        assert_eq!(
            split_agent_text(&broken),
            Some(AgentBurst {
                words: "",
                prose: vec!["and more"],
                parts: vec![AgentText::Report(report.clone())]
            })
        );
        assert_eq!(
            split_agent_text(
                "<agent name=\"x\" session=\"bad\" state=\"completed\">\nhi\n</agent>"
            ),
            None,
            "an element that does not parse is plain user text"
        );
    }

    #[test]
    fn prose_between_reports_is_kept_and_both_reports_parse() {
        let one = finished(ReportState::Completed);
        let mut two = finished(ReportState::Cancelled);
        two.limit = Some(AgentLimit::Time);
        let text = format!(
            "{}\nnote between\n{}\nnote after",
            one.to_text(),
            two.to_text()
        );
        let burst = split_agent_text(&text).expect("prose does not refuse the burst");
        assert_eq!(burst.words, "");
        assert_eq!(burst.prose, ["note between", "note after"]);
        assert_eq!(
            burst.parts,
            [AgentText::Report(one), AgentText::Report(two)]
        );
    }

    #[test]
    fn an_old_header_without_stats_or_model_reads() {
        let old = AgentReport::header(
            "<agent name=\"general\" session=\"01K62W8Q3T9V5M2C7X4B1N0R6S\" state=\"completed\" tools=\"2\">",
        )
        .unwrap();
        let stats = old.stats.unwrap();
        assert_eq!(stats.tool_calls, 2);
        assert_eq!(stats.model, "");
        assert_eq!(stats.run_ms, None);
    }

    #[test]
    fn a_padded_header_line_still_reads() {
        let report = AgentReport::header(
            "  <agent name=\"general\" session=\"01K62W8Q3T9V5M2C7X4B1N0R6S\" state=\"completed\" tools=\"2\">",
        )
        .expect("leading padding is ignored");
        assert_eq!(report.name, "general");
        assert_eq!(report.stats.unwrap().tool_calls, 2);
        assert_eq!(
            AgentReport::header("plain text line"),
            None,
            "text without the element is not a header"
        );
    }
}
