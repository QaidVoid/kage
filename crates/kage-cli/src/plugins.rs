//! Plugin integration for the CLI.
//!
//! [`setup_runtime`] builds a [`PluginRuntime`] for the current run, loads
//! every `*.lua` file in `plugins_dir`, and returns the runtime if any
//! plugin contributed anything. [`PluginEventHooks`] wraps another `Hooks`
//! and forwards loop events to subscribed plugin handlers, plus synthesizes
//! the `agent_start` / `agent_end` events the loop never emits itself.

use std::path::Path;
use std::sync::Arc;

use kage_core::{LoopEvent, Message, ToolOutput};
use kage_loop::{CompactionPrep, Hooks, StreamRequest, TurnSummary};
use kage_plugin::{LogLevel, PluginRuntime, SharedHostLog, default_host_log};
use kage_provider::{Provider, ProviderRegistry};
use serde_json::json;

/// Construct a plugin runtime, load `*.lua` files from `plugins_dir`, and
/// return the runtime if at least one plugin loaded successfully. Returns
/// `Ok(None)` when the directory is missing or empty. Uses the default
/// stderr-backed sink; use [`setup_runtime_with_sink`] when the host owns
/// the alt screen and stderr writes would corrupt the rendered frame.
pub fn setup_runtime(
    plugins_dir: &Path,
    workdir: &Path,
    model: &str,
    system_prompt: &str,
) -> Result<Option<Arc<PluginRuntime>>, String> {
    setup_runtime_with_sink(
        plugins_dir,
        workdir,
        model,
        system_prompt,
        default_host_log(),
    )
}

/// Same as [`setup_runtime`] but with a caller-supplied `HostLog` sink.
/// The TUI uses [`kage_tui::buffer_host_log`] to route plugin output
/// into the conversation buffer instead of stderr.
pub fn setup_runtime_with_sink(
    plugins_dir: &Path,
    workdir: &Path,
    model: &str,
    system_prompt: &str,
    sink: SharedHostLog,
) -> Result<Option<Arc<PluginRuntime>>, String> {
    // Capability grants and the load allowlist come from the same
    // layered config the rest of the host reads. Fail closed: if the
    // config cannot be loaded, no plugin gets any elevated capability
    // rather than silently proceeding with an unknown grant set.
    let plugins_cfg = kage_core::config::Config::load_layered(workdir)
        .map(|c| c.plugins)
        .unwrap_or_default();
    let runtime = PluginRuntime::builder()
        .sink(sink)
        .workdir(workdir.to_path_buf())
        .capabilities(plugins_cfg.capabilities)
        .enabled(plugins_cfg.enabled)
        .plugin_config(plugins_cfg.config)
        .config(json!({
            "model": model,
            "cwd": workdir.display().to_string(),
            "system_prompt": system_prompt,
        }))
        .build()
        .map_err(|e| format!("plugin runtime: {e}"))?;
    let report =
        kage_plugin::load_dir(plugins_dir, &runtime).map_err(|e| format!("plugin load: {e}"))?;
    for (path, err) in &report.failed {
        eprintln!("kage: plugin {} failed to load: {err}", path.display());
    }
    for path in &report.skipped {
        eprintln!(
            "kage: plugin {} skipped (not in [plugins] enabled)",
            path.display()
        );
    }
    if report.loaded.is_empty() {
        return Ok(None);
    }
    eprintln!(
        "kage: loaded {} plugin{} from {}",
        report.loaded.len(),
        if report.loaded.len() == 1 { "" } else { "s" },
        plugins_dir.display(),
    );
    Ok(Some(Arc::new(runtime)))
}

/// Register every provider a plugin contributed via
/// `kage.register_provider` into `registry`. A plugin can shadow a
/// built-in id (the registry overwrites prior entries with the same
/// id); we log that case so the user sees the override deliberately.
pub fn merge_plugin_providers(runtime: &PluginRuntime, registry: &mut ProviderRegistry) {
    let existing: std::collections::HashSet<String> = registry.ids().map(str::to_owned).collect();
    for provider in runtime.registered_providers() {
        let id = provider.metadata().id.clone();
        if existing.contains(&id) {
            eprintln!("kage: plugin provider `{id}` shadows the built-in registration");
        }
        registry.register(provider);
    }
}

/// Hooks adapter that forwards loop events to plugin event handlers.
///
/// The host wraps another `Hooks` (typically the session recorder) so
/// plugin dispatch and session recording both see every event. Plugin
/// dispatch errors are logged through the runtime's host log; they never
/// abort the loop.
pub struct PluginEventHooks<H: Hooks> {
    inner: H,
    runtime: Arc<PluginRuntime>,
    /// FIFO of `kage.send_message` payloads pulled out of the runtime
    /// at most once per loop pass. The loop polls
    /// [`Hooks::get_steering`] potentially many times; we drain the
    /// runtime queue lazily into `pending_steering` so each call
    /// returns at most one message and we never lose ordering across
    /// turns.
    pending_steering: std::collections::VecDeque<String>,
}

