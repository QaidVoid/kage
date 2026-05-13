//! [`Tool`] trait and per-call execution context.

use std::path::Path;
use std::sync::Arc;

use kage_core::{CancelFlag, Risk, ToolOutput, ToolUpdate};

use crate::ToolError;

/// Sink the dispatcher hands to a [`ToolContext`] so a long-running tool
/// can stream progress without blocking on the loop's emit closure.
///
/// Implementations must be `Send + Sync` because parallel dispatch hands
/// the same sink to multiple worker threads. The loop's built-in sink
/// translates each call into a [`kage_core::LoopEvent::ToolUpdate`].
pub trait ProgressSink: Send + Sync {
    /// Report one progress payload to the dispatcher.
    fn emit(&self, update: ToolUpdate);
}

/// Per-tool override for dispatch ordering.
///
/// The loop runs tools in parallel when [`crate::LoopConfig::parallel_tools`]
/// is true. A tool returning [`ExecMode::Sequential`] from
/// [`Tool::execution_mode`] forces its batch to fall back to sequential
/// even when parallelism is enabled, so two such tools never race for a
/// shared resource (e.g. the user's terminal in `bash`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecMode {
    /// Run sequentially regardless of the loop-wide parallel flag.
    Sequential,
    /// Run in parallel when the loop permits it. Same effect as returning
    /// `None` from [`Tool::execution_mode`]; provided for explicitness.
    Parallel,
}

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

    /// Override the loop's parallel-vs-sequential dispatch decision.
    ///
    /// Returning `Some(ExecMode::Sequential)` forces any batch that
    /// includes this tool to run sequentially, even when the loop's
    /// `parallel_tools` flag is set. Use this for tools that own a
    /// non-shareable resource (the user's terminal, a global lock).
    /// Default `None` means "follow the loop config."
    fn execution_mode(&self) -> Option<ExecMode> {
        None
    }

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
/// the tool should poll at safe points for long-running work, and an
/// optional [`ProgressSink`] long-running tools call to stream progress.
#[derive(Clone)]
pub struct ToolContext<'a> {
    workdir: &'a Path,
    cancel: &'a CancelFlag,
    progress: Option<Arc<dyn ProgressSink>>,
}

impl std::fmt::Debug for ToolContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("workdir", &self.workdir)
            .field("has_progress", &self.progress.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> ToolContext<'a> {
    /// Construct a context with no progress sink.
    #[must_use]
    pub fn new(workdir: &'a Path, cancel: &'a CancelFlag) -> Self {
        Self {
            workdir,
            cancel,
            progress: None,
        }
    }

    /// Attach a progress sink so [`Self::update`] can emit mid-execution
    /// progress events. Without this the method is a no-op.
    #[must_use]
    pub fn with_progress(mut self, sink: Arc<dyn ProgressSink>) -> Self {
        self.progress = Some(sink);
        self
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

    /// Report mid-execution progress to the host. Long-running tools call
    /// this with structured progress (e.g. `12/45 crates compiled`); the
    /// dispatcher wraps each call in a `LoopEvent::ToolUpdate`. Without an
    /// attached sink this is a no-op so unit tests do not need to wire one.
    pub fn update(&self, update: ToolUpdate) {
        if let Some(sink) = &self.progress {
            sink.emit(update);
        }
    }
}
