//! Small controls more than one view draws the same way.

use gpui_kit::component::h_flex;
use gpui_kit::{
    Div, FontWeight, InteractiveElement as _, ParentElement as _, SharedString, Stateful,
    Styled as _, div, px,
};

use crate::theme::{FS_XS, Palette, R_FULL, R_MD, SP_3};

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
