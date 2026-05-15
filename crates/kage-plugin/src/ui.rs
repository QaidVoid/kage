//! `kage.ui.*` blocking dialog APIs, built on the coroutine bridge.
//!
//! These look synchronous to plugin code but suspend the running
//! coroutine instead of blocking the host. `kage.ui.select(title,
//! items)` opens a picker, parks the coroutine via `kage._suspend`
//! (see [`crate::bridge`]), and returns whatever the host resumes it
//! with: the chosen item's value, or `nil` if the user cancelled.
//!
//! The Lua wrappers are defined as Lua functions (not Rust closures)
//! because the suspension travels through `coroutine.yield`, which
//! cannot cross a Rust call frame in non-async mlua. They do only
//! shape validation, then hand off to `kage._suspend`.
//!
//! The host reads the parked request's payload through
//! [`SelectRequest::from_payload`], which normalises the plugin's
//! `items` (bare strings or `{label, value?, detail?}` tables) into a
//! uniform list it can render in an `OverlayPicker`.

use mlua::Lua;

use crate::error::PluginError;

/// Lua source for the `kage.ui.*` dialog wrappers. The `kage.ui`
/// table is created on demand so this is order-independent with other
/// installers that may also populate `kage.ui`.
const UI_LUA: &str = "kage.ui = kage.ui or {}\n\
     kage.ui.select = function(title, items)\n  \
       if type(title) ~= 'string' then\n    \
         error('kage.ui.select: title must be a string', 2)\n  \
       end\n  \
       if type(items) ~= 'table' then\n    \
         error('kage.ui.select: items must be a table', 2)\n  \
       end\n  \
       return kage._suspend('ui.select', { title = title, items = items })\n\
     end\n";

/// One row the host should render in the picker for a `ui.select`.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectItem {
    /// Text shown in the picker list.
    pub label: String,
    /// Value the coroutine is resumed with when this row is chosen.
    /// Defaults to the label when the plugin did not set one.
    pub value: serde_json::Value,
    /// Optional dimmed secondary text.
    pub detail: Option<String>,
}

/// A parsed `ui.select` request: everything the host needs to build
/// and run the picker.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectRequest {
    /// Picker title.
    pub title: String,
    /// Rows to choose from, in plugin-supplied order.
    pub items: Vec<SelectItem>,
}

impl SelectRequest {
    /// Parse the payload of a `kind == "ui.select"` suspend request.
    ///
    /// Accepts each item as either a JSON string (label and value
    /// both that string) or an object `{ label, value?, detail? }`
    /// (`value` defaults to `label`). A malformed payload is a plugin
    /// contract violation and surfaces as
    /// [`PluginError::BridgeProtocol`] so the host can fail the call
    /// instead of opening a broken picker.
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, PluginError> {
        let obj = payload.as_object().ok_or_else(|| {
            PluginError::BridgeProtocol("ui.select payload is not an object".to_owned())
        })?;
        let title = obj
            .get("title")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                PluginError::BridgeProtocol("ui.select payload has no string `title`".to_owned())
            })?
            .to_owned();
        let raw_items = obj.get("items").and_then(serde_json::Value::as_array);
        let Some(raw_items) = raw_items else {
            return Err(PluginError::BridgeProtocol(
                "ui.select payload has no `items` array".to_owned(),
            ));
        };
        let mut items = Vec::with_capacity(raw_items.len());
        for (idx, raw) in raw_items.iter().enumerate() {
            items.push(parse_item(idx, raw)?);
        }
        Ok(Self { title, items })
    }
}

/// Normalise one `items` entry into a [`SelectItem`].
fn parse_item(idx: usize, raw: &serde_json::Value) -> Result<SelectItem, PluginError> {
    if let Some(label) = raw.as_str() {
        return Ok(SelectItem {
            label: label.to_owned(),
            value: serde_json::Value::String(label.to_owned()),
            detail: None,
        });
    }
    let obj = raw.as_object().ok_or_else(|| {
        PluginError::BridgeProtocol(format!(
            "ui.select item {idx} is neither a string nor an object"
        ))
    })?;
    let label = obj
        .get("label")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            PluginError::BridgeProtocol(format!("ui.select item {idx} has no string `label`"))
        })?
        .to_owned();
    let value = obj
        .get("value")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::String(label.clone()));
    let detail = obj
        .get("detail")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok(SelectItem {
        label,
        value,
        detail,
    })
}

