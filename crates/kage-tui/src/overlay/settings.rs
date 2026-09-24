//! `:settings` multi-tab overlay.
//!
//! A modal [`OverlayWidget`] with seven sections: Theme, Model,
//! Mouse, Autocompaction, a read-only Keybindings list, Editor, and
//! Thinking. `Tab`/`BackTab` cycle sections; within a section the
//! arrow keys change the value; `Enter` or `Ctrl+S` resolves the
//! values that changed as JSON and `Esc` cancels. The host applies
//! the result live (theme, mouse, model, thinking level) and
//! persists it comment-preserving via `Config::save`.
//!
//! The Thinking tab edits the persisted default level for new
//! sessions; the live session level stays on the `Shift+Tab` cycle
//! and follows when the host applies a resolved change.
//!
//! A tab whose option was last set from Lua says so, since `init.lua`
//! sets it again on the next start and shadows the saved value.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use crate::overlay::widget::{OverlayAction, OverlayCtx, OverlayWidget};
use crate::view::scroll_offset_centered;

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
    "Thinking",
];

/// Option each tab edits, by tab index.
const TAB_OPTIONS: [Option<&str>; 7] = [
    Some("theme"),
    None,
    Some("mouse"),
    Some("compaction_threshold"),
    None,
    Some("editor"),
    Some("thinking_level"),
];

/// Shown on a tab whose option was last set from Lua.
const LUA_NOTE: &str = "  set from Lua: init.lua shadows a saved value on the next start";

