//! Prompt-input autocomplete popup.
//!
//! A light, non-modal suggestion list floated just above the input
//! card (like the slash palette), driven by the plugin autocomplete
//! provider stack (`kage.add_autocomplete_provider`). Unlike the slash
//! palette it never takes full keyboard ownership: only Up / Down /
//! Ctrl-p / Ctrl-n (navigate), Tab (accept), and Esc (dismiss) are
//! consumed. Every other key passes through to normal text editing,
//! after which the host re-queries the provider stack and rebuilds the
//! popup. The host computes the splice range when a candidate is
//! accepted; this widget only owns selection and painting.

use kage_core::protocol::McpServerInfo;
use kage_plugin::AutocompleteItem;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::layout::Regions;
use crate::view::UnicodeWidthStr as _;
use crate::view::{pad_to_width, scroll_offset_centered, truncate_to_width};

/// Most visible rows before `... N more` indicators kick in.
const MAX_VISIBLE: usize = 8;

/// Outcome of routing a key into an open completion popup.
#[derive(Debug, PartialEq, Eq)]
pub enum CompletionAction {
    /// Key consumed; the selection moved and the popup stays open.
    Navigated,
    /// User dismissed the popup (Esc); key consumed, no edit.
    Dismissed,
    /// User accepted a candidate (Tab); key consumed. The host applies
    /// it via [`crate::input::InputState::splice`].
    Accepted(AutocompleteItem),
    /// The popup does not own this key; the host routes it to the
    /// input and then re-queries the provider stack.
    PassThrough,
}

/// Inline autocomplete popup for the prompt input.
#[derive(Debug)]
pub struct InputCompletion {
    items: Vec<AutocompleteItem>,
    selected: usize,
}

impl InputCompletion {
    /// Build a popup for `items`, or `None` when there is nothing to
    /// show so the host can clear any open popup uniformly.
    #[must_use]
    pub fn new(items: Vec<AutocompleteItem>) -> Option<Self> {
        if items.is_empty() {
            None
        } else {
            Some(Self { items, selected: 0 })
        }
    }

    /// Index of the highlighted row.
    #[cfg(test)]
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The candidates, in display order.
    #[cfg(test)]
    #[must_use]
    pub fn items(&self) -> &[AutocompleteItem] {
        &self.items
    }

    /// Number of candidates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the popup has no candidates. Always `false` for a popup
    /// built through [`Self::new`]; present so clippy does not nag
    /// about a `len` without an `is_empty`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Route a key. See [`CompletionAction`].
    pub fn handle_key(&mut self, key: KeyEvent) -> CompletionAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Down => {
                self.select_next();
                CompletionAction::Navigated
            }
            KeyCode::Up => {
                self.select_prev();
                CompletionAction::Navigated
            }
            KeyCode::Char('n') if ctrl => {
                self.select_next();
                CompletionAction::Navigated
            }
            KeyCode::Char('p') if ctrl => {
                self.select_prev();
                CompletionAction::Navigated
            }
            KeyCode::Tab => CompletionAction::Accepted(self.items[self.selected].clone()),
            KeyCode::Esc => CompletionAction::Dismissed,
            _ => CompletionAction::PassThrough,
        }
    }

    fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    fn select_prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + self.items.len() - 1) % self.items.len();
        }
    }

    /// Paint the popup above the input card. The host draws the rest
    /// of the frame first, then calls this when the popup is open.
    pub fn render(&self, frame: &mut Frame, regions: Regions) {
        let Some(area) = popup_area(regions, self.items.len(), self.selected) else {
            return;
        };
        frame.render_widget(crate::opaque::OpaqueClear, area);
        let lines = self.rows(usize::from(area.width));
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn rows(&self, width: usize) -> Vec<Line<'static>> {
        let theme = crate::theme::current();
        let bg = theme.modeline_bg;
        let row_style = Style::default().fg(theme.overlay_fg).bg(bg);
        let dim_style = Style::default().fg(theme.status_dim_fg).bg(bg);
        let sel_style = Style::default()
            .fg(theme.overlay_selected_fg)
            .bg(theme.overlay_selected_bg)
            .add_modifier(Modifier::BOLD);

        let total = self.items.len();
        let max_visible = MAX_VISIBLE.min(total);
        let offset = scroll_offset_centered(self.selected, total, max_visible);
        let window = total.min(max_visible);
        let above = offset;
        let below = total.saturating_sub(offset + window);

        let label_col = self
            .items
            .iter()
            .skip(offset)
            .take(window)
            .map(|i| i.label.width())
            .max()
            .unwrap_or(0);

        let mut lines: Vec<Line<'static>> = Vec::new();
        if above > 0 {
            lines.push(Line::from(Span::styled(
                pad(&format!("  ... {above} more above"), width),
                dim_style,
            )));
        }
        for (idx, item) in self.items.iter().enumerate().skip(offset).take(window) {
            let selected = idx == self.selected;
            let main = if selected { sel_style } else { row_style };
            let aside = if selected { sel_style } else { dim_style };
            lines.push(row(item, label_col, width, main, aside));
        }
        if below > 0 {
            lines.push(Line::from(Span::styled(
                pad(&format!("  ... {below} more below"), width),
                dim_style,
            )));
        }
        lines
    }
}

