//! Print-mode run driver and the hooks wrapper.

#[allow(clippy::wildcard_imports)] // split out of main.rs; shares the crate-root scope
use super::*;

/// Drive one print-mode run on the engine. Streams events to stdout as
/// text or JSONL, records the conversation when a writer is supplied, and
/// maps the outcome to a process exit code.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_print_run(
    registry: Arc<ProviderRegistry>,
    model: &str,
    tools: kage_tools::ToolRegistry,
    mut cx: AgentContext,
    prompt: String,
    writer: Option<SessionWriter>,
    plugin_runtime: Option<Arc<kage_plugin::PluginRuntime>>,
    json_mode: bool,
) -> ExitCode {
    use kage_core::protocol::{Command, CommandKind, Delivery, Event, HostEvent, RunOutcome};

    let workdir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let layered = match kage_core::config::Config::load_layered(&workdir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kage: {e}; using defaults");
            kage_core::config::Config::default()
        }
    };
    if let Err(e) = layered.permissions.validate() {
        eprintln!("kage: {e}");
        return ExitCode::from(1);
    }
    if layered.permissions.confine_paths {
        cx.confine_paths = true;
    }
    if let Err(err) = crate::sigint::install() {
        eprintln!("kage: {err}; Ctrl-C will kill the process");
    }

    let (ended_tx, ended_rx) = std::sync::mpsc::channel();
    let printer: crate::engine::Subscriber = Box::new(move |envelope| {
        let mut stdout = io::stdout().lock();
        if json_mode {
            print_envelope_json(&mut stdout, envelope);
        }
        match &envelope.event {
            Event::Loop(event) if !json_mode => print_event(&mut stdout, event),
            Event::Host(HostEvent::Notice { text, .. }) if !json_mode => eprintln!("kage: {text}"),
            Event::Host(HostEvent::RunEnded { outcome }) => {
                let _ = ended_tx.send(outcome.clone());
            }
            _ => {}
        }
    });

    let session = writer
        .as_ref()
        .and_then(|w| crate::engine::session_id_of(w.path()))
        .unwrap_or_default();
    let engine = crate::engine::Engine::start(
        crate::engine::EngineConfig {
            registry,
            tools,
            loop_cfg: LoopConfig {
                compaction_threshold: layered.loop_settings.compaction_threshold,
                ..LoopConfig::default()
            },
            gate: crate::permissions::PermissionGate::new(layered.permissions),
            plugins: plugin_runtime.clone(),
        },
        vec![printer],
    );
    engine.open(crate::engine::SessionSpec {
        id: session,
        model: model.to_owned(),
        cx,
        recorder: writer.map(|w| crate::engine::Recorder::new(w, plugin_runtime)),
    });
    engine.send(Command::active(CommandKind::Prompt {
        content: vec![Content::Text { text: prompt }],
        delivery: Delivery::Queue,
    }));

    let outcome = loop {
        match ended_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(outcome) => break Some(outcome),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if crate::sigint::requested() {
                    engine.send(Command::active(CommandKind::Cancel));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break None,
        }
    };
    engine.shutdown();
    if !json_mode {
        println!();
    }
    if crate::sigint::requested() {
        return ExitCode::from(130);
    }
    match outcome {
        Some(RunOutcome::Completed) => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
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
