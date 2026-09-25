//! Listing recorded sessions in a directory.
//!
//! [`list`] scans a directory for `*.jsonl` files, reads each one's header
//! plus the most recent user prompt, and returns the resulting summaries
//! sorted by creation time (newest first).
//!
//! A summary needs only the header, the entry after it, and the latest
//! entry, user message and title. So each file is scanned for line
//! boundaries and entry tags without decoding, and only those few lines
//! are decoded, found by walking back from the end.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::entry::{Header, SessionEntry, SessionId};
use crate::error::SessionError;

/// Kind of the [`SessionEntry::Custom`] entry that marks a session an
/// `agent` call started. Written as the first entry after the header.
pub const AGENT_ENTRY_KIND: &str = "kage:agent";

/// One row in `kage list`. Reflects the persisted state of a session file
/// at the moment of listing; subsequent appends will not be visible until
/// [`list`] is called again.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    /// Session id from the header.
    pub id: SessionId,
    /// Absolute path to the session file.
    pub path: PathBuf,
    /// Header creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Timestamp of the most recently appended entry.
    pub updated_at: DateTime<Utc>,
    /// Working directory recorded in the header.
    pub cwd: PathBuf,
    /// Provider-qualified model from the header.
    pub model: String,
    /// Parent session this one was forked from or spawned by, if any.
    /// Lets callers reconstruct the session forest from a flat directory
    /// listing.
    pub parent_session: Option<SessionId>,
    /// Text of the most recent user message, if any.
    pub last_user_prompt: Option<String>,
    /// Generated session title (latest `Title` entry), if one was
    /// written. `None` for pre-title sessions; callers fall back to
    /// [`Self::last_user_prompt`] for a label.
    pub title: Option<String>,
    /// Total number of valid entries (including the header).
    pub entry_count: usize,
    /// Agent definition name when an `agent` call started this session,
    /// read from the [`AGENT_ENTRY_KIND`] entry right after the header.
    pub agent: Option<String>,
}

/// Scan `dir` for `*.jsonl` session files and summarize each.
///
/// Files that fail to open or whose first entry is not a header are skipped
/// silently; this lets `kage list` tolerate stray files in the sessions
/// directory without aborting on the first malformed one. Files with a
/// torn trailing line are summarized using everything that did parse.
pub fn list(dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(d) => d,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(SessionError::Io {
                path: dir.to_path_buf(),
                source: err,
            });
        }
    };

    let mut summaries = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|err| SessionError::Io {
            path: dir.to_path_buf(),
            source: err,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(summary) = summarize_one(&path) {
            summaries.push(summary);
        }
    }
    summaries.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    Ok(summaries)
}

/// What a line's leading `"type"` tag says it holds. `Untagged` lines do
/// not start with the tag, so only decoding tells what they are.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Message,
    Title,
    Other,
    Untagged,
}

fn summarize_one(path: &Path) -> Option<SessionSummary> {
    let mut file = BufReader::new(File::open(path).ok()?);
    let mut buf = Vec::new();
    let mut offset = 0;
    let mut lines: Vec<(u64, Kind)> = Vec::new();
    let mut header = None;
    while let Ok(n) = file.read_until(b'\n', &mut buf) {
        if n == 0 {
            break;
        }
        let line = buf.trim_ascii_end();
        if !line.is_empty() {
            if header.is_none() {
                let Ok(SessionEntry::Header(h)) = serde_json::from_slice(line) else {
                    return None;
                };
                header = Some(h);
            } else {
                lines.push((offset, kind_of(line)));
            }
        }
        offset += n as u64;
        buf.clear();
    }
    let header = header?;

    let agent = lines
        .iter()
        .find_map(|&(at, _)| decode_at(&mut file, at, &mut buf))
        .and_then(|entry| match entry {
            SessionEntry::Custom(c) if c.kind == AGENT_ENTRY_KIND => {
                let name = c.data.get("agent").and_then(serde_json::Value::as_str);
                Some(name.unwrap_or_default().to_owned())
            }
            _ => None,
        });

    let mut updated_at = None;
    let mut last_user_prompt = None;
    let mut title = None;
    let mut invalid = 0;
    for &(at, kind) in lines.iter().rev() {
        let wanted = updated_at.is_none()
            || kind == Kind::Untagged
            || (kind == Kind::Message && last_user_prompt.is_none())
            || (kind == Kind::Title && title.is_none());
        if !wanted {
            continue;
        }
        let Some(entry) = decode_at(&mut file, at, &mut buf) else {
            invalid += 1;
            continue;
        };
        updated_at.get_or_insert(entry.ts());
        match entry {
            SessionEntry::Message(m)
                if m.message.role == kage_core::Role::User && last_user_prompt.is_none() =>
            {
                last_user_prompt = Some(first_text(&m.message));
            }
            SessionEntry::Title(t) if title.is_none() => title = Some(t.title),
            _ => {}
        }
        if updated_at.is_some() && last_user_prompt.is_some() && title.is_some() {
            break;
        }
    }
    let updated_at = updated_at.unwrap_or(header.ts);
    Some(summary_from_header(
        header,
        path.to_path_buf(),
        updated_at,
        last_user_prompt.flatten(),
        title,
        1 + lines.len() - invalid,
        agent,
    ))
}

