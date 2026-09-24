//! Autocmds: event subscriptions with groups, patterns and `once`.
//!
//! Rust keeps the metadata of every autocmd in [`Autocmds`], shared
//! with [`crate::PluginRuntime`] behind a mutex, so the host can count
//! the subscribers of an event without waiting on the Lua owner thread.
//! Lua keeps only `id -> callback` in a registry table. The primitives
//! live on `kage.api`:
//!
//! * `autocmd_create(event, opts)` returns an integer id. `opts` holds
//!   `callback` (required), `group`, `pattern`, `once` and `desc`.
//!   Unknown event names raise.
//! * `autocmd_del(id)` removes one autocmd. A missing id is ignored.
//! * `augroup_create(name, opts?)` returns the group id. With
//!   `clear = true` (the default) an existing group loses its autocmds,
//!   so re-running a config that creates the group is idempotent.
//! * `augroup_del(group)` removes a group, by id or name, and its
//!   autocmds.
//! * `autocmd_exec(event, opts?)` fires the event with `opts.pattern` as
//!   the match and `opts.data` as the payload. Nesting is capped at 16
//!   levels.
//!
//! Callbacks receive `ev = { id, event, match, group, data }`. Events
//! with a match key filter on it: the tool name for `tool_call` and
//! `tool_result`, the new value for `model_select` and
//! `thinking_level_select`, the option name for `option_set`, and the
//! exec pattern for `user`. A pattern
//! is an exact string or a list of them, and `*` matches everything.
//! Events without a match key accept only `*`. A `once` autocmd is
//! removed before its callback runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kage_core::sync::lock;
use mlua::{FromLuaMulti, Function, Lua, Table, Value};

use crate::api::{LogLevel, SharedHostLog};
use crate::capabilities::CurrentPlugin;
use crate::error::PluginError;
use crate::events::KNOWN_EVENTS;

/// Lua-registry key of the `id -> callback` table.
const CALLBACKS_KEY: &str = "kage._autocmds";

/// Deepest allowed nesting of `autocmd_exec` calls.
const MAX_EXEC_DEPTH: usize = 16;

/// Payload field each event matches patterns against.
const MATCH_FIELDS: &[(&str, &str)] = &[
    ("tool_call", "name"),
    ("tool_result", "name"),
    ("model_select", "next"),
    ("thinking_level_select", "next"),
    ("option_set", "name"),
];

/// Autocmd metadata shared between the owner thread and the host.
pub(crate) type SharedAutocmds = Arc<Mutex<Autocmds>>;

/// Every live autocmd and group. Ids are never reused, even across a
/// reload, so a stale `off` cannot delete a newer autocmd.
#[derive(Default)]
pub(crate) struct Autocmds {
    next_id: i64,
    by_event: HashMap<String, Vec<Autocmd>>,
    groups: HashMap<String, i64>,
    exec_depth: usize,
}

struct Autocmd {
    id: i64,
    group: Option<i64>,
    patterns: Vec<String>,
    once: bool,
    origin: Arc<str>,
}

impl Autocmd {
    fn matches(&self, matched: Option<&str>) -> bool {
        self.patterns.is_empty() || matched.is_some_and(|m| self.patterns.iter().any(|p| p == m))
    }
}

impl Autocmds {
    /// Number of autocmds subscribed to `event`.
    pub(crate) fn count(&self, event: &str) -> usize {
        self.by_event.get(event).map_or(0, Vec::len)
    }

    fn next_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    fn remove(&mut self, id: i64) {
        for list in self.by_event.values_mut() {
            list.retain(|a| a.id != id);
        }
    }

    fn remove_group(&mut self, group: i64) -> Vec<i64> {
        let mut removed = Vec::new();
        for list in self.by_event.values_mut() {
            list.retain(|a| {
                let hit = a.group == Some(group);
                if hit {
                    removed.push(a.id);
                }
                !hit
            });
        }
        removed
    }

