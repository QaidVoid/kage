//! Prompt-template loader and renderer.
//!
//! Templates are markdown files with YAML-subset frontmatter living at
//! `~/.config/kage/templates/<name>.md` or `./.kage/templates/<name>.md`.
//! Unlike skills they are NOT injected into the system prompt; they are
//! summoned as `/<name>` and their rendered body
//! becomes the next user message.
//!
//! Frontmatter keys:
//! * `name` (optional) - canonical name, must match the filename stem
//! * `description` (optional) - shown in completion
//! * `argument-hint` (optional) - free text shown next to the name
//!
//! Body placeholders:
//! * `$1`, `$2`, ... positional, 1-indexed
//! * `$@` and `$ARGUMENTS` - all args joined by single space
//! * `${@:N:L}` - bash-style slice: starting at index `N`, up to `L` args
//!
//! Unknown placeholders pass through unchanged so a template can
//! include literal `$VAR` strings; write `$$` for a literal `$`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A loaded prompt template.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    /// Canonical name. Equal to the file stem.
    pub name: String,
    /// Description shown in completion.
    pub description: String,
    /// Free-form hint shown next to the name in completion
    /// (e.g. `"<path>"` or `"<bug-id> [reproducer]"`).
    pub argument_hint: String,
    /// Raw template body with placeholders intact.
    pub body: String,
    /// Path the template was loaded from.
    pub path: PathBuf,
}

/// Failure mode for [`load_template_file`].
#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
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
    /// Schema validation failed.
    #[error("{path:?}: invalid template: {message}")]
    Invalid {
        /// Path that failed.
        path: PathBuf,
        /// Human-readable reason.
        message: String,
    },
}

/// Walk a `templates/` directory and load every markdown file in it.
/// The extension matches case-insensitively, so `Review.MD` loads too.
#[must_use]
pub fn load_templates_dir(dir: &Path) -> Vec<Result<Template, TemplateError>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        out.push(load_template_file(&path));
    }
    out
}

/// Read and validate one `<name>.md` template.
pub fn load_template_file(path: &Path) -> Result<Template, TemplateError> {
    let raw = fs::read_to_string(path).map_err(|source| TemplateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let (front, body) = crate::skills::split_frontmatter_pub(&raw).map_err(|message| {
        TemplateError::Frontmatter {
            path: path.to_path_buf(),
            message,
        }
    })?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| TemplateError::Invalid {
            path: path.to_path_buf(),
            message: "template filename has no stem".to_owned(),
        })?
        .to_owned();
    crate::skills::validate_name_pub(&stem).map_err(|message| TemplateError::Invalid {
        path: path.to_path_buf(),
        message,
    })?;
    if let Some(declared) = front.get("name")
        && declared != &stem
    {
        return Err(TemplateError::Invalid {
            path: path.to_path_buf(),
            message: format!("frontmatter name {declared:?} disagrees with filename stem {stem:?}"),
        });
    }
    let description = front
        .get("description")
        .map_or("", String::as_str)
        .trim()
        .to_owned();
    let argument_hint = front
        .get("argument-hint")
        .map_or("", String::as_str)
        .trim()
        .to_owned();
    Ok(Template {
        name: stem,
        description,
        argument_hint,
        body: body.to_owned(),
        path: path.to_path_buf(),
    })
}

/// Render a template body by substituting positional placeholders with
/// the provided args. Unknown placeholders pass through unchanged and
/// `$$` renders a literal `$`, so `price $$5.00` renders
/// `price $5.00` while a missing `$N` arg renders empty. Characters
/// outside placeholders are copied verbatim, multibyte ones included.
#[must_use]
pub fn render_template(body: &str, args: &[&str]) -> String {
    let joined = args.join(" ");
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        let rest = &body[i..];
        let c = rest.chars().next().expect("i sits on a char boundary");
        if c != '$' {
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        // Try to match a placeholder starting at i.
        if let Some((replacement, consumed)) = try_match_placeholder(rest, args, &joined) {
            out.push_str(&replacement);
            i += consumed;
        } else {
            out.push('$');
            i += 1;
        }
    }
    out
}

