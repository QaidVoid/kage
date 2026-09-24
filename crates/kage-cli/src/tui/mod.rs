//! Interactive TUI mode for kage.
//!
//! [`run_tui`] runs [`kage_tui::App`] on the main thread against an
//! engine: the App's requests become engine commands through
//! [`host::Host`], and the engine's events flow back to the App over a
//! channel.

pub(crate) use std::path::PathBuf;
pub(crate) use std::process::ExitCode;
pub(crate) use std::sync::mpsc;
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::thread;

pub(crate) use kage_core::{Content, sync::lock};
pub(crate) use kage_loop::{AgentContext, LoopConfig};
pub(crate) use kage_plugin::{
    BridgePrep, BridgeStep, CommandOutput, ConfirmRequest, EditorRequest, InputRequest,
    PluginRuntime, SelectRequest, SwitchTarget,
};
pub(crate) use kage_provider::ProviderRegistry;
pub(crate) use kage_session::{SessionReader, SessionSummary};
pub(crate) use kage_tui::{
    App, PickItem, PluginDialog, PluginRefresh, RunRequest, Tui, buffer_host_log, shared_buffer,
    shared_session_usage, shared_toasts,
};

pub(crate) use crate::engine::Commander;
pub(crate) use crate::plugins::setup_tui_runtime;

mod entry;
mod host;
mod support;

pub use entry::run_tui;

pub(crate) use support::{
    available_model_items, list_session_choices, list_session_nodes, refresh_session_entries,
    resolve_switch_target, run_bridged_command, run_bridged_keybinding, snapshot_plugin_commands,
};

#[cfg(test)]
pub(crate) use crate::engine::render_session_markdown;

// Internal helpers the test module exercises directly.
#[cfg(test)]
pub(crate) use support::{format_session_label, relative_day};

#[cfg(test)]
mod tests;
