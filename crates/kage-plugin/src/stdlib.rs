//! The embedded Lua stdlib and `_defaults.lua`.
//!
//! The stdlib modules under `crates/kage-plugin/lua/kage/` build the
//! friendly `kage.*` surface (today the `kage.on` alias) on top of the
//! Rust primitives in `kage.api`. They run once, in the global
//! environment, before the shared tables are frozen. Each module chunk
//! receives one private table of host internals as its argument, which
//! plugins never see: `events` is the set of known event names, and
//! `kage/init.lua` adds shared helpers for the modules after it.
//!
//! `_defaults.lua` holds kage's own defaults. The loader evaluates it in
//! its own environment before any plugin, so every later layer can
//! override what it sets.

use mlua::Lua;

use crate::error::PluginError;
use crate::events::KNOWN_EVENTS;

/// Stdlib modules as `(chunk name, source)`, in evaluation order.
const MODULES: &[(&str, &str)] = &[
    ("=kage/init.lua", include_str!("../lua/kage/init.lua")),
    ("=kage/on.lua", include_str!("../lua/kage/on.lua")),
];

/// Source of the embedded `_defaults.lua`.
pub(crate) const DEFAULTS: &str = include_str!("../lua/_defaults.lua");

/// Environment name `_defaults.lua` evaluates under. The `@` prefix
/// cannot collide with a plugin file stem.
pub(crate) const DEFAULTS_ENV: &str = "@defaults";

/// Evaluate every stdlib module against the global environment.
pub(crate) fn install(lua: &Lua) -> Result<(), PluginError> {
    let internal = lua.create_table()?;
    let events = lua.create_table()?;
    for (name, _, _) in KNOWN_EVENTS {
        events.raw_set(*name, true)?;
    }
    internal.raw_set("events", events)?;
    for (name, source) in MODULES {
        lua.load(*source).set_name(*name).call::<()>(&internal)?;
    }
    Ok(())
}