/// Look at `s` starting with `$` and try to parse a placeholder. Returns
/// `(replacement, bytes_consumed)` on a match.
fn try_match_placeholder(s: &str, args: &[&str], joined: &str) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    debug_assert_eq!(bytes.first().copied(), Some(b'$'));
    // $$ escapes a literal dollar.
    if bytes.get(1).copied() == Some(b'$') {
        return Some(("$".to_owned(), 2));
    }
    // $@
    if bytes.get(1).copied() == Some(b'@') {
        return Some((joined.to_owned(), 2));
    }
    // $ARGUMENTS
    if s.starts_with("$ARGUMENTS") {
        return Some((joined.to_owned(), "$ARGUMENTS".len()));
    }
    // $N (1-indexed positional)
    if let Some(c) = bytes.get(1).copied()
        && c.is_ascii_digit()
    {
        let mut end = 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let n: usize = s[1..end].parse().ok()?;
        if n == 0 {
            return None;
        }
        let value = args.get(n - 1).copied().unwrap_or("");
        return Some((value.to_owned(), end));
    }
    // ${@:N:L}  bash-style slice
    if s.starts_with("${@:") {
        let close = s.find('}')?;
        let inner = &s["${@:".len()..close];
        let mut parts = inner.split(':');
        let n: usize = parts.next()?.parse().ok()?;
        let l: usize = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        if n == 0 {
            return None;
        }
        let start = n - 1;
        let end = start.saturating_add(l).min(args.len());
        let slice = if start < args.len() {
            args[start..end].join(" ")
        } else {
            String::new()
        };
        return Some((slice, close + 1));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_substitution() {
        assert_eq!(
            render_template("$1 vs $2", &["alpha", "beta"]),
            "alpha vs beta"
        );
        assert_eq!(render_template("$1", &[]), "");
        assert_eq!(render_template("$3", &["a", "b"]), "");
    }

    #[test]
    fn at_and_arguments_join_all() {
        assert_eq!(render_template("$@", &["a", "b c", "d"]), "a b c d");
        assert_eq!(render_template("$ARGUMENTS", &["x", "y"]), "x y");
    }

    #[test]
    fn bash_slice_picks_args() {
        let args = ["a", "b", "c", "d", "e"];
        let args_ref: Vec<&str> = args.to_vec();
        assert_eq!(render_template("${@:2:2}", &args_ref), "b c");
        assert_eq!(render_template("${@:1:1}", &args_ref), "a");
        assert_eq!(render_template("${@:4:99}", &args_ref), "d e");
        assert_eq!(render_template("${@:10:1}", &args_ref), "");
    }

    #[test]
    fn unknown_placeholders_pass_through() {
        assert_eq!(render_template("$FOO bar", &["x"]), "$FOO bar");
        assert_eq!(render_template("price $5.00", &[]), "price .00");
        // ^ $5 is empty because there's no 5th arg; remainder passes through.
    }

    #[test]
    fn a_doubled_dollar_renders_one_literal_dollar() {
        assert_eq!(render_template("price $$5.00", &[]), "price $5.00");
        assert_eq!(render_template("$$1 and $$@", &["a", "b"]), "$1 and $@");
        assert_eq!(render_template("$$$", &[]), "$$");
    }

    #[test]
    fn multibyte_text_around_placeholders_survives() {
        assert_eq!(render_template("caf\u{e9} $1", &["x"]), "caf\u{e9} x");
        assert_eq!(
            render_template("\u{4f60}\u{597d}$1\u{1f600}", &["ok"]),
            "\u{4f60}\u{597d}ok\u{1f600}"
        );
        assert_eq!(render_template("\u{00fc}$3", &["a", "b"]), "\u{00fc}");
    }

    #[test]
    fn loads_a_template_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.md");
        std::fs::write(
            &path,
            "---\ndescription: review code\nargument-hint: <path>\n---\nReview $1 carefully.\n",
        )
        .unwrap();
        let tpl = load_template_file(&path).unwrap();
        assert_eq!(tpl.name, "review");
        assert_eq!(tpl.description, "review code");
        assert_eq!(tpl.argument_hint, "<path>");
        let rendered = render_template(&tpl.body, &["src/lib.rs"]);
        assert!(rendered.contains("Review src/lib.rs carefully."));
    }

    #[test]
    fn rejects_when_frontmatter_name_disagrees_with_stem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.md");
        std::fs::write(&path, "---\nname: typo\n---\n").unwrap();
        let err = load_template_file(&path).unwrap_err();
        assert!(err.to_string().contains("disagrees"));
    }

    #[test]
    fn rejects_invalid_filename_stem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Bad_Name.md");
        std::fs::write(&path, "---\n---\n").unwrap();
        let err = load_template_file(&path).unwrap_err();
        assert!(err.to_string().contains("illegal character"));
    }

    #[test]
    fn loads_templates_dir_ignores_non_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("review.md"), "---\n---\nbody\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        std::fs::write(dir.path().join("review.mdown"), "ignored").unwrap();
        let results = load_templates_dir(dir.path());
        assert_eq!(results.len(), 1);
        assert!(results.into_iter().next().unwrap().is_ok());
    }

    #[test]
    fn loads_templates_dir_matches_the_extension_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("review.MD"),
            "---\ndescription: caps\n---\nbody\n",
        )
        .unwrap();
        let mut results = load_templates_dir(dir.path());
        assert_eq!(results.len(), 1);
        let tpl = results.pop().unwrap().unwrap();
        assert_eq!(tpl.name, "review");
        assert_eq!(tpl.description, "caps");
    }

    #[test]
    fn missing_directory_returns_empty() {
        assert!(load_templates_dir(Path::new("/definitely/not/here")).is_empty());
    }
}
