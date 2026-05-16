//! Sandboxed Lua plugin runtime and extension API.
//!
//! Plugins extend kage at runtime in Lua. The host loads a [`PluginRuntime`]
//! per process, evaluates plugin scripts against it, and dispatches loop
//! events through the runtime so plugins can react.

pub mod acp;
pub mod api;
pub mod autocomplete;
pub mod bridge;
pub mod chrome;
pub mod commands;
pub mod error;
pub mod events;
pub mod fs;
pub mod http;
pub mod keybindings;
pub mod lifecycle;
pub mod loader;
pub mod mcp;
pub mod messages;
pub mod providers;
pub mod runtime;
pub mod sessions;
pub mod status;
pub mod terminal_input;
pub mod theme;
pub mod tools;
pub mod ui;
pub mod watcher;
pub mod widgets;

pub use api::{HostLog, LogLevel, SharedHostLog, StderrHostLog, default_host_log};
pub use autocomplete::{AutocompleteItem, LuaAutocompleteProvider};
pub use bridge::{BridgeStep, SharedBridge, SuspendRequest};
pub use chrome::{ChromeAttrs, ChromeLine, ChromeSlot, ChromeSpan, LuaChrome, SharedChrome};
pub use commands::{BridgeArgs, BridgePrep, CommandOutput, LuaCommand, PluginArgSpec};
pub use error::PluginError;
pub use events::{DiscoveryEntries, KNOWN_EVENTS, SessionOpDecision};
pub use keybindings::LuaKeybinding;
pub use lifecycle::{SharedCompactRequest, SharedUsage};
pub use loader::{LoadReport, load_dir};
pub use messages::{PendingMessage, PendingRole, SharedPendingMessages};
pub use providers::LuaProvider;
pub use runtime::{PluginRuntime, PluginRuntimeBuilder, SANDBOX_REMOVALS, SharedLua};
pub use sessions::{PendingSessionOp, SharedForkRequest, SharedSessionList, SharedSessionOps};
pub use status::SharedStatus;
pub use terminal_input::{LuaTerminalHook, RegisteredTerminalHooks};
pub use theme::{SharedThemeRequest, SharedThemeState, ThemeState};
pub use tools::LuaTool;
pub use ui::{ConfirmRequest, EditorRequest, InputRequest, SelectItem, SelectRequest};
pub use watcher::PluginWatcher;
pub use widgets::LuaWidget;
