//! `BlockWidget` trait and per-block render context.
//!
//! Every block kind (user, assistant, thinking, tool pair, custom,
//! plugin-supplied) is a type that implements [`BlockWidget`] and is
//! dispatched through the block registry instead of a giant `match`.
//!
//! The trait is deliberately `dyn`-compatible: no generic methods, no
//! `Self` in return positions, no associated types. This lets plugin
//! adapters return `Box<dyn BlockWidget>` from factories and the
//! registry hold `Arc<dyn BlockWidget>` per kind.

use ratatui::text::Line;

use super::Emphasis;
use crate::theme::Theme;

/// Read-only context handed to every [`BlockWidget::lines`] call.
///
/// Carries the theme, navigation focus, emphasis tier, an optional
/// screen-level selection range, and the active search pattern.
/// Widgets read this to decide highlight / chrome styling without
/// reaching back into the host.
#[derive(Clone, Copy, Debug)]
pub struct RenderCtx<'a> {
    /// Active theme for color and style lookups.
    pub theme: &'a Theme,
    /// `true` when this block is the navigation focus head.
    pub focused: bool,
    /// Attention tier: focused / search-match / none. Computed by the
    /// caller from focus and search hits.
    pub emphasis: Emphasis,
    /// Screen-level selection, if any. The post-frame
    /// `capture_and_overlay` path still owns selection painting until
    /// a widget elects to handle it locally.
    pub selection: Option<&'a SelectionState>,
    /// Active search pattern, used by widgets that highlight matches
    /// inline (e.g. assistant text). `None` when no `/` search is
    /// open.
    pub search_pattern: Option<&'a str>,
    /// Hint: max visual rows the caller will actually display.
    /// Widgets can use this to skip expensive work (syntect
    /// highlighting) on rows beyond the visible window. `None`
    /// means unlimited (test / backward compat).
    pub row_budget: Option<usize>,
}

/// Anchor / cursor pair describing a screen-level visual selection.
///
/// Coordinates are in the buffer's virtual row-and-column space, the
/// same units `Buffer::screen_top_of` and the host's
/// `screen_selection` use. Widgets that opt into local selection
/// painting intersect this against their painted area.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionState {
    /// Where the user pressed `v` (or where the drag started).
    pub anchor_vrow: usize,
    /// Anchor column.
    pub anchor_col: u16,
    /// Current cursor virtual row.
    pub cursor_vrow: usize,
    /// Current cursor column.
    pub cursor_col: u16,
}

/// One block kind's renderer.
///
/// [`Self::lines`] is the single render path: `render_buffer` sizes
/// each block from `lines().len()` and paints the returned lines.
///
/// Implementations are stateless or hold an immutable snapshot of the
/// block they paint; per-frame state lives in [`RenderCtx`].
pub trait BlockWidget: Send + Sync {
    /// Produce the styled `Line`s this block paints at `width`
    /// columns, including any chrome (focus rule column, padding) it
    /// owns. The default returns no lines; every real widget overrides
    /// this.
    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let _ = (width, ctx);
        Vec::new()
    }
}

/// Uniform layout convention for every block widget.
///
/// No-op widget for tests. Yields no lines.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyBlockWidget;

#[cfg(test)]
impl BlockWidget for EmptyBlockWidget {}

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

    #[test]
    fn empty_widget_yields_no_lines() {
        let theme = Theme::default();
        assert!(EmptyBlockWidget.lines(80, &ctx(&theme)).is_empty());
    }

    #[test]
    fn trait_is_object_safe() {
        let theme = Theme::default();
        let w: Box<dyn BlockWidget> = Box::new(EmptyBlockWidget);
        assert!(w.lines(80, &ctx(&theme)).is_empty());
    }
}
