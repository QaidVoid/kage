//! Interactive TUI mode for kage.
//!
//! [`run_tui`] runs [`kage_tui::App`] on the main thread against an ACP
//! agent: the pump in [`acp`] owns the wire, the App's requests become
//! client calls, and the agent's frames flow back to the App over a
//! channel.

pub(crate) use std::path::PathBuf;
pub(crate) use std::process::ExitCode;
pub(crate) use std::sync::mpsc;
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::thread;

pub(crate) use kage_core::{Content, sync::lock};
pub(crate) use kage_loop::LoopConfig;
pub(crate) use kage_plugin::{
    BridgePrep, BridgeStep, CommandOutput, ConfirmRequest, EditorRequest, InputRequest,
    PluginRuntime, SelectRequest, SwitchTarget,
};
pub(crate) use kage_provider::ProviderRegistry;
pub(crate) use kage_session::SessionSummary;
pub(crate) use kage_tui::{
    App, PickItem, PluginDialog, PluginRefresh, RunRequest, Tui, buffer_host_log, shared_buffer,
    shared_session_usage, shared_toasts,
};

pub(crate) use crate::plugins::setup_user_runtime;

mod acp;
mod entry;
mod pipe;
mod support;

pub use entry::run_tui;

pub(crate) use entry::startup_options;

pub(crate) use support::{
    Mirror, available_model_items, external_agent_items, list_session_choices, list_session_nodes,
    register_block_renderers, run_bridged_command, run_bridged_keymap, snapshot_plugin_commands,
    start_notices,
};

#[cfg(test)]
pub(crate) use crate::engine::render_session_markdown;

// Internal helpers the test module exercises directly.
#[cfg(test)]
pub(crate) use support::{format_session_label, relative_day};

#[cfg(test)]
mod tests;
