//! The main panel: the gate banner over the active session's
//! transcript.
//!
//! The transcript is the virtual list the spike proved out, now fed
//! by store state: agent text renders as markdown, tool calls and
//! turn boundaries as compact rows. Row heights are deterministic
//! estimates, which is all the virtual list needs.

use std::rc::Rc;

use gpui_kit::component::button::Button;
use gpui_kit::component::text::markdown;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{VirtualListScrollHandle, h_flex, v_flex, v_virtual_list};
use gpui_kit::{
    Context, Div, Entity, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Render, SharedString, Size, Styled as _, Window, div, px, size,
};

use crate::store::Store;
use kage_client::{Session, TranscriptItem};

/// One estimated text line, for row sizing.
const LINE: f32 = 18.0;
/// The padding every row carries beyond its text.
const BASE: f32 = 30.0;
/// Characters a line holds before the estimate wraps.
const COLUMNS: usize = 72;

/// The estimated lines `text` wraps to.
fn text_lines(text: &str) -> usize {
    let breaks = text.matches('\n').count() + 1;
    let wrapped = text.chars().count().div_ceil(COLUMNS).max(1);
    breaks.max(wrapped)
}

/// The estimated rendered height of one transcript item.
#[must_use]
pub fn item_height(item: &TranscriptItem) -> Pixels {
    let lines = |text: &str| text_lines(text) as f32 * LINE;
    match item {
        TranscriptItem::User { content } => px(BASE + lines(content.as_text().unwrap_or(""))),
        TranscriptItem::Assistant { text } | TranscriptItem::Thinking { text } => {
            px(BASE + lines(text))
        }
        TranscriptItem::ToolCall(call) => {
            let output = call.text();
            px(BASE
                + if output.is_empty() {
                    0.0
                } else {
                    lines(&output)
                })
        }
        TranscriptItem::TurnEnd { .. } => px(24.0),
        TranscriptItem::Notice { text, .. } => px(BASE + lines(text)),
        TranscriptItem::Compaction { .. } => px(BASE),
        TranscriptItem::Plan { entries } => px(BASE + entries.len() as f32 * LINE),
    }
}

/// The size table the virtual list scrolls by.
fn sizes_for(items: &[TranscriptItem]) -> Rc<Vec<Size<Pixels>>> {
    Rc::new(
        items
            .iter()
            .map(|item| size(px(0.), item_height(item)))
            .collect(),
    )
}

/// The status line of one tool call.
fn call_status(status: kage_client::wire::ToolCallStatus) -> &'static str {
    match status {
        kage_client::wire::ToolCallStatus::Pending => "pending",
        kage_client::wire::ToolCallStatus::InProgress => "running",
        kage_client::wire::ToolCallStatus::Completed => "done",
        kage_client::wire::ToolCallStatus::Failed => "failed",
    }
}

/// The middle panel.
pub struct TranscriptView {
    store: Entity<Store>,
    scroll: VirtualListScrollHandle,
    sizes: Rc<Vec<Size<Pixels>>>,
    rendered: usize,
}

