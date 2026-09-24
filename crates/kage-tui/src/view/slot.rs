//! Slot painting: the header, the activity row, the input pill, the
//! footer and the start screen, composed from their [`SlotSpec`]s.
//!
//! Built-in components render here from frame state and carry their own
//! spacing. Lua components and span items paint the lines the plugin
//! runtime retained, so painting never calls Lua. A spec's `sep` goes
//! between two adjacent items that both produced output, except after
//! `working`, which is a lead-in indicator followed by one space.

#[allow(clippy::wildcard_imports)] // free-fn split: shares the parent view module scope
use super::*;

use kage_plugin::{ChromeLine, SlotItem, SlotName, SlotSpec};

use super::modeline::{format_token_count, mode_label, spinner_frame};

/// Styles one slot paints with.
pub(super) struct Styles {
    /// Base under Lua component and span items.
    pub base: Style,
    /// Built-in component text.
    pub text: Style,
    /// Emphasized built-in component text.
    pub strong: Style,
    /// Padding and the row background.
    pub pad: Style,
    /// The separator.
    pub sep: Style,
    /// The `hint` component.
    pub hint: Style,
}

impl Styles {
    fn uniform(style: Style) -> Self {
        Self {
            base: style,
            text: style,
            strong: style.add_modifier(Modifier::BOLD),
            pad: Style::default(),
            sep: style,
            hint: style,
        }
    }
}

/// Frame state the built-in components read.
pub(super) struct Sources<'a> {
    pub status: &'a StatusCtx<'a>,
    /// Session usage, only when it has anything to show.
    pub usage: Option<&'a SessionUsage>,
    pub mode: Mode,
    /// Whether the editor is modeless, which hides the `mode` pill.
    pub modeless: bool,
    /// Whether `!` shell mode is armed.
    pub shell: bool,
}

impl<'a> Sources<'a> {
    pub(super) fn new(
        status: &'a StatusCtx<'a>,
        usage: Option<&'a SessionUsage>,
        input: &InputState,
    ) -> Self {
        let usage = usage.filter(|u| {
            !u.model.is_empty() || u.total_tokens() > 0 || u.current_context > 0 || u.working
        });
        Self {
            status,
            usage,
            mode: input.mode(),
            modeless: input.is_modeless(),
            shell: input.shell_armed(),
        }
    }
}

/// Whether `slot`'s row paints anything this frame. Runs the same item
/// painters as the row itself, without a frame.
pub(super) fn row_has_content(slot: SlotName, src: &Sources<'_>) -> bool {
    let spec = src.status.slots.get(slot);
    let styles = Styles::uniform(Style::default());
    let mut piece = Vec::new();
    spec.left.iter().chain(&spec.right).any(|item| {
        piece.clear();
        push_item(item, src, &styles, &mut piece);
        piece.iter().any(|span| !span.content.is_empty())
    })
}

/// Paint the header slot into `area`.
pub(super) fn render_header(frame: &mut Frame, area: Rect, src: &Sources<'_>) {
    let theme = crate::theme::current();
    let text = Style::default().fg(theme.muted_fg).bg(theme.status_bg);
    let styles = Styles {
        base: Style::default().fg(theme.status_dim_fg).bg(theme.status_bg),
        text,
        strong: text.add_modifier(Modifier::BOLD),
        pad: Style::default().bg(theme.status_bg),
        sep: text,
        hint: text,
    };
    let spec = src.status.slots.get(SlotName::Header);
    paint_row(frame, area, &spec, src, &styles);
}

/// Paint the activity slot, the working row, into `area`.
pub(super) fn render_activity(frame: &mut Frame, area: Rect, src: &Sources<'_>) {
    let theme = crate::theme::current();
    let text = Style::default()
        .fg(theme.muted_fg)
        .patch(theme.group_style("KageWorking"));
    let mut styles = Styles::uniform(text);
    styles.hint = Style::default().fg(theme.input_hint_fg);
    let spec = src.status.slots.get(SlotName::Activity);
    paint_row(frame, area, &spec, src, &styles);
}

