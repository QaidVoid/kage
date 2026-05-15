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
//! The host reads each parked request's payload through a typed
//! parser: [`SelectRequest::from_payload`] normalises `ui.select`
//! `items` (bare strings or `{label, value?, detail?}` tables) for an
//! `OverlayPicker`; [`ConfirmRequest::from_payload`] reads the
//! `ui.confirm` title/message for a yes/no overlay;
//! [`InputRequest::from_payload`] reads the `ui.input` title and
//! optional placeholder for a single-line input overlay;
//! [`EditorRequest::from_payload`] reads the `ui.editor` title and
//! optional prefill for a multi-line editor overlay.

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
     end\n\
     kage.ui.confirm = function(title, message)\n  \
       if type(title) ~= 'string' then\n    \
         error('kage.ui.confirm: title must be a string', 2)\n  \
       end\n  \
       if type(message) ~= 'string' then\n    \
         error('kage.ui.confirm: message must be a string', 2)\n  \
       end\n  \
       return kage._suspend('ui.confirm', { title = title, message = message })\n\
     end\n\
     kage.ui.input = function(title, placeholder)\n  \
       if type(title) ~= 'string' then\n    \
         error('kage.ui.input: title must be a string', 2)\n  \
       end\n  \
       if placeholder ~= nil and type(placeholder) ~= 'string' then\n    \
         error('kage.ui.input: placeholder must be a string or nil', 2)\n  \
       end\n  \
       return kage._suspend('ui.input', { title = title, placeholder = placeholder })\n\
     end\n\
     kage.ui.editor = function(title, prefill)\n  \
       if type(title) ~= 'string' then\n    \
         error('kage.ui.editor: title must be a string', 2)\n  \
       end\n  \
       if prefill ~= nil and type(prefill) ~= 'string' then\n    \
         error('kage.ui.editor: prefill must be a string or nil', 2)\n  \
       end\n  \
       return kage._suspend('ui.editor', { title = title, prefill = prefill })\n\
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

/// A parsed `ui.confirm` request: the title and body the host shows
/// in a yes/no overlay. The coroutine is resumed with a JSON boolean.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmRequest {
    /// Overlay title.
    pub title: String,
    /// Body text explaining what is being confirmed.
    pub message: String,
}

impl ConfirmRequest {
    /// Parse the payload of a `kind == "ui.confirm"` suspend request.
    /// Both `title` and `message` are required strings; anything else
    /// is a plugin contract violation and surfaces as
    /// [`PluginError::BridgeProtocol`].
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, PluginError> {
        let obj = payload.as_object().ok_or_else(|| {
            PluginError::BridgeProtocol("ui.confirm payload is not an object".to_owned())
        })?;
        let field = |key: &str| -> Result<String, PluginError> {
            obj.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    PluginError::BridgeProtocol(format!("ui.confirm payload has no string `{key}`"))
                })
        };
        Ok(Self {
            title: field("title")?,
            message: field("message")?,
        })
    }
}

/// A parsed `ui.input` request: the prompt title and an optional
/// dimmed placeholder. The coroutine is resumed with the entered
/// string, or `nil` if the user cancelled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputRequest {
    /// Prompt title.
    pub title: String,
    /// Placeholder shown while the field is empty. Not part of the
    /// resolved value.
    pub placeholder: Option<String>,
}

impl InputRequest {
    /// Parse the payload of a `kind == "ui.input"` suspend request.
    /// `title` is a required string; `placeholder` is optional (absent
    /// or JSON null means none) but must be a string when present.
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, PluginError> {
        let obj = payload.as_object().ok_or_else(|| {
            PluginError::BridgeProtocol("ui.input payload is not an object".to_owned())
        })?;
        let title = obj
            .get("title")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                PluginError::BridgeProtocol("ui.input payload has no string `title`".to_owned())
            })?
            .to_owned();
        let placeholder = match obj.get("placeholder") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(PluginError::BridgeProtocol(
                    "ui.input `placeholder` must be a string".to_owned(),
                ));
            }
        };
        Ok(Self { title, placeholder })
    }
}

