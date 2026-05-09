//! `kage.register_provider` and the `Provider` adapter that backs into Lua.
//!
//! Plugins declare a custom provider:
//! ```lua
//! kage.register_provider({
//!     id = "echo",
//!     stream = function(req)
//!         local i = 0
//!         return function()
//!             i = i + 1
//!             if i == 1 then return { type = "message_start" } end
//!             if i == 2 then return { type = "text_delta", delta = "hi" } end
//!             if i == 3 then return { type = "message_end", stop_reason = "end_turn" } end
//!             return nil
//!         end
//!     end,
//! })
//! ```
//! The host registers each [`LuaProvider`] with its `ProviderRegistry` so
//! the agent loop can route `provider:model` strings into Lua.

use std::sync::{Arc, Mutex};

use kage_core::CancelFlag;
use kage_provider::{
    EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, StreamRequest,
};
use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua, lua_to_json};
use crate::error::PluginError;
use crate::runtime::SharedLua;

/// `Provider` whose `stream` runs inside the plugin runtime's Lua state.
pub struct LuaProvider {
    metadata: ProviderMetadata,
    lua: SharedLua,
    sink: SharedHostLog,
    handler_key: Arc<RegistryKey>,
}

impl std::fmt::Debug for LuaProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaProvider")
            .field("id", &self.metadata.id)
            .finish_non_exhaustive()
    }
}

impl Provider for LuaProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn stream(
        &self,
        req: StreamRequest,
        _cancel: &CancelFlag,
    ) -> Result<EventStream, ProviderError> {
        let req_value = serde_json::to_value(&req)
            .map_err(|e| ProviderError::Decode(format!("plugin provider: encode request: {e}")))?;
        let events = collect_events(&self.lua, &self.handler_key, &self.sink, &req_value)
            .map_err(|e| ProviderError::Decode(format!("plugin provider: {e}")))?;
        Ok(Box::new(events.into_iter()))
    }
}

fn collect_events(
    lua: &SharedLua,
    handler_key: &Arc<RegistryKey>,
    sink: &SharedHostLog,
    req: &serde_json::Value,
) -> Result<Vec<Result<ProviderEvent, ProviderError>>, PluginError> {
    let lua = lua.lock().expect("plugin lua mutex poisoned");
    let handler: Function = lua.registry_value(handler_key)?;
    let lua_req = json_to_lua(&lua, req)?;

    let result: Value = handler.call(lua_req)?;
    let mut events = Vec::new();
    match result {
        Value::Table(t) => {
            for pair in t.clone().sequence_values::<Value>() {
                let v = pair?;
                events.push(value_to_provider_event(v, sink));
            }
        }
        Value::Function(f) => loop {
            let next: Value = match f.call::<Value>(()) {
                Ok(v) => v,
                Err(err) => {
                    events.push(Err(ProviderError::Decode(format!(
                        "plugin provider iterator raised: {err}"
                    ))));
                    break;
                }
            };
            if matches!(next, Value::Nil) {
                break;
            }
            events.push(value_to_provider_event(next, sink));
        },
        _ => {
            events.push(Err(ProviderError::Decode(
                "plugin provider's stream() returned neither a table nor a function".to_owned(),
            )));
        }
    }
    Ok(events)
}

fn value_to_provider_event(
    value: Value,
    sink: &SharedHostLog,
) -> Result<ProviderEvent, ProviderError> {
    let json = lua_to_json(value)
        .map_err(|e| ProviderError::Decode(format!("plugin provider: lua to json: {e}")))?;
    serde_json::from_value::<ProviderEvent>(json).map_err(|err| {
        if let Ok(mut s) = sink.lock() {
            s.log(
                LogLevel::Error,
                &format!("plugin provider yielded undecodable event: {err}"),
            );
        }
        ProviderError::Decode(format!("plugin provider: decode event: {err}"))
    })
}

/// Shared registry of providers contributed by Lua plugins.
pub type RegisteredProviders = Arc<Mutex<Vec<Arc<LuaProvider>>>>;

/// Construct an empty provider registry.
#[must_use]
pub fn registered_providers() -> RegisteredProviders {
    Arc::new(Mutex::new(Vec::new()))
}

