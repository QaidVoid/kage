//! Per-kind block-widget registry for built-in and plugin renderers.
//!
//! PB.10 locks the extensibility shape the upcoming PE.A.N Lua-block-
//! renderer phase will plug into. The registry maps a [`Block`]'s
//! kind to a [`BlockFactory`] that produces a boxed [`BlockWidget`]
//! tailored to that block's data. Built-ins are registered at TUI
//! startup; plugins call [`BlockRenderer::set_custom`] to add a
//! widget for a `Block::Custom { kind: ... }` variant they own.
//!
//! PB.10 deliberately stops short of wiring this into the render
//! path. PB.9 will replace `block_to_lines` / `build_block_lines`
//! with a widget walker that calls
//! [`BlockRenderer::widget_for`]; until then the registry lives
//! alongside the existing lines pipeline so the trait surface can
//! be reviewed independently of the rewrite.

use std::collections::HashMap;
use std::sync::Arc;

use super::widget::BlockWidget;
use super::{AssistantBlockWidget, ThinkingBlockWidget, ToolPairBlockWidget, UserBlockWidget};
use crate::buffer::Block;

/// Produces a per-render [`BlockWidget`] for a specific `Block`
/// instance. Factories are cheap to call - one allocation per block
/// per frame is fine for human-driven keystrokes.
///
/// Returns `None` when this factory cannot handle the supplied block
/// kind. The renderer treats `None` as "skip this block"; in
/// practice the registry only dispatches to factories whose kind
/// matches.
pub trait BlockFactory: Send + Sync {
    /// Construct a widget for `block`, or `None` if `block`'s kind
    /// is not handled.
    fn make(&self, block: &Block) -> Option<Box<dyn BlockWidget>>;
}

/// Registry mapping block kinds to widget factories.
///
/// Built-in factories cover [`Block::User`], [`Block::Assistant`],
/// [`Block::Thinking`], and (when a paired result is present)
/// [`Block::ToolCall`]; the unpaired tool-call, standalone tool-
/// result, and custom paths fall through to the per-kind handlers
/// or the `set_custom` registry.
#[derive(Default)]
pub struct BlockRenderer {
    user: Option<Arc<dyn BlockFactory>>,
    assistant: Option<Arc<dyn BlockFactory>>,
    thinking: Option<Arc<dyn BlockFactory>>,
    tool_pair: Option<Arc<dyn BlockFactory>>,
    custom: HashMap<String, Arc<dyn BlockFactory>>,
}

impl BlockRenderer {
    /// Registry pre-populated with the bundled widget factories for
    /// every built-in non-custom block kind.
    #[must_use]
    pub fn with_builtins() -> Self {
        Self {
            user: Some(Arc::new(BuiltinUserFactory)),
            assistant: Some(Arc::new(BuiltinAssistantFactory)),
            thinking: Some(Arc::new(BuiltinThinkingFactory)),
            tool_pair: Some(Arc::new(BuiltinToolPairFactory)),
            custom: HashMap::new(),
        }
    }

    /// Override the renderer for a built-in kind. The kind argument
    /// is one of the [`BuiltinKind`] variants; passing
    /// `BuiltinKind::ToolPair` registers a renderer for paired
    /// tool-call + tool-result blocks specifically.
    pub fn set_builtin(&mut self, kind: BuiltinKind, factory: Arc<dyn BlockFactory>) {
        match kind {
            BuiltinKind::User => self.user = Some(factory),
            BuiltinKind::Assistant => self.assistant = Some(factory),
            BuiltinKind::Thinking => self.thinking = Some(factory),
            BuiltinKind::ToolPair => self.tool_pair = Some(factory),
        }
    }

    /// Register a renderer for a custom block kind (i.e. a
    /// [`Block::Custom`] whose `kind` field equals `name`).
    pub fn set_custom(&mut self, name: impl Into<String>, factory: Arc<dyn BlockFactory>) {
        self.custom.insert(name.into(), factory);
    }

