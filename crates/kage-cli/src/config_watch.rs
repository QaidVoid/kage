//! Notices edits to configuration files by polling their metadata.
//!
//! Polling a handful of files once a second costs a few `stat` calls
//! and sees every way an editor writes a file, a rename over it
//! included, without a watch per directory. The set of files can change
//! between polls, as sessions open in other projects.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How often a watcher looks at its files.
pub(crate) const POLL: Duration = Duration::from_secs(1);

/// What a file looked like at the last poll: its modification time and
/// length, or `None` when it did not exist.
type Stamp = Option<(SystemTime, u64)>;

/// The last seen stamp of each watched file.
#[derive(Default)]
pub(crate) struct Stamps(HashMap<PathBuf, Stamp>);

impl Stamps {
    /// The files of `paths` that changed since the last poll: written,
    /// created or removed. A file seen for the first time counts as
    /// unchanged, and a file no longer in `paths` is forgotten.
    pub(crate) fn changed(&mut self, paths: &[PathBuf]) -> Vec<PathBuf> {
        self.0.retain(|path, _| paths.contains(path));
        paths
            .iter()
            .filter(|path| {
                let now = stamp(path);
                self.0
                    .insert((*path).clone(), now)
                    .is_some_and(|before| before != now)
            })
            .cloned()
            .collect()
    }
}

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write, a removal and a creation each count once; a file first
    /// seen, or left alone, does not.
    #[test]
    fn stamps_report_each_change_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        let paths = [file.clone()];
        let mut stamps = Stamps::default();
        assert!(stamps.changed(&paths).is_empty(), "first sighting");
        std::fs::write(&file, "a = 1\n").unwrap();
        assert_eq!(stamps.changed(&paths), paths, "created");
        assert!(stamps.changed(&paths).is_empty(), "unchanged");
        std::fs::write(&file, "a = 12\n").unwrap();
        assert_eq!(stamps.changed(&paths), paths, "written");
        std::fs::remove_file(&file).unwrap();
        assert_eq!(stamps.changed(&paths), paths, "removed");
        assert!(stamps.changed(&[]).is_empty());
    }
}
