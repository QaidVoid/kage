//! [`Tool`] trait and per-call execution context.

use std::path::Path;

use kage_core::{CancelFlag, Risk, ToolOutput};

use crate::ToolError;

/// One tool the agent can invoke.
///
/// Implementations are synchronous, `Send + Sync`, and `Debug` so they can
/// live inside `Arc<dyn Tool>` registry entries. They build a JSON Schema
/// describing their input via [`Tool::schema`] (typically derived through
/// `schemars` on a strongly-typed input struct), declare a coarse risk
/// classification, and run blocking work inside [`Tool::execute`].
pub trait Tool: Send + Sync + std::fmt::Debug {
    /// Stable name. Lowercase `snake_case`, matches what the model invokes.
    fn name(&self) -> &str;

    /// Short, model-readable description. Shown to the model verbatim.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's input shape.
    fn schema(&self) -> serde_json::Value;

    /// Coarse risk classification used by hosts to gate permission prompts.
    fn risk(&self) -> Risk;

    /// Run the tool, returning either a structured output or a typed error.
    ///
    /// User-visible failures (file missing, command exited non-zero, etc.)
    /// should be reported by returning `Ok(ToolOutput { is_error: true, .. })`.
    /// Reserve [`ToolError`] for issues the loop needs to surface differently.
    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError>;
}

/// Per-call execution context handed to [`Tool::execute`].
///
/// Carries the working directory the tool must respect, a cancellation flag
/// the tool should poll at safe points for long-running work, and (in later
/// phases) handles to the sandbox and progress sink.
#[derive(Debug)]
pub struct ToolContext<'a> {
    workdir: &'a Path,
    cancel: &'a CancelFlag,
}

impl<'a> ToolContext<'a> {
    /// Construct a context.
    #[must_use]
    pub fn new(workdir: &'a Path, cancel: &'a CancelFlag) -> Self {
        Self { workdir, cancel }
    }

    /// The directory tools must scope all filesystem operations under.
    #[must_use]
    pub fn workdir(&self) -> &Path {
        self.workdir
    }

    /// Whether the caller has requested cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Return the cancellation flag, for tools that need to clone or share it.
    #[must_use]
    pub fn cancel_flag(&self) -> &CancelFlag {
        self.cancel
    }
}
