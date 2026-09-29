//! The window that carries the hard things in one view: a 5,000 row
//! variable-height virtual list, a multi-line composer, a markdown view
//! and the kage shadow theme.

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::text::markdown;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{VirtualListScrollHandle, h_flex, v_flex, v_virtual_list};
use gpui_kit::{
    App, AppContext, Context, Div, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, ScrollStrategy, SharedString, Size, StatefulInteractiveElement,
    Styled, TitlebarOptions, Window, WindowOptions, div, px, relative, size,
};

use crate::Launch;

/// Rows the list starts with.
const INITIAL_ROWS: usize = 5000;

/// Row heights stay variable and deterministic, which is what the virtual
/// list needs: it scrolls by precomputed sizes and renders only the
/// visible range.
fn row_height(ix: usize) -> Pixels {
    px((28 + (ix * 89) % 113) as f32)
}

fn sizes_for(heights: &[Pixels]) -> Rc<Vec<Size<Pixels>>> {
    Rc::new(heights.iter().map(|h| size(px(0.), *h)).collect())
}

const MARKDOWN_DOC: &str = r#"# kage shadow

A **bold statement**, some *emphasis*, and a `code span` beside plain text.

## Lists and structure

- first bullet
- second bullet
- third bullet

1. ordered one
2. ordered two

## Code

An inline `let answer = 42` and a fenced block:

```rust
fn main() {
    println!("hello from kage");
}
```

## Links and text

A [link](https://example.com) plus a long paragraph that wraps across
several lines so the pane has realistic body text to lay out. The client
renders agent answers through this view, so headings, emphasis, inline
code and fenced code blocks are the shapes that must read right under
the kage shadow palette.
"#;

/// Opens the spike window and returns a handle for automated runs.
pub fn open(cx: &mut App, launch: &Launch) -> SpikeHandle {
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("kage client".into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(960.), px(640.))),
        ..Default::default()
    };
    let stream = launch.stream;
    let (_, view) = gpui_kit::open_window(options, cx, move |window, cx| {
        cx.new(|cx| SpikeWindow::new(window, cx, stream))
    })
    .expect("failed to open the window");
    SpikeHandle(view)
}

/// A handle on the open window, so automated runs can read its counters.
pub struct SpikeHandle(Entity<SpikeWindow>);

impl SpikeHandle {
    /// The total row count and the streamed append count.
    pub fn counts(&self, cx: &App) -> (usize, usize) {
        let spike = self.0.read(cx);
        (spike.heights.len(), spike.streamed)
    }
}

/// The root view of the window.
struct SpikeWindow {
    heights: Vec<Pixels>,
    sizes: Rc<Vec<Size<Pixels>>>,
    list_scroll: VirtualListScrollHandle,
    composer: Entity<TextareaState>,
    streaming: bool,
    streamed: usize,
}

impl SpikeWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>, stream: bool) -> Self {
        let heights: Vec<Pixels> = (0..INITIAL_ROWS).map(row_height).collect();
        let sizes = sizes_for(&heights);
        let composer =
            cx.new(|cx| TextareaState::new(window, cx).placeholder("Message the agent..."));