/// Install `kage.register_provider` on the running Lua state.
pub fn install_register_provider(
    lua: &Lua,
    shared_lua: SharedLua,
    sink: SharedHostLog,
    registered: RegisteredProviders,
) -> Result<(), PluginError> {
    let kage: Table = lua.globals().get("kage")?;
    kage.set(
        "register_provider",
        lua.create_function(move |lua, spec: Table| {
            let id: String = spec.get("id")?;
            let display_name: Option<String> = spec.get("display_name").ok();
            let supports_caching: bool = spec.get("supports_caching").unwrap_or(false);
            let supports_thinking: bool = spec.get("supports_thinking").unwrap_or(false);
            let supports_tool_use: bool = spec.get("supports_tool_use").unwrap_or(true);
            let stream: Function = spec.get("stream")?;
            let key = lua.create_registry_value(stream)?;
            let metadata = ProviderMetadata {
                id: id.clone(),
                display_name: display_name.unwrap_or_else(|| id.clone()),
                supports_caching,
                supports_thinking,
                supports_tool_use,
            };
            let provider = LuaProvider {
                metadata,
                lua: shared_lua.clone(),
                sink: sink.clone(),
                handler_key: Arc::new(key),
            };
            registered
                .lock()
                .map_err(|_| mlua::Error::external("plugin providers registry poisoned"))?
                .push(Arc::new(provider));
            Ok(())
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use kage_core::{CancelFlag, Message, Role};
    use kage_provider::{Provider, StopReason};

    use crate::PluginRuntime;

    #[test]
    fn lua_provider_streams_table_of_events() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_provider({
                id = 'fake',
                display_name = 'Fake',
                stream = function(req)
                    return {
                        { type = 'message_start' },
                        { type = 'text_delta', delta = 'hi ' },
                        { type = 'text_delta', delta = req.model },
                        { type = 'message_end', stop_reason = 'end_turn',
                          usage = { input = 1, output = 2, cache_read = 0, cache_write = 0 } },
                    }
                end,
            })
            ",
        )
        .unwrap();
        let providers = rt.registered_providers();
        assert_eq!(providers.len(), 1);
        let provider = &providers[0];
        assert_eq!(provider.metadata().id, "fake");

        let req = kage_provider::StreamRequest::new(
            "model-x",
            vec![Message::new(
                Role::User,
                vec![kage_core::Content::Text { text: "hi".into() }],
                None,
            )],
        );
        let cancel = CancelFlag::new();
        let stream = provider.stream(req, &cancel).unwrap();
        let events: Vec<_> = stream.collect::<Result<_, _>>().unwrap();
        assert_eq!(events.len(), 4);
        assert!(matches!(
            events[0],
            kage_provider::ProviderEvent::MessageStart
        ));
        assert!(matches!(
            &events[1],
            kage_provider::ProviderEvent::TextDelta { delta } if delta == "hi "
        ));
        assert!(matches!(
            &events[2],
            kage_provider::ProviderEvent::TextDelta { delta } if delta == "model-x"
        ));
        assert!(matches!(
            events[3],
            kage_provider::ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));
    }

    #[test]
    fn lua_provider_streams_iterator_function() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_provider({
                id = 'iter',
                stream = function(req)
                    local i = 0
                    return function()
                        i = i + 1
                        if i == 1 then return { type = 'message_start' } end
                        if i == 2 then return { type = 'text_delta', delta = 'ok' } end
                        if i == 3 then return { type = 'message_end', stop_reason = 'end_turn',
                            usage = { input = 0, output = 0, cache_read = 0, cache_write = 0 } } end
                        return nil
                    end
                end,
            })
            ",
        )
        .unwrap();
        let provider = rt.registered_providers().pop().unwrap();
        let cancel = CancelFlag::new();
        let req = kage_provider::StreamRequest::new("m", vec![]);
        let events: Vec<_> = provider
            .stream(req, &cancel)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn malformed_event_propagates_as_provider_error() {
        let rt = PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.register_provider({
                id = 'bad',
                stream = function() return { { type = 'unknown_kind' } } end,
            })
            ",
        )
        .unwrap();
        let provider = rt.registered_providers().pop().unwrap();
        let stream = provider
            .stream(
                kage_provider::StreamRequest::new("m", vec![]),
                &CancelFlag::new(),
            )
            .unwrap();
        let events: Vec<_> = stream.collect();
        assert!(events[0].is_err());
    }
}
