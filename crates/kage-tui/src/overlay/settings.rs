//! `:settings` multi-tab overlay.
//!
//! A modal [`OverlayWidget`] with five sections: Theme, Model, Mouse,
//! Autocompaction, and a read-only Keybindings list. `Tab`/`BackTab`
//! cycle sections; within a section the arrow keys change the value;
//! `Enter` or `Ctrl+S` resolves with the edited settings as JSON and
//! `Esc` cancels. The host applies the result live (theme, mouse,
//! model) and persists it comment-preserving via `Config::save`.
//!
//! Scope is intentionally config-backed only: thinking level is
//! session state (already `Shift+Tab`) and is not shown here.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};

/// Smallest / largest / step for the autocompaction threshold. The
/// loop clamps to `(0.0, 1.0]`; the dialog keeps it in a sane band.
const THRESHOLD_MIN: f32 = 0.10;
const THRESHOLD_MAX: f32 = 1.0;
const THRESHOLD_STEP: f32 = 0.05;

/// Section tabs in display order.
const TABS: &[&str] = &[
    "Theme",
    "Model",
    "Mouse",
    "Autocompaction",
    "Keybindings",
    "Editor",
];

/// Inputs the host gathers (from the loaded config + live state) to
/// seed the dialog.
#[derive(Clone, Debug, Default)]
pub struct SettingsInit {
    /// All selectable theme names, host order.
    pub themes: Vec<String>,
    /// Currently active theme name.
    pub theme: String,
    /// All selectable `provider:model` ids.
    pub models: Vec<String>,
    /// Currently active model id.
    pub model: String,
    /// Whether mouse capture is on.
    pub mouse: bool,
    /// Autocompaction threshold (fraction of the context window).
    pub threshold: f32,
    /// Read-only `(chord, action)` pairs to display.
    pub keybindings: Vec<(String, String)>,
    /// Whether the prompt input is non-modal (`editor = "modeless"`).
    pub editor_modeless: bool,
}

/// The `:settings` overlay.
#[derive(Debug)]
pub struct SettingsOverlay {
    tab: usize,
    themes: Vec<String>,
    theme_idx: usize,
    models: Vec<String>,
    model_idx: usize,
    mouse: bool,
    threshold: f32,
    keybindings: Vec<(String, String)>,
    editor_modeless: bool,
    list_scroll: usize,
}

impl SettingsOverlay {
    /// Build the overlay from host-gathered state. Selection starts on
    /// the active theme / model when present.
    #[must_use]
    pub fn new(init: SettingsInit) -> Self {
        let theme_idx = init
            .themes
            .iter()
            .position(|t| *t == init.theme)
            .unwrap_or(0);
        let model_idx = init
            .models
            .iter()
            .position(|m| *m == init.model)
            .unwrap_or(0);
        Self {
            tab: 0,
            themes: init.themes,
            theme_idx,
            models: init.models,
            model_idx,
            mouse: init.mouse,
            threshold: init.threshold.clamp(THRESHOLD_MIN, THRESHOLD_MAX),
            keybindings: init.keybindings,
            editor_modeless: init.editor_modeless,
            list_scroll: 0,
        }
    }

    /// Selected theme name, or empty when no themes were supplied.
    #[must_use]
    pub fn selected_theme(&self) -> &str {
        self.themes.get(self.theme_idx).map_or("", String::as_str)
    }

    /// Selected model id, or empty when no models were supplied.
    #[must_use]
    pub fn selected_model(&self) -> &str {
        self.models.get(self.model_idx).map_or("", String::as_str)
    }

    /// Whether mouse capture is enabled in the current edit.
    #[must_use]
    pub fn mouse(&self) -> bool {
        self.mouse
    }