impl TranscriptView {
    /// A transcript following `store`.
    #[must_use]
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |this, _, cx| {
            let count = this
                .store
                .read(cx)
                .active_session()
                .map(|session| session.items.len())
                .unwrap_or(0);
            if count > this.rendered {
                this.rendered = count;
                this.scroll.scroll_to_bottom();
            }
            cx.notify();
        })
        .detach();
        Self {
            store,
            scroll: VirtualListScrollHandle::new(),
            sizes: Rc::default(),
            rendered: 0,
        }
    }

    /// The dismissible banner the gate report raises.
    fn banner(&self, cx: &mut Context<Self>) -> Option<Div> {
        let store = self.store.read(cx);
        if store.gate().is_clean() || store.gate_dismissed() {
            return None;
        }
        let theme = cx.theme().colors;
        let lines = store.gate().lines().join("\n");
        let store_handle = self.store.clone();
        Some(
            h_flex()
                .w_full()
                .px_3()
                .py_2()
                .gap_3()
                .items_center()
                .justify_between()
                .bg(theme.warning)
                .border_b_1()
                .border_color(theme.warning)
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.warning_foreground)
                        .child(SharedString::from(format!(
                            "this agent is too old or missing capabilities: {lines}"
                        ))),
                )
                .child(
                    Button::new("dismiss-gate")
                        .label("Dismiss")
                        .on_click(move |_, _, cx| {
                            store_handle.update(cx, |store, cx| {
                                store.dismiss_gate();
                                cx.notify();
                            });
                        }),
                ),
        )
    }

    /// One transcript row.
    fn render_item(&self, item: &TranscriptItem, cx: &Context<Self>) -> Div {
        let theme = cx.theme().colors;
        match item {
            TranscriptItem::User { content } => div()
                .w_full()
                .px_3()
                .py_1()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child("you"),
                )
                .child(div().text_color(theme.foreground).child(SharedString::from(
                    content.as_text().unwrap_or("").to_owned(),
                ))),
            TranscriptItem::Assistant { text } => div()
                .w_full()
                .px_3()
                .py_1()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(theme.foreground).child(markdown(text))),
            TranscriptItem::Thinking { text } => div().w_full().px_3().py_1().child(
                div()
                    .italic()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(text.clone())),
            ),
            TranscriptItem::ToolCall(call) => {
                let label = call_status(call.status);
                let output = call.text();
                let mut row = v_flex().w_full().px_3().py_1().gap_1().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.foreground)
                                .child(SharedString::from(call.title.clone())),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(label),
                        ),
                );
                if !output.is_empty() {
                    row = row.child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(output)),
                    );
                }
                row
            }
            TranscriptItem::TurnEnd { reason } => {
                let why = match reason {
                    Some(kage_client::wire::TurnReason::ToolCalls) => "turn ends, tools follow",
                    Some(kage_client::wire::TurnReason::NoToolCalls) => "turn ends",
                    _ => "turn ends",
                };
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().h(px(1.)).bg(theme.border))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(why),
                    )
            }
            TranscriptItem::Notice { tone, text } => {
                let color = match tone {
                    kage_client::wire::NoticeTone::Info => theme.info,
                    kage_client::wire::NoticeTone::Warn => theme.warning,
                    kage_client::wire::NoticeTone::Error => theme.danger,
                    kage_client::wire::NoticeTone::Success => theme.success,
                };
                div()
                    .w_full()
                    .px_3()
                    .py_1()
                    .text_size(px(13.))
                    .text_color(color)
                    .child(SharedString::from(text.clone()))
            }
            TranscriptItem::Compaction { .. } => div()
                .w_full()
                .px_3()
                .py_1()
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child("older context was compacted"),
            TranscriptItem::Plan { entries } => {
                let mut rows = v_flex().w_full().px_3().py_1().gap_0().child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child("plan"),
                );
                for entry in entries {
                    let status = entry
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let mark = match status {
                        "completed" => "x",
                        "in_progress" => "~",
                        _ => " ",
                    };
                    let text = entry
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    rows = rows.child(
                        h_flex()
                            .gap_2()
                            .child(div().w(px(12.)).text_color(theme.primary).child(mark))
                            .child(
                                div()
                                    .text_color(theme.foreground)
                                    .child(SharedString::from(text.to_owned())),
                            ),
                    );
                }
                rows
            }
        }
    }

    /// The placeholder shown while the session has nothing to show.
    fn placeholder(&self, cx: &Context<Self>) -> Div {
        let theme = cx.theme().colors;
        let text = match self.store.read(cx).active_session() {
            None => "no session yet; the agent answers here once one opens",
            Some(session) if session.items.is_empty() => {
                "say hello to start the run; the transcript plays out here"
            }
            Some(_) => "",
        };
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme.muted_foreground)
            .child(text)
    }
}

/// The active session a transcript view renders, borrowed for one
/// frame.
fn session_of(store: &Store) -> Option<&Session> {
    store.active_session()
}

impl Render for TranscriptView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count;
        let sizes;
        let has_items;
        {
            let store = self.store.read(cx);
            let session = store.active_session();
            count = session.map(|s| s.items.len()).unwrap_or(0);
            sizes = session.map(|s| sizes_for(&s.items)).unwrap_or_default();
            has_items = session.is_some_and(|s| !s.items.is_empty());
        }
        self.rendered = count;
        self.sizes = sizes.clone();
        let banner = self.banner(cx);
        let view = cx.entity();
        let scroll = self.scroll.clone();

        let mut panel = v_flex().id("transcript").size_full().min_h_0();
        if let Some(banner) = banner {
            panel = panel.child(banner);
        }
        panel.child(if has_items {
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(
                    v_virtual_list(view, "transcript-rows", sizes, move |this, range, _, cx| {
                        let Some(session) = session_of(this.store.read(cx)) else {
                            return Vec::new();
                        };
                        range
                            .map(|ix| this.render_item(&session.items[ix], cx))
                            .collect::<Vec<Div>>()
                    })
                    .track_scroll(&scroll),
                )
                .into_any_element()
        } else {
            self.placeholder(cx).into_any_element()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{item_height, text_lines};
    use kage_client::TranscriptItem;

    #[test]
    fn estimates_wrap_and_count_breaks() {
        assert_eq!(text_lines("one line"), 1);
        assert_eq!(text_lines("two\nlines"), 2);
        assert_eq!(text_lines(&"x".repeat(200)), 3);
    }

    #[test]
    fn row_heights_stay_variable_and_determined_by_the_item() {
        let short = TranscriptItem::TurnEnd { reason: None };
        let long = TranscriptItem::Assistant {
            text: "a reply that runs across several\nlines of text".into(),
        };
        let tool = TranscriptItem::ToolCall(kage_client::ToolCallItem {
            tool_call_id: "c1".into(),
            title: "read".into(),
            kind: kage_client::wire::ToolKind::Read,
            status: kage_client::wire::ToolCallStatus::Completed,
            input: None,
            content: Vec::new(),
            raw_output: None,
        });
        let heights = [
            u32::from(item_height(&short)),
            u32::from(item_height(&long)),
            u32::from(item_height(&tool)),
        ];
        assert!(heights.iter().all(|h| *h >= 24));
        assert!(
            heights.windows(2).any(|pair| pair[0] != pair[1]),
            "heights vary by item: {heights:?}"
        );
        assert_eq!(heights[1], u32::from(item_height(&long)), "deterministic");
    }
}