    fn group_id(&self, group: &Value) -> Option<i64> {
        match group {
            Value::Integer(id) => self.groups.values().any(|g| g == id).then_some(*id),
            Value::String(name) => self.groups.get(&*name.to_str().ok()?).copied(),
            _ => None,
        }
    }

    /// Snapshot the autocmds for `event` that accept `matched`, and
    /// drop the `once` ones from the table.
    fn take_matching(&mut self, event: &str, matched: Option<&str>) -> Vec<Picked> {
        let Some(list) = self.by_event.get_mut(event) else {
            return Vec::new();
        };
        let picked: Vec<Picked> = list
            .iter()
            .filter(|a| a.matches(matched))
            .map(|a| Picked {
                id: a.id,
                group: a.group,
                once: a.once,
                origin: Arc::clone(&a.origin),
            })
            .collect();
        if picked.iter().any(|p| p.once) {
            list.retain(|a| !(a.once && a.matches(matched)));
        }
        picked
    }
}

struct Picked {
    id: i64,
    group: Option<i64>,
    once: bool,
    origin: Arc<str>,
}

/// One autocmd selected for a dispatch, with its callback resolved.
pub(crate) struct Target {
    id: i64,
    group: Option<i64>,
    origin: Arc<str>,
    callback: Function,
}

impl Target {
    /// Call the callback with `ev = { id, event, match, group, data }`.
    pub(crate) fn call<R: FromLuaMulti>(
        &self,
        lua: &Lua,
        event: &str,
        matched: Option<&str>,
        data: Value,
    ) -> mlua::Result<R> {
        let ev = lua.create_table()?;
        ev.raw_set("id", self.id)?;
        ev.raw_set("event", event)?;
        ev.raw_set("match", matched)?;
        ev.raw_set("group", self.group)?;
        ev.raw_set("data", data)?;
        self.callback.call(ev)
    }

    /// Log that this callback raised while handling `event`.
    pub(crate) fn log_error(&self, sink: &SharedHostLog, event: &str, err: impl std::fmt::Display) {
        lock(sink).log(
            LogLevel::Error,
            &format!("plugin handler for '{event}'{} raised: {err}", self.origin),
        );
    }
}

/// Whether `event` is in [`KNOWN_EVENTS`].
fn is_known(event: &str) -> bool {
    KNOWN_EVENTS.iter().any(|(name, _, _)| *name == event)
}

fn has_match_key(event: &str) -> bool {
    event == "user" || MATCH_FIELDS.iter().any(|(name, _)| *name == event)
}

/// The value a host payload for `event` matches patterns against.
pub(crate) fn match_key<'a>(event: &str, payload: &'a serde_json::Value) -> Option<&'a str> {
    let (_, field) = MATCH_FIELDS.iter().find(|(name, _)| *name == event)?;
    payload.get(field)?.as_str()
}

/// Number of autocmds subscribed to `event` on this Lua state.
pub(crate) fn count(lua: &Lua, event: &str) -> usize {
    lua.app_data_ref::<SharedAutocmds>()
        .map_or(0, |shared| lock(&shared).count(event))
}

/// Select the autocmds for `event` that accept `matched`, in creation
/// order. `once` autocmds are removed before this returns.
pub(crate) fn targets(lua: &Lua, event: &str, matched: Option<&str>) -> mlua::Result<Vec<Target>> {
    let picked = match lua.app_data_ref::<SharedAutocmds>() {
        Some(shared) => lock(&shared).take_matching(event, matched),
        None => return Ok(Vec::new()),
    };
    if picked.is_empty() {
        return Ok(Vec::new());
    }
    let callbacks: Table = lua.named_registry_value(CALLBACKS_KEY)?;
    let mut out = Vec::with_capacity(picked.len());
    for p in picked {
        let Some(callback) = callbacks.raw_get::<Option<Function>>(p.id)? else {
            continue;
        };
        if p.once {
            callbacks.raw_set(p.id, Value::Nil)?;
        }
        out.push(Target {
            id: p.id,
            group: p.group,
            origin: p.origin,
            callback,
        });
    }
    Ok(out)
}

