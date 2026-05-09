//! Modal raw-view terminal UI with differential rendering.

pub mod app;
pub mod buffer;
pub mod error;
pub mod events;
pub mod hostlog;
pub mod input;
pub mod layout;
pub mod overlay;
pub mod picker;
pub mod terminal;
pub mod view;

pub use app::{App, AppExit, RunRequest};
pub use buffer::{Block, Buffer};
pub use error::TuiError;
pub use events::{SharedBuffer, TuiHooks, shared_buffer};
pub use hostlog::buffer_host_log;
pub use input::{HISTORY_MAX, InputAction, InputState, Mode};
pub use layout::{INPUT_MAX_LINES, INPUT_MIN_LINES, Regions, input_height_for, split};
pub use overlay::{OverlayPicker, PickerEvent};
pub use picker::{PickItem, pick};
pub use terminal::Tui;