fn row(
    item: &AutocompleteItem,
    label_col: usize,
    width: usize,
    main: Style,
    aside: Style,
) -> Line<'static> {
    let leading = "  ";
    let label_width = item.label.width();
    let gap = label_col.saturating_sub(label_width);
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(leading.to_owned(), main),
        Span::styled(item.label.clone(), main),
    ];
    if gap > 0 {
        spans.push(Span::styled(" ".repeat(gap), main));
    }
    if let Some(detail) = item.detail.as_deref() {
        let used = leading.width() + label_width + gap;
        let room = width.saturating_sub(used).saturating_sub(2);
        if room > 0 {
            spans.push(Span::styled("  ".to_owned(), aside));
            spans.push(Span::styled(
                truncate_to_width(detail, room, "\u{2026}"),
                aside,
            ));
        }
    }
    let painted: usize = spans.iter().map(|s| s.content.width()).sum();
    if painted < width {
        spans.push(Span::styled(" ".repeat(width - painted), main));
    }
    Line::from(spans)
}

/// The rect just above the input card, clamped to the space between
/// the conversation buffer and the input. `None` when there is no
/// room or nothing to show.
fn popup_area(regions: Regions, total: usize, selected: usize) -> Option<Rect> {
    if total == 0 || regions.input.width == 0 {
        return None;
    }
    let max_visible = MAX_VISIBLE.min(total);
    let offset = scroll_offset_centered(selected, total, max_visible);
    let window = total.min(max_visible);
    let above = offset;
    let below = total.saturating_sub(offset + window);
    let rows = window + usize::from(above > 0) + usize::from(below > 0);
    let space_above = regions.input.y.saturating_sub(regions.buffer.y);
    let rows = rows.min(usize::from(space_above));
    if rows == 0 {
        return None;
    }
    let height = u16::try_from(rows).unwrap_or(u16::MAX);
    Some(Rect {
        x: regions.input.x,
        y: regions.input.y.saturating_sub(height),
        width: regions.input.width,
        height,
    })
}

/// Truncate to `width`, then right-pad to exactly `width`; indicator
/// rows must fill the full card width or the bubble edge tears.
fn pad(s: &str, width: usize) -> String {
    pad_to_width(&truncate_to_width(s, width, ""), width)
}

/// The completion prefix: the run of non-whitespace characters
/// immediately before `cursor`. Providers also receive the full text
/// and cursor so they can tokenize differently (e.g. an `@file`
/// trigger); this is only the host's default replace span.
#[must_use]
pub fn prefix_before_cursor(text: &str, cursor: usize) -> &str {
    let head = &text[..cursor.min(text.len())];
    let start = head
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(idx, c)| idx + c.len_utf8());
    &head[start..]
}

/// Filesystem entries the recursive `@file` walk scans before giving
/// up, so a huge tree cannot stall a keystroke.
const FILE_WALK_CAP: usize = 20_000;

/// Most `@file` or `@server:` candidates handed to the popup.
const FILE_RESULT_CAP: usize = 50;

