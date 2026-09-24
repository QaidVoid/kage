//! Styled lines returned by Lua render functions.
//!
//! Slot components (see [`crate::slots`]), including the
//! `kage.ui.set_header` and `kage.ui.set_footer` row takeovers, and
//! block renderers return one of: a plain string (one unstyled span), a
//! span table (`{ text = "x", hl = "KageMuted", fg = "red", bold = true }`),
//! or an array of those (one line per element; an element that is
//! itself an array becomes a multi-span line). A `nil` return or a
//! non-conforming value yields no lines.
//!
//! Styles are passed through as strings and resolved by the host when
//! it paints, so retained lines never depend on the theme. `hl` names
//! a highlight group whose colors and attributes apply first. `fg` and
//! `bg` take a group name (its fg or bg), a theme role name
//! (`muted_fg`), or a color (`"red"`, `"#1f1f28"`).

use mlua::{Table, Value};

/// Text attributes for a [`ChromeSpan`], packed into a bitset so the
/// span struct stays narrow and the host can map the whole set to
/// terminal modifiers in one pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChromeAttrs(u8);

impl ChromeAttrs {
    /// Bold weight.
    pub const BOLD: ChromeAttrs = ChromeAttrs(1 << 0);
    /// Dim / faint.
    pub const DIM: ChromeAttrs = ChromeAttrs(1 << 1);
    /// Italic slant.
    pub const ITALIC: ChromeAttrs = ChromeAttrs(1 << 2);
    /// Underline.
    pub const UNDERLINE: ChromeAttrs = ChromeAttrs(1 << 3);

    /// An empty attribute set.
    #[must_use]
    pub const fn empty() -> Self {
        ChromeAttrs(0)
    }

    /// `true` if every bit in `other` is set in `self`.
    #[must_use]
    pub const fn contains(self, other: ChromeAttrs) -> bool {
        self.0 & other.0 == other.0
    }

    /// Set the bits in `other`.
    pub const fn insert(&mut self, other: ChromeAttrs) {
        self.0 |= other.0;
    }

    /// Bold is set.
    #[must_use]
    pub const fn bold(self) -> bool {
        self.contains(Self::BOLD)
    }

    /// Dim is set.
    #[must_use]
    pub const fn dim(self) -> bool {
        self.contains(Self::DIM)
    }

    /// Italic is set.
    #[must_use]
    pub const fn italic(self) -> bool {
        self.contains(Self::ITALIC)
    }

    /// Underline is set.
    #[must_use]
    pub const fn underline(self) -> bool {
        self.contains(Self::UNDERLINE)
    }
}

/// One styled run of text within a chrome row. Colors are host-resolved
/// strings; an absent color means "inherit the row default".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChromeSpan {
    /// Display text.
    pub text: String,
    /// Highlight group applied before `fg`, `bg` and `attrs`.
    pub hl: Option<String>,
    /// Foreground: a group name, a theme role or a color; host resolves it.
    pub fg: Option<String>,
    /// Background: a group name, a theme role or a color; host resolves it.
    pub bg: Option<String>,
    /// Text attributes (bold, dim, italic, underline).
    pub attrs: ChromeAttrs,
}

/// One rendered chrome row: an ordered list of styled spans painted
/// left to right.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChromeLine {
    /// Spans painted left to right on this row.
    pub spans: Vec<ChromeSpan>,
}

/// Parse the value a render function returned into a list of styled
/// lines. See the module docs for the accepted shapes; anything else
/// yields no lines.
pub(crate) fn parse_lines(value: &Value) -> Vec<ChromeLine> {
    match value {
        Value::String(s) => vec![ChromeLine {
            spans: vec![ChromeSpan {
                text: s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
                ..ChromeSpan::default()
            }],
        }],
        Value::Table(t) => {
            if t.contains_key("text").unwrap_or(false) {
                return vec![ChromeLine {
                    spans: vec![parse_span_table(t)],
                }];
            }
            let mut lines = Vec::new();
            for entry in t.clone().sequence_values::<Value>().flatten() {
                lines.push(parse_line(&entry));
            }
            lines
        }
        _ => Vec::new(),
    }
}

/// Parse one element of the outer array into a chrome line.
fn parse_line(value: &Value) -> ChromeLine {
    match value {
        Value::String(s) => ChromeLine {
            spans: vec![ChromeSpan {
                text: s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
                ..ChromeSpan::default()
            }],
        },
        Value::Table(t) => {
            if t.contains_key("text").unwrap_or(false) {
                return ChromeLine {
                    spans: vec![parse_span_table(t)],
                };
            }
            let mut spans = Vec::new();
            for entry in t.clone().sequence_values::<Value>().flatten() {
                spans.push(parse_span(&entry));
            }
            ChromeLine { spans }
        }
        _ => ChromeLine::default(),
    }
}

/// Parse one element of a line's span array.
fn parse_span(value: &Value) -> ChromeSpan {
    match value {
        Value::String(s) => ChromeSpan {
            text: s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
            ..ChromeSpan::default()
        },
        Value::Table(t) => parse_span_table(t),
        _ => ChromeSpan::default(),
    }
}

/// Parse a `{ text = ..., fg = ..., bold = ... }` span table. Missing
/// or wrong-typed fields fall back to the span default.
pub(crate) fn parse_span_table(t: &Table) -> ChromeSpan {
    let opt_string = |key: &str| -> Option<String> {
        match t.get::<Value>(key) {
            Ok(Value::String(s)) => s.to_str().map(|s| s.to_owned()).ok(),
            _ => None,
        }
    };
    let flag = |key: &str| -> bool { matches!(t.get::<Value>(key), Ok(Value::Boolean(true))) };
    let mut attrs = ChromeAttrs::empty();
    if flag("bold") {
        attrs.insert(ChromeAttrs::BOLD);
    }
    if flag("dim") {
        attrs.insert(ChromeAttrs::DIM);
    }
    if flag("italic") {
        attrs.insert(ChromeAttrs::ITALIC);
    }
    if flag("underline") {
        attrs.insert(ChromeAttrs::UNDERLINE);
    }
    ChromeSpan {
        text: opt_string("text").unwrap_or_default(),
        hl: opt_string("hl"),
        fg: opt_string("fg"),
        bg: opt_string("bg"),
        attrs,
    }
}
