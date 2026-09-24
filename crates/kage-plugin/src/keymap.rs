//! Keymaps: `kage.api.keymap_set` / `keymap_del`, `kage.action`, the
//! `kage.register_keybinding` alias and the `[keybindings]` load phase.
//!
//! Mappings live in a [`kage_core::keymap::Keymap`] the runtime shares
//! with the host ([`SharedKeymap`]). The TUI resolves keys against it
//! under a short lock, so no Lua runs to look a key up.
//!
//! * `kage.api.keymap_set(mode, lhs, rhs, opts?)` maps `lhs` in one
//!   mode. `rhs` is a `kage.action` value, a `":command"` string, a Lua
//!   function or `"<Nop>"`. `opts` takes `desc` and `group`. `<leader>`
//!   expands with the `leader` option at set time. The owner recorded
//!   is the layer being loaded: `defaults`, a plugin stem or `init.lua`.
//! * `kage.api.keymap_del(mode, lhs)` removes a mapping and raises when
//!   there is none.
//! * `kage.action.<Name>` is an action value. An action that takes an
//!   integer is a function under its lowercase name
//!   (`kage.action.scroll(-10)`).
//! * `kage.register_keybinding(spec, handler)` maps a chord in mode `g`
//!   and returns an idempotent `off`.
//!
//! A Lua function rhs is kept in a registry table under an id that is
//! never reused. The host fetches it with
//! [`crate::PluginRuntime::keymap_handler`] and runs it through the
//! coroutine bridge, so it may open `kage.ui.*` dialogs.
//!
//! Every value here is a userdata or a closure, so the private copies
//! of the shared tables each environment gets still reach the table.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kage_core::config::KeybindingsConfig;
use kage_core::keymap::{
    ACTIONS, Key, Keymap, Lookup, Mapping, Mode, OWNER_DEFAULTS, OWNER_TOML, OWNER_USER, Rhs,
    parse_key, parse_keys,
};
use kage_core::options::OptionValue;
use kage_core::sync::lock;
use mlua::{Function, Lua, Table, UserData, Value};

use crate::api::{LogLevel, SharedHostLog};
use crate::capabilities::CurrentPlugin;
use crate::error::PluginError;
use crate::options::SharedOptions;

/// Keymap table shared by the host, the runtime and Lua.
pub type SharedKeymap = Arc<Mutex<Keymap>>;

/// Named registry slot of the table holding Lua handlers by id.
const HANDLERS_KEY: &str = "kage.keymap.handlers";

/// Keys that the quit and interrupt hatches take before any mapping
/// that `init.lua` or `config.toml` does not own.
const HATCHES: [&str; 2] = ["<C-q>", "<C-c>"];

/// A `kage.action` value: a Rust action and its optional argument.
#[derive(Clone, Copy, Debug)]
struct KeyAction {
    name: &'static str,
    arg: Option<i64>,
}

impl UserData for KeyAction {}

/// The keymap table plus what a set needs besides it.
#[derive(Clone)]
pub(crate) struct Keymaps {
    pub(crate) table: SharedKeymap,
    options: SharedOptions,
    current: CurrentPlugin,
    sink: SharedHostLog,
    next_id: Arc<AtomicU64>,
}

impl Keymaps {
    /// Bundle the shared state for [`install`].
    pub(crate) fn new(options: SharedOptions, current: CurrentPlugin, sink: SharedHostLog) -> Self {
        Self {
            table: SharedKeymap::default(),
            options,
            current,
            sink,
            next_id: Arc::new(AtomicU64::new(0)),
        }
    }

    fn leader(&self) -> String {
        lock(&self.options)
            .get("leader")
            .and_then(OptionValue::as_str)
            .unwrap_or("\\")
            .to_owned()
    }

    fn owner(&self) -> String {
        match lock(&self.current).as_deref() {
            Some(crate::stdlib::DEFAULTS_ENV) => OWNER_DEFAULTS.to_owned(),
            Some(crate::user::USER_ENV) => OWNER_USER.to_owned(),
            Some(name) => name.to_owned(),
            None => "lua".to_owned(),
        }
    }

    fn keys(&self, fname: &str, lhs: &str) -> mlua::Result<Vec<Key>> {
        parse_keys(lhs, &self.leader()).map_err(|e| fail(format!("{fname}: {e}")))
    }

