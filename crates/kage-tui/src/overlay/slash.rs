//! `/` command palette overlay.
//!
//! A [`CommandLine`] rendered inline above the input card so the
//! palette and the `:` ex-line share one parser, completer, and
//! Tab/Down/Up navigation.
//!
//! The palette shows a `/` prefix and the typed input on a single row
//! immediately above the input card, with a tight completion list
//! stacked above it. As the user types, the list filters; Tab applies
//! the longest common prefix and opens cycling, Down/Up navigate, and
//! Enter dispatches the selected command. Once a command name is
//! committed, continued typing edits the arguments inline with per-arg
//! completion driven by the same [`Resolver`] the `:` line uses.
//! Plugin commands appear in the list tagged `[plugin]` via the
//! description suffix the host installs in
//! [`crate::App::set_plugin_commands`].
//!
//! Implements [`OverlayWidget`]. Unlike [`crate::overlay::OverlayPicker`]
//! the slash palette is layout-aware: it paints in three places (popup
//! above the input card, inline error in the same band, slash-prefixed
//! text inside the input card body). The [`OverlayWidget`] trait only
//! exposes one rectangular paint surface, so the existing layout-aware
//! render path stays as an inherent [`SlashPalette::render`] method;
//! the trait render paints just the popup, and the trait `handle_key` is
//! the canonical input entry. App still drives this overlay through
//! the inherent render until PO.5 wires the registry.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::cmdline::{CommandLine, CommandLineEvent};
use crate::cmdparse::{Completion, Resolver};
use crate::command::{ArgSource, CommandSpec};
use crate::layout::Regions;
use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};
use crate::picker::PickItem;

/// Maximum visible command rows in the palette body before
/// `... N more` indicators kick in.
const PALETTE_MAX_VISIBLE: usize = 8;

/// Owned snapshot of the dynamic resolver inputs. The slash palette
/// stores this once at open time so [`OverlayWidget::handle_key`] can
/// rebuild a [`Resolver`] on every keystroke without borrowing from
/// the host.
#[derive(Debug, Default, Clone)]
pub struct SlashContext {
    /// Model ids the user can switch to.
    pub models: Vec<String>,
    /// Plugin command `(name, description)` pairs.
    pub plugin_commands: Vec<(String, String)>,
    /// Session ids the user can resume.
    pub sessions: Vec<PickItem>,
}

struct SnapshotResolver<'a> {
    ctx: &'a SlashContext,
}

impl Resolver for SnapshotResolver<'_> {
    fn dynamic_choice(&self, source: &ArgSource) -> Vec<String> {
        match source {
            ArgSource::Models => self.ctx.models.clone(),
            ArgSource::Themes => crate::theme::Theme::bundled_names()
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            ArgSource::PluginCommands => self
                .ctx
                .plugin_commands
                .iter()
                .map(|(n, _)| n.clone())
                .collect(),
            ArgSource::Sessions => self.ctx.sessions.iter().map(|p| p.value.clone()).collect(),
            ArgSource::Custom(f) => f(),
        }
    }

    fn sessions(&self) -> Vec<String> {
        self.ctx.sessions.iter().map(|p| p.value.clone()).collect()
    }
}

/// `/` command palette overlay.
#[derive(Debug)]
pub struct SlashPalette {
    cmdline: CommandLine,
    registry: Vec<&'static CommandSpec>,
    ctx: SlashContext,
}

impl SlashPalette {
    /// Construct a palette with the registry and resolver snapshot it
    /// will use for its lifetime. The host computes both fresh on each
    /// palette open via [`crate::App::set_plugin_commands`] and the
    /// other setters.
    #[must_use]
    pub fn new(registry: Vec<&'static CommandSpec>, ctx: SlashContext) -> Self {
        Self {
            cmdline: CommandLine::new(),
            registry,
            ctx,
        }
    }

    /// Read-only access to the wrapped command line. Useful for tests
    /// that want to inspect the typed text or selection.
    #[must_use]
    pub fn cmdline(&self) -> &CommandLine {
        &self.cmdline
    }

