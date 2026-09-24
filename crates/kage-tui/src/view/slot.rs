//! Slot painting: the header, the footer, the input pill and the start
//! screen, composed from their [`SlotSpec`]s.
//!
//! Built-in components render here from frame state and carry their own
//! spacing. Lua components and span items paint the lines the plugin
//! runtime retained, so painting never calls Lua. A spec's `sep` goes
//! between two adjacent items that both produced output, except next to
//! `working`, which is a lead-in indicator.

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
}

impl Styles {
    fn uniform(style: Style) -> Self {
        Self {
            base: style,
            text: style,
            strong: style.add_modifier(Modifier::BOLD),
            pad: Style::default(),
            sep: style,
        }
    }
}

/// Frame state the built-in components read.
pub(super) struct Sources<'a> {
    pub status: &'a StatusCtx<'a>,
    /// Session usage, only when the modeline has anything to show.
    pub usage: Option<&'a SessionUsage>,
    pub mode: Mode,
}

impl<'a> Sources<'a> {
    pub(super) fn new(
        status: &'a StatusCtx<'a>,
        usage: Option<&'a SessionUsage>,
        mode: Mode,
    ) -> Self {
        let usage = usage.filter(|u| {
            !u.model.is_empty() || u.total_tokens() > 0 || u.current_context > 0 || u.working
        });
        Self {
            status,
            usage,
            mode,
        }
    }
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
    };
    let spec = src.status.slots.get(SlotName::Header);
    paint_row(frame, area, &spec, SlotName::Header, src, &styles);
}

/// Paint the footer slot into `area`.
pub(super) fn render_footer(frame: &mut Frame, area: Rect, src: &Sources<'_>) {
    let theme = crate::theme::current();
    let text = Style::default().fg(theme.modeline_fg).bg(theme.modeline_bg);
    let styles = Styles {
        base: text,
        text,
        strong: text.add_modifier(Modifier::BOLD),
        pad: Style::default().bg(theme.modeline_bg),
        sep: Style::default().fg(theme.muted_fg).bg(theme.modeline_bg),
    };
    let spec = src.status.slots.get(SlotName::Footer);
    paint_row(frame, area, &spec, SlotName::Footer, src, &styles);
}

