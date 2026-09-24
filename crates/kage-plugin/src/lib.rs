//! Sandboxed Lua plugin runtime and extension API.
//!
//! Layering: depends on `kage-core`, `kage-provider`, and `kage-tools`.
//!
//! Plugins extend kage at runtime in Lua. The host loads a [`PluginRuntime`]
//! per process, evaluates plugin scripts against it, and dispatches loop
//! events through the runtime so plugins can react. A host may also
//! give the runtime a user dir, whose trusted `init.lua` loads after
//! every plugin. The Lua state lives
//! on one owner thread; render surfaces read retained output and never
//! wait on it (see [`PluginRuntime::redraw_flag`]).

pub mod acp;
pub mod api;
pub(crate) mod autocmd;
pub mod autocomplete;
pub mod block_renderers;
pub mod bridge;
pub(crate) mod capabilities;
pub mod chrome;
pub mod commands;
pub(crate) mod env;
pub mod error;
pub mod events;
pub(crate) mod exec;
pub mod fs;
pub mod highlight;
pub(crate) mod host;
pub mod http;
pub mod keymap;
pub mod lifecycle;
pub mod loader;
pub mod mcp;
pub mod messages;
pub(crate) mod options;
pub mod providers;
pub(crate) mod retained;
pub mod runtime;
pub(crate) mod schedule;
pub(crate) mod session_write;
pub mod sessions;
pub mod slots;
pub mod spec;
pub mod status;
pub(crate) mod stdlib;
pub(crate) mod store;
pub mod terminal_input;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub(crate) mod theme;
pub mod tools;
pub mod ui;
pub(crate) mod user;
pub mod watchdog;
pub mod watcher;
pub mod widgets;

pub use api::{HostLog, LogLevel, SharedHostLog, StderrHostLog, default_host_log};
pub use autocomplete::{AutocompleteItem, LuaAutocompleteProvider};
pub use block_renderers::{LuaBlockRenderer, SharedBlockRenderers};
pub use bridge::{BridgeStep, SharedBridge, SuspendRequest};
pub use chrome::{ChromeAttrs, ChromeLine, ChromeSpan};
pub use commands::{BridgeArgs, BridgePrep, CommandOutput, LuaCommand, PluginArgSpec};
pub use error::PluginError;
pub use events::{DiscoveryEntries, KNOWN_EVENTS, SessionOpDecision};
pub use highlight::{SharedHighlights, SharedThemeResolver, ThemeBase, ThemeResolver};
pub use keymap::SharedKeymap;
pub use lifecycle::{SharedCompactRequest, SharedUsage};
pub use loader::{LoadReport, load_all, load_dir};
pub use messages::{PendingMessage, PendingRole, SharedPendingMessages};
pub use options::SharedOptions;
pub use providers::LuaProvider;
pub use runtime::{PluginRuntime, PluginRuntimeBuilder, SANDBOX_REMOVALS};
pub use session_write::{SharedSessionEntries, SharedSwitchRequest, SwitchTarget};
pub use sessions::{PendingSessionOp, SharedForkRequest, SharedSessionList, SharedSessionOps};
pub use slots::{SharedUiState, SlotItem, SlotName, SlotSpec, SlotSpecs, Slots, UiState};
pub use status::SharedStatus;
pub use terminal_input::{LuaTerminalHook, RegisteredTerminalHooks};
pub use tools::LuaTool;
pub use ui::{ConfirmRequest, EditorRequest, InputRequest, SelectItem, SelectRequest};
pub use watcher::PluginWatcher;
pub use widgets::LuaWidget;