    /// Set an inline validation error on the wrapped command line.
    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.cmdline.set_error(msg);
    }

    /// Populate the candidate list from the current text. Call this
    /// once after construction so the palette opens already showing
    /// every available command.
    pub fn refresh(&mut self) {
        let resolver = SnapshotResolver { ctx: &self.ctx };
        self.cmdline.refresh_completions(&self.registry, &resolver);
    }

    /// Paint the palette as two pieces: the completion popup floats
    /// above the input card, and the input card's interior is
    /// overpainted with `/ <text>` so the slash command reads as if
    /// the user were typing a message directly into the input box.
    /// Caller is expected to have drawn the rest of the frame first.
    pub fn render(&self, frame: &mut Frame, regions: Regions) {
        self.render_error_above_input(frame, regions);
        if self.cmdline.error().is_none() {
            self.render_popup_above_input(frame, regions);
        }
        self.render_slash_in_input_card(frame, regions);
    }

    /// Place the terminal cursor inside the input card, after the
    /// `/ ` prefix at the column the wrapped [`CommandLine`] thinks
    /// it is editing.
    pub fn place_cursor(&self, frame: &mut Frame, regions: Regions) {
        let Some(inner) = input_inner(regions) else {
            return;
        };
        let prefix: u16 = 1;
        let chars: u16 =
            u16::try_from(self.cmdline.text()[..self.cmdline.cursor()].chars().count())
                .unwrap_or(u16::MAX);
        let cx = inner
            .x
            .saturating_add(prefix)
            .saturating_add(chars)
            .min(inner.x + inner.width.saturating_sub(1));
        frame.set_cursor_position((cx, inner.y));
    }

    fn render_slash_in_input_card(&self, frame: &mut Frame, regions: Regions) {
        let Some(inner) = input_inner(regions) else {
            return;
        };
        let theme = crate::theme::current();
        let prefix_style = Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD);
        let text_style = Style::default().fg(theme.assistant_fg);
        let line = Line::from(vec![
            Span::styled("/", prefix_style),
            Span::styled(self.cmdline.text().to_owned(), text_style),
        ]);
        frame.render_widget(crate::opaque::OpaqueClear, inner);
        frame.render_widget(Paragraph::new(line), inner);
    }

    fn render_popup_above_input(&self, frame: &mut Frame, regions: Regions) {
        let Some(area) = popup_area(
            regions,
            self.cmdline.completions().items.len(),
            self.cmdline.selected(),
        ) else {
            return;
        };
        frame.render_widget(crate::opaque::OpaqueClear, area);
        self.paint_popup(area, frame.buffer_mut());
    }

    fn paint_popup(&self, area: Rect, buf: &mut Buffer) {
        let theme = crate::theme::current();
        let bg = theme.modeline_bg;
        let row_style = Style::default().fg(Color::White).bg(bg);
        let dim_style = Style::default().fg(theme.status_dim_fg).bg(bg);
        let sel_style = Style::default()
            .fg(Color::White)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD);

        let completions = self.cmdline.completions();
        let total = completions.items.len();
        if total == 0 {
            return;
        }
        let inner_width = usize::from(area.width);
        let max_visible = PALETTE_MAX_VISIBLE.min(total);
        let (offset, window) = scroll_window(self.cmdline.selected(), total, max_visible);
        let above = offset;
        let below = total.saturating_sub(offset + window);

        let max_value_chars = completions
            .items
            .iter()
            .skip(offset)
            .take(window)
            .map(|c| c.value.chars().count())
            .max()
            .unwrap_or(0);

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(area.height));
        if above > 0 {
            lines.push(Line::from(Span::styled(
                pad_to_width(&format!("  ... {above} more above"), inner_width),
                dim_style,
            )));
        }
        for (i, item) in completions
            .items
            .iter()
            .enumerate()
            .skip(offset)
            .take(window)
        {
            let selected = self.cmdline.selected() == Some(i);
            let value_style = if selected { sel_style } else { row_style };
            let desc_style = if selected { sel_style } else { dim_style };
            lines.push(render_row(
                item,
                max_value_chars,
                inner_width,
                value_style,
                desc_style,
            ));
        }
        if below > 0 {
            lines.push(Line::from(Span::styled(
                pad_to_width(&format!("  ... {below} more below"), inner_width),
                dim_style,
            )));
        }

        Widget::render(Paragraph::new(lines).style(row_style), area, buf);
    }

    fn render_error_above_input(&self, frame: &mut Frame, regions: Regions) {
        let Some(err) = self.cmdline.error() else {
            return;
        };
        let Some(area) = error_area(regions) else {
            return;
        };
        let theme = crate::theme::current();
        let bg = theme.modeline_bg;
        let fg = theme.tool_error_fg;
        let style = Style::default().fg(fg).bg(bg);

        let marker = "! ";
        let marker_chars = marker.len();
        let inner = usize::from(area.width).saturating_sub(marker_chars);
        let text = truncate(err, inner);
        let total_chars = marker_chars + text.chars().count();
        let pad = usize::from(area.width).saturating_sub(total_chars);
        let line = Line::from(vec![
            Span::styled(marker.to_owned(), style.add_modifier(Modifier::BOLD)),
            Span::styled(format!("{text}{}", " ".repeat(pad)), style),
        ]);
        frame.render_widget(crate::opaque::OpaqueClear, area);
        frame.render_widget(Paragraph::new(line), area);
    }
}

