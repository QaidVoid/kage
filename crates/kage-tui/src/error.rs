//! Errors raised by the TUI runtime.

/// Anything that can go wrong while owning the terminal.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// `crossterm` returned an I/O error while changing terminal state.
    #[error("terminal io: {0}")]
    Io(#[from] std::io::Error),
}
