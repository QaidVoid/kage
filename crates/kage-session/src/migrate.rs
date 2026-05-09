//! Forward-only migration of session entries between format versions.
//!
//! Sessions carry a [`Header::version`](crate::Header::version) declaring
//! the format their entries were written in. When a reader is on a newer
//! kage build, it asks [`ensure_version`] to walk each loaded entry from
//! its file's recorded version up to the current [`FORMAT_VERSION`].
//!
//! v0.1 ships with `FORMAT_VERSION = 1`, so the function is a no-op for the
//! only supported case (`from == target == 1`). The scaffold exists so
//! future format bumps slot in by adding one new migration step at a time
//! without changing the call sites.

use crate::entry::{FORMAT_VERSION, SessionEntry};

/// Anything that can go wrong migrating an entry.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// No migration path exists for the requested version pair. Either the
    /// file pre-dates the first released format, post-dates the current
    /// build, or no migration step has been written for the gap yet.
    #[error("no migration path from v{from} to v{target}")]
    Unsupported {
        /// Version recorded in the source file.
        from: u32,
        /// Version the caller is migrating toward.
        target: u32,
    },
}

/// Migrate `entry` from format version `from` up to `target`.
///
/// Returns the migrated entry. For v0.1 the only supported case is
/// `from == target == FORMAT_VERSION`, in which case the entry is returned
/// unchanged.
pub fn ensure_version(
    entry: SessionEntry,
    from: u32,
    target: u32,
) -> Result<SessionEntry, MigrationError> {
    if from == target {
        return Ok(entry);
    }
    if from == FORMAT_VERSION && target == FORMAT_VERSION {
        return Ok(entry);
    }
    Err(MigrationError::Unsupported { from, target })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use kage_core::{Content, Message, Role};

    use super::*;
    use crate::entry::{EntryId, MessageEntry};

    fn sample_entry() -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Message::new(Role::User, vec![Content::Text { text: "hi".into() }], None),
        })
    }

    #[test]
    fn no_op_when_versions_match_current() {
        let entry = sample_entry();
        let cloned = entry.clone();
        let out = ensure_version(entry, FORMAT_VERSION, FORMAT_VERSION).unwrap();
        assert_eq!(out, cloned);
    }

    #[test]
    fn no_op_when_from_equals_target() {
        let entry = sample_entry();
        let cloned = entry.clone();
        let out = ensure_version(entry, 99, 99).unwrap();
        assert_eq!(out, cloned);
    }

    #[test]
    fn upgrade_to_unknown_target_errors() {
        let err = ensure_version(sample_entry(), FORMAT_VERSION, FORMAT_VERSION + 1).unwrap_err();
        assert!(
            matches!(err, MigrationError::Unsupported { from, target } if from == FORMAT_VERSION && target == FORMAT_VERSION + 1)
        );
    }

    #[test]
    fn downgrade_from_unknown_source_errors() {
        let err = ensure_version(sample_entry(), 0, FORMAT_VERSION).unwrap_err();
        assert!(matches!(err, MigrationError::Unsupported { .. }));
    }
}