/// A parsed `ui.editor` request: the editor title and an optional
/// prefilled body. The coroutine is resumed with the final text, or
/// `nil` if the user cancelled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorRequest {
    /// Editor title.
    pub title: String,
    /// Initial buffer contents. Empty when the plugin passed none.
    pub prefill: Option<String>,
}

impl EditorRequest {
    /// Parse the payload of a `kind == "ui.editor"` suspend request.
    /// `title` is a required string; `prefill` is optional (absent or
    /// JSON null means none) but must be a string when present.
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, PluginError> {
        let obj = payload.as_object().ok_or_else(|| {
            PluginError::BridgeProtocol("ui.editor payload is not an object".to_owned())
        })?;
        let title = obj
            .get("title")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                PluginError::BridgeProtocol("ui.editor payload has no string `title`".to_owned())
            })?
            .to_owned();
        let prefill = match obj.get("prefill") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(PluginError::BridgeProtocol(
                    "ui.editor `prefill` must be a string".to_owned(),
                ));
            }
        };
        Ok(Self { title, prefill })
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

    #[test]
    fn confirm_suspends_with_title_and_message_payload() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return kage.ui.confirm('Delete?', 'are you sure') end",
        );
        let step = rt.bridge_call(&f, &[]).unwrap();
        assert_eq!(
            step,
            BridgeStep::Suspended(SuspendRequest {
                kind: "ui.confirm".to_owned(),
                payload: json!({ "title": "Delete?", "message": "are you sure" }),
            })
        );
    }

    #[test]
    fn confirm_returns_resumed_boolean() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function()\n  \
               if kage.ui.confirm('t', 'm') then return 'yes' end\n  \
               return 'no'\n\
             end",
        );
        assert!(matches!(
            rt.bridge_call(&f, &[]).unwrap(),
            BridgeStep::Suspended(_)
        ));
        assert_eq!(
            rt.bridge_resume(&json!(true)).unwrap(),
            BridgeStep::Done(json!("yes"))
        );
    }

    #[test]
    fn confirm_rejects_non_string_title() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(&rt, "return function() return kage.ui.confirm(1, 'm') end");
        let err = rt.bridge_call(&f, &[]).unwrap_err();
        assert!(matches!(err, PluginError::Lua(_)));
        assert!(err.to_string().contains("title"));
    }

    #[test]
    fn confirm_rejects_non_string_message() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(&rt, "return function() return kage.ui.confirm('t', {}) end");
        let err = rt.bridge_call(&f, &[]).unwrap_err();
        assert!(matches!(err, PluginError::Lua(_)));
        assert!(err.to_string().contains("message"));
    }

    #[test]
    fn confirm_from_payload_parses_fields() {
        let req = ConfirmRequest::from_payload(&json!({ "title": "T", "message": "M" })).unwrap();
        assert_eq!(req.title, "T");
        assert_eq!(req.message, "M");
    }

    #[test]
    fn confirm_from_payload_rejects_missing_message() {
        let err = ConfirmRequest::from_payload(&json!({ "title": "T" })).unwrap_err();
        assert!(matches!(err, PluginError::BridgeProtocol(_)));
        assert!(err.to_string().contains("message"));
    }

    #[test]
    fn input_suspends_with_title_and_optional_placeholder() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return kage.ui.input('Your name', 'e.g. Ada') end",
        );
        assert_eq!(
            rt.bridge_call(&f, &[]).unwrap(),
            BridgeStep::Suspended(SuspendRequest {
                kind: "ui.input".to_owned(),
                payload: json!({ "title": "Your name", "placeholder": "e.g. Ada" }),
            })
        );
    }

    #[test]
    fn input_without_placeholder_omits_it() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(&rt, "return function() return kage.ui.input('Name') end");
        assert_eq!(
            rt.bridge_call(&f, &[]).unwrap(),
            BridgeStep::Suspended(SuspendRequest {
                kind: "ui.input".to_owned(),
                payload: json!({ "title": "Name" }),
            })
        );
    }

    #[test]
    fn input_returns_resumed_string() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return (kage.ui.input('q') or 'nil') end",
        );
        assert!(matches!(
            rt.bridge_call(&f, &[]).unwrap(),
            BridgeStep::Suspended(_)
        ));
        assert_eq!(
            rt.bridge_resume(&json!("Ada")).unwrap(),
            BridgeStep::Done(json!("Ada"))
        );
    }

    #[test]
    fn input_cancel_returns_nil() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return (kage.ui.input('q') or 'cancelled') end",
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
    fn input_rejects_non_string_placeholder() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(&rt, "return function() return kage.ui.input('t', 5) end");
        let err = rt.bridge_call(&f, &[]).unwrap_err();
        assert!(matches!(err, PluginError::Lua(_)));
        assert!(err.to_string().contains("placeholder"));
    }

    #[test]
    fn input_from_payload_parses_optional_placeholder() {
        let with =
            InputRequest::from_payload(&json!({ "title": "T", "placeholder": "p" })).unwrap();
        assert_eq!(with.placeholder.as_deref(), Some("p"));
        let without = InputRequest::from_payload(&json!({ "title": "T" })).unwrap();
        assert_eq!(without.placeholder, None);
    }

    #[test]
    fn input_from_payload_rejects_non_string_placeholder() {
        let err =
            InputRequest::from_payload(&json!({ "title": "T", "placeholder": 1 })).unwrap_err();
        assert!(matches!(err, PluginError::BridgeProtocol(_)));
        assert!(err.to_string().contains("placeholder"));
    }

    #[test]
    fn editor_suspends_with_title_and_optional_prefill() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return kage.ui.editor('Note', 'draft body') end",
        );
        assert_eq!(
            rt.bridge_call(&f, &[]).unwrap(),
            BridgeStep::Suspended(SuspendRequest {
                kind: "ui.editor".to_owned(),
                payload: json!({ "title": "Note", "prefill": "draft body" }),
            })
        );
    }

    #[test]
    fn editor_without_prefill_omits_it() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(&rt, "return function() return kage.ui.editor('Note') end");
        assert_eq!(
            rt.bridge_call(&f, &[]).unwrap(),
            BridgeStep::Suspended(SuspendRequest {
                kind: "ui.editor".to_owned(),
                payload: json!({ "title": "Note" }),
            })
        );
    }

    #[test]
    fn editor_returns_resumed_string() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return (kage.ui.editor('e') or 'nil') end",
        );
        assert!(matches!(
            rt.bridge_call(&f, &[]).unwrap(),
            BridgeStep::Suspended(_)
        ));
        assert_eq!(
            rt.bridge_resume(&json!("final text")).unwrap(),
            BridgeStep::Done(json!("final text"))
        );
    }

    #[test]
    fn editor_cancel_returns_nil() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(
            &rt,
            "return function() return (kage.ui.editor('e') or 'cancelled') end",
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
    fn editor_rejects_non_string_prefill() {
        let rt = PluginRuntime::new().unwrap();
        let f = func(&rt, "return function() return kage.ui.editor('t', {}) end");
        let err = rt.bridge_call(&f, &[]).unwrap_err();
        assert!(matches!(err, PluginError::Lua(_)));
        assert!(err.to_string().contains("prefill"));
    }

    #[test]
    fn editor_from_payload_parses_optional_prefill() {
        let with = EditorRequest::from_payload(&json!({ "title": "T", "prefill": "x" })).unwrap();
        assert_eq!(with.prefill.as_deref(), Some("x"));
        let without = EditorRequest::from_payload(&json!({ "title": "T" })).unwrap();
        assert_eq!(without.prefill, None);
    }

    #[test]
    fn editor_from_payload_rejects_non_string_prefill() {
        let err = EditorRequest::from_payload(&json!({ "title": "T", "prefill": 7 })).unwrap_err();
        assert!(matches!(err, PluginError::BridgeProtocol(_)));
        assert!(err.to_string().contains("prefill"));
    }
}