/// The input pill as a left title and an optional right title.
pub(super) fn pill_titles(
    src: &Sources<'_>,
    pill: Style,
) -> (Line<'static>, Option<Line<'static>>) {
    let styles = Styles::uniform(pill);
    let spec = src.status.slots.get(SlotName::InputPill);
    let left = row_spans(&spec.left, &spec.sep, SlotName::InputPill, src, &styles);
    let right = row_spans(&spec.right, &spec.sep, SlotName::InputPill, src, &styles);
    let right = (!right.is_empty()).then(|| Line::from(right).right_aligned());
    (Line::from(left), right)
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
                push_item(other, SlotName::Start, src, &styles, &mut spans);
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

fn paint_row(
    frame: &mut Frame,
    area: Rect,
    spec: &SlotSpec,
    slot: SlotName,
    src: &Sources<'_>,
    styles: &Styles,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut spans = row_spans(&spec.left, &spec.sep, slot, src, styles);
    let right = row_spans(&spec.right, &spec.sep, slot, src, styles);
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
    slot: SlotName,
    src: &Sources<'_>,
    styles: &Styles,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut separable = false;
    let mut piece = Vec::new();
    for item in items {
        piece.clear();
        push_item(item, slot, src, styles, &mut piece);
        if piece.iter().all(|span| span.content.is_empty()) {
            continue;
        }
        let takes_sep = !matches!(item, SlotItem::Builtin("working"));
        if separable && takes_sep && !sep.is_empty() {
            out.push(Span::styled(sep.to_owned(), styles.sep));
        }
        separable = takes_sep;
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

fn push_item(
    item: &SlotItem,
    slot: SlotName,
    src: &Sources<'_>,
    styles: &Styles,
    out: &mut Vec<Span<'static>>,
) {
    match item {
        SlotItem::Builtin(name) => push_builtin(name, slot, src, styles, out),
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
    slot: SlotName,
    src: &Sources<'_>,
    styles: &Styles,
    out: &mut Vec<Span<'static>>,
) {
    let status = src.status;
    match name {
        "brand" => out.push(Span::styled(" kage".to_owned(), styles.text)),
        "model" if slot == SlotName::Header => {
            if let Some(model) = status.model.filter(|m| !m.is_empty()) {
                out.push(Span::styled(" ".to_owned(), styles.pad));
                out.push(Span::styled(model.to_owned(), styles.text));
            }
        }
        "model" => {
            if let Some(u) = src.usage.filter(|u| !u.model.is_empty()) {
                out.push(Span::styled(u.model.clone(), styles.strong));
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
            if let Some((current, total)) = status.search_match_count {
                let label = if total == 0 {
                    "no match".to_owned()
                } else if current == 0 {
                    format!("match -/{total}")
                } else {
                    format!("match {current}/{total}")
                };
                let theme = crate::theme::current();
                let style = styles
                    .pad
                    .fg(theme.match_color)
                    .add_modifier(Modifier::BOLD);
                out.push(Span::styled(format!("{label}  "), style));
            }
        }
        "session" => {
            if let Some(sid) = status.session_id.filter(|s| !s.is_empty()) {
                out.push(Span::styled(format!("#{sid} "), styles.text));
            }
        }
        "working" | "context" | "tokens" | "thinking" | "permission" => {
            if let Some(u) = src.usage {
                push_usage(name, u, styles, out);
            }
        }
        "mode" => out.push(Span::styled(
            format!(" {} ", mode_glyph(src.mode)),
            styles.text,
        )),
        "hint" => {
            if let Some(hint) = status.key_hint.filter(|h| !h.is_empty()) {
                out.push(Span::styled(format!(" {hint} "), styles.text));
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

/// Paint a built-in component that reads the session usage.
fn push_usage(name: &str, u: &SessionUsage, styles: &Styles, out: &mut Vec<Span<'static>>) {
    match name {
        "working" => {
            out.push(Span::styled(" ".to_owned(), styles.pad));
            if u.working {
                out.push(Span::styled(format!("{} ", spinner_frame()), styles.strong));
            } else {
                out.push(Span::styled("  ".to_owned(), styles.pad));
            }
        }
        "context" => {
            if u.context_window > 0 {
                #[allow(clippy::cast_precision_loss)]
                let pct =
                    (u.current_context as f64 / u.context_window as f64 * 100.0).clamp(0.0, 999.9);
                out.push(Span::styled(
                    format!(
                        "ctx {}/{} ({:.0}%)",
                        format_token_count(u.current_context),
                        format_token_count(u.context_window),
                        pct
                    ),
                    styles.text,
                ));
            } else if u.current_context > 0 {
                out.push(Span::styled(
                    format!("ctx {}", format_token_count(u.current_context)),
                    styles.text,
                ));
            }
        }
        "tokens" => {
            out.push(Span::styled(
                format!(
                    "io {}+{}",
                    format_token_count(u.input_tokens),
                    format_token_count(u.output_tokens)
                ),
                styles.text,
            ));
            if u.total_cost > 0.0 {
                out.push(Span::styled(format!(" ${:.4}", u.total_cost), styles.text));
            }
        }
        "thinking" => {
            if let Some(level) = u.thinking_level.filter(|l| !l.is_off()) {
                out.push(Span::styled(
                    format!("think:{}", level.label()),
                    styles.strong,
                ));
            }
        }
        "permission" => {
            if let Some(mode) = u
                .permission_mode
                .filter(|m| *m != kage_core::permissions::PermissionAction::Allow)
            {
                out.push(Span::styled(
                    format!("perm:{}", mode_label(mode)),
                    styles.strong,
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
            context_window: 10,
            thinking_level: Some(kage_core::ThinkingLevel::High),
            permission_mode: Some(kage_core::permissions::PermissionAction::Ask),
            ..SessionUsage::default()
        };
        let widgets = ["w".to_owned()];
        let status = StatusCtx {
            model: Some("m"),
            session_id: Some("s"),
            search_match_count: Some((1, 2)),
            plugin_widgets: &widgets,
            key_hint: Some("g"),
            cwd: Some("/w"),
            ..StatusCtx::default()
        };
        let src = Sources::new(&status, Some(&usage), Mode::Insert);
        let styles = Styles::uniform(Style::default());
        for slot in SlotName::ALL {
            for name in kage_plugin::slots::BUILTIN_COMPONENTS {
                let mut out = Vec::new();
                push_builtin(name, slot, &src, &styles, &mut out);
                assert!(!out.is_empty(), "{name} painted nothing in {slot:?}");
            }
        }
    }
}
