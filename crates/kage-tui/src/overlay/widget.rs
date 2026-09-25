//! `OverlayWidget` trait, render context, and action enum.
//!
//! Every overlay surface (model picker, slash palette, settings
//! dialog, session tree, login, confirmation, single-line input,
//! multi-line editor, custom plugin overlay) is a type that
//! implements [`OverlayWidget`].
//!
//! The trait is deliberately `dyn`-compatible: no generic methods, no
//! `Self` in return positions, no associated types. That lets the App
//! hold a plugin dialog as a `Box<dyn OverlayWidget>`. The dispatch
//! layers in the App route keys to whichever modal is on top.

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
/// `Resolve` carries a JSON payload because the plugin dialog
/// overlays round-trip results to Lua handlers; built-in overlays whose
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
/// `dyn`-compatible so overlays stay swappable behind trait objects
/// without enum dispatch at the call sites.
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
    /// `&mut self` (unlike [`crate::view::widget::BlockWidget::lines`])
    /// because interactive overlays persist layout state across
    /// frames (scroll offset, last viewport size, animation cursor).
    /// Blocks paint a content snapshot and have no such state.
    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &OverlayCtx<'_>);

    /// Drive the overlay by one key event.
    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction;

    /// Route a bracketed paste into the overlay. The default swallows
    /// the paste: a modal that does not accept text must not let it
    /// fall through to the host's hidden input underneath. Text-entry
    /// overlays override this to insert at their cursor.
    fn handle_paste(&mut self, _text: &str) {}

    /// What the footer says the keys do while the overlay has them.
    fn footer_hint(&self) -> &'static str {
        "enter to confirm \u{b7} esc to cancel"
    }
}

/// No-op overlay for tests. Reports a zero-size area, paints nothing,
/// and propagates every key.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyOverlayWidget;

#[cfg(test)]
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