/// Thinking-level ladder, mirroring the `kage_provider::ThinkingLevel`
/// wire strings. kage-tui is provider-free, so the ladder is spelled
/// here and the host's worker parses the resolved string.
const LADDER: [&str; 6] = ["off", "minimal", "low", "medium", "high", "xhigh"];

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
    /// Persisted thinking level the dialog starts from, one of the
    /// [`LADDER`] strings; unknown strings fall back to `"off"`.
    pub thinking_level: String,
    /// Options whose current value was last set from Lua.
    pub from_lua: Vec<&'static str>,
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
    thinking_idx: usize,
    /// The values the dialog opened with. `result_json` resolves only
    /// fields that differ from these, so a value inherited from the
    /// project config layer is never stamped into the user file.
    seed: SettingsInit,
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
        let thinking_idx = LADDER
            .iter()
            .position(|l| *l == init.thinking_level)
            .unwrap_or(0);
        Self {
            tab: 0,
            seed: init.clone(),
            themes: init.themes,
            theme_idx,
            models: init.models,
            model_idx,
            mouse: init.mouse,
            threshold: init.threshold.clamp(THRESHOLD_MIN, THRESHOLD_MAX),
            keybindings: init.keybindings,
            editor_modeless: init.editor_modeless,
            thinking_idx,
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

    /// Selected thinking level, a [`LADDER`] wire string.
    #[must_use]
    pub fn selected_thinking_level(&self) -> &'static str {
        LADDER[self.thinking_idx]
    }

    /// The edited settings as the JSON the host applies on resolve.
    /// Only values that differ from the seed are included: the host
    /// persists whatever arrives, so values inherited from the
    /// project config layer are never copied into the user file.
    #[must_use]
    pub fn result_json(&self) -> serde_json::Value {
        // The threshold moves on a 0.05 grid; round to 2 decimals so
        // neither the JSON nor the persisted config carries an ugly
        // f32->f64 tail (0.800000011...).
        let threshold = (f64::from(self.threshold) * 100.0).round() / 100.0;
        let seed_threshold = (f64::from(self.seed.threshold) * 100.0).round() / 100.0;
        let editor = if self.editor_modeless {
            "modeless"
        } else {
            "vim"
        };
        let seed_editor = if self.seed.editor_modeless {
            "modeless"
        } else {
            "vim"
        };
        let mut out = serde_json::Map::new();
        if self.selected_theme() != self.seed.theme {
            out.insert("theme".into(), serde_json::json!(self.selected_theme()));
        }
        if self.selected_model() != self.seed.model {
            out.insert("model".into(), serde_json::json!(self.selected_model()));
        }
        if self.mouse != self.seed.mouse {
            out.insert("mouse".into(), serde_json::json!(self.mouse));
        }
        if (threshold - seed_threshold).abs() > f64::EPSILON {
            out.insert("compaction_threshold".into(), serde_json::json!(threshold));
        }
        if editor != seed_editor {
            out.insert("editor".into(), serde_json::json!(editor));
        }
        if self.selected_thinking_level() != self.seed.thinking_level {
            out.insert(
                "thinking_level".into(),
                serde_json::json!(self.selected_thinking_level()),
            );
        }
        serde_json::Value::Object(out)
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
            6 => self.thinking_idx = step_index(self.thinking_idx, LADDER.len(), delta),
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
                    .fg(ctx.theme.selection_fg)
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

        let mut body = rows[2];
        if self.set_from_lua() {
            body.height = body.height.saturating_sub(1);
            buf.set_line(
                body.x,
                body.y + body.height,
                &Line::from(Span::styled(
                    LUA_NOTE,
                    Style::default().fg(ctx.theme.status_dim_fg),
                )),
                body.width,
            );
        }
        self.render_body(buf, body, ctx);

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
    /// Whether the active tab's option was last set from Lua.
    fn set_from_lua(&self) -> bool {
        TAB_OPTIONS[self.tab].is_some_and(|name| self.seed.from_lua.contains(&name))
    }

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
            6 => {
                buf.set_line(
                    area.x,
                    area.y,
                    &Line::from(Span::styled(
                        format!("  thinking level: {}", self.selected_thinking_level()),
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
                        "  default for new sessions; shift+tab cycles this session",
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
        let offset = scroll_offset_centered(selected, items.len(), rows);
        for (row, (idx, item)) in items.iter().enumerate().skip(offset).take(rows).enumerate() {
            let style = if idx == selected {
                Style::default()
                    .fg(ctx.theme.selection_fg)
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
        let offset = scroll_offset_centered(self.list_scroll, self.keybindings.len(), rows);
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
            thinking_level: "off".into(),
            from_lua: vec!["mouse"],
        })
    }

    fn rendered(s: &mut SettingsOverlay) -> String {
        let area = Rect::new(0, 0, 90, 20);
        let mut buf = Buffer::empty(area);
        let theme = crate::theme::current();
        let ctx = OverlayCtx {
            theme: &theme,
            viewport: area,
        };
        OverlayWidget::render(s, area, &mut buf, &ctx);
        buf.content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn tab_set_from_lua_shows_the_shadow_note() {
        let mut s = sample();
        assert!(!rendered(&mut s).contains("set from Lua"));
        s.handle_key(key(KeyCode::Tab));
        s.handle_key(key(KeyCode::Tab));
        assert_eq!(s.tab, 2);
        assert!(rendered(&mut s).contains("set from Lua: init.lua shadows"));
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
        // Editor is the second-to-last tab; Thinking follows it.
        for _ in 0..(TABS.len() - 2) {
            s.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(s.tab, TABS.len() - 2);
        // Default sample is vim (false); Space flips to modeless.
        s.handle_key(key(KeyCode::Char(' ')));
        match s.handle_key(key(KeyCode::Enter)) {
            OverlayAction::Resolve(v) => assert_eq!(v["editor"], serde_json::json!("modeless")),
            other => panic!("expected Resolve, got {other:?}"),
        }
    }

    #[test]
    fn unchanged_fields_are_omitted_from_result() {
        // Fresh from the seed nothing changed, so nothing resolves:
        // the host then applies nothing and rewrites no config. This
        // is what keeps a project-layer value from being stamped
        // into the user config on an untouched dialog.
        let s = sample();
        assert!(s.result_json().as_object().unwrap().is_empty());
        // Change only the theme: exactly that key resolves.
        let mut s = sample();
        s.handle_key(key(KeyCode::Down)); // theme -> ayu
        let v = s.result_json();
        assert_eq!(v["theme"], serde_json::json!("ayu"));
        assert!(v.get("model").is_none());
        assert!(v.get("mouse").is_none());
        assert!(v.get("compaction_threshold").is_none());
        assert!(v.get("editor").is_none());
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
    fn enter_resolves_only_edited_values() {
        let mut s = sample();
        s.handle_key(key(KeyCode::Down)); // theme -> ayu
        match s.handle_key(key(KeyCode::Enter)) {
            OverlayAction::Resolve(v) => {
                assert_eq!(v["theme"], serde_json::json!("ayu"));
                assert_eq!(v.as_object().unwrap().len(), 1, "only the edit resolves");
            }
            other => panic!("expected Resolve, got {other:?}"),
        }
    }

    #[test]
    fn thinking_tab_cycles_ladder_and_wraps() {
        let mut s = sample();
        for _ in 0..(TABS.len() - 1) {
            s.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(s.tab, TABS.len() - 1);
        assert_eq!(s.selected_thinking_level(), "off");
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.selected_thinking_level(), "minimal");
        for _ in 0..4 {
            s.handle_key(key(KeyCode::Down));
        }
        assert_eq!(s.selected_thinking_level(), "xhigh");
        // Both ladder ends wrap.
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.selected_thinking_level(), "off");
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.selected_thinking_level(), "xhigh");
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.selected_thinking_level(), "high");
    }

    #[test]
    fn thinking_level_resolves_only_when_changed() {
        let s = sample();
        assert!(s.result_json().get("thinking_level").is_none());
        let mut s = sample();
        for _ in 0..(TABS.len() - 1) {
            s.handle_key(key(KeyCode::Tab));
        }
        s.handle_key(key(KeyCode::Down)); // off -> minimal
        match s.handle_key(key(KeyCode::Enter)) {
            OverlayAction::Resolve(v) => {
                assert_eq!(v["thinking_level"], serde_json::json!("minimal"));
                assert_eq!(v.as_object().unwrap().len(), 1, "only the edit resolves");
            }
            other => panic!("expected Resolve, got {other:?}"),
        }
    }

    #[test]
    fn unknown_thinking_seed_falls_back_to_off() {
        let init = SettingsInit {
            thinking_level: "maximum".into(),
            ..SettingsInit::default()
        };
        let s = SettingsOverlay::new(init);
        assert_eq!(s.selected_thinking_level(), "off");
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
