//! Modal raw-view terminal UI with differential rendering.

pub mod app;
pub mod buffer;
pub mod chord;
pub mod cmdline;
pub mod cmdparse;
pub mod command;
pub mod error;
pub mod events;
pub mod hostlog;
pub mod input;
pub mod layout;
pub mod markdown;
pub mod overlay;
pub mod picker;
pub mod syntax;
pub mod terminal;
pub mod theme;
pub mod toast;
pub mod usage;
pub mod view;

pub use app::{App, AppExit, PluginDialog, RunRequest, SessionLister};
pub use buffer::{Block, Buffer};
pub use chord::Chord;
pub use cmdline::{CommandLine, CommandLineEvent};
pub use cmdparse::{
    Completion, Completions, EmptyResolver, ParseError, Resolver, complete, parse_input,
};
pub use command::{
    ArgSource, ArgSpec, ArgValue, CommandCategory, CommandSpec, ParsedArgs, find_builtin_command,
};
pub use error::TuiError;
pub use events::populate_from_history;
pub use events::{SharedBuffer, TuiHooks, shared_buffer};
pub use hostlog::buffer_host_log;
pub use input::{HISTORY_MAX, InputAction, InputState, Mode, Pane};
pub use layout::{
    INPUT_CHROME_LINES, INPUT_CONTENT_MAX_LINES, INPUT_CONTENT_MIN_LINES, INPUT_MAX_LINES,
    INPUT_MIN_LINES, Regions, STATUS_BOTTOM_LINES_DEFAULT, input_height_for, split,
};
pub use overlay::{
    OverlayAction, OverlayCtx, OverlayPicker, OverlayWidget, SessionNode, SlashContext,
    SlashPalette,
};
pub use picker::{PickItem, pick};
pub use terminal::Tui;
pub use theme::{Theme, current as current_theme, set_current as set_current_theme};
pub use toast::{
    DEFAULT_TOAST_DURATION, MAX_VISIBLE_TOASTS, SharedToasts, Toast, ToastKind, push_toast,
    shared_toasts,
};
pub use usage::{SessionUsage, SharedSessionUsage, shared_session_usage};
pub use view::{
    AssistantBlockWidget, BlockFactory, BlockRenderer, BlockWidget, BuiltinKind, EmptyBlockWidget,
    RenderCtx, SelectionState, StatusCtx, ThinkingBlockWidget, ToolPairBlockWidget,
    UserBlockWidget,
};
