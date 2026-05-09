//! Modal raw-view terminal UI with differential rendering.

pub mod app;
pub mod buffer;
pub mod cmdline;
pub mod error;
pub mod events;
pub mod hostlog;
pub mod input;
pub mod layout;
pub mod overlay;
pub mod picker;
pub mod syntax;
pub mod terminal;
pub mod theme;
pub mod usage;
pub mod view;

pub use app::{App, AppExit, RunRequest, SessionLister};
pub use buffer::{Block, Buffer};
pub use cmdline::{CommandLine, CommandLineEvent};
pub use error::TuiError;
pub use events::populate_from_history;
pub use events::{SharedBuffer, TuiHooks, shared_buffer};
pub use hostlog::buffer_host_log;
pub use input::{HISTORY_MAX, InputAction, InputState, Mode, Pane};
pub use layout::{
    INPUT_CHROME_LINES, INPUT_CONTENT_MAX_LINES, INPUT_CONTENT_MIN_LINES, INPUT_MAX_LINES,
    INPUT_MIN_LINES, Regions, STATUS_BOTTOM_LINES_DEFAULT, input_height_for, split,
};
pub use overlay::{OverlayPicker, PickerEvent};
pub use picker::{PickItem, pick};
pub use terminal::Tui;
pub use theme::{Theme, current as current_theme, set_current as set_current_theme};
pub use usage::{SessionUsage, SharedSessionUsage, shared_session_usage};
pub use view::StatusCtx;
