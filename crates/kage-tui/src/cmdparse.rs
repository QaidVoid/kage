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

/// Find the closest command name to `input` from the registry using a
/// simple edit-distance heuristic. Returns the best-matching name
/// (primary or alias) if the distance is below a reasonable
/// threshold, or `None` when nothing looks close enough.
///
/// Used by the error UX (PN.9) to suggest "did you mean X?" when the
/// user types an unknown command name.
#[must_use]
pub fn suggest_command(registry: &[&CommandSpec], input: &str) -> Option<&'static str> {
    let input_lower = input.to_ascii_lowercase();
    let mut best_name: Option<&'static str> = None;
    let mut best_dist = usize::MAX;

    for spec in registry {
        for name in spec.names() {
            let name_lower = name.to_ascii_lowercase();
            let dist = edit_distance(&input_lower, &name_lower);
            if dist < best_dist {
                best_dist = dist;
                best_name = Some(name);
            }
        }
    }

    // Only suggest when the distance is "close enough": at most
    // half the length of the typed input (with a floor of 2 so
    // single-char typos always suggest). Empty input never matches.
    if input.is_empty() {
        return None;
    }
    let threshold = (input.len() / 2).max(2);
    if best_dist <= threshold {
        best_name
    } else {
        None
    }
}

/// Compute the Levenshtein edit distance between two strings.
/// Optimised for short strings (command names): uses the
/// classic O(n*m) DP without any early-exit tricks.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev = vec![0usize; m + 1];
    let mut curr = vec![0usize; m + 1];
    for (j, slot) in prev.iter_mut().enumerate() {
        *slot = j;
    }
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = prev[j]
                .saturating_add(1)
                .min(curr[j - 1].saturating_add(1))
                .min(prev[j - 1].saturating_add(cost));
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

#[cfg(test)]
mod tests;
