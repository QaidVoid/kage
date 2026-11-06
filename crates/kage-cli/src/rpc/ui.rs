//! The plugin UI of a project or a session, for clients that draw it.
//!
//! Plugins and `init.lua` run on the host, in a project's runtime. A
//! client that draws their keymaps, commands, highlights, options and
//! status asks for them with `_kage/ui/attach`, which also keeps the
//! runtime loaded while the client is attached, and pins the watcher
//! to a session's runtime when the attach names one. When something
//! changes, the client gets the parts that did as a `_kage/ui/update`
//! tagged with the attachment's id, at most every 200 ms.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use kage_acp::acp::{UiCommand, UiCommandArg, UiDialogRequest, UiOutput, UiSnapshot, UiUpdate};
use kage_acp::agent::AgentPeer;
use kage_core::environment::Environment;
use kage_core::options::{OPTIONS, option_to_json};
use kage_core::sync::lock;
use kage_jsonrpc::RpcError;
use kage_plugin::{
    BridgePrep, BridgeStep, CommandOutput, LuaCommand, PluginArgSpec, PluginRuntime,
};

use super::host::Host;

/// How often a watcher looks for a change to send.
const WATCH: Duration = Duration::from_millis(200);
/// Every this many looks, a watcher also asks whether the project's
/// plugins, `init.lua` or config changed, which loads them again.
const RELOAD_EVERY: u32 = 5;

/// What moves whenever the snapshot would change, cheap to read.
#[derive(PartialEq, Eq)]
pub(super) struct Marker {
    keymap: u64,
    highlights: u64,
    options: u64,
    status: BTreeMap<String, String>,
    commands: Vec<UiCommand>,
    registered: [usize; 3],
}

/// The marker of `runtime` as it stands now. Each table is locked on its
/// own, never two at once.
pub(super) fn marker(runtime: &PluginRuntime) -> Marker {
    let keymap = lock(&runtime.keymap()).generation();
    let highlights = lock(&runtime.highlights()).generation();
    let options = lock(&runtime.options()).generation();
    let status = lock(&runtime.shared_status()).clone();
    let hooks = lock(&runtime.shared_terminal_hooks()).len();
    Marker {
        keymap,
        highlights,
        options,
        status,
        commands: commands(runtime),
        registered: [
            runtime.registered_block_renderers().len(),
            runtime.registered_autocomplete_providers().len(),
            hooks,
        ],
    }
}

/// What a watcher needs to follow one plugin runtime for one client.
pub(super) struct Watch {
    pub(super) host: Arc<Host>,
    pub(super) cwd: String,
    pub(super) env: Environment,
    pub(super) runtime: Arc<PluginRuntime>,
    /// Whether the watcher follows the project's latest loaded
    /// runtime. A watcher pinned to a session stays on the runtime its
    /// session opened with, so what the UI advertises is what handles
    /// the session's commands and mappings.
    pub(super) reload: bool,
    /// The attachment id this watcher's updates carry.
    pub(super) attach_id: u64,
    pub(super) marker: Marker,
    pub(super) sent: UiSnapshot,
    pub(super) peer: AgentPeer,
    pub(super) stop: Arc<AtomicBool>,
}

/// Send `watch.peer` what changes in the project's plugin UI after
/// `watch.sent`, as `_kage/ui/update`, until `watch.stop` is set or the
/// peer is gone. The watcher holds the project, so it stays loaded, and
/// moves to a freshly loaded runtime when the project's plugins change.
pub(super) fn watch(watch: Watch) {
    let _ = thread::Builder::new()
        .name("kage-rpc-ui".to_owned())
        .spawn(move || run_watch(watch));
}

