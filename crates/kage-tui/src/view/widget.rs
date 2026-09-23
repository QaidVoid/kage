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
    /// owns. The default returns an empty vector for
    /// [`EmptyBlockWidget`]; every real widget overrides this.
    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let _ = (width, ctx);
        Vec::new()
    }
}

/// Uniform layout convention for every block widget.
///
/// The spacing every block reserves around its content keeps the
/// buffer reading as a series of evenly-spaced blocks rather than a
/// mix of "tinted bubbles padded inside" and "naked assistant text
/// crammed against its neighbours".
///
/// The 1-row bottom padding gives non-bubble blocks the same vertical
/// breathing room bubbles already had via their internal pad row,
/// and the 2-col left padding aligns content with the bubble interior
/// (1 col rule + 1 col internal pad).
pub struct BlockPadding;

impl BlockPadding {
    /// Rows of blank space above the block's content. Currently `0`
    /// for both bubbles and non-bubbles; bumped only if a future
    /// design wants more headroom.
    pub const TOP: usize = 0;
    /// Rows of blank space below the block's content. `1` for
    /// non-bubble blocks (added by `mark_emphasis`) and matched by
    /// the bubble's existing trailing pad row.
    pub const BOTTOM: usize = 1;
    /// Left chrome cells reserved before content. `2` to match the
    /// focus-rule (1 col) plus its trailing space (1 col).
    pub const LEFT: usize = 2;
    /// Right chrome cells reserved after content. `0` - blocks fill
    /// to the right edge.
    pub const RIGHT: usize = 0;
}

/// No-op widget used as the safe fallback from registry lookups for
/// kinds without a real widget. Yields no lines.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyBlockWidget;

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
