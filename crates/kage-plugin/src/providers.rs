//! `kage.register_provider` and the `Provider` adapter that backs into Lua.
//!
//! Plugins declare a custom provider. The handler may take an optional
//! second `emit` argument and stream events as they happen:
//! ```lua
//! kage.register_provider({
//!     id = "echo",
//!     stream = function(req, emit)
//!         emit({ type = "message_start" })
//!         emit({ type = "text_delta", delta = "hi" })
//!         emit({ type = "message_end", stop_reason = "end_turn",
//!                usage = { input = 0, output = 0, cache_read = 0, cache_write = 0 } })
//!     end,
//! })
//! ```
//! Returning a table or an iterator function still works; events are
//! drained after the handler returns. The host registers each
//! [`LuaProvider`] with its `ProviderRegistry` so the agent loop can
//! route `provider/model` strings into Lua.
//!
//! The handler runs as one job on the runtime's Lua owner thread and
//! occupies it for the whole stream; see [`crate::PluginRuntime`].
//!
//! # Raising a typed error
//!
//! A bare `error("...")` reaches the loop as [`ProviderError::Decode`],
//! which [`ProviderError::is_transient`] treats as permanent, so a
//! plugin that raises on a dropped connection or a 5xx gets no retry
//! even though the agent loop has a retry-with-backoff path for
//! exactly that case. A plugin in the business of talking to an HTTP
//! endpoint therefore classifies its own failures and says which kind
//! it hit:
//!
//! ```lua
//! -- a connect timeout is the pipe's fault, so let the loop retry
//! kage.provider_error("transport", "connect timed out")
//! -- the model does not exist on this provider at all
//! kage.provider_error("unknown_model", model)
//! ```
//!
//! Kinds map onto [`ProviderError`] as follows. `transport` and
//! `rate_limited` are the transient pair the loop retries; the rest are
//! permanent and surface with their own wording instead of the
//! catch-all "malformed response" a bare raise produces.
//!
//! | kind | [`ProviderError`] | retried |
//! | --- | --- | --- |
//! | `transport` | [`Transport`](ProviderError::Transport) | yes |
//! | `rate_limited` | [`RateLimited`](ProviderError::RateLimited) | yes |
//! | `http` | [`Http`](ProviderError::Http) | on 5xx / 408 / 429 |
//! | `auth` | [`Auth`](ProviderError::Auth) | no |
//! | `unknown_model` | [`UnknownModel`](ProviderError::UnknownModel) | no |
//! | `decode` | [`Decode`](ProviderError::Decode) | no |
//!
//! The call never returns: it raises, and the raised value carries the
//! kind rather than being a bare string, because mlua stringifies
//! `error(<table>)` to `table: 0x...` and a table payload would be lost.
//! An unknown kind raises rather than defaulting, so a typo in a kind
//! name is visible instead of silently non-retried.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use kage_core::{CancelFlag, Content, sync::lock};
use kage_provider::{
    EventStream, KillRegistry, Provider, ProviderError, ProviderEvent, ProviderMetadata,
    ProviderModel, StreamRequest, make_cancelable,
};
use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua, lua_to_json};
use crate::capabilities::{Capability, CapabilityRegistry};
use crate::error::PluginError;
use crate::host::{self, LuaHost, WeakHost};
use crate::tasks;

/// Prefix marking a raised message as a typed provider error. The
/// separator is a control character so it cannot collide with prose a
/// plugin would write, and so a message that merely starts with a kind
/// name is not mistaken for one.
const ERROR_KIND_MARKER: &str = "\u{1}kage-provider-error:";

/// Separator between the kind, the optional status, and the message
/// inside a raised typed error.
const ERROR_FIELD_SEP: char = '\u{1}';

/// Where mlua's appended "stack traceback:" block begins in a message
/// that came from a Lua raise.
const TRACEBACK_MARKER: &str = "\nstack traceback:";

/// Events the stream channel buffers before `emit` blocks the owner
/// thread. A lagging consumer therefore applies backpressure to a
/// plugin handler, the way a slow HTTP provider would, instead of
/// letting a fast emitter buffer without bound.
const STREAM_CHANNEL_CAP: usize = 64;

/// Every kind `typed_error` accepts, named in the error a typo
/// produces.
const ERROR_KINDS: &str = "transport, rate_limited, http, auth, unknown_model, decode";

