//! Branch a new session from an existing one at a specific entry.
//!
//! [`fork`] copies the source's entries verbatim from the start of the file
//! up to and including the entry with id `at`. The destination header
//! carries a fresh [`SessionId`], a freshly-minted creation timestamp, and
//! `parent_session` / `parent_entry` fields linking back to the source.
//! All other header fields (cwd, model, system prompt) are inherited.
//!
//! Forking does not mutate the source. After a fork, both files exist
//! independently and may diverge.

use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::entry::{EntryId, FORMAT_VERSION, Header, SessionEntry, SessionId};
use crate::error::SessionError;
use crate::reader::SessionReader;
use crate::writer::SessionWriter;

/// Fork `src` into a new session file at `dst`, copying entries up to and
/// including the entry whose id equals `at`. The destination's session id
/// is `new_session`, which the caller is expected to have used when
/// constructing `dst` (so the file name and the in-file id agree).
///
/// # Errors
///
/// Errors if `src` cannot be opened, if its first entry is not a header,
/// if `dst` already exists, or if no entry in `src` has id `at`.
pub fn fork(
    src: &Path,
    dst: &Path,
    new_session: SessionId,
    at: EntryId,
) -> Result<(), SessionError> {
    let mut reader = SessionReader::iter(src)?;
    let first = reader
        .next()
        .ok_or_else(|| missing_header_error(src, "session is empty"))??;
    let SessionEntry::Header(parent_header) = first else {
        return Err(missing_header_error(src, "first entry is not a header"));
    };

    let new_header = Header {
        version: FORMAT_VERSION,
        session: new_session,
        id: EntryId::new(),
        ts: Utc::now(),
        cwd: parent_header.cwd.clone(),
        model: parent_header.model.clone(),
        system_prompt: parent_header.system_prompt.clone(),
        parent_session: Some(parent_header.session),
        parent_entry: Some(at),
    };

    if at == parent_header.id {
        SessionWriter::create(PathBuf::from(dst), new_header)?;
        return Ok(());
    }

    let mut writer = SessionWriter::create(PathBuf::from(dst), new_header)?;
    let mut copied_target = false;
    for item in reader {
        let entry = item?;
        let entry_id = entry.id();
        writer.append(&entry)?;
        if entry_id == at {
            copied_target = true;
            break;
        }
    }
    if !copied_target {
        // The user named an entry that doesn't exist in the source. The
        // partial dst file is still on disk; remove it so the caller is not
        // left with a confusing half-fork.
        let _ = std::fs::remove_file(dst);
        return Err(SessionError::Decode {
            path: src.to_path_buf(),
            line: 0,
            source: serde_json::from_str::<SessionEntry>(&format!(
                "{{\"err\":\"entry id {at} not found\"}}"
            ))
            .unwrap_err(),
        });
    }
    Ok(())
}

/// Resolve `prefix` against entry ids in `src`. Errors if zero or multiple
/// entries match.
pub fn resolve_entry_prefix(src: &Path, prefix: &str) -> Result<EntryId, SessionError> {
    let reader = SessionReader::iter(src)?;
    let mut matches: Vec<EntryId> = Vec::new();
    for item in reader {
        let entry = item?;
        let id = entry.id();
        if id.to_string().starts_with(prefix) {
            matches.push(id);
        }
    }
    if matches.is_empty() {
        return Err(SessionError::Io {
            path: src.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no entry id starts with '{prefix}'"),
            ),
        });
    }
    if matches.len() > 1 {
        return Err(SessionError::Io {
            path: src.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "entry id prefix '{prefix}' is ambiguous ({} matches)",
                    matches.len()
                ),
            ),
        });
    }
    Ok(matches.remove(0))
}

