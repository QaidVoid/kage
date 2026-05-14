//! LLM provider abstraction and built-in implementations.
//!
//! Synchronous, iterator-based streaming. Implementations use `ureq` for
//! HTTP and parse SSE responses by reading the body line by line. There
//! is no tokio, no async-trait, and no `Pin<Box<dyn Stream>>`.

pub mod anthropic;
pub mod cancelable;
pub mod catalog;
pub mod compat;
pub mod error;
pub mod event;
pub mod gemini;
pub mod metadata;
pub mod openai;
pub mod openai_responses;
pub mod registry;
pub mod request;
pub mod tokens;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use cancelable::{cancellable_call, make_cancelable};
pub use catalog::{ModelInfo, PROVIDERS, ProviderInfo};
pub use error::ProviderError;
pub use event::{ProviderEvent, StopReason};
pub use kage_core::ToolSpec;
pub use metadata::ProviderMetadata;
pub use registry::{ProviderRegistry, ResolvedProvider};
pub use request::{StreamRequest, ThinkingConfig, ThinkingLevel};

use kage_core::CancelFlag;

/// Boxed iterator yielded by [`Provider::stream`].
///
/// Iterating it blocks on the next event. Dropping the iterator before it
/// is exhausted aborts the underlying request. The iterator is `Send` so
/// callers may run it on a worker thread.
pub type EventStream =
    Box<dyn Iterator<Item = Result<ProviderEvent, ProviderError>> + Send + 'static>;

/// LLM provider abstraction.
///
/// Implementations block synchronously inside [`Provider::stream`] until
/// the request has been accepted, then return an iterator the caller
/// drains for events. Cancellation is cooperative through `cancel`; the
/// iterator polls it at safe points (between SSE events).
pub trait Provider: Send + Sync + std::fmt::Debug {
    /// Static metadata describing this provider.
    fn metadata(&self) -> &ProviderMetadata;

    /// Issue a streaming request.
    ///
    /// Errors raised here are setup errors (auth, malformed request,
    /// unknown model). Errors that happen mid-stream are yielded as
    /// `Err(ProviderError)` items inside the returned iterator.
    fn stream(&self, req: StreamRequest, cancel: &CancelFlag)
    -> Result<EventStream, ProviderError>;
}