/// Install the `kage.ui.*` wrappers. Call after
/// [`crate::bridge::install_suspend`]; the wrappers reference
/// `kage._suspend` at call time, so install order does not matter
/// beyond the `kage` table existing.
pub fn install_ui(lua: &Lua) -> Result<(), PluginError> {
    lua.load(UI_LUA).set_name("kage.ui").exec()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::PluginRuntime;
    use crate::bridge::{BridgeStep, SuspendRequest};

    fn func(rt: &PluginRuntime, src: &str) -> mlua::Function {
        let lua = rt.lock_lua();
        lua.load(src).eval::<mlua::Function>().unwrap()
    }

    #[test]
    fn select_suspends_with_title_and_items_payload() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return kage.ui.select('Pick one', {'a','b'}) end",
        );
        let step = rt.bridge_call(&f, &[]).unwrap();
        assert_eq!(
            step,
            BridgeStep::Suspended(SuspendRequest {
                kind: "ui.select".to_owned(),
                payload: json!({ "title": "Pick one", "items": ["a", "b"] }),
            })
        );
    }

    #[test]
    fn select_returns_the_resumed_value() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return kage.ui.select('t', {'a','b'}) end",
        );
        assert!(matches!(
            rt.bridge_call(&f, &[]).unwrap(),
            BridgeStep::Suspended(_)
        ));
        let step = rt.bridge_resume(&json!("b")).unwrap();
        assert_eq!(step, BridgeStep::Done(json!("b")));
    }

    #[test]
    fn select_returns_nil_on_cancel() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function()\n  \
               local picked = kage.ui.select('t', {'a'})\n  \
               if picked == nil then return 'cancelled' end\n  \
               return picked\n\
             end",
        );
        assert!(matches!(
            rt.bridge_call(&f, &[]).unwrap(),
            BridgeStep::Suspended(_)
        ));
        assert_eq!(
            rt.bridge_cancel().unwrap(),
            BridgeStep::Done(json!("cancelled"))
        );
    }

    #[test]
    fn select_rejects_non_string_title() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return kage.ui.select(42, {'a'}) end",
        );
        let err = rt.bridge_call(&f, &[]).unwrap_err();
        assert!(matches!(err, PluginError::Lua(_)));
        assert!(err.to_string().contains("title"));
        assert!(!rt.bridge_is_suspended());
    }

    #[test]
    fn select_rejects_non_table_items() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return kage.ui.select('t', 'nope') end",
        );
        let err = rt.bridge_call(&f, &[]).unwrap_err();
        assert!(matches!(err, PluginError::Lua(_)));
        assert!(err.to_string().contains("items"));
    }

    #[test]
    fn from_payload_parses_string_items() {
        let req = SelectRequest::from_payload(&json!({
            "title": "Branch",
            "items": ["main", "dev"],
        }))
        .unwrap();
        assert_eq!(req.title, "Branch");
        assert_eq!(
            req.items,
            vec![
                SelectItem {
                    label: "main".to_owned(),
                    value: json!("main"),
                    detail: None,
                },
                SelectItem {
                    label: "dev".to_owned(),
                    value: json!("dev"),
                    detail: None,
                },
            ]
        );
    }

    #[test]
    fn from_payload_parses_object_items_and_defaults_value_to_label() {
        let req = SelectRequest::from_payload(&json!({
            "title": "T",
            "items": [
                { "label": "First", "value": 1, "detail": "the one" },
                { "label": "Second" },
            ],
        }))
        .unwrap();
        assert_eq!(
            req.items,
            vec![
                SelectItem {
                    label: "First".to_owned(),
                    value: json!(1),
                    detail: Some("the one".to_owned()),
                },
                SelectItem {
                    label: "Second".to_owned(),
                    value: json!("Second"),
                    detail: None,
                },
            ]
        );
    }

    #[test]
    fn from_payload_rejects_missing_title() {
        let err = SelectRequest::from_payload(&json!({ "items": ["a"] })).unwrap_err();
        assert!(matches!(err, PluginError::BridgeProtocol(_)));
        assert!(err.to_string().contains("title"));
    }

    #[test]
    fn from_payload_rejects_item_without_label() {
        let err = SelectRequest::from_payload(&json!({
            "title": "T",
            "items": [{ "value": "x" }],
        }))
        .unwrap_err();
        assert!(matches!(err, PluginError::BridgeProtocol(_)));
        assert!(err.to_string().contains("label"));
    }

    #[test]
    fn from_payload_allows_empty_items() {
        let req = SelectRequest::from_payload(&json!({ "title": "T", "items": [] })).unwrap();
        assert!(req.items.is_empty());
    }
}
