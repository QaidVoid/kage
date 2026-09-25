//! `/settings` dialog: one list generated from the option registry.
//!
//! A modal [`OverlayWidget`] with a row per [`OPTIONS`] entry showing
//! its name, its value and whether `init.lua` set it. The selected
//! option's doc shows below the list. Left, Right and Space cycle
//! booleans, choices and themes and step numbers, digits type a
//! number, and any printable key sets a key option. Each edit is
//! queued for the host to apply live ([`SettingsOverlay::take_edit`]).
//! `Enter` or `Ctrl+S` resolves the edited options as JSON for the
//! host to persist, and `Esc` closes so the host restores
//! [`SettingsOverlay::reverts`].

use kage_core::options::{OPTIONS, OptionDef, OptionKind, OptionSource, OptionStore, OptionValue};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};
use crate::view::scroll_offset_centered;

/// Row marker for an option whose value came from Lua.
const LUA_MARK: &str = "set in init.lua";

/// Rows reserved for the selected option's doc.
const DOC_ROWS: u16 = 3;

/// Help line of a number row.
const NUMBER_HINT: &str = "type or left/right to change \u{b7} enter to save \u{b7} esc to cancel";

/// Help line of a key row.
const KEY_HINT: &str = "press a key to set it \u{b7} enter to save \u{b7} esc to cancel";

/// Help line of every other row.
const CYCLE_HINT: &str = "left/right to change \u{b7} enter to save \u{b7} esc to cancel";

#[derive(Debug)]
struct Row {
    def: &'static OptionDef,
    value: OptionValue,
    seed: OptionValue,
    lua: bool,
}

/// The `/settings` overlay.
#[derive(Debug)]
pub struct SettingsOverlay {
    rows: Vec<Row>,
    themes: Vec<String>,
    selected: usize,
    draft: Option<String>,
    edit: Option<usize>,
    name_width: usize,
    value_width: usize,
}

impl SettingsOverlay {
    /// Build the dialog from the current option values. `themes` are
    /// the names the `theme` row cycles through.
    #[must_use]
    pub fn new(store: &OptionStore, themes: Vec<String>) -> Self {
        let rows: Vec<Row> = OPTIONS
            .iter()
            .map(|def| {
                let value = store
                    .get(def.name)
                    .cloned()
                    .unwrap_or_else(|| def.default_value());
                Row {
                    def,
                    seed: value.clone(),
                    value,
                    lua: store.source(def.name) == Some(OptionSource::Lua),
                }
            })
            .collect();
        let choices = OPTIONS
            .iter()
            .filter_map(|def| match def.kind {
                OptionKind::Choice { values, .. } => Some(values),
                _ => None,
            })
            .flatten();
        let value_width = rows
            .iter()
            .map(|row| display(&row.value).len())
            .chain(themes.iter().map(String::len))
            .chain(choices.map(|value| display(&OptionValue::Str((*value).to_owned())).len()))
            .max()
            .unwrap_or_default();
        Self {
            name_width: OPTIONS.iter().map(|def| def.name.len()).max().unwrap_or(0),
            value_width,
            rows,
            themes,
            selected: 0,
            draft: None,
            edit: None,
        }
    }

    /// The last edit not yet applied, as `(name, value)`.
    pub fn take_edit(&mut self) -> Option<(&'static str, OptionValue)> {
        let row = &self.rows[self.edit.take()?];
        Some((row.def.name, row.value.clone()))
    }

    /// The values the dialog opened with, for every edited option.
    #[must_use]
    pub fn reverts(&self) -> Vec<(&'static str, OptionValue)> {
        self.edited()
            .map(|row| (row.def.name, row.seed.clone()))
            .collect()
    }

    /// The edited options as `{ name: value }`. Unchanged options are
    /// left out, so a value inherited from the project config layer is
    /// never stamped into the user file.
    #[must_use]
    pub fn result_json(&self) -> serde_json::Value {
        self.edited()
            .map(|row| (row.def.name.to_owned(), to_json(&row.value)))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }

