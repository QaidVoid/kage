//! [`OpaqueClear`]: like [`ratatui::widgets::Clear`], but opaque.
//!
//! `Clear` resets cells to the terminal default, which on a
//! transparent / wallpapered terminal punches a see-through hole.
//! Every popup and overlay needs to wipe what is under it *and* sit
//! on the theme's base canvas, so they use this instead: it blanks
//! each cell's symbol (so leftover glyphs don't bleed through) and
//! paints the base background, keeping the whole UI consistently
//! opaque.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;

/// Drop-in replacement for [`ratatui::widgets::Clear`] that leaves an
/// opaque base-canvas rectangle instead of a transparent one.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpaqueClear;

impl Widget for OpaqueClear {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let bg = crate::theme::current().bg;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &mut buf[(x, y)];
                cell.reset();
                cell.set_style(Style::default().bg(bg));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    #[test]
    fn fills_the_area_with_the_base_bg_and_blanks_symbols() {
        crate::theme::set_current(crate::theme::Theme::default_dark());
        let area = Rect::new(0, 0, 3, 2);
        let mut buf = Buffer::empty(area);
        buf[(1, 0)].set_symbol("X");
        OpaqueClear.render(area, &mut buf);
        for y in 0..2 {
            for x in 0..3 {
                let cell = &buf[(x, y)];
                assert_eq!(cell.symbol(), " ", "symbol blanked at {x},{y}");
                assert_eq!(
                    cell.bg,
                    crate::theme::Theme::default_dark().bg,
                    "base bg painted at {x},{y}"
                );
            }
        }
        assert_ne!(crate::theme::Theme::default_dark().bg, Color::Reset);
    }
}
