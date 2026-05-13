//! `OverlayWidget` trait, render context, and action enum.
//!
//! PO.1 lays the foundation for the per-overlay widget refactor: every
//! overlay surface (model picker, slash palette, settings dialog,
//! session tree, login, confirmation, single-line input, multi-line
//! editor, custom plugin overlay) eventually becomes a type that
//! implements [`OverlayWidget`] and is dispatched through an
//! [`crate::overlay::OverlayRegistry`] instead of a stack of
//! hand-rolled `Option<X>` fields on [`crate::App`].
//!
//! The trait is deliberately `dyn`-compatible: no generic methods, no
//! `Self` in return positions, no associated types. This lets PE.B's
//! `ui.custom(...)` accept `Box<dyn OverlayWidget>` from a Lua
//! factory and the registry hold `Arc<dyn OverlayWidget>` per key.
//!
//! No callers are wired in this commit. PO.2 onward port one overlay
//! at a time onto this trait while the existing `OverlayPicker` and
//! `SlashPalette` paths keep working in parallel. The registry lands
//! in PO.5 once enough overlays implement the trait that there is
//! something to dispatch.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;

use crate::theme::Theme;

/// Read-only context handed to every [`OverlayWidget::render`] call.
///
/// Carries the theme and the available viewport. Overlays decide how
/// much of `viewport` to use; the host paints whatever sub-rectangle
/// [`OverlayWidget::measure`] returned.
#[derive(Clone, Copy, Debug)]
pub struct OverlayCtx<'a> {
    /// Active theme for color and style lookups.
    pub theme: &'a Theme,
    /// Available area the overlay may paint into. Typically the full
    /// frame minus any system chrome the host wants preserved.
    pub viewport: Rect,
}

/// Outcome of one keystroke on an overlay.
///
/// `Resolve` carries a JSON payload because PE.B's dialog APIs need
/// to round-trip results to Lua handlers; built-in overlays whose
/// result is a single string still serialize it as
/// `serde_json::Value::String`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverlayAction {
    /// Key consumed; overlay stays open.
    Stay,
    /// Close the overlay without resolving a value (Esc, Ctrl+C).
    Close,
    /// Close the overlay and resolve the carried value.
    Resolve(serde_json::Value),
    /// Overlay ignored the key; let the parent handle it.
    PropagateKey,
}

/// One overlay surface.
///
/// `dyn`-compatible by design so the upcoming [`crate::overlay::OverlayRegistry`]
/// can hold `Arc<dyn OverlayWidget>` and plugin adapters can return
/// `Box<dyn OverlayWidget>` from factories.
///
/// Implementations own their own input state (cursor, selection,
/// search text). The host calls [`Self::measure`] to learn the
/// preferred sub-rectangle inside the available viewport, then
/// [`Self::render`] to paint, then [`Self::handle_key`] for each key
/// the overlay should see.
pub trait OverlayWidget: Send + Sync {
    /// Compute the preferred sub-rectangle inside `available`. The
    /// host paints the returned rect; the overlay must not paint
    /// outside it. Most overlays center themselves and pick a size
    /// proportional to `available`.
    fn measure(&self, available: Rect) -> Rect;

    /// Paint the overlay into `area`. The widget is responsible for
    /// the full content of `area`, including borders and inner
    /// chrome. The host blanks `area` with [`ratatui::widgets::Clear`]
    /// before the call, so overlays can assume a clean surface.
    ///
    /// `&mut self` (unlike [`crate::view::widget::BlockWidget::render`])
    /// because interactive overlays persist layout state across
    /// frames (scroll offset, last viewport size, animation cursor).
    /// Blocks paint a content snapshot and have no such state.
    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &OverlayCtx<'_>);

    /// Drive the overlay by one key event.
    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction;
}

/// No-op overlay used to lock the trait shape and as a safe default
/// from registry lookups before every overlay is migrated. Reports a
/// zero-size area, paints nothing, and propagates every key.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyOverlayWidget;

impl OverlayWidget for EmptyOverlayWidget {
    fn measure(&self, _available: Rect) -> Rect {
        Rect::new(0, 0, 0, 0)
    }

    fn render(&mut self, _area: Rect, _buf: &mut Buffer, _ctx: &OverlayCtx<'_>) {}

    fn handle_key(&mut self, _key: KeyEvent) -> OverlayAction {
        OverlayAction::PropagateKey
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    fn ctx(theme: &Theme) -> OverlayCtx<'_> {
        OverlayCtx {
            theme,
            viewport: Rect::new(0, 0, 80, 24),
        }
    }

    #[test]
    fn empty_overlay_measures_zero() {
        let measured = EmptyOverlayWidget.measure(Rect::new(0, 0, 80, 24));
        assert_eq!(measured, Rect::new(0, 0, 0, 0));
    }

    #[test]
    fn empty_overlay_paints_nothing() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        let mut w = EmptyOverlayWidget;
        w.render(area, &mut buf, &ctx(&theme));
        for x in area.left()..area.right() {
            for y in area.top()..area.bottom() {
                assert_eq!(buf[(x, y)].symbol(), " ");
            }
        }
    }

    #[test]
    fn empty_overlay_propagates_keys() {
        let mut w = EmptyOverlayWidget;
        let action = w.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, OverlayAction::PropagateKey);
    }

    #[test]
    fn trait_is_object_safe() {
        let mut w: Box<dyn OverlayWidget> = Box::new(EmptyOverlayWidget);
        let _ = w.measure(Rect::new(0, 0, 10, 10));
        let _ = w.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn resolve_carries_arbitrary_json() {
        let action = OverlayAction::Resolve(serde_json::json!({ "id": "anthropic:claude" }));
        match action {
            OverlayAction::Resolve(v) => {
                assert_eq!(
                    v["id"],
                    serde_json::Value::String("anthropic:claude".into())
                );
            }
            _ => panic!("expected Resolve"),
        }
    }
}
