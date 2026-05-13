//! In-TUI overlay surfaces.
//!
//! Overlays are interactive modals painted over the conversation
//! buffer: model picker, session picker, slash command palette,
//! future settings dialog, login dialog, plugin-supplied custom
//! overlays. Every one of them implements [`OverlayWidget`] so the
//! upcoming registry (PO.5) can dispatch through one trait surface
//! and `ui.custom(...)` in PE.B can accept plugin factories.
//!
//! Unlike the standalone [`crate::picker::pick`] (which owns the
//! terminal in raw mode for one-shot prompts like `kage auth login`),
//! these overlays run *inside* the App's render loop: they draw into
//! a sub-rect over the conversation buffer using ratatui widgets, and
//! consume key events through the same path the rest of the App uses.

pub mod picker;
pub mod widget;

pub use picker::OverlayPicker;
pub use widget::{EmptyOverlayWidget, OverlayAction, OverlayCtx, OverlayWidget};
