//! Bridge a Lua `kage.register_block_renderer` handler into the
//! [`BlockWidget`] registry (PT.7).
//!
//! A [`PluginBlockFactory`] holds one plugin renderer and produces a
//! [`PluginBlockWidget`] for every `Block::Custom` of the matching
//! kind. The widget calls into Lua to get the block's styled lines
//! (the same `ChromeLine` shape `kage.ui.set_header` uses), then runs
//! them through [`mark_emphasis`] so a plugin-drawn block still gets
//! the conversation's focus rule and spacing - the plugin owns the
//! *content*; the host keeps *block chrome* uniform.
//!
//! A renderer that errors or returns nothing yields a single visible
//! marker line rather than a blank gap: a broken renderer is obvious,
//! never silent.

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use super::registry::BlockFactory;
use super::widget::{BlockWidget, RenderCtx};
use super::{Emphasis, mark_emphasis};
use crate::buffer::Block;

/// A [`BlockFactory`] backed by one plugin block renderer. Registered
/// into the global registry under the renderer's `kind` via
/// [`super::registry::register_custom`].
pub struct PluginBlockFactory {
    renderer: Arc<kage_plugin::LuaBlockRenderer>,
}

impl PluginBlockFactory {
    /// Wrap a plugin renderer so the registry can dispatch its kind.
    #[must_use]
    pub fn new(renderer: Arc<kage_plugin::LuaBlockRenderer>) -> Self {
        Self { renderer }
    }
}

impl BlockFactory for PluginBlockFactory {
    fn make(&self, block: &Block) -> Option<Box<dyn BlockWidget>> {
        // The registry only routes a block to the factory registered
        // for its slot (a custom kind, or a builtin variant), so any
        // variant that arrives here is one this renderer owns.
        Some(Box::new(PluginBlockWidget {
            renderer: Arc::clone(&self.renderer),
            payload: block_payload(block),
        }))
    }
}

/// Serialize a block into the JSON payload its Lua renderer sees.
/// Always carries `kind`; per-variant fields let a builtin override
/// (e.g. `assistant`) read the same data the built-in widget would.
/// `width` is injected per-frame by the widget, not here.
fn block_payload(block: &Block) -> serde_json::Value {
    match block {
        Block::User { text } => serde_json::json!({ "kind": "user", "text": text }),
        Block::Assistant { text, live } => {
            serde_json::json!({ "kind": "assistant", "text": text, "live": live })
        }
        Block::Thinking { text, folded, live } => serde_json::json!({
            "kind": "thinking", "text": text, "folded": folded, "live": live,
        }),
        Block::ToolCall {
            name,
            input_summary,
            input_pretty,
            folded,
            ..
        } => serde_json::json!({
            "kind": "tool_call", "name": name, "input_summary": input_summary,
            "input_pretty": input_pretty, "folded": folded,
        }),
        Block::ToolResult {
            name,
            output,
            is_error,
            folded,
            duration_ms,
            ..
        } => serde_json::json!({
            "kind": "tool_result", "name": name, "output": output,
            "is_error": is_error, "folded": folded, "duration_ms": duration_ms,
        }),
        Block::Custom { kind, text, folded } => serde_json::json!({
            "kind": kind, "text": text, "folded": folded,
        }),
    }
}

/// One block rendered by a plugin's Lua handler (custom kind or a
/// builtin-variant override).
struct PluginBlockWidget {
    renderer: Arc<kage_plugin::LuaBlockRenderer>,
    payload: serde_json::Value,
}

impl PluginBlockWidget {
    /// Call the Lua renderer, map its `ChromeLine`s onto ratatui
    /// lines, and apply the uniform block chrome at `emphasis`. Empty
    /// Lua output (error / non-conforming / genuinely empty) becomes
    /// one marker line so the failure is visible, never a silent
    /// blank block.
    fn lines_for(&self, width: u16, emphasis: Emphasis) -> Vec<Line<'static>> {
        let mut payload = self.payload.clone();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("width".into(), serde_json::json!(width));
        }
        let chrome = self.renderer.render(&payload);
        let body = if chrome.is_empty() {
            vec![Line::from(format!(
                "[block renderer `{}` produced no output]",
                self.renderer.kind()
            ))]
        } else {
            super::chrome_lines_to_ratatui(&chrome, Style::default())
        };
        mark_emphasis(body, width, emphasis, None)
    }
}

impl BlockWidget for PluginBlockWidget {
    fn measure(&self, width: u16) -> u16 {
        u16::try_from(self.lines_for(width, Emphasis::None).len()).unwrap_or(u16::MAX)
    }

    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &RenderCtx<'_>) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        Paragraph::new(self.lines(area.width, ctx)).render(area, buf);
    }

    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        self.lines_for(width, ctx.emphasis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn ctx(theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            theme,
            focused: false,
            emphasis: Emphasis::None,
            selection: None,
            search_pattern: None,
            row_budget: None,
        }
    }

    /// A renderer whose Lua handler echoes the block text in
    /// brackets, built through a real `PluginRuntime`.
    fn echo_renderer() -> Arc<kage_plugin::LuaBlockRenderer> {
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval(
            r#"kage.register_block_renderer("demo:card", function(b)
                 return "<<" .. b.text .. ">>"
               end)"#,
        )
        .unwrap();
        rt.registered_block_renderers().into_iter().next().unwrap()
    }

    #[test]
    fn builtin_override_receives_variant_payload() {
        // An "assistant" override sees the assistant block's text +
        // live fields - not just a custom {kind,text}. Routing by
        // slot is the registry's job; the factory shapes the payload.
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval(
            r#"kage.register_block_renderer("assistant", function(b)
                 return "A[" .. b.text .. "|" .. tostring(b.live) .. "]"
               end)"#,
        )
        .unwrap();
        let renderer = rt.registered_block_renderers().into_iter().next().unwrap();
        let f = PluginBlockFactory::new(renderer);
        let w = f
            .make(&Block::Assistant {
                text: "hi".into(),
                live: true,
            })
            .unwrap();
        let theme = Theme::default();
        let text: String = w
            .lines(40, &ctx(&theme))
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<String>()
            .split_whitespace()
            .collect();
        assert!(text.contains("A[hi|true]"), "rendered: {text}");
    }

    #[test]
    fn widget_renders_plugin_output() {
        let f = PluginBlockFactory::new(echo_renderer());
        let w = f
            .make(&Block::Custom {
                kind: "demo:card".into(),
                text: "hello".into(),
                folded: false,
            })
            .unwrap();
        let theme = Theme::default();
        let lines = w.lines(40, &ctx(&theme));
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<String>()
            .split_whitespace()
            .collect();
        assert!(text.contains("<<hello>>"), "rendered: {text}");
    }

    #[test]
    fn broken_renderer_shows_visible_marker_not_blank() {
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval(r#"kage.register_block_renderer("b", function() error("boom") end)"#)
            .unwrap();
        let renderer = rt.registered_block_renderers().into_iter().next().unwrap();
        let f = PluginBlockFactory::new(renderer);
        let w = f
            .make(&Block::Custom {
                kind: "b".into(),
                text: "x".into(),
                folded: false,
            })
            .unwrap();
        let theme = Theme::default();
        let text: String = w
            .lines(40, &ctx(&theme))
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<String>()
            .split_whitespace()
            .collect();
        assert!(text.contains("producednooutput"), "rendered: {text}");
    }
}
