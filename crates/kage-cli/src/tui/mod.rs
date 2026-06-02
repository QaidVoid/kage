//! Interactive TUI mode for kage.
//!
//! [`run_tui`] glues [`kage_tui::App`] (main thread) to a worker thread
//! that runs the agent loop. Submitted prompts arrive via an `mpsc`
//! channel; for each prompt the worker pushes the user message into
//! [`AgentContext::history`], runs the loop with [`TuiHooks`] mirroring
//! events into the shared buffer, and resets the cancel flag for the
//! next turn.

pub(crate) use std::path::PathBuf;
pub(crate) use std::process::ExitCode;
pub(crate) use std::sync::mpsc;
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::thread;

pub(crate) use kage_core::{CancelFlag, Content, Message, Role};
pub(crate) use kage_loop::{AgentContext, LoopConfig, NoopHooks, force_compact, run};
pub(crate) use kage_mcp::McpManager;
pub(crate) use kage_plugin::{
    BridgePrep, BridgeStep, CommandOutput, ConfirmRequest, EditorRequest, InputRequest,
    PluginRuntime, SelectRequest, SwitchTarget,
};
pub(crate) use kage_provider::ProviderRegistry;
pub(crate) use kage_session::{SessionId, SessionReader, SessionSummary, SessionWriter};
pub(crate) use kage_tools::ToolRegistry;
pub(crate) use kage_tui::{
    App, PickItem, PluginDialog, RunRequest, SharedBuffer, SharedSessionUsage, SharedToasts, Toast,
    Tui, TuiHooks, buffer_host_log, populate_from_history, push_toast, shared_buffer,
    shared_session_usage, shared_toasts,
};

pub(crate) use crate::plugins::{PluginEventHooks, setup_runtime_with_sink};
pub(crate) use crate::session::SessionRecordingHooks;

pub(crate) struct WorkerConfig {
    registry: Arc<ProviderRegistry>,
    active_qualified: Arc<Mutex<String>>,
    tools: ToolRegistry,
    /// Owns the session's MCP child processes and mediates their
    /// tools into `tools`. Drained for restart / hot-refresh between
    /// turns; kept here so it lives exactly as long as the worker.
    mcp_manager: McpManager,
    cx: Arc<Mutex<AgentContext>>,
    buffer: SharedBuffer,
    cancel: CancelFlag,
    plugin_runtime: Option<Arc<PluginRuntime>>,
    rx: mpsc::Receiver<RunRequest>,
    /// Path to the session file the worker appends to. Shared with
    /// the resume handler so `Ctrl+R` can swap the file in place.
    session_path: Option<Arc<Mutex<PathBuf>>>,
    /// Header to write the first time the planned session file is
    /// created. After creation, this is taken (`Some(_) -> None`) and
    /// subsequent turns reopen the existing file in append mode.
    session_header: Option<Arc<Mutex<Option<kage_session::Header>>>>,
    /// Shared session-usage snapshot the modeline reads from. The
    /// worker updates it after every turn from `cx.budget` so the
    /// modeline reflects live token totals without polling.
    session_usage: SharedSessionUsage,
    /// Shared toast queue. The worker pushes into it for model
    /// switches, session resume confirmations, and other async
    /// notifications that should appear as overlays rather than
    /// inline conversation noise.
    toasts: SharedToasts,
    /// Sender for blocking plugin dialogs (`kage.ui.select`). The
    /// worker forwards a suspended coroutine's dialog request here and
    /// parks on a per-request reply channel until the App answers.
    dialog_tx: mpsc::Sender<PluginDialog>,
    /// Loop tuning resolved from user/project config at startup.
    loop_cfg: LoopConfig,
    /// Shared FIFO of user prompts the App pushes when a `Submit`
    /// lands mid-run. The worker hands a clone to [`TuiHooks`] so the
    /// agent loop drains it at every turn boundary, and re-queues any
    /// leftovers after each run via [`Self::tx_self`].
    steering: kage_tui::SharedSteering,
    /// Sender on the worker's own request channel. Used to re-submit
    /// any prompts left in the steering queue after a run finishes so
    /// the next iteration of the worker loop picks them up as fresh
    /// `Submit`s instead of stranding the user's input.
    tx_self: mpsc::Sender<RunRequest>,
    /// Directory the plugin runtime loaded from at startup. Used by
    /// the `ReloadPlugins` handler to call `runtime.reload_dir(...)`.
    /// `None` when no plugin dir resolved (e.g. `$XDG_CONFIG_HOME`
    /// unset and `$HOME` missing); reloads become no-ops.
    plugins_dir: Option<PathBuf>,
}

mod entry;
mod session_ops;
mod support;
mod worker;

pub use entry::run_tui;

pub(crate) use session_ops::{
    available_model_items, handle_clone, handle_delete_session, handle_export, handle_fork_file,
    handle_new, handle_plugin_fork, handle_resume,
};
pub(crate) use support::{
    consult_session_op, find_last_entry, first_text_of, list_session_choices, list_session_nodes,
    open_writer_for_turn, push_error, refresh_session_entries, resolve_switch_target,
    run_bridged_command, run_bridged_keybinding, run_compact_with_hooks, run_with_hooks,
    translate_plugin_arg, write_session_title,
};
pub(crate) use worker::spawn_worker;

// Internal helpers the test module exercises directly.
#[cfg(test)]
pub(crate) use session_ops::render_session_markdown;
#[cfg(test)]
pub(crate) use support::{format_session_label, relative_day};

#[cfg(test)]
mod tests;
