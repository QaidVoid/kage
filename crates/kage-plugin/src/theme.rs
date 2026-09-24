//! `kage.theme.list` / `kage.theme.set` / `kage.theme.current`:
//! plugin-facing theme manager over the `theme` option.
//!
//! `current` reads the option store, `list` asks the host's
//! [`crate::ThemeResolver`], and `set` sets the option from Lua, which
//! switches the base highlight groups before it returns and fires
//! `color_scheme` and `option_set`. Lets a plugin auto-toggle
//! light/dark on a system event without owning the theme registry.

use kage_core::options::{OptionSource, OptionValue};
use mlua::{Lua, Table, Value};

use crate::error::PluginError;
use crate::options::Options;

/// Install `kage.theme.{list,set,current}` on the running Lua state.
pub(crate) fn install_theme(lua: &Lua, options: &Options) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    let theme = lua.create_table()?;

    let current = options.clone();
    theme.set(
        "current",
        lua.create_function(move |_, ()| Ok(current.theme()))?,
    )?;

    let list = options.clone();
    theme.set(
        "list",
        lua.create_function(move |lua, ()| {
            let names = list.themes.as_ref().map(|t| t.names()).unwrap_or_default();
            lua.create_sequence_from(names)
        })?,
    )?;

    let set = options.clone();
    theme.set(
        "set",
        lua.create_function(move |lua, name: Value| {
            let Value::String(name) = name else {
                return Err(mlua::Error::external(
                    "kage.theme.set: name must be a string",
                ));
            };
            let name = name.to_str()?.to_owned();
            if name.is_empty() {
                return Err(mlua::Error::external(
                    "kage.theme.set: name must be non-empty",
                ));
            }
            set.set(lua, "theme", OptionValue::Str(name), OptionSource::Lua)
        })?,
    )?;

    kage.set("theme", theme)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kage_core::options::OptionValue;
    use kage_core::sync::lock;

    use crate::PluginRuntime;
    use crate::highlight::FakeThemes;

    fn runtime() -> PluginRuntime {
        PluginRuntime::builder()
            .themes(Arc::new(FakeThemes))
            .build()
            .unwrap()
    }

    fn eval_bool(rt: &PluginRuntime, source: &str) -> bool {
        rt.eval(source).unwrap().as_boolean().unwrap_or(false)
    }

    #[test]
    fn current_reads_the_option() {
        let rt = runtime();
        assert!(eval_bool(&rt, "return kage.theme.current() == 'default'"));
        rt.eval("kage.opt.theme = 'tokyo-night'").unwrap();
        assert!(eval_bool(
            &rt,
            "return kage.theme.current() == 'tokyo-night'"
        ));
    }

    #[test]
    fn list_returns_the_resolver_names() {
        let rt = runtime();
        assert!(eval_bool(
            &rt,
            "local t = kage.theme.list(); return #t == 2 and t[1] == 'default' and t[2] == 'tokyo-night'"
        ));
        let bare = PluginRuntime::new().unwrap();
        assert!(eval_bool(&bare, "return #kage.theme.list() == 0"));
    }

    #[test]
    fn set_applies_before_it_returns() {
        let rt = runtime();
        let fg: String = rt
            .eval(
                "kage.theme.set('tokyo-night')
                 return kage.api.hl_get('KageMuted').fg",
            )
            .unwrap()
            .as_string()
            .map(mlua::String::to_string_lossy)
            .unwrap_or_default();
        assert_eq!(fg, "#000002");
        assert_eq!(
            lock(&rt.options()).get("theme"),
            Some(&OptionValue::Str("tokyo-night".into()))
        );
        assert_eq!(lock(&rt.highlights()).theme(), "tokyo-night");
    }

    #[test]
    fn set_rejects_non_string_empty_and_unknown() {
        let rt = runtime();
        assert!(rt.eval("kage.theme.set(42)").is_err());
        assert!(rt.eval("kage.theme.set('')").is_err());
        assert!(rt.eval("kage.theme.set('nope')").is_err());
        assert!(lock(&rt.options()).take_changes().is_empty());
    }
}
