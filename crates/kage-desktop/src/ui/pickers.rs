//! The composer's model and permission pickers, as the design's
//! popovers draw them. The composer hosts each above its pill.
//!
//! The model picker searches every model the engine can run, starred
//! ones first and the rest under their provider, each with its context
//! window, prices and a star, over a thinking level row that dims the
//! levels the chosen model does not take. The permission picker lists
//! the modes with what each does to every kind of tool call. Both keep
//! the keys on their search field: the arrows move the highlight,
//! Enter picks and Esc closes.

use crate::store::Store;
use crate::theme::Palette;
use kage_client::wire::{ModelEntry, ModelProvider, SessionConfigSelectOption};

/// What a picker asks of the composer that hosts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerEvent {
    /// The pick is made or abandoned: close the picker.
    Close,
}

/// One model row: the facts the row shows.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRow {
    /// The `provider/model` value the option takes.
    pub id: String,
    /// The display name.
    pub name: String,
    /// The provider's display name.
    pub provider: String,
    /// The model half of the id, for the row's second line.
    pub model: String,
    /// The context window in tokens, when known.
    pub context: Option<u64>,
    /// USD per million input and output tokens, when known.
    pub cost: Option<(f64, f64)>,
    /// Whether the model reads images.
    pub images: bool,
}

impl ModelRow {
    /// The row one catalog entry becomes, under `provider`'s name.
    #[must_use]
    pub fn of(provider: &str, entry: &ModelEntry) -> Self {
        let model = entry
            .id
            .split_once('/')
            .map_or(entry.id.as_str(), |(_, model)| model)
            .to_owned();
        Self {
            id: entry.id.clone(),
            name: entry.name.clone(),
            provider: provider.to_owned(),
            model,
            context: entry.context,
            cost: entry.input_cost.zip(entry.output_cost),
            images: entry.images,
        }
    }

    /// Whether the row matches `needle`, an already lowercased query,
    /// over its id, name and provider's display name.
    #[must_use]
    pub fn matches(&self, needle: &str, provider_name: &str) -> bool {
        needle.is_empty()
            || format!("{} {} {provider_name}", self.id, self.name)
                .to_lowercase()
                .contains(needle)
    }
}

/// A labeled run of rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// The section's label, as the popover prints it.
    pub label: String,
    /// The rows under the label.
    pub rows: Vec<ModelRow>,
}

/// The rows the model catalog delivers for `query`, grouped by
/// provider, with the starred rows in a section of their own while the
/// query is empty. An empty query matches every row.
#[must_use]
pub fn model_sections(
    providers: Option<&[ModelProvider]>,
    fallback: Option<Vec<SessionConfigSelectOption>>,
    starred: &[String],
    query: &str,
) -> Vec<Section> {
    let needle = query.trim().to_lowercase();
    let mut groups: Vec<(String, Vec<ModelRow>)> = match providers {
        Some(providers) => providers
            .iter()
            .map(|provider| {
                let rows = provider
                    .models
                    .iter()
                    .map(|entry| ModelRow::of(&provider.name, entry))
                    .collect();
                (provider.name.clone(), rows)
            })
            .collect(),
        None => fallback_groups(fallback),
    };
    let mut sections = Vec::new();
    if needle.is_empty() {
        let rows: Vec<ModelRow> = groups
            .iter()
            .flat_map(|(_, rows)| rows)
            .filter(|row| starred.iter().any(|star| star == &row.id))
            .cloned()
            .collect();
        if !rows.is_empty() {
            sections.push(Section {
                label: "Starred".to_owned(),
                rows,
            });
        }
    }
    for (name, rows) in &mut groups {
        rows.retain(|row| row.matches(&needle, name));
        if !rows.is_empty() {
            sections.push(Section {
                label: name.clone(),
                rows: std::mem::take(rows),
            });
        }
    }
    sections
}

