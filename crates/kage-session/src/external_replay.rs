//! Display-only replay of persisted external ACP presentation events.
//!
//! This does not reconstruct agent state, native history, or permission decisions.

use std::io;
use std::path::Path;

use crate::entry::{ExternalAcpPresentation, Header, SessionBackend, SessionEntry};
use crate::{SessionError, SessionTree};

/// Presentation events on the active branch of an external ACP session.
#[derive(Debug)]
pub struct ExternalReplay {
    /// Validated version 4 external session header.
    pub header: Header,
    /// Display events in active-chain order, without merging fragments.
    pub events: Vec<ExternalAcpPresentation>,
    /// Whether an invalid unterminated trailing line was discarded as torn.
    pub truncated: bool,
}

/// Read display events from the active chain of an external ACP session.
///
/// Non-presentation entries are ignored. The entire file, including abandoned
/// branches, is decoded and checked by [`SessionTree`]. An invalid unterminated
/// trailing line is treated as torn, and reported through [`ExternalReplay::truncated`].
/// Events are not checked for turn pairing or tool call consistency. They must
/// not be used as native engine history, upstream state, or permission decisions.
///
/// # Errors
///
/// Returns an error for an invalid session tree, malformed reserved record,
/// unsupported header version, or a header without an external ACP backend.
pub fn replay_external(path: &Path) -> Result<ExternalReplay, SessionError> {
    let tree = SessionTree::read(path)?;
    let header = tree.header();
    if header.version != 4 || !matches!(header.backend, Some(SessionBackend::ExternalAcp(_))) {
        return Err(SessionError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "external ACP replay requires a version 4 external ACP header",
            ),
        });
    }
    let mut events = Vec::new();
    for entry in tree.ancestry(tree.head()) {
        if let SessionEntry::Custom(custom) = entry {
            let event = ExternalAcpPresentation::from_custom(custom).map_err(|source| {
                SessionError::Io {
                    path: path.to_path_buf(),
                    source: io::Error::new(io::ErrorKind::InvalidData, source),
                }
            })?;
            events.extend(event);
        }
    }
    Ok(ExternalReplay {
        header: header.clone(),
        events,
        truncated: tree.truncated(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{
        BranchEntry, Custom, EntryId, ExternalAcpSession, PresentationText, SessionId,
    };
    use chrono::Utc;
    use serde_json::json;
    use tempfile::tempdir;

    fn header() -> Header {
        Header {
            version: 4,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: "/tmp".into(),
            model: String::new(),
            system_prompt: String::new(),
            backend: Some(SessionBackend::ExternalAcp(ExternalAcpSession {
                agent_name: "agent".into(),
                protocol_version: 1,
                upstream_session_id: "upstream".into(),
            })),
            parent_session: None,
            parent_entry: None,
        }
    }

    fn prompt(text: &str) -> SessionEntry {
        ExternalAcpPresentation::Prompt {
            text: PresentationText::new(text.to_owned()).unwrap(),
        }
        .into_entry(EntryId::new(), Utc::now())
    }

    fn write(path: &Path, entries: &[SessionEntry]) {
        let lines = entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap() + "\n")
            .collect::<String>();
        std::fs::write(path, lines).unwrap();
    }

    #[test]
    fn active_events_preserve_order_and_skip_other_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session");
        let h = header();
        let first = prompt("first");
        let abandoned = prompt("abandoned");
        let branch = SessionEntry::Branch(BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at: first.id(),
        });
        let other = SessionEntry::Custom(Custom {
            id: EntryId::new(),
            ts: Utc::now(),
            kind: "plugin:other".into(),
            data: json!({}),
        });
        let last = prompt("last");
        write(
            &path,
            &[
                SessionEntry::Header(h.clone()),
                first.clone(),
                abandoned,
                branch,
                other,
                last.clone(),
            ],
        );
        let result = replay_external(&path).unwrap();
        let event = |entry: SessionEntry| match entry {
            SessionEntry::Custom(c) => ExternalAcpPresentation::from_custom(&c).unwrap().unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(result.header, h);
        assert_eq!(result.events, vec![event(first), event(last)]);
        assert!(!result.truncated);
    }

    #[test]
    fn rejects_wrong_backend_and_invalid_headers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session");
        let mut h = header();
        h.backend = None;
        write(&path, &[SessionEntry::Header(h)]);
        assert!(replay_external(&path).is_err());
        let mut value = serde_json::to_value(SessionEntry::Header(header())).unwrap();
        for version in [0, 3, 5] {
            value["version"] = json!(version);
            std::fs::write(&path, format!("{value}\n")).unwrap();
            assert!(replay_external(&path).is_err());
        }
        value["version"] = json!(4);
        value["backend"]["secret"] = json!("no");
        std::fs::write(&path, format!("{value}\n")).unwrap();
        assert!(replay_external(&path).is_err());
    }

    #[test]
    fn rejects_malformed_reserved_records_even_on_abandoned_branches() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session");
        let h = header();
        let first = serde_json::to_string(&SessionEntry::Header(h.clone())).unwrap();
        let bad = json!({"type":"custom", "id":EntryId::new(), "ts":Utc::now(), "kind":"kage:external_acp_presentation", "data":{"event":"prompt", "text":"hi", "secret":"no"}});
        let branch = serde_json::to_string(&SessionEntry::Branch(BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at: h.id,
        }))
        .unwrap();
        std::fs::write(&path, format!("{first}\n{bad}\n{branch}\n")).unwrap();
        assert!(matches!(
            replay_external(&path),
            Err(SessionError::Decode { .. })
        ));
    }
}
