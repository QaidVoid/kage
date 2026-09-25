//! Slot painting: the header, the activity row, the input pill, the
//! footer and the start card, composed from their [`SlotSpec`]s.
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
    /// Width of the frame, which the `breadcrumb` fits its task into.
    pub width: u16,
}

impl<'a> Sources<'a> {
    pub(super) fn new(
        status: &'a StatusCtx<'a>,
        usage: Option<&'a SessionUsage>,
        input: &InputState,
        width: u16,
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
            width,
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

/// Columns before the start card's text, and after its hints.
const START_INDENT: usize = 3;
/// Width of the label column of the start card's labeled rows.
const START_LABEL_WIDTH: usize = 14;
/// Recent sessions the start card lists.
pub(crate) const START_SESSIONS: usize = 3;
/// Notices the start card keeps when it runs out of rows.
const START_NOTICES_KEPT: usize = 2;

/// Which start card lines go when the card does not fit, in drop
/// order. `Always` lines stay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keep {
    Tip,
    Sessions,
    ExtraNotices,
    Always,
}

/// Paint the start slot bottom-aligned in `area`, the rows between the
/// last notice block and the input. Span lines, such as the tip, wrap
/// at the card width. Lines drop when it does not fit: the tip first,
/// then the sessions, then notices past the first two.
pub(super) fn render_start(frame: &mut Frame, area: Rect, src: &Sources<'_>) {
    let spec = src.status.slots.get(SlotName::Start);
    if spec.lines.is_empty() || area.height == 0 || area.width == 0 {
        return;
    }
    let height = usize::from(area.height);
    let mut lines = start_lines(&spec, src, usize::from(area.width));
    let trim = |lines: &mut Vec<(Keep, Line<'static>)>| {
        while lines.last().is_some_and(|(_, line)| line.width() == 0) {
            lines.pop();
        }
    };
    trim(&mut lines);
    for drop in [Keep::Tip, Keep::Sessions, Keep::ExtraNotices] {
        if lines.len() <= height {
            break;
        }
        lines.retain(|(keep, _)| *keep != drop);
        trim(&mut lines);
    }
    lines.truncate(height);
    let rows = u16::try_from(lines.len()).unwrap_or(area.height);
    let rect = Rect::new(area.x, area.y + area.height - rows, area.width, rows);
    let lines: Vec<Line<'static>> = lines.into_iter().map(|(_, line)| line).collect();
    frame.render_widget(Paragraph::new(lines), rect);
}

fn start_lines(spec: &SlotSpec, src: &Sources<'_>, width: usize) -> Vec<(Keep, Line<'static>)> {
    let theme = crate::theme::current();
    let value = Style::default().fg(theme.assistant_fg);
    let styles = Styles {
        base: Style::default().fg(theme.muted_fg),
        text: value,
        strong: value.add_modifier(Modifier::BOLD),
        pad: Style::default(),
        sep: Style::default().fg(theme.muted_fg),
        hint: Style::default().fg(theme.input_hint_fg),
    };
    let indent = || Span::raw(" ".repeat(START_INDENT));
    let mut out = Vec::new();
    for item in &spec.lines {
        match item {
            SlotItem::Builtin("brand") => out.push((
                Keep::Always,
                Line::from(vec![
                    indent(),
                    Span::styled("kage", styles.strong),
                    Span::styled(format!(" {}", env!("CARGO_PKG_VERSION")), styles.base),
                ]),
            )),
            SlotItem::Builtin(name @ ("model" | "cwd" | "permission" | "thinking")) => {
                let (label, text, hint) = labeled(name, src);
                out.push((
                    Keep::Always,
                    labeled_row(label, &text, hint.as_deref(), width, &styles),
                ));
            }
            SlotItem::Builtin("sessions") => push_sessions(src, width, &styles, &mut out),
            SlotItem::Builtin("notices") => push_notices(src, width, &styles, &mut out),
            SlotItem::Builtin(name) => {
                let mut spans = vec![indent()];
                push_builtin(name, src, &styles, &mut spans);
                if spans.len() > 1 {
                    out.push((Keep::Always, Line::from(spans)));
                }
            }
            SlotItem::Text(span) if span.text.is_empty() => {
                out.push((Keep::Always, Line::default()));
            }
            SlotItem::Text(_) => {
                let mut spans = Vec::new();
                push_item(item, src, &styles, &mut spans);
                let style = spans.first().map_or(styles.base, |span| span.style);
                let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
                let body = width.saturating_sub(START_INDENT * 2);
                let body = u16::try_from(body).unwrap_or(u16::MAX);
                for (from, to) in wrap_input_rows(&text, body) {
                    let row = text[from..to].trim_end().to_owned();
                    out.push((
                        Keep::Tip,
                        Line::from(vec![indent(), Span::styled(row, style)]),
                    ));
                }
            }
            SlotItem::Lua(component) => {
                let base = with_hl(styles.base, component.hl());
                for line in chrome_lines_to_ratatui(&component.lines(), base) {
                    let mut spans = vec![indent()];
                    spans.extend(line.spans);
                    out.push((Keep::Tip, Line::from(spans)));
                }
            }
        }
    }
    out
}

/// Label, value and change hint of a labeled start card row.
fn labeled(name: &str, src: &Sources<'_>) -> (&'static str, String, Option<String>) {
    let status = src.status;
    let keys = &status.start_keys;
    let change = |key: &Option<String>| key.as_ref().map(|k| format!("{k} to change"));
    match name {
        "model" => {
            let label = status
                .model
                .or_else(|| src.usage.map(|u| u.model.as_str()))
                .filter(|m| !m.is_empty());
            let text = match (label, status.model_id) {
                (Some(label), Some(id)) if label != id => format!("{label} ({id})"),
                (Some(label), _) => label.to_owned(),
                (None, id) => id.unwrap_or("none").to_owned(),
            };
            ("model", text, change(&keys.model))
        }
        "cwd" => (
            "directory",
            status.cwd.map(home_relative).unwrap_or_default(),
            None,
        ),
        "permission" => {
            let text = match src.usage.and_then(|u| u.permission_mode) {
                Some(mode) => format!("{} mode for this session", mode_label(mode)),
                None => status
                    .start
                    .map(|s| s.permissions.clone())
                    .unwrap_or_default(),
            };
            (
                "permissions",
                text,
                Some("/permission to change".to_owned()),
            )
        }
        _ => {
            let level = src
                .usage
                .and_then(|u| u.thinking_level)
                .map_or("off", kage_core::ThinkingLevel::label);
            ("thinking", level.to_owned(), change(&keys.thinking))
        }
    }
}

/// `path` with the home directory written as `~`.
fn home_relative(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => path.to_owned(),
    }
}