/// Adds configured external agents not already present in the model catalog.
pub fn append_external_agents(sections: &mut Vec<Section>, names: &[String], query: &str) {
    let needle = query.trim().to_lowercase();
    let rows: Vec<ModelRow> = names
        .iter()
        .filter_map(|name| {
            let id = format!("acp:{name}");
            if sections
                .iter()
                .any(|section| section.rows.iter().any(|row| row.id == id))
            {
                return None;
            }
            let row = ModelRow {
                id,
                name: name.clone(),
                provider: "External ACP agents".to_owned(),
                model: name.clone(),
                context: None,
                cost: None,
                images: false,
            };
            row.matches(&needle, "External ACP agents").then_some(row)
        })
        .collect();
    if !rows.is_empty() {
        sections.push(Section {
            label: "External ACP agents".to_owned(),
            rows,
        });
    }
}

/// The rows `sections` holds, flattened in section order.
#[must_use]
pub fn flat_rows(sections: &[Section]) -> Vec<ModelRow> {
    sections
        .iter()
        .flat_map(|section| section.rows.iter().cloned())
        .collect()
}

/// The provider groups the model option's own values deliver, used
/// while the catalog has not arrived. Each value's description names
/// its group; a value with none groups under its provider prefix.
#[must_use]
pub fn fallback_groups(
    options: Option<Vec<SessionConfigSelectOption>>,
) -> Vec<(String, Vec<ModelRow>)> {
    let mut groups: Vec<(String, Vec<ModelRow>)> = Vec::new();
    for value in options.unwrap_or_default() {
        let provider = value
            .value
            .find(['/', ':'])
            .map_or(value.value.as_str(), |at| &value.value[..at])
            .to_owned();
        let entry = ModelEntry {
            id: value.value.clone(),
            name: value.name.clone(),
            context: None,
            input_cost: None,
            output_cost: None,
            thinking: Vec::new(),
            images: false,
            released: None,
        };
        let label = value
            .description
            .clone()
            .unwrap_or_else(|| provider.clone());
        let row = ModelRow::of(&provider, &entry);
        match groups.iter_mut().find(|(name, _)| *name == label) {
            Some((_, rows)) => rows.push(row),
            None => groups.push((label, vec![row])),
        }
    }
    groups
}

/// A level's segment label: `Auto` for the engine's default, `XHigh`
/// for `xhigh`, else the option's name capitalized.
#[must_use]
pub fn level_label(value: &SessionConfigSelectOption) -> String {
    match value.value.as_str() {
        "default" => "Auto".to_owned(),
        "xhigh" => "XHigh".to_owned(),
        _ => {
            let mut chars = value.name.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    }
}

/// A context window as the picker's chip shows it: `200k`, `1.0M`.
#[must_use]
pub fn window_size(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// The two-letter provider tile a row leads with: the provider's
/// ascii letters, uppercased, so `openai` reads `OP`.
#[must_use]
pub fn provider_tile(provider: &str) -> String {
    provider
        .chars()
        .filter(char::is_ascii_alphabetic)
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

/// What a permission mode does to one kind of tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The call runs with no gate.
    Runs,
    /// The configured rules decide.
    Rules,
    /// The user is asked first.
    Asks,
    /// The call is refused.
    Refused,
}

impl Verdict {
    /// The word the policy grid prints for the verdict.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Runs => "runs",
            Self::Rules => "your rules",
            Self::Asks => "asks first",
            Self::Refused => "refused",
        }
    }

    /// The tone the word prints in.
    #[must_use]
    pub fn tone(self, palette: &Palette) -> egui::Color32 {
        match self {
            Self::Runs => palette.ok,
            Self::Rules | Self::Asks => palette.warn,
            Self::Refused => palette.danger,
        }
    }
}

/// The kinds of tool call the policy grid names, in its order.
pub const KINDS: [&str; 6] = [
    "Read and search",
    "Edits and writes",
    "Shell commands",
    "Web fetch",
    "Agents and swarms",
    "MCP tools",
];

