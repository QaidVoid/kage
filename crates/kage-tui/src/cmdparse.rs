//! Tokenizer, parser, and completion engine for slash and colon
//! command lines.
//!
//! The tokenizer is shell-lexer style: whitespace separates tokens,
//! `"double"` and `'single'` quoted strings preserve internal
//! whitespace, and inside double-quoted strings `\"` and `\\` escape
//! the quote and backslash respectively. Single-quoted strings have
//! no escape handling. Unterminated quotes return [`ParseError::UnterminatedQuote`].
//!
//! The parser walks the [`CommandSpec::args`] list, consuming one
//! token per non-`Rest` arg and validating membership for [`ArgSpec::Choice`]
//! and bool parsing for [`ArgSpec::Flag`]. A trailing [`ArgSpec::Rest`]
//! arg consumes the verbatim raw substring from its position to the
//! end of input (with leading whitespace skipped and trailing
//! whitespace trimmed) so callers can preserve user formatting.
//!
//! [`complete`] determines what the cursor is editing (the head or
//! arg N) by mapping the cursor byte offset onto token spans, then
//! returns candidate completions filtered by the prefix being typed.
//! Dynamic value sources (models, themes, sessions, plugin commands,
//! file paths) are resolved through a caller-provided [`Resolver`] so
//! this module stays free of TUI-runtime dependencies.

use std::ops::Range;

use crate::command::{ArgSource, ArgSpec, ArgValue, CommandSpec, ParsedArgs};

/// Reasons a command line can fail to parse against its [`CommandSpec`].
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    /// A required argument was not supplied.
    #[error("missing required argument `{0}`")]
    MissingArg(&'static str),
    /// A [`ArgSpec::Choice`] argument received a value not in its
    /// allowed set.
    #[error("argument `{name}` must be one of {expected:?}, got `{got}`")]
    InvalidChoice {
        /// Argument name from the spec.
        name: &'static str,
        /// Value the user provided.
        got: String,
        /// Allowed values from the spec.
        expected: Vec<&'static str>,
    },
    /// A [`ArgSpec::Flag`] argument received a value that did not
    /// parse as a boolean.
    #[error("argument `{name}` expected a boolean, got `{got}`")]
    InvalidFlag {
        /// Argument name from the spec.
        name: &'static str,
        /// Value the user provided.
        got: String,
    },
    /// More tokens were supplied than the spec accepts.
    #[error("unexpected extra argument `{0}`")]
    UnexpectedTokens(String),
    /// Tokenizer hit end-of-input while inside a quoted string.
    #[error("unterminated quoted string")]
    UnterminatedQuote,
}

/// One token emitted by the tokenizer.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Token {
    /// Decoded value with surrounding quotes removed and escapes
    /// applied.
    value: String,
    /// Byte range in the original input string that produced this
    /// token, including any surrounding quotes and escape sequences.
    span: Range<usize>,
    /// `true` if any portion of the token was wrapped in quotes.
    quoted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuoteState {
    None,
    Double,
    Single,
}

/// Split `raw` into shell-style tokens. Whitespace separates tokens
/// outside quotes; double and single quotes preserve internal
/// whitespace. Returns [`ParseError::UnterminatedQuote`] if a quoted
/// region is not closed before end-of-input.
fn tokenize(raw: &str) -> Result<Vec<Token>, ParseError> {
    tokenize_inner(raw, false)
}

/// Lenient variant for the completion engine: an unclosed quote at
/// end-of-input emits whatever was inside as a final token rather
/// than erroring, so the user can autocomplete inside an open quote.
fn tokenize_lenient(raw: &str) -> Vec<Token> {
    tokenize_inner(raw, true).unwrap_or_default()
}