/// The kind, detail and status a typed raise carries, recovered from
/// the string mlua reduced the raised value to.
struct TypedError {
    /// One of the kinds in the table on this module.
    kind: String,
    /// Plugin-supplied detail.
    message: String,
    /// HTTP status, for the `http` kind.
    status: Option<u16>,
}

/// Map a kind name onto a `ProviderError`, or `None` when the name is
/// not one this module documents.
fn typed_error(kind: &str, message: String, status: Option<u16>) -> Option<ProviderError> {
    Some(match kind {
        "transport" => ProviderError::Transport(message),
        // RateLimited carries no detail field, so the plugin message
        // is dropped rather than smuggled somewhere misleading. The
        // retry, which is the point of the kind, still happens.
        "rate_limited" => ProviderError::RateLimited { retry_after: None },
        "http" => ProviderError::Http {
            status: status.unwrap_or(0),
            body: message,
        },
        "auth" => ProviderError::Auth(message),
        "unknown_model" => ProviderError::UnknownModel(message),
        "decode" => ProviderError::Decode(message),
        _ => return None,
    })
}

/// Recover a typed raise from a Lua error, or `None` when this is an
/// ordinary `error("some text")` that keeps the catch-all `Decode`
/// treatment.
///
/// mlua collapses every Lua error to a string, so the kind travels as a
/// marker prefix. The `CallbackError` wrapper a typed raise picks up
/// crossing a Rust callback is unwrapped first, since that is the
/// shape it actually arrives in.
fn recover_typed_error(err: &mlua::Error) -> Option<TypedError> {
    let mut root: &mlua::Error = err;
    while let mlua::Error::CallbackError { cause, .. } = root {
        root = cause;
    }
    // A typed raise is produced by a Rust callback, so it surfaces as
    // an `ExternalError` whose Display is exactly the marker plus the
    // payload. A plugin that hand-rolled the same marker through
    // `error(...)` would be a `RuntimeError`; both are read the same.
    let text = match root {
        mlua::Error::RuntimeError(text) => text.as_str(),
        mlua::Error::ExternalError(inner) => return recover_from_text(&inner.to_string()),
        mlua::Error::WithContext { cause, .. } => return recover_typed_error(cause),
        _ => return None,
    };
    recover_from_text(text)
}

/// Parse the marker payload out of a rendered Lua error message.
fn recover_from_text(text: &str) -> Option<TypedError> {
    let rest = text.strip_prefix(ERROR_KIND_MARKER)?;
    // mlua appends a newline and a "stack traceback:" block to every
    // message it surfaces, so the plugin's own text ends where that
    // begins. Cutting here keeps the traceback off the message, and
    // also keeps it out of the user-facing wording.
    let rest = match rest.find(TRACEBACK_MARKER) {
        Some(at) => &rest[..at],
        None => rest,
    };
    let (kind, rest) = rest.split_once(ERROR_FIELD_SEP)?;
    let (status, message) = match rest.split_once(ERROR_FIELD_SEP) {
        Some((status, message)) => match status.parse::<u16>() {
            Ok(code) => (Some(code), message),
            Err(_) => (None, rest),
        },
        None => (None, rest),
    };
    Some(TypedError {
        kind: kind.to_owned(),
        message: message.to_owned(),
        status,
    })
}

/// Classify a raised Lua error for the loop, honouring a typed raise
/// from `kage.provider_error` and falling back to `Decode` for a bare
/// `error("...")`.
fn classify_raised(err: &mlua::Error) -> ProviderError {
    if let Some(typed) = recover_typed_error(err) {
        return match typed_error(&typed.kind, typed.message, typed.status) {
            Some(mapped) => mapped,
            None => ProviderError::Decode(format!(
                "plugin provider: unknown error kind {:?} (known: {ERROR_KINDS})",
                typed.kind,
            )),
        };
    }
    ProviderError::Decode(format!("plugin provider: {err}"))
}

/// Classify a `PluginError` from the handler job, unwrapping
/// `PluginError::Lua` so a typed raise survives the trip through the
/// error enum.
fn classify_plugin_error(err: &PluginError) -> ProviderError {
    match err {
        PluginError::Lua(inner) => classify_raised(inner),
        other => ProviderError::Decode(format!("plugin provider: {other}")),
    }
}