fn missing_header_error(path: &Path, msg: &str) -> SessionError {
    SessionError::Decode {
        path: path.to_path_buf(),
        line: 0,
        source: serde_json::from_str::<SessionEntry>(&format!("{{\"err\":\"{msg}\"}}"))
            .unwrap_err(),
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
    use crate::reader::SessionReader;
    use crate::writer::SessionWriter;

    fn fresh_header(model: &str) -> Header {
        Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: model.into(),
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

    fn write(path: &Path, header: Header, entries: &[SessionEntry]) {
        let mut w = SessionWriter::create(path, header).unwrap();
        for e in entries {
            w.append(e).unwrap();
        }
    }

    #[test]
    fn fork_copies_through_target_entry() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let parent_header = fresh_header("anthropic:claude");
        let parent_id = parent_header.session;
        let m1 = message_entry(Role::User, "first");
        let m1_id = m1.id();
        let m2 = message_entry(Role::Assistant, "second");
        let m3 = message_entry(Role::User, "third");
        write(&src, parent_header, &[m1.clone(), m2.clone(), m3.clone()]);

        let dst = dir.path().join("forked.jsonl");
        let new_id = SessionId::new();
        fork(&src, &dst, new_id, m1_id).unwrap();

        let entries: Vec<_> = SessionReader::iter(&dst)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Header + m1 only.
        assert_eq!(entries.len(), 2);
        let SessionEntry::Header(h) = &entries[0] else {
            panic!("expected header");
        };
        assert_eq!(h.session, new_id);
        assert_eq!(h.parent_session, Some(parent_id));
        assert_eq!(h.parent_entry, Some(m1_id));
        assert_eq!(h.model, "anthropic:claude");
        let SessionEntry::Message(m) = &entries[1] else {
            panic!("expected message");
        };
        match &m.message.content[0] {
            Content::Text { text } => assert_eq!(text, "first"),
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[test]
    fn fork_at_header_copies_only_header() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let parent_header = fresh_header("openai:gpt");
        let header_entry_id = parent_header.id;
        write(&src, parent_header, &[message_entry(Role::User, "hello")]);

        let dst = dir.path().join("forked.jsonl");
        fork(&src, &dst, SessionId::new(), header_entry_id).unwrap();

        let entries: Vec<_> = SessionReader::iter(&dst)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], SessionEntry::Header(_)));
    }

    #[test]
    fn fork_at_last_entry_copies_everything() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let parent_header = fresh_header("openai:gpt");
        let m1 = message_entry(Role::User, "a");
        let m2 = message_entry(Role::Assistant, "b");
        let last_id = m2.id();
        write(&src, parent_header, &[m1.clone(), m2.clone()]);

        let dst = dir.path().join("forked.jsonl");
        fork(&src, &dst, SessionId::new(), last_id).unwrap();
        let entries: Vec<_> = SessionReader::iter(&dst)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Header + m1 + m2.
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn fork_with_unknown_entry_id_errors_and_cleans_up() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        write(
            &src,
            fresh_header("x:y"),
            &[message_entry(Role::User, "only one")],
        );

        let dst = dir.path().join("forked.jsonl");
        let err = fork(&src, &dst, SessionId::new(), EntryId::new()).unwrap_err();
        assert!(matches!(err, SessionError::Decode { .. }));
        assert!(!dst.exists(), "fork should clean up its dst on error");
    }

    #[test]
    fn fork_refuses_to_overwrite() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        write(
            &src,
            fresh_header("x:y"),
            &[message_entry(Role::User, "hi")],
        );

        let dst = dir.path().join("dst.jsonl");
        std::fs::write(&dst, b"existing").unwrap();
        let err = fork(&src, &dst, SessionId::new(), EntryId::new()).unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }

    #[test]
    fn resolve_entry_prefix_finds_unique() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("s.jsonl");
        let m1 = message_entry(Role::User, "x");
        let id_str = m1.id().to_string();
        // Ulids generated in the same millisecond share a long prefix, so
        // we look up the entry by its full id here.
        write(&src, fresh_header("x:y"), std::slice::from_ref(&m1));
        let resolved = resolve_entry_prefix(&src, &id_str).unwrap();
        assert_eq!(resolved.to_string(), id_str);
    }

    #[test]
    fn resolve_entry_prefix_errors_on_no_match() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("s.jsonl");
        write(&src, fresh_header("x:y"), &[message_entry(Role::User, "x")]);
        let err = resolve_entry_prefix(&src, "ZZZZZZZZZZ").unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }
}