fn run_watch(mut watch: Watch) {
    let mut looks = 0u32;
    while !watch.stop.load(Ordering::SeqCst) {
        thread::sleep(WATCH);
        looks += 1;
        let mut moved = false;
        if watch.reload
            && looks.is_multiple_of(RELOAD_EVERY)
            && let Some(current) = watch.host.project(Path::new(&watch.cwd), &watch.env)
            && !Arc::ptr_eq(&current.runtime, &watch.runtime)
        {
            watch.runtime = current.runtime;
            moved = true;
        }
        let now = marker(&watch.runtime);
        if !moved && now == watch.marker {
            continue;
        }
        watch.marker = now;
        let mut fresh = snapshot(&watch.runtime);
        fresh.generation = watch.sent.generation + 1;
        let Some(update) = changes(&watch.cwd, watch.attach_id, &watch.sent, &fresh) else {
            continue;
        };
        let Ok(params) = serde_json::to_value(&update) else {
            continue;
        };
        // A client that attached again has a new watcher; this one stops
        // without sending what it computed for the old attachment.
        if watch.stop.load(Ordering::SeqCst) {
            return;
        }
        if watch.peer.notify("_kage/ui/update", params).is_err() {
            return;
        }
        watch.sent = fresh;
    }
}

/// What differs between `sent` and `fresh`, or `None` when nothing does.
/// The update names `attach_id`, the attachment that started the
/// watcher that measured it.
fn changes(cwd: &str, attach_id: u64, sent: &UiSnapshot, fresh: &UiSnapshot) -> Option<UiUpdate> {
    fn changed<T: PartialEq + Clone>(sent: &T, fresh: &T) -> Option<T> {
        (sent != fresh).then(|| fresh.clone())
    }
    let update = UiUpdate {
        cwd: cwd.to_owned(),
        attach_id,
        generation: fresh.generation,
        keymap: changed(&sent.keymap, &fresh.keymap),
        highlights: changed(&sent.highlights, &fresh.highlights),
        options: changed(&sent.options, &fresh.options),
        option_sources: changed(&sent.option_sources, &fresh.option_sources),
        commands: changed(&sent.commands, &fresh.commands),
        status: changed(&sent.status, &fresh.status),
        block_kinds: changed(&sent.block_kinds, &fresh.block_kinds),
        autocomplete: changed(&sent.autocomplete, &fresh.autocomplete),
        terminal_hooks: changed(&sent.terminal_hooks, &fresh.terminal_hooks),
    };
    let unchanged = UiUpdate {
        cwd: update.cwd.clone(),
        attach_id: update.attach_id,
        generation: update.generation,
        ..UiUpdate::default()
    };
    (update != unchanged).then_some(update)
}

/// What `runtime` declares for a client to draw, as it stands now.
pub(super) fn snapshot(runtime: &PluginRuntime) -> UiSnapshot {
    let keymap = lock(&runtime.keymap())
        .to_data()
        .iter()
        .filter_map(|mapping| serde_json::to_value(mapping).ok())
        .collect();
    let highlights = serde_json::to_value(&*lock(&runtime.highlights())).unwrap_or_default();
    let (options, option_sources) = {
        let store = runtime.options();
        let store = lock(&store);
        let options = OPTIONS
            .iter()
            .filter_map(|def| {
                let value = store.get(def.name)?;
                Some((def.name.to_owned(), option_to_json(value)))
            })
            .collect();
        let sources = OPTIONS
            .iter()
            .filter_map(|def| {
                let source = store.source(def.name)?;
                Some((def.name.to_owned(), source.as_str().to_owned()))
            })
            .collect();
        (options, sources)
    };
    let commands = commands(runtime);
    UiSnapshot {
        generation: 0,
        keymap,
        highlights,
        options,
        option_sources,
        commands,
        status: lock(&runtime.shared_status()).clone(),
        block_kinds: runtime
            .registered_block_renderers()
            .iter()
            .map(|renderer| renderer.kind().to_owned())
            .collect(),
        autocomplete: !runtime.registered_autocomplete_providers().is_empty(),
        terminal_hooks: !lock(&runtime.shared_terminal_hooks()).is_empty(),
    }
}

/// Run the plugin command `name` with the argument line `args`, asking
/// `peer` for each dialog it opens. The caller sets the invocation.
pub(super) fn run_command(
    runtime: &PluginRuntime,
    name: &str,
    args: &str,
    peer: &AgentPeer,
) -> Result<UiOutput, RpcError> {
    let command = runtime
        .registered_command_overrides()
        .into_iter()
        .chain(runtime.registered_commands())
        .find(|command| command.name() == name || command.aliases().iter().any(|a| a == name))
        .ok_or_else(|| RpcError::new(-32602, format!("no plugin command {name}")))?;
    let failed = |e: &dyn std::fmt::Display| RpcError::internal(format!("command {name}: {e}"));
    let bridged = match command
        .prepare_bridge(args, &serde_json::Value::Null)
        .map_err(|e| failed(&e))?
    {
        BridgePrep::Ready(bridged) => bridged,
        BridgePrep::ArgError(output) => return Ok(ui_output(output)),
    };
    let (call, step) = runtime
        .bridge_start(&bridged.handler, &bridged.args)
        .map_err(|e| failed(&e))?;
    drive(runtime, call, step, peer).map_err(|e| failed(&e))
}