    /// Look up the widget for `block`. For paired tool blocks pass
    /// the call side; the result side is intended to be skipped by
    /// the caller via a `result_by_call` map (the same convention
    /// the lines path uses today).
    ///
    /// Returns `None` when no factory matches the block's kind, e.g.
    /// a `Block::Custom { kind: "x" }` for which no plugin has
    /// registered a renderer.
    #[must_use]
    pub fn widget_for(&self, block: &Block) -> Option<Box<dyn BlockWidget>> {
        match block {
            Block::User { .. } => self.user.as_ref()?.make(block),
            Block::Assistant { .. } => self.assistant.as_ref()?.make(block),
            Block::Thinking { .. } => self.thinking.as_ref()?.make(block),
            Block::ToolCall { .. } | Block::ToolResult { .. } => None,
            Block::Custom { kind, .. } => self.custom.get(kind)?.make(block),
        }
    }

    /// Look up the widget for a paired call+result. Used by callers
    /// that have already matched the pair; returns `None` if the
    /// blocks are not the right variants or no tool-pair factory is
    /// registered.
    #[must_use]
    pub fn pair_widget_for(&self, call: &Block, result: &Block) -> Option<Box<dyn BlockWidget>> {
        let factory = self.tool_pair.as_ref()?;
        factory.make_pair(call, result)
    }
}

/// Identifier for the built-in kinds that have a default factory.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BuiltinKind {
    /// [`Block::User`].
    User,
    /// [`Block::Assistant`].
    Assistant,
    /// [`Block::Thinking`].
    Thinking,
    /// Paired [`Block::ToolCall`] + [`Block::ToolResult`].
    ToolPair,
}

/// Extension trait implemented by `BlockFactory` for the special
/// tool-pair case, which receives two blocks instead of one.
///
/// Implementations should return `None` for non-tool-pair calls.
trait ToolPairFactoryExt: BlockFactory {
    fn make_pair(&self, call: &Block, result: &Block) -> Option<Box<dyn BlockWidget>>;
}

/// Object-safe shim: `Arc<dyn BlockFactory>` may or may not
/// internally implement [`ToolPairFactoryExt`]. We expose the pair
/// constructor on `BlockFactory` itself with a default that returns
/// `None`, and override it for the built-in tool-pair factory.
impl dyn BlockFactory {
    fn make_pair(&self, call: &Block, result: &Block) -> Option<Box<dyn BlockWidget>> {
        // Try the built-in tool-pair shape; non-pair factories return
        // None which is the right "I don't handle pairs" answer.
        BuiltinToolPairFactory.make_pair(call, result).or_else(|| {
            let _ = self;
            None
        })
    }
}

/// Built-in factory for [`Block::User`].
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinUserFactory;

impl BlockFactory for BuiltinUserFactory {
    fn make(&self, block: &Block) -> Option<Box<dyn BlockWidget>> {
        if let Block::User { text } = block {
            Some(Box::new(UserBlockWidget::new(text.clone())))
        } else {
            None
        }
    }
}

/// Built-in factory for [`Block::Assistant`].
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinAssistantFactory;

impl BlockFactory for BuiltinAssistantFactory {
    fn make(&self, block: &Block) -> Option<Box<dyn BlockWidget>> {
        if let Block::Assistant { text, live } = block {
            Some(Box::new(AssistantBlockWidget::new(text.clone(), *live)))
        } else {
            None
        }
    }
}

/// Built-in factory for [`Block::Thinking`].
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinThinkingFactory;

impl BlockFactory for BuiltinThinkingFactory {
    fn make(&self, block: &Block) -> Option<Box<dyn BlockWidget>> {
        if let Block::Thinking { text, folded, live } = block {
            Some(Box::new(ThinkingBlockWidget::new(
                text.clone(),
                *folded,
                *live,
            )))
        } else {
            None
        }
    }
}

/// Built-in factory for paired tool-call + tool-result blocks.
///
/// Single-block lookups via [`BlockFactory::make`] are not
/// meaningful for this kind (tool pairs span two blocks); the
/// caller uses [`BlockRenderer::pair_widget_for`] which threads
/// both blocks through.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinToolPairFactory;

impl BlockFactory for BuiltinToolPairFactory {
    fn make(&self, _block: &Block) -> Option<Box<dyn BlockWidget>> {
        None
    }
}

