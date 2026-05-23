//! `kage.ui.set_header` / `kage.ui.set_footer`: plugin-owned chrome
//! rows.
//!
//! A plugin installs a render function for the top status row
//! (`set_header`) or the bottom modeline row (`set_footer`); the host
//! calls it once per redraw, inside the same Lua mutex tool dispatch
//! uses, and paints the returned styled lines in place of the built-in
//! chrome. Passing `nil` clears the slot and restores the built-in
//! row.
//!
//! The render function receives the row width and returns one of:
//! a plain string (one unstyled span), a span table
//! (`{ text = "x", fg = "red", bold = true }`), or an array of those
//! (one line per element; an element that is itself an array becomes a
//! multi-span line). A `nil` return, a non-conforming value, or a Lua
//! error logs to the host sink and yields no lines, so the host falls
//! back to its built-in chrome rather than failing silently.
//!
//! Colors are passed through as strings (`"red"`, `"#1f1f28"`) and
//! resolved by the host against the active theme; this crate does not
//! depend on the TUI's color types.

use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::api::{LogLevel, SharedHostLog};
use crate::error::PluginError;
use crate::runtime::SharedLua;

/// Which chrome row a [`LuaChrome`] paints. Used only to label render
/// errors in the host log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeSlot {
    /// Top status row.
    Header,
    /// Bottom modeline row.
    Footer,
}

impl ChromeSlot {
    /// Lowercase label used in host-log error messages.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ChromeSlot::Header => "header",
            ChromeSlot::Footer => "footer",
        }
    }
}

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
    /// Foreground color name or `#rrggbb` hex; host resolves it.
    pub fg: Option<String>,
    /// Background color name or `#rrggbb` hex; host resolves it.
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

/// Shared single-slot handle to a plugin chrome renderer. The host
/// clones the inner [`LuaChrome`] per redraw; `set_header` /
/// `set_footer` overwrite it, and `nil` clears it.
pub type SharedChrome = Arc<Mutex<Option<Arc<LuaChrome>>>>;

/// Construct an empty chrome slot.
#[must_use]
pub fn shared_chrome() -> SharedChrome {
    Arc::new(Mutex::new(None))
}

/// A chrome-row renderer defined in Lua.
pub struct LuaChrome {
    slot: ChromeSlot,
    lua: SharedLua,
    sink: SharedHostLog,
    handler_key: Arc<RegistryKey>,
    /// Last successful render with a wall-clock timestamp. Replayed
    /// only for sub-second contention so the host can fall through to
    /// the built-in modeline (and its working spinner) when a long
    /// provider call is keeping the Lua state busy.
    cache: Mutex<(std::time::Instant, Vec<ChromeLine>)>,
}

const CACHE_STALE_AFTER: std::time::Duration = std::time::Duration::from_millis(500);

impl std::fmt::Debug for LuaChrome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaChrome")
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

impl LuaChrome {
    /// Which row this renderer paints.
    #[must_use]
    pub fn slot(&self) -> ChromeSlot {
        self.slot
    }

    /// Call into Lua to produce the row's styled lines for a chrome
    /// area of `width` columns. Uses `try_lock` so the TUI render loop
    /// is never blocked by an in-flight provider call. Brief contention
    /// replays the last render; once the cache is older than
    /// [`CACHE_STALE_AFTER`] the row goes empty so the host can paint
    /// the built-in modeline (with its working spinner).
    #[must_use]
    pub fn render(&self, width: u16) -> Vec<ChromeLine> {
        let Ok(lua) = self.lua.try_lock() else {
            return self.fresh_cached();
        };
        let func: Function = match lua.registry_value(&self.handler_key) {
            Ok(f) => f,
            Err(e) => {
                self.log_error(&e);
                return self.fresh_cached();
            }
        };
        match func.call::<Value>(width) {
            Ok(value) => {
                let lines = parse_lines(&value);
                if let Ok(mut slot) = self.cache.lock() {
                    *slot = (std::time::Instant::now(), lines.clone());
                }
                lines
            }
            Err(e) => {
                self.log_error(&e);
                self.fresh_cached()
            }
        }
    }

    fn fresh_cached(&self) -> Vec<ChromeLine> {
        let Ok(slot) = self.cache.lock() else {
            return Vec::new();
        };
        if slot.0.elapsed() <= CACHE_STALE_AFTER {
            slot.1.clone()
        } else {
            Vec::new()
        }
    }

    fn log_error(&self, e: &dyn std::fmt::Display) {
        if let Ok(mut s) = self.sink.lock() {
            s.log(
                LogLevel::Error,
                &format!("plugin {}: {e}", self.slot.label()),
            );
        }
    }
}