impl OverlayWidget for SlashPalette {
    fn measure(&self, available: Rect) -> Rect {
        let total = self.cmdline.completions().items.len();
        if total == 0 {
            return Rect::new(available.x, available.bottom().saturating_sub(1), 0, 0);
        }
        let max_visible = PALETTE_MAX_VISIBLE.min(total);
        let (offset, window) = scroll_window(self.cmdline.selected(), total, max_visible);
        let above = offset;
        let below = total.saturating_sub(offset + window);
        let rows = window + usize::from(above > 0) + usize::from(below > 0);
        let height = u16::try_from(rows)
            .unwrap_or(u16::MAX)
            .min(available.height);
        let y = available.bottom().saturating_sub(height);
        Rect::new(available.x, y, available.width, height)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, _ctx: &OverlayCtx<'_>) {
        Widget::render(crate::opaque::OpaqueClear, area, buf);
        self.paint_popup(area, buf);
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        let resolver = SnapshotResolver { ctx: &self.ctx };
        match self.cmdline.handle_key(key, &self.registry, &resolver) {
            CommandLineEvent::Pending => OverlayAction::Stay,
            CommandLineEvent::Cancelled => OverlayAction::Close,
            CommandLineEvent::Submit(text) => {
                OverlayAction::Resolve(serde_json::Value::String(text))
            }
        }
    }
}

fn popup_area(regions: Regions, total: usize, selected: Option<usize>) -> Option<Rect> {
    if total == 0 {
        return None;
    }
    let width = regions.input.width;
    if width == 0 {
        return None;
    }
    let max_visible = PALETTE_MAX_VISIBLE.min(total);
    let (offset, window) = scroll_window(selected, total, max_visible);
    let above = offset;
    let below = total.saturating_sub(offset + window);
    let rows = window + usize::from(above > 0) + usize::from(below > 0);

    let space_above = regions.input.y.saturating_sub(regions.buffer.y);
    let rows = rows.min(usize::from(space_above));
    if rows == 0 {
        return None;
    }
    let height = u16::try_from(rows).unwrap_or(u16::MAX);
    let y = regions.input.y.saturating_sub(height);
    Some(Rect {
        x: regions.input.x,
        y,
        width,
        height,
    })
}

fn error_area(regions: Regions) -> Option<Rect> {
    let width = regions.input.width;
    if width == 0 {
        return None;
    }
    let space_above = regions.input.y.saturating_sub(regions.buffer.y);
    if space_above == 0 {
        return None;
    }
    let y = regions.input.y.saturating_sub(1);
    Some(Rect {
        x: regions.input.x,
        y,
        width,
        height: 1,
    })
}

fn input_inner(regions: Regions) -> Option<Rect> {
    if regions.input.width < 3 || regions.input.height < 3 {
        return None;
    }
    Some(Rect {
        x: regions.input.x.saturating_add(1),
        y: regions.input.y.saturating_add(1),
        width: regions.input.width.saturating_sub(2),
        height: 1,
    })
}

fn render_row(
    item: &Completion,
    value_col_chars: usize,
    inner_width: usize,
    value_style: Style,
    desc_style: Style,
) -> Line<'static> {
    let leading = "  ";
    let value_chars = item.value.chars().count();
    let pad = value_col_chars.saturating_sub(value_chars);
    let after_value = leading.chars().count() + value_chars + pad;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    spans.push(Span::styled(leading.to_owned(), value_style));
    spans.push(Span::styled(item.value.clone(), value_style));
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), value_style));
    }
    if let Some(desc) = item.description.as_deref() {
        let remaining = inner_width.saturating_sub(after_value).saturating_sub(2);
        if remaining > 0 {
            spans.push(Span::styled("  ".to_owned(), desc_style));
            spans.push(Span::styled(truncate(desc, remaining), desc_style));
        }
    }
    let painted: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if painted < inner_width {
        spans.push(Span::styled(" ".repeat(inner_width - painted), value_style));
    }
    Line::from(spans)
}

fn scroll_window(selected: Option<usize>, total: usize, max_visible: usize) -> (usize, usize) {
    if total <= max_visible {
        return (0, total);
    }
    let sel = selected.unwrap_or(0);
    let offset = if sel < max_visible {
        0
    } else {
        (sel + 1)
            .saturating_sub(max_visible)
            .min(total - max_visible)
    };
    (offset, max_visible)
}