        cx.spawn(async move |this, cx| {
            let frame = Duration::from_secs_f64(1.0 / 30.0);
            loop {
                cx.background_executor().timer(frame).await;
                if this
                    .update(cx, |spike, cx| {
                        if spike.streaming {
                            spike.append_row(cx);
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();

        Self {
            heights,
            sizes,
            list_scroll: VirtualListScrollHandle::new(),
            composer,
            streaming: stream,
            streamed: 0,
        }
    }

    fn append_row(&mut self, cx: &mut Context<Self>) {
        let ix = self.heights.len();
        self.heights.push(row_height(ix));
        self.sizes = sizes_for(&self.heights);
        self.streamed += 1;
        self.list_scroll.scroll_to_bottom();
        cx.notify();
    }

    fn set_streaming(&mut self, streaming: bool, cx: &mut Context<Self>) {
        self.streaming = streaming;
        cx.notify();
    }

    fn top_bar(&self, view: Entity<Self>, cx: &Context<Self>) -> impl IntoElement {
        let stream_label = if self.streaming {
            "Stream: on"
        } else {
            "Stream: off"
        };
        let stream_view = view.clone();
        let jump_view = view.clone();
        let append_view = view;
        h_flex()
            .h(px(44.))
            .px_3()
            .gap_2()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().font_weight(FontWeight::MEDIUM).child("kage client"))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("stream")
                            .label(stream_label)
                            .on_click(move |_, _, cx| {
                                stream_view.update(cx, |spike, cx| {
                                    spike.set_streaming(!spike.streaming, cx);
                                });
                            }),
                    )
                    .child(Button::new("jump").label("Jump to row 2500").on_click(
                        move |_, _, cx| {
                            jump_view.update(cx, |spike, _| {
                                spike
                                    .list_scroll
                                    .scroll_to_item(2500, ScrollStrategy::Center);
                            });
                        },
                    ))
                    .child(
                        Button::new("append")
                            .label("Append row")
                            .on_click(move |_, _, cx| {
                                append_view.update(cx, |spike, cx| spike.append_row(cx));
                            }),
                    ),
            )
    }

    fn list_pane(&self, view: Entity<Self>, cx: &Context<Self>) -> impl IntoElement {
        let handle = self.list_scroll.clone();
        let sizes = self.sizes.clone();
        div()
            .w(relative(0.58))
            .h_full()
            .min_h_0()
            .overflow_hidden()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                v_virtual_list(view, "rows", sizes, move |spike, range, _, cx| {
                    range
                        .map(|ix| render_row(ix, spike.heights[ix], cx))
                        .collect::<Vec<Div>>()
                })
                .track_scroll(&handle),
            )
    }

    fn markdown_pane(&self) -> impl IntoElement {
        div()
            .id("markdown")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .child(markdown(MARKDOWN_DOC))
    }

    fn composer_pane(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .border_t_1()
            .border_color(cx.theme().border)
            .p_3()
            .gap_2()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("Composer"),
            )
            .child(Textarea::new(&self.composer).h(px(110.)))
    }

    fn status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let composer_bytes = self.composer.read(cx).value().len();
        div()
            .h(px(26.))
            .px_3()
            .flex()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .child(SharedString::from(format!(
                "{} rows | {} streamed | composer {} bytes",
                self.heights.len(),
                self.streamed,
                composer_bytes
            )))
    }
}

fn render_row(ix: usize, height: Pixels, cx: &Context<SpikeWindow>) -> Div {
    let theme = cx.theme().colors;
    let background = if ix.is_multiple_of(2) {
        theme.list
    } else {
        theme.list_even
    };
    let is_streamed = ix >= INITIAL_ROWS;

    let mut row = div()
        .h(height)
        .w_full()
        .px_3()
        .flex()
        .flex_col()
        .justify_center()
        .gap_1()
        .bg(background)
        .border_b_1()
        .border_color(theme.border);

    let mut head = h_flex().justify_between().items_center().child(
        div()
            .text_color(theme.foreground)
            .child(SharedString::from(format!("Row {ix}"))),
    );
    if is_streamed {
        head = head.child(
            div()
                .text_size(px(12.))
                .text_color(theme.primary)
                .child(SharedString::from(format!("+{}", ix - INITIAL_ROWS + 1))),
        );
    }
    row = row.child(head);

    if height >= px(72.) {
        row = row.child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child("detail line shown because the row is tall"),
        );
    }
    row
}

impl Render for SpikeWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.top_bar(view.clone(), cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.list_pane(view, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .child(self.markdown_pane())
                            .child(self.composer_pane(cx)),
                    ),
            )
            .child(self.status_bar(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{INITIAL_ROWS, row_height};

    #[test]
    fn row_heights_stay_variable_and_bounded() {
        let heights: Vec<u32> = (0..INITIAL_ROWS)
            .map(|ix| u32::from(row_height(ix)))
            .collect();
        assert_eq!(heights.len(), INITIAL_ROWS);
        assert!(heights.iter().all(|h| (28..=140).contains(h)));
        assert!(heights.windows(2).any(|pair| pair[0] != pair[1]));
        assert_eq!(heights[0], heights[113]);
    }
}
