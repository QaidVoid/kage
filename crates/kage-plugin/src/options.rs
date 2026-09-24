//! Options: `kage.opt` and `kage.api.option_get` / `option_set`.
//!
//! Values live in a [`kage_core::options::OptionStore`] the host shares
//! with the runtime ([`SharedOptions`]). The host seeds it from config
//! before any Lua runs, so `init.lua`, which loads last, wins over TOML
//! and environment values.
//!
//! * `kage.opt.<name>` reads the current value. Assigning to it sets
//!   the option. Unknown names and invalid values raise with what is
//!   valid.
//! * `kage.api.option_get(name)` returns the value and its source
//!   (`default`, `toml`, `lua` or `runtime`).
//! * `kage.api.option_set(name, value)` is the assignment as a call.
//!
//! A set validates the value (`theme` must be one of the names of the
//! [`crate::ThemeResolver`] from [`crate::PluginRuntimeBuilder::themes`]
//! when the host gave one), records the source, queues the change for
//! the host to apply, and fires `option_set` with
//! `{ name, old, new, source }`, matched on the option name. A `theme`
//! set that picks a different theme first switches the base highlight
//! groups and fires `color_scheme` (see [`crate::highlight`]). Host UI changes go through
//! [`crate::PluginRuntime::set_option`], which runs the same set on the
//! owner thread, so every `option_set` fires there in order.
//!
//! `kage.opt` is a userdata rather than a table, so the private copies
//! of the shared tables each environment gets still reach the store.

use std::sync::{Arc, Mutex};

use kage_core::options::{self, OptionError, OptionSource, OptionStore, OptionValue};
use kage_core::sync::lock;
use mlua::{Lua, MetaMethod, Table, UserData, UserDataMethods, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua};
use crate::autocmd;
use crate::error::PluginError;
use crate::highlight::{self, SharedHighlights, SharedThemeResolver, ThemeBase};

/// Option store shared by the host, the runtime and Lua.
pub type SharedOptions = Arc<Mutex<OptionStore>>;

/// The option store plus what a set needs besides it.
#[derive(Clone)]
pub(crate) struct Options {
    pub(crate) store: SharedOptions,
    pub(crate) themes: Option<SharedThemeResolver>,
    pub(crate) highlights: SharedHighlights,
    pub(crate) sink: SharedHostLog,
}

impl Options {
    /// Validate `value` for option `name` without setting it.
    pub(crate) fn check(&self, name: &str, value: OptionValue) -> Result<OptionValue, OptionError> {
        let def = options::lookup(name)?;
        let value = def.validate(value)?;
        if let (Some(themes), OptionValue::Str(theme)) = (&self.themes, &value)
            && def.name == "theme"
        {
            let names = themes.names();
            if !names.contains(theme) {
                let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
                return Err(OptionError::Invalid {
                    name: def.name,
                    expected: format!("one of {}", quoted.join(", ")),
                });
            }
        }
        Ok(value)
    }

    /// Validate and set option `name`, then fire `option_set`. Runs on
    /// the owner thread.
    pub(crate) fn set(
        &self,
        lua: &Lua,
        name: &str,
        value: OptionValue,
        source: OptionSource,
    ) -> mlua::Result<()> {
        let value = self.check(name, value).map_err(mlua::Error::external)?;
        let base = self
            .theme_switch(name, &value)
            .map_err(mlua::Error::external)?;
        let change = lock(&self.store)
            .set(name, value, source)
            .map_err(mlua::Error::external)?;
        if let (Some(base), OptionValue::Str(theme)) = (base, &change.new) {
            highlight::set_base(&self.highlights, theme, base);
            highlight::fire_color_scheme(lua, &self.sink, theme)?;
        }
        if autocmd::count(lua, "option_set") == 0 {
            return Ok(());
        }
        let payload = serde_json::json!({
            "name": change.name,
            "old": to_json(&change.old),
            "new": to_json(&change.new),
            "source": change.source.as_str(),
        });
        let data = json_to_lua(lua, &payload)?;
        autocmd::exec(lua, &self.sink, "option_set", Some(change.name), &data)
    }