    /// Inherent render wrapper, matching `OverlayPicker::render`, so
    /// the App's draw closure can pass a `Frame` directly.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let modal = OverlayWidget::measure(self, area);
        frame.render_widget(crate::opaque::OpaqueClear, modal);
        let theme = crate::theme::current();
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        OverlayWidget::render(self, modal, frame.buffer_mut(), &ctx);
    }

    fn edited(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|row| row.value != row.seed)
    }

    fn row(&self) -> &Row {
        &self.rows[self.selected]
    }

    fn set(&mut self, value: OptionValue) {
        let row = &mut self.rows[self.selected];
        if let Ok(value) = row.def.validate(value)
            && value != row.value
        {
            row.value = value;
            self.edit = Some(self.selected);
        }
    }

    fn select(&mut self, delta: isize) {
        self.commit();
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.rows.len() - 1);
    }

    fn cycle(&mut self, delta: i8) {
        self.commit();
        let row = self.row();
        let next = match (row.def.kind, &row.value) {
            (OptionKind::Bool { .. }, OptionValue::Bool(on)) => OptionValue::Bool(!on),
            (OptionKind::Int { min, max, .. }, OptionValue::Int(n)) => {
                OptionValue::Int((n + i64::from(delta)).clamp(min, max))
            }
            (OptionKind::Fraction { .. }, OptionValue::Float(x)) => {
                OptionValue::Float((((x * 20.0).round() + f64::from(delta)) / 20.0).clamp(0.0, 1.0))
            }
            (OptionKind::Choice { values, .. }, OptionValue::Str(cur)) => step(values, cur, delta),
            (OptionKind::Str { .. }, OptionValue::Str(cur)) if row.def.name == "theme" => {
                step(&self.themes, cur, delta)
            }
            _ => return,
        };
        self.set(next);
    }

    fn type_char(&mut self, c: char) {
        match self.row().def.kind {
            OptionKind::Int { .. } if c.is_ascii_digit() => {
                self.draft.get_or_insert_default().push(c);
            }
            OptionKind::Fraction { .. } if c.is_ascii_digit() || c == '.' => {
                self.draft.get_or_insert_default().push(c);
            }
            OptionKind::Key { .. } => {
                let key = if c == ' ' {
                    "<Space>".to_owned()
                } else {
                    c.to_string()
                };
                self.set(OptionValue::Str(key));
            }
            _ if c == ' ' => self.cycle(1),
            _ => {}
        }
    }

    fn backspace(&mut self) {
        if !is_number(self.row().def) {
            return;
        }
        let value = display(&self.row().value);
        self.draft.get_or_insert(value).pop();
    }

    fn commit(&mut self) {
        if let Some(value) = self.draft.take().and_then(|d| self.parse_draft(&d)) {
            self.set(value);
        }
    }

    fn parse_draft(&self, draft: &str) -> Option<OptionValue> {
        let def = self.row().def;
        let value = match def.kind {
            OptionKind::Int { .. } => OptionValue::Int(draft.parse().ok()?),
            _ => OptionValue::Float(draft.parse().ok()?),
        };
        def.validate(value).ok()
    }

    fn hint(&self) -> &'static str {
        match self.row().def.kind {
            OptionKind::Int { .. } | OptionKind::Fraction { .. } => NUMBER_HINT,
            OptionKind::Key { .. } => KEY_HINT,
            _ => CYCLE_HINT,
        }
    }

    fn doc(&self) -> String {
        let row = self.row();
        let mut doc = row.def.doc.to_owned();
        if !row.def.live {
            doc.push_str(" Applies to new sessions.");
        }
        if row.lua {
            doc.push_str(" init.lua sets it again on the next start.");
        }
        doc
    }

    fn row_line(&self, idx: usize, ctx: &OverlayCtx<'_>) -> Line<'static> {
        let row = &self.rows[idx];
        let selected = idx == self.selected;
        let base = if selected {
            Style::default()
                .fg(ctx.theme.overlay_selected_fg)
                .bg(ctx.theme.overlay_selected_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(ctx.theme.overlay_fg)
        };
        let (value, value_style) = match self.draft.as_deref().filter(|_| selected) {
            Some(draft) if self.parse_draft(draft).is_none() => {
                (format!("{draft}_"), base.fg(ctx.theme.tool_error_fg))
            }
            Some(draft) => (format!("{draft}_"), base),
            None => (display(&row.value), base),
        };
        let mark_style = if selected {
            base
        } else {
            base.fg(ctx.theme.muted_fg)
        };
        let (nw, vw) = (self.name_width, self.value_width);
        Line::from(vec![
            Span::styled(
                format!(
                    "{} {:<nw$}  ",
                    if selected { ">" } else { " " },
                    row.def.name
                ),
                base,
            ),
            Span::styled(format!("{value:<vw$}  "), value_style),
            Span::styled(if row.lua { LUA_MARK } else { "" }, mark_style),
        ])
    }
}

impl OverlayWidget for SettingsOverlay {
    fn measure(&self, available: Rect) -> Rect {
        let row = 2 + self.name_width + 2 + self.value_width + 2 + LUA_MARK.len();
        let hint = [NUMBER_HINT, KEY_HINT, CYCLE_HINT]
            .iter()
            .map(|h| h.len() + 2)
            .max()
            .unwrap_or(0);
        let width = u16::try_from(row.max(hint) + 3).unwrap_or(u16::MAX);
        let rows = u16::try_from(self.rows.len()).unwrap_or(u16::MAX);
        let height = rows + 1 + DOC_ROWS + 1 + 2;
        let width = width.min(available.width);
        let height = height.min(available.height);
        let x = available.x + available.width.saturating_sub(width) / 2;
        let y = available.y + available.height.saturating_sub(height) / 2;
        Rect::new(x, y, width, height)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &OverlayCtx<'_>) {
        let block = Block::default()
            .title(" settings ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ctx.theme.overlay_border));
        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let [list, doc, hint] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(DOC_ROWS + 1),
                Constraint::Length(1),
            ])
            .areas(inner);

        let visible = usize::from(list.height);
        let offset = scroll_offset_centered(self.selected, self.rows.len(), visible);
        for (y, idx) in (list.y..list.bottom()).zip(offset..self.rows.len()) {
            buf.set_line(list.x, y, &self.row_line(idx, ctx), list.width);
        }

        let text = Rect {
            x: doc.x + 2,
            y: doc.y + 1,
            width: doc.width.saturating_sub(3),
            height: DOC_ROWS.min(doc.height.saturating_sub(1)),
        };
        Paragraph::new(self.doc())
            .style(Style::default().fg(ctx.theme.overlay_fg))
            .wrap(Wrap { trim: true })
            .render(text, buf);
        buf.set_line(
            hint.x + 2,
            hint.y,
            &Line::from(Span::styled(
                self.hint(),
                Style::default().fg(ctx.theme.muted_fg),
            )),
            hint.width.saturating_sub(2),
        );
    }

    fn footer_hint(&self) -> &'static str {
        "enter to save \u{b7} esc to cancel"
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        if key.kind != KeyEventKind::Press {
            return OverlayAction::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return OverlayAction::Close,
            KeyCode::Char('c') if ctrl => return OverlayAction::Close,
            KeyCode::Enter => {
                self.commit();
                return OverlayAction::Resolve(self.result_json());
            }
            KeyCode::Char('s') if ctrl => {
                self.commit();
                return OverlayAction::Resolve(self.result_json());
            }
            KeyCode::Up => self.select(-1),
            KeyCode::Down => self.select(1),
            KeyCode::Left => self.cycle(-1),
            KeyCode::Right => self.cycle(1),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Char(c) if !ctrl => self.type_char(c),
            _ => {}
        }
        OverlayAction::Stay
    }
}

