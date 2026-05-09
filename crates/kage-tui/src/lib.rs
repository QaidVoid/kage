//! Modal raw-view terminal UI with differential rendering.

pub mod app;
pub mod buffer;
pub mod error;
pub mod events;
pub mod input;
pub mod layout;
pub mod terminal;
pub mod view;

pub use app::{App, AppExit, RunRequest};
pub use buffer::{Block, Buffer};
pub use error::TuiError;
pub use events::{SharedBuffer, TuiHooks, shared_buffer};
pub use input::{InputAction, InputState, Mode};
pub use layout::{INPUT_MAX_LINES, INPUT_MIN_LINES, Regions, input_height_for, split};
pub use terminal::Tui;