/// Paint the footer slot into `area`.
pub(super) fn render_footer(frame: &mut Frame, area: Rect, src: &Sources<'_>) {
    let theme = crate::theme::current();
    let text = Style::default().fg(theme.muted_fg);
    let styles = Styles {
        base: text,
        text,
        strong: text.add_modifier(Modifier::BOLD),
        pad: Style::default(),
        sep: text,
        hint: Style::default().fg(theme.input_hint_fg),
    };
    let spec = src.status.slots.get(SlotName::Footer);
    paint_row(frame, area, &spec, src, &styles);
}

/// The input pill as the spans of the top rule's left and right
/// titles. `rule` styles plain text and `strong` the emphasized parts.
pub(super) fn pill_titles(
    src: &Sources<'_>,
    rule: Style,
    strong: Style,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let styles = Styles {
        base: rule,
        text: rule,
        strong,
        pad: rule,
        sep: rule,
        hint: rule,
    };
    let spec = src.status.slots.get(SlotName::InputPill);
    let left = row_spans(&spec.left, &spec.sep, src, &styles);
    let right = row_spans(&spec.right, &spec.sep, src, &styles);
    (left, right)
}

/// Paint the start slot centered in the empty buffer region.
pub(super) fn render_start(frame: &mut Frame, area: Rect, src: &Sources<'_>) {
    let spec = src.status.slots.get(SlotName::Start);
    if spec.lines.is_empty() || area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    let styles = Styles::uniform(Style::default().fg(theme.muted_fg));
    let mut lines = Vec::new();
    for item in &spec.lines {
        match item {
            SlotItem::Lua(component) => {
                let base = with_hl(styles.base, component.hl());
                lines.extend(chrome_lines_to_ratatui(&component.lines(), base));
            }
            other => {
                let mut spans = Vec::new();
                push_item(other, src, &styles, &mut spans);
                lines.push(Line::from(spans));
            }
        }
    }
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let rect = Rect::new(
        area.x,
        area.y + (area.height - height) / 2,
        area.width,
        height,
    );
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), rect);
}

fn paint_row(frame: &mut Frame, area: Rect, spec: &SlotSpec, src: &Sources<'_>, styles: &Styles) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut spans = row_spans(&spec.left, &spec.sep, src, styles);
    let right = row_spans(&spec.right, &spec.sep, src, styles);
    let used: usize = spans.iter().chain(&right).map(Span::width).sum();
    let pad = usize::from(area.width).saturating_sub(used);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), styles.pad));
    }
    spans.extend(right);
    let paragraph = Paragraph::new(Line::from(spans))
        .alignment(Alignment::Left)
        .style(styles.pad);
    frame.render_widget(paragraph, area);
}

fn row_spans(
    items: &[SlotItem],
    sep: &str,
    src: &Sources<'_>,
    styles: &Styles,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut previous: Option<&SlotItem> = None;
    let mut piece = Vec::new();
    for item in items {
        piece.clear();
        push_item(item, src, styles, &mut piece);
        if piece.iter().all(|span| span.content.is_empty()) {
            continue;
        }
        match previous {
            Some(SlotItem::Builtin("working")) => {
                out.push(Span::styled(" ".to_owned(), styles.pad));
            }
            Some(_) if !sep.is_empty() => out.push(Span::styled(sep.to_owned(), styles.sep)),
            _ => {}
        }
        previous = Some(item);
        out.append(&mut piece);
    }
    out
}

fn with_hl(base: Style, hl: Option<&str>) -> Style {
    match hl {
        Some(group) => base.patch(crate::theme::current().group_style(group)),
        None => base,
    }
}

fn push_item(item: &SlotItem, src: &Sources<'_>, styles: &Styles, out: &mut Vec<Span<'static>>) {
    match item {
        SlotItem::Builtin(name) => push_builtin(name, src, styles, out),
        SlotItem::Text(span) => {
            let line = ChromeLine {
                spans: vec![span.clone()],
            };
            push_line(&line, styles.base, out);
        }
        SlotItem::Lua(component) => {
            if let Some(line) = component.lines().first() {
                push_line(line, with_hl(styles.base, component.hl()), out);
            }
        }
    }
}

