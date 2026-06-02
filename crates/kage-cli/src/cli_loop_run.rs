//! Print-mode run driver and the hooks wrapper.

#[allow(clippy::wildcard_imports)] // split out of main.rs; shares the crate-root scope
use super::*;

/// Drive one print-mode run. Streams loop events to stdout and, when a
/// writer is supplied, records the conversation. When a plugin runtime is
/// supplied, plugin event handlers fire at turn boundaries. Returns the
/// appropriate process exit code.
pub(crate) fn execute_print_run(
    provider: &dyn kage_provider::Provider,
    tools: &kage_tools::ToolRegistry,
    cx: &mut AgentContext,
    user_msg: &Message,
    writer: Option<SessionWriter>,
    plugin_runtime: Option<std::sync::Arc<kage_plugin::PluginRuntime>>,
    json_mode: bool,
) -> ExitCode {
    let workdir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let cfg = match kage_core::config::Config::load_layered(&workdir) {
        Ok(c) => LoopConfig {
            compaction_threshold: c.loop_settings.compaction_threshold,
            ..LoopConfig::default()
        },
        Err(e) => {
            eprintln!("kage: config error: {e}; using defaults");
            LoopConfig::default()
        }
    };
    let cancel = CancelFlag::new();
    let mut stdout = io::stdout().lock();
    let result = run_with_hooks(
        provider,
        tools,
        cx,
        cfg,
        &cancel,
        NoopHooks,
        user_msg,
        writer,
        plugin_runtime,
        |event| {
            if json_mode {
                print_event_json(&mut stdout, &event);
            } else {
                print_event(&mut stdout, &event);
            }
        },
    );
    if !json_mode {
        let _ = writeln!(stdout);
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_with_hooks<B, F>(
    provider: &dyn kage_provider::Provider,
    tools: &kage_tools::ToolRegistry,
    cx: &mut AgentContext,
    cfg: LoopConfig,
    cancel: &CancelFlag,
    base_hooks: B,
    user_msg: &Message,
    writer: Option<SessionWriter>,
    plugin_runtime: Option<std::sync::Arc<kage_plugin::PluginRuntime>>,
    mut emit: F,
) -> Result<(), kage_core::LoopError>
where
    B: Hooks + 'static,
    F: FnMut(LoopEvent),
{
    let mut session_layer: Box<dyn Hooks> = match writer {
        None => Box::new(base_hooks),
        Some(w) => {
            let mut hooks = SessionRecordingHooks::new(base_hooks, w);
            if let Some(rt) = plugin_runtime.as_ref() {
                hooks = hooks.with_plugin_runtime(Arc::clone(rt));
            }
            hooks.record_user_message(user_msg);
            Box::new(hooks)
        }
    };

    if let Some(runtime) = plugin_runtime {
        let mut hooks = PluginEventHooks::new(BoxedHooks(session_layer), runtime.clone());
        hooks.dispatch_before_agent_start(&cx.system_prompt, &first_user_text(user_msg));
        hooks.dispatch_agent_start();
        let res = run(provider, tools, cx, cfg, &mut hooks, cancel, &mut emit);
        hooks.dispatch_agent_end(res.is_ok());
        res
    } else {
        run(
            provider,
            tools,
            cx,
            cfg,
            session_layer.as_mut(),
            cancel,
            &mut emit,
        )
    }
}

/// Extract the first text block from a user message, joined with newlines
/// if there are multiple. Returns an empty string when the message carries
/// no text (image-only, tool-result-only, etc.).
pub(crate) fn first_user_text(msg: &Message) -> String {
    let mut out = String::new();
    for block in &msg.content {
        if let Content::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

/// Adapter so a `Box<dyn Hooks>` satisfies the static-dispatch `Hooks`
/// bound on [`PluginEventHooks`].
struct BoxedHooks(Box<dyn Hooks>);

impl Hooks for BoxedHooks {
    fn before_tool_call(
        &mut self,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<kage_core::ToolOutput> {
        self.0.before_tool_call(name, input)
    }

    fn after_tool_call(
        &mut self,
        name: &str,
        output: kage_core::ToolOutput,
    ) -> kage_core::ToolOutput {
        self.0.after_tool_call(name, output)
    }

    fn on_event(&mut self, event: &LoopEvent) {
        self.0.on_event(event);
    }

    fn transform_context(&mut self, messages: &mut Vec<kage_core::Message>) -> Result<(), String> {
        self.0.transform_context(messages)
    }

    fn transform_provider_request(
        &mut self,
        req: &mut kage_loop::StreamRequest,
    ) -> Result<(), String> {
        self.0.transform_provider_request(req)
    }

    fn on_turn_start(&mut self, index: u32) {
        self.0.on_turn_start(index);
    }

    fn on_turn_end(&mut self, index: u32, had_tool_calls: bool) {
        self.0.on_turn_end(index, had_tool_calls);
    }

    fn should_stop_after_turn(&mut self, summary: &kage_loop::TurnSummary) -> bool {
        self.0.should_stop_after_turn(summary)
    }

    fn get_steering(&mut self) -> Option<String> {
        self.0.get_steering()
    }

    fn get_followup(&mut self) -> Option<String> {
        self.0.get_followup()
    }

    fn on_user_message(&mut self, message: &kage_core::Message) {
        self.0.on_user_message(message);
    }
}