    /// The edited autocompaction threshold.
    #[must_use]
    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// The edited settings as the JSON the host applies on resolve.
    #[must_use]
    pub fn result_json(&self) -> serde_json::Value {
        // The threshold moves on a 0.05 grid; round to 2 decimals so
        // neither the JSON nor the persisted config carries an ugly
        // f32->f64 tail (0.800000011...).
        let threshold = (f64::from(self.threshold) * 100.0).round() / 100.0;
        serde_json::json!({
            "theme": self.selected_theme(),
            "model": self.selected_model(),
            "mouse": self.mouse,
            "compaction_threshold": threshold,
            "editor": if self.editor_modeless { "modeless" } else { "vim" },
        })
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

    fn move_selection(&mut self, delta: isize) {
        match self.tab {
            0 => self.theme_idx = step_index(self.theme_idx, self.themes.len(), delta),
            1 => self.model_idx = step_index(self.model_idx, self.models.len(), delta),
            2 => self.mouse = !self.mouse,
            3 => {
                let next = if delta < 0 {
                    self.threshold - THRESHOLD_STEP
                } else {
                    self.threshold + THRESHOLD_STEP
                };
                // Round to the step grid so repeated presses stay tidy.
                let snapped = (next / THRESHOLD_STEP).round() * THRESHOLD_STEP;
                self.threshold = snapped.clamp(THRESHOLD_MIN, THRESHOLD_MAX);
            }
            4 => {
                let n = self.keybindings.len();
                if n > 0 {
                    self.list_scroll = step_index(self.list_scroll, n, delta);
                }
            }
            5 => self.editor_modeless = !self.editor_modeless,
            _ => {}
        }
    }
}

fn step_index(cur: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    if delta < 0 {
        if cur == 0 { len - 1 } else { cur - 1 }
    } else {
        (cur + 1) % len
    }
}

fn center(available: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = (available.width * pct_w / 100).max(1);
    let h = (available.height * pct_h / 100).max(1);
    let x = available.x + (available.width.saturating_sub(w)) / 2;
    let y = available.y + (available.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

impl OverlayWidget for SettingsOverlay {
    fn measure(&self, available: Rect) -> Rect {
        center(available, 70, 70)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer, ctx: &OverlayCtx<'_>) {
        let accent = ctx.theme.focus_color;
        let block = Block::default()
            .title(" settings ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent));
        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        // Tab bar.
        let mut tab_spans: Vec<Span<'static>> = Vec::new();
        for (i, name) in TABS.iter().enumerate() {
            if i > 0 {
                tab_spans.push(Span::styled("  ", Style::default()));
            }
            let style = if i == self.tab {
                Style::default()
                    .fg(Color::Black)
                    .bg(accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ctx.theme.status_dim_fg)
            };
            tab_spans.push(Span::styled(format!(" {name} "), style));
        }
        buf.set_line(rows[0].x, rows[0].y, &Line::from(tab_spans), rows[0].width);

        buf.set_line(
            rows[1].x,
            rows[1].y,
            &Line::from(Span::styled(
                "-".repeat(usize::from(rows[1].width)),
                Style::default().fg(ctx.theme.status_dim_fg),
            )),
            rows[1].width,
        );

        self.render_body(buf, rows[2], ctx);

        let help = match self.tab {
            2 | 5 => "Tab section - Space toggle - Enter/Ctrl+S save - Esc cancel",
            3 => "Tab section - Left/Right adjust - Enter/Ctrl+S save - Esc cancel",
            _ => "Tab section - Up/Down move - Enter/Ctrl+S save - Esc cancel",
        };
        buf.set_line(
            rows[3].x,
            rows[3].y,
            &Line::from(Span::styled(
                help,
                Style::default().fg(ctx.theme.status_dim_fg),
            )),
            rows[3].width,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        if key.kind != KeyEventKind::Press {
            return OverlayAction::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            return OverlayAction::Close;
        }
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            return OverlayAction::Resolve(self.result_json());
        }
        match key.code {
            KeyCode::Esc => OverlayAction::Close,
            KeyCode::Enter => OverlayAction::Resolve(self.result_json()),
            KeyCode::Tab => {
                self.tab = (self.tab + 1) % TABS.len();
                self.list_scroll = 0;
                OverlayAction::Stay
            }
            KeyCode::BackTab => {
                self.tab = if self.tab == 0 {
                    TABS.len() - 1
                } else {
                    self.tab - 1
                };
                self.list_scroll = 0;
                OverlayAction::Stay
            }
            KeyCode::Up | KeyCode::Left | KeyCode::Char('k' | 'h') => {
                self.move_selection(-1);
                OverlayAction::Stay
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Char('j' | 'l') => {
                self.move_selection(1);
                OverlayAction::Stay
            }
            KeyCode::Char(' ') if self.tab == 2 => {
                self.mouse = !self.mouse;
                OverlayAction::Stay
            }
            KeyCode::Char(' ') if self.tab == 5 => {
                self.editor_modeless = !self.editor_modeless;
                OverlayAction::Stay
            }
            _ => OverlayAction::Stay,
        }
    }
}

impl SettingsOverlay {
    fn render_body(&self, buf: &mut Buffer, area: Rect, ctx: &OverlayCtx<'_>) {
        match self.tab {
            0 => Self::render_list(buf, area, ctx, &self.themes, self.theme_idx),
            1 => Self::render_list(buf, area, ctx, &self.models, self.model_idx),
            2 => {
                let mark = if self.mouse { "[x]" } else { "[ ]" };
                buf.set_line(
                    area.x,
                    area.y,
                    &Line::from(Span::styled(
                        format!("  {mark} capture mouse"),
                        Style::default().fg(ctx.theme.assistant_fg),
                    )),
                    area.width,
                );
            }
            3 => {
                buf.set_line(
                    area.x,
                    area.y,
                    &Line::from(Span::styled(
                        format!("  compaction threshold: {:.0}%", self.threshold * 100.0),
                        Style::default()
                            .fg(ctx.theme.assistant_fg)
                            .add_modifier(Modifier::BOLD),
                    )),
                    area.width,
                );
                buf.set_line(
                    area.x,
                    area.y + 1,
                    &Line::from(Span::styled(
                        "  compact older history once the prompt fills this much of the window",
                        Style::default().fg(ctx.theme.status_dim_fg),
                    )),
                    area.width,
                );
            }
            4 => self.render_keybindings(buf, area, ctx),
            5 => {
                let mark = if self.editor_modeless { "[x]" } else { "[ ]" };
                buf.set_line(
                    area.x,
                    area.y,
                    &Line::from(Span::styled(
                        format!("  {mark} modeless editor (off = vim modal)"),
                        Style::default().fg(ctx.theme.assistant_fg),
                    )),
                    area.width,
                );
                buf.set_line(
                    area.x,
                    area.y + 1,
                    &Line::from(Span::styled(
                        "  non-modal: Emacs keys, Esc cancels the turn. Applies immediately.",
                        Style::default().fg(ctx.theme.status_dim_fg),
                    )),
                    area.width,
                );
            }
            _ => {}
        }
    }

    fn render_list(
        buf: &mut Buffer,
        area: Rect,
        ctx: &OverlayCtx<'_>,
        items: &[String],
        selected: usize,
    ) {
        if items.is_empty() {
            buf.set_line(
                area.x,
                area.y,
                &Line::from(Span::styled(
                    "  (none available)",
                    Style::default().fg(ctx.theme.status_dim_fg),
                )),
                area.width,
            );
            return;
        }
        let rows = usize::from(area.height);
        let offset = scroll_offset(selected, items.len(), rows);
        for (row, (idx, item)) in items.iter().enumerate().skip(offset).take(rows).enumerate() {
            let style = if idx == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(ctx.theme.focus_color)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ctx.theme.assistant_fg)
            };
            let marker = if idx == selected { "> " } else { "  " };
            buf.set_line(
                area.x,
                area.y + u16::try_from(row).unwrap_or(0),
                &Line::from(Span::styled(format!("{marker}{item}"), style)),
                area.width,
            );
        }
    }

    fn render_keybindings(&self, buf: &mut Buffer, area: Rect, ctx: &OverlayCtx<'_>) {
        if self.keybindings.is_empty() {
            buf.set_line(
                area.x,
                area.y,
                &Line::from(Span::styled(
                    "  (no keybinding overrides configured)",
                    Style::default().fg(ctx.theme.status_dim_fg),
                )),
                area.width,
            );
            return;
        }
        let rows = usize::from(area.height);
        let offset = scroll_offset(self.list_scroll, self.keybindings.len(), rows);
        for (row, (chord, action)) in self.keybindings.iter().skip(offset).take(rows).enumerate() {
            buf.set_line(
                area.x,
                area.y + u16::try_from(row).unwrap_or(0),
                &Line::from(vec![
                    Span::styled(
                        format!("  {chord:<18}"),
                        Style::default()
                            .fg(ctx.theme.focus_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(action.clone(), Style::default().fg(ctx.theme.assistant_fg)),
                ]),
                area.width,
            );
        }
    }
}

/// Keep `selected` visible within a window of `rows` over `total`.
fn scroll_offset(selected: usize, total: usize, rows: usize) -> usize {
    if rows == 0 || total <= rows {
        return 0;
    }
    selected
        .saturating_sub(rows / 2)
        .min(total.saturating_sub(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn sample() -> SettingsOverlay {
        SettingsOverlay::new(SettingsInit {
            themes: vec!["default".into(), "tokyo-night".into(), "ayu".into()],
            theme: "tokyo-night".into(),
            models: vec!["anthropic:opus".into(), "anthropic:sonnet".into()],
            model: "anthropic:sonnet".into(),
            mouse: true,
            threshold: 0.8,
            keybindings: vec![("ctrl+x".into(), "compact".into())],
            editor_modeless: false,
        })
    }

    #[test]
    fn selection_starts_on_active_values() {
        let s = sample();
        assert_eq!(s.selected_theme(), "tokyo-night");
        assert_eq!(s.selected_model(), "anthropic:sonnet");
        assert!(s.mouse());
    }

    #[test]
    fn tab_cycles_sections_and_wraps() {
        let mut s = sample();
        for _ in 0..TABS.len() {
            assert_eq!(s.handle_key(key(KeyCode::Tab)), OverlayAction::Stay);
        }
        assert_eq!(s.tab, 0);
        assert_eq!(s.handle_key(key(KeyCode::BackTab)), OverlayAction::Stay);
        assert_eq!(s.tab, TABS.len() - 1);
    }

    #[test]
    fn up_down_change_theme_selection_and_wrap() {
        let mut s = sample(); // theme tab, idx 1 (tokyo-night)
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.selected_theme(), "ayu");
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.selected_theme(), "default");
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.selected_theme(), "ayu");
    }

    #[test]
    fn mouse_tab_space_toggles() {
        let mut s = sample();
        s.handle_key(key(KeyCode::Tab)); // Model
        s.handle_key(key(KeyCode::Tab)); // Mouse
        assert_eq!(s.tab, 2);
        assert!(s.mouse());
        s.handle_key(key(KeyCode::Char(' ')));
        assert!(!s.mouse());
        s.handle_key(key(KeyCode::Up));
        assert!(s.mouse());
    }

    #[test]
    fn editor_tab_toggles_and_resolves_modeless() {
        let mut s = sample();
        // Editor is the last tab.
        for _ in 0..(TABS.len() - 1) {
            s.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(s.tab, TABS.len() - 1);
        // Default sample is vim (false); Space flips to modeless.
        s.handle_key(key(KeyCode::Char(' ')));
        match s.handle_key(key(KeyCode::Enter)) {
            OverlayAction::Resolve(v) => assert_eq!(v["editor"], serde_json::json!("modeless")),
            other => panic!("expected Resolve, got {other:?}"),
        }
    }

    #[test]
    fn editor_defaults_to_vim_in_result() {
        let s = sample();
        assert_eq!(s.result_json()["editor"], serde_json::json!("vim"));
    }

    #[test]
    fn autocompaction_clamps_to_band() {
        let mut s = sample();
        for _ in 0..3 {
            s.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(s.tab, 3);
        for _ in 0..50 {
            s.handle_key(key(KeyCode::Right));
        }
        assert!((s.threshold() - THRESHOLD_MAX).abs() < f32::EPSILON);
        for _ in 0..50 {
            s.handle_key(key(KeyCode::Left));
        }
        assert!((s.threshold() - THRESHOLD_MIN).abs() < f32::EPSILON);
    }

    #[test]
    fn enter_resolves_with_edited_values() {
        let mut s = sample();
        s.handle_key(key(KeyCode::Down)); // theme -> ayu
        match s.handle_key(key(KeyCode::Enter)) {
            OverlayAction::Resolve(v) => {
                assert_eq!(v["theme"], serde_json::json!("ayu"));
                assert_eq!(v["model"], serde_json::json!("anthropic:sonnet"));
                assert_eq!(v["mouse"], serde_json::json!(true));
                assert_eq!(v["compaction_threshold"], serde_json::json!(0.8));
            }
            other => panic!("expected Resolve, got {other:?}"),
        }
    }

    #[test]
    fn esc_and_ctrl_c_cancel() {
        let mut s = sample();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), OverlayAction::Close);
        let cc = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(s.handle_key(cc), OverlayAction::Close);
    }

    #[test]
    fn ctrl_s_resolves() {
        let mut s = sample();
        let cs = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(matches!(s.handle_key(cs), OverlayAction::Resolve(_)));
    }

    #[test]
    fn empty_lists_do_not_panic() {
        let mut s = SettingsOverlay::new(SettingsInit::default());
        s.handle_key(key(KeyCode::Down));
        s.handle_key(key(KeyCode::Tab));
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.selected_theme(), "");
        assert_eq!(s.selected_model(), "");
    }
}