/// What `mode` does to each of [`KINDS`], as the engine's permission
/// gate decides with no config rules: built-ins run, MCP tools ask,
/// and a mode overrides both. `None` for a mode the engine did not
/// define.
#[must_use]
pub fn policy(mode: &str) -> Option<[Verdict; 6]> {
    use Verdict::{Asks, Refused, Rules, Runs};
    Some(match mode {
        "default" => [Runs, Runs, Rules, Runs, Runs, Asks],
        "ask" => [Asks; 6],
        "allow" => [Runs; 6],
        "deny" => [Runs, Refused, Refused, Runs, Refused, Refused],
        _ => return None,
    })
}

/// The icon one permission mode's row leads with.
#[must_use]
pub fn mode_icon(mode: &str) -> crate::icons::Icon {
    if mode.contains("allow") {
        crate::icons::Icon::ShieldAlert
    } else if mode.contains("deny") || mode.contains("read") {
        crate::icons::Icon::Eye
    } else if mode.contains("ask") {
        crate::icons::Icon::Hand
    } else if mode.contains("plan") {
        crate::icons::Icon::PenLine
    } else {
        crate::icons::Icon::ShieldQuestionMark
    }
}

/// The tone one permission mode paints with: warn for the rules,
/// danger for allow, the accent for deny, plain for ask.
#[must_use]
pub fn mode_tone(mode: &str, palette: &Palette) -> Option<egui::Color32> {
    if mode.contains("allow") {
        Some(palette.danger)
    } else if mode.contains("deny") || mode.contains("read") {
        Some(palette.accent)
    } else if mode.contains("ask") || mode.contains("plan") {
        None
    } else {
        Some(palette.warn)
    }
}

// The mode vocabulary the crate already owns: the pickers read it
// rather than hold a second copy of each rule.
use crate::modes::PLAN_MODE;

/// The goal text option of a session, when one is set.
#[must_use]
pub fn session_goal(session: Option<&kage_client::Session>) -> Option<String> {
    session?
        .config_options
        .iter()
        .find(|option| option.id == "goal")
        .map(|option| option.current_value.clone())
        .filter(|goal| !goal.is_empty())
}

/// The mode rows the picker lists for `query`: every advertised
/// permission mode but plan mode, narrowed by name and description.
#[must_use]
pub fn mode_rows(
    options: &[SessionConfigSelectOption],
    query: &str,
) -> Vec<SessionConfigSelectOption> {
    let needle = picker_query(query);
    options
        .iter()
        .filter(|mode| mode.value != PLAN_MODE)
        .filter(|mode| {
            needle.is_empty()
                || format!(
                    "{} {}",
                    mode.name,
                    mode.description.as_deref().unwrap_or_default()
                )
                .to_lowercase()
                .contains(&needle)
        })
        .cloned()
        .collect()
}

/// The mode options of `session`, for the tests that build rows from one.
#[cfg(test)]
fn session_mode_options(session: &kage_client::Session) -> Vec<SessionConfigSelectOption> {
    crate::modes::select_option(session, "mode")
        .map(|option| option.options.clone())
        .unwrap_or_default()
}

/// The thinking rows the picker lists for `query`: the thinking
/// option's values, narrowed by name and description.
#[must_use]
pub fn thinking_rows(
    session: &kage_client::Session,
    query: &str,
) -> Vec<SessionConfigSelectOption> {
    let needle = picker_query(query);
    crate::modes::select_option(session, "thinking")
        .map(|option| option.options.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|level| {
            needle.is_empty()
                || format!(
                    "{} {}",
                    level.name,
                    level.description.as_deref().unwrap_or_default()
                )
                .to_lowercase()
                .contains(&needle)
        })
        .collect()
}

/// The thinking levels the model `id` accepts, as the model option's
/// catalog entry lists them. A model with no entry takes every level.
#[must_use]
pub fn model_levels(store: &Store, id: &str) -> Option<Vec<String>> {
    let providers = store.models()?;
    let entry = providers
        .iter()
        .flat_map(|provider| provider.models.iter())
        .find(|entry| entry.id == id)?;
    Some(entry.thinking.clone())
}

