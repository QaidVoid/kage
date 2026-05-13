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
use kage_loop::{Hooks, TurnSummary};
use kage_plugin::{LogLevel, PluginRuntime, SharedHostLog, default_host_log};
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
    let runtime = PluginRuntime::builder()
        .sink(sink)
        .workdir(workdir.to_path_buf())
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

/// Hooks adapter that forwards loop events to plugin event handlers.
///
/// The host wraps another `Hooks` (typically the session recorder) so
/// plugin dispatch and session recording both see every event. Plugin
/// dispatch errors are logged through the runtime's host log; they never
/// abort the loop.
pub struct PluginEventHooks<H: Hooks> {
    inner: H,
    runtime: Arc<PluginRuntime>,
}

impl<H: Hooks> PluginEventHooks<H> {
    /// Wrap `inner` so its calls flow through `runtime`'s plugin dispatch.
    pub fn new(inner: H, runtime: Arc<PluginRuntime>) -> Self {
        Self { inner, runtime }
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
                let _ = self.runtime.dispatch_event(
                    "message_end",
                    &json!({
                        "id": id.to_string(),
                        "usage": {
                            "input": usage.input,
                            "output": usage.output,
                            "cache_read": usage.cache_read,
                            "cache_write": usage.cache_write,
                        },
                    }),
                );
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
            _ => {}
        }
        self.inner.on_event(event);
    }

    fn transform_context(&mut self, messages: &mut Vec<Message>) -> Result<(), String> {
        self.inner.transform_context(messages)
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
        self.inner.should_stop_after_turn(summary)
    }

    fn get_steering(&mut self) -> Option<String> {
        self.inner.get_steering()
    }

    fn get_followup(&mut self) -> Option<String> {
        self.inner.get_followup()
    }
}
