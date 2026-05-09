//! Modal raw-view terminal UI with differential rendering.

pub mod buffer;
pub mod error;
pub mod layout;
pub mod terminal;

pub use buffer::{Block, Buffer};
pub use error::TuiError;
pub use layout::{INPUT_MAX_LINES, INPUT_MIN_LINES, Regions, input_height_for, split};
pub use terminal::Tui;
