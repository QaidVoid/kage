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

    /// Paint the palette inline above the input card. The input row
    /// shows `/ <text>` and the completion list stacks above it.
    /// Caller is expected to have drawn the rest of the frame first;
    /// [`Clear`] blanks the palette region before painting.
    pub fn render(&self, frame: &mut Frame, regions: Regions) {
        let Some(area) = palette_area(
            regions,
            self.cmdline.completions().items.len(),
            self.cmdline.selected(),
        ) else {
            return;
        };
        let theme = crate::theme::current();
        let bg = theme.modeline_bg;
        let prefix_style = Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD)
            .bg(bg);
        let row_style = Style::default().fg(Color::White).bg(bg);
        let dim_style = Style::default().fg(theme.status_dim_fg).bg(bg);
        let sel_style = Style::default()
            .fg(Color::White)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD);

        frame.render_widget(Clear, area);

        let list_rows = usize::from(area.height).saturating_sub(1);
        let inner_width = usize::from(area.width);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(area.height));

        let completions = self.cmdline.completions();
        let total = completions.items.len();
        if total > 0 && list_rows > 0 {
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
        }

        // Input row (always present, bottom).
        let input_text = self.cmdline.text();
        let mut input_spans = vec![
            Span::styled("/ ", prefix_style),
            Span::styled(input_text.to_owned(), row_style),
        ];
        let painted = "/ ".chars().count() + input_text.chars().count();
        if painted < inner_width {
            input_spans.push(Span::styled(" ".repeat(inner_width - painted), row_style));
        }
        lines.push(Line::from(input_spans));

        frame.render_widget(Paragraph::new(lines).style(row_style), area);
    }

    /// Place the terminal cursor in the palette's input row so the
    /// user can see where typing will land. Mirrors the layout used
    /// by [`Self::render`].
    pub fn place_cursor(&self, frame: &mut Frame, regions: Regions) {
        let Some(area) = palette_area(
            regions,
            self.cmdline.completions().items.len(),
            self.cmdline.selected(),
        ) else {
            return;
        };
        let row_y = area.y + area.height.saturating_sub(1);
        let prefix: u16 = 2;
        let chars: u16 =
            u16::try_from(self.cmdline.text()[..self.cmdline.cursor()].chars().count())
                .unwrap_or(u16::MAX);
        let cx = area
            .x
            .saturating_add(prefix)
            .saturating_add(chars)
            .min(area.x + area.width.saturating_sub(1));
        frame.set_cursor_position((cx, row_y));
    }
}

/// Compute the palette's painting rectangle anchored just above the
/// input card. Returns `None` when there is no vertical room or the
/// input region is degenerate. Height accommodates the input row plus
/// up to [`PALETTE_MAX_VISIBLE`] completion rows and any `... N more`
/// indicators, clamped to the buffer area above the input.
fn palette_area(regions: Regions, total: usize, selected: Option<usize>) -> Option<Rect> {
    let width = regions.input.width;
    if width == 0 {
        return None;
    }
    let max_visible = PALETTE_MAX_VISIBLE.min(total);
    let (offset, window) = scroll_window(selected, total, max_visible);
    let above = offset;
    let below = total.saturating_sub(offset + window);
    let list_rows = window + usize::from(above > 0) + usize::from(below > 0);
    let total_rows = list_rows + 1; // +1 for the input row

    let space_above = regions.input.y.saturating_sub(regions.buffer.y);
    let total_rows = total_rows.min(usize::from(space_above)).max(1);
    let height = u16::try_from(total_rows).unwrap_or(u16::MAX);
    if height == 0 {
        return None;
    }
    let y = regions.input.y.saturating_sub(height);
    Some(Rect {
        x: regions.input.x,
        y,
        width,
        height,
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
    };

    const QUIT: CommandSpec = CommandSpec {
        name: "quit",
        aliases: &["q"],
        description: "leave the TUI",
        category: CommandCategory::Both,
        args: &[],
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
    fn open_palette_shows_input_row_with_slash_prefix() {
        let mut palette = SlashPalette::new();
        let reg = registry();
        palette.refresh(&reg, &EmptyResolver);
        let lines = snapshot(&palette, 60, 16);
        // The snapshot helper trims trailing spaces, so an empty text
        // input row appears as just "/".
        assert!(
            lines.iter().any(|l| l == "/" || l.starts_with("/ ")),
            "expected a `/` input row, got {lines:#?}"
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
    fn palette_anchored_above_input_card() {
        let mut palette = SlashPalette::new();
        let reg = registry();
        palette.refresh(&reg, &EmptyResolver);
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let regions_input_y = std::cell::Cell::new(0u16);
        terminal
            .draw(|frame| {
                let regions = crate::layout::split(frame.area(), 3, 0);
                regions_input_y.set(regions.input.y);
                palette.render(frame, regions);
            })
            .unwrap();
        let _ = regions_input_y.get();
        // The `/` input row should sit immediately above regions.input,
        // which split() positions at y = 20 - 3 = 17.
        let buf = terminal.backend().buffer();
        let input_row = (0..buf.area.width)
            .map(|x| buf[(x, 16)].symbol())
            .collect::<String>();
        assert!(
            input_row.trim_start().starts_with("/ "),
            "row 16 should be the palette input, got {input_row:?}"
        );
        let unused = Rect::new(0, 0, 0, 0);
        let _ = unused;
    }
}