/// Call every target with the same `data`, logging and skipping the
/// ones that raise.
pub(crate) fn notify(
    lua: &Lua,
    sink: &SharedHostLog,
    event: &str,
    matched: Option<&str>,
    targets: &[Target],
    data: &Value,
) {
    for target in targets {
        if let Err(err) = target.call::<()>(lua, event, matched, data.clone()) {
            target.log_error(sink, event, err);
        }
    }
}

/// Drop every autocmd and group. Used by reload.
pub(crate) fn clear(lua: &Lua) -> mlua::Result<()> {
    if let Some(shared) = lua.app_data_ref::<SharedAutocmds>() {
        let mut autocmds = lock(&shared);
        autocmds.by_event.clear();
        autocmds.groups.clear();
    }
    lua.set_named_registry_value(CALLBACKS_KEY, lua.create_table()?)
}

fn drop_callbacks(lua: &Lua, ids: &[i64]) -> mlua::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let callbacks: Table = lua.named_registry_value(CALLBACKS_KEY)?;
    for id in ids {
        callbacks.raw_set(*id, Value::Nil)?;
    }
    Ok(())
}

fn fail(message: String) -> mlua::Error {
    mlua::Error::external(message)
}

fn parse_patterns(event: &str, value: Value) -> mlua::Result<Vec<String>> {
    let patterns = match value {
        Value::Nil => Vec::new(),
        Value::String(s) => vec![s.to_str()?.to_owned()],
        Value::Table(list) => list
            .sequence_values::<String>()
            .collect::<mlua::Result<_>>()?,
        _ => {
            return Err(fail(
                "autocmd_create: pattern must be a string or a list of strings".to_owned(),
            ));
        }
    };
    if patterns.iter().any(|p| p == "*") {
        return Ok(Vec::new());
    }
    if !patterns.is_empty() && !has_match_key(event) {
        return Err(fail(format!(
            "autocmd_create: event '{event}' has no match key, so pattern must be '*'"
        )));
    }
    Ok(patterns)
}

fn origin(desc: Option<&str>, owner: Option<&str>) -> Arc<str> {
    let desc = desc.map(|d| format!(" \"{d}\"")).unwrap_or_default();
    let owner = owner.map(|o| format!(" from '{o}'")).unwrap_or_default();
    format!("{desc}{owner}").into()
}

/// Install the autocmd primitives on `kage.api` and return the metadata
/// handle the runtime reads subscriber counts from.
pub(crate) fn install(
    lua: &Lua,
    sink: SharedHostLog,
    current: CurrentPlugin,
) -> Result<SharedAutocmds, PluginError> {
    let shared = SharedAutocmds::default();
    lua.set_app_data(Arc::clone(&shared));
    lua.set_named_registry_value(CALLBACKS_KEY, lua.create_table()?)?;
    let api: Table = lua.globals().get::<Table>("kage")?.get("api")?;
    api.set(
        "autocmd_create",
        create_fn(lua, Arc::clone(&shared), current)?,
    )?;

    let autocmds = Arc::clone(&shared);
    api.set(
        "autocmd_del",
        lua.create_function(move |lua, id: i64| {
            lock(&autocmds).remove(id);
            drop_callbacks(lua, &[id])
        })?,
    )?;

    let autocmds = Arc::clone(&shared);
    api.set(
        "augroup_create",
        lua.create_function(move |lua, (name, opts): (String, Option<Table>)| {
            if name.is_empty() {
                return Err(fail("augroup_create: name must be non-empty".to_owned()));
            }
            let clear = match opts {
                Some(opts) => opts.get::<Option<bool>>("clear")?.unwrap_or(true),
                None => true,
            };
            let (id, removed) = {
                let mut autocmds = lock(&autocmds);
                if let Some(&id) = autocmds.groups.get(&name) {
                    let removed = if clear {
                        autocmds.remove_group(id)
                    } else {
                        Vec::new()
                    };
                    (id, removed)
                } else {
                    let id = autocmds.next_id();
                    autocmds.groups.insert(name, id);
                    (id, Vec::new())
                }
            };
            drop_callbacks(lua, &removed)?;
            Ok(id)
        })?,
    )?;

    let autocmds = Arc::clone(&shared);
    api.set(
        "augroup_del",
        lua.create_function(move |lua, group: Value| {
            let removed = {
                let mut autocmds = lock(&autocmds);
                let Some(id) = autocmds.group_id(&group) else {
                    return Ok(());
                };
                autocmds.groups.retain(|_, g| *g != id);
                autocmds.remove_group(id)
            };
            drop_callbacks(lua, &removed)
        })?,
    )?;

    api.set("autocmd_exec", exec_fn(lua, sink)?)?;

    Ok(shared)
}

