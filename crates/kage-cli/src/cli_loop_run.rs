//! Print-mode run driver and the hooks wrapper.

use std::io;
use std::process::ExitCode;
use std::sync::Arc;

use kage_core::{Content, LoopEvent, Message};
use kage_loop::{AgentContext, LoopConfig};
use kage_provider::ProviderRegistry;
use kage_session::{SessionId, SessionWriter};

use crate::cli_printing::{print_envelope_json, print_event};

/// Drive one print-mode run on the engine. Streams events to stdout as
/// text or JSONL, records the conversation when a writer is supplied, and
/// maps the outcome to a process exit code. Text mode prints the opened
/// session only, while JSON mode prints the envelopes of its agents too.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is separately built run state"
)]
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
            eprintln!("kage: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = layered.permissions.validate() {
        eprintln!("kage: {e}");
        return ExitCode::from(1);
    }
    if let Err(e) = layered.bash.validate() {
        eprintln!("kage: {e}");
        return ExitCode::from(1);
    }
    let tools = tools.with_env_scrub(&layered.bash.scrub_env);
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
        Event::Loop(event) if envelope.session == session => print_loop_event(out, event),
        Event::Host(HostEvent::Notice { text, .. }) => eprintln!("kage: {text}"),
        _ => {}
    }
}

/// Print a loop event as text. An `agent` call prints the agent and its
/// description when it starts and how the agent ended, instead of the
/// generic tool lines and the wrapper the model reads.
fn print_loop_event<W: io::Write>(out: &mut W, event: &LoopEvent) {
    match event {
        LoopEvent::ToolCallStart {
            name,
            input_partial,
            ..
        } if name == "agent" => {
            let field = |key| input_partial.get(key).and_then(serde_json::Value::as_str);
            let agent = field("agent").unwrap_or("general");
            let description = field("description").unwrap_or_default();
            let _ = writeln!(out, "\n[agent {agent}: {description}]");
            let _ = out.flush();
        }
        LoopEvent::ToolCallEnd { output, .. } => match agent_end(&output.text) {
            Some((agent, "failed", error)) => {
                let _ = writeln!(out, "[agent {agent} failed] {error}");
                let _ = out.flush();
            }
            Some((agent, state, _)) => {
                let _ = writeln!(out, "[agent {agent} {state}]");
                let _ = out.flush();
            }
            None => print_event(out, event),
        },
        _ => print_event(out, event),
    }
}

/// The agent name, end state and body of an `agent` call result, or
/// `None` when the text is not wrapped in an `<agent>` element.
fn agent_end(text: &str) -> Option<(&str, &str, &str)> {
    let (attrs, rest) = text.strip_prefix("<agent ")?.split_once(">\n")?;
    let body = rest.strip_suffix("\n</agent>")?;
    let attr = |key: &str| {
        attrs
            .split_once(&format!("{key}=\""))
            .and_then(|(_, value)| value.split_once('"'))
            .map(|(value, _)| value)
    };
    Some((attr("name")?, attr("state")?, body))
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