    fn store_handler(&self, lua: &Lua, handler: Function) -> mlua::Result<u64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        handlers(lua)?.raw_set(id, handler)?;
        Ok(id)
    }

    fn rhs(&self, lua: &Lua, value: Value) -> mlua::Result<Rhs> {
        let bad = || {
            fail(
                "keymap_set: rhs must be a kage.action value, a \":command\" string, \
                 a function or \"<Nop>\""
                    .to_owned(),
            )
        };
        match value {
            Value::UserData(ud) => {
                let action = ud.borrow::<KeyAction>().map_err(|_| bad())?;
                Ok(Rhs::Action {
                    name: action.name,
                    arg: action.arg,
                })
            }
            Value::String(s) => {
                let s = s.to_str()?;
                if s.eq_ignore_ascii_case("<Nop>") {
                    return Ok(Rhs::Nop);
                }
                match s.strip_prefix(':').map(str::trim) {
                    Some(command) if !command.is_empty() => Ok(Rhs::Command(command.to_owned())),
                    _ => Err(bad()),
                }
            }
            Value::Function(f) => Ok(Rhs::Lua(self.store_handler(lua, f)?)),
            _ => Err(bad()),
        }
    }

    fn warn_hatch(&self, keys: &[Key], owner: &str) {
        if owner == OWNER_USER || owner == OWNER_TOML {
            return;
        }
        let Some(hatch) = HATCHES
            .into_iter()
            .find(|h| parse_key(h).is_ok_and(|k| keys == [k]))
        else {
            return;
        };
        lock(&self.sink).log(
            LogLevel::Warn,
            &format!(
                "mapping {hatch} from '{owner}' never fires: the quit and interrupt keys \
                 yield only to init.lua or config.toml"
            ),
        );
    }

    fn set(&self, lua: &Lua, mode: Mode, keys: Vec<Key>, mapping: Mapping) -> mlua::Result<()> {
        let replaced = lock(&self.table).set(mode, keys, mapping);
        drop_handler(lua, replaced.as_ref())
    }

    /// Apply `[keybindings]` from `config.toml`: every `bindings` entry
    /// maps in mode `g` with owner `config.toml`, as a command line or
    /// an `action:<Name>`. Returns one message per entry that could not
    /// be applied, including keys written directly under the table.
    pub(crate) fn apply_toml(
        &self,
        lua: &Lua,
        config: &KeybindingsConfig,
    ) -> mlua::Result<Vec<String>> {
        let mut errors: Vec<String> = config
            .unknown
            .keys()
            .map(|key| {
                format!(
                    "keybindings: unknown key `{key}`; mappings go in the `bindings` table, \
                     as in `bindings = {{ \"{key}\" = \"...\" }}`"
                )
            })
            .collect();
        let leader = self.leader();
        for (lhs, value) in &config.bindings {
            let rhs = match value.strip_prefix("action:") {
                Some(name) => Rhs::action(name, None),
                None => Ok(Rhs::Command(value.clone())),
            };
            match parse_keys(lhs, &leader).and_then(|keys| Ok((keys, rhs?))) {
                Ok((keys, rhs)) => {
                    let mapping = Mapping {
                        rhs,
                        desc: None,
                        group: None,
                        owner: OWNER_TOML.to_owned(),
                    };
                    self.set(lua, Mode::Global, keys, mapping)?;
                }
                Err(err) => errors.push(format!("keybindings: `{lhs}` = \"{value}\": {err}")),
            }
        }
        Ok(errors)
    }

    /// Remove every mapping and drop every Lua handler.
    pub(crate) fn clear(&self, lua: &Lua) -> mlua::Result<()> {
        lock(&self.table).clear();
        lua.set_named_registry_value(HANDLERS_KEY, lua.create_table()?)
    }
}

