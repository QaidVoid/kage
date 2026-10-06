//! `_kage/themes`: the user themes in the themes folder, with the
//! `[gui]` table a client that draws its own palette reads. The
//! terminal reads the rest of each file itself (see `kage_tui::theme`).

use std::path::Path;

use kage_acp::acp::{ThemesResult, UserTheme};

/// The themes under `dir`, sorted by name, plus the files left out as
/// `"<file name>: <reason>"`. A file whose stem is not UTF-8, that
/// cannot be read, or that does not parse is skipped; the terminal
/// reports it when it loads one.
pub(super) fn themes(dir: &Path) -> ThemesResult {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return ThemesResult::default();
    };
    let mut themes: Vec<UserTheme> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some("toml".as_ref()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.file_stem().and_then(|stem| stem.to_str()).is_none() {
            skipped.push(format!("{name}: non-utf8"));
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            skipped.push(format!("{name}: unreadable"));
            continue;
        };
        let Ok(table) = text.parse::<toml::Table>() else {
            skipped.push(format!("{name}: invalid"));
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_owned();
        let base = table
            .get("base")
            .and_then(toml::Value::as_str)
            .map(str::to_owned);
        let gui = table
            .get("gui")
            .and_then(toml::Value::as_table)
            .map(|gui| {
                gui.iter()
                    .filter_map(|(token, color)| Some((token.clone(), color.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        themes.push(UserTheme {
            name: stem,
            base,
            gui,
        });
    }
    themes.sort_by(|a, b| a.name.cmp(&b.name));
    ThemesResult { themes, skipped }
}

#[cfg(test)]
mod tests {
    use super::themes;

    #[test]
    fn lists_each_theme_with_its_gui_table() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ember.toml"),
            "base = \"kimi-dark\"\n[colors]\nbg = \"#000000\"\n[gui]\naccent = \"#fe8019\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("plain.toml"),
            "[colors]\nbg = \"#111111\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("broken.toml"), "[gui\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a theme").unwrap();
        let result = themes(dir.path());
        let listed = result.themes;
        let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["ember", "plain"]);
        assert_eq!(listed[0].base.as_deref(), Some("kimi-dark"));
        assert_eq!(listed[0].gui["accent"], "#fe8019");
        assert!(listed[1].gui.is_empty());
        assert_eq!(result.skipped, vec!["broken.toml: invalid".to_owned()]);
    }

    #[test]
    fn a_missing_folder_has_no_themes() {
        let dir = tempfile::tempdir().unwrap();
        let result = themes(&dir.path().join("none"));
        assert!(result.themes.is_empty());
        assert!(result.skipped.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_and_non_utf8_files_are_skipped_with_a_reason() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked.toml");
        std::fs::write(&locked, "base = \"x\"\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let foreign = dir
            .path()
            .join(std::ffi::OsStr::from_bytes(b"caf\xe9.toml"));
        std::fs::write(&foreign, "base = \"x\"\n").unwrap();

        let result = themes(dir.path());
        assert!(result.themes.is_empty(), "{:?}", result.themes);
        if std::fs::read_to_string(&locked).is_err() {
            assert!(
                result
                    .skipped
                    .iter()
                    .any(|s| s == "locked.toml: unreadable"),
                "{:?}",
                result.skipped
            );
        }
        assert!(
            result
                .skipped
                .iter()
                .any(|s| s.ends_with(".toml: non-utf8") && s.starts_with("caf")),
            "{:?}",
            result.skipped
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