/// `Provider` whose `stream` runs inside the plugin runtime's Lua state.
pub struct LuaProvider {
    metadata: ProviderMetadata,
    models: Vec<ProviderModel>,
    preserves_thinking: bool,
    host: LuaHost,
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
        mut req: StreamRequest,
        cancel: &CancelFlag,
    ) -> Result<EventStream, ProviderError> {
        // Display-only timing never goes to a plugin provider. Messages
        // are shared with the caller's history, so only ones that
        // actually carry a timed thinking block are cloned out of the
        // `Arc`.
        for msg in &mut req.messages {
            let timed = msg.content.iter().any(|c| {
                matches!(
                    c,
                    Content::Thinking {
                        duration_ms: Some(_),
                        ..
                    }
                )
            });
            if timed {
                for block in &mut Arc::make_mut(msg).content {
                    if let Content::Thinking { duration_ms, .. } = block {
                        *duration_ms = None;
                    }
                }
            }
        }
        let req_value = serde_json::to_value(&req)
            .map_err(|e| ProviderError::Decode(format!("plugin provider: encode request: {e}")))?;
        let (tx, rx) =
            mpsc::sync_channel::<Result<ProviderEvent, ProviderError>>(STREAM_CHANNEL_CAP);
        let handler_key = self.handler_key.clone();
        let sink = self.sink.clone();
        let worker_cancel = cancel.clone();
        self.host
            .submit(move |lua| {
                let tx_err = tx.clone();
                if let Err(e) =
                    run_handler(lua, &handler_key, &sink, &req_value, &worker_cancel, tx)
                {
                    let _ = tx_err.send(Err(classify_plugin_error(&e)));
                }
            })
            .map_err(|e| ProviderError::Decode(format!("plugin provider: {e}")))?;
        Ok(make_cancelable(
            Box::new(ChannelStream { rx }),
            cancel.clone(),
            // Lua streams have no socket to tear down; an empty registry
            // makes the shutdown a no-op and cancel stays cooperative.
            Arc::new(KillRegistry::new()),
        ))
    }

    fn models(&self) -> Vec<ProviderModel> {
        self.models.clone()
    }

    fn preserves_thinking(&self) -> bool {
        self.preserves_thinking
    }
}

/// Channel-backed iterator returned from [`LuaProvider::stream`]. The
/// channel holds at most [`STREAM_CHANNEL_CAP`] events, so an `emit`
/// call blocks the owner thread while the consumer lags. The receiver
/// blocks on `recv()` until the owner-thread job either sends an event
/// or drops the sender (which fuses the iterator).
struct ChannelStream {
    rx: mpsc::Receiver<Result<ProviderEvent, ProviderError>>,
}

impl Iterator for ChannelStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.rx.recv().ok()
    }
}

/// Owner-thread job body: install an `emit` callback that forwards each
/// event onto `tx`, then start the registered Lua handler as a driven
/// coroutine (see [`tasks`]). It runs between other jobs, yielding while
/// it waits on `kage.http` or `kage.sleep_ms`, so concurrent streams
/// share the owner thread instead of queueing behind each other. A
/// plugin that calls `emit` streams; one that returns a table or
/// iterator function is drained after the handler returns.
///
/// The task holds `tx` in an `Arc` and the emit closure only a `Weak`,
/// so the channel closes as soon as the task ends even if the Lua GC
/// has not yet released the closure.
///
/// `cancel` is observed cooperatively: the `emit` callback raises when
/// the flag is set so a streaming handler unwinds, and the table and
/// iterator drain loops break. The foreground iterator already returns
/// `Cancelled` promptly through [`make_cancelable`]; this bounds the
/// task so it does not linger after the turn is abandoned.
fn run_handler(
    lua: &Lua,
    handler_key: &Arc<RegistryKey>,
    sink: &SharedHostLog,
    req: &serde_json::Value,
    cancel: &CancelFlag,
    tx: mpsc::SyncSender<Result<ProviderEvent, ProviderError>>,
) -> Result<(), PluginError> {
    let handler: Function = lua.registry_value(handler_key)?;
    let lua_req = json_to_lua(lua, req)?;
    let tx = Arc::new(tx);
    let emit = emit_function(lua, Arc::downgrade(&tx), sink.clone(), cancel.clone())?;
    let run = tasks::drive::<Value>(lua, handler, (lua_req, emit))?;
    let sink = sink.clone();
    let cancel = cancel.clone();
    tasks::spawn(lua, async move {
        let finished = match run.await {
            Ok(result) => forward_result(result, &tx, &sink, &cancel),
            Err(err) => Err(err),
        };
        if let Err(err) = finished {
            let _ = tx.send(Err(classify_raised(&err)));
        }
    });
    Ok(())
}

