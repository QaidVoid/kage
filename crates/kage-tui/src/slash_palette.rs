//! `/` command palette - a [`CommandLine`] rendered inline above the
//! input card so the palette and the `:` ex-line share one parser,
//! completer, and Tab/Down/Up navigation.
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

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::cmdline::{CommandLine, CommandLineEvent};
use crate::cmdparse::{Completion, Resolver};
use crate::command::CommandSpec;
use crate::layout::Regions;

/// Maximum visible command rows in the palette body before
/// `... N more` indicators kick in.
const PALETTE_MAX_VISIBLE: usize = 8;

/// `/` command palette overlay.
#[derive(Debug)]
pub struct SlashPalette {
    cmdline: CommandLine,
}

impl Default for SlashPalette {
    fn default() -> Self {
        Self::new()
    }
}

impl SlashPalette {
    /// Construct an empty palette. Call [`Self::refresh`] right after
    /// construction so the list reflects the full command set on open.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cmdline: CommandLine::new(),
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

    /// Drive the palette by one keystroke. Delegates to the wrapped
    /// [`CommandLine`].
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        registry: &[&CommandSpec],
        resolver: &dyn Resolver,
    ) -> CommandLineEvent {
        self.cmdline.handle_key(key, registry, resolver)
    }

    /// Populate the candidate list from the current text. Call this
    /// once after [`Self::new`] so the palette opens already showing
    /// every available command.
    pub fn refresh(&mut self, registry: &[&CommandSpec], resolver: &dyn Resolver) {
        self.cmdline.refresh_completions(registry, resolver);
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
        frame.render_widget(Clear, inner);
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
        let theme = crate::theme::current();
        let bg = theme.modeline_bg;
        let row_style = Style::default().fg(Color::White).bg(bg);
        let dim_style = Style::default().fg(theme.status_dim_fg).bg(bg);
        let sel_style = Style::default()
            .fg(Color::White)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD);

        frame.render_widget(Clear, area);

        let completions = self.cmdline.completions();
        let total = completions.items.len();
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

        frame.render_widget(Paragraph::new(lines).style(row_style), area);
    }

    /// Render an inline validation error just above the input card,
    /// in the same position the completion popup would occupy.
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
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(line), area);
    }
}

/// Compute the popup's painting rectangle anchored just above the
/// input card. Returns `None` when there are no completions to show
/// or no vertical room.
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

/// Compute the single-row error rectangle anchored just above the
/// input card.
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

/// Inner row of the input card where the slash text overpaints. Skips
/// the surrounding 1-cell border. Returns `None` when the input region
/// is too small to host a single line of content.
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
    use ratatui::layout::Rect;

    use super::*;
    use crate::cmdparse::EmptyResolver;
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
        let mut palette = SlashPalette::new();
        let reg = registry();
        palette.refresh(&reg, &EmptyResolver);
        let lines = snapshot(&palette, 60, 16);
        assert!(lines.iter().any(|l| l.contains("quit")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("model")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("mouse")), "{lines:#?}");
    }

    #[test]
    fn slash_text_overpaints_input_card_body() {
        let mut palette = SlashPalette::new();
        let reg = registry();
        palette.refresh(&reg, &EmptyResolver);
        let lines = snapshot(&palette, 60, 16);
        // Input card occupies the bottom 3 rows; the body row is at
        // y = height - 2 (last row is the bottom border). With 16
        // rows total and input_height=3, body row is y=14.
        // The overpaint lands at input_inner.x = regions.input.x + 1,
        // so the leading column is whitespace (the border position).
        assert!(
            lines[14].trim_start().starts_with('/'),
            "expected `/` near start of input body row 14, got {:?}",
            lines[14]
        );
    }

    #[test]
    fn open_palette_shows_per_arg_hints_in_description() {
        let mut palette = SlashPalette::new();
        let reg = registry();
        palette.refresh(&reg, &EmptyResolver);
        let lines = snapshot(&palette, 80, 16);
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
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut palette = SlashPalette::new();
        let reg = registry();
        palette.refresh(&reg, &EmptyResolver);
        palette.handle_key(
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
            &reg,
            &EmptyResolver,
        );
        let lines = snapshot(&palette, 60, 16);
        assert!(lines.iter().any(|l| l.contains("model")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("mouse")), "{lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("quit")), "{lines:#?}");
    }

    #[test]
    fn popup_anchored_above_input_card() {
        let mut palette = SlashPalette::new();
        let reg = registry();
        palette.refresh(&reg, &EmptyResolver);
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let regions = crate::layout::split(frame.area(), 3, 0);
                palette.render(frame, regions);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        // Input card is at y=17..20 (height=3). The popup must paint
        // strictly above y=17.
        let row_below_input_start = (0..buf.area.width)
            .map(|x| buf[(x, 17)].symbol())
            .collect::<String>();
        assert!(
            !row_below_input_start.contains("model"),
            "popup should not paint into the input card border, got {row_below_input_start:?}"
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
        let unused = Rect::new(0, 0, 0, 0);
        let _ = unused;
    }
}
