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
