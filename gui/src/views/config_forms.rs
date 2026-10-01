//! Pieces the settings forms share: a labeled field, name and value
//! rows, segmented choices, and how a form learns whether its write
//! landed.
//!
//! Every text field is made with its value as a default rather than set
//! after: setting text lays it out at once, and before the field's first
//! render that layout uses a font the web cannot resolve.

use std::collections::BTreeMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Div, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _,
    Window, div, px,
};

use crate::theme::{FONT_MONO, FS_SM, FS_XS, Palette, R_FULL, R_MD};

/// What the snapshot shows in place of a secret value.
pub(crate) const REDACTED: &str = "<redacted>";

/// A one-line text field holding `value`.
pub(crate) fn text_field(
    window: &mut Window,
    cx: &mut App,
    placeholder: &'static str,
    value: &str,
) -> Entity<InputState> {
    let value = value.to_owned();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

/// A labeled form row: the label with a star when required, the
/// control, and a hint under it.
pub(crate) fn field(
    label: &'static str,
    required: bool,
    hint: Option<&'static str>,
    control: impl IntoElement,
    pal: &Palette,
) -> Div {
    v_flex()
        .gap(px(5.))
        .child(
            h_flex()
                .gap(px(2.))
                .text_size(px(FS_XS))
                .text_color(pal.ink)
                .child(label)
                .when(required, |label| {
                    label.child(div().text_color(pal.danger).child("*"))
                }),
        )
        .child(control)
        .children(hint.map(|hint| div().text_size(px(FS_XS)).text_color(pal.faint).child(hint)))
}

/// A segmented choice: one segment per `(value, label)`, the current
/// one filled. Clicking a segment calls `pick` with its value.
pub(crate) fn segments(
    id: &str,
    choices: &[(&'static str, &'static str)],
    current: &str,
    pick: impl Fn(&'static str, &mut Window, &mut App) + Clone + 'static,
    pal: &Palette,
) -> Div {
    let mut row = h_flex()
        .p(px(2.))
        .gap(px(2.))
        .rounded(px(R_MD))
        .bg(pal.fill)
        .border_1()
        .border_color(pal.line);
    for (value, label) in choices {
        let on = *value == current;
        let pick = pick.clone();
        let value = *value;
        row = row.child(
            div()
                .id(SharedString::from(format!("{id}-{value}")))
                .px(px(10.))
                .h(px(24.))
                .flex()
                .items_center()
                .rounded(px(R_MD))
                .text_size(px(FS_XS))
                .cursor_pointer()
                .text_color(if on { pal.ink_strong } else { pal.muted })
                .when(on, |seg| seg.bg(pal.surface))
                .child(*label)
                .on_click(move |_, window, cx| pick(value, window, cx)),
        );
    }
    row
}

/// The name and value rows of a map a form edits, such as headers or
/// environment variables. A row whose value the snapshot redacted
/// starts blank and keeps the saved value unless one is typed.
pub(crate) struct Pairs {
    rows: Vec<Pair>,
}

struct Pair {
    name: Entity<InputState>,
    value: Entity<InputState>,
    /// The saved value is secret and stays unless a new one is typed.
    kept: bool,
}

impl Pairs {
    /// Rows for `map`, as the snapshot shows it.
    pub(crate) fn new(window: &mut Window, cx: &mut App, map: &BTreeMap<String, String>) -> Self {
        let rows = map
            .iter()
            .map(|(name, value)| {
                let kept = value == REDACTED;
                Pair {
                    name: text_field(window, cx, "NAME", name),
                    value: text_field(
                        window,
                        cx,
                        if kept { "unchanged" } else { "value" },
                        if kept { "" } else { value },
                    ),
                    kept,
                }
            })
            .collect();
        Self { rows }
    }

    /// Adds a blank row.
    pub(crate) fn push(&mut self, window: &mut Window, cx: &mut App) {
        self.rows.push(Pair {
            name: text_field(window, cx, "NAME", ""),
            value: text_field(window, cx, "value", ""),
            kept: false,
        });
    }

    /// Drops row `ix`.
    pub(crate) fn remove(&mut self, ix: usize) {
        if ix < self.rows.len() {
            self.rows.remove(ix);
        }
    }

    /// The map the rows hold. A row without a name drops out, and a kept
    /// row left blank sends the redaction marker, which keeps the saved
    /// value.
    pub(crate) fn values(&self, cx: &App) -> BTreeMap<String, String> {
        self.rows
            .iter()
            .filter_map(|row| {
                let name = row.name.read(cx).value().trim().to_owned();
                let value = row.value.read(cx).value().to_string();
                if name.is_empty() {
                    return None;
                }
                let value = if value.is_empty() && row.kept {
                    REDACTED.to_owned()
                } else {
                    value
                };
                Some((name, value))
            })
            .collect()
    }

    /// The rows with a remove button each, then an add button. `edit`
    /// reaches these rows inside the owning form.
    pub(crate) fn render<T: 'static>(
        &self,
        id: &'static str,
        owner: &Entity<T>,
        edit: fn(&mut T) -> &mut Pairs,
        pal: &Palette,
    ) -> Div {
        let mut list = v_flex().gap(px(6.));
        for (ix, row) in self.rows.iter().enumerate() {
            let owner = owner.clone();
            list = list.child(
                h_flex()
                    .gap(px(6.))
                    .items_center()
                    .child(
                        div()
                            .w(px(180.))
                            .font_family(FONT_MONO)
                            .child(Input::new(&row.name).small()),
                    )
                    .child(div().flex_1().child(Input::new(&row.value).small()))
                    .child(
                        icon_button(format!("{id}-rm-{ix}"), IconName::Trash, pal).on_click(
                            move |_, _, cx| {
                                owner.update(cx, |form, cx| {
                                    edit(form).remove(ix);
                                    cx.notify();
                                });
                            },
                        ),
                    ),
            );
        }
        let owner = owner.clone();
        list.child(
            h_flex().child(
                div()
                    .id(SharedString::from(format!("{id}-add")))
                    .px(px(8.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .rounded(px(R_FULL))
                    .border_1()
                    .border_dashed()
                    .border_color(pal.line)
                    .text_size(px(FS_XS))
                    .text_color(pal.muted)
                    .cursor_pointer()
                    .child(Icon::new(IconName::Plus).with_size(px(12.)))
                    .child("Add")
                    .on_click(move |_, window, cx| {
                        owner.update(cx, |form, cx| {
                            edit(form).push(window, cx);
                            cx.notify();
                        });
                    }),
            ),
        )
    }
}

/// A small square button holding one icon.
pub(crate) fn icon_button(id: String, icon: IconName, pal: &Palette) -> Stateful<Div> {
    let hover = pal.fill_hover;
    div()
        .id(SharedString::from(id))
        .size(px(26.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(R_MD))
        .text_color(pal.faint)
        .cursor_pointer()
        .hover(move |button| button.bg(hover))
        .child(Icon::new(icon).with_size(px(13.)))
}

/// Where a form's save stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Saving {
    /// Nothing sent.
    #[default]
    Idle,
    /// Writes in flight, by request id; the form closes once all land.
    Sent(Vec<u64>),
    /// Why the form cannot save, or why the engine refused.
    Refused(String),
}

impl Saving {
    /// Follows the store's answers to the writes in flight: `true` once
    /// every write landed, a refusal once one failed.
    pub(crate) fn settle(&mut self, store: &crate::store::Store) -> bool {
        let Self::Sent(ids) = self else {
            return false;
        };
        let mut done = true;
        for id in ids.iter() {
            match store.write_outcome(*id) {
                Some(Ok(())) => {}
                Some(Err(why)) => {
                    *self = Self::Refused(why.clone());
                    return false;
                }
                None => done = false,
            }
        }
        done
    }

    /// The line a form shows under its fields: the refusal, or that the
    /// save is on its way.
    pub(crate) fn line(&self, pal: &Palette) -> Option<AnyElement> {
        match self {
            Self::Idle => None,
            Self::Sent(_) => Some(
                div()
                    .text_size(px(FS_XS))
                    .text_color(pal.muted)
                    .child("Saving\u{2026}")
                    .into_any_element(),
            ),
            Self::Refused(why) => Some(
                h_flex()
                    .gap(px(6.))
                    .items_start()
                    .text_size(px(FS_XS))
                    .text_color(pal.danger)
                    .child(Icon::new(IconName::TriangleAlert).with_size(px(12.)))
                    .child(why.clone())
                    .into_any_element(),
            ),
        }
    }
}

/// The header of a form page: a back button and the title.
pub(crate) fn form_head<T: 'static>(
    title: impl Into<SharedString>,
    owner: &Entity<T>,
    back: fn(&mut T, &mut Context<T>),
    pal: &Palette,
) -> Div {
    let owner = owner.clone();
    h_flex()
        .gap(px(8.))
        .items_center()
        .mb(px(14.))
        .child(
            icon_button("form-back".to_owned(), IconName::ChevronLeft, pal)
                .on_click(move |_, _, cx| owner.update(cx, back)),
        )
        .child(
            div()
                .text_size(px(FS_SM + 4.))
                .font_weight(crate::theme::WEIGHT_SEMIBOLD)
                .text_color(pal.ink_strong)
                .child(title.into()),
        )
}