/// Run the Lua handler `id` of a key mapping, asking `peer` for each
/// dialog it opens. The caller sets the invocation.
pub(super) fn run_keymap(
    runtime: &PluginRuntime,
    id: u64,
    peer: &AgentPeer,
) -> Result<UiOutput, RpcError> {
    let failed = |e: &dyn std::fmt::Display| RpcError::internal(format!("key mapping: {e}"));
    let handler = runtime.keymap_handler(id).map_err(|e| failed(&e))?;
    let (call, step) = runtime
        .bridge_start(&handler, &[])
        .map_err(|e| failed(&e))?;
    drive(runtime, call, step, peer).map_err(|e| failed(&e))
}

/// Drive the bridged `call` to its end, asking `peer` for every dialog. A
/// client that cannot answer dismisses the dialog.
fn drive(
    runtime: &PluginRuntime,
    call: u64,
    mut step: BridgeStep,
    peer: &AgentPeer,
) -> Result<UiOutput, kage_plugin::PluginError> {
    loop {
        match step {
            BridgeStep::Done(value) => return Ok(ui_output(CommandOutput::from_json(&value))),
            BridgeStep::Suspended(request) => {
                let dialog = UiDialogRequest {
                    kind: request.kind,
                    payload: request.payload,
                };
                let params = serde_json::to_value(&dialog).unwrap_or_default();
                let resumed = match peer.request("_kage/ui/dialog", params) {
                    Ok(answer) if !answer.is_null() => runtime.bridge_resume_call(call, &answer),
                    // The client is gone: nothing could answer the next
                    // dialog either, so the call ends here.
                    Err(e) if e.is_connection_closed() => {
                        let _ = runtime.bridge_abort_call(call);
                        return Ok(UiOutput {
                            text: "the client went away".to_owned(),
                            is_error: true,
                            structured: None,
                        });
                    }
                    Ok(_) | Err(_) => runtime.bridge_cancel_call(call),
                };
                step = match resumed {
                    Ok(step) => step,
                    Err(e) => {
                        let _ = runtime.bridge_abort_call(call);
                        return Err(e);
                    }
                };
            }
        }
    }
}

fn ui_output(output: CommandOutput) -> UiOutput {
    UiOutput {
        text: output.text,
        is_error: output.is_error,
        structured: output.structured,
    }
}

/// The commands `runtime` registered, then the ones it overrides.
fn commands(runtime: &PluginRuntime) -> Vec<UiCommand> {
    runtime
        .registered_commands()
        .iter()
        .map(|command| ui_command(command, false))
        .chain(
            runtime
                .registered_command_overrides()
                .iter()
                .map(|command| ui_command(command, true)),
        )
        .collect()
}

fn ui_command(command: &LuaCommand, overrides: bool) -> UiCommand {
    UiCommand {
        name: command.name().to_owned(),
        aliases: command.aliases().to_vec(),
        description: command.description().to_owned(),
        args: command.args().iter().map(ui_arg).collect(),
        overrides,
    }
}

fn ui_arg(arg: &PluginArgSpec) -> UiCommandArg {
    match arg.clone() {
        PluginArgSpec::Text {
            name,
            optional,
            hint,
        } => UiCommandArg::Text {
            name,
            optional,
            hint,
        },
        PluginArgSpec::Choice {
            name,
            values,
            optional,
        } => UiCommandArg::Choice {
            name,
            values,
            optional,
        },
        PluginArgSpec::Path { name, optional } => UiCommandArg::Path { name, optional },
        PluginArgSpec::Session { name, optional } => UiCommandArg::Session { name, optional },
        PluginArgSpec::Flag { name } => UiCommandArg::Flag { name },
    }
}
