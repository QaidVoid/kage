//! `_kage/themes`: the user themes in the themes folder, with the
//! `[gui]` table a client that draws its own palette reads. The
//! terminal reads the rest of each file itself (see `kage_tui::theme`).

use std::path::Path;

use kage_acp::acp::{ThemesResult, UserTheme};

/// The themes under `dir`, sorted by name. A file that cannot be read
/// or parsed is left out; the terminal reports it when it loads one.
pub(super) fn themes(dir: &Path) -> ThemesResult {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return ThemesResult::default();
    };
    let mut themes: Vec<UserTheme> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "toml" {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_owned();
            let table: toml::Table = std::fs::read_to_string(&path).ok()?.parse().ok()?;
            let base = table
                .get("base")
                .and_then(toml::Value::as_str)
                .map(str::to_owned);
            let gui = table
                .get("gui")
                .and_then(toml::Value::as_table)
                .map(|gui| {
                    gui.iter()
                        .filter_map(|(token, color)| {
                            Some((token.clone(), color.as_str()?.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(UserTheme { name, base, gui })
        })
        .collect();
    themes.sort_by(|a, b| a.name.cmp(&b.name));
    ThemesResult { themes }
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
        let listed = themes(dir.path()).themes;
        let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["ember", "plain"]);
        assert_eq!(listed[0].base.as_deref(), Some("kimi-dark"));
        assert_eq!(listed[0].gui["accent"], "#fe8019");
        assert!(listed[1].gui.is_empty());
    }

    #[test]
    fn a_missing_folder_has_no_themes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(themes(&dir.path().join("none")).themes.is_empty());
    }
}