fn create_fn(
    lua: &Lua,
    autocmds: SharedAutocmds,
    current: CurrentPlugin,
) -> mlua::Result<Function> {
    lua.create_function(move |lua, (event, opts): (String, Table)| {
        if !is_known(&event) {
            return Err(fail(format!("autocmd_create: unknown event '{event}'")));
        }
        let Value::Function(callback) = opts.get::<Value>("callback")? else {
            return Err(fail(
                "autocmd_create: opts.callback must be a function".to_owned(),
            ));
        };
        let patterns = parse_patterns(&event, opts.get("pattern")?)?;
        let once = opts.get::<Option<bool>>("once")?.unwrap_or(false);
        let desc = opts.get::<Option<String>>("desc")?;
        let group: Value = opts.get("group")?;
        let owner = lock(&current).clone();
        let id = {
            let mut autocmds = lock(&autocmds);
            let group = match group {
                Value::Nil => None,
                other => Some(autocmds.group_id(&other).ok_or_else(|| {
                    let name = other.to_string().unwrap_or_default();
                    fail(format!("autocmd_create: unknown group '{name}'"))
                })?),
            };
            let id = autocmds.next_id();
            autocmds.by_event.entry(event).or_default().push(Autocmd {
                id,
                group,
                patterns,
                once,
                origin: origin(desc.as_deref(), owner.as_deref()),
            });
            id
        };
        let callbacks: Table = lua.named_registry_value(CALLBACKS_KEY)?;
        callbacks.raw_set(id, callback)?;
        Ok(id)
    })
}

fn exec_fn(lua: &Lua, sink: SharedHostLog) -> mlua::Result<Function> {
    lua.create_function(move |lua, (event, opts): (String, Option<Table>)| {
        if !is_known(&event) {
            return Err(fail(format!("autocmd_exec: unknown event '{event}'")));
        }
        let (pattern, data) = match opts {
            Some(opts) => (
                opts.get::<Option<String>>("pattern")?,
                opts.get::<Value>("data")?,
            ),
            None => (None, Value::Nil),
        };
        exec(lua, &sink, &event, pattern.as_deref(), &data)
    })
}

/// Fire `event` from inside Lua with `matched` as the match and `data`
/// as the payload. Callback errors are logged. Nested calls, through
/// `autocmd_exec` or an option set in a callback, are capped at
/// [`MAX_EXEC_DEPTH`] levels.
pub(crate) fn exec(
    lua: &Lua,
    sink: &SharedHostLog,
    event: &str,
    matched: Option<&str>,
    data: &Value,
) -> mlua::Result<()> {
    let Some(autocmds) = lua.app_data_ref::<SharedAutocmds>().map(|a| Arc::clone(&a)) else {
        return Ok(());
    };
    {
        let mut autocmds = lock(&autocmds);
        if autocmds.exec_depth >= MAX_EXEC_DEPTH {
            return Err(fail(format!(
                "autocmd_exec: nested deeper than {MAX_EXEC_DEPTH}"
            )));
        }
        autocmds.exec_depth += 1;
    }
    let result = targets(lua, event, matched).map(|targets| {
        notify(lua, sink, event, matched, &targets, data);
    });
    lock(&autocmds).exec_depth -= 1;
    result
}

#[cfg(test)]
mod tests;
