//! `BlockWidget` trait and per-block render context.
//!
//! PB.1 lays the foundation for the per-block widget refactor: every
//! block kind (user, assistant, thinking, tool pair, custom,
//! plugin-supplied) eventually becomes a type that implements
//! [`BlockWidget`], owns its own measure-and-paint loop, and is
//! dispatched through a registry instead of a giant `match`.
//!
//! The trait is deliberately `dyn`-compatible: no generic methods, no
//! `Self` in return positions, no associated types. This lets plugin
//! authors return `Box<dyn BlockWidget>` from their factories and the
//! upcoming registry hold `Arc<dyn BlockWidget>` per kind.
//!
//! No callers are wired in this commit. PB.2 onward port one block
//! kind at a time onto this trait while the lines-based renderer keeps
//! working in parallel. PB.9 retires the lines path.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

use super::Emphasis;
use crate::theme::Theme;

/// Read-only context handed to every `BlockWidget::render` call.
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
    /// Screen-level selection, if any. PB.1 surface only; the
    /// post-frame `capture_and_overlay` path still owns selection
    /// painting until a widget elects to handle it locally.
    pub selection: Option<&'a SelectionState>,
    /// Active search pattern, used by widgets that highlight matches
    /// inline (e.g. assistant text). `None` when no `/` search is
    /// open.
    pub search_pattern: Option<&'a str>,
}

/// Anchor / cursor pair describing a screen-level visual selection.
///
/// Coordinates are in the buffer's virtual row-and-column space, the
/// same units `Buffer::screen_top_of` and the host's
/// `screen_selection` use. Widgets that opt into local selection
/// painting (PB.5+) intersect this against their painted area.
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
/// The trait is `dyn`-compatible so the upcoming `BlockRenderer`
/// registry can hold `Arc<dyn BlockWidget>` and plugin adapters can
/// return `Box<dyn BlockWidget>` from factories.
///
/// Implementations are stateless or hold an immutable snapshot of the
/// block they paint; per-frame state lives in [`RenderCtx`]. The
/// renderer calls [`Self::measure`] first to get an exact height for
/// vertical layout, then [`Self::render`] with the resulting `Rect`.
///
/// While the existing `render_buffer` still composes blocks into one
/// big `Paragraph`, [`Self::lines`] returns the same `Line`s the
/// widget would paint so the lines path can dispatch via the
/// registry. PB.9 retires the `Paragraph` composition; until then
/// `lines()` and `render()` are kept in sync.
pub trait BlockWidget: Send + Sync {
    /// Compute the exact number of terminal rows this block occupies
    /// when rendered at `width` columns. Replaces the
    /// approximation-based `approximate_block_height` once every
    /// block kind has migrated.
    fn measure(&self, width: u16) -> u16;

    /// Paint the block into `area` of `buf`. The widget is responsible
    /// for the full content of `area`, including any chrome (focus
    /// rule column, padding) it owns.
    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &RenderCtx<'_>);

    /// Produce the styled lines this widget would paint at `width`.
    /// The default returns an empty vector for [`EmptyBlockWidget`];
    /// every real widget overrides this so `render_buffer` can route
    /// through the registry while the lines-based composition path
    /// still exists.
    fn lines(&self, width: u16, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let _ = (width, ctx);
        Vec::new()
    }
}

/// Uniform layout convention for every block widget.
///
/// PB.7 locks the spacing every block reserves around its content,
/// so the buffer reads as a series of evenly-spaced blocks rather
/// than the previous mix of "tinted bubbles padded inside" and
/// "naked assistant text crammed against its neighbours".
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

/// No-op widget used to lock the trait shape and as a safe default
/// from registry lookups before every block kind is migrated. Renders
/// nothing and reports zero rows.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyBlockWidget;

impl BlockWidget for EmptyBlockWidget {
    fn measure(&self, _width: u16) -> u16 {
        0
    }

    fn render(&self, _area: Rect, _buf: &mut Buffer, _ctx: &RenderCtx<'_>) {}
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
        }
    }

    #[test]
    fn empty_widget_measures_zero() {
        assert_eq!(EmptyBlockWidget.measure(80), 0);
    }

    #[test]
    fn empty_widget_paints_nothing() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        EmptyBlockWidget.render(area, &mut buf, &ctx(&theme));
        for x in area.left()..area.right() {
            for y in area.top()..area.bottom() {
                let cell = &buf[(x, y)];
                assert_eq!(cell.symbol(), " ", "cell ({x},{y}) should be untouched");
            }
        }
    }

    #[test]
    fn trait_is_object_safe() {
        let w: Box<dyn BlockWidget> = Box::new(EmptyBlockWidget);
        assert_eq!(w.measure(80), 0);
    }
}