    /// The base groups to switch to when setting `name` to `value`
    /// picks a theme other than the one the groups came from.
    fn theme_switch(&self, name: &str, value: &OptionValue) -> Result<Option<ThemeBase>, String> {
        let (Some(themes), "theme", OptionValue::Str(theme)) = (&self.themes, name, value) else {
            return Ok(None);
        };
        if lock(&self.highlights).theme() == theme {
            return Ok(None);
        }
        themes.groups(theme).map(Some)
    }

    /// The current value of the `theme` option.
    pub(crate) fn theme(&self) -> String {
        lock(&self.store)
            .get("theme")
            .and_then(OptionValue::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    /// Load the base groups of the current theme. When that fails the
    /// error is logged and the `default` theme is loaded instead.
    pub(crate) fn load_theme(&self) {
        let Some(themes) = &self.themes else {
            return;
        };
        let theme = self.theme();
        let loaded = themes
            .groups(&theme)
            .map(|base| (theme, base))
            .or_else(|err| {
                lock(&self.sink).log(LogLevel::Error, &format!("theme: {err}"));
                themes
                    .groups("default")
                    .map(|base| ("default".to_owned(), base))
            });
        if let Ok((theme, base)) = loaded {
            highlight::set_base(&self.highlights, &theme, base);
        }
    }

    fn set_lua(&self, lua: &Lua, name: &str, value: Value) -> mlua::Result<()> {
        let def = options::lookup(name).map_err(mlua::Error::external)?;
        let value = from_lua(value).ok_or_else(|| {
            mlua::Error::external(OptionError::Invalid {
                name: def.name,
                expected: def.expected(),
            })
        })?;
        self.set(lua, name, value, OptionSource::Lua)
    }

    fn get(&self, lua: &Lua, name: &str) -> mlua::Result<(Value, &'static str)> {
        let def = options::lookup(name).map_err(mlua::Error::external)?;
        let store = lock(&self.store);
        let value = store
            .get(def.name)
            .cloned()
            .unwrap_or_else(|| def.default_value());
        let source = store.source(def.name).unwrap_or(OptionSource::Default);
        Ok((to_lua(lua, &value)?, source.as_str()))
    }
}

/// The `kage.opt` accessor.
struct Opt(Options);

impl UserData for Opt {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, name: String| {
            this.0.get(lua, &name).map(|(value, _)| value)
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |lua, this, (name, value): (String, Value)| this.0.set_lua(lua, &name, value),
        );
    }
}

/// Install `kage.opt`, `kage.api.option_get` and `kage.api.option_set`.
pub(crate) fn install(lua: &Lua, options: &Options) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let api: Table = kage.get("api")?;

    let get = options.clone();
    api.set(
        "option_get",
        lua.create_function(move |lua, name: String| get.get(lua, &name))?,
    )?;

    let set = options.clone();
    api.set(
        "option_set",
        lua.create_function(move |lua, (name, value): (String, Value)| {
            set.set_lua(lua, &name, value)
        })?,
    )?;

    kage.set("opt", Opt(options.clone()))?;
    Ok(())
}

fn from_lua(value: Value) -> Option<OptionValue> {
    Some(match value {
        Value::Boolean(b) => OptionValue::Bool(b),
        Value::Integer(n) => OptionValue::Int(n),
        Value::Number(x) => OptionValue::Float(x),
        Value::String(s) => OptionValue::Str(s.to_str().ok()?.to_owned()),
        _ => return None,
    })
}

fn to_lua(lua: &Lua, value: &OptionValue) -> mlua::Result<Value> {
    Ok(match value {
        OptionValue::Bool(b) => Value::Boolean(*b),
        OptionValue::Int(n) => Value::Integer(*n),
        OptionValue::Float(x) => Value::Number(*x),
        OptionValue::Str(s) => Value::String(lua.create_string(s)?),
    })
}

fn to_json(value: &OptionValue) -> serde_json::Value {
    match value {
        OptionValue::Bool(b) => serde_json::json!(b),
        OptionValue::Int(n) => serde_json::json!(n),
        OptionValue::Float(x) => serde_json::json!(x),
        OptionValue::Str(s) => serde_json::json!(s),
    }
}

#[cfg(test)]
mod tests;