/// The `emit` callback a handler streams through. Inside the driven
/// coroutine a full channel yields until the consumer catches up;
/// called anywhere else it blocks, as it always did.
fn emit_function(
    lua: &Lua,
    tx: Weak<mpsc::SyncSender<Result<ProviderEvent, ProviderError>>>,
    sink: SharedHostLog,
    cancel: CancelFlag,
) -> mlua::Result<Function> {
    lua.create_async_function(move |lua, value: Value| {
        let tx = tx.clone();
        let sink = sink.clone();
        let cancel = cancel.clone();
        async move {
            if cancel.is_cancelled() {
                return Err(mlua::Error::external("plugin provider stream cancelled"));
            }
            let Some(tx) = tx.upgrade() else {
                return Ok(());
            };
            let mut event = value_to_provider_event(value, &sink);
            if !tasks::is_driven(&lua) {
                let _ = tx.send(event);
                return Ok(());
            }
            loop {
                match tx.try_send(event) {
                    Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => return Ok(()),
                    Err(mpsc::TrySendError::Full(back)) => {
                        event = back;
                        tasks::sleep(EMIT_RETRY).await;
                    }
                }
            }
        }
    })
}

/// How long a driven `emit` waits before retrying a full channel.
const EMIT_RETRY: Duration = Duration::from_millis(10);

/// Forward what a handler returned: a table of events, an iterator
/// function, or nothing when it streamed through `emit`.
fn forward_result(
    result: Value,
    tx: &mpsc::SyncSender<Result<ProviderEvent, ProviderError>>,
    sink: &SharedHostLog,
    cancel: &CancelFlag,
) -> mlua::Result<()> {
    match result {
        Value::Table(t) => {
            for pair in t.sequence_values::<Value>() {
                if cancel.is_cancelled() {
                    break;
                }
                let v = pair?;
                let _ = tx.send(value_to_provider_event(v, sink));
            }
        }
        Value::Function(f) => loop {
            if cancel.is_cancelled() {
                break;
            }
            let next: Value = match f.call::<Value>(()) {
                Ok(v) => v,
                Err(err) => {
                    let _ = tx.send(Err(classify_raised(&err)));
                    break;
                }
            };
            if matches!(next, Value::Nil) {
                break;
            }
            let _ = tx.send(value_to_provider_event(next, sink));
        },
        Value::Nil => {}
        _ => {
            let _ = tx.send(Err(ProviderError::Decode(
                "plugin provider's stream() returned neither nil, a table, nor a function"
                    .to_owned(),
            )));
        }
    }
    Ok(())
}

/// Parse the optional `models` array on a `register_provider` spec.
/// Each entry must be a `{ id = "...", name = "..." }` table; `name`
/// defaults to `id` when omitted. Missing or non-table `models` yields
/// an empty list (built-in catalog drives the picker in that case).
fn parse_models(spec: &Table) -> mlua::Result<Vec<ProviderModel>> {
    let Ok(models_tbl) = spec.get::<Table>("models") else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for pair in models_tbl.clone().sequence_values::<Value>() {
        let value = pair?;
        let entry = match value {
            Value::Table(t) => t,
            other => {
                return Err(mlua::Error::external(format!(
                    "register_provider: models entry must be a table, got {other:?}"
                )));
            }
        };
        let id: String = entry.get("id").map_err(|e| {
            mlua::Error::external(format!("register_provider: models[].id missing: {e}"))
        })?;
        let name: String = entry.get("name").unwrap_or_else(|_| id.clone());
        let context: Option<u64> = entry.get("context").ok();
        let max_output: Option<u32> = entry.get("max_output").ok();
        out.push(ProviderModel {
            id,
            name,
            context,
            max_output,
            ..ProviderModel::default()
        });
    }
    Ok(out)
}

