//! Render helpers shared by the view modules' unit tests.

use ratatui::text::Line;

use super::Emphasis;
use super::widget::RenderCtx;
use crate::theme::Theme;

/// The context the view tests paint under: unfocused, no emphasis,
/// selection or search, and no row budget.
pub(crate) fn ctx(theme: &Theme) -> RenderCtx<'_> {
    RenderCtx {
        theme,
        focused: false,
        emphasis: Emphasis::None,
        selection: None,
        search_pattern: None,
        row_budget: None,
    }
}

/// Each painted line as one plain row, whitespace trimmed.
pub(crate) fn rows(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .collect()
}

/// [`rows`] keeping the leading gutter, so tests can compare the
/// column a row's text starts at.
pub(crate) fn aligned_rows(lines: &[Line<'_>]) -> Vec<String> {
    lines
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