fn tokenize_inner(raw: &str, lenient: bool) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut idx = 0;
    while idx < raw.len() {
        let c = raw[idx..].chars().next().expect("idx within bounds");
        if c.is_whitespace() {
            idx += c.len_utf8();
            continue;
        }
        let start = idx;
        let mut value = String::new();
        let mut quoted = false;
        let mut state = QuoteState::None;
        while idx < raw.len() {
            let c = raw[idx..].chars().next().expect("idx within bounds");
            let clen = c.len_utf8();
            match (state, c) {
                (QuoteState::None, ch) if ch.is_whitespace() => break,
                (QuoteState::None, '"') => {
                    quoted = true;
                    state = QuoteState::Double;
                    idx += 1;
                }
                (QuoteState::None, '\'') => {
                    quoted = true;
                    state = QuoteState::Single;
                    idx += 1;
                }
                (QuoteState::Double, '"') | (QuoteState::Single, '\'') => {
                    state = QuoteState::None;
                    idx += 1;
                }
                (QuoteState::Double, '\\') => {
                    idx += 1;
                    let Some(esc) = raw[idx..].chars().next() else {
                        if lenient {
                            value.push('\\');
                            break;
                        }
                        return Err(ParseError::UnterminatedQuote);
                    };
                    if esc == '"' || esc == '\\' {
                        value.push(esc);
                    } else {
                        value.push('\\');
                        value.push(esc);
                    }
                    idx += esc.len_utf8();
                }
                _ => {
                    value.push(c);
                    idx += clen;
                }
            }
        }
        if state != QuoteState::None && !lenient {
            return Err(ParseError::UnterminatedQuote);
        }
        tokens.push(Token {
            value,
            span: start..idx,
            quoted,
        });
    }
    Ok(tokens)
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Parse `args` against `spec`, producing a typed [`ParsedArgs`] map
/// keyed by argument name. `args` is the substring after the command
/// head (e.g. for `:model anthropic:claude` pass `"anthropic:claude"`).
///
/// On success, every required arg in `spec.args` is present and every
/// [`ArgSpec::Choice`] value has been validated. Optional args are
/// omitted from the map when the user did not supply them.
///
/// # Errors
///
/// - [`ParseError::MissingArg`] when a required arg is absent.
/// - [`ParseError::InvalidChoice`] when a [`ArgSpec::Choice`] value
///   is not in the allowed set.
/// - [`ParseError::InvalidFlag`] when a [`ArgSpec::Flag`] value does
///   not parse as a boolean.
/// - [`ParseError::UnexpectedTokens`] when extra tokens follow the
///   last spec arg.
/// - [`ParseError::UnterminatedQuote`] when the tokenizer hits EOF
///   inside a quoted region.
pub fn parse_input(spec: &CommandSpec, args: &str) -> Result<ParsedArgs, ParseError> {
    let tokens = tokenize(args)?;
    let mut parsed = ParsedArgs::new();
    let mut cursor = 0usize;

    for arg in spec.args {
        match arg {
            ArgSpec::Rest { name, optional, .. } => {
                if cursor < tokens.len() {
                    let rest_start = tokens[cursor].span.start;
                    let rest_text = args[rest_start..].trim_end().to_owned();
                    if !rest_text.is_empty() {
                        parsed.insert(name, ArgValue::Text(rest_text));
                    } else if !optional {
                        return Err(ParseError::MissingArg(name));
                    }
                } else if !optional {
                    return Err(ParseError::MissingArg(name));
                }
                cursor = tokens.len();
                break;
            }
            ArgSpec::Choice {
                name,
                values,
                optional,
            } => {
                let Some(tok) = tokens.get(cursor) else {
                    if !optional {
                        return Err(ParseError::MissingArg(name));
                    }
                    break;
                };
                cursor += 1;
                if values.contains(&tok.value.as_str()) {
                    parsed.insert(name, ArgValue::Choice(tok.value.clone()));
                } else {
                    return Err(ParseError::InvalidChoice {
                        name,
                        got: tok.value.clone(),
                        expected: values.to_vec(),
                    });
                }
            }
            ArgSpec::DynamicChoice { name, optional, .. } => {
                let Some(tok) = tokens.get(cursor) else {
                    if !optional {
                        return Err(ParseError::MissingArg(name));
                    }
                    break;
                };
                cursor += 1;
                parsed.insert(name, ArgValue::Text(tok.value.clone()));
            }
            ArgSpec::Path { name, optional } => {
                let Some(tok) = tokens.get(cursor) else {
                    if !optional {
                        return Err(ParseError::MissingArg(name));
                    }
                    break;
                };
                cursor += 1;
                parsed.insert(name, ArgValue::Path(tok.value.clone()));
            }
            ArgSpec::SessionId { name, optional } => {
                let Some(tok) = tokens.get(cursor) else {
                    if !optional {
                        return Err(ParseError::MissingArg(name));
                    }
                    break;
                };
                cursor += 1;
                parsed.insert(name, ArgValue::SessionId(tok.value.clone()));
            }
            ArgSpec::Flag { name } => {
                if let Some(tok) = tokens.get(cursor) {
                    let b = parse_bool(&tok.value).ok_or_else(|| ParseError::InvalidFlag {
                        name,
                        got: tok.value.clone(),
                    })?;
                    cursor += 1;
                    parsed.insert(name, ArgValue::Bool(b));
                }
            }
        }
    }

    if let Some(extra) = tokens.get(cursor) {
        return Err(ParseError::UnexpectedTokens(extra.value.clone()));
    }

    Ok(parsed)
}

