//! `kage.add_autocomplete_provider`: stacked completion providers for
//! the prompt input area.
//!
//! A plugin registers a provider:
//! ```lua
//! kage.add_autocomplete_provider({
//!     name = "files",
//!     complete = function(prefix, ctx)
//!         -- ctx = { text = <full input>, cursor = <byte offset> }
//!         return {
//!             { value = "README.md", label = "README.md",
//!               detail = "file", range = { ctx.cursor - #prefix, ctx.cursor } },
//!         }
//!     end,
//! })
//! ```
//! The host pulls the provider stack via
//! [`crate::PluginRuntime::registered_autocomplete_providers`] and, on
//! each input change, calls [`LuaAutocompleteProvider::complete`] inside
//! the same Lua mutex tool dispatch uses (synchronous, like
//! [`crate::widgets::LuaWidget`]). Providers are consulted in reverse
//! registration order so the most recently added one wins; this is the
//! foundation for `@file`-style references.
//!
//! `complete` returns an array of item tables. `value` is the only
//! required field (an item missing it is dropped). `label` defaults to
//! `value`; `detail` is an optional dim annotation; `range` is an
//! optional `{ from, to }` pair of 0-based byte offsets into the input
//! the host should overwrite (absent means "let the host replace the
//! matched prefix"). A `nil` return, a non-table return, or a Lua
//! error logs to the host sink and yields no items.

use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::api::{LogLevel, SharedHostLog};
use crate::error::PluginError;
use crate::runtime::SharedLua;

/// One completion candidate a provider produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutocompleteItem {
    /// Text shown in the popup row. Defaults to [`Self::value`].
    pub label: String,
    /// Optional dim annotation painted beside the label.
    pub detail: Option<String>,
    /// Replacement text inserted when the item is accepted.
    pub value: String,
    /// `(from, to)` 0-based byte offsets into the input the host should
    /// overwrite with [`Self::value`]. `None` means the host replaces
    /// the matched prefix span it computed.
    pub range: Option<(usize, usize)>,
}

/// Shared, ordered stack of registered providers. The host snapshots
/// it and consults entries in reverse order (last registered first).
pub type RegisteredAutocompleteProviders = Arc<Mutex<Vec<Arc<LuaAutocompleteProvider>>>>;

/// Construct an empty provider stack.
#[must_use]
pub fn registered_autocomplete_providers() -> RegisteredAutocompleteProviders {
    Arc::new(Mutex::new(Vec::new()))
}

/// A completion provider defined in Lua.
pub struct LuaAutocompleteProvider {
    name: String,
    lua: SharedLua,
    sink: SharedHostLog,
    handler_key: Arc<RegistryKey>,
}

impl std::fmt::Debug for LuaAutocompleteProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaAutocompleteProvider")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl LuaAutocompleteProvider {
    /// Identifier the host uses to deduplicate providers and to label
    /// any completion error.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Call into Lua to produce completions for `prefix`, passing the
    /// full input `text` and the `cursor` byte offset as a context
    /// table. A Lua error, a poisoned Lua mutex, or a non-conforming
    /// return logs to the sink and yields no items, so a broken
    /// provider degrades to "no suggestions" rather than failing the
    /// input.
    #[must_use]
    pub fn complete(&self, prefix: &str, text: &str, cursor: usize) -> Vec<AutocompleteItem> {
        let Ok(lua) = self.lua.lock() else {
            return Vec::new();
        };
        let func: Function = match lua.registry_value(&self.handler_key) {
            Ok(f) => f,
            Err(e) => {
                self.log_error(&e);
                return Vec::new();
            }
        };
        let ctx = match lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.log_error(&e);
                return Vec::new();
            }
        };
        if let Err(e) = ctx
            .set("text", text)
            .and_then(|()| ctx.set("cursor", cursor))
        {
            self.log_error(&e);
            return Vec::new();
        }
        match func.call::<Value>((prefix.to_owned(), ctx)) {
            Ok(Value::Table(items)) => parse_items(&items),
            Ok(_) => Vec::new(),
            Err(e) => {
                self.log_error(&e);
                Vec::new()
            }
        }
    }

    fn log_error(&self, e: &dyn std::fmt::Display) {
        if let Ok(mut s) = self.sink.lock() {
            s.log(
                LogLevel::Error,
                &format!("plugin autocomplete '{}': {e}", self.name),
            );
        }
    }
}

/// Parse the array a provider returned into items, dropping any entry
/// without a non-empty `value`.
fn parse_items(items: &Table) -> Vec<AutocompleteItem> {
    let mut out = Vec::new();
    for entry in items.clone().sequence_values::<Value>().flatten() {
        let Value::Table(t) = entry else {
            continue;
        };
        let value = match t.get::<Value>("value") {
            Ok(Value::String(s)) => s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
            _ => String::new(),
        };
        if value.is_empty() {
            continue;
        }
        let label = match t.get::<Value>("label") {
            Ok(Value::String(s)) => s.to_str().map_or_else(|_| value.clone(), |s| s.to_owned()),
            _ => value.clone(),
        };
        let detail = match t.get::<Value>("detail") {
            Ok(Value::String(s)) => s.to_str().map(|s| s.to_owned()).ok(),
            _ => None,
        };
        out.push(AutocompleteItem {
            label,
            detail,
            value,
            range: parse_range(&t),
        });
    }
    out
}