fn truncate(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = chars[..max_chars.saturating_sub(1)].iter().collect();
    out.push('\u{2026}');
    out
}

fn pad_to_width(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_owned();
    }
    let mut out = s.to_owned();
    out.push_str(&" ".repeat(width - n));
    out
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::command::{ArgSource, ArgSpec, CommandCategory};

    const MODEL: CommandSpec = CommandSpec {
        name: "model",
        aliases: &[],
        description: "switch model",
        category: CommandCategory::Both,
        args: &[ArgSpec::DynamicChoice {
            name: "id",
            source: ArgSource::Models,
            optional: false,
        }],
        subcommands: &[],
    };

    const MOUSE: CommandSpec = CommandSpec {
        name: "mouse",
        aliases: &[],
        description: "toggle mouse",
        category: CommandCategory::Both,
        args: &[ArgSpec::Choice {
            name: "state",
            values: &["on", "off", "toggle"],
            optional: true,
        }],
        subcommands: &[],
    };

    const QUIT: CommandSpec = CommandSpec {
        name: "quit",
        aliases: &["q"],
        description: "leave the TUI",
        category: CommandCategory::Both,
        args: &[],
        subcommands: &[],
    };

    fn registry() -> Vec<&'static CommandSpec> {
        vec![&QUIT, &MODEL, &MOUSE]
    }

    fn palette() -> SlashPalette {
        let mut p = SlashPalette::new(registry(), SlashContext::default());
        p.refresh();
        p
    }

    fn snapshot(palette: &SlashPalette, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let regions = crate::layout::split(frame.area(), 3, 0);
                palette.render(frame, regions);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = Vec::new();
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            out.push(row.trim_end().to_owned());
        }
        out
    }

    #[test]
    fn open_palette_lists_every_command() {
        let p = palette();
        let lines = snapshot(&p, 60, 16);
        assert!(lines.iter().any(|l| l.contains("quit")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("model")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("mouse")), "{lines:#?}");
    }

    #[test]
    fn slash_text_overpaints_input_card_body() {
        let p = palette();
        let lines = snapshot(&p, 60, 16);
        assert!(
            lines[14].trim_start().starts_with('/'),
            "expected `/` near start of input body row 14, got {:?}",
            lines[14]
        );
    }

    #[test]
    fn open_palette_shows_per_arg_hints_in_description() {
        let p = palette();
        let lines = snapshot(&p, 80, 16);
        let model_row = lines
            .iter()
            .find(|l| l.contains("model"))
            .expect("model row");
        assert!(model_row.contains("<id>"), "got {model_row:?}");
        let mouse_row = lines
            .iter()
            .find(|l| l.contains("mouse"))
            .expect("mouse row");
        assert!(mouse_row.contains("[on|off|toggle]"), "got {mouse_row:?}");
    }

    #[test]
    fn typing_filters_the_list() {
        let mut p = palette();
        let _ = OverlayWidget::handle_key(
            &mut p,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
        );
        let lines = snapshot(&p, 60, 16);
        assert!(lines.iter().any(|l| l.contains("model")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("mouse")), "{lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("quit")), "{lines:#?}");
    }

    #[test]
    fn enter_resolves_with_submitted_text() {
        let mut p = palette();
        let _ = OverlayWidget::handle_key(
            &mut p,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        let action =
            OverlayWidget::handle_key(&mut p, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match action {
            OverlayAction::Resolve(serde_json::Value::String(s)) => {
                assert!(s.contains('q') || s.contains("quit"));
            }
            other => panic!("expected Resolve, got {other:?}"),
        }
    }

    #[test]
    fn esc_closes_when_popup_dismissed() {
        let mut p = palette();
        let _ = OverlayWidget::handle_key(
            &mut p,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        // First Esc dismisses any popup; the cmdline stays open.
        let _ = OverlayWidget::handle_key(&mut p, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        // Second Esc cancels the cmdline.
        let action =
            OverlayWidget::handle_key(&mut p, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, OverlayAction::Close);
    }

    #[test]
    fn popup_anchored_above_input_card() {
        let p = palette();
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let regions = crate::layout::split(frame.area(), 3, 0);
                p.render(frame, regions);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row_below_input_start = (0..buf.area.width)
            .map(|x| buf[(x, 17)].symbol())
            .collect::<String>();
        assert!(
            !row_below_input_start.contains("model"),
            "popup should not paint into the input card border"
        );
        let mut found_in_popup_band = false;
        for y in 13..17 {
            let row = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>();
            if row.contains("model") {
                found_in_popup_band = true;
                break;
            }
        }
        assert!(found_in_popup_band, "expected popup to paint above input");
    }
}
