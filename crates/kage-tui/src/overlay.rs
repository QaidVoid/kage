//! In-TUI overlay surfaces.
//!
//! Overlays are interactive modals painted over the conversation
//! buffer: model picker, session picker, slash command palette,
//! settings dialog, login dialog. Every one of them implements
//! [`OverlayWidget`], the shared render/input contract the App drives
//! directly.
//!
//! Unlike the standalone [`crate::picker::pick`] (which owns the
//! terminal in raw mode for one-shot prompts like `kage auth login`),
//! these overlays run *inside* the App's render loop: they draw into
//! a sub-rect over the conversation buffer using ratatui widgets, and
//! consume key events through the same path the rest of the App uses.
//!
//! The [`ApprovalPanel`] is the exception: it paints into the input
//! region in place of the input box, never over the conversation.

pub mod approval;
pub mod completion;
pub mod confirm;
pub mod context_menu;
pub mod editor;
pub mod help;
pub mod input;
pub mod picker;
pub mod session_tree;
pub mod settings;
pub mod slash;
pub mod widget;

pub use approval::{ApprovalOutcome, ApprovalPanel};
pub use completion::{CompletionAction, InputCompletion, file_completions, prefix_before_cursor};
pub use confirm::{Choice, ConfirmOverlay};
pub use context_menu::{ContextAction, ContextMenu, ContextMenuOutcome};
pub use editor::EditorOverlay;
pub use help::HelpOverlay;
pub use input::InputOverlay;
pub use picker::OverlayPicker;
pub use session_tree::{SessionNode, SessionTreeOverlay, SessionTreeSource};
pub use settings::{SettingsInit, SettingsOverlay};
pub use slash::{SlashContext, SlashPalette};
pub use widget::{EmptyOverlayWidget, OverlayAction, OverlayCtx, OverlayWidget};