fn push_line(line: &ChromeLine, base: Style, out: &mut Vec<Span<'static>>) {
    for painted in chrome_lines_to_ratatui(std::slice::from_ref(line), base) {
        out.extend(painted.spans);
    }
}

/// Paint the built-in component `name`. Pushing nothing means it has
/// no output this frame.
pub(super) fn push_builtin(
    name: &str,
    src: &Sources<'_>,
    styles: &Styles,
    out: &mut Vec<Span<'static>>,
) {
    let status = src.status;
    match name {
        "brand" => out.push(Span::styled(" kage".to_owned(), styles.text)),
        "title" => {
            if let Some(title) = status.title.filter(|t| !t.is_empty()) {
                out.push(Span::styled(format!(" {title}"), styles.text));
            }
        }
        "model" => {
            let model = status
                .model
                .or_else(|| src.usage.map(|u| u.model.as_str()))
                .filter(|m| !m.is_empty());
            if let Some(model) = model {
                out.push(Span::styled(model.to_owned(), styles.text));
            }
        }
        "widgets" => {
            let texts = status
                .plugin_widgets
                .iter()
                .chain(status.plugin_status.iter().map(|(_, text)| text));
            for text in texts.filter(|t| !t.is_empty()) {
                out.push(Span::styled(format!("{text}  "), styles.text));
            }
        }
        "search" => {
            if let Some(count) = status.search_match_count {
                let theme = crate::theme::current();
                let style = styles
                    .pad
                    .fg(theme.match_color)
                    .add_modifier(Modifier::BOLD);
                out.push(Span::styled(
                    format!("{}  ", search_count_label(count)),
                    style,
                ));
            }
        }
        "session" => {
            if let Some(sid) = status.session_id.filter(|s| !s.is_empty()) {
                out.push(Span::styled(format!("#{sid} "), styles.text));
            }
        }
        "activity" => {
            if let Some(text) = status.activity.filter(|t| !t.is_empty()) {
                out.push(Span::styled(format!("  {text}"), styles.text));
            }
        }
        "working" | "context" | "tokens" | "thinking" | "permission" => {
            if let Some(u) = src.usage {
                push_usage(name, u, styles, out);
            }
        }
        "mode" => {
            let label = if src.shell {
                "shell"
            } else if src.modeless {
                return;
            } else {
                match src.mode {
                    Mode::Normal => "NORMAL",
                    Mode::Insert => "INSERT",
                    Mode::Visual => "VISUAL",
                }
            };
            out.push(Span::styled(label.to_owned(), styles.strong));
        }
        "hint" => {
            if let Some(hint) = status.hint.filter(|h| !h.is_empty()) {
                out.push(Span::styled(format!("  {hint}"), styles.hint));
            }
        }
        "cwd" => {
            if let Some(cwd) = status.cwd.filter(|c| !c.is_empty()) {
                out.push(Span::styled(cwd.to_owned(), styles.text));
            }
        }
        "version" => out.push(Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            styles.text,
        )),
        _ => {}
    }
}

/// `match 2/5`, `match -/5` off the matches, or `no match`.
pub(super) fn search_count_label((current, total): (usize, usize)) -> String {
    if total == 0 {
        "no match".to_owned()
    } else if current == 0 {
        format!("match -/{total}")
    } else {
        format!("match {current}/{total}")
    }
}