/// Built-in `@file` completion: the bottom of the provider stack.
///
/// When the token under the cursor starts with `@`, fuzzy-match the
/// text after it against every workdir-relative path (recursively,
/// honoring `.gitignore` / `.kageignore` like the `find` and `ls`
/// tools) and offer the best matches as candidates that replace the
/// whole `@...` token. A bare `@` lists the immediate children only
/// (fast); once you type, the match is recursive, so `@main` finds
/// `src/main.rs`. Directories gain a trailing `/`. Ranking prefers a
/// substring of the file name, then of the full path, then a
/// subsequence; ties break on shorter path then name. Results are
/// capped so a large tree cannot blow up the popup.
#[must_use]
pub fn file_completions(
    workdir: &std::path::Path,
    prefix: &str,
    cursor: usize,
) -> Vec<AutocompleteItem> {
    let Some(frag) = prefix.strip_prefix('@') else {
        return Vec::new();
    };
    let token_start = cursor.saturating_sub(prefix.len());
    let query = frag.to_lowercase();

    let mut builder = ignore::WalkBuilder::new(workdir);
    builder.add_custom_ignore_filename(".kageignore");
    if query.is_empty() {
        // Bare `@`: a shallow listing keeps the first keystroke snappy.
        builder.max_depth(Some(1));
    }

    let mut scored: Vec<(u8, usize, String, bool)> = Vec::new();
    for (scanned, entry) in builder.build().enumerate() {
        if scanned >= FILE_WALK_CAP {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(rel) = entry.path().strip_prefix(workdir) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let Some(rel) = rel.to_str() else {
            continue;
        };
        let Some(rank) = match_rank(&query, rel) else {
            continue;
        };
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        scored.push((rank, rel.len(), rel.to_owned(), is_dir));
    }
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    scored.truncate(FILE_RESULT_CAP);
    scored
        .into_iter()
        .map(|(_, _, rel, is_dir)| {
            let suffix = if is_dir { "/" } else { "" };
            AutocompleteItem {
                label: format!("{rel}{suffix}"),
                detail: Some(if is_dir { "dir" } else { "file" }.to_owned()),
                value: format!("@{rel}{suffix}"),
                range: Some((token_start, cursor)),
            }
        })
        .collect()
}

/// Built-in `@server:` completion for MCP resources.
///
/// A fragment `server:` or `server:part` lists that server's resources,
/// fuzzy matched over URI and name, then its resource templates, which
/// are inserted as written. A fragment without `:` lists the matching
/// server names as `@name:` items, which the host appends after the
/// file items. Only servers that list resources or templates are
/// offered. Candidates replace the whole `@...` token.
#[must_use]
pub fn mcp_completions(
    servers: &[McpServerInfo],
    prefix: &str,
    cursor: usize,
) -> Vec<AutocompleteItem> {
    let Some(frag) = prefix.strip_prefix('@') else {
        return Vec::new();
    };
    let range = Some((cursor.saturating_sub(prefix.len()), cursor));
    let mut offered = servers
        .iter()
        .filter(|s| !s.resources.is_empty() || !s.templates.is_empty());
    let Some((name, part)) = frag.split_once(':') else {
        let query = frag.to_lowercase();
        return offered
            .filter(|s| match_rank(&query, &s.name).is_some())
            .map(|s| AutocompleteItem {
                label: format!("{}:", s.name),
                detail: Some("mcp server".to_owned()),
                value: format!("@{}:", s.name),
                range,
            })
            .collect();
    };
    let Some(server) = offered.find(|s| s.name == name) else {
        return Vec::new();
    };
    let query = part.to_lowercase();
    let rank = |uri: &str, title: &str| {
        match_rank(&query, uri)
            .into_iter()
            .chain(match_rank(&query, title))
            .min()
    };
    let item = |uri: &str, detail: String| AutocompleteItem {
        label: format!("{name}:{uri}"),
        detail: Some(detail),
        value: format!("@{name}:{uri}"),
        range,
    };
    let resources = server.resources.iter().filter_map(|r| {
        let detail = match &r.mime_type {
            Some(mime) => format!("{}  {mime}", r.name),
            None => r.name.clone(),
        };
        Some((rank(&r.uri, &r.name)?, item(&r.uri, detail)))
    });
    let templates = server.templates.iter().filter_map(|t| {
        let detail = format!("template: {}", t.name);
        Some((
            rank(&t.uri_template, &t.name)?,
            item(&t.uri_template, detail),
        ))
    });
    let mut scored: Vec<(u8, AutocompleteItem)> = resources.chain(templates).collect();
    scored.sort_by_key(|(rank, _)| *rank);
    scored
        .into_iter()
        .take(FILE_RESULT_CAP)
        .map(|(_, item)| item)
        .collect()
}

/// Rank a workdir-relative path against the already-lowercased
/// `query`. Lower is better; `None` means no match. An empty query
/// matches everything at a neutral rank so a bare `@` still lists.
fn match_rank(query: &str, rel: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(4);
    }
    let lower = rel.to_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(lower.as_str());
    if base.contains(query) {
        Some(0)
    } else if lower.contains(query) {
        Some(1)
    } else if is_subsequence(query, base) {
        Some(2)
    } else if is_subsequence(query, &lower) {
        Some(3)
    } else {
        None
    }
}