fn value_to_provider_event(
    value: Value,
    sink: &SharedHostLog,
) -> Result<ProviderEvent, ProviderError> {
    let json = lua_to_json(value)
        .map_err(|e| ProviderError::Decode(format!("plugin provider: lua to json: {e}")))?;
    serde_json::from_value::<ProviderEvent>(json).map_err(|err| {
        let mut s = lock(sink);
        s.log(
            LogLevel::Error,
            &format!("plugin provider yielded undecodable event: {err}"),
        );
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

/// Register the `provider` installer that attaches
/// `kage.register_provider` to a granted plugin's `kage` proxy.
///
/// A registered provider's handler sees the full outgoing request and
/// fabricates the response stream, so registration is not base
/// surface. Reading stored credentials (`env`) and outbound HTTP
/// (`net`) stay separately gated.
pub(crate) fn register(
    registry: &CapabilityRegistry,
    host: WeakHost,
    sink: SharedHostLog,
    registered: &RegisteredProviders,
) {
    let registered = Arc::downgrade(registered);
    let mut reg = lock(registry);
    reg.entry(Capability::Provider)
        .or_default()
        .push(Box::new(move |lua: &Lua, pkage: &Table| {
            let weak_host = host.clone();
            let sink = sink.clone();
            let registered = registered.clone();
            pkage.set(
                "register_provider",
                lua.create_function(move |lua, spec: Table| {
                    let id: String = spec.get("id")?;
                    let display_name: Option<String> = spec.get("display_name").ok();
                    let supports_caching: bool = spec.get("supports_caching").unwrap_or(false);
                    let supports_thinking: bool = spec.get("supports_thinking").unwrap_or(false);
                    let supports_tool_use: bool = spec.get("supports_tool_use").unwrap_or(true);
                    let preserves_thinking: bool = spec.get("preserves_thinking").unwrap_or(false);
                    let stream: Function = spec.get("stream")?;
                    let models = parse_models(&spec)?;
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
                        models,
                        preserves_thinking,
                        host: weak_host.upgrade()?,
                        sink: sink.clone(),
                        handler_key: Arc::new(key),
                    };
                    host::upgrade(&registered)?
                        .lock()
                        .map_err(|_| mlua::Error::external("plugin providers registry poisoned"))?
                        .push(Arc::new(provider));
                    Ok(())
                })?,
            )?;
            // The typed-error raiser rides the same grant as
            // register_provider: it only means something to a plugin
            // that registered a provider.
            pkage.set(
                "provider_error",
                lua.create_function(|_, (kind, message, status): (String, String, Option<u16>)| {
                    if typed_error(&kind, String::new(), None).is_none() {
                        return Err::<(), mlua::Error>(mlua::Error::external(format!(
                            "kage.provider_error: unknown kind {kind:?} (known: {ERROR_KINDS})"
                        )));
                    }
                    let status_field = match kind.as_str() {
                        "http" => match status {
                            Some(code) => code.to_string(),
                            None => String::new(),
                        },
                        _ => String::new(),
                    };
                    Err(mlua::Error::external(format!(
                        "{ERROR_KIND_MARKER}{kind}{ERROR_FIELD_SEP}{status_field}{ERROR_FIELD_SEP}{message}"
                    )))
                })?,
            )?;
            Ok(())
        }));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kage_core::{CancelFlag, Message, Role};
    use kage_provider::{Provider, StopReason};

    use crate::PluginRuntime;

    /// A runtime whose plugin `t` holds `provider`, for exercising the
    /// granted registration path.
    fn granted_runtime() -> PluginRuntime {
        let mut caps = std::collections::BTreeMap::new();
        caps.insert("t".to_owned(), vec!["provider".to_owned()]);
        PluginRuntime::builder().capabilities(caps).build().unwrap()
    }

    fn eval_provider(
        rt: &PluginRuntime,
        body: &str,
    ) -> Result<mlua::Value, crate::error::PluginError> {
        rt.eval_plugin(
            "t",
            &format!("kage.request_capabilities({{'provider'}}); {body}"),
        )
    }

    #[test]
    fn lua_provider_streams_table_of_events() {
        let rt = granted_runtime();
        eval_provider(
            &rt,
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
            vec![Arc::new(Message::new(
                Role::User,
                vec![kage_core::Content::Text { text: "hi".into() }],
                None,
            ))],
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
    fn lua_provider_never_sees_thinking_durations() {
        let rt = granted_runtime();
        eval_provider(&rt,
            r"
            kage.register_provider({
                id = 'peek',
                stream = function(req)
                    local block = req.messages[1].content[1]
                    return {
                        { type = 'text_delta', delta = block.text .. ' ' .. tostring(block.duration_ms) },
                        { type = 'message_end', stop_reason = 'end_turn',
                          usage = { input = 0, output = 0, cache_read = 0, cache_write = 0 } },
                    }
                end,
            })
            ",
        )
        .unwrap();
        let provider = rt.registered_providers().pop().unwrap();
        let req = kage_provider::StreamRequest::new(
            "m",
            vec![Arc::new(Message::new(
                Role::Assistant,
                vec![kage_core::Content::Thinking {
                    text: "plan".into(),
                    signature: None,
                    duration_ms: Some(2_000),
                }],
                None,
            ))],
        );
        let events: Vec<_> = provider
            .stream(req, &CancelFlag::new())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(matches!(
            &events[0],
            kage_provider::ProviderEvent::TextDelta { delta } if delta == "plan nil"
        ));
    }

    #[test]
    fn lua_provider_streams_iterator_function() {
        let rt = granted_runtime();
        eval_provider(
            &rt,
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
    fn lua_provider_streams_via_emit_callback() {
        let rt = granted_runtime();
        eval_provider(
            &rt,
            r"
            kage.register_provider({
                id = 'emitter',
                stream = function(req, emit)
                    emit({ type = 'message_start' })
                    emit({ type = 'text_delta', delta = 'streaming ' })
                    emit({ type = 'text_delta', delta = req.model })
                    emit({ type = 'message_end', stop_reason = 'end_turn',
                           usage = { input = 0, output = 0, cache_read = 0, cache_write = 0 } })
                end,
            })
            ",
        )
        .unwrap();
        let provider = rt.registered_providers().pop().unwrap();
        let cancel = CancelFlag::new();
        let req = kage_provider::StreamRequest::new("model-z", vec![]);
        let events: Vec<_> = provider
            .stream(req, &cancel)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(events.len(), 4);
        assert!(matches!(
            &events[2],
            kage_provider::ProviderEvent::TextDelta { delta } if delta == "model-z"
        ));
        assert!(matches!(
            events[3],
            kage_provider::ProviderEvent::MessageEnd {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));
    }

    /// A provider whose stream sleeps four times for 100ms between
    /// deltas, the way a real one waits on the network.
    fn napping_runtime() -> PluginRuntime {
        let rt = granted_runtime();
        eval_provider(
            &rt,
            r"
            kage.register_provider({
                id = 'napper',
                stream = function(req, emit)
                    emit({ type = 'message_start' })
                    for i = 1, 4 do
                        kage.sleep_ms(100)
                        emit({ type = 'text_delta', delta = tostring(i) })
                    end
                    emit({ type = 'message_end', stop_reason = 'end_turn',
                           usage = { input = 0, output = 0, cache_read = 0, cache_write = 0 } })
                end,
            })
            ",
        )
        .unwrap();
        rt
    }

    fn drain(provider: &super::LuaProvider) -> usize {
        let cancel = CancelFlag::new();
        provider
            .stream(kage_provider::StreamRequest::new("m", vec![]), &cancel)
            .unwrap()
            .map(Result::unwrap)
            .count()
    }

    #[test]
    fn concurrent_streams_share_the_owner_thread() {
        let rt = napping_runtime();
        let provider = rt.registered_providers().pop().unwrap();
        let started = std::time::Instant::now();
        let other = Arc::clone(&provider);
        let second = std::thread::spawn(move || drain(&other));
        assert_eq!(drain(&provider), 6);
        assert_eq!(second.join().unwrap(), 6);
        // Each stream naps 400ms; one after the other they would take 800.
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_millis(700),
            "took {took:?}"
        );
    }

    #[test]
    fn a_host_job_runs_while_a_stream_naps() {
        let rt = napping_runtime();
        let provider = rt.registered_providers().pop().unwrap();
        let stream = std::thread::spawn(move || drain(&provider));
        std::thread::sleep(std::time::Duration::from_millis(50));
        let started = std::time::Instant::now();
        rt.eval_plugin("t", "return 1").unwrap();
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_millis(80), "took {took:?}");
        assert_eq!(stream.join().unwrap(), 6);
    }

    #[test]
    fn lua_provider_stream_observes_cancel_while_handler_runs() {
        use kage_provider::ProviderError;

        let rt = granted_runtime();
        eval_provider(
            &rt,
            r"
            kage.register_provider({
                id = 'hang',
                stream = function(req, emit)
                    emit({ type = 'message_start' })
                    while true do
                        emit({ type = 'text_delta', delta = 'x' })
                        kage.sleep_ms(5)
                    end
                end,
            })
            ",
        )
        .unwrap();
        let provider = rt.registered_providers().pop().unwrap();
        let cancel = CancelFlag::new();
        let mut stream = provider
            .stream(kage_provider::StreamRequest::new("m", vec![]), &cancel)
            .unwrap();
        let first = stream.next().expect("first event before cancel");
        assert!(first.is_ok());
        cancel.cancel();
        let after = stream.next().expect("an item after cancel");
        assert!(
            matches!(after, Err(ProviderError::Cancelled)),
            "expected Cancelled, got {after:?}"
        );
        assert!(stream.next().is_none(), "stream fuses after cancel");
    }

    #[test]
    fn malformed_event_propagates_as_provider_error() {
        let rt = granted_runtime();
        eval_provider(
            &rt,
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

    /// A typed raise must reach the loop as the matching
    /// `ProviderError`, so its retry policy is the right one. A
    /// transport failure is transient; a bare raise stays Decode and
    /// is not, which is the default the plugin boundary has always had.
    #[test]
    fn typed_raise_maps_to_its_provider_error() {
        for (kind, expect_transient) in [
            ("transport", true),
            ("rate_limited", true),
            ("unknown_model", false),
            ("auth", false),
            ("decode", false),
        ] {
            let rt = granted_runtime();
            eval_provider(
                &rt,
                &format!(
                    r#"
            kage.register_provider({{
                id = "boom",
                stream = function()
                    kage.provider_error("{kind}", "boom happened")
                end,
            }})
            "#
                ),
            )
            .unwrap();
            let provider = rt.registered_providers().pop().unwrap();
            let events: Vec<_> = provider
                .stream(
                    kage_provider::StreamRequest::new("m", vec![]),
                    &CancelFlag::new(),
                )
                .unwrap()
                .collect();
            let err = events
                .into_iter()
                .find_map(std::result::Result::err)
                .unwrap_or_else(|| panic!("{kind} produced no error"));
            // RateLimited has no detail field to carry the message,
            // so only the kinds whose variant has one are checked for it.
            if kind != "rate_limited" {
                let text = err.to_string();
                assert!(
                    text.contains("boom happened"),
                    "{kind} lost the message: {text}"
                );
            }
            assert_eq!(
                err.is_transient(),
                expect_transient,
                "{kind} transient={} but test wants {expect_transient}: {err:?}",
                err.is_transient()
            );
        }
    }

    /// The fallback must survive the new binding: a bare
    /// error("...") is still a permanent Decode, and a typo in a kind
    /// name is reported rather than silently treated as permanent.
    #[test]
    fn bare_and_unknown_raises_stay_permanent() {
        let rt = granted_runtime();
        eval_provider(
            &rt,
            r#"
            kage.register_provider({
                id = "plain",
                stream = function() error("just a string") end,
            })
            "#,
        )
        .unwrap();
        let provider = rt.registered_providers().pop().unwrap();
        let events: Vec<_> = provider
            .stream(
                kage_provider::StreamRequest::new("m", vec![]),
                &CancelFlag::new(),
            )
            .unwrap()
            .collect();
        let err = events
            .into_iter()
            .find_map(std::result::Result::err)
            .unwrap();
        assert!(
            matches!(err, kage_provider::ProviderError::Decode(_)),
            "got {err:?}"
        );
        assert!(!err.is_transient(), "a bare raise must not be retried");
        assert!(err.to_string().contains("just a string"));

        // A typo raises when it is called, naming the known kinds. The
        // raise happens inside stream(), so it reaches the stream as an
        // error rather than at registration.
        let rt = granted_runtime();
        eval_provider(
            &rt,
            r#"
            kage.register_provider({
                id = "typo",
                stream = function()
                    kage.provider_error("transprot", "typo")
                end,
            })
            "#,
        )
        .unwrap();
        let provider = rt.registered_providers().pop().unwrap();
        let events: Vec<_> = provider
            .stream(
                kage_provider::StreamRequest::new("m", vec![]),
                &CancelFlag::new(),
            )
            .unwrap()
            .collect();
        let err = events
            .into_iter()
            .find_map(std::result::Result::err)
            .unwrap();
        let text = err.to_string();
        assert!(text.contains("unknown kind"), "got {text}");
        assert!(text.contains("transport"), "names the known kinds: {text}");
    }
}