/// The index one step from `current` over `total` rows, clamped at the
/// ends. No rows stays put.
#[must_use]
pub fn step_highlight(current: usize, total: usize, delta: isize) -> usize {
    if total == 0 {
        return 0;
    }
    current.saturating_add_signed(delta).min(total - 1)
}

/// A picker's search field value, narrowed the way the rows narrow:
/// trimmed and lowercased, empty for a blank query.
#[must_use]
pub fn picker_query(value: &str) -> String {
    value.trim().to_lowercase()
}

/// The tokens the usage numbers read as: `12480` becomes `12.5k`.
#[must_use]
pub fn tokens(used: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    if used >= 1_000_000 {
        format!("{:.1}M", used as f64 / 1_000_000.0)
    } else if used >= 1_000 {
        format!("{:.1}k", used as f64 / 1_000.0)
    } else {
        used.to_string()
    }
}

/// The context fill fraction, zero when the window size is unknown.
#[must_use]
pub fn fill_fraction(used: u64, size: u64) -> f64 {
    if size == 0 {
        0.0
    } else {
        used as f64 / size as f64
    }
}

/// The context fill as a whole percent, rounded.
#[must_use]
pub fn fill_percent(used: u64, size: u64) -> i64 {
    (fill_fraction(used, size) * 100.0).round() as i64
}

/// The compaction threshold the engine applies on its own.
pub const COMPACT_AT: f64 = 0.8;

/// Whether the context fill reached the compaction threshold.
#[must_use]
pub fn compacts_soon(used: u64, size: u64) -> bool {
    fill_fraction(used, size) >= COMPACT_AT
}

/// The cells the context gauge draws, and which of them are lit at
/// `fill`: twenty cells, the sixteenth marked as the compaction point.
#[must_use]
pub fn gauge_cells(fill: f64) -> Vec<(usize, bool)> {
    (0..20)
        .map(|cell| {
            let mid = (cell as f64 + 0.5) / 20.0;
            (cell, mid <= fill)
        })
        .collect()
}

/// The cell the gauge marks as the compaction point.
pub const COMPACT_CELL: usize = 16;

#[cfg(test)]
mod tests {
    use super::{
        COMPACT_AT, COMPACT_CELL, KINDS, ModelRow, Section, compacts_soon, fallback_groups,
        fill_percent, flat_rows, gauge_cells, level_label, mode_rows, model_levels, model_sections,
        picker_query, policy, provider_tile, session_mode_options, step_highlight, thinking_rows,
        tokens, window_size,
    };
    use crate::modes::{next_mode_value, permission_values};
    use kage_client::Session;
    use kage_client::wire::{ModelEntry, ModelProvider, SessionConfigSelectOption};
    use serde_json::json;

    #[test]
    fn configured_external_agents_are_searchable_without_catalog_duplicates() {
        let names = vec!["claude".to_owned(), "reviewer".to_owned()];
        let mut sections = Vec::new();
        super::append_external_agents(&mut sections, &names, "CLAUDE");
        assert_eq!(flat_rows(&sections)[0].id, "acp:claude");
        super::append_external_agents(&mut sections, &names, "");
        let ids: Vec<_> = flat_rows(&sections).into_iter().map(|row| row.id).collect();
        assert_eq!(ids, ["acp:claude", "acp:reviewer"]);
    }