fn kind_of(line: &[u8]) -> Kind {
    match leading_tag(line) {
        Some(b"message") => Kind::Message,
        Some(b"title") => Kind::Title,
        Some(_) => Kind::Other,
        None => Kind::Untagged,
    }
}

/// The `"type"` tag when it is the first key of `line`, the way the
/// writer emits entries.
fn leading_tag(line: &[u8]) -> Option<&[u8]> {
    let rest = line
        .trim_ascii_start()
        .strip_prefix(b"{")?
        .trim_ascii_start();
    let rest = rest.strip_prefix(b"\"type\"")?.trim_ascii_start();
    let rest = rest.strip_prefix(b":")?.trim_ascii_start();
    let rest = rest.strip_prefix(b"\"")?;
    let end = rest.iter().position(|&b| b == b'"')?;
    Some(&rest[..end])
}

/// Decode the entry on the line starting at byte `at`, or `None` when it
/// does not parse.
fn decode_at(file: &mut BufReader<File>, at: u64, buf: &mut Vec<u8>) -> Option<SessionEntry> {
    buf.clear();
    file.seek(SeekFrom::Start(at)).ok()?;
    file.read_until(b'\n', buf).ok()?;
    serde_json::from_slice(buf).ok()
}

fn first_text(message: &kage_core::Message) -> Option<String> {
    for block in &message.content {
        if let kage_core::Content::Text { text } = block {
            return Some(text.clone());
        }
    }
    None
}