/// Paint a built-in component that reads the session usage.
fn push_usage(name: &str, u: &SessionUsage, styles: &Styles, out: &mut Vec<Span<'static>>) {
    match name {
        "working" if u.working => {
            out.push(Span::styled(spinner_frame().to_owned(), styles.strong));
        }
        "context" => {
            if u.context_window > 0 {
                #[allow(clippy::cast_precision_loss)]
                let pct =
                    (u.current_context as f64 / u.context_window as f64 * 100.0).clamp(0.0, 999.9);
                out.push(Span::styled(format!("{pct:.0}% ctx"), styles.text));
            } else if u.current_context > 0 {
                out.push(Span::styled(
                    format!("{} ctx", format_token_count(u.current_context)),
                    styles.text,
                ));
            }
        }
        "tokens" => {
            let total = u.total_tokens();
            if total > 0 {
                out.push(Span::styled(
                    format!("{} tok", format_token_count(total)),
                    styles.text,
                ));
            }
            if u.total_cost > 0.0 {
                let lead = if total > 0 { " " } else { "" };
                out.push(Span::styled(
                    format!("{lead}${:.2}", u.total_cost),
                    styles.text,
                ));
            }
        }
        "thinking" => {
            if let Some(level) = u.thinking_level.filter(|l| !l.is_off()) {
                out.push(Span::styled(
                    format!("thinking {}", level.label()),
                    styles.text,
                ));
            }
        }
        "permission" => {
            if let Some(mode) = u.permission_mode {
                out.push(Span::styled(
                    format!("{} mode", mode_label(mode)),
                    styles.text,
                ));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_component_has_an_implementation() {
        let usage = SessionUsage {
            model: "m".into(),
            working: true,
            input_tokens: 10,
            context_window: 10,
            thinking_level: Some(kage_core::ThinkingLevel::High),
            permission_mode: Some(kage_core::permissions::PermissionAction::Ask),
            ..SessionUsage::default()
        };
        let widgets = ["w".to_owned()];
        let status = StatusCtx {
            model: Some("m"),
            session_id: Some("s"),
            title: Some("t"),
            search_match_count: Some((1, 2)),
            plugin_widgets: &widgets,
            hint: Some("g"),
            activity: Some("Working"),
            cwd: Some("/w"),
            ..StatusCtx::default()
        };
        let src = Sources::new(&status, Some(&usage), &InputState::new());
        let styles = Styles::uniform(Style::default());
        for name in kage_plugin::slots::BUILTIN_COMPONENTS {
            let mut out = Vec::new();
            push_builtin(name, &src, &styles, &mut out);
            assert!(!out.is_empty(), "{name} painted nothing");
        }
    }

    fn painted(name: &str, usage: &SessionUsage, input: &InputState) -> String {
        let status = StatusCtx::default();
        let src = Sources::new(&status, Some(usage), input);
        let mut out = Vec::new();
        push_builtin(name, &src, &Styles::uniform(Style::default()), &mut out);
        out.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn footer_components_read_as_plain_words() {
        let usage = SessionUsage {
            model: "fake:m".into(),
            input_tokens: 12_000,
            output_tokens: 2_000,
            current_context: 24_000,
            context_window: 200_000,
            total_cost: 0.02,
            thinking_level: Some(kage_core::ThinkingLevel::High),
            permission_mode: Some(kage_core::permissions::PermissionAction::Ask),
            ..SessionUsage::default()
        };
        let input = InputState::new();
        assert_eq!(painted("context", &usage, &input), "12% ctx");
        assert_eq!(painted("tokens", &usage, &input), "14k tok $0.02");
        assert_eq!(painted("thinking", &usage, &input), "thinking high");
        assert_eq!(painted("permission", &usage, &input), "ask mode");
        assert_eq!(painted("model", &usage, &input), "fake:m");
        let quiet = SessionUsage {
            model: "fake:m".into(),
            thinking_level: Some(kage_core::ThinkingLevel::Off),
            ..SessionUsage::default()
        };
        assert_eq!(painted("thinking", &quiet, &input), "");
        assert_eq!(painted("tokens", &quiet, &input), "");
        assert_eq!(painted("permission", &quiet, &input), "");
    }

    #[test]
    fn mode_is_empty_in_modeless_and_a_word_in_vim() {
        let usage = SessionUsage::default();
        let mut input = InputState::new();
        assert_eq!(painted("mode", &usage, &input), "INSERT");
        input.handle_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Esc,
        ));
        assert_eq!(painted("mode", &usage, &input), "NORMAL");
        input.set_modeless(true);
        assert_eq!(painted("mode", &usage, &input), "");
    }
}