    fn entry(id: &str, name: &str, context: Option<u64>) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            name: name.to_owned(),
            context,
            input_cost: None,
            output_cost: None,
            thinking: Vec::new(),
            images: false,
            released: None,
        }
    }

    fn value(value: &str, name: &str) -> SessionConfigSelectOption {
        SessionConfigSelectOption {
            value: value.to_owned(),
            name: name.to_owned(),
            description: None,
        }
    }

    fn catalog() -> Vec<ModelProvider> {
        vec![
            ModelProvider {
                id: "anthropic".to_owned(),
                name: "Anthropic".to_owned(),
                models: vec![
                    entry(
                        "anthropic/claude-sonnet-4",
                        "Claude Sonnet 4",
                        Some(200_000),
                    ),
                    entry("anthropic/claude-opus-4", "Claude Opus 4", Some(200_000)),
                ],
            },
            ModelProvider {
                id: "openai".to_owned(),
                name: "OpenAI".to_owned(),
                models: vec![entry("openai/gpt-5", "GPT-5", Some(400_000))],
            },
        ]
    }

    #[test]
    fn the_model_row_splits_the_provider_from_the_model() {
        let row = ModelRow::of(
            "Anthropic",
            &entry(
                "anthropic/claude-sonnet-4",
                "Claude Sonnet 4",
                Some(200_000),
            ),
        );
        assert_eq!(row.provider, "Anthropic");
        assert_eq!(row.model, "claude-sonnet-4");
        assert_eq!(row.context, Some(200_000));
        assert!(row.matches("claude", "Anthropic"));
        assert!(row.matches("anthropic", "Anthropic"));
        assert!(!row.matches("gpt", "Anthropic"));
        assert!(
            row.matches("", "Anthropic"),
            "an empty query matches every row"
        );
    }

    #[test]
    fn the_starred_rows_lead_while_the_query_is_empty() {
        let sections = model_sections(Some(&catalog()), None, &["openai/gpt-5".to_owned()], "");
        assert_eq!(sections[0].label, "Starred");
        assert_eq!(sections[0].rows.len(), 1);
        assert_eq!(sections[0].rows[0].id, "openai/gpt-5");
        assert_eq!(sections.len(), 3, "starred, Anthropic, OpenAI");
        let narrowed = model_sections(Some(&catalog()), None, &["openai/gpt-5".to_owned()], "opus");
        assert_eq!(narrowed.len(), 1, "a query drops the starred section");
        assert_eq!(narrowed[0].label, "Anthropic");
        assert_eq!(flat_rows(&narrowed).len(), 1);
    }

    #[test]
    fn the_fallback_groups_group_by_the_description() {
        let groups = fallback_groups(Some(vec![
            value("openai/gpt-5", "GPT-5"),
            value("openai/gpt-5-mini", "GPT-5 mini"),
            value("anthropic/claude", "Claude"),
        ]));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "openai", "no description groups by the prefix");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "anthropic");
        assert!(fallback_groups(None).is_empty(), "no values, no groups");
    }

    #[test]
    fn context_windows_read_like_the_design() {
        assert_eq!(window_size(200_000), "200k");
        assert_eq!(window_size(1_000_000), "1.0M");
        assert_eq!(window_size(512), "512");
    }

    #[test]
    fn levels_read_as_the_segment_labels() {
        assert_eq!(level_label(&value("default", "auto")), "Auto");
        assert_eq!(level_label(&value("xhigh", "xhigh")), "XHigh");
        assert_eq!(level_label(&value("max", "max")), "Max");
        assert_eq!(level_label(&value("low", "low")), "Low");
    }

    #[test]
    fn the_provider_tile_takes_two_letters() {
        assert_eq!(provider_tile("openai"), "OP");
        assert_eq!(provider_tile("z.ai"), "ZA");
        assert_eq!(provider_tile("x"), "X");
    }

    #[test]
    fn every_engine_mode_has_a_policy() {
        for mode in ["default", "ask", "allow", "deny"] {
            assert!(policy(mode).is_some(), "{mode}");
        }
        assert!(policy("plan").is_none());
        assert_eq!(KINDS.len(), 6);
        assert_eq!(policy("deny").expect("deny")[1].word(), "refused");
    }

    #[test]
    fn the_mode_rows_drop_plan_mode_and_narrow_on_the_query() {
        let mut session = Session::default();
        session
            .config_options
            .push(kage_client::wire::SessionConfigOption {
                id: "mode".to_owned(),
                name: "Mode".to_owned(),
                description: None,
                category: None,
                kind: kage_client::wire::SessionConfigKind::Select,
                current_value: "default".to_owned(),
                options: vec![
                    value("default", "Default"),
                    value("plan", "Plan"),
                    value("ask", "Ask"),
                ],
            });
        let rows = mode_rows(&session_mode_options(&session), "");
        assert_eq!(rows.len(), 2, "plan mode has its own chip");
        assert_eq!(rows[0].value, "default");
        let options = session_mode_options(&session);
        assert_eq!(mode_rows(&options, "as").len(), 1);
        assert_eq!(mode_rows(&options, "plan").len(), 0);
        assert_eq!(permission_values(&session).len(), 2);
        assert_eq!(next_mode_value(&session, "default").as_deref(), Some("ask"));
        assert_eq!(next_mode_value(&session, "ask").as_deref(), Some("default"));
    }

    #[test]
    fn the_thinking_rows_narrow_on_the_query() {
        let mut session = Session::default();
        session
            .config_options
            .push(kage_client::wire::SessionConfigOption {
                id: "thinking".to_owned(),
                name: "Thinking".to_owned(),
                description: None,
                category: None,
                kind: kage_client::wire::SessionConfigKind::Select,
                current_value: "default".to_owned(),
                options: vec![value("default", "Auto"), value("max", "max")],
            });
        assert_eq!(thinking_rows(&session, "").len(), 2);
        assert_eq!(thinking_rows(&session, "max").len(), 1);
        assert!(thinking_rows(&session, "nope").is_empty());
    }

    #[test]
    fn the_model_levels_come_from_the_catalog_entry() {
        let mut store = crate::store::Store::new("/w", false);
        assert_eq!(model_levels(&store, "openai/gpt-5"), None, "no catalog yet");
        store.ask_models();
        let request = store
            .take_outgoing()
            .into_iter()
            .find_map(|frame| match frame {
                kage_client::Frame::Request { id, .. } => Some(id),
                _ => None,
            })
            .expect("the models ask rides the wire");
        store.absorb(kage_client::Frame::Success {
            id: request,
            result: json!({"providers": [{
                "id": "openai",
                "name": "OpenAI",
                "models": [{"id": "openai/gpt-5", "name": "GPT-5", "thinking": ["low", "high"]}],
            }]}),
        });
        assert_eq!(
            model_levels(&store, "openai/gpt-5"),
            Some(vec!["low".to_owned(), "high".to_owned()])
        );
        assert_eq!(model_levels(&store, "nope"), None);
    }

    #[test]
    fn the_highlight_steps_and_clamps() {
        assert_eq!(step_highlight(0, 5, 1), 1);
        assert_eq!(step_highlight(4, 5, 1), 4, "the last row stays");
        assert_eq!(step_highlight(2, 5, -1), 1);
        assert_eq!(step_highlight(0, 5, -1), 0, "the first row stays");
        assert_eq!(step_highlight(3, 0, 1), 0, "no rows, no highlight");
    }

    #[test]
    fn the_query_trims_and_lowercases() {
        assert_eq!(picker_query("  Opus "), "opus");
        assert_eq!(picker_query("   "), "");
    }

    #[test]
    fn the_tokens_read_like_the_gauge() {
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(12_480), "12.5k");
        assert_eq!(tokens(2_000_000), "2.0M");
    }

    #[test]
    fn the_gauge_fills_and_marks_the_compaction_point() {
        assert_eq!(fill_percent(0, 200_000), 0);
        assert_eq!(fill_percent(100_000, 200_000), 50);
        assert_eq!(fill_percent(1, 0), 0, "no window size, no fill");
        assert!(!compacts_soon(100_000, 200_000));
        assert!(compacts_soon(160_000, 200_000));
        assert!((COMPACT_AT - 0.8).abs() < f64::EPSILON);
        let cells = gauge_cells(0.5);
        assert_eq!(cells.len(), 20);
        assert!(cells[9].1, "the tenth cell is lit at half fill");
        assert!(!cells[10].1);
        assert_eq!(COMPACT_CELL, 16);
        let _ = Section {
            label: "x".to_owned(),
            rows: Vec::new(),
        };
    }
}