/// `true` if every char of `needle` appears in `haystack` in order
/// (not necessarily contiguously). Both are expected lowercased.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    'next: for want in needle.chars() {
        for have in chars.by_ref() {
            if have == want {
                continue 'next;
            }
        }
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(value: &str) -> AutocompleteItem {
        AutocompleteItem {
            label: value.to_owned(),
            detail: None,
            value: value.to_owned(),
            range: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn new_is_none_when_empty() {
        assert!(InputCompletion::new(Vec::new()).is_none());
    }

    #[test]
    fn down_and_up_wrap_around() {
        let mut c = InputCompletion::new(vec![item("a"), item("b")]).unwrap();
        assert_eq!(c.selected_index(), 0);
        assert_eq!(
            c.handle_key(key(KeyCode::Down)),
            CompletionAction::Navigated
        );
        assert_eq!(c.selected_index(), 1);
        c.handle_key(key(KeyCode::Down));
        assert_eq!(c.selected_index(), 0);
        c.handle_key(key(KeyCode::Up));
        assert_eq!(c.selected_index(), 1);
    }

    #[test]
    fn ctrl_n_ctrl_p_navigate() {
        let mut c = InputCompletion::new(vec![item("a"), item("b")]).unwrap();
        let cn = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(c.handle_key(cn), CompletionAction::Navigated);
        assert_eq!(c.selected_index(), 1);
        let cp = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        c.handle_key(cp);
        assert_eq!(c.selected_index(), 0);
    }

    #[test]
    fn tab_accepts_selected_item() {
        let mut c = InputCompletion::new(vec![item("first"), item("second")]).unwrap();
        c.handle_key(key(KeyCode::Down));
        match c.handle_key(key(KeyCode::Tab)) {
            CompletionAction::Accepted(it) => assert_eq!(it.value, "second"),
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[test]
    fn esc_dismisses_and_other_keys_pass_through() {
        let mut c = InputCompletion::new(vec![item("a")]).unwrap();
        assert_eq!(c.handle_key(key(KeyCode::Esc)), CompletionAction::Dismissed);
        assert_eq!(
            c.handle_key(key(KeyCode::Char('x'))),
            CompletionAction::PassThrough
        );
    }

    #[test]
    fn prefix_is_run_before_cursor() {
        assert_eq!(prefix_before_cursor("hello wor", 9), "wor");
        assert_eq!(prefix_before_cursor("hello wor", 5), "hello");
        assert_eq!(prefix_before_cursor("", 0), "");
        assert_eq!(prefix_before_cursor("@src/m", 6), "@src/m");
        assert_eq!(prefix_before_cursor("a b", 2), "");
    }

    #[test]
    fn file_completions_lists_workdir_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        std::fs::write(dir.path().join(".hidden"), "x").unwrap();
        let items = file_completions(dir.path(), "@", 1);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        // Directory sorts first with a trailing slash; dotfile hidden.
        assert_eq!(labels, vec!["src/", "README.md"]);
        assert_eq!(items[0].value, "@src/");
        assert_eq!(items[0].detail.as_deref(), Some("dir"));
        assert_eq!(items[0].range, Some((0, 1)));
    }

    #[test]
    fn file_completions_filters_by_partial_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "x").unwrap();
        let items = file_completions(dir.path(), "@RE", 3);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "@README.md");
        assert_eq!(items[0].range, Some((0, 3)));
    }

    #[test]
    fn file_completions_descends_into_subdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "x").unwrap();
        let items = file_completions(dir.path(), "@src/ma", 7);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "@src/main.rs");
    }

    #[test]
    fn file_completions_finds_nested_file_by_name() {
        // Regression: typing a bare name must reach files in
        // subdirectories, not only the top level.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/loop")).unwrap();
        std::fs::write(dir.path().join("src/loop/run.rs"), "x").unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        let items = file_completions(dir.path(), "@run", 4);
        let values: Vec<&str> = items.iter().map(|i| i.value.as_str()).collect();
        assert!(
            values.contains(&"@src/loop/run.rs"),
            "expected nested hit, got {values:?}"
        );
    }

    #[test]
    fn file_completions_ranks_basename_substring_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("app")).unwrap();
        std::fs::write(dir.path().join("app/main.rs"), "x").unwrap();
        std::fs::write(dir.path().join("mainframe.txt"), "x").unwrap();
        // "main" is a basename substring of both; the shorter path
        // wins the tie, but both rank above a mere subsequence.
        let items = file_completions(dir.path(), "@main", 5);
        let values: Vec<&str> = items.iter().map(|i| i.value.as_str()).collect();
        assert!(values.contains(&"@app/main.rs"));
        assert!(values.contains(&"@mainframe.txt"));
    }

    #[test]
    fn file_completions_fuzzy_subsequence_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crates/kage-tui/src")).unwrap();
        std::fs::write(dir.path().join("crates/kage-tui/src/app.rs"), "x").unwrap();
        // Non-contiguous subsequence: c..t..app should still hit.
        let items = file_completions(dir.path(), "@ktapp", 6);
        let values: Vec<&str> = items.iter().map(|i| i.value.as_str()).collect();
        assert!(
            values.contains(&"@crates/kage-tui/src/app.rs"),
            "expected subsequence hit, got {values:?}"
        );
    }

    #[test]
    fn file_completions_skips_hidden_entries() {
        // Hidden entries are skipped (consistent with the `find` /
        // `ls` tools' walker defaults). `.gitignore` rules also apply
        // in a real git workdir; that path is not exercised here
        // because the walker requires a repo for git rules.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".secret"), "x").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "x").unwrap();
        let items = file_completions(dir.path(), "@", 1);
        let values: Vec<&str> = items.iter().map(|i| i.value.as_str()).collect();
        assert!(values.contains(&"@visible.txt"));
        assert!(!values.iter().any(|v| v.contains(".secret")));
    }

    fn servers() -> Vec<McpServerInfo> {
        use kage_core::protocol::{McpResource, McpResourceTemplate, McpServerStatus};
        let resource = |uri: &str, name: &str| McpResource {
            uri: uri.to_owned(),
            name: name.to_owned(),
            description: None,
            mime_type: Some("text/plain".to_owned()),
        };
        let server = |name: &str| McpServerInfo {
            name: name.to_owned(),
            status: McpServerStatus::Connected,
            tools: 0,
            resources: Vec::new(),
            templates: Vec::new(),
            prompts: Vec::new(),
        };
        vec![
            McpServerInfo {
                resources: vec![
                    resource("test://static/resource/1", "Resource 1"),
                    resource("test://static/notes", "Meeting notes"),
                ],
                templates: vec![McpResourceTemplate {
                    uri_template: "test://static/resource/{id}".to_owned(),
                    name: "Static resource".to_owned(),
                    description: None,
                }],
                ..server("everything")
            },
            server("toolsonly"),
        ]
    }

    #[test]
    fn mcp_completions_offer_servers_with_resources_by_name() {
        let items = mcp_completions(&servers(), "@every", 6);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "everything:");
        assert_eq!(items[0].value, "@everything:");
        assert_eq!(items[0].detail.as_deref(), Some("mcp server"));
        assert_eq!(items[0].range, Some((0, 6)));
        assert!(mcp_completions(&servers(), "@tools", 6).is_empty());
        assert!(mcp_completions(&servers(), "every", 5).is_empty());
    }

    #[test]
    fn mcp_completions_list_resources_then_templates_of_a_server() {
        let items = mcp_completions(&servers(), "@everything:", 16);
        let values: Vec<&str> = items.iter().map(|i| i.value.as_str()).collect();
        assert_eq!(
            values,
            [
                "@everything:test://static/resource/1",
                "@everything:test://static/notes",
                "@everything:test://static/resource/{id}",
            ]
        );
        assert_eq!(items[0].detail.as_deref(), Some("Resource 1  text/plain"));
        assert_eq!(
            items[2].detail.as_deref(),
            Some("template: Static resource")
        );
        assert_eq!(items[0].range, Some((4, 16)));
    }

    #[test]
    fn mcp_completions_match_the_uri_or_the_name() {
        let by_name = mcp_completions(&servers(), "@everything:meet", 16);
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].value, "@everything:test://static/notes");
        let by_uri = mcp_completions(&servers(), "@everything:resource/", 21);
        let values: Vec<&str> = by_uri.iter().map(|i| i.value.as_str()).collect();
        assert_eq!(
            values,
            [
                "@everything:test://static/resource/1",
                "@everything:test://static/resource/{id}",
            ]
        );
        assert!(mcp_completions(&servers(), "@nope:x", 7).is_empty());
    }

    #[test]
    fn file_completions_empty_without_at_prefix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "x").unwrap();
        assert!(file_completions(dir.path(), "a", 1).is_empty());
    }
}
