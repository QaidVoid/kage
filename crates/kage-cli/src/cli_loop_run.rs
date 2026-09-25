//! Print-mode run driver and the hooks wrapper.

#[allow(clippy::wildcard_imports)] // split out of main.rs; shares the crate-root scope
use super::*;

/// Drive one print-mode run on the engine. Streams events to stdout as
/// text or JSONL, records the conversation when a writer is supplied, and
/// maps the outcome to a process exit code. Text mode prints the opened
/// session only, while JSON mode prints the envelopes of its agents too.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_print_run(
    registry: Arc<ProviderRegistry>,
    model: &str,
    tools: kage_tools::ToolRegistry,
    mut cx: AgentContext,
    prompt: String,
    writer: Option<SessionWriter>,
    plugin_runtime: Option<Arc<kage_plugin::PluginRuntime>>,
    mcp: Option<kage_mcp::McpManager>,
    json_mode: bool,
) -> ExitCode {
    use kage_core::protocol::{Command, CommandKind, Delivery, Event, HostEvent, RunOutcome};

    let workdir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::trust::warn_if_untrusted(&workdir);
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

    let (defs, agent_errors) = crate::agents::load(&workdir);
    for err in agent_errors {
        eprintln!("kage: {err}");
    }
    let agents = crate::engine::AgentSetup::from_config(defs, &layered);

    let session = writer
        .as_ref()
        .and_then(|w| crate::engine::session_id_of(w.path()))
        .unwrap_or_default();
    let (ended_tx, ended_rx) = std::sync::mpsc::channel();
    let printer: crate::engine::Subscriber = Box::new(move |envelope| {
        print_envelope(&mut io::stdout().lock(), envelope, session, json_mode);
        if envelope.session == session
            && let Event::Host(HostEvent::RunEnded { outcome }) = &envelope.event
        {
            let _ = ended_tx.send(outcome.clone());
        }
    });

    let mcp_servers = mcp
        .as_ref()
        .map(|m| m.server_names().map(str::to_owned).collect())
        .unwrap_or_default();
    let engine = crate::engine::Engine::start(registry);
    engine.subscribe(printer);
    engine.open(crate::engine::SessionSpec {
        id: session,
        model: model.to_owned(),
        cx,
        recorder: writer.map(|w| crate::engine::Recorder::new(w, plugin_runtime.clone())),
        tools,
        plugins: plugin_runtime,
        gate: crate::permissions::PermissionGate::new(layered.permissions)
            .with_mcp_servers(mcp_servers),
        loop_cfg: LoopConfig {
            compaction_threshold: layered.loop_settings.compaction_threshold,
            ..LoopConfig::default()
        },
        mcp,
        interactive: false,
        title: false,
        agents: Some(agents),
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

/// Print one envelope: every envelope as a JSON line in JSON mode, else
/// the loop events of `session` as text and notices on stderr.
pub(crate) fn print_envelope<W: io::Write>(
    out: &mut W,
    envelope: &kage_core::protocol::Envelope,
    session: SessionId,
    json_mode: bool,
) {
    use kage_core::protocol::{Event, HostEvent};

    if json_mode {
        print_envelope_json(out, envelope);
        return;
    }
    match &envelope.event {
        Event::Loop(event) if envelope.session == session => print_event(out, event),
        Event::Host(HostEvent::Notice { text, .. }) => eprintln!("kage: {text}"),
        _ => {}
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