/// One completion candidate. Callers replace `replace_range` in the
/// raw input with `value` (and may want to re-quote `value` if it
/// contains whitespace).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    /// Replacement text inserted when this completion is accepted.
    pub value: String,
    /// Optional one-line description shown beside the value (used for
    /// command-name completions to display the command description).
    pub description: Option<String>,
    /// Byte range in the raw input that should be overwritten by
    /// `value`.
    pub replace_range: Range<usize>,
}

/// Result of [`complete`]. Empty `items` means "nothing to suggest";
/// callers should still respect `anchor` when inserting a literal
/// completion later.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Completions {
    /// Candidate completions, in registry order. The renderer
    /// typically displays them top-to-bottom and selects the first
    /// item by default.
    pub items: Vec<Completion>,
    /// Byte offset in the raw input at which a literal completion
    /// would begin. For all items this equals `replace_range.start`.
    pub anchor: usize,
}

impl Completions {
    /// Construct an empty completion result with `anchor` set to 0.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Caller-supplied resolver for completion sources whose values are
/// only known at runtime (model registry, theme list, file system,
/// session lister). Default implementations return empty so callers
/// can implement only the sources they care about.
pub trait Resolver {
    /// Values for an [`ArgSource`] referenced by [`ArgSpec::DynamicChoice`].
    fn dynamic_choice(&self, source: &ArgSource) -> Vec<String> {
        let _ = source;
        Vec::new()
    }
    /// File-path completions filtered by `prefix`, used for
    /// [`ArgSpec::Path`].
    fn paths(&self, prefix: &str) -> Vec<String> {
        let _ = prefix;
        Vec::new()
    }
    /// Session identifier completions, used for [`ArgSpec::SessionId`].
    fn sessions(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A [`Resolver`] that returns empty for every source. Useful in
/// tests and when wiring the engine before the runtime sources are
/// available.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyResolver;

impl Resolver for EmptyResolver {}

/// Compute completion candidates for `raw` at byte offset `cursor`.
///
/// Determines whether the cursor is editing the command head or
/// argument N, then filters the appropriate candidate set by the
/// prefix being typed. The candidate set is:
///
/// - command head: every name and alias in `registry` that starts
///   with the prefix.
/// - `Choice`: the spec's allowed values.
/// - `DynamicChoice`: [`Resolver::dynamic_choice`].
/// - `Path`: [`Resolver::paths`] (prefix passed through).
/// - `SessionId`: [`Resolver::sessions`].
/// - `Flag`: the literal strings `true|false|yes|no|on|off`.
/// - `Rest`: empty (free text has nothing to suggest).
///
/// On a tokenizer error (such as an unterminated quote earlier in
/// the line) returns [`Completions::empty`]: completion is best-effort
/// and we never return partial / misleading suggestions for
/// malformed input.
#[allow(clippy::too_many_lines)]
pub fn complete(
    registry: &[&CommandSpec],
    raw: &str,
    cursor: usize,
    resolver: &dyn Resolver,
) -> Completions {
    let cursor = cursor.min(raw.len());
    let tokens = tokenize_lenient(raw);

    let mut active: Option<usize> = None;
    let mut completed = 0usize;
    for (i, tok) in tokens.iter().enumerate() {
        if cursor >= tok.span.start && cursor <= tok.span.end {
            active = Some(i);
            completed = i;
            break;
        }
        if cursor > tok.span.end {
            completed = i + 1;
        }
    }

    let (anchor, prefix) = if let Some(i) = active {
        let tok = &tokens[i];
        let prefix_end = cursor.min(tok.span.end);
        let raw_prefix = &raw[tok.span.start..prefix_end];
        let stripped = raw_prefix.trim_start_matches(['"', '\'']);
        (tok.span.start, stripped.to_owned())
    } else {
        (cursor, String::new())
    };

    let arg_position = completed;
    let replace_range = anchor..cursor;

    if arg_position == 0 {
        let mut items = Vec::new();
        for spec in registry {
            for name in spec.names() {
                if name.starts_with(&prefix) {
                    let hints = crate::command::arg_hints_text(spec.args);
                    let description = if hints.is_empty() {
                        spec.description.to_owned()
                    } else {
                        format!("{}  {hints}", spec.description)
                    };
                    items.push(Completion {
                        value: name.to_owned(),
                        description: Some(description),
                        replace_range: replace_range.clone(),
                    });
                }
            }
        }
        return Completions { items, anchor };
    }

    let Some(head_tok) = tokens.first() else {
        return Completions::empty();
    };
    let Some(head_spec) = registry
        .iter()
        .find(|s| s.names().any(|n| n == head_tok.value))
    else {
        return Completions::empty();
    };

    // Walk down the subcommand tree as far as the typed tokens match.
    // `spec_arg_start` is the token index where `spec`'s own args begin;
    // tokens before it are the chain of command + subcommand names that
    // brought us here.
    let mut spec: &CommandSpec = head_spec;
    let mut spec_arg_start = 1usize;
    while spec_arg_start < arg_position {
        let Some(tok) = tokens.get(spec_arg_start) else {
            break;
        };
        let Some(sub) = spec.subcommand(tok.value.as_str()) else {
            break;
        };
        spec = sub;
        spec_arg_start += 1;
    }
    let local_arg_idx = arg_position - spec_arg_start;

    let mut items: Vec<Completion> = Vec::new();

    // At the first position after the spec's name, suggest its
    // subcommands. They mix with the parent's first arg (if any) so a
    // command can offer both a default arg and named subcommands.
    if local_arg_idx == 0 {
        for sub in spec.subcommands {
            for name in sub.names() {
                if !name.starts_with(&prefix) {
                    continue;
                }
                let hints = crate::command::arg_hints_text(sub.args);
                let description = if hints.is_empty() {
                    sub.description.to_owned()
                } else {
                    format!("{}  {hints}", sub.description)
                };
                items.push(Completion {
                    value: name.to_owned(),
                    description: Some(description),
                    replace_range: replace_range.clone(),
                });
            }
        }
    }

    if let Some(arg_spec) = spec.args.get(local_arg_idx) {
        let candidates: Vec<String> = match arg_spec {
            ArgSpec::Choice { values, .. } => values.iter().map(|v| (*v).to_owned()).collect(),
            ArgSpec::DynamicChoice { source, .. } => resolver.dynamic_choice(source),
            ArgSpec::Path { .. } => resolver.paths(&prefix),
            ArgSpec::SessionId { .. } => resolver.sessions(),
            ArgSpec::Flag { .. } => ["true", "false", "yes", "no", "on", "off"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            ArgSpec::Rest { .. } => Vec::new(),
        };
        for c in candidates {
            if !c.starts_with(&prefix) {
                continue;
            }
            items.push(Completion {
                value: c,
                description: None,
                replace_range: replace_range.clone(),
            });
        }
    }

    Completions { items, anchor }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CommandCategory;

    const NO_ARGS: CommandSpec = CommandSpec {
        name: "quit",
        aliases: &["q"],
        description: "leave",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    };

    const ONE_CHOICE: CommandSpec = CommandSpec {
        name: "mouse",
        aliases: &[],
        description: "toggle mouse",
        category: CommandCategory::Both,
        args: &[ArgSpec::Choice {
            name: "state",
            values: &["on", "off", "toggle"],
            optional: false,
        }],
        subcommands: &[],
    };

    const ONE_CHOICE_OPTIONAL: CommandSpec = CommandSpec {
        name: "fold",
        aliases: &[],
        description: "fold",
        category: CommandCategory::Both,
        args: &[ArgSpec::Choice {
            name: "scope",
            values: &["all"],
            optional: true,
        }],
        subcommands: &[],
    };

    const ONE_REST: CommandSpec = CommandSpec {
        name: "compact",
        aliases: &[],
        description: "compact with prompt",
        category: CommandCategory::Both,
        args: &[ArgSpec::Rest {
            name: "prompt",
            optional: true,
            hint: "free text",
        }],
        subcommands: &[],
    };

    const ONE_REST_REQUIRED: CommandSpec = CommandSpec {
        name: "name",
        aliases: &[],
        description: "rename session",
        category: CommandCategory::Both,
        args: &[ArgSpec::Rest {
            name: "title",
            optional: false,
            hint: "session title",
        }],
        subcommands: &[],
    };

    const ONE_DYN: CommandSpec = CommandSpec {
        name: "model",
        aliases: &[],
        description: "switch model",
        category: CommandCategory::Both,
        args: &[ArgSpec::DynamicChoice {
            name: "id",
            source: ArgSource::Models,
            optional: false,
        }],
        subcommands: &[],
    };

    const ONE_PATH: CommandSpec = CommandSpec {
        name: "open",
        aliases: &[],
        description: "open file",
        category: CommandCategory::Both,
        args: &[ArgSpec::Path {
            name: "path",
            optional: false,
        }],
        subcommands: &[],
    };

    const ONE_FLAG: CommandSpec = CommandSpec {
        name: "verbose",
        aliases: &[],
        description: "toggle verbose",
        category: CommandCategory::Both,
        args: &[ArgSpec::Flag { name: "on" }],
        subcommands: &[],
    };

    const CHOICE_THEN_REST: CommandSpec = CommandSpec {
        name: "say",
        aliases: &[],
        description: "speak",
        category: CommandCategory::Both,
        args: &[
            ArgSpec::Choice {
                name: "voice",
                values: &["loud", "soft"],
                optional: false,
            },
            ArgSpec::Rest {
                name: "message",
                optional: false,
                hint: "what to say",
            },
        ],
        subcommands: &[],
    };

    fn registry() -> Vec<&'static CommandSpec> {
        vec![
            &NO_ARGS,
            &ONE_CHOICE,
            &ONE_CHOICE_OPTIONAL,
            &ONE_REST,
            &ONE_DYN,
            &ONE_PATH,
            &ONE_FLAG,
            &CHOICE_THEN_REST,
        ]
    }

    struct TestResolver;
    impl Resolver for TestResolver {
        fn dynamic_choice(&self, source: &ArgSource) -> Vec<String> {
            match source {
                ArgSource::Models => vec![
                    "anthropic:claude-sonnet-4-6".into(),
                    "anthropic:claude-opus-4-7".into(),
                    "openai:gpt-4o".into(),
                ],
                ArgSource::Themes => vec!["dark".into(), "dawn".into(), "moonlight".into()],
                _ => Vec::new(),
            }
        }
        fn paths(&self, prefix: &str) -> Vec<String> {
            let all = ["src/lib.rs", "src/main.rs", "tests/it.rs"];
            all.iter()
                .filter(|p| p.starts_with(prefix))
                .map(|p| (*p).to_owned())
                .collect()
        }
        fn sessions(&self) -> Vec<String> {
            vec!["sess-1".into(), "sess-2".into()]
        }
    }

    #[test]
    fn tokenize_empty_input_yields_no_tokens() {
        assert!(tokenize("").unwrap().is_empty());
    }

    #[test]
    fn tokenize_single_bare_token() {
        let toks = tokenize("model").unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].value, "model");
        assert_eq!(toks[0].span, 0..5);
        assert!(!toks[0].quoted);
    }