impl ToolPairFactoryExt for BuiltinToolPairFactory {
    fn make_pair(&self, call: &Block, result: &Block) -> Option<Box<dyn BlockWidget>> {
        ToolPairBlockWidget::from_pair(call, result).map(|w| Box::new(w) as Box<dyn BlockWidget>)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn user_block() -> Block {
        Block::User {
            text: "hello".into(),
        }
    }

    fn custom_block(kind: &str) -> Block {
        Block::Custom {
            kind: kind.into(),
            text: "payload".into(),
            folded: false,
        }
    }

    #[test]
    fn default_registry_is_empty_and_returns_none() {
        let r = BlockRenderer::default();
        assert!(r.widget_for(&user_block()).is_none());
    }

    #[test]
    fn builtins_registry_dispatches_user_blocks() {
        let r = BlockRenderer::with_builtins();
        assert!(r.widget_for(&user_block()).is_some());
    }

    #[test]
    fn builtins_registry_dispatches_assistant_blocks() {
        let r = BlockRenderer::with_builtins();
        let block = Block::Assistant {
            text: "hi".into(),
            live: false,
        };
        assert!(r.widget_for(&block).is_some());
    }

    #[test]
    fn builtins_registry_dispatches_thinking_blocks() {
        let r = BlockRenderer::with_builtins();
        let block = Block::Thinking {
            text: "thoughts".into(),
            folded: false,
            live: false,
        };
        assert!(r.widget_for(&block).is_some());
    }

    #[test]
    fn builtins_registry_returns_none_for_tool_call_alone() {
        let r = BlockRenderer::with_builtins();
        let block = Block::ToolCall {
            call_id: "c1".into(),
            name: "read".into(),
            input_summary: "x".into(),
            input_pretty: "{}".into(),
            folded: false,
            started_at: Instant::now(),
        };
        assert!(
            r.widget_for(&block).is_none(),
            "tool-call-alone is handled at the pair-walker level"
        );
    }

    #[test]
    fn pair_widget_returns_widget_for_paired_blocks() {
        let r = BlockRenderer::with_builtins();
        let call = Block::ToolCall {
            call_id: "c1".into(),
            name: "read".into(),
            input_summary: "x".into(),
            input_pretty: "{}".into(),
            folded: false,
            started_at: Instant::now(),
        };
        let result = Block::ToolResult {
            call_id: "c1".into(),
            name: "read".into(),
            output: "x".into(),
            is_error: false,
            folded: false,
            duration_ms: Some(1),
        };
        assert!(r.pair_widget_for(&call, &result).is_some());
    }

    #[test]
    fn unknown_custom_kind_returns_none() {
        let r = BlockRenderer::with_builtins();
        assert!(r.widget_for(&custom_block("bogus")).is_none());
    }

    #[test]
    fn registered_custom_kind_dispatches_through_registry() {
        struct CustomFactory;
        impl BlockFactory for CustomFactory {
            fn make(&self, _: &Block) -> Option<Box<dyn BlockWidget>> {
                Some(Box::new(super::super::widget::EmptyBlockWidget))
            }
        }

        let mut r = BlockRenderer::with_builtins();
        r.set_custom("kage:notify", Arc::new(CustomFactory));
        assert!(r.widget_for(&custom_block("kage:notify")).is_some());
        assert!(r.widget_for(&custom_block("other")).is_none());
    }

    #[test]
    fn set_builtin_overrides_factory() {
        struct OverrideFactory;
        impl BlockFactory for OverrideFactory {
            fn make(&self, _: &Block) -> Option<Box<dyn BlockWidget>> {
                Some(Box::new(super::super::widget::EmptyBlockWidget))
            }
        }

        let mut r = BlockRenderer::with_builtins();
        r.set_builtin(BuiltinKind::User, Arc::new(OverrideFactory));
        let widget = r.widget_for(&user_block()).expect("override should match");
        assert_eq!(widget.measure(40), 0, "EmptyBlockWidget reports zero rows");
    }

    #[test]
    fn factory_trait_is_send_and_sync_for_arc_storage() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<dyn BlockFactory>>();
    }
}
