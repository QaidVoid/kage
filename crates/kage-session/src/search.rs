//! Regex search across recorded sessions.
//!
//! [`search`] walks every `*.jsonl` file in a directory and reports each
//! matching line. The query is a regular expression; matching is done with
//! the same engine that powers `ripgrep` (`grep-regex` + `grep-searcher`).
//! Results carry the source path, the 1-based line number, and the raw
//! matched line so the caller can render or further parse it.

use std::path::{Path, PathBuf};

use grep::regex::RegexMatcher;
use grep::searcher::{Searcher, Sink, SinkError, SinkMatch};

use crate::entry::SessionEntry;
use crate::error::SessionError;

/// One match returned by [`search`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    /// Path of the session file containing the match.
    pub path: PathBuf,
    /// 1-based line number within the file.
    pub line_no: u64,
    /// Raw text of the matched line, with trailing newline stripped.
    pub line: String,
}

impl SearchHit {
    /// Best-effort decode of the matched line as a [`SessionEntry`]. Returns
    /// `None` for lines that aren't valid entries (which `kage` does not
    /// write but `kage search` may encounter in stray files).
    #[must_use]
    pub fn entry(&self) -> Option<SessionEntry> {
        serde_json::from_str(&self.line).ok()
    }
}

/// Run `query` against every `*.jsonl` file in `dir` and return all hits.
///
/// `query` is parsed as a regex. Hits are returned in directory-traversal
/// order; within a single file they appear in line order.
pub fn search(dir: &Path, query: &str) -> Result<Vec<SearchHit>, SessionError> {
    let matcher = RegexMatcher::new(query).map_err(|err| SessionError::Io {
        path: dir.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string()),
    })?;

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

    let mut hits = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|err| SessionError::Io {
            path: dir.to_path_buf(),
            source: err,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        search_one(&matcher, &path, &mut hits)?;
    }
    Ok(hits)
}

fn search_one(
    matcher: &RegexMatcher,
    path: &Path,
    hits: &mut Vec<SearchHit>,
) -> Result<(), SessionError> {
    let file = std::fs::File::open(path).map_err(|err| SessionError::Io {
        path: path.to_path_buf(),
        source: err,
    })?;
    let mut sink = HitSink {
        path: path.to_path_buf(),
        hits,
    };
    Searcher::new()
        .search_file(matcher, &file, &mut sink)
        .map_err(|err| SessionError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(err.to_string()),
        })?;
    Ok(())
}

struct HitSink<'a> {
    path: PathBuf,
    hits: &'a mut Vec<SearchHit>,
}

impl Sink for HitSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch) -> Result<bool, Self::Error> {
        let line_no = mat
            .line_number()
            .ok_or_else(|| std::io::Error::error_message("line number unavailable"))?;
        let raw = mat.bytes();
        let trimmed = raw.strip_suffix(b"\n").unwrap_or(raw);
        let line = String::from_utf8_lossy(trimmed).into_owned();
        self.hits.push(SearchHit {
            path: self.path.clone(),
            line_no,
            line,
        });
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use kage_core::{Content, Message, Role};
    use tempfile::tempdir;

    use super::*;
    use crate::entry::{EntryId, FORMAT_VERSION, Header, MessageEntry, SessionEntry, SessionId};
    use crate::writer::SessionWriter;

    fn fresh_header() -> Header {
        Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: "anthropic:claude".into(),
            system_prompt: "be helpful".into(),
            parent_session: None,
            parent_entry: None,
        }
    }

    fn message_entry(role: Role, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Message::new(
                role,
                vec![Content::Text {
                    text: text.to_owned(),
                }],
                None,
            ),
        })
    }

    fn write_session(path: &Path, prompt: &str) {
        let mut w = SessionWriter::create(path, fresh_header()).unwrap();
        w.append(&message_entry(Role::User, prompt)).unwrap();
    }

    #[test]
    fn search_finds_literal_match() {
        let dir = tempdir().unwrap();
        write_session(&dir.path().join("a.jsonl"), "what is migration about");
        write_session(&dir.path().join("b.jsonl"), "let's add a feature");

        let hits = search(dir.path(), "migration").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].line.contains("migration about"));
    }

    #[test]
    fn search_returns_empty_for_no_match() {
        let dir = tempdir().unwrap();
        write_session(&dir.path().join("a.jsonl"), "hello world");
        let hits = search(dir.path(), "absent").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_supports_regex() {
        let dir = tempdir().unwrap();
        write_session(&dir.path().join("a.jsonl"), "fix bug 1234");
        write_session(&dir.path().join("b.jsonl"), "fix typo");

        let hits = search(dir.path(), r"bug \d+").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_skips_non_jsonl_files() {
        let dir = tempdir().unwrap();
        write_session(&dir.path().join("real.jsonl"), "alpha");
        std::fs::write(dir.path().join("notes.txt"), b"alpha\n").unwrap();
        let hits = search(dir.path(), "alpha").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("real.jsonl"));
    }

    #[test]
    fn search_returns_empty_for_missing_dir() {
        let hits = search(Path::new("/nonexistent/dir/here"), "x").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn hit_can_decode_back_to_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write_session(&path, "decode me");
        let hits = search(dir.path(), "decode").unwrap();
        let entry = hits[0].entry().expect("decode succeeds");
        assert!(matches!(entry, SessionEntry::Message(_)));
    }

    #[test]
    fn invalid_regex_errors() {
        let dir = tempdir().unwrap();
        write_session(&dir.path().join("a.jsonl"), "hi");
        let err = search(dir.path(), "(unbalanced").unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }
}
