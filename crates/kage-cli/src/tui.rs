//! Interactive TUI mode for kage.
//!
//! [`run_tui`] glues [`kage_tui::App`] (main thread) to a worker thread
//! that runs the agent loop. Submitted prompts arrive via an `mpsc`
//! channel; for each prompt the worker pushes the user message into
//! [`AgentContext::history`], runs the loop with [`TuiHooks`] mirroring
//! events into the shared buffer, and resets the cancel flag for the
//! next turn.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use kage_core::{CancelFlag, Content, Message, Role};
use kage_loop::{AgentContext, LoopConfig, NoopHooks, run};
use kage_plugin::PluginRuntime;
use kage_provider::Provider;
use kage_tools::ToolRegistry;
use kage_tui::{App, RunRequest, SharedBuffer, Tui, TuiHooks, buffer_host_log, shared_buffer};

use crate::plugins::{PluginEventHooks, setup_runtime_with_sink};

/// Drop into the interactive TUI. Returns the appropriate process exit
/// code once the user quits.
pub fn run_tui(model: &str, system: &str) -> ExitCode {
    let registry = crate::build_provider_registry();
    if registry.ids().count() == 0 {
        eprintln!(
            "kage: no provider API keys found in environment. Set one of \
             ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, ZAI_API_KEY."
        );
        return ExitCode::from(1);
    }
    let resolved = match registry.resolve(model) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kage: cannot resolve model {model}: {e}");
            return ExitCode::from(1);
        }
    };
    let provider = Arc::clone(resolved.provider);
    let bare_model = resolved.model.clone();
    let qualified_model = model.to_owned();

    // The buffer must exist before we build the plugin runtime so we can
    // hand the runtime a sink that routes notify/log into the buffer
    // instead of stderr (which would corrupt the alt screen).
    let buffer = shared_buffer();
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let plugin_runtime = match crate::plugins_dir() {
        Ok(dir) => match setup_runtime_with_sink(
            &dir,
            &workdir,
            model,
            system,
            buffer_host_log(buffer.clone()),
        ) {
            Ok(rt) => rt,
            Err(e) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.push_custom("kage:error", e, false);
                }
                None
            }
        },
        Err(e) => {
            if let Ok(mut buf) = buffer.lock() {
                buf.push_custom("kage:error", e, false);
            }
            None
        }
    };

    let mut tools = kage_tools::builtin_registry();
    if let Some(rt) = plugin_runtime.as_ref() {
        for tool in rt.registered_tools() {
            tools.register(tool);
        }
    }
    let cancel = CancelFlag::new();
    let cx = Arc::new(Mutex::new(
        AgentContext::new(bare_model, system).with_workdir(&workdir),
    ));
    let (tx, rx) = mpsc::channel::<RunRequest>();

    let worker = spawn_worker(WorkerConfig {
        provider,
        tools,
        cx: Arc::clone(&cx),
        buffer: buffer.clone(),
        cancel: cancel.clone(),
        plugin_runtime,
        rx,
        qualified_model: qualified_model.clone(),
    });

    let mut tui = match Tui::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("kage: failed to enter raw mode: {e}");
            return ExitCode::from(1);
        }
    };
    let mut app = App::new(buffer, tx);
    let result = app.run(&mut tui);
    drop(tui);
    drop(app);
    let _ = worker.join();

    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kage: tui error: {e}");
            ExitCode::from(1)
        }
    }
}

struct WorkerConfig {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    cx: Arc<Mutex<AgentContext>>,
    buffer: SharedBuffer,
    cancel: CancelFlag,
    plugin_runtime: Option<Arc<PluginRuntime>>,
    rx: mpsc::Receiver<RunRequest>,
    qualified_model: String,
}

fn spawn_worker(cfg: WorkerConfig) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let WorkerConfig {
            provider,
            tools,
            cx,
            buffer,
            cancel,
            plugin_runtime,
            rx,
            qualified_model,
        } = cfg;
        let loop_cfg = LoopConfig::default();

        for req in rx {
            match req {
                RunRequest::Submit(text) => {
                    cancel.reset();
                    let mut cx_guard = cx.lock().expect("agent context mutex poisoned");
                    let parent = cx_guard.history.last().map(|m| m.id);
                    cx_guard.history.push(Message::new(
                        Role::User,
                        vec![Content::Text { text }],
                        parent,
                    ));
                    let ok = if let Some(rt) = plugin_runtime.as_ref() {
                        let inner = TuiHooks::new(NoopHooks, buffer.clone());
                        let mut hooks = PluginEventHooks::new(inner, Arc::clone(rt));
                        hooks.dispatch_agent_start();
                        let res = run(
                            provider.as_ref(),
                            &tools,
                            &mut cx_guard,
                            &loop_cfg,
                            &mut hooks,
                            &cancel,
                            |_| {},
                        );
                        hooks.dispatch_agent_end(res.is_ok());
                        res.is_ok()
                    } else {
                        let mut hooks = TuiHooks::new(NoopHooks, buffer.clone());
                        run(
                            provider.as_ref(),
                            &tools,
                            &mut cx_guard,
                            &loop_cfg,
                            &mut hooks,
                            &cancel,
                            |_| {},
                        )
                        .is_ok()
                    };
                    if ok {
                        crate::state::record_last_model(&qualified_model);
                    }
                }
                RunRequest::Cancel => cancel.cancel(),
            }
        }
    })
}
