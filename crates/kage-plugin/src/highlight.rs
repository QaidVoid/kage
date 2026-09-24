//! Highlight groups: `kage.api.hl_set`, `kage.api.hl_get` and the
//! `color_scheme` event.
//!
//! The table ([`SharedHighlights`]) is shared with the host, which
//! recompiles its palette whenever the table's generation moves. This
//! crate cannot read theme files, so the host injects a
//! [`ThemeResolver`]. Setting the `theme` option to a different theme
//! resolves its base groups through it on the owner thread, replaces
//! the base (dropping overrides of `Kage*` groups and keeping the
//! rest), then fires `color_scheme` with the theme name as match. Every
//! load fires `color_scheme` once more after `init.lua`, so highlight
//! setups written as autocmds apply at startup.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use kage_core::highlight::{Highlights, HlSpec};
use kage_core::sync::lock;
use mlua::{Lua, Table, Value};

use crate::api::{SharedHostLog, json_to_lua};
use crate::autocmd;
use crate::error::PluginError;

/// Highlight table shared by the host, the runtime and Lua.
pub type SharedHighlights = Arc<Mutex<Highlights>>;

/// The base groups of one theme.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThemeBase {
    /// Whether the theme leaves the terminal background showing.
    pub transparent: bool,
    /// Group specs by name.
    pub groups: BTreeMap<String, HlSpec>,
}

/// The host's theme registry.
pub trait ThemeResolver: Send + Sync {
    /// Theme names the `theme` option accepts, in display order.
    fn names(&self) -> Vec<String>;

    /// The base groups of theme `name`.
    ///
    /// # Errors
    ///
    /// Returns a message when the theme is unknown or fails to load.
    fn groups(&self, name: &str) -> Result<ThemeBase, String>;
}

/// Shared handle to the injected [`ThemeResolver`].
pub type SharedThemeResolver = Arc<dyn ThemeResolver>;

/// Replace the base groups with those of `theme`.
pub(crate) fn set_base(highlights: &SharedHighlights, theme: &str, base: ThemeBase) {
    lock(highlights).set_base(theme.to_owned(), base.transparent, base.groups);
}

/// Fire `color_scheme` for `theme`, matched on the theme name.
pub(crate) fn fire_color_scheme(lua: &Lua, sink: &SharedHostLog, theme: &str) -> mlua::Result<()> {
    if autocmd::count(lua, "color_scheme") == 0 {
        return Ok(());
    }
    let data = json_to_lua(lua, &serde_json::json!({ "name": theme }))?;
    autocmd::exec(lua, sink, "color_scheme", Some(theme), &data)
}

/// Install `kage.api.hl_set` and `kage.api.hl_get`.
pub(crate) fn install(lua: &Lua, highlights: &SharedHighlights) -> Result<(), PluginError> {
    let api: Table = lua.globals().get::<Table>("kage")?.get("api")?;

    let hl = Arc::clone(highlights);
    api.set(
        "hl_set",
        lua.create_function(move |_, (name, spec): (String, Table)| {
            let spec = spec_from_lua(&spec)?;
            lock(&hl)
                .set(&name, spec)
                .map_err(|e| mlua::Error::external(format!("hl_set: {e}")))
        })?,
    )?;

    let hl = Arc::clone(highlights);
    api.set(
        "hl_get",
        lua.create_function(move |lua, (name, opts): (String, Option<Table>)| {
            let follow = match opts {
                Some(opts) => opts.get::<Option<bool>>("link")? == Some(false),
                None => false,
            };
            let spec = {
                let hl = lock(&hl);
                match hl.get(&name) {
                    None => return Ok(Value::Nil),
                    Some(_) if follow => hl.resolve(&name),
                    Some(spec) => spec.clone(),
                }
            };
            spec_to_lua(lua, &spec).map(Value::Table)
        })?,
    )?;
    Ok(())
}

fn spec_from_lua(table: &Table) -> mlua::Result<HlSpec> {
    let fail = |msg: String| mlua::Error::external(format!("hl_set: {msg}"));
    let text = |key: &str, value: Value| match value {
        Value::String(s) => Ok(Some(s.to_str()?.to_owned())),
        _ => Err(fail(format!("`{key}` must be a string"))),
    };
    let flag = |key: &str, value: Value| match value {
        Value::Boolean(on) => Ok(on),
        _ => Err(fail(format!("`{key}` must be a boolean"))),
    };
    let mut spec = HlSpec::default();
    for pair in table.pairs::<String, Value>() {
        let (key, value) = pair?;
        match key.as_str() {
            "fg" => spec.fg = text(&key, value)?,
            "bg" => spec.bg = text(&key, value)?,
            "link" => spec.link = text(&key, value)?,
            "bold" => spec.bold = flag(&key, value)?,
            "italic" => spec.italic = flag(&key, value)?,
            "underline" => spec.underline = flag(&key, value)?,
            "dim" => spec.dim = flag(&key, value)?,
            "reverse" => spec.reverse = flag(&key, value)?,
            _ => return Err(fail(format!("unknown field `{key}`"))),
        }
    }
    Ok(spec)
}

fn spec_to_lua(lua: &Lua, spec: &HlSpec) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (key, value) in [("fg", &spec.fg), ("bg", &spec.bg), ("link", &spec.link)] {
        if let Some(value) = value {
            table.set(key, value.as_str())?;
        }
    }
    for (key, on) in [
        ("bold", spec.bold),
        ("italic", spec.italic),
        ("underline", spec.underline),
        ("dim", spec.dim),
        ("reverse", spec.reverse),
    ] {
        if on {
            table.set(key, true)?;
        }
    }
    Ok(table)
}

/// Test themes `default` and `tokyo-night`, whose `KageMuted` fg is
/// `#000001` and `#000002`.
#[cfg(test)]
pub(crate) struct FakeThemes;

#[cfg(test)]
impl ThemeResolver for FakeThemes {
    fn names(&self) -> Vec<String> {
        vec!["default".into(), "tokyo-night".into()]
    }

    fn groups(&self, name: &str) -> Result<ThemeBase, String> {
        let fg = match name {
            "default" => "#000001",
            "tokyo-night" => "#000002",
            _ => return Err(format!("unknown theme `{name}`")),
        };
        let muted = HlSpec {
            fg: Some(fg.to_owned()),
            ..HlSpec::default()
        };
        Ok(ThemeBase {
            transparent: false,
            groups: BTreeMap::from([("KageMuted".to_owned(), muted)]),
        })
    }
}

#[cfg(test)]
mod tests;