fn is_number(def: &OptionDef) -> bool {
    matches!(
        def.kind,
        OptionKind::Int { .. } | OptionKind::Fraction { .. }
    )
}

fn step<S: AsRef<str>>(values: &[S], cur: &str, delta: i8) -> OptionValue {
    let len = values.len();
    let idx = values
        .iter()
        .position(|v| v.as_ref() == cur)
        .map_or(0, |i| {
            if delta < 0 {
                (i + len - 1) % len
            } else {
                (i + 1) % len
            }
        });
    OptionValue::Str(values.get(idx).map_or(cur, AsRef::as_ref).to_owned())
}

fn display(value: &OptionValue) -> String {
    match value {
        OptionValue::Bool(true) => "on".to_owned(),
        OptionValue::Bool(false) => "off".to_owned(),
        OptionValue::Int(n) => n.to_string(),
        OptionValue::Float(x) => x.to_string(),
        OptionValue::Str(s) if s.is_empty() => "auto".to_owned(),
        OptionValue::Str(s) => s.clone(),
    }
}

fn to_json(value: &OptionValue) -> serde_json::Value {
    match value {
        OptionValue::Bool(b) => serde_json::json!(b),
        OptionValue::Int(n) => serde_json::json!(n),
        OptionValue::Float(x) => serde_json::json!(x),
        OptionValue::Str(s) => serde_json::json!(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn themes() -> Vec<String> {
        vec!["default".into(), "tokyo-night".into(), "ayu".into()]
    }

    fn sample() -> SettingsOverlay {
        SettingsOverlay::new(&OptionStore::default(), themes())
    }

    fn select(s: &mut SettingsOverlay, name: &str) {
        let target = OPTIONS.iter().position(|def| def.name == name).unwrap();
        while s.selected != target {
            let dir = if s.selected < target {
                KeyCode::Down
            } else {
                KeyCode::Up
            };
            s.handle_key(key(dir));
        }
    }

    fn value(s: &SettingsOverlay, name: &str) -> OptionValue {
        let row = s.rows.iter().find(|row| row.def.name == name).unwrap();
        row.value.clone()
    }

    fn rendered(s: &mut SettingsOverlay, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let theme = crate::theme::current();
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        let modal = OverlayWidget::measure(s, area);
        OverlayWidget::render(s, modal, &mut buf, &ctx);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn every_option_appears_once() {
        let rows = rendered(&mut sample(), 120, 36);
        for def in OPTIONS {
            let hits = rows
                .iter()
                .filter(|row| row.contains(&format!(" {} ", def.name)))
                .count();
            assert_eq!(hits, 1, "{}", def.name);
        }
    }

    #[test]
    fn unset_thinking_level_reads_auto() {
        let rows = rendered(&mut sample(), 120, 36);
        let row = rows
            .iter()
            .find(|row| row.contains(" thinking_level "))
            .unwrap();
        let mut words = row
            .split_whitespace()
            .skip_while(|w| *w != "thinking_level");
        assert_eq!(words.nth(1), Some("auto"), "{row}");
    }

    #[test]
    fn dialog_fits_80x24() {
        let mut s = sample();
        let modal = OverlayWidget::measure(&s, Rect::new(0, 0, 80, 24));
        assert!(modal.width <= 80 && modal.height <= 24, "{modal:?}");
        let rows = rendered(&mut s, 80, 24).join("\n");
        assert!(rows.contains("timeoutlen"), "{rows}");
        assert!(rows.contains("enter to save"), "{rows}");
        assert!(rows.contains("Color theme"), "{rows}");
    }

    #[test]
    fn lua_marker_shows_for_a_lua_set_option() {
        let mut store = OptionStore::default();
        store
            .set("mouse", OptionValue::Bool(false), OptionSource::Lua)
            .unwrap();
        let mut s = SettingsOverlay::new(&store, themes());
        let rows = rendered(&mut s, 120, 36);
        let marked: Vec<&String> = rows.iter().filter(|r| r.contains(LUA_MARK)).collect();
        assert_eq!(marked.len(), 1, "{rows:#?}");
        assert!(marked[0].contains("mouse"));
        select(&mut s, "mouse");
        assert!(
            rendered(&mut s, 120, 36)
                .join(" ")
                .contains("init.lua sets it again")
        );
    }

    #[test]
    fn cycling_queues_live_edits() {
        let mut s = sample();
        select(&mut s, "editor");
        s.handle_key(key(KeyCode::Right));
        assert_eq!(
            s.take_edit(),
            Some(("editor", OptionValue::Str("vim".into())))
        );
        assert_eq!(s.take_edit(), None);
        s.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(value(&s, "editor"), OptionValue::Str("modeless".into()));
    }

    #[test]
    fn theme_cycles_through_the_given_names() {
        let mut s = sample();
        s.handle_key(key(KeyCode::Left));
        assert_eq!(value(&s, "theme"), OptionValue::Str("ayu".into()));
        s.handle_key(key(KeyCode::Right));
        s.handle_key(key(KeyCode::Right));
        assert_eq!(value(&s, "theme"), OptionValue::Str("tokyo-night".into()));
    }

    #[test]
    fn numbers_step_within_bounds() {
        let mut s = sample();
        select(&mut s, "input_min_lines");
        s.handle_key(key(KeyCode::Left));
        assert_eq!(value(&s, "input_min_lines"), OptionValue::Int(1));
        s.handle_key(key(KeyCode::Right));
        assert_eq!(value(&s, "input_min_lines"), OptionValue::Int(2));
        select(&mut s, "compaction_threshold");
        s.handle_key(key(KeyCode::Right));
        assert_eq!(value(&s, "compaction_threshold"), OptionValue::Float(0.85));
        for _ in 0..10 {
            s.handle_key(key(KeyCode::Right));
        }
        assert_eq!(value(&s, "compaction_threshold"), OptionValue::Float(1.0));
    }

    #[test]
    fn digits_edit_numbers_and_invalid_drafts_are_dropped() {
        let mut s = sample();
        select(&mut s, "timeoutlen");
        for c in "300".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(value(&s, "timeoutlen"), OptionValue::Int(1000));
        s.handle_key(key(KeyCode::Up));
        assert_eq!(value(&s, "timeoutlen"), OptionValue::Int(300));

        select(&mut s, "timeoutlen");
        for c in "99999".chars() {
            s.handle_key(key(KeyCode::Char(c)));
        }
        assert!(rendered(&mut s, 120, 36).join(" ").contains("99999_"));
        s.handle_key(key(KeyCode::Up));
        assert_eq!(value(&s, "timeoutlen"), OptionValue::Int(300));

        select(&mut s, "compaction_threshold");
        s.handle_key(key(KeyCode::Backspace));
        s.handle_key(key(KeyCode::Char('6')));
        s.handle_key(key(KeyCode::Enter));
        assert_eq!(value(&s, "compaction_threshold"), OptionValue::Float(0.6));
    }

    #[test]
    fn a_printable_key_sets_the_leader() {
        let mut s = sample();
        select(&mut s, "leader");
        s.handle_key(key(KeyCode::Char(',')));
        assert_eq!(value(&s, "leader"), OptionValue::Str(",".into()));
        s.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(value(&s, "leader"), OptionValue::Str("<Space>".into()));
    }

    #[test]
    fn enter_resolves_only_edited_options() {
        let mut s = sample();
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            OverlayAction::Resolve(serde_json::json!({}))
        );
        select(&mut s, "mouse");
        s.handle_key(key(KeyCode::Char(' ')));
        select(&mut s, "input_max_lines");
        s.handle_key(key(KeyCode::Char('1')));
        s.handle_key(key(KeyCode::Char('2')));
        let cs = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(
            s.handle_key(cs),
            OverlayAction::Resolve(serde_json::json!({ "mouse": false, "input_max_lines": 12 }))
        );
    }

    #[test]
    fn reverts_restore_the_seed_of_edited_options() {
        let mut s = sample();
        s.handle_key(key(KeyCode::Right));
        s.handle_key(key(KeyCode::Down));
        s.handle_key(key(KeyCode::Right));
        s.handle_key(key(KeyCode::Right));
        assert_eq!(s.reverts(), [("theme", OptionValue::Str("default".into()))]);
    }

    #[test]
    fn esc_and_ctrl_c_close() {
        let mut s = sample();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), OverlayAction::Close);
        let cc = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(s.handle_key(cc), OverlayAction::Close);
    }

    #[test]
    fn selection_stops_at_both_ends() {
        let mut s = sample();
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.selected, 0);
        for _ in 0..OPTIONS.len() + 3 {
            s.handle_key(key(KeyCode::Down));
        }
        assert_eq!(s.selected, OPTIONS.len() - 1);
    }
}
