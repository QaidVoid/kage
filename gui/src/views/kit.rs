//! Small controls more than one view draws the same way.

use gpui_kit::assets::IconName;
use gpui_kit::component::h_flex;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::{
    Div, Entity, FontWeight, Hsla, InteractiveElement, ParentElement as _, SharedString, Stateful,
    Styled, div, px,
};
use kage_client::wire::Cost;

use crate::theme::{FS_2XS, FS_XS, Palette, R_FULL, R_MD, SP_3};

/// The tones a small action button carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BtnTone {
    /// The plain hairline button.
    Plain,
    /// The filled accent button, for the main action.
    Primary,
    /// The outlined danger button, for the refuse answer.
    Danger,
}

/// A single-line input at the default size, without its vertical
/// padding. The padding leaves a 14px box for a 20px line, and the
/// field's horizontal clip cuts the glyphs vertically too.
pub(crate) fn input(state: &Entity<InputState>) -> Input {
    Input::new(state).py_0()
}

/// One small action button as the design draws it (`.btn.sm`): 26px
/// tall, an 8px radius, 12px medium text, in one tone. Children are the
/// caller's.
pub(crate) fn btn_sm(id: impl Into<SharedString>, tone: BtnTone, pal: &Palette) -> Stateful<Div> {
    let (fill_hover, line_strong) = (pal.fill_hover, pal.line_strong);
    let (danger_soft, accent_hover) = (pal.danger_soft, pal.accent_hover);
    let btn = h_flex()
        .id(id.into())
        .h(px(26.))
        .px(px(9.))
        .gap(px(SP_3))
        .flex_none()
        .items_center()
        .rounded(px(R_MD))
        .border_1()
        .font_weight(FontWeight::MEDIUM)
        .text_size(px(FS_XS))
        .cursor_pointer();
    match tone {
        BtnTone::Plain => btn
            .border_color(pal.line)
            .bg(pal.fill)
            .text_color(pal.ink)
            .hover(move |style| style.bg(fill_hover).border_color(line_strong)),
        BtnTone::Primary => btn
            .border_color(pal.accent)
            .bg(pal.accent)
            .text_color(gpui_kit::white())
            .hover(move |style| style.bg(accent_hover).border_color(accent_hover)),
        BtnTone::Danger => btn
            .border_color(pal.danger_bd)
            .text_color(pal.danger)
            .hover(move |style| style.bg(danger_soft)),
    }
}

/// The design's string hash, so an avatar hue and the constellation's
/// jitter agree with the web client.
pub(crate) fn design_hash(text: &str) -> u32 {
    let mut x: i32 = 0;
    for unit in text.encode_utf16() {
        x = x.wrapping_mul(31).wrapping_add(i32::from(unit));
    }
    x.unsigned_abs()
}

/// What a session tree spent, as the label under the status line: one
/// currency reads `USD 1.23`, a tree that mixed currencies reads every
/// subtotal joined with `+` instead of one silently dropped total.
/// Empty when nothing is priced.
pub(crate) fn cost_label(costs: &[Cost], decimals: usize) -> Option<String> {
    if costs.is_empty() {
        return None;
    }
    Some(
        costs
            .iter()
            .map(|cost| format!("{} {:.*}", cost.currency, decimals, cost.amount))
            .collect::<Vec<_>>()
            .join(" + "),
    )
}

/// The neutral badge of the design: fully round, a hairline border
/// over the soft fill, 10.5px medium muted text. One-off spacing a
/// site needs chains onto the returned div.
pub(crate) fn badge(label: impl Into<SharedString>, pal: &Palette) -> Div {
    div()
        .flex_none()
        .px(px(7.))
        .py(px(1.))
        .rounded(px(R_FULL))
        .border_1()
        .border_color(pal.line)
        .bg(pal.fill)
        .font_weight(FontWeight::MEDIUM)
        .text_size(px(10.5))
        .text_color(pal.muted)
        .child(label.into())
}

/// The badge on a custom tint: the neutral recipe with the caller's
/// colors, for badges that carry a meaning.
pub(crate) fn badge_in(label: impl Into<SharedString>, fg: Hsla, bg: Hsla, line: Hsla) -> Div {
    div()
        .flex_none()
        .px(px(7.))
        .py(px(1.))
        .rounded(px(R_FULL))
        .border_1()
        .border_color(line)
        .bg(bg)
        .font_weight(FontWeight::MEDIUM)
        .text_size(px(10.5))
        .text_color(fg)
        .child(label.into())
}