/// Parse the value a chrome render function returned into a list of
/// styled lines. See the module docs for the accepted shapes; anything
/// else yields no lines. Shared with `block_renderers` so a Lua block
/// renderer accepts the exact same return shape as `set_header`.
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
fn parse_span_table(t: &Table) -> ChromeSpan {
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
        fg: opt_string("fg"),
        bg: opt_string("bg"),
        attrs,
    }
}

/// Install `kage.ui.set_header` and `kage.ui.set_footer` on the running
/// Lua state. Each accepts a render function or `nil`; a function
/// replaces the slot's renderer, `nil` clears it, and any other value
/// errors.
pub fn install_chrome(
    lua: &Lua,
    shared_lua: SharedLua,
    sink: SharedHostLog,
    header: SharedChrome,
    footer: SharedChrome,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let ui: Table = kage.get("ui")?;
    ui.set(
        "set_header",
        make_setter(
            lua,
            ChromeSlot::Header,
            shared_lua.clone(),
            sink.clone(),
            header,
        )?,
    )?;
    ui.set(
        "set_footer",
        make_setter(lua, ChromeSlot::Footer, shared_lua, sink, footer)?,
    )?;
    kage.set("ui", ui)?;
    Ok(())
}

fn make_setter(
    lua: &Lua,
    slot: ChromeSlot,
    shared_lua: SharedLua,
    sink: SharedHostLog,
    target: SharedChrome,
) -> Result<Function, PluginError> {
    let func = lua.create_function(move |lua, value: Value| {
        let mut guard = target
            .lock()
            .map_err(|_| mlua::Error::external("plugin chrome slot poisoned"))?;
        match value {
            Value::Nil => {
                *guard = None;
                Ok(())
            }
            Value::Function(f) => {
                let handler_key = lua.create_registry_value(f)?;
                *guard = Some(Arc::new(LuaChrome {
                    slot,
                    lua: shared_lua.clone(),
                    sink: sink.clone(),
                    handler_key: Arc::new(handler_key),
                    cache: Mutex::new((std::time::Instant::now(), Vec::new())),
                }));
                Ok(())
            }
            _ => Err(mlua::Error::external(format!(
                "kage.ui.set_{}: expected a function or nil",
                slot.label()
            ))),
        }
    })?;
    Ok(func)
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    #[test]
    fn set_header_registers_a_renderer() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.ui.set_header(function(_w) return 'hi' end)")
            .unwrap();
        let chrome = rt.header_chrome().expect("header registered");
        let lines = chrome.render(80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].text, "hi");
    }

    #[test]
    fn set_header_nil_clears_the_slot() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.ui.set_header(function() return 'x' end)")
            .unwrap();
        assert!(rt.header_chrome().is_some());
        rt.eval("kage.ui.set_header(nil)").unwrap();
        assert!(rt.header_chrome().is_none());
    }

    #[test]
    fn set_header_rejects_non_function() {
        let rt = PluginRuntime::new().unwrap();
        assert!(rt.eval("kage.ui.set_header(42)").is_err());
        assert!(rt.header_chrome().is_none());
    }

    #[test]
    fn header_and_footer_are_independent_slots() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.ui.set_footer(function() return 'foot' end)")
            .unwrap();
        assert!(rt.header_chrome().is_none());
        let foot = rt.footer_chrome().expect("footer registered");
        assert_eq!(foot.render(80)[0].spans[0].text, "foot");
    }

    #[test]
    fn render_parses_a_span_table() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            "kage.ui.set_header(function()
                 return { text = 'on', fg = 'red', bold = true }
             end)",
        )
        .unwrap();
        let span = &rt.header_chrome().unwrap().render(80)[0].spans[0];
        assert_eq!(span.text, "on");
        assert_eq!(span.fg.as_deref(), Some("red"));
        assert!(span.attrs.bold());
        assert!(!span.attrs.dim());
    }

    #[test]
    fn render_parses_an_array_of_multi_span_lines() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            "kage.ui.set_footer(function()
                 return {
                     'plain',
                     { { text = 'a' }, { text = 'b', dim = true } },
                 }
             end)",
        )
        .unwrap();
        let lines = rt.footer_chrome().unwrap().render(80);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].text, "plain");
        assert_eq!(lines[1].spans.len(), 2);
        assert_eq!(lines[1].spans[1].text, "b");
        assert!(lines[1].spans[1].attrs.dim());
    }

    #[test]
    fn render_receives_width_argument() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.ui.set_header(function(w) return tostring(w) end)")
            .unwrap();
        assert_eq!(
            rt.header_chrome().unwrap().render(123)[0].spans[0].text,
            "123"
        );
    }

    #[test]
    fn render_error_yields_no_lines() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.ui.set_header(function() error('boom') end)")
            .unwrap();
        assert!(rt.header_chrome().unwrap().render(80).is_empty());
    }

    #[test]
    fn render_nil_yields_no_lines() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval("kage.ui.set_header(function() return nil end)")
            .unwrap();
        assert!(rt.header_chrome().unwrap().render(80).is_empty());
    }
}