impl<H: Hooks> PluginEventHooks<H> {
    /// Wrap `inner` so its calls flow through `runtime`'s plugin dispatch.
    pub fn new(inner: H, runtime: Arc<PluginRuntime>) -> Self {
        Self {
            inner,
            runtime,
            pending_steering: std::collections::VecDeque::new(),
        }
    }

    /// Synthesize the `before_agent_start` event before the loop's first
    /// turn. Fires with the system prompt and the first user message text
    /// in scope so plugins can observe the inputs to the upcoming run.
    pub fn dispatch_before_agent_start(&self, system_prompt: &str, first_user_message: &str) {
        let payload = json!({
            "system_prompt": system_prompt,
            "first_user_message": first_user_message,
        });
        if let Err(err) = self.runtime.dispatch_event("before_agent_start", &payload) {
            self.log_error(format_args!("before_agent_start dispatch: {err}"));
        }
    }

    /// Synthesize the `agent_start` event before the loop's first turn.
    pub fn dispatch_agent_start(&self) {
        if let Err(err) = self.runtime.dispatch_event("agent_start", &json!({})) {
            self.log_error(format_args!("agent_start dispatch: {err}"));
        }
    }

    /// Synthesize the `agent_end` event after the loop returns.
    pub fn dispatch_agent_end(&self, ok: bool) {
        let payload = json!({ "ok": ok });
        if let Err(err) = self.runtime.dispatch_event("agent_end", &payload) {
            self.log_error(format_args!("agent_end dispatch: {err}"));
        }
    }

    fn log_error(&self, args: std::fmt::Arguments<'_>) {
        if let Ok(mut sink) = self.runtime.sink().lock() {
            sink.log(LogLevel::Error, &args.to_string());
        }
    }
}

impl<H: Hooks> Hooks for PluginEventHooks<H> {
    fn before_tool_call(&mut self, name: &str, input: &serde_json::Value) -> Option<ToolOutput> {
        self.inner.before_tool_call(name, input)
    }

    fn after_tool_call(&mut self, name: &str, output: ToolOutput) -> ToolOutput {
        self.inner.after_tool_call(name, output)
    }

    fn on_event(&mut self, event: &LoopEvent) {
        match event {
            LoopEvent::MessageStart { id } if self.runtime.handler_count("message_start") > 0 => {
                let _ = self
                    .runtime
                    .dispatch_event("message_start", &json!({ "id": id.to_string() }));
            }
            LoopEvent::TextDelta { id, delta }
                if self.runtime.handler_count("message_update") > 0 =>
            {
                let _ = self.runtime.dispatch_event(
                    "message_update",
                    &json!({
                        "id": id.to_string(),
                        "delta": delta,
                    }),
                );
            }
            LoopEvent::MessageEnd { id, usage } => {
                let payload = json!({
                    "id": id.to_string(),
                    "usage": {
                        "input": usage.input,
                        "output": usage.output,
                        "cache_read": usage.cache_read,
                        "cache_write": usage.cache_write,
                    },
                });
                let _ = self.runtime.dispatch_event("message_end", &payload);
                if self.runtime.handler_count("after_provider_response") > 0 {
                    let _ = self
                        .runtime
                        .dispatch_event("after_provider_response", &payload);
                }
            }
            LoopEvent::ToolCallStart {
                id,
                name,
                input_partial,
            } => {
                let _ = self.runtime.dispatch_event(
                    "tool_call",
                    &json!({
                        "id": id.to_string(),
                        "name": name,
                        "input": input_partial,
                    }),
                );
            }
            LoopEvent::ToolCallEnd { id, output } => {
                let _ = self.runtime.dispatch_event(
                    "tool_result",
                    &json!({
                        "id": id.to_string(),
                        "is_error": output.is_error,
                        "text": output.text,
                    }),
                );
            }
            LoopEvent::ToolUpdate { id, update }
                if self.runtime.handler_count("tool_update") > 0 =>
            {
                let _ = self.runtime.dispatch_event(
                    "tool_update",
                    &json!({
                        "id": id.to_string(),
                        "content": update.content,
                        "structured": update.structured,
                    }),
                );
            }
            _ => {}
        }
        self.inner.on_event(event);
    }