/// Parse an optional `range = { from, to }` pair. Accepts a 2-element
/// array or a `{ from =, to = }` table; anything else yields `None`.
fn parse_range(t: &Table) -> Option<(usize, usize)> {
    let Ok(Value::Table(r)) = t.get::<Value>("range") else {
        return None;
    };
    let as_usize = |v: Value| -> Option<usize> {
        match v {
            Value::Integer(i) if i >= 0 => usize::try_from(i).ok(),
            _ => None,
        }
    };
    let from = r
        .get::<Value>(1)
        .ok()
        .and_then(&as_usize)
        .or_else(|| r.get::<Value>("from").ok().and_then(&as_usize))?;
    let to = r
        .get::<Value>(2)
        .ok()
        .and_then(&as_usize)
        .or_else(|| r.get::<Value>("to").ok().and_then(&as_usize))?;
    if to < from { None } else { Some((from, to)) }
}

/// Install `kage.add_autocomplete_provider` on the running Lua state.
/// Each call pushes a [`LuaAutocompleteProvider`] onto `registered`;
/// re-adding a provider with the same `name` replaces the existing one
/// in place so a plugin can hot-reload its definition without changing
/// stack order.
pub fn install_add_autocomplete_provider(
    lua: &Lua,
    shared_lua: SharedLua,
    sink: SharedHostLog,
    registered: RegisteredAutocompleteProviders,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "add_autocomplete_provider",
        lua.create_function(move |lua, spec: Table| {
            let name: String = spec.get("name")?;
            let complete: Function = spec.get("complete")?;
            let handler_key = lua.create_registry_value(complete)?;
            let provider = Arc::new(LuaAutocompleteProvider {
                name: name.clone(),
                lua: shared_lua.clone(),
                sink: sink.clone(),
                handler_key: Arc::new(handler_key),
            });
            let mut list = registered
                .lock()
                .map_err(|_| mlua::Error::external("plugin autocomplete registry poisoned"))?;
            if let Some(slot) = list.iter_mut().find(|p| p.name == name) {
                *slot = provider;
            } else {
                list.push(provider);
            }
            Ok(())
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::PluginRuntime;

    #[test]
    fn add_provider_appends_to_stack() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.add_autocomplete_provider({
                name = 'files',
                complete = function(_p, _c) return {} end,
            })
            ",
        )
        .unwrap();
        let providers = rt.registered_autocomplete_providers();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name(), "files");
    }

    #[test]
    fn complete_returns_parsed_items() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.add_autocomplete_provider({
                name = 'p',
                complete = function(prefix, ctx)
                    return {
                        { value = 'README.md', detail = 'file' },
                        { value = 'src/', label = 'src/', range = { 0, ctx.cursor } },
                    }
                end,
            })
            ",
        )
        .unwrap();
        let p = &rt.registered_autocomplete_providers()[0];
        let items = p.complete("RE", "RE", 2);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].value, "README.md");
        assert_eq!(items[0].label, "README.md");
        assert_eq!(items[0].detail.as_deref(), Some("file"));
        assert_eq!(items[0].range, None);
        assert_eq!(items[1].value, "src/");
        assert_eq!(items[1].range, Some((0, 2)));
    }

    #[test]
    fn complete_receives_prefix_and_context() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.add_autocomplete_provider({
                name = 'echo',
                complete = function(prefix, ctx)
                    return { { value = prefix .. ':' .. ctx.text .. ':' .. tostring(ctx.cursor) } }
                end,
            })
            ",
        )
        .unwrap();
        let p = &rt.registered_autocomplete_providers()[0];
        let items = p.complete("fo", "foo", 2);
        assert_eq!(items[0].value, "fo:foo:2");
    }

    #[test]
    fn items_without_value_are_dropped() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.add_autocomplete_provider({
                name = 'p',
                complete = function() return { { label = 'no value' }, { value = 'ok' } } end,
            })
            ",
        )
        .unwrap();
        let items = rt.registered_autocomplete_providers()[0].complete("", "", 0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "ok");
    }

    #[test]
    fn re_adding_same_name_replaces_in_place() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.add_autocomplete_provider({ name='a', complete=function() return {{value='1'}} end })
            kage.add_autocomplete_provider({ name='b', complete=function() return {} end })
            kage.add_autocomplete_provider({ name='a', complete=function() return {{value='2'}} end })
            ",
        )
        .unwrap();
        let providers = rt.registered_autocomplete_providers();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].name(), "a");
        assert_eq!(providers[0].complete("", "", 0)[0].value, "2");
    }

    #[test]
    fn provider_error_yields_no_items() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.add_autocomplete_provider({
                name = 'oops',
                complete = function() error('boom') end,
            })
            ",
        )
        .unwrap();
        assert!(
            rt.registered_autocomplete_providers()[0]
                .complete("x", "x", 1)
                .is_empty()
        );
    }

    #[test]
    fn nil_return_yields_no_items() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.add_autocomplete_provider({
                name = 'n',
                complete = function() return nil end,
            })
            ",
        )
        .unwrap();
        assert!(
            rt.registered_autocomplete_providers()[0]
                .complete("x", "x", 1)
                .is_empty()
        );
    }
}
