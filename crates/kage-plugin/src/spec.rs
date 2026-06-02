//! Single source of truth for the `kage` Lua plugin surface.
//!
//! Every alias, record class, sub-table, and function a plugin can see
//! is described here, in Rust, next to the crate that implements it.
//! `cargo xtask gen-lua-types` renders [`surface`] into the
//! `lua-language-server` stub `plugins/types/kage.lua`; the CI drift
//! gate re-renders and diffs, so the shipped stub cannot drift from
//! this description, and this description cannot drift from the crate
//! that owns it (the [`tests`] module asserts every declared function
//! path resolves in a freshly built [`crate::PluginRuntime`]).
//!
//! This replaces the previously hand-maintained spec that lived in
//! `xtask/src/luatypes.rs`: that copy could (and did) fall out of step
//! with the Rust bindings. Keeping the description in this crate makes
//! adding a binding a single edit instead of two.

/// One `---@param` (on a function) or `---@field` (on a class).
#[derive(Clone, Copy, Debug)]
pub struct Field {
    /// Field name. A trailing `?` marks it optional; the renderer
    /// strips the `?` from the displayed name and emits the emmylua
    /// optional marker instead.
    pub name: &'static str,
    /// emmylua type expression (e.g. `string`, `kage.ToolSpec`,
    /// `fun(x: integer): string`).
    pub ty: &'static str,
    /// One-line trailing doc. Empty renders no trailing text.
    pub doc: &'static str,
}

/// A `---@class` record type.
#[derive(Clone, Copy, Debug)]
pub struct Class {
    /// Fully qualified class name (e.g. `kage.ToolSpec`).
    pub name: &'static str,
    /// Doc lines emitted above the class. An empty entry renders a
    /// bare `---` separator line.
    pub doc: &'static [&'static str],
    /// Record fields, in declaration order.
    pub fields: &'static [Field],
}

/// A `---@alias` sum type. Variants render inline when there are five
/// or fewer, otherwise as the multi-line `---| "x"` form.
#[derive(Clone, Copy, Debug)]
pub struct Alias {
    /// Fully qualified alias name (e.g. `kage.Event`).
    pub name: &'static str,
    /// Doc lines emitted above the alias.
    pub doc: &'static [&'static str],
    /// String-literal variants, in order.
    pub variants: &'static [&'static str],
}

/// A single function binding.
#[derive(Clone, Copy, Debug)]
pub struct Func {
    /// Doc lines emitted above the function.
    pub doc: &'static [&'static str],
    /// Dotted path, e.g. `kage.ui.select`.
    pub path: &'static str,
    /// Parameters, in order.
    pub params: &'static [Field],
    /// emmylua return type, or `None` for a function that returns
    /// nothing.
    pub ret: Option<&'static str>,
}

/// A sub-table to declare (`kage.ui = {}`) before its first function.
#[derive(Clone, Copy, Debug)]
pub struct Table {
    /// Dotted path, e.g. `kage.ui`.
    pub path: &'static str,
    /// One-line doc emitted above the table declaration.
    pub class_doc: &'static str,
}

/// A function that is only present when the plugin has been granted
/// `cap` (see [`crate::capabilities`]). It is rendered into the stub
/// like any function but is not on the base surface, so it resolves
/// only on a granted plugin's `kage` proxy, never the default one.
#[derive(Clone, Copy, Debug)]
pub struct GatedFunc {
    /// Wire name of the capability that unlocks this function.
    pub cap: &'static str,
    /// The function binding itself.
    pub func: Func,
}

/// The complete declarative description of the `kage` Lua surface.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    /// `---@alias` sum types.
    pub aliases: &'static [Alias],
    /// `---@class` record types.
    pub classes: &'static [Class],
    /// Sub-tables declared before their first function.
    pub tables: &'static [Table],
    /// Base function bindings, present for every plugin.
    pub funcs: &'static [Func],
    /// Capability-gated functions, present only on a plugin granted
    /// the named capability.
    pub gated: &'static [GatedFunc],
}

/// The single source of truth: the full `kage` plugin surface.
#[must_use]
pub fn surface() -> Surface {
    Surface {
        aliases: aliases::ALIASES,
        classes: classes::CLASSES,
        tables: tables::TABLES,
        funcs: funcs::FUNCS,
        gated: gated::GATED,
    }
}

mod aliases;
mod classes;
mod funcs;
mod gated;
mod tables;

#[cfg(test)]
mod tests;