/// One labeled start card row: the label column, the value, and the
/// hint against the right edge while it fits.
fn labeled_row(
    label: &str,
    value: &str,
    hint: Option<&str>,
    width: usize,
    styles: &Styles,
) -> Line<'static> {
    let lead = format!("{:START_INDENT$}{label:<START_LABEL_WIDTH$}", "");
    let room = width.saturating_sub(lead.width() + START_INDENT);
    let value = truncate_to_width(value, room, "...");
    let mut spans = vec![
        Span::styled(lead, styles.base),
        Span::styled(value.clone(), styles.text),
    ];
    if let Some(hint) = hint.filter(|h| value.width() + 2 + h.width() <= room) {
        let pad = room - value.width() - hint.width();
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(hint.to_owned(), styles.hint));
    }
    Line::from(spans)
}

/// The `sessions` rows: title and time per recent session, the resume
/// hint on the first row, then a blank line. Nothing without sessions.
fn push_sessions(
    src: &Sources<'_>,
    width: usize,
    styles: &Styles,
    out: &mut Vec<(Keep, Line<'static>)>,
) {
    let Some(start) = src.status.start.filter(|s| !s.sessions.is_empty()) else {
        return;
    };
    let sessions = &start.sessions[..start.sessions.len().min(START_SESSIONS)];
    let times: Vec<String> = sessions
        .iter()
        .map(|s| {
            [s.group.as_deref(), s.right.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let time_w = times.iter().map(|t| t.width()).max().unwrap_or(0);
    let title_w = sessions.iter().map(|s| s.label.width()).max().unwrap_or(0);
    let room = width.saturating_sub(START_INDENT * 2 + START_LABEL_WIDTH + time_w + 2);
    let hint = src
        .status
        .start_keys
        .sessions
        .as_ref()
        .map(|k| format!("{k} to resume"))
        .filter(|h| room >= title_w.min(24) + 2 + h.width());
    let title_room = room.saturating_sub(hint.as_ref().map_or(0, |h| h.width() + 2));
    let title_w = title_w.min(title_room);
    for (i, (session, time)) in sessions.iter().zip(&times).enumerate() {
        let label = if i == 0 { "recent" } else { "" };
        let lead = format!("{:START_INDENT$}{label:<START_LABEL_WIDTH$}", "");
        let title = pad_to_width(&truncate_to_width(&session.label, title_w, "..."), title_w);
        let mut spans = vec![
            Span::styled(lead, styles.base),
            Span::styled(title, styles.text),
            Span::styled(format!("  {}", pad_to_width(time, time_w)), styles.base),
        ];
        if let Some(hint) = hint.as_ref().filter(|_| i == 0) {
            let pad = title_room.saturating_sub(title_w) + 2;
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(hint.clone(), styles.hint));
        }
        out.push((Keep::Sessions, Line::from(spans)));
    }
    out.push((Keep::Sessions, Line::default()));
}

/// The `notices` rows, wrapped with a hanging indent: warnings and
/// errors behind a `!` in their style, info in the muted style, then a
/// blank line. Nothing without notices.
fn push_notices(
    src: &Sources<'_>,
    width: usize,
    styles: &Styles,
    out: &mut Vec<(Keep, Line<'static>)>,
) {
    use kage_core::protocol::NoticeLevel;
    let Some(start) = src.status.start.filter(|s| !s.notices.is_empty()) else {
        return;
    };
    let theme = crate::theme::current();
    let body = width.saturating_sub(START_INDENT * 2 + 2);
    let body = u16::try_from(body).unwrap_or(u16::MAX);
    for (i, (level, text)) in start.notices.iter().enumerate() {
        let (glyph, style) = match level {
            NoticeLevel::Warning => ("! ", Style::default().fg(theme.warning_fg)),
            NoticeLevel::Error => ("! ", Style::default().fg(theme.tool_error_fg)),
            NoticeLevel::Info => ("  ", styles.base),
        };
        let keep = if i < START_NOTICES_KEPT {
            Keep::Always
        } else {
            Keep::ExtraNotices
        };
        for (row, (from, to)) in wrap_input_rows(text, body).into_iter().enumerate() {
            let lead = if row == 0 { glyph } else { "  " };
            out.push((
                keep,
                Line::from(vec![
                    Span::raw(" ".repeat(START_INDENT)),
                    Span::styled(format!("{lead}{}", text[from..to].trim_end()), style),
                ]),
            ));
        }
    }
    out.push((Keep::Always, Line::default()));
}

fn paint_row(frame: &mut Frame, area: Rect, spec: &SlotSpec, src: &Sources<'_>, styles: &Styles) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = usize::from(area.width);
    let left = row_spans(&spec.left, &spec.sep, src, styles);
    let left_width: usize = left.iter().map(Span::width).sum();
    let right_budget = width.saturating_sub(left_width.min(width * 2 / 3) + 2);
    let right = fit_right(&spec.right, &spec.sep, src, styles, right_budget);
    let right_width: usize = right.iter().map(Span::width).sum();
    let left_budget = match right_width {
        0 => width,
        w => width.saturating_sub(w + 2),
    };
    let mut spans = clip_spans(left, left_budget);
    let used: usize = spans.iter().map(Span::width).sum::<usize>() + right_width;
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

/// The right side of a row in `max` columns. The left side keeps up
/// to two thirds of the row, so items drop from the front of the
/// right side while it does not fit, and the last one is cut.
fn fit_right(
    items: &[SlotItem],
    sep: &str,
    src: &Sources<'_>,
    styles: &Styles,
    max: usize,
) -> Vec<Span<'static>> {
    for start in 0..items.len() {
        let spans = row_spans(&items[start..], sep, src, styles);
        if spans.iter().map(Span::width).sum::<usize>() <= max {
            return spans;
        }
        if start + 1 == items.len() {
            return clip_spans(spans, max);
        }
    }
    Vec::new()
}

/// Drop what does not fit in `max` columns, ending on `...` when
/// anything was cut.
fn clip_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    if spans.iter().map(Span::width).sum::<usize>() <= max {
        return spans;
    }
    let mut out = Vec::new();
    let mut left = max;
    for span in spans {
        let width = span.width();
        if width + 3 <= left {
            left -= width;
            out.push(span);
            continue;
        }
        if left >= 3 {
            let kept = truncate_to_width(&span.content, left - 3, "");
            out.push(Span::styled(format!("{kept}..."), span.style));
        }
        break;
    }
    out
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
        "breadcrumb" => {
            if let Some(crumb) = status.breadcrumb {
                push_breadcrumb(crumb, usize::from(src.width), styles, out);
            }
        }
        "title" if status.breadcrumb.is_none() => {
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

/// Paint the `breadcrumb`: `kage > explore: <task>`, then the agent's
/// state, time, tokens and tool count. The task is cut so the row fits
/// in `width` columns.
fn push_breadcrumb(
    crumb: &Breadcrumb,
    width: usize,
    styles: &Styles,
    out: &mut Vec<Span<'static>>,
) {
    let dot = " \u{b7} ";
    let mut stats = vec![crumb.state.to_owned()];
    stats.extend(crumb.elapsed_ms.map(super::tool_view::format_seconds));
    if crumb.tokens > 0 {
        stats.push(format!("{} tok", format_token_count(crumb.tokens)));
    }
    let tools = usize::try_from(crumb.tool_calls).unwrap_or(usize::MAX);
    stats.push(format!(
        "{tools} {}",
        if tools == 1 { "tool" } else { "tools" }
    ));
    let stats = format!("  {}", stats.join(dot));
    let lead = format!(" kage > {}", crumb.trail.join(" > "));
    let room = width.saturating_sub(lead.width() + 2 + stats.width() + 1);
    out.push(Span::styled(lead, styles.strong));
    if !crumb.description.is_empty() && room > 3 {
        let task = truncate_to_width(&crumb.description, room, "...");
        out.push(Span::styled(format!(": {task}"), styles.text));
    }
    out.push(Span::styled(stats, styles.text));
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
        let crumb = Breadcrumb {
            trail: vec!["explore".to_owned()],
            state: "running",
            ..Breadcrumb::default()
        };
        let focused = StatusCtx {
            breadcrumb: Some(&crumb),
            ..StatusCtx::default()
        };
        let src = Sources::new(&status, Some(&usage), &InputState::new(), 80);
        let agent_src = Sources::new(&focused, Some(&usage), &InputState::new(), 80);
        let styles = Styles::uniform(Style::default());
        let row_components = kage_plugin::slots::BUILTIN_COMPONENTS
            .iter()
            .filter(|name| !matches!(**name, "sessions" | "notices"));
        for name in row_components {
            let mut out = Vec::new();
            let src = if *name == "breadcrumb" {
                &agent_src
            } else {
                &src
            };
            push_builtin(name, src, &styles, &mut out);
            assert!(!out.is_empty(), "{name} painted nothing");
        }
    }

    fn painted(name: &str, usage: &SessionUsage, input: &InputState) -> String {
        let status = StatusCtx::default();
        let src = Sources::new(&status, Some(usage), input, 80);
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
    fn the_breadcrumb_replaces_the_title_and_cuts_its_task_to_fit() {
        let crumb = Breadcrumb {
            trail: vec!["explore".to_owned()],
            description: "map exports under src/components".to_owned(),
            state: "running",
            elapsed_ms: Some(41_000),
            tokens: 22_000,
            tool_calls: 14,
        };
        let status = StatusCtx {
            title: Some("fix the router"),
            breadcrumb: Some(&crumb),
            ..StatusCtx::default()
        };
        let paint = |name: &str, width: u16| {
            let src = Sources::new(&status, None, &InputState::new(), width);
            let mut out = Vec::new();
            push_builtin(name, &src, &Styles::uniform(Style::default()), &mut out);
            out.iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        assert_eq!(paint("title", 100), "");
        assert_eq!(
            paint("breadcrumb", 100),
            " kage > explore: map exports under src/components  \
             running \u{b7} 41s \u{b7} 22k tok \u{b7} 14 tools"
        );
        let narrow = paint("breadcrumb", 70);
        assert_eq!(
            narrow,
            " kage > explore: map exports u...  \
             running \u{b7} 41s \u{b7} 22k tok \u{b7} 14 tools"
        );
        assert!(narrow.width() < 70);
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
