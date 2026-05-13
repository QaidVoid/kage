//! Declarative skill loader for `SKILL.md` files.
//!
//! Skills are markdown files with a YAML-subset frontmatter that get
//! injected into the system prompt by default and can be invoked from the
//! slash palette as `/skill:<name>`. They give non-Lua users a way to
//! extend kage with reusable instructions.
//!
//! Directory layout (per Pi convention):
//!
//! ```text
//! ~/.config/kage/skills/<name>/SKILL.md
//! ./.kage/skills/<name>/SKILL.md
//! ```
//!
//! The parent directory name is the canonical skill name; the
//! frontmatter's `name` field must match it (or be omitted, in which case
//! the parent directory name is used). This catches typos between the
//! filename and the frontmatter.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A loaded SKILL.md file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    /// Canonical name. Equal to the parent directory name.
    pub name: String,
    /// Human-readable description shown in the slash palette and in the
    /// system prompt header.
    pub description: String,
    /// Markdown body (everything after the frontmatter).
    pub body: String,
    /// Whether to hide this skill from the slash palette. The body still
    /// gets injected into the system prompt context.
    pub disable_model_invocation: bool,
    /// The absolute path the skill was loaded from. Used by the host to
    /// resolve body-relative file references.
    pub path: PathBuf,
}

/// Failure mode for [`load_skill_file`].
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// I/O failure while reading the file.
    #[error("read {path:?}: {source}")]
    Io {
        /// Path that failed.
        path: PathBuf,
        /// Underlying io error.
        #[source]
        source: std::io::Error,
    },
    /// Frontmatter was missing or malformed.
    #[error("{path:?}: invalid frontmatter: {message}")]
    Frontmatter {
        /// Path that failed.
        path: PathBuf,
        /// Human-readable reason.
        message: String,
    },
    /// A schema validation rule failed.
    #[error("{path:?}: invalid skill: {message}")]
    Invalid {
        /// Path that failed.
        path: PathBuf,
        /// Human-readable reason.
        message: String,
    },
}

/// Walk a `skills/` directory and load every `<name>/SKILL.md` underneath
/// it. Returns a list of per-skill results so callers can surface
/// individual failures without aborting the whole load.
///
/// Missing or non-directory inputs return an empty Vec without error;
/// this lets callers blindly try both user and project paths.
#[must_use]
pub fn load_skills_dir(dir: &Path) -> Vec<Result<Skill, SkillError>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        out.push(load_skill_file(&skill_md));
    }
    out
}

/// Read and validate one `SKILL.md` file. The skill's canonical name is
/// taken from the parent directory; the frontmatter `name` field, if
/// present, must match.
pub fn load_skill_file(path: &Path) -> Result<Skill, SkillError> {
    let raw = fs::read_to_string(path).map_err(|source| SkillError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let (front, body) = split_frontmatter(&raw).map_err(|message| SkillError::Frontmatter {
        path: path.to_path_buf(),
        message,
    })?;
    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .ok_or_else(|| SkillError::Invalid {
            path: path.to_path_buf(),
            message: "skill must live under a named directory".to_owned(),
        })?
        .to_owned();
    validate_name(&dir_name).map_err(|message| SkillError::Invalid {
        path: path.to_path_buf(),
        message,
    })?;
    if let Some(declared) = front.get("name")
        && declared != &dir_name
    {
        return Err(SkillError::Invalid {
            path: path.to_path_buf(),
            message: format!(
                "frontmatter name {declared:?} disagrees with directory name {dir_name:?}",
            ),
        });
    }
    let description = front
        .get("description")
        .map_or("", String::as_str)
        .trim()
        .to_owned();
    if description.len() > MAX_DESCRIPTION_LEN {
        return Err(SkillError::Invalid {
            path: path.to_path_buf(),
            message: format!(
                "description is {}/{MAX_DESCRIPTION_LEN} chars",
                description.len(),
            ),
        });
    }
    let disable_model_invocation = front
        .get("disable_model_invocation")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    Ok(Skill {
        name: dir_name,
        description,
        body: body.trim_end().to_owned(),
        disable_model_invocation,
        path: path.to_path_buf(),
    })
}

const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 1024;

/// Validate the directory name against the `^[a-z0-9-]+$` regex with the
/// added constraint that consecutive hyphens are not allowed.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name is empty".to_owned());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!("name is {}/{MAX_NAME_LEN} chars", name.len()));
    }
    let mut last_was_hyphen = false;
    for ch in name.chars() {
        let ok = ch.is_ascii_digit() || ch.is_ascii_lowercase() || ch == '-';
        if !ok {
            return Err(format!(
                "name {name:?} contains illegal character {ch:?}; only [a-z0-9-] allowed",
            ));
        }
        if ch == '-' && last_was_hyphen {
            return Err(format!("name {name:?} contains consecutive hyphens"));
        }
        last_was_hyphen = ch == '-';
    }
    Ok(())
}

