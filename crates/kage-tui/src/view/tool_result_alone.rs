//! Widget for an unpaired tool result (rare; usually a result is
//! consumed via [`super::ToolPairBlockWidget`] and skipped at the
//! per-block layer). Renders as a finished tool row without input.

use ratatui::text::Line;
use serde_json::Value;

use super::tool_view::ToolPhase;
use super::widget::{BlockWidget, RenderCtx};
use super::{ToolRow, tool_row_lines};
use crate::buffer::Block;

/// Renders an orphan [`crate::buffer::Block::ToolResult`] (a tool
/// result whose matching [`crate::buffer::Block::ToolCall`] is
/// missing).
#[derive(Clone, Debug)]
pub struct ToolResultAloneBlockWidget {
    name: String,
    output: String,
    is_error: bool,
    folded: bool,
    duration_ms: Option<u64>,
}

impl ToolResultAloneBlockWidget {
    /// Construct a widget for an orphan tool result.
    #[must_use]
    pub fn from_block(block: &Block) -> Option<Self> {
        match block {
            Block::ToolResult {
                name,
                output,
                is_error,
                folded,
                duration_ms,
                ..
            } => Some(Self {
                name: name.clone(),
                output: output.clone(),
                is_error: *is_error,
                folded: *folded,
                duration_ms: *duration_ms,
            }),
            _ => None,
        }
    }
}

impl BlockWidget for ToolResultAloneBlockWidget {
    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let row = ToolRow {
            name: &self.name,
            input: &Value::Null,
            phase: if self.is_error {
                ToolPhase::Failed
            } else {
                ToolPhase::Done
            },
            folded: self.folded,
            elapsed_ms: self.duration_ms,
            output: &self.output,
        };
        tool_row_lines(&row, width, ctx.emphasis, ctx.row_budget)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Emphasis;
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn from_block_rejects_non_result_variants() {
        let user = Block::User { text: "hi".into() };
        assert!(ToolResultAloneBlockWidget::from_block(&user).is_none());
    }

    #[test]
    fn lines_paint_tool_name_and_body() {
        let block = Block::ToolResult {
            call_id: "missing".into(),
            name: "custom".into(),
            output: "result body".into(),
            is_error: false,
            folded: true,
            duration_ms: Some(5),
        };
        let theme = Theme::default();
        let ctx = RenderCtx {
            theme: &theme,
            focused: false,
            emphasis: Emphasis::None,
            selection: None,
            search_pattern: None,
            row_budget: None,
        };
        let w = ToolResultAloneBlockWidget::from_block(&block).unwrap();
        let text: String = w
            .lines(60, &ctx)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("Called custom"), "got {text:?}");
        assert!(text.contains("result body"), "got {text:?}");
    }
}
