//! Widget for an unpaired tool call: one whose arguments are still
//! streaming, that waits for approval or runs, or that never got a
//! result.

use std::sync::Arc;
use std::time::Instant;

use ratatui::text::Line;
use serde_json::Value;

use super::tool_view::ToolPhase;
use super::widget::{BlockWidget, RenderCtx};
use super::{ToolRow, tool_row_lines};
use crate::buffer::Block;

/// Renders a [`crate::buffer::Block::ToolCall`] without a result as a
/// tool row in its current phase. A running call shows its live
/// elapsed time and the tail of its latest progress.
#[derive(Clone, Debug)]
pub struct ToolCallAloneBlockWidget {
    name: String,
    input: Arc<Value>,
    phase: ToolPhase,
    folded: bool,
    progress: String,
    started_at: Instant,
}

impl ToolCallAloneBlockWidget {
    /// Construct a widget for a not-yet-paired tool call.
    #[must_use]
    pub fn from_block(block: &Block) -> Option<Self> {
        match block {
            Block::ToolCall {
                name,
                input,
                phase,
                folded,
                progress,
                started_at,
                ..
            } => Some(Self {
                name: name.clone(),
                input: Arc::clone(input),
                phase: *phase,
                folded: *folded,
                progress: progress.clone(),
                started_at: *started_at,
            }),
            _ => None,
        }
    }
}

impl BlockWidget for ToolCallAloneBlockWidget {
    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let elapsed = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let row = ToolRow {
            name: &self.name,
            input: &self.input,
            phase: self.phase,
            folded: self.folded,
            elapsed_ms: (self.phase == ToolPhase::Running).then_some(elapsed),
            output: &self.progress,
        };
        tool_row_lines(&row, width, ctx.emphasis, ctx.row_budget)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::Emphasis;
    use super::*;
    use crate::buffer::Buffer;
    use crate::theme::Theme;

    fn header(buf: &Buffer) -> Vec<String> {
        let theme = Theme::default();
        let ctx = RenderCtx {
            theme: &theme,
            focused: false,
            emphasis: Emphasis::None,
            selection: None,
            search_pattern: None,
            row_budget: None,
        };
        ToolCallAloneBlockWidget::from_block(&buf.blocks()[0])
            .unwrap()
            .lines(80, &ctx)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn bash_call() -> Buffer {
        let mut buf = Buffer::new();
        buf.push_tool_call("c1", "bash", json!({"command": "cargo test"}));
        buf
    }

    #[test]
    fn from_block_rejects_non_tool_call_variants() {
        let user = Block::User { text: "hi".into() };
        assert!(ToolCallAloneBlockWidget::from_block(&user).is_none());
    }

    #[test]
    fn header_text_follows_the_phase() {
        let mut buf = bash_call();
        let rows = header(&buf);
        assert!(rows[0].ends_with("\u{2022} Running cargo test"), "{rows:?}");

        buf.set_tool_phase("c1", ToolPhase::Waiting);
        let rows = header(&buf);
        assert!(rows[0].contains("\u{2022} Running cargo test"), "{rows:?}");
        assert!(rows[0].ends_with("waiting"), "{rows:?}");

        buf.set_tool_phase("c1", ToolPhase::Running);
        let rows = header(&buf);
        assert!(rows[0].contains("Running cargo test"), "{rows:?}");
        assert!(rows[0].ends_with("0.0s"), "{rows:?}");

        buf.set_tool_phase("c1", ToolPhase::Denied);
        let rows = header(&buf);
        assert!(rows[0].contains("\u{2298} Ran cargo test"), "{rows:?}");
        assert!(rows[0].ends_with("denied"), "{rows:?}");

        buf.set_tool_phase("c1", ToolPhase::Interrupted);
        assert!(header(&buf)[0].ends_with("interrupted"));
    }

    #[test]
    fn a_running_bash_shows_its_progress_tail() {
        let mut buf = bash_call();
        buf.set_tool_phase("c1", ToolPhase::Running);
        let progress: Vec<String> = (1..=7).map(|i| format!("step {i}")).collect();
        buf.set_tool_progress("c1", progress.join("\n"));
        let rows = header(&buf);
        assert!(rows[1].ends_with("... 2 earlier lines"), "{rows:?}");
        assert!(rows.last().unwrap().ends_with("step 7"), "{rows:?}");
    }

    #[test]
    fn read_only_calls_show_live_verbs() {
        let mut buf = Buffer::new();
        buf.push_tool_call("c1", "read", json!({"path": "a.rs"}));
        assert!(header(&buf)[0].ends_with("Reading a.rs"));
    }
}
