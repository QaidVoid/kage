//! Bridge a Lua `kage.register_block_renderer` handler into the
//! [`BlockWidget`] registry.
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
//! never silent. While output is still pending, an override of a
//! built-in kind paints the built-in widget so a streaming block does
//! not flicker; a custom kind shows a dim placeholder.

use std::sync::Arc;

use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use super::mark_emphasis;
use super::registry::{BlockFactory, builtin_widget_for};
use super::widget::{BlockWidget, RenderCtx};
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
            fallback: builtin_widget_for(block),
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
    fallback: Option<Box<dyn BlockWidget>>,
}

impl BlockWidget for PluginBlockWidget {
    /// Call the Lua renderer, map its `ChromeLine`s onto ratatui
    /// lines, and apply the uniform block chrome. Pending output
    /// paints the built-in widget for a built-in kind, or a dim
    /// placeholder for a custom kind. Empty Lua output (error,
    /// non-conforming, or genuinely empty) becomes one marker line so
    /// the failure is visible, never a silent blank block.
    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let mut payload = self.payload.clone();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("width".into(), serde_json::json!(width));
        }
        let body = match self.renderer.render(&payload) {
            None => {
                if let Some(fallback) = &self.fallback {
                    return fallback.lines(width, ctx);
                }
                vec![Line::styled(
                    "...",
                    Style::default().add_modifier(Modifier::DIM),
                )]
            }
            Some(chrome) if chrome.is_empty() => vec![Line::from(format!(
                "[block renderer `{}` produced no output]",
                self.renderer.kind()
            ))],
            Some(chrome) => super::chrome_lines_to_ratatui(&chrome, Style::default()),
        };
        mark_emphasis(body, width, ctx.emphasis, None)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Emphasis;
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

    fn flat_text(w: &dyn BlockWidget) -> String {
        let theme = Theme::default();
        w.lines(40, &ctx(&theme))
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
            .collect::<String>()
            .split_whitespace()
            .collect()
    }

    #[test]
    fn pending_builtin_override_paints_the_builtin_widget() {
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval(
            r#"kage.register_block_renderer("assistant", function(b)
                 kage.sleep_ms(200)
                 return "A[" .. b.text .. "]"
               end)"#,
        )
        .unwrap();
        let renderer = rt.registered_block_renderers().into_iter().next().unwrap();
        let w = PluginBlockFactory::new(renderer)
            .make(&Block::Assistant {
                text: "streamed".into(),
                live: true,
            })
            .unwrap();
        let text = flat_text(w.as_ref());
        assert!(text.contains("streamed"), "rendered: {text}");
        assert!(!text.contains("A["), "rendered: {text}");
        assert!(!text.contains("..."), "rendered: {text}");
    }

    #[test]
    fn pending_custom_kind_shows_the_placeholder() {
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval(
            r#"kage.register_block_renderer("demo:slow", function(b)
                 kage.sleep_ms(200)
                 return b.text
               end)"#,
        )
        .unwrap();
        let renderer = rt.registered_block_renderers().into_iter().next().unwrap();
        let w = PluginBlockFactory::new(renderer)
            .make(&Block::Custom {
                kind: "demo:slow".into(),
                text: "late".into(),
                folded: false,
            })
            .unwrap();
        assert_eq!(flat_text(w.as_ref()), "...");
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
