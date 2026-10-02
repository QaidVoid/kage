//! Agent text in a user message: the `<agent>` reports and `<message>`
//! mail the engine hands a session as a prompt of its own, such as a
//! background agent's result. A client shows them as report cards and
//! message rows instead of words the user typed.
//!
//! ```text
//! <agent name="general" session="01K6..." state="completed" run_ms="4200">
//! the reply
//! </agent>
//! ```

/// One element of agent text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentText {
    /// An agent's report.
    Report(Report),
    /// A message from another session.
    Mail {
        /// Who sent it: `kage` for a main session, else an agent's name.
        from: String,
        /// What it says.
        body: String,
    },
}

/// An agent's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The agent definition's name.
    pub name: String,
    /// The agent's session id.
    pub session: String,
    /// How the run stands: `started`, `completed`, `cancelled` or
    /// `failed`.
    pub state: String,
    /// The limit that ended the run: `turns`, `time` or `budget`.
    pub limit: Option<String>,
    /// How long the run took, when recorded.
    pub run_ms: Option<u64>,
    /// The reply, or the error the run ended on.
    pub body: String,
}

/// A user message's text split into the words the user typed in front
/// (empty when there are none) and the agent text after them. `None`
/// for plain words, or when an element does not read.
#[must_use]
pub fn split(text: &str) -> Option<(&str, Vec<AgentText>)> {
    let starts = |at: usize| at == 0 || text[..at].ends_with('\n');
    let first = ["<agent ", "<message "]
        .iter()
        .filter_map(|open| {
            text.match_indices(open)
                .map(|(at, _)| at)
                .find(|at| starts(*at))
        })
        .min()?;
    let mut parts = Vec::new();
    let mut rest = &text[first..];
    while !rest.is_empty() {
        let (tag, close) = if rest.starts_with("<agent ") {
            ("agent", "\n</agent>")
        } else if rest.starts_with("<message ") {
            ("message", "\n</message>")
        } else {
            return None;
        };
        let end = rest.find(close)? + close.len();
        let (head, body) = rest[..end - close.len()].split_once('\n')?;
        let attrs = head
            .strip_prefix('<')?
            .strip_prefix(tag)?
            .strip_suffix('>')?;
        let attr = |key: &str| attr(attrs, key);
        let body = body.replace(&format!("<\\/{tag}"), &format!("</{tag}"));
        parts.push(if tag == "agent" {
            AgentText::Report(Report {
                name: attr("name")?,
                session: attr("session")?,
                state: attr("state")?,
                limit: attr("limit"),
                run_ms: attr("run_ms").and_then(|ms| ms.parse().ok()),
                body,
            })
        } else {
            AgentText::Mail {
                from: attr("from")?,
                body,
            }
        });
        rest = rest[end..].trim_start();
    }
    Some((text[..first].trim(), parts))
}

/// The value of `key="..."` in an attribute list.
fn attr(attrs: &str, key: &str) -> Option<String> {
    let padded = format!(" {attrs}");
    let needle = format!(" {key}=\"");
    let start = padded.find(&needle)? + needle.len();
    let value = &padded[start..];
    Some(value[..value.find('"')?].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_after_steering_splits_into_its_parts() {
        let text = "stop after this\n\n\
            <agent name=\"general\" session=\"01K6\" state=\"completed\" limit=\"turns\" \
            tools=\"2\" run_ms=\"4200\">\nall <\\/agent> done\n</agent>\n\n\
            <message from=\"kage\" session=\"01K5\">\nalso this\n</message>";
        let (words, parts) = split(text).unwrap();
        assert_eq!(words, "stop after this");
        assert_eq!(
            parts,
            [
                AgentText::Report(Report {
                    name: "general".into(),
                    session: "01K6".into(),
                    state: "completed".into(),
                    limit: Some("turns".into()),
                    run_ms: Some(4200),
                    body: "all </agent> done".into(),
                }),
                AgentText::Mail {
                    from: "kage".into(),
                    body: "also this".into(),
                },
            ]
        );
    }

    #[test]
    fn plain_words_and_broken_elements_are_not_agent_text() {
        assert_eq!(split("hello"), None);
        assert_eq!(split("say <agent name=\"x\"> inline"), None);
        assert_eq!(
            split("<agent name=\"x\" session=\"y\" state=\"completed\">\nno end"),
            None
        );
    }
}