/// The colored chip of the design's chip anatomy: 20px tall, fully
/// round, mono at the smallest size, on the caller's tint.
pub(crate) fn chip(
    text: impl Into<SharedString>,
    fg: Hsla,
    bg: Hsla,
    mono: impl Into<SharedString>,
) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(20.))
        .px(px(7.))
        .rounded(px(R_FULL))
        .bg(bg)
        .font_family(mono.into())
        .text_size(px(FS_2XS))
        .text_color(fg)
        .whitespace_nowrap()
        .child(text.into())
}

/// The state chip of the agent cards: an icon and its label in one
/// color, without a fill, so a status reads as tinted text.
pub(crate) fn status_chip(icon: IconName, label: impl Into<SharedString>, color: Hsla) -> Div {
    h_flex()
        .gap(px(5.))
        .items_center()
        .whitespace_nowrap()
        .text_size(px(FS_XS))
        .text_color(color)
        .child(Icon::new(icon).with_size(px(12.)))
        .child(label.into())
}

/// The dock pill shape: 28px tall, fully round, a hairline border over
/// the surface, muted small text that lifts onto the raised fill with
/// a stronger border while hovered or open.
pub(crate) fn pill<E: InteractiveElement + Styled>(el: E, pal: &Palette) -> E {
    let (raised, ink, line_strong) = (pal.raised, pal.ink, pal.line_strong);
    el.flex()
        .h(px(28.))
        .px(px(10.))
        .gap(px(SP_3))
        .flex_none()
        .max_w(px(320.))
        .items_center()
        .rounded(px(R_FULL))
        .border_1()
        .border_color(pal.line)
        .bg(pal.surface)
        .text_size(px(FS_XS))
        .text_color(pal.muted)
        .hover(move |style| style.bg(raised).border_color(line_strong).text_color(ink))
}

/// An on/off switch as the design draws it: a 36 by 20 pill with its
/// knob on the right when on. The click is the caller's.
pub(crate) fn switch(id: impl Into<SharedString>, on: bool, pal: &Palette) -> Stateful<Div> {
    div()
        .id(id.into())
        .w(px(36.))
        .h(px(20.))
        .flex_none()
        .rounded(px(R_FULL))
        .bg(if on { pal.accent } else { pal.fill_hover })
        .relative()
        .cursor_pointer()
        .child(
            div()
                .absolute()
                .top(px(2.))
                .left(px(if on { 18. } else { 2. }))
                .size(px(16.))
                .rounded(px(R_FULL))
                .bg(gpui_kit::white()),
        )
}

/// Where the focus goes back to when an overlay closes: the element
/// that held it when the overlay opened, or none. Without it the focus
/// stays on the overlay's own handle, which no longer renders, and
/// every action a button or shortcut dispatches from the focus is
/// lost.
#[derive(Default)]
pub(crate) struct FocusReturn(Option<(gpui_kit::AnyWindowHandle, Option<gpui_kit::FocusHandle>)>);

impl FocusReturn {
    /// Remembers the focus `window` holds, before the overlay takes it.
    /// An overlay opened again while open keeps the first one.
    pub(crate) fn remember(&mut self, window: &gpui_kit::Window, cx: &gpui_kit::App) {
        if self.0.is_none() {
            self.0 = Some((window.window_handle(), window.focused(cx)));
        }
    }

    /// Hands the focus back, once the current update is done.
    pub(crate) fn restore(&mut self, cx: &mut gpui_kit::App) {
        let Some((handle, focus)) = self.0.take() else {
            return;
        };
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, cx| match &focus {
                Some(focus) => window.focus(focus, cx),
                None => window.blur(cx),
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{cost_label, design_hash};
    use kage_client::wire::Cost;

    fn cost(amount: f64, currency: &str) -> Cost {
        Cost {
            amount,
            currency: currency.to_owned(),
        }
    }

    #[test]
    fn cost_label_renders_one_currency_as_one_total() {
        assert_eq!(
            cost_label(&[cost(1.234, "USD")], 2).as_deref(),
            Some("USD 1.23")
        );
        assert_eq!(cost_label(&[], 2), None);
    }

    #[test]
    fn cost_label_keeps_mixed_currencies_as_subtotals() {
        let costs = [cost(1.0, "USD"), cost(2.5, "EUR")];
        assert_eq!(
            cost_label(&costs, 2).as_deref(),
            Some("USD 1.00 + EUR 2.50")
        );
    }

    #[test]
    fn design_hash_matches_the_design_values() {
        assert_eq!(design_hash("explore"), 1_309_148_525);
        assert_eq!(design_hash(""), 0);
    }
}
