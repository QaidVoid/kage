//! The shell's panels and the composer, thin over the store.
//!
//! Each view reads [`kage_client`] state through the store entity and
//! sends commands back through it. No view parses frames or names
//! wire types beyond the transcript items it renders.

pub(crate) mod agents;
pub mod approval;
pub mod chrome;
pub mod composer;
pub(crate) mod constellation;
pub mod deferred;
pub mod dialog;
pub mod dock;
pub(crate) mod eclipse;
pub(crate) mod kit;
pub mod settings;
pub(crate) mod settings_config;
#[cfg(not(target_arch = "wasm32"))]
pub mod setup;
pub mod sidebar;
pub mod transcript;
pub mod vim;
pub mod workbench;

pub use approval::{ApprovalCard, ApprovalEvent};
pub use chrome::{FindBar, PaletteView, Toasts, WelcomeView};
pub use composer::ComposerView;
pub use dock::{DockEvent, DockRow};
pub use sidebar::SidebarView;
pub use transcript::{TranscriptEvent, TranscriptView};
pub use workbench::{WorkbenchEvent, WorkbenchView};