    #[test]
    fn tokenize_two_tokens_split_on_space() {
        let toks = tokenize("model gpt-4").unwrap();
        let values: Vec<&str> = toks.iter().map(|t| t.value.as_str()).collect();
        assert_eq!(values, vec!["model", "gpt-4"]);
        assert_eq!(toks[0].span, 0..5);
        assert_eq!(toks[1].span, 6..11);
    }

    #[test]
    fn tokenize_collapses_runs_of_whitespace() {
        let toks = tokenize("a   b\t\tc").unwrap();
        let values: Vec<&str> = toks.iter().map(|t| t.value.as_str()).collect();
        assert_eq!(values, vec!["a", "b", "c"]);
    }

    #[test]
    fn tokenize_strips_leading_and_trailing_whitespace() {
        let toks = tokenize("  hello  ").unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].value, "hello");
        assert_eq!(toks[0].span, 2..7);
    }

    #[test]
    fn tokenize_double_quoted_preserves_internal_space() {
        let toks = tokenize(r#""hello world""#).unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].value, "hello world");
        assert!(toks[0].quoted);
        assert_eq!(toks[0].span, 0..13);
    }

    #[test]
    fn tokenize_single_quoted_preserves_internal_space() {
        let toks = tokenize("'hello world'").unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].value, "hello world");
        assert!(toks[0].quoted);
    }

    #[test]
    fn tokenize_escaped_quote_inside_double() {
        let toks = tokenize(r#""she said \"hi\"""#).unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].value, r#"she said "hi""#);
    }

    #[test]
    fn tokenize_escaped_backslash_inside_double() {
        let toks = tokenize(r#""a\\b""#).unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].value, r"a\b");
    }

    #[test]
    fn tokenize_unknown_escape_inside_double_keeps_backslash() {
        let toks = tokenize(r#""a\nb""#).unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].value, "a\\nb");
    }

    #[test]
    fn tokenize_unterminated_double_quote_errors() {
        let err = tokenize(r#""hello"#).unwrap_err();
        assert_eq!(err, ParseError::UnterminatedQuote);
    }

    #[test]
    fn tokenize_unterminated_single_quote_errors() {
        let err = tokenize("'hello").unwrap_err();
        assert_eq!(err, ParseError::UnterminatedQuote);
    }

    #[test]
    fn tokenize_unicode_token_span_uses_byte_offsets() {
        let toks = tokenize("\u{1f600} done").unwrap();
        assert_eq!(toks.len(), 2);
        assert_eq!(toks[0].value, "\u{1f600}");
        assert_eq!(toks[0].span, 0..4);
        assert_eq!(toks[1].span, 5..9);
    }

    #[test]
    fn parse_no_args_with_empty_input_succeeds() {
        let parsed = parse_input(&NO_ARGS, "").unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_no_args_with_extra_token_errors() {
        let err = parse_input(&NO_ARGS, "extra").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedTokens(s) if s == "extra"));
    }

    #[test]
    fn parse_choice_valid_value_succeeds() {
        let parsed = parse_input(&ONE_CHOICE, "on").unwrap();
        assert_eq!(parsed.get("state"), Some(&ArgValue::Choice("on".into())));
    }

    #[test]
    fn parse_choice_invalid_value_errors() {
        let err = parse_input(&ONE_CHOICE, "yes").unwrap_err();
        assert!(matches!(
            err,
            ParseError::InvalidChoice { name: "state", ref got, .. } if got == "yes"
        ));
    }

    #[test]
    fn parse_required_choice_missing_errors() {
        let err = parse_input(&ONE_CHOICE, "").unwrap_err();
        assert_eq!(err, ParseError::MissingArg("state"));
    }

    #[test]
    fn parse_optional_choice_missing_yields_empty() {
        let parsed = parse_input(&ONE_CHOICE_OPTIONAL, "").unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_rest_takes_verbatim_remainder() {
        let parsed = parse_input(&ONE_REST, "summarize the chat").unwrap();
        assert_eq!(
            parsed.get("prompt"),
            Some(&ArgValue::Text("summarize the chat".into()))
        );
    }

    #[test]
    fn parse_rest_preserves_internal_quotes() {
        let parsed = parse_input(&ONE_REST, r#"do "the thing" please"#).unwrap();
        assert_eq!(
            parsed.get("prompt"),
            Some(&ArgValue::Text(r#"do "the thing" please"#.into()))
        );
    }

    #[test]
    fn parse_rest_optional_empty_yields_no_value() {
        let parsed = parse_input(&ONE_REST, "").unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_rest_required_empty_errors() {
        let err = parse_input(&ONE_REST_REQUIRED, "").unwrap_err();
        assert_eq!(err, ParseError::MissingArg("title"));
    }

    #[test]
    fn parse_dynamic_choice_accepts_any_token() {
        let parsed = parse_input(&ONE_DYN, "anthropic:claude-sonnet-4-6").unwrap();
        assert_eq!(
            parsed.get("id"),
            Some(&ArgValue::Text("anthropic:claude-sonnet-4-6".into()))
        );
    }

    #[test]
    fn parse_path_takes_one_token() {
        let parsed = parse_input(&ONE_PATH, "src/lib.rs").unwrap();
        assert_eq!(
            parsed.get("path"),
            Some(&ArgValue::Path("src/lib.rs".into()))
        );
    }

    #[test]
    fn parse_flag_with_yes_yields_true() {
        let parsed = parse_input(&ONE_FLAG, "yes").unwrap();
        assert_eq!(parsed.get("on"), Some(&ArgValue::Bool(true)));
    }

    #[test]
    fn parse_flag_with_off_yields_false() {
        let parsed = parse_input(&ONE_FLAG, "off").unwrap();
        assert_eq!(parsed.get("on"), Some(&ArgValue::Bool(false)));
    }

    #[test]
    fn parse_flag_invalid_value_errors() {
        let err = parse_input(&ONE_FLAG, "maybe").unwrap_err();
        assert!(matches!(
            err,
            ParseError::InvalidFlag { name: "on", ref got } if got == "maybe"
        ));
    }

    #[test]
    fn parse_flag_absent_yields_empty() {
        let parsed = parse_input(&ONE_FLAG, "").unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_choice_then_rest_consumes_correctly() {
        let parsed = parse_input(&CHOICE_THEN_REST, "loud hello world").unwrap();
        assert_eq!(parsed.get("voice"), Some(&ArgValue::Choice("loud".into())));
        assert_eq!(
            parsed.get("message"),
            Some(&ArgValue::Text("hello world".into()))
        );
    }

    #[test]
    fn parse_quoted_token_preserves_spaces_in_arg_value() {
        let parsed = parse_input(&ONE_DYN, r#""my model""#).unwrap();
        assert_eq!(parsed.get("id"), Some(&ArgValue::Text("my model".into())));
    }

    #[test]
    fn parse_propagates_unterminated_quote_error() {
        let err = parse_input(&ONE_DYN, r#""open"#).unwrap_err();
        assert_eq!(err, ParseError::UnterminatedQuote);
    }

    #[test]
    fn parse_extra_tokens_after_choice_errors() {
        let err = parse_input(&ONE_CHOICE, "on extra").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedTokens(ref s) if s == "extra"));
    }

    #[test]
    fn complete_empty_input_lists_every_command() {
        let r = TestResolver;
        let c = complete(&registry(), "", 0, &r);
        let names: Vec<&str> = c.items.iter().map(|i| i.value.as_str()).collect();
        assert!(names.contains(&"quit"));
        assert!(names.contains(&"q"));
        assert!(names.contains(&"model"));
        assert_eq!(c.anchor, 0);
    }

    #[test]
    fn complete_partial_command_filters_by_prefix() {
        let r = TestResolver;
        let c = complete(&registry(), "mo", 2, &r);
        let names: Vec<&str> = c.items.iter().map(|i| i.value.as_str()).collect();
        assert_eq!(names, vec!["mouse", "model"]);
        for item in &c.items {
            assert_eq!(item.replace_range, 0..2);
        }
    }

    #[test]
    fn complete_alias_matches_independently() {
        let r = TestResolver;
        let c = complete(&registry(), "q", 1, &r);
        let names: Vec<&str> = c.items.iter().map(|i| i.value.as_str()).collect();
        assert!(names.contains(&"quit"));
        assert!(names.contains(&"q"));
    }

    #[test]
    fn complete_after_command_and_space_starts_first_arg() {
        let r = TestResolver;
        let c = complete(&registry(), "mouse ", 6, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        assert_eq!(names, vec!["on", "off", "toggle"]);
        assert_eq!(c.anchor, 6);
    }

    #[test]
    fn complete_partial_choice_value_filters() {
        let r = TestResolver;
        let c = complete(&registry(), "mouse o", 7, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        assert_eq!(names, vec!["on", "off"]);
        for item in &c.items {
            assert_eq!(item.replace_range, 6..7);
        }
    }

    #[test]
    fn complete_dynamic_choice_uses_resolver() {
        let r = TestResolver;
        let c = complete(&registry(), "model anth", 10, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        assert_eq!(names.len(), 2);
        assert!(names[0].starts_with("anthropic:"));
    }

    #[test]
    fn complete_path_uses_resolver_paths() {
        let r = TestResolver;
        let c = complete(&registry(), "open src/", 9, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        assert_eq!(names, vec!["src/lib.rs", "src/main.rs"]);
    }

    #[test]
    fn complete_past_last_arg_returns_empty() {
        let r = TestResolver;
        let c = complete(&registry(), "mouse on ", 9, &r);
        assert!(c.items.is_empty());
    }

    #[test]
    fn complete_unknown_command_then_arg_returns_empty() {
        let r = TestResolver;
        let c = complete(&registry(), "bogus arg", 9, &r);
        assert!(c.items.is_empty());
    }

    #[test]
    fn complete_in_middle_of_word_still_works() {
        let r = TestResolver;
        let c = complete(&registry(), "model", 3, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        assert_eq!(names, vec!["model"]);
        for item in &c.items {
            assert_eq!(item.replace_range, 0..3);
        }
    }

    #[test]
    fn complete_handles_quoted_partial_prefix() {
        let r = TestResolver;
        let c = complete(&registry(), r#"model "anth"#, 11, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.iter().all(|n| n.starts_with("anth")));
    }

    #[test]
    fn complete_flag_arg_lists_bool_literals() {
        let r = TestResolver;
        let c = complete(&registry(), "verbose ", 8, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        assert_eq!(names, vec!["true", "false", "yes", "no", "on", "off"]);
    }

    #[test]
    fn complete_unterminated_quote_returns_empty() {
        let r = TestResolver;
        let c = complete(&registry(), r#"model "anth"#, 11, &EmptyResolver);
        let _ = r;
        let _ = c;
        let c2 = complete(&registry(), r#"compact "broken"#, 15, &EmptyResolver);
        assert!(c2.items.is_empty());
    }

    #[test]
    fn complete_rest_arg_returns_no_suggestions() {
        let r = TestResolver;
        let c = complete(&registry(), "compact ", 8, &r);
        assert!(c.items.is_empty());
    }

    #[test]
    fn complete_after_choice_and_space_completes_rest_arg() {
        let r = TestResolver;
        let c = complete(&registry(), "say loud ", 9, &r);
        assert!(c.items.is_empty());
    }

    #[test]
    fn complete_first_arg_when_two_args_specified() {
        let r = TestResolver;
        let c = complete(&registry(), "say lo", 6, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        assert_eq!(names, vec!["loud"]);
    }

    const SUB_LEAF_ARGS: CommandSpec = CommandSpec {
        name: "rename",
        aliases: &[],
        description: "rename to N",
        category: CommandCategory::Both,
        args: &[ArgSpec::Rest {
            name: "name",
            optional: false,
            hint: "new name",
        }],
        subcommands: &[],
    };

    const SUB_LEAF_NOARGS: CommandSpec = CommandSpec {
        name: "list",
        aliases: &[],
        description: "list things",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    };

    const SUB_LEAF_DYN: CommandSpec = CommandSpec {
        name: "set",
        aliases: &[],
        description: "switch to one",
        category: CommandCategory::Both,
        args: &[ArgSpec::DynamicChoice {
            name: "id",
            source: ArgSource::Themes,
            optional: false,
        }],
        subcommands: &[],
    };

    const PARENT_WITH_SUBS: CommandSpec = CommandSpec {
        name: "tree",
        aliases: &[],
        description: "session tree ops",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[SUB_LEAF_NOARGS, SUB_LEAF_DYN, SUB_LEAF_ARGS],
    };

    fn subcommand_registry() -> Vec<&'static CommandSpec> {
        vec![&PARENT_WITH_SUBS]
    }

    #[test]
    fn complete_after_parent_lists_subcommands() {
        let r = TestResolver;
        let c = complete(&subcommand_registry(), "tree ", 5, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        assert_eq!(names, vec!["list", "set", "rename"]);
    }

    #[test]
    fn complete_partial_subcommand_filters() {
        let r = TestResolver;
        let c = complete(&subcommand_registry(), "tree s", 6, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        assert_eq!(names, vec!["set"]);
    }

    #[test]
    fn complete_descends_into_subcommand_args() {
        let r = TestResolver;
        let c = complete(&subcommand_registry(), "tree set ", 9, &r);
        let names: Vec<String> = c.items.iter().map(|i| i.value.clone()).collect();
        // Theme resolver returns dark/dawn/moonlight from TestResolver.
        assert!(names.contains(&"dark".to_owned()));
        assert!(names.contains(&"dawn".to_owned()));
    }

    #[test]
    fn complete_subcommand_arg_hints_in_description() {
        let r = TestResolver;
        let c = complete(&subcommand_registry(), "tree ", 5, &r);
        let set_item = c.items.iter().find(|i| i.value == "set").expect("set item");
        assert!(
            set_item
                .description
                .as_deref()
                .unwrap_or("")
                .contains("<id>"),
            "got {:?}",
            set_item.description
        );
    }
}