/// Install `kage.api.keymap_set`, `kage.api.keymap_del`, `kage.action`
/// and `kage.register_keybinding`.
pub(crate) fn install(lua: &Lua, keymaps: &Keymaps) -> Result<(), PluginError> {
    lua.set_named_registry_value(HANDLERS_KEY, lua.create_table()?)?;
    let kage: Table = lua.globals().get("kage")?;
    let api: Table = kage.get("api")?;

    let this = keymaps.clone();
    api.set(
        "keymap_set",
        lua.create_function(
            move |lua, (mode, lhs, rhs, opts): (String, String, Value, Option<Table>)| {
                let mode = parse_mode("keymap_set", &mode)?;
                let keys = this.keys("keymap_set", &lhs)?;
                let (desc, group) = match &opts {
                    Some(opts) => (opts.get("desc")?, opts.get("group")?),
                    None => (None, None),
                };
                let rhs = this.rhs(lua, rhs)?;
                let owner = this.owner();
                this.warn_hatch(&keys, &owner);
                let mapping = Mapping {
                    rhs,
                    desc,
                    group,
                    owner,
                };
                this.set(lua, mode, keys, mapping)
            },
        )?,
    )?;

    let this = keymaps.clone();
    api.set(
        "keymap_del",
        lua.create_function(move |lua, (mode, lhs): (String, String)| {
            let mode = parse_mode("keymap_del", &mode)?;
            let keys = this.keys("keymap_del", &lhs)?;
            let removed = lock(&this.table)
                .del(mode, &keys)
                .map_err(|e| fail(format!("keymap_del: {e}")))?;
            drop_handler(lua, Some(&removed))
        })?,
    )?;

    let actions = lua.create_table()?;
    for def in ACTIONS {
        let name = def.name;
        if def.arg {
            actions.set(
                name.to_ascii_lowercase(),
                lua.create_function(move |_, arg: i64| {
                    Ok(KeyAction {
                        name,
                        arg: Some(arg),
                    })
                })?,
            )?;
        } else {
            actions.set(name, KeyAction { name, arg: None })?;
        }
    }
    kage.set("action", actions)?;

    kage.set("register_keybinding", register_fn(lua, keymaps.clone())?)?;
    Ok(())
}

/// `kage.register_keybinding(spec, handler)`: map a chord in mode `g`
/// with group `plugins` and return an `off` that removes the mapping
/// while it is still this one.
fn register_fn(lua: &Lua, this: Keymaps) -> mlua::Result<Function> {
    lua.create_function(move |lua, (spec, handler): (Value, Function)| {
        const NAME: &str = "kage.register_keybinding";
        let (chord, desc) = match spec {
            Value::String(s) => (s.to_str()?.to_owned(), None),
            Value::Table(t) => {
                let chord: String = t
                    .get("key")
                    .map_err(|_| fail(format!("{NAME}: spec table needs a string `key`")))?;
                let desc: Option<String> = t
                    .get("description")
                    .map_err(|_| fail(format!("{NAME}: `description` must be a string")))?;
                (chord, desc.filter(|d| !d.is_empty()))
            }
            _ => {
                return Err(fail(format!(
                    "{NAME}: spec must be a chord string or a table"
                )));
            }
        };
        let keys = this.keys(NAME, &chord)?;
        let owner = this.owner();
        this.warn_hatch(&keys, &owner);
        let id = this.store_handler(lua, handler)?;
        let mapping = Mapping {
            rhs: Rhs::Lua(id),
            desc,
            group: Some("plugins".to_owned()),
            owner,
        };
        this.set(lua, Mode::Global, keys.clone(), mapping)?;
        let this = this.clone();
        lua.create_function(move |lua, ()| {
            let removed = {
                let mut table = lock(&this.table);
                let mine = matches!(
                    table.lookup(&[Mode::Global], &keys),
                    Lookup::Exact(m) | Lookup::Prefix { exact: Some(m) } if m.rhs == Rhs::Lua(id)
                );
                if mine {
                    table.del(Mode::Global, &keys).ok()
                } else {
                    None
                }
            };
            drop_handler(lua, removed.as_ref())
        })
    })
}

/// The Lua handler stored under `id`.
pub(crate) fn handler(lua: &Lua, id: u64) -> mlua::Result<Function> {
    match handlers(lua)?.raw_get::<Value>(id)? {
        Value::Function(f) => Ok(f),
        _ => Err(fail(format!("no keymap handler {id}"))),
    }
}

fn handlers(lua: &Lua) -> mlua::Result<Table> {
    lua.named_registry_value(HANDLERS_KEY)
}

fn drop_handler(lua: &Lua, mapping: Option<&Mapping>) -> mlua::Result<()> {
    if let Some(Mapping {
        rhs: Rhs::Lua(id), ..
    }) = mapping
    {
        handlers(lua)?.raw_set(*id, Value::Nil)?;
    }
    Ok(())
}

fn parse_mode(fname: &str, mode: &str) -> mlua::Result<Mode> {
    Mode::parse(mode).ok_or_else(|| {
        fail(format!(
            "{fname}: unknown mode '{mode}' (expected n, b, i, v or g)"
        ))
    })
}

fn fail(message: String) -> mlua::Error {
    mlua::Error::external(message)
}

#[cfg(test)]
mod tests;