fn summary_from_header(
    header: Header,
    path: PathBuf,
    updated_at: DateTime<Utc>,
    last_user_prompt: Option<String>,
    title: Option<String>,
    entry_count: usize,
    agent: Option<String>,
) -> SessionSummary {
    SessionSummary {
        id: header.session,
        path,
        created_at: header.ts,
        updated_at,
        cwd: header.cwd,
        model: header.model,
        parent_session: header.parent_session,
        last_user_prompt,
        title,
        entry_count,
        agent,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use kage_core::{Content, Message, Role};
    use tempfile::tempdir;

    use super::*;
    use crate::entry::{
        Custom, EntryId, FORMAT_VERSION, Header, Label, MessageEntry, SessionEntry, SessionId,
    };
    use crate::writer::SessionWriter;

    fn write_session(dir: &Path, name: &str, prompt: &str) -> PathBuf {
        let path = dir.join(name);
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "be helpful".into(),
            parent_session: None,
            parent_entry: None,
        };
        let mut writer = SessionWriter::create(&path, header).unwrap();
        writer
            .append(&SessionEntry::Message(MessageEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                message: Message::new(
                    Role::User,
                    vec![Content::Text {
                        text: prompt.to_owned(),
                    }],
                    None,
                ),
                usage: None,
            }))
            .unwrap();
        writer
            .append(&SessionEntry::Label(Label {
                id: EntryId::new(),
                ts: Utc::now(),
                text: "tail".into(),
                anchor: EntryId::new(),
            }))
            .unwrap();
        path
    }

    #[test]
    fn returns_empty_for_missing_dir() {
        let summaries = list(Path::new("/nonexistent/path/here")).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn returns_empty_for_empty_dir() {
        let dir = tempdir().unwrap();
        let summaries = list(dir.path()).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn summarizes_valid_sessions() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "a.jsonl", "ask one");
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_session(dir.path(), "b.jsonl", "ask two");

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 2);
        // Newest first.
        assert!(summaries[0].created_at >= summaries[1].created_at);
        assert!(
            summaries
                .iter()
                .any(|s| s.last_user_prompt.as_deref() == Some("ask one"))
        );
        assert!(
            summaries
                .iter()
                .any(|s| s.last_user_prompt.as_deref() == Some("ask two"))
        );
        for s in &summaries {
            assert_eq!(s.entry_count, 3);
            assert!(s.updated_at >= s.created_at);
        }
    }

    #[test]
    fn summary_carries_parent_session() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "root.jsonl", "root prompt");
        let parent = SessionId::new();
        let child_path = dir.path().join("child.jsonl");
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "be helpful".into(),
            parent_session: Some(parent),
            parent_entry: Some(EntryId::new()),
        };
        SessionWriter::create(&child_path, header).unwrap();

        let summaries = list(dir.path()).unwrap();
        let root = summaries
            .iter()
            .find(|s| s.path.ends_with("root.jsonl"))
            .unwrap();
        let child = summaries
            .iter()
            .find(|s| s.path.ends_with("child.jsonl"))
            .unwrap();
        assert_eq!(root.parent_session, None);
        assert_eq!(child.parent_session, Some(parent));
    }

    #[test]
    fn ignores_non_jsonl_files() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "real.jsonl", "hi");
        std::fs::write(dir.path().join("notes.txt"), b"random").unwrap();
        std::fs::write(dir.path().join("README"), b"random").unwrap();

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
    }

    #[test]
    fn summary_title_is_none_without_a_title_entry() {
        let dir = tempdir().unwrap();
        write_session(dir.path(), "a.jsonl", "hello");
        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries[0].title, None, "pre-title sessions have None");
        assert_eq!(summaries[0].last_user_prompt.as_deref(), Some("hello"));
    }

    #[test]
    fn latest_title_entry_wins_in_summary() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "t.jsonl", "do a thing");
        let mut writer = crate::writer::SessionWriter::open(&path).unwrap();
        for t in ["first title", "better title"] {
            writer
                .append(&SessionEntry::Title(crate::SessionTitle {
                    id: EntryId::new(),
                    ts: Utc::now(),
                    title: t.to_owned(),
                }))
                .unwrap();
        }
        drop(writer);
        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries[0].title.as_deref(), Some("better title"));
    }

    #[test]
    fn skips_files_without_header_first() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("garbage.jsonl"), b"{\"oops\":true}\n").unwrap();
        write_session(dir.path(), "good.jsonl", "ok");

        let summaries = list(dir.path()).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].last_user_prompt.as_deref(), Some("ok"));
    }

    fn write_agent_session(dir: &Path, name: &str, first_is_agent: bool) -> PathBuf {
        let path = dir.join(name);
        let header = Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "explore".into(),
            parent_session: Some(SessionId::new()),
            parent_entry: None,
        };
        let marker = SessionEntry::Custom(Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: AGENT_ENTRY_KIND.into(),
            data: serde_json::json!({ "agent": "explore", "description": "map exports" }),
        });
        let title = SessionEntry::Title(crate::SessionTitle {
            id: EntryId::new(),
            ts: Utc::now(),
            title: "map exports".into(),
        });
        let mut writer = SessionWriter::create(&path, header).unwrap();
        let entries = if first_is_agent {
            [marker, title]
        } else {
            [title, marker]
        };
        for entry in &entries {
            writer.append(entry).unwrap();
        }
        path
    }

    #[test]
    fn summary_reports_the_agent_of_a_spawned_session() {
        let dir = tempdir().unwrap();
        write_agent_session(dir.path(), "agent.jsonl", true);
        write_agent_session(dir.path(), "late.jsonl", false);
        write_session(dir.path(), "plain.jsonl", "hi");

        let summaries = list(dir.path()).unwrap();
        let agent_of = |file: &str| {
            summaries
                .iter()
                .find(|s| s.path.ends_with(file))
                .unwrap()
                .agent
                .clone()
        };
        assert_eq!(agent_of("agent.jsonl").as_deref(), Some("explore"));
        assert_eq!(agent_of("late.jsonl"), None);
        assert_eq!(agent_of("plain.jsonl"), None);
    }

    /// The summary a full decode of every entry gives, to check that
    /// decoding only the needed lines agrees with it.
    fn full_decode(path: &Path) -> SessionSummary {
        let mut entries = crate::reader::SessionReader::iter(path)
            .unwrap()
            .filter_map(Result::ok);
        let Some(SessionEntry::Header(header)) = entries.next() else {
            panic!("no header");
        };
        let mut updated_at = header.ts;
        let (mut last_user_prompt, mut title, mut agent) = (None, None, None);
        let mut entry_count = 1;
        for entry in entries {
            entry_count += 1;
            updated_at = entry.ts();
            match entry {
                SessionEntry::Custom(c) if entry_count == 2 && c.kind == AGENT_ENTRY_KIND => {
                    agent = Some(c.data["agent"].as_str().unwrap_or_default().to_owned());
                }
                SessionEntry::Message(m) if m.message.role == Role::User => {
                    last_user_prompt = first_text(&m.message);
                }
                SessionEntry::Title(t) => title = Some(t.title),
                _ => {}
            }
        }
        summary_from_header(
            header,
            path.to_path_buf(),
            updated_at,
            last_user_prompt,
            title,
            entry_count,
            agent,
        )
    }

    fn message(role: Role, content: Vec<Content>) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Message::new(role, content, None),
            usage: None,
        })
    }

    fn text(text: &str) -> Vec<Content> {
        vec![Content::Text { text: text.into() }]
    }

    fn append_raw(path: &Path, line: &str) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(line.as_bytes()).unwrap();
    }

    fn title_entry(title: &str) -> SessionEntry {
        SessionEntry::Title(crate::SessionTitle {
            id: EntryId::new(),
            ts: Utc::now(),
            title: title.into(),
        })
    }

    #[test]
    fn early_title_and_last_user_prompt_survive_a_long_tail() {
        let dir = tempdir().unwrap();
        let path = write_agent_session(dir.path(), "long.jsonl", true);
        let mut writer = SessionWriter::open(&path).unwrap();
        writer
            .append(&message(Role::User, text("first ask")))
            .unwrap();
        writer.append(&title_entry("early title")).unwrap();
        writer
            .append(&message(Role::User, text("second ask")))
            .unwrap();
        for i in 0..50 {
            writer
                .append(&message(Role::Assistant, text(&format!("step {i}"))))
                .unwrap();
            writer
                .append(&message(Role::ToolResult, text(&format!("output {i}"))))
                .unwrap();
        }
        drop(writer);

        let summary = summarize_one(&path).unwrap();
        assert_eq!(summary, full_decode(&path));
        assert_eq!(summary.title.as_deref(), Some("early title"));
        assert_eq!(summary.last_user_prompt.as_deref(), Some("second ask"));
        assert_eq!(summary.agent.as_deref(), Some("explore"));
        assert_eq!(summary.entry_count, 106);
    }

    #[test]
    fn a_textless_last_user_message_clears_the_prompt() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "img.jsonl", "with text");
        let mut writer = SessionWriter::open(&path).unwrap();
        let image = Content::Image {
            source: kage_core::ImageSource::Url {
                url: "https://example.com/a.png".into(),
            },
            mime: "image/png".into(),
        };
        writer.append(&message(Role::User, vec![image])).unwrap();
        drop(writer);

        let summary = summarize_one(&path).unwrap();
        assert_eq!(summary, full_decode(&path));
        assert_eq!(summary.last_user_prompt, None);
    }

    #[test]
    fn a_torn_trailing_line_is_not_counted_or_dated() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "torn.jsonl", "hi");
        append_raw(&path, "{\"type\":\"title\",\"id\":\"01");

        let summary = summarize_one(&path).unwrap();
        assert_eq!(summary, full_decode(&path));
        assert_eq!(summary.entry_count, 3);
        assert_eq!(summary.title, None);
    }

    #[test]
    fn an_entry_whose_tag_is_not_first_is_still_read() {
        let dir = tempdir().unwrap();
        let path = write_session(dir.path(), "late.jsonl", "hi");
        let mut writer = SessionWriter::open(&path).unwrap();
        writer.append(&title_entry("tagged")).unwrap();
        drop(writer);
        let line = format!(
            "{{\"title\":\"untagged\",\"id\":{},\"ts\":{},\"type\":\"title\"}}\n\n",
            serde_json::to_string(&EntryId::new()).unwrap(),
            serde_json::to_string(&Utc::now()).unwrap(),
        );
        append_raw(&path, &line);

        let summary = summarize_one(&path).unwrap();
        assert_eq!(summary, full_decode(&path));
        assert_eq!(summary.title.as_deref(), Some("untagged"));
        assert_eq!(summary.entry_count, 5);
    }
}