    fn transform_context(&mut self, messages: &mut Vec<Message>) -> Result<(), String> {
        self.inner.transform_context(messages)?;
        if self.runtime.handler_count("transform_context") == 0 {
            return Ok(());
        }
        let payload = serde_json::to_value(&*messages)
            .map_err(|e| format!("transform_context: serialize history: {e}"))?;
        let result = self
            .runtime
            .dispatch_transform("transform_context", payload)
            .map_err(|e| format!("transform_context: lua dispatch: {e}"))?;
        let next: Vec<Message> = serde_json::from_value(result)
            .map_err(|e| format!("transform_context: plugin returned invalid history: {e}"))?;
        *messages = next;
        Ok(())
    }

    fn transform_provider_request(&mut self, req: &mut StreamRequest) -> Result<(), String> {
        self.inner.transform_provider_request(req)?;
        if self.runtime.handler_count("before_provider_request") == 0 {
            return Ok(());
        }
        let payload = serde_json::to_value(&*req)
            .map_err(|e| format!("before_provider_request: serialize request: {e}"))?;
        let result = self
            .runtime
            .dispatch_transform("before_provider_request", payload)
            .map_err(|e| format!("before_provider_request: lua dispatch: {e}"))?;
        let next: StreamRequest = serde_json::from_value(result).map_err(|e| {
            format!("before_provider_request: plugin returned invalid request: {e}")
        })?;
        *req = next;
        Ok(())
    }

    fn prepare_compaction(&mut self, prep: &mut CompactionPrep) -> Result<(), String> {
        self.inner.prepare_compaction(prep)?;
        if self.runtime.handler_count("compact_prepare") == 0 {
            return Ok(());
        }
        let payload = json!({
            "transcript": prep.transcript,
            "instruction": prep.instruction,
            "prompt": prep.prompt,
            "model": prep.model,
            "summarized": prep.summarized,
            "kept": prep.kept,
        });
        let result = self
            .runtime
            .dispatch_transform("compact_prepare", payload)
            .map_err(|e| format!("compact_prepare: lua dispatch: {e}"))?;
        if let Some(obj) = result.as_object() {
            if let Some(s) = obj.get("prompt").and_then(|v| v.as_str()) {
                s.clone_into(&mut prep.prompt);
            }
            if let Some(s) = obj.get("instruction").and_then(|v| v.as_str()) {
                s.clone_into(&mut prep.instruction);
            }
            if let Some(s) = obj.get("summary").and_then(|v| v.as_str()) {
                prep.summary_override = Some(s.to_owned());
            }
        }
        Ok(())
    }

    fn on_turn_start(&mut self, index: u32) {
        let _ = self
            .runtime
            .dispatch_event("turn_start", &json!({ "index": index }));
        self.inner.on_turn_start(index);
    }

    fn on_turn_end(&mut self, index: u32, had_tool_calls: bool) {
        let _ = self.runtime.dispatch_event(
            "turn_end",
            &json!({
                "index": index,
                "had_tool_calls": had_tool_calls,
            }),
        );
        self.inner.on_turn_end(index, had_tool_calls);
    }

    fn should_stop_after_turn(&mut self, summary: &TurnSummary) -> bool {
        if self.inner.should_stop_after_turn(summary) {
            return true;
        }
        if self.runtime.handler_count("should_stop_after_turn") == 0 {
            return false;
        }
        let payload = json!({
            "index": summary.index,
            "had_tool_calls": summary.had_tool_calls,
            "usage": {
                "input": summary.usage.input,
                "output": summary.usage.output,
                "cache_read": summary.usage.cache_read,
                "cache_write": summary.usage.cache_write,
            },
        });
        match self
            .runtime
            .dispatch_predicate("should_stop_after_turn", &payload)
        {
            Ok(stop) => stop,
            Err(err) => {
                self.log_error(format_args!("should_stop_after_turn dispatch: {err}"));
                false
            }
        }
    }

    fn get_steering(&mut self) -> Option<String> {
        // Inner host hooks win when they have a steering message: a
        // CLI-issued slash command or the user's typed prompt should
        // not be overridden by a plugin's `send_message` chatter. We
        // only consult the plugin queue when the inner hook produced
        // nothing.
        if let Some(msg) = self.inner.get_steering() {
            return Some(msg);
        }
        self.drain_plugin_messages();
        self.pending_steering.pop_front()
    }

    fn get_followup(&mut self) -> Option<String> {
        if let Some(msg) = self.inner.get_followup() {
            return Some(msg);
        }
        self.drain_plugin_messages();
        self.pending_steering.pop_front()
    }

    fn on_user_message(&mut self, message: &Message) {
        self.inner.on_user_message(message);
    }
}