/// Split YAML-subset frontmatter from the body. Returns
/// `(key_value_pairs, body)` where `body` is everything after the closing
/// `---` delimiter (or the entire file if no frontmatter exists).
///
/// Supported frontmatter:
/// * `key: value` (single-line)
/// * `key: "quoted value"` (double-quoted, no escapes besides standard)
/// * Lines starting with `#` are comments and are ignored.
///
/// Multi-line scalars (`|`, `>`) are not supported in v0.1; skills that
/// need long descriptions can put detail in the body.
fn split_frontmatter(
    input: &str,
) -> Result<(std::collections::HashMap<String, String>, &str), String> {
    let body_only = || (std::collections::HashMap::new(), input);
    let mut rest = input
        .strip_prefix("---\n")
        .or_else(|| input.strip_prefix("---\r\n"));
    if rest.is_none() {
        return Ok(body_only());
    }
    let remaining = rest.take().unwrap_or("");
    // Locate the closing delimiter on its own line.
    let mut end_byte: Option<usize> = None;
    let mut after_end: Option<&str> = None;
    let mut cursor: &str = remaining;
    let mut absolute_offset = 0usize;
    while !cursor.is_empty() {
        let line_end = cursor.find('\n').map_or(cursor.len(), |i| i + 1);
        let line = &cursor[..line_end];
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            end_byte = Some(absolute_offset);
            after_end = Some(&cursor[line_end..]);
            break;
        }
        absolute_offset += line.len();
        cursor = &cursor[line_end..];
    }
    let Some(end) = end_byte else {
        return Err("frontmatter has no closing `---` delimiter".to_owned());
    };
    let header = &remaining[..end];
    let body = after_end.unwrap_or("");

    let mut map = std::collections::HashMap::new();
    for (lineno, raw_line) in header.lines().enumerate() {
        let line = raw_line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("line {} is not `key: value`", lineno + 1));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("line {} has empty key", lineno + 1));
        }
        let value = unquote(value.trim());
        map.insert(key.to_owned(), value);
    }
    Ok((map, body))
}

fn unquote(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_owned();
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_name_regex_and_length() {
        assert!(validate_name("foo").is_ok());
        assert!(validate_name("foo-bar").is_ok());
        assert!(validate_name("123").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("Foo").is_err()); // uppercase
        assert!(validate_name("foo bar").is_err());
        assert!(validate_name("foo--bar").is_err()); // consecutive hyphens
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn parses_simple_frontmatter() {
        let input = "---\nname: foo\ndescription: bar\n---\nbody text\n";
        let (map, body) = split_frontmatter(input).unwrap();
        assert_eq!(map.get("name").map(String::as_str), Some("foo"));
        assert_eq!(map.get("description").map(String::as_str), Some("bar"));
        assert_eq!(body, "body text\n");
    }

    #[test]
    fn no_frontmatter_returns_whole_input_as_body() {
        let input = "no frontmatter here\n";
        let (map, body) = split_frontmatter(input).unwrap();
        assert!(map.is_empty());
        assert_eq!(body, input);
    }

    #[test]
    fn unterminated_frontmatter_errors() {
        let input = "---\nname: foo\n";
        let err = split_frontmatter(input).unwrap_err();
        assert!(err.contains("no closing"));
    }

    #[test]
    fn quoted_values_are_unquoted() {
        let input = "---\nname: \"foo bar\"\n---\n";
        let (map, _) = split_frontmatter(input).unwrap();
        assert_eq!(map.get("name").map(String::as_str), Some("foo bar"));
    }

    #[test]
    fn loads_a_skill_directory() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("code-review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: review code\n---\n# Body\n",
        )
        .unwrap();
        let results = load_skills_dir(dir.path());
        assert_eq!(results.len(), 1);
        let skill = results.into_iter().next().unwrap().unwrap();
        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.description, "review code");
        assert_eq!(skill.body.trim(), "# Body");
        assert!(!skill.disable_model_invocation);
    }

    #[test]
    fn skill_name_must_match_directory() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("code-review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "---\nname: typo-name\n---\n").unwrap();
        let results = load_skills_dir(dir.path());
        let err = results.into_iter().next().unwrap().unwrap_err();
        let s = err.to_string();
        assert!(s.contains("disagrees with directory name"), "got {s}");
    }

    #[test]
    fn invalid_directory_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("Bad_Name");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "---\n---\n").unwrap();
        let results = load_skills_dir(dir.path());
        let err = results.into_iter().next().unwrap().unwrap_err();
        assert!(err.to_string().contains("illegal character"));
    }

    #[test]
    fn disable_model_invocation_parses() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("hidden");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndisable_model_invocation: true\n---\n",
        )
        .unwrap();
        let skill = load_skills_dir(dir.path())
            .into_iter()
            .next()
            .unwrap()
            .unwrap();
        assert!(skill.disable_model_invocation);
    }

    #[test]
    fn empty_directory_returns_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_skills_dir(dir.path()).is_empty());
    }

    #[test]
    fn missing_directory_returns_empty_list() {
        assert!(load_skills_dir(Path::new("/definitely/not/here")).is_empty());
    }

    #[test]
    fn description_over_max_length_errors() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("verbose");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let long = "a".repeat(MAX_DESCRIPTION_LEN + 1);
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\ndescription: {long}\n---\n"),
        )
        .unwrap();
        let err = load_skills_dir(dir.path())
            .into_iter()
            .next()
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("description is"));
    }
}