impl<H: Hooks> PluginEventHooks<H> {
    /// Move every queued `kage.send_message` payload from the runtime
    /// into `pending_steering`. Non-user roles are filtered out and
    /// logged because 0.1 has no synthetic-assistant or system-note
    /// delivery path; the Lua boundary already rejects those, so
    /// hitting this branch means a future API expansion didn't
    /// update the host side.
    fn drain_plugin_messages(&mut self) {
        for msg in self.runtime.take_pending_messages() {
            match msg.deliver_as {
                kage_plugin::PendingRole::User => self.pending_steering.push_back(msg.text),
                other => self.log_error(format_args!(
                    "send_message: dropping unsupported deliver_as {other:?}"
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use kage_loop::NoopHooks;
    use kage_plugin::PluginRuntime;

    use super::*;

    #[test]
    fn get_steering_returns_send_message_payload_when_inner_is_empty() {
        let rt = Arc::new(PluginRuntime::new().unwrap());
        rt.eval("kage.send_message('please continue')").unwrap();
        let mut hooks = PluginEventHooks::new(NoopHooks, Arc::clone(&rt));
        assert_eq!(hooks.get_steering(), Some("please continue".to_owned()));
        // Queue exhausted after one drain.
        assert_eq!(hooks.get_steering(), None);
    }

    #[test]
    fn inner_steering_takes_precedence_over_plugin_queue() {
        struct InnerOnce(Option<String>);
        impl Hooks for InnerOnce {
            fn get_steering(&mut self) -> Option<String> {
                self.0.take()
            }
        }
        let rt = Arc::new(PluginRuntime::new().unwrap());
        rt.eval("kage.send_message('plugin says hi')").unwrap();
        let mut hooks =
            PluginEventHooks::new(InnerOnce(Some("user typed this".into())), Arc::clone(&rt));
        // First poll: inner wins.
        assert_eq!(hooks.get_steering(), Some("user typed this".to_owned()));
        // Second poll: plugin queue drains.
        assert_eq!(hooks.get_steering(), Some("plugin says hi".to_owned()));
    }

    fn sample_prep() -> CompactionPrep {
        CompactionPrep {
            transcript: "=== user ===\nhi\n".to_owned(),
            instruction: "old instruction".to_owned(),
            prompt: "old prompt".to_owned(),
            model: "mock:m".to_owned(),
            summarized: 6,
            kept: 4,
            summary_override: None,
        }
    }

    #[test]
    fn compact_prepare_summary_override_skips_model() {
        let rt = Arc::new(PluginRuntime::new().unwrap());
        rt.eval(
            "kage.on('compact_prepare', function(ev)
                 return { summary = 'PLUGIN ' .. tostring(ev.summarized) }
             end)",
        )
        .unwrap();
        let mut hooks = PluginEventHooks::new(NoopHooks, Arc::clone(&rt));
        let mut p = sample_prep();
        hooks.prepare_compaction(&mut p).unwrap();
        assert_eq!(p.summary_override.as_deref(), Some("PLUGIN 6"));
    }

    #[test]
    fn compact_prepare_rewrites_prompt_and_instruction() {
        let rt = Arc::new(PluginRuntime::new().unwrap());
        rt.eval(
            "kage.on('compact_prepare', function(_ev)
                 return { prompt = 'NEW', instruction = 'INS' }
             end)",
        )
        .unwrap();
        let mut hooks = PluginEventHooks::new(NoopHooks, Arc::clone(&rt));
        let mut p = sample_prep();
        hooks.prepare_compaction(&mut p).unwrap();
        assert_eq!(p.prompt, "NEW");
        assert_eq!(p.instruction, "INS");
        assert_eq!(p.summary_override, None);
    }

    #[test]
    fn compact_prepare_without_handler_is_passthrough() {
        let rt = Arc::new(PluginRuntime::new().unwrap());
        let mut hooks = PluginEventHooks::new(NoopHooks, Arc::clone(&rt));
        let mut p = sample_prep();
        hooks.prepare_compaction(&mut p).unwrap();
        assert_eq!(p.prompt, "old prompt");
        assert_eq!(p.instruction, "old instruction");
        assert_eq!(p.summary_override, None);
    }

    #[test]
    fn send_message_drains_in_fifo_order() {
        let rt = Arc::new(PluginRuntime::new().unwrap());
        rt.eval("kage.send_message('first'); kage.send_message('second')")
            .unwrap();
        let mut hooks = PluginEventHooks::new(NoopHooks, Arc::clone(&rt));
        assert_eq!(hooks.get_steering(), Some("first".to_owned()));
        assert_eq!(hooks.get_steering(), Some("second".to_owned()));
        assert_eq!(hooks.get_steering(), None);
    }
}
