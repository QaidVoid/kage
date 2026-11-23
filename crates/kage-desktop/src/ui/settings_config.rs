//! The settings pages read from the engine's `_kage/config/get`
//! snapshot: model providers, MCP servers, permissions and plugins.
//!
//! The pages say only what the snapshot and the live session carry. A
//! fact the snapshot leaves out reads unknown rather than guessed. Edits
//! go to the engine through `_kage/config/set`, and the snapshot it
//! answers with redraws the page.

use crate::icons::PaintSnapped as _;
use std::collections::BTreeMap;

use kage_client::wire::{McpServerStatus, SessionConfigOption};
use serde::Deserialize;

use crate::store::Store;
use crate::theme::{
    FAMILY_BODY_BOLD, FAMILY_BODY_MEDIUM, FAMILY_BODY_REGULAR, FAMILY_BODY_SEMIBOLD,
    FAMILY_MONO_REGULAR, FS_SM, FS_XS, Palette, R_LG, R_MD, family,
};
use crate::ui::provider_form::Target;
use crate::ui::settings::{SettingsEvent, boxed, note};

/// The parts of the snapshot the pages read. Every field defaults, so a
/// section the engine leaves out reads as empty.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    providers: Providers,
    mcp: Mcp,
    permissions: Permissions,
    plugins: Plugins,
    #[serde(rename = "installedPlugins")]
    installed_plugins: Vec<InstalledPlugin>,
    #[serde(rename = "providerKeys")]
    provider_keys: BTreeMap<String, KeyState>,
    acp: Acp,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Acp {
    agents: BTreeMap<String, AcpAgent>,
}

/// One `[acp.agents.<name>]` entry as the snapshot shows it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AcpAgent {
    /// The executable that starts the agent.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
}

/// Where one provider finds its key, as the snapshot says.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct KeyState {
    /// The environment variable the key is read from; empty when the
    /// provider needs none.
    pub env: String,
    /// `env`, `auth`, `missing` or `unneeded`.
    pub source: String,
    /// A display name the engine knows for the id, when it has one.
    pub name: Option<String>,
}

impl Snapshot {
    /// Reads a snapshot answer; one that does not parse is empty.
    #[must_use]
    pub fn parse(value: &serde_json::Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }

    /// Where provider `id` finds its key, when the snapshot says.
    #[must_use]
    pub fn key_state(&self, id: &str) -> Option<&KeyState> {
        self.provider_keys.get(id)
    }

    /// The providers kage registers itself, by id: the ones a key makes
    /// usable without defining them.
    pub fn registered(&self) -> impl Iterator<Item = &str> {
        self.provider_keys
            .keys()
            .map(String::as_str)
            .filter(|id| !self.providers.custom.contains_key(*id))
    }

    /// The name a row or chip shows for provider `id`: what the
    /// engine's snapshot calls it, else the id itself.
    #[must_use]
    pub fn provider_name(&self, id: &str) -> String {
        self.key_state(id)
            .and_then(|key| key.name.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    /// The ACP agent configured as `name`.
    #[must_use]
    pub fn acp_agent(&self, name: &str) -> Option<&AcpAgent> {
        self.acp.agents.get(name)
    }

    /// The MCP server configured as `name`.
    #[must_use]
    pub fn mcp_server(&self, name: &str) -> Option<&McpServer> {
        self.mcp.servers.get(name)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Providers {
    custom: BTreeMap<String, CustomProvider>,
    #[serde(flatten)]
    overrides: BTreeMap<String, ProviderOverride>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CustomProvider {
    kind: Option<String>,
    base_url: String,
    display_name: Option<String>,
    api_key_env: Option<String>,
    headers: BTreeMap<String, String>,
    models: Vec<CustomModel>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CustomModel {
    id: String,
    name: String,
    context: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProviderOverride {
    base_url: Option<String>,
    api_key_env: Option<String>,
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Mcp {
    servers: BTreeMap<String, McpServer>,
    allow_sampling: bool,
}

/// One `[mcp.servers.<name>]` entry as the snapshot shows it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct McpServer {
    /// The command that starts a stdio server.
    pub command: Option<String>,
    /// The stdio server's arguments.
    pub args: Vec<String>,
    /// The stdio server's environment.
    pub env: BTreeMap<String, String>,
    /// The http server's URL.
    pub url: Option<String>,
    /// The http server's headers.
    pub headers: BTreeMap<String, String>,
    /// Whether the server is configured but not started.
    pub disabled: bool,
    /// The server's OAuth settings, when it logs in.
    pub oauth: Option<serde_json::Value>,
    /// The tools the server's config disables.
    pub disabled_tools: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Permissions {
    confine_paths: bool,
    tools: BTreeMap<String, ToolRules>,
    mcp: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ToolRules {
    default: String,
    allow: Vec<String>,
    deny: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Plugins {
    dir: Option<String>,
    disabled: Vec<String>,
    enabled: Vec<String>,
    capabilities: BTreeMap<String, Vec<String>>,
    config: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct InstalledPlugin {
    name: String,
    enabled: bool,
    version: Option<String>,
    from_registry: bool,
}

/// One provider row: a configured one, or one the session's model
/// option names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    /// The provider id.
    pub id: String,
    /// The name the picker shows.
    pub label: String,
    /// The wire protocol, when the snapshot names it.
    pub protocol: Option<String>,
    /// The endpoint, when the snapshot names it.
    pub base_url: Option<String>,
    /// Where the key comes from, when the snapshot says.
    pub key_env: Option<String>,
    /// The header names it sends.
    pub headers: Vec<String>,
    /// The models it serves, as the snapshot or the session lists them.
    pub models: Vec<String>,
    /// Whether the config file defines it.
    pub custom: bool,
}

/// The provider rows: custom providers and overrides from the snapshot,
/// joined with every provider the session's model option offers.
#[must_use]
pub fn provider_rows(snapshot: &Snapshot, model: Option<&SessionConfigOption>) -> Vec<ProviderRow> {
    let mut rows: Vec<ProviderRow> = Vec::new();
    for (id, custom) in &snapshot.providers.custom {
        rows.push(ProviderRow {
            id: id.clone(),
            label: custom.display_name.clone().unwrap_or_else(|| id.clone()),
            protocol: Some(custom.kind.clone().unwrap_or_else(|| "openai".to_owned())),
            base_url: Some(custom.base_url.clone()),
            key_env: custom.api_key_env.clone(),
            headers: custom.headers.keys().cloned().collect(),
            models: custom
                .models
                .iter()
                .map(|m| match m.context {
                    Some(context) => format!("{} \u{b7} {}k", m.name, context / 1000),
                    None => m.name.clone(),
                })
                .collect(),
            custom: true,
        });
    }
    let offered = model
        .map(|option| option.options.as_slice())
        .unwrap_or_default();
    for choice in offered {
        // `provider/model`, or the older `provider:model`.
        let Some(provider) = choice.value.find(['/', ':']).map(|at| &choice.value[..at]) else {
            continue;
        };
        let name = choice.name.clone();
        match rows.iter_mut().find(|row| row.id == provider) {
            Some(row) if row.custom => {}
            Some(row) => row.models.push(name),
            None => rows.push(ProviderRow {
                id: provider.to_owned(),
                label: snapshot.provider_name(provider),
                protocol: None,
                base_url: None,
                key_env: None,
                headers: Vec::new(),
                models: vec![name],
                custom: false,
            }),
        }
    }
    for (id, over) in &snapshot.providers.overrides {
        if id == "custom" {
            continue;
        }
        let row = match rows.iter_mut().find(|row| &row.id == id) {
            Some(row) => row,
            None => {
                rows.push(ProviderRow {
                    id: id.clone(),
                    label: snapshot.provider_name(id),
                    protocol: None,
                    base_url: None,
                    key_env: None,
                    headers: Vec::new(),
                    models: Vec::new(),
                    custom: false,
                });
                rows.last_mut().expect("just pushed")
            }
        };
        if over.base_url.is_some() {
            row.base_url.clone_from(&over.base_url);
        }
        if over.api_key_env.is_some() {
            row.key_env.clone_from(&over.api_key_env);
        }
        row.headers.extend(over.headers.keys().cloned());
    }
    // A provider kage registers is usable once its key is somewhere.
    for (id, key) in &snapshot.provider_keys {
        if matches!(key.source.as_str(), "env" | "auth") && !rows.iter().any(|row| &row.id == id) {
            rows.push(ProviderRow {
                id: id.clone(),
                label: id.clone(),
                protocol: None,
                base_url: None,
                key_env: Some(key.env.clone()),
                headers: Vec::new(),
                models: Vec::new(),
                custom: false,
            });
        }
    }
    rows
}

/// What is being added to the permission rules while the shared text
/// field shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleAdd {
    /// A rule for a tool, named in the field.
    Tool,
    /// A glob on tool `tool`'s allow list, or its deny list.
    Glob {
        /// The tool the rule is for.
        tool: String,
        /// Whether the glob refuses instead of allowing.
        deny: bool,
    },
}

/// The actions a rule's default may take.
const ACTIONS: [&str; 3] = ["allow", "ask", "deny"];

/// The config path and value an added `text` writes, read against the
/// snapshot `config`: a new tool rule that asks, or the tool's rule with
/// one more glob. `None` when `text` is blank.
pub fn rule_added(
    config: &serde_json::Value,
    add: &RuleAdd,
    text: &str,
) -> Option<(Vec<String>, serde_json::Value)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let path = |tool: &str| {
        vec![
            "permissions".to_owned(),
            "tools".to_owned(),
            tool.to_owned(),
        ]
    };
    match add {
        RuleAdd::Tool => Some((path(text), serde_json::json!({ "default": "ask" }))),
        RuleAdd::Glob { tool, deny } => {
            let snapshot = Snapshot::parse(config);
            let mut rule = snapshot
                .permissions
                .tools
                .get(tool)
                .cloned()
                .unwrap_or_default();
            let list = if *deny {
                &mut rule.deny
            } else {
                &mut rule.allow
            };
            if !list.iter().any(|glob| glob == text) {
                list.push(text.to_owned());
            }
            Some((path(tool), rule_json(&rule)))
        }
    }
}

/// `rule` as `[permissions.tools.<tool>]` holds it.
fn rule_json(rule: &ToolRules) -> serde_json::Value {
    serde_json::json!({
        "default": if rule.default.is_empty() { "allow" } else { rule.default.as_str() },
        "allow": rule.allow,
        "deny": rule.deny,
    })
}

/// The capabilities `[plugins.capabilities]` may grant, as kage-plugin
/// names them.
const CAPABILITIES: [&str; 8] = [
    "session_write",
    "exec",
    "env",
    "net",
    "crypto",
    "context",
    "provider",
    "fs_write",
];

/// One plugin as the Plugins page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    /// The plugin's name, its file stem.
    pub name: String,
    /// The capabilities config.toml grants it.
    pub caps: Vec<String>,
    /// Whether it loads, whether it has grants and whether it has
    /// settings.
    pub detail: String,
    /// Whether the allowlist lets it load; `None` when no file is
    /// installed under the name.
    pub enabled: Option<bool>,
}

/// Every plugin installed or named in the config, by name.
fn plugin_rows(snapshot: &Snapshot) -> Vec<PluginRow> {
    let plugins = &snapshot.plugins;
    let mut names: Vec<&String> = snapshot
        .installed_plugins
        .iter()
        .map(|plugin| &plugin.name)
        .chain(plugins.capabilities.keys())
        .chain(plugins.config.keys())
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            let caps = plugins.capabilities.get(name).cloned().unwrap_or_default();
            let installed = snapshot
                .installed_plugins
                .iter()
                .find(|plugin| &plugin.name == name);
            let enabled = installed.map(|plugin| plugin.enabled);
            let version = installed.and_then(|plugin| plugin.version.clone());
            let source = installed
                .is_some_and(|plugin| plugin.from_registry)
                .then_some("from kura");
            let state = match enabled {
                Some(true) => "loads",
                Some(false) => "disabled",
                None => "not installed",
            };
            let detail = [
                Some(state),
                version.as_deref(),
                source,
                (!caps.is_empty()).then_some("capabilities granted in config.toml"),
                plugins.config.contains_key(name).then_some("has settings"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
            PluginRow {
                name: name.clone(),
                caps,
                detail,
                enabled,
            }
        })
        .collect()
}

/// The `[plugins] disabled` list after turning plugin `name` on or off.
/// It starts from every plugin that does not load now, so a config still
/// on the deprecated `enabled` allowlist keeps loading the same plugins
/// once that list is dropped.
fn toggled_disabled(snapshot: &Snapshot, name: &str, on: bool) -> Vec<String> {
    let mut list: Vec<String> = snapshot
        .installed_plugins
        .iter()
        .filter(|plugin| !plugin.enabled)
        .map(|plugin| plugin.name.clone())
        .chain(snapshot.plugins.disabled.iter().cloned())
        .filter(|entry| entry != name)
        .collect();
    if !on {
        list.push(name.to_owned());
    }
    list.sort();
    list.dedup();
    list
}

/// What the config pages edit in place: the provider chooser, the
/// rule being added, and the plugin install field and its outcome.
#[derive(Debug, Default)]
pub struct PageState {
    /// Whether the Providers page shows the providers to add from.
    pub choosing: bool,
    /// What the rule field adds, while it shows.
    pub adding: Option<RuleAdd>,
    /// The text the rule field holds.
    pub rule_text: String,
    /// The plugin install field's text, while it shows.
    pub plugin_input: Option<String>,
    /// The install in flight, by write id.
    pub plugin_install: Option<u64>,
    /// The plugin whose grants show.
    pub plugin_open: Option<String>,
    /// The registry read the kura list shows, by request id.
    pub registry: Option<u64>,
    /// The text the kura list is filtered by.
    pub registry_filter: String,
    /// The registry package whose details show.
    pub registry_open: Option<String>,
    /// The registry install in flight or last done: the package and the
    /// request id.
    pub registry_install: Option<(String, u64)>,
    /// The General page's theme menu that shows: `Some(true)` for the
    /// dark desktop's, `Some(false)` for the light one's.
    pub pick_open: Option<bool>,
}

impl PageState {
    /// Whether something on the page answers Escape itself: an open
    /// menu, the rule field or the plugin install field. The dialog
    /// waits for it before Escape closes the dialog.
    #[must_use]
    pub fn holds_escape(&self) -> bool {
        self.pick_open.is_some() || self.adding.is_some() || self.plugin_input.is_some()
    }
}

/// The tone a badge or chip paints in: its text, fill and border.
#[derive(Debug, Clone, Copy)]
struct Tone {
    fg: egui::Color32,
    bg: egui::Color32,
    line: egui::Color32,
}

impl Tone {
    /// The neutral badge: muted text over the soft fill.
    fn plain(palette: &Palette) -> Self {
        Self {
            fg: palette.muted,
            bg: palette.fill,
            line: palette.line,
        }
    }

    fn ok(palette: &Palette) -> Self {
        Self {
            fg: palette.ok,
            bg: palette.ok_soft,
            line: palette.ok_bd,
        }
    }

    fn warn(palette: &Palette) -> Self {
        Self {
            fg: palette.warn,
            bg: palette.warn_soft,
            line: palette.warn_bd,
        }
    }

    fn danger(palette: &Palette) -> Self {
        Self {
            fg: palette.danger,
            bg: palette.danger_soft,
            line: palette.danger_bd,
        }
    }
}

/// One piece of a list row's name line.
enum Part {
    /// Text in the body face.
    Text(String),
    /// Text in the mono face, as names and paths read.
    Mono(String),
    /// A rounded badge in its tone.
    Badge(String, Tone),
}

/// What a list row leads with.
enum Lead {
    /// A provider's logo on its white tile, by id and endpoint, or the
    /// bot glyph while it loads or when there is none.
    Logo(String, Option<String>),
    /// The two-letter tile of a provider without a logo.
    Initials(String),
    /// A faint icon.
    Icon(crate::icons::Icon),
    /// A status dot.
    Dot(egui::Color32),
}

/// What a list row trails with.
enum Trail {
    /// Nothing.
    None,
    /// A badge.
    Badge(String, Tone),
    /// Quiet text, cut at 240px.
    Faint(String),
    /// Mono text in the ink.
    Mono(String),
    /// A switch, dimmed when it cannot flip.
    Switch { on: bool, live: bool },
}

/// What a click on a list row hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Nothing,
    Row,
    Switch,
}

/// Paints a badge with its left edge at `at.x`, centered on `at.y`, and
/// gives back where it landed.
fn paint_badge(ui: &egui::Ui, at: egui::Pos2, text: &str, mono: bool, tone: Tone) -> egui::Rect {
    let face = if mono {
        FAMILY_MONO_REGULAR
    } else {
        FAMILY_BODY_MEDIUM
    };
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(10.5, family(face)),
        tone.fg,
    );
    // The reference's badge: 7px and a 1px border either side, a pixel
    // and the border above and below one line.
    let line = crate::theme::line_h(10.5);
    let height = line + 4.0;
    let rect = egui::Rect::from_min_size(
        egui::pos2(at.x, crate::theme::snap(at.y - height / 2.0)),
        egui::vec2(galley.size().x.ceil() + 16.0, height),
    );
    let radius = egui::CornerRadius::same((height / 2.0) as u8);
    ui.painter().rect_filled(rect, radius, tone.bg);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, tone.line),
        egui::StrokeKind::Inside,
    );
    let y = crate::theme::line_y(rect.top() + 2.0, line, &galley);
    ui.painter()
        .galley(egui::pos2(rect.left() + 8.0, y), galley, tone.fg);
    rect
}

/// The width a badge of `text` takes.
fn badge_width(ui: &egui::Ui, text: &str, mono: bool) -> f32 {
    let face = if mono {
        FAMILY_MONO_REGULAR
    } else {
        FAMILY_BODY_MEDIUM
    };
    ui.painter()
        .layout_no_wrap(
            text.to_owned(),
            egui::FontId::new(10.5, family(face)),
            egui::Color32::WHITE,
        )
        .size()
        .x
        .ceil()
        + 16.0
}

/// One list row: its lead, a name line with badges, an optional detail
/// line, and its trailing fact. Clickable rows hover with the soft
/// fill; the switch answers on its own.
#[allow(clippy::too_many_arguments)]
fn list_row(
    ui: &mut egui::Ui,
    id: &str,
    lead: &Lead,
    name: &[Part],
    detail: Option<&str>,
    trail: &Trail,
    clickable: bool,
    palette: &Palette,
) -> Hit {
    // The name line is as tall as its tallest part: a 13px name, a
    // 12px mono name, or a badge.
    let name_h = name
        .iter()
        .map(|part| match part {
            Part::Text(_) => crate::theme::line_h(FS_SM),
            Part::Mono(_) => crate::theme::line_h(FS_XS),
            Part::Badge(..) => crate::theme::line_h(10.5) + 4.0,
        })
        .fold(0.0, f32::max);
    let detail_h = if detail.is_some() {
        crate::theme::line_h(FS_XS)
    } else {
        0.0
    };
    // The reference's row: a 1px top border, 10px above and below, and
    // the name over its detail centered beside the 28px lead.
    let column_h = name_h
        + if detail.is_some() {
            2.0 + detail_h
        } else {
            0.0
        };
    let lead_h = match lead {
        Lead::Logo(..) | Lead::Initials(_) => 28.0,
        Lead::Icon(_) => 14.0,
        Lead::Dot(_) => 8.0,
    };
    let content_h = column_h.max(lead_h);
    let height = 1.0 + 10.0 + content_h + 10.0;
    let sense = if clickable {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), sense);
    let label: String = name
        .iter()
        .map(|part| match part {
            Part::Text(text) | Part::Mono(text) => text.as_str(),
            Part::Badge(..) => "",
        })
        .collect::<Vec<_>>()
        .join(" ");
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            if clickable {
                egui::WidgetType::Button
            } else {
                egui::WidgetType::Label
            },
            true,
            label.trim(),
        )
    });
    if clickable && response.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::ZERO, palette.fill_hover);
    }
    crate::ui::settings::row_rule(ui, rect, palette);
    let content_top = rect.top() + 11.0;
    let mid = content_top + content_h / 2.0;
    let mut x = rect.left() + 16.0;
    match lead {
        Lead::Logo(id, site) => {
            x = paint_logo(ui, egui::pos2(x, mid), id, site.as_deref(), 22.0, palette) + 12.0;
        }
        Lead::Initials(text) => {
            let tile = egui::Rect::from_min_size(egui::pos2(x, mid - 14.0), egui::vec2(28.0, 28.0));
            ui.painter()
                .rect_filled(tile, egui::CornerRadius::same(8), palette.fill);
            ui.painter().text(
                tile.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::new(10.5, family(FAMILY_BODY_BOLD)),
                palette.muted,
            );
            x = tile.right() + 12.0;
        }
        Lead::Icon(icon) => {
            let at = egui::Rect::from_min_size(egui::pos2(x, mid - 7.0), egui::vec2(14.0, 14.0));
            crate::icons::themed_image(*icon, 14.0, palette.faint).paint_snapped(ui, at);
            x = at.right() + 12.0;
        }
        Lead::Dot(color) => {
            ui.painter()
                .circle_filled(egui::pos2(x + 4.0, mid), 4.0, *color);
            x += 8.0 + 12.0;
        }
    }
    let mut right = rect.right() - 16.0;
    let mut switch_rect = None;
    match trail {
        Trail::None => {}
        Trail::Badge(text, tone) => {
            let width = badge_width(ui, text, false);
            paint_badge(ui, egui::pos2(right - width, mid), text, false, *tone);
            right -= width + 12.0;
        }
        Trail::Faint(text) | Trail::Mono(text) => {
            let (face, size, color) = match trail {
                Trail::Mono(_) => (FAMILY_MONO_REGULAR, FS_XS, palette.ink),
                _ => (FAMILY_BODY_REGULAR, FS_XS, palette.faint),
            };
            let galley = crate::ui::transcript::head::one_line(
                ui,
                text.clone(),
                egui::TextFormat::simple(egui::FontId::new(size, family(face)), color),
                240.0,
            );
            let left = right - galley.size().x.ceil();
            let y = crate::theme::line_y(content_top, content_h, &galley);
            ui.painter().galley(egui::pos2(left, y), galley, color);
            right = left - 12.0;
        }
        Trail::Switch { on, live } => {
            let track = egui::Rect::from_min_size(
                egui::pos2(right - 36.0, mid - 10.0),
                egui::vec2(36.0, 20.0),
            );
            let opacity = if *live { 1.0 } else { 0.45 };
            let fill = if *on {
                palette.accent
            } else {
                palette.fill_hover
            };
            ui.painter().rect_filled(
                track,
                egui::CornerRadius::same(10),
                fill.gamma_multiply(opacity),
            );
            let knob = egui::pos2(track.left() + if *on { 26.0 } else { 10.0 }, mid);
            ui.painter()
                .circle_filled(knob, 8.0, egui::Color32::WHITE.gamma_multiply(opacity));
            switch_rect = Some(track);
            right = track.left() - 12.0;
        }
    }
    let top = crate::theme::snap(content_top + (content_h - column_h) / 2.0);
    let mut nx = x;
    for part in name {
        match part {
            Part::Text(text) | Part::Mono(text) => {
                let (face, size) = match part {
                    Part::Mono(_) => (FAMILY_MONO_REGULAR, FS_XS),
                    _ => (FAMILY_BODY_REGULAR, FS_SM),
                };
                let galley = crate::ui::transcript::head::one_line(
                    ui,
                    text.clone(),
                    egui::TextFormat::simple(egui::FontId::new(size, family(face)), palette.ink),
                    (right - nx).max(0.0),
                );
                let width = galley.size().x.ceil();
                let y = crate::theme::line_y(top, name_h, &galley);
                ui.painter().galley(egui::pos2(nx, y), galley, palette.ink);
                nx += width + 6.0;
            }
            Part::Badge(text, tone) => {
                if nx + badge_width(ui, text, false) > right {
                    continue;
                }
                nx = paint_badge(ui, egui::pos2(nx, top + name_h / 2.0), text, false, *tone)
                    .right()
                    + 6.0;
            }
        }
    }
    if let Some(detail) = detail {
        let galley = crate::ui::transcript::head::one_line(
            ui,
            detail.to_owned(),
            egui::TextFormat::simple(
                egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
                palette.muted,
            ),
            (right - x).max(0.0),
        );
        let y = crate::theme::line_y(top + name_h + 2.0, detail_h, &galley);
        ui.painter().galley(egui::pos2(x, y), galley, palette.muted);
    }
    if let Some(track) = switch_rect {
        let toggle = ui
            .interact(track, ui.id().with(("switch", id)), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if toggle.clicked() {
            return Hit::Switch;
        }
    }
    if response.clicked() {
        Hit::Row
    } else {
        Hit::Nothing
    }
}

/// A group heading with an optional small button at its right.
fn group_with(
    ui: &mut egui::Ui,
    text: &str,
    action: Option<(&str, Option<crate::icons::Icon>)>,
    palette: &Palette,
) -> bool {
    ui.add_space(22.0);
    let (rect, heading) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::hover());
    heading.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(FS_SM, family(FAMILY_BODY_SEMIBOLD)),
        palette.ink_strong,
    );
    let y = crate::theme::line_y(rect.top(), rect.height(), &galley);
    ui.painter()
        .galley(egui::pos2(rect.left(), y), galley, palette.ink_strong);
    let mut clicked = false;
    if let Some((label, icon)) = action {
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                clicked = btn_sm(ui, label, icon, BtnTone::Plain, palette).clicked();
            },
        );
    }
    ui.add_space(8.0);
    clicked
}

/// The tone a small button carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BtnTone {
    Plain,
    Primary,
    Danger,
}

/// The design's small button: 26px tall, an 8px radius, medium 12px
/// text with an optional leading icon, in one tone.
pub(crate) fn btn_sm(
    ui: &mut egui::Ui,
    label: &str,
    icon: Option<crate::icons::Icon>,
    tone: BtnTone,
    palette: &Palette,
) -> egui::Response {
    let galley = btn_sm_galley(ui, label);
    let (rect, response) = ui.allocate_exact_size(btn_sm_size(&galley, icon), egui::Sense::click());
    paint_btn_sm(ui, rect, response, galley, label, icon, tone, palette)
}

/// [`btn_sm`] placed by `align` against `anchor`, for rows painted at
/// fixed heights.
#[allow(clippy::too_many_arguments)]
pub(crate) fn btn_sm_at(
    ui: &mut egui::Ui,
    anchor: egui::Pos2,
    align: egui::Align2,
    id: egui::Id,
    label: &str,
    icon: Option<crate::icons::Icon>,
    tone: BtnTone,
    palette: &Palette,
) -> egui::Response {
    let galley = btn_sm_galley(ui, label);
    let size = btn_sm_size(&galley, icon);
    let rect = align.anchor_size(anchor, size);
    let response = ui.interact(rect, id, egui::Sense::click());
    paint_btn_sm(ui, rect, response, galley, label, icon, tone, palette)
}

/// A small button's side padding: the reference's 9px inside its
/// one pixel border.
const BTN_PAD: f32 = 10.0;

/// A small button's label, laid out in its 12px medium face.
pub(crate) fn btn_sm_galley(ui: &egui::Ui, label: &str) -> std::sync::Arc<egui::Galley> {
    ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::new(FS_XS, family(FAMILY_BODY_MEDIUM)),
        egui::Color32::PLACEHOLDER,
    )
}

/// A small button's size for `galley` and its optional icon.
pub(crate) fn btn_sm_size(galley: &egui::Galley, icon: Option<crate::icons::Icon>) -> egui::Vec2 {
    let icon_w = if icon.is_some() { 12.0 + 6.0 } else { 0.0 };
    // The reference rounds a text's measured width up to a whole pixel.
    egui::vec2(BTN_PAD + icon_w + galley.size().x.ceil() + BTN_PAD, 26.0)
}

#[allow(clippy::too_many_arguments)]
fn paint_btn_sm(
    ui: &egui::Ui,
    rect: egui::Rect,
    response: egui::Response,
    galley: std::sync::Arc<egui::Galley>,
    label: &str,
    icon: Option<crate::icons::Icon>,
    tone: BtnTone,
    palette: &Palette,
) -> egui::Response {
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let hovered = response.hovered();
    let (fill, line, ink) = match tone {
        BtnTone::Plain if hovered => (palette.fill_hover, palette.line_strong, palette.ink),
        BtnTone::Plain => (palette.fill, palette.line, palette.ink),
        BtnTone::Primary if hovered => (
            palette.accent_hover,
            palette.accent_hover,
            egui::Color32::WHITE,
        ),
        BtnTone::Primary => (palette.accent, palette.accent, egui::Color32::WHITE),
        BtnTone::Danger if hovered => (palette.danger_soft, palette.danger_bd, palette.danger),
        BtnTone::Danger => (
            egui::Color32::TRANSPARENT,
            palette.danger_bd,
            palette.danger,
        ),
    };
    let radius = egui::CornerRadius::same(R_MD as u8);
    ui.painter().rect_filled(rect, radius, fill);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, line),
        egui::StrokeKind::Inside,
    );
    let mut x = rect.left() + BTN_PAD;
    if let Some(icon) = icon {
        crate::icons::themed_image(icon, 12.0, ink).paint_snapped(
            ui,
            egui::Rect::from_min_size(egui::pos2(x, rect.center().y - 6.0), egui::vec2(12.0, 12.0)),
        );
        x += 12.0 + 6.0;
    }
    // The label's line centers inside the 1px border.
    let y = crate::theme::line_y(rect.top() + 1.0, rect.height() - 2.0, &galley);
    ui.painter().galley(egui::pos2(x, y), galley, ink);
    response
}

/// A sub heading over one list of a page, such as `Ready \u{b7} 3`.
fn subgroup(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    ui.add_space(4.0);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), crate::theme::line_h(FS_XS)),
        egui::Sense::hover(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
        palette.faint,
    );
    let y = crate::theme::line_y(rect.top(), rect.height(), &galley);
    ui.painter()
        .galley(egui::pos2(rect.left(), y), galley, palette.faint);
    ui.add_space(6.0);
}

/// Paints provider `id`'s logo at `size` on its white tile, its left
/// edge at `at.x` and centered on `at.y`, or the bot glyph while the
/// logo loads or when there is none. Gives back its right edge.
///
/// The tile is white because models.dev draws its logos for a light
/// page, and most carry no fill of their own.
fn paint_logo(
    ui: &egui::Ui,
    at: egui::Pos2,
    id: &str,
    site: Option<&str>,
    size: f32,
    palette: &Palette,
) -> f32 {
    match crate::logos::ensure(ui.ctx(), id, site) {
        Some(source) => {
            let tile = egui::Rect::from_min_size(
                egui::pos2(at.x, at.y - (size + 6.0) / 2.0),
                egui::vec2(size + 6.0, size + 6.0),
            );
            ui.painter()
                .rect_filled(tile, egui::CornerRadius::same(6), egui::Color32::WHITE);
            egui::Image::new(source)
                .fit_to_exact_size(egui::vec2(size, size))
                .paint_snapped(
                    ui,
                    egui::Rect::from_center_size(tile.center(), egui::vec2(size, size)),
                );
            tile.right()
        }
        None => {
            let glyph = egui::Rect::from_min_size(
                egui::pos2(at.x, at.y - (size - 2.0) / 2.0),
                egui::vec2(size - 2.0, size - 2.0),
            );
            crate::icons::themed_image(crate::icons::Icon::Bot, size - 2.0, palette.muted)
                .paint_snapped(ui, glyph);
            glyph.right()
        }
    }
}

/// The two letters a provider's tile carries.
fn initials(id: &str) -> String {
    id.chars()
        .filter(char::is_ascii_alphabetic)
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

/// The waiting state while the snapshot is on its way.
pub fn waiting(ui: &mut egui::Ui, palette: &Palette) {
    note(ui, "Reading the engine's configuration\u{2026}", palette);
}

/// The Model Providers page: the providers, grouped by whether their
/// key is there, each opening its form, then the ACP agents; or, while
/// adding, the providers to start from.
pub fn providers_page(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    model: Option<&SessionConfigOption>,
    state: &mut PageState,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    let rows = provider_rows(snapshot, model);
    if state.choosing {
        provider_chooser(ui, snapshot, &rows, state, palette, events);
        return;
    }
    if group_with(
        ui,
        "Providers",
        Some(("Add provider", Some(crate::icons::Icon::Plus))),
        palette,
    ) {
        state.choosing = true;
    }
    let missing = |row: &ProviderRow| {
        snapshot
            .key_state(&row.id)
            .is_some_and(|key| key.source == "missing")
    };
    for (title, needs) in [("Ready", false), ("Needs a key", true)] {
        let group_rows: Vec<&ProviderRow> =
            rows.iter().filter(|row| missing(row) == needs).collect();
        if group_rows.is_empty() {
            continue;
        }
        subgroup(ui, &format!("{title} \u{b7} {}", group_rows.len()), palette);
        boxed(ui, palette, |ui| {
            for row in group_rows {
                provider_row(ui, snapshot, row, palette, events);
            }
        });
    }
    if !snapshot.acp.agents.is_empty() {
        subgroup(
            ui,
            &format!("ACP agents \u{b7} {}", snapshot.acp.agents.len()),
            palette,
        );
        boxed(ui, palette, |ui| {
            for (name, agent) in &snapshot.acp.agents {
                let line = std::iter::once(agent.command.clone())
                    .chain(agent.args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" ");
                let hit = list_row(
                    ui,
                    &format!("acp-{name}"),
                    &Lead::Initials(initials(name)),
                    &[
                        Part::Text(format!("acp:{name}")),
                        Part::Badge("acp".to_owned(), Tone::plain(palette)),
                    ],
                    Some(&line),
                    &Trail::None,
                    true,
                    palette,
                );
                if hit == Hit::Row {
                    events.push(SettingsEvent::AcpForm(Some(name.clone())));
                }
            }
        });
    }
    if rows.is_empty() && snapshot.acp.agents.is_empty() {
        note(ui, "No providers configured yet.", palette);
    }
    note(
        ui,
        "Providers kage registers itself become usable with a key; edits to them are saved as overrides. Custom providers are saved whole under [providers.custom] in config.toml. Keys stay with the engine.",
        palette,
    );
}

/// One provider's row: its tile, name, protocol and origin badges, the
/// endpoint and models, and where its key is. A click opens its form.
fn provider_row(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    row: &ProviderRow,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    let mut name = vec![Part::Text(row.label.clone())];
    if let Some(protocol) = &row.protocol {
        name.push(Part::Badge(protocol.clone(), Tone::plain(palette)));
    }
    if row.custom {
        name.push(Part::Badge("config.toml".to_owned(), Tone::plain(palette)));
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(url) = &row.base_url {
        parts.push(url.clone());
    }
    parts.push(match row.models.len() {
        0 => "no models listed".to_owned(),
        1 => "1 model".to_owned(),
        n => format!("{n} models"),
    });
    if !row.headers.is_empty() {
        parts.push(format!("headers: {}", row.headers.join(", ")));
    }
    let key = match snapshot.key_state(&row.id) {
        Some(state) => match state.source.as_str() {
            "env" => (format!("key from {}", state.env), Tone::ok(palette)),
            "auth" => ("key in auth.json".to_owned(), Tone::ok(palette)),
            "unneeded" => ("no key needed".to_owned(), Tone::plain(palette)),
            _ => (format!("{} not set", state.env), Tone::warn(palette)),
        },
        None => (
            "key source unknown".to_owned(),
            Tone {
                fg: palette.faint,
                bg: palette.fill,
                line: palette.subtle,
            },
        ),
    };
    let hit = list_row(
        ui,
        &format!("provider-{}", row.id),
        &Lead::Logo(row.id.clone(), row.base_url.clone()),
        &name,
        Some(&parts.join(" \u{b7} ")),
        &Trail::Badge(key.0, key.1),
        true,
        palette,
    );
    if hit == Hit::Row {
        let target = if row.custom {
            Target::Custom(Some(row.id.clone()))
        } else {
            Target::Registered(row.id.clone())
        };
        events.push(SettingsEvent::ProviderForm(target));
    }
}

/// What a chooser chip leads with.
enum ChipLead<'a> {
    /// A catalog icon.
    Icon(crate::icons::Icon),
    /// A provider's logo, by id.
    Logo(&'a str),
}

/// A rounded chip with an optional leading icon, as the chooser lists
/// the providers to start from.
fn pick_chip(
    ui: &mut egui::Ui,
    label: &str,
    lead: Option<ChipLead<'_>>,
    palette: &Palette,
) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::new(FS_XS, family(FAMILY_BODY_MEDIUM)),
        palette.ink,
    );
    let lead_w = match lead {
        Some(ChipLead::Icon(_)) => 14.0 + 7.0,
        Some(ChipLead::Logo(_)) => 22.0 + 7.0,
        None => 0.0,
    };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(9.0 + lead_w + galley.size().x + 12.0, 30.0),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let (fill, line) = if response.hovered() {
        (palette.fill_hover, palette.muted)
    } else {
        (palette.surface, palette.line)
    };
    let radius = egui::CornerRadius::same(15);
    ui.painter().rect_filled(rect, radius, fill);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, line),
        egui::StrokeKind::Inside,
    );
    let mut x = rect.left() + 9.0;
    match lead {
        Some(ChipLead::Icon(icon)) => {
            crate::icons::themed_image(icon, 14.0, palette.muted).paint_snapped(
                ui,
                egui::Rect::from_min_size(
                    egui::pos2(x, rect.center().y - 7.0),
                    egui::vec2(14.0, 14.0),
                ),
            );
        }
        Some(ChipLead::Logo(id)) => {
            paint_logo(ui, egui::pos2(x, rect.center().y), id, None, 16.0, palette);
        }
        None => {}
    }
    x += lead_w;
    let h = galley.size().y;
    ui.painter().galley(
        egui::pos2(x, rect.center().y - h / 2.0),
        galley,
        palette.ink,
    );
    response
}

/// The providers to start a new one from: each registered provider not
/// listed yet, the directories to import from, a local server, a custom
/// endpoint, and an ACP agent.
fn provider_chooser(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    rows: &[ProviderRow],
    state: &mut PageState,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    if group_with(ui, "Add provider", Some(("Cancel", None)), palette) {
        state.choosing = false;
    }
    let chips = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            add(ui);
        });
    };
    note(ui, "A provider kage knows: give it a key.", palette);
    chips(ui, &mut |ui| {
        for id in snapshot
            .registered()
            .filter(|id| !rows.iter().any(|row| row.id == *id))
        {
            if pick_chip(
                ui,
                &snapshot.provider_name(id),
                Some(ChipLead::Logo(id)),
                palette,
            )
            .clicked()
            {
                events.push(SettingsEvent::ProviderForm(Target::Registered(
                    id.to_owned(),
                )));
                state.choosing = false;
            }
        }
    });
    note(ui, "Or import one from a directory.", palette);
    chips(ui, &mut |ui| {
        if pick_chip(
            ui,
            "From models.dev",
            Some(ChipLead::Icon(crate::icons::Icon::Globe)),
            palette,
        )
        .clicked()
        {
            events.push(SettingsEvent::Directory(false));
            state.choosing = false;
        }
        if pick_chip(
            ui,
            "api.json",
            Some(ChipLead::Icon(crate::icons::Icon::File)),
            palette,
        )
        .clicked()
        {
            events.push(SettingsEvent::Directory(true));
            state.choosing = false;
        }
    });
    note(ui, "Or an endpoint of your own.", palette);
    chips(ui, &mut |ui| {
        if pick_chip(ui, "Local server (Ollama)", None, palette).clicked() {
            events.push(SettingsEvent::ProviderForm(Target::Local));
            state.choosing = false;
        }
        if pick_chip(ui, "Custom", None, palette).clicked() {
            events.push(SettingsEvent::ProviderForm(Target::Custom(None)));
            state.choosing = false;
        }
        if pick_chip(
            ui,
            "ACP agent",
            Some(ChipLead::Icon(crate::icons::Icon::Bot)),
            palette,
        )
        .clicked()
        {
            events.push(SettingsEvent::AcpForm(None));
            state.choosing = false;
        }
    });
}

/// The MCP Servers page: configured servers with the live status the
/// active session reports, each opening its form.
pub fn mcp_page(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    live: &BTreeMap<String, McpServerStatus>,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    if group_with(
        ui,
        "Servers",
        Some(("Add server", Some(crate::icons::Icon::Plus))),
        palette,
    ) {
        events.push(SettingsEvent::McpForm(None));
    }
    let servers = &snapshot.mcp.servers;
    if servers.is_empty() {
        note(ui, "No MCP servers configured yet.", palette);
        return;
    }
    boxed(ui, palette, |ui| {
        for (name, server) in servers {
            let transport = if server.url.is_some() {
                "http"
            } else {
                "stdio"
            };
            let faint = Tone {
                fg: palette.faint,
                bg: palette.fill,
                line: palette.line,
            };
            let (word, tone, dot) = match (server.disabled, live.get(name)) {
                (true, _) => ("disabled", faint, palette.faint),
                (false, Some(McpServerStatus::Connected)) => {
                    ("connected", Tone::ok(palette), palette.ok)
                }
                (false, Some(McpServerStatus::Failed { .. })) => {
                    ("failed", Tone::danger(palette), palette.danger)
                }
                (false, Some(McpServerStatus::NeedsAuth)) => {
                    ("needs auth", Tone::warn(palette), palette.warn)
                }
                (false, Some(McpServerStatus::Starting)) => {
                    ("starting", Tone::warn(palette), palette.warn)
                }
                (false, None | Some(McpServerStatus::Unknown)) => (
                    "status unknown",
                    Tone {
                        line: palette.subtle,
                        ..faint
                    },
                    palette.faint,
                ),
            };
            let target = match (&server.url, &server.command) {
                (Some(url), _) => url.clone(),
                (None, Some(command)) => std::iter::once(command.clone())
                    .chain(server.args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" "),
                (None, None) => "no command or url".to_owned(),
            };
            let mut extras: Vec<String> = Vec::new();
            if !server.env.is_empty() {
                extras.push(format!(
                    "env: {}",
                    server.env.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            if !server.headers.is_empty() {
                extras.push(format!(
                    "headers: {}",
                    server
                        .headers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if server.oauth.is_some() {
                extras.push("OAuth".to_owned());
            }
            if let Some(McpServerStatus::Failed { error }) = live.get(name) {
                extras.push(error.clone());
            }
            let trail = if extras.is_empty() {
                Trail::None
            } else {
                Trail::Faint(extras.join(" \u{b7} "))
            };
            let hit = list_row(
                ui,
                &format!("mcp-{name}"),
                &Lead::Dot(dot),
                &[
                    Part::Mono(name.clone()),
                    Part::Badge(transport.to_owned(), Tone::plain(palette)),
                    Part::Badge(word.to_owned(), tone),
                ],
                Some(&target),
                &trail,
                true,
                palette,
            );
            if hit == Hit::Row {
                events.push(SettingsEvent::McpForm(Some(name.clone())));
            }
        }
    });
    note(
        ui,
        if snapshot.mcp.allow_sampling {
            "Servers start with the session. Sampling is allowed: a server may run completions on your default model."
        } else {
            "Servers start with the session. Header and environment values stay with the engine; only their names show here."
        },
        palette,
    );
}

/// The Permissions page: the session's mode cards, then the configured
/// rules, editable in place.
#[allow(clippy::too_many_arguments)]
pub fn permissions_page(
    ui: &mut egui::Ui,
    store: &mut Store,
    snapshot: &Snapshot,
    modes: Option<&SessionConfigOption>,
    current: Option<&str>,
    state: &mut PageState,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    crate::ui::settings::group(ui, "This session", palette);
    match modes {
        Some(option) if !option.options.is_empty() => {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                for choice in &option.options {
                    let on = current == Some(choice.value.as_str());
                    if mode_card(ui, choice, on, palette).clicked() {
                        events.push(SettingsEvent::Mode(choice.value.clone()));
                    }
                }
            });
        }
        _ => note(
            ui,
            "Open a session to see and change its permission mode.",
            palette,
        ),
    }
    if group_with(
        ui,
        "Tool rules",
        Some(("Add rule", Some(crate::icons::Icon::Plus))),
        palette,
    ) {
        state.adding = Some(RuleAdd::Tool);
        state.rule_text.clear();
    }
    let rules = &snapshot.permissions;
    boxed(ui, palette, |ui| {
        let hit = list_row(
            ui,
            "confine",
            &Lead::Icon(crate::icons::Icon::FolderLock),
            &[Part::Text(
                "Confine file tools to the working directory".to_owned(),
            )],
            Some("Reads and writes outside the session's directory are refused"),
            &Trail::Switch {
                on: rules.confine_paths,
                live: true,
            },
            false,
            palette,
        );
        if hit == Hit::Switch {
            store.config_set(
                &["permissions", "confine_paths"],
                Some(serde_json::json!(!rules.confine_paths)),
            );
        }
        if state.adding == Some(RuleAdd::Tool) {
            rule_field_row(
                ui,
                state,
                store,
                snapshot,
                "Name the tool, such as shell or github__create_issue, then Enter",
                palette,
            );
        }
        if !rules.tools.is_empty() || !rules.mcp.is_empty() {
            rule_header(ui, palette);
        }
        for (tool, rule) in &rules.tools {
            tool_rule_row(ui, store, snapshot, tool, rule, state, palette);
        }
        for (server, action) in &rules.mcp {
            server_rule_row(ui, store, server, action, palette);
        }
    });
    note(
        ui,
        "Rules are saved to config.toml under [permissions] and apply to sessions opened after. Deny beats allow, and the mode decides what no rule matches.",
        palette,
    );
}

/// One permission mode card: its name with the check on the active
/// mode, over its description, outlined in the accent when chosen.
fn mode_card(
    ui: &mut egui::Ui,
    choice: &kage_client::wire::SessionConfigSelectOption,
    on: bool,
    palette: &Palette,
) -> egui::Response {
    let width = 200.0_f32.min(ui.available_width());
    let description = choice.description.as_deref().map(|text| {
        let mut job = egui::text::LayoutJob::simple(
            text.to_owned(),
            egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
            palette.muted,
            width - 24.0,
        );
        job.wrap.max_width = width - 24.0;
        ui.painter().layout_job(job)
    });
    let name_h = crate::theme::line_h(FS_SM);
    let body = name_h + description.as_ref().map_or(0.0, |text| 6.0 + text.size().y);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, (12.0 + body + 12.0).max(84.0)),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, on, &choice.name)
    });
    let border = if on {
        palette.accent
    } else if response.hovered() {
        palette.line_strong
    } else {
        palette.line
    };
    let radius = egui::CornerRadius::same(R_LG as u8);
    ui.painter().rect_filled(rect, radius, palette.surface);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );
    let top = rect.top() + 12.0;
    ui.painter().text(
        egui::pos2(rect.left() + 12.0, top + name_h / 2.0),
        egui::Align2::LEFT_CENTER,
        &choice.name,
        egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
        palette.ink_strong,
    );
    if on {
        crate::icons::themed_image(crate::icons::Icon::Check, 14.0, palette.accent).paint_snapped(
            ui,
            egui::Rect::from_min_size(
                egui::pos2(rect.right() - 12.0 - 14.0, top + name_h / 2.0 - 7.0),
                egui::vec2(14.0, 14.0),
            ),
        );
    }
    if let Some(text) = description {
        ui.painter().galley(
            egui::pos2(rect.left() + 12.0, top + name_h + 6.0),
            text,
            palette.muted,
        );
    }
    response
}

/// The columns of the rule table.
const RULE_TOOL_W: f32 = 180.0;
const RULE_DEFAULT_W: f32 = 132.0;
const RULE_REMOVE_W: f32 = 22.0;
/// A rule's line: the chips' 19px text and their borders.
const RULE_LINE_H: f32 = 21.0;
/// A rule row: its top border, 8px above and below, and the line.
const RULE_ROW_H: f32 = 1.0 + 8.0 + RULE_LINE_H + 8.0;

/// The x ranges of the rule table's columns inside `rect`: tool,
/// default, allow, deny and remove.
fn rule_columns(rect: egui::Rect) -> [egui::Rangef; 5] {
    let gap = 12.0;
    let left = rect.left() + 16.0;
    let right = rect.right() - 16.0;
    let tool = egui::Rangef::new(left, left + RULE_TOOL_W);
    let default = egui::Rangef::new(tool.max + gap, tool.max + gap + RULE_DEFAULT_W);
    let remove = egui::Rangef::new(right - RULE_REMOVE_W, right);
    let flex = ((remove.min - gap) - (default.max + gap) - gap) / 2.0;
    let allow = egui::Rangef::new(default.max + gap, default.max + gap + flex);
    let deny = egui::Rangef::new(allow.max + gap, allow.max + gap + flex);
    [tool, default, allow, deny, remove]
}

/// The rule table's header row.
fn rule_header(ui: &mut egui::Ui, palette: &Palette) {
    let line = crate::theme::line_h(FS_XS);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 1.0 + 8.0 + line + 8.0),
        egui::Sense::hover(),
    );
    crate::ui::settings::row_rule(ui, rect, palette);
    let columns = rule_columns(rect);
    for (column, text) in columns.iter().zip(["Tool", "Default", "Allow", "Deny"]) {
        let galley = ui.painter().layout_no_wrap(
            text.to_owned(),
            egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
            palette.faint,
        );
        let y = crate::theme::line_y(rect.top() + 9.0, line, &galley);
        ui.painter()
            .galley(egui::pos2(column.min, y), galley, palette.faint);
    }
}

/// The field that names a new tool or glob, and the hint beside it.
/// Enter writes it; Escape drops it.
fn rule_field_row(
    ui: &mut egui::Ui,
    state: &mut PageState,
    store: &mut Store,
    snapshot: &Snapshot,
    hint: &str,
    palette: &Palette,
) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 44.0), egui::Sense::hover());
    crate::ui::settings::row_rule(ui, rect, palette);
    let field = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 16.0, rect.center().y - 13.0),
        egui::vec2(RULE_TOOL_W, 26.0),
    );
    rule_input(ui, field, state, store, snapshot, palette);
    ui.painter().text(
        egui::pos2(field.right() + 12.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        hint,
        egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
        palette.muted,
    );
}

/// The small text field the rule edits type into, at `rect`.
fn rule_input(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    state: &mut PageState,
    store: &mut Store,
    snapshot: &Snapshot,
    palette: &Palette,
) {
    let _ = snapshot;
    let mut submitted = false;
    let mut dropped = false;
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        let response = crate::ui::forms::add_field(
            ui,
            egui::TextEdit::singleline(&mut state.rule_text)
                .id(egui::Id::new("settings-rule-input"))
                .hint_text(
                    egui::RichText::new("glob or tool")
                        .font(egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)))
                        .color(palette.faint),
                )
                .font(egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)))
                .desired_width(rect.width()),
            palette,
        );
        response.request_focus();
        submitted = ui.input(|input| input.key_pressed(egui::Key::Enter));
        dropped = ui.input(|input| input.key_pressed(egui::Key::Escape));
    });
    if submitted
        && let Some(add) = state.adding.clone()
        && let Some(config) = store.config().cloned()
        && let Some((path, value)) = rule_added(&config, &add, &state.rule_text)
    {
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        store.config_set(&path, Some(value));
        state.adding = None;
        state.rule_text.clear();
    } else if dropped {
        state.adding = None;
        state.rule_text.clear();
    }
}

/// One glob of a rule as a badge with its remove mark inside, left
/// edge at `at.x`, centered on `at.y`. Gives back where it landed.
fn glob_chip(ui: &egui::Ui, at: egui::Pos2, glob: &str, tone: Tone) -> egui::Rect {
    let galley = ui.painter().layout_no_wrap(
        glob.to_owned(),
        egui::FontId::new(10.5, family(FAMILY_MONO_REGULAR)),
        tone.fg,
    );
    // The reference's badge: 7px and the border either side, a pixel
    // and the border above and below one line.
    let text_w = galley.size().x.ceil();
    let line = crate::theme::line_h(10.5);
    let rect = egui::Rect::from_min_size(
        egui::pos2(at.x, crate::theme::snap(at.y - (line + 4.0) / 2.0)),
        egui::vec2(8.0 + text_w + 4.0 + 10.0 + 8.0, line + 4.0),
    );
    let radius = egui::CornerRadius::same(((line + 4.0) / 2.0) as u8);
    ui.painter().rect_filled(rect, radius, tone.bg);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, tone.line),
        egui::StrokeKind::Inside,
    );
    let y = crate::theme::line_y(rect.top() + 2.0, line, &galley);
    ui.painter()
        .galley(egui::pos2(rect.left() + 8.0, y), galley, tone.fg);
    crate::icons::themed_image(crate::icons::Icon::X, 10.0, tone.fg).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(rect.left() + 8.0 + text_w + 4.0, rect.center().y - 5.0),
            egui::vec2(10.0, 10.0),
        ),
    );
    rect
}

/// One choice of a rule's default, filled in its tone when chosen.
fn action_chip(
    ui: &egui::Ui,
    at: egui::Pos2,
    action: &str,
    on: bool,
    palette: &Palette,
) -> egui::Rect {
    let tone = match action {
        "deny" => Tone::danger(palette),
        "ask" => Tone::warn(palette),
        _ => Tone::ok(palette),
    };
    let galley = ui.painter().layout_no_wrap(
        action.to_owned(),
        egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
        if on { tone.fg } else { palette.faint },
    );
    // 6px and a 1px border either side of one line.
    let line = crate::theme::line_h(FS_XS);
    let rect = egui::Rect::from_min_size(
        egui::pos2(at.x, crate::theme::snap(at.y - (line + 2.0) / 2.0)),
        egui::vec2(galley.size().x.ceil() + 14.0, line + 2.0),
    );
    if on {
        let radius = egui::CornerRadius::same(((line + 2.0) / 2.0) as u8);
        ui.painter().rect_filled(rect, radius, tone.bg);
        ui.painter().rect_stroke(
            rect,
            radius,
            egui::Stroke::new(1.0, tone.line),
            egui::StrokeKind::Inside,
        );
    }
    let color = if on { tone.fg } else { palette.faint };
    let y = crate::theme::line_y(rect.top() + 1.0, line, &galley);
    ui.painter()
        .galley(egui::pos2(rect.left() + 7.0, y), galley, color);
    rect
}

/// One tool's rule: its default, its allow and deny globs, and a remove
/// button, each writing the rule back when clicked.
fn tool_rule_row(
    ui: &mut egui::Ui,
    store: &mut Store,
    snapshot: &Snapshot,
    tool: &str,
    rule: &ToolRules,
    state: &mut PageState,
    palette: &Palette,
) {
    let (rect, row) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), RULE_ROW_H),
        egui::Sense::hover(),
    );
    row.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, tool));
    crate::ui::settings::row_rule(ui, rect, palette);
    let [tool_col, default_col, allow_col, deny_col, remove_col] = rule_columns(rect);
    let mid = rect.top() + 9.0 + RULE_LINE_H / 2.0;
    let name = ui.painter().layout_no_wrap(
        tool.to_owned(),
        egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
        palette.ink,
    );
    let y = crate::theme::line_y(rect.top() + 9.0, RULE_LINE_H, &name);
    ui.painter()
        .galley(egui::pos2(tool_col.min, y), name, palette.ink);
    let mut write: Option<ToolRules> = None;
    let mut x = default_col.min;
    for action in ACTIONS {
        let on = rule.default == action || (rule.default.is_empty() && action == "allow");
        let chip = action_chip(ui, egui::pos2(x, mid), action, on, palette);
        x = chip.right() + 2.0;
        let response = ui
            .interact(
                chip,
                ui.id().with(("rule-default", tool, action)),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if response.clicked() {
            let mut next = rule.clone();
            action.clone_into(&mut next.default);
            write = Some(next);
        }
    }
    for (deny, column) in [(false, allow_col), (true, deny_col)] {
        let list = if deny { &rule.deny } else { &rule.allow };
        let mut x = column.min;
        for (ix, glob) in list.iter().enumerate() {
            let tone = if deny {
                Tone::danger(palette)
            } else {
                Tone::plain(palette)
            };
            let width = badge_width(ui, glob, true) + 14.0;
            if x + width > column.max {
                break;
            }
            let chip = glob_chip(ui, egui::pos2(x, mid), glob, tone);
            let response = ui
                .interact(
                    chip,
                    ui.id().with(("glob", tool, deny, ix)),
                    egui::Sense::click(),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Remove");
            if response.clicked() {
                let mut next = rule.clone();
                if deny {
                    next.deny.remove(ix);
                } else {
                    next.allow.remove(ix);
                }
                write = Some(next);
            }
            x = chip.right() + 4.0;
        }
        let adding = RuleAdd::Glob {
            tool: tool.to_owned(),
            deny,
        };
        if state.adding.as_ref() == Some(&adding) {
            let field = egui::Rect::from_min_size(
                egui::pos2(x, mid - 12.0),
                egui::vec2((column.max - x).clamp(60.0, 140.0), 24.0),
            );
            rule_input(ui, field, state, store, snapshot, palette);
        } else {
            let galley = ui.painter().layout_no_wrap(
                "+ glob".to_owned(),
                egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
                palette.faint,
            );
            let chip = egui::Rect::from_min_size(
                egui::pos2(x, mid - 9.0),
                egui::vec2(galley.size().x + 12.0, 18.0),
            );
            crate::ui::transcript::text_rows::dashed_rect(ui, chip, 9.0, palette.line);
            ui.painter()
                .galley(chip.center() - galley.size() / 2.0, galley, palette.faint);
            let response = ui
                .interact(
                    chip,
                    ui.id().with(("glob-add", tool, deny)),
                    egui::Sense::click(),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                state.adding = Some(adding);
                state.rule_text.clear();
            }
        }
    }
    let bin = egui::Rect::from_center_size(
        egui::pos2(remove_col.center(), mid),
        egui::vec2(RULE_REMOVE_W, 22.0),
    );
    crate::icons::themed_image(crate::icons::Icon::Trash, 13.0, palette.faint).paint_snapped(
        ui,
        egui::Rect::from_center_size(bin.center(), egui::vec2(13.0, 13.0)),
    );
    let remove = ui
        .interact(
            bin,
            ui.id().with(("rule-remove", tool)),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Remove the rule");
    if remove.clicked() {
        store.config_set(&["permissions", "tools", tool], None);
    } else if let Some(next) = write {
        store.config_set(&["permissions", "tools", tool], Some(rule_json(&next)));
    }
}

/// One MCP server's fallback action, with a remove button.
fn server_rule_row(
    ui: &mut egui::Ui,
    store: &mut Store,
    server: &str,
    action: &str,
    palette: &Palette,
) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), RULE_ROW_H),
        egui::Sense::hover(),
    );
    crate::ui::settings::row_rule(ui, rect, palette);
    let [tool_col, default_col, _, _, remove_col] = rule_columns(rect);
    let mid = rect.top() + 9.0 + RULE_LINE_H / 2.0;
    let label = if server == "*" {
        "every MCP server".to_owned()
    } else {
        format!("mcp: {server}")
    };
    let name = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
        palette.ink,
    );
    let y = crate::theme::line_y(rect.top() + 9.0, RULE_LINE_H, &name);
    ui.painter()
        .galley(egui::pos2(tool_col.min, y), name, palette.ink);
    let mut x = default_col.min;
    for choice in ACTIONS {
        let chip = action_chip(ui, egui::pos2(x, mid), choice, action == choice, palette);
        x = chip.right() + 2.0;
        let response = ui
            .interact(
                chip,
                ui.id().with(("mcp-default", server, choice)),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if response.clicked() {
            store.config_set(
                &["permissions", "mcp", server],
                Some(serde_json::json!(choice)),
            );
        }
    }
    let bin = egui::Rect::from_center_size(
        egui::pos2(remove_col.center(), mid),
        egui::vec2(RULE_REMOVE_W, 22.0),
    );
    crate::icons::themed_image(crate::icons::Icon::Trash, 13.0, palette.faint).paint_snapped(
        ui,
        egui::Rect::from_center_size(bin.center(), egui::vec2(13.0, 13.0)),
    );
    let remove = ui
        .interact(
            bin,
            ui.id().with(("mcp-remove", server)),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if remove.clicked() {
        store.config_set(&["permissions", "mcp", server], None);
    }
}

/// The Plugins page: the directory and the allowlist, then every plugin
/// installed or named in the config, with whether it loads, its
/// capability grants and whether it has settings.
pub fn plugins_page(
    ui: &mut egui::Ui,
    store: &mut Store,
    snapshot: &Snapshot,
    state: &mut PageState,
    palette: &Palette,
) {
    let plugins = &snapshot.plugins;
    let dir = plugins.dir.clone().unwrap_or_else(|| {
        std::path::Path::new(&crate::prefs::display_config_dir())
            .join("plugins")
            .to_string_lossy()
            .into_owned()
    });
    boxed(ui, palette, |ui| {
        list_row(
            ui,
            "plugin-dir",
            &Lead::Icon(crate::icons::Icon::Folder),
            &[Part::Text("Plugin directory".to_owned())],
            None,
            &Trail::Mono(dir),
            false,
            palette,
        );
        let loaded = if !plugins.enabled.is_empty() {
            format!(
                "Only these load: {} (a deprecated allowlist; switching a plugin moves \
                 the rest to the disabled list)",
                plugins.enabled.join(", ")
            )
        } else if plugins.disabled.is_empty() {
            "Every plugin in the directory loads".to_owned()
        } else {
            format!("Every plugin loads but {}", plugins.disabled.join(", "))
        };
        list_row(
            ui,
            "plugin-loaded",
            &Lead::Icon(crate::icons::Icon::ListTodo),
            &[Part::Text("Loaded plugins".to_owned())],
            Some(&loaded),
            &Trail::None,
            false,
            palette,
        );
    });
    let shown = group_with(
        ui,
        "Plugins",
        Some(("Install a plugin", Some(crate::icons::Icon::Plus))),
        palette,
    );
    if shown {
        state.plugin_input = Some(String::new());
        state.plugin_install = None;
    }
    install_row(ui, store, state, shown, palette);
    let rows = plugin_rows(snapshot);
    if !rows.is_empty() {
        boxed(ui, palette, |ui| {
            for row in &rows {
                plugin_row(ui, store, snapshot, row, state, palette);
            }
        });
    }
    kura_section(ui, store, snapshot, state, palette);
    note(
        ui,
        "Lua plugins load from the plugin directory when a session opens, so a change applies to the next session. Blocks and widgets a plugin draws carry its name as an owner badge.",
        palette,
    );
}

/// The plugin registry's packages: a filter, one row per package that
/// opens to what it asks for and its install button, and the outcome of
/// the last install with a button to grant what it asks for.
fn kura_section(
    ui: &mut egui::Ui,
    store: &mut Store,
    snapshot: &Snapshot,
    state: &mut PageState,
    palette: &Palette,
) {
    let refresh = group_with(
        ui,
        "From kura",
        Some(("Refresh", Some(crate::icons::Icon::RefreshCw))),
        palette,
    );
    if refresh || state.registry.is_none() {
        state.registry = Some(store.ask_registry(refresh));
    }
    let faint = |ui: &mut egui::Ui, text: &str, color: egui::Color32| {
        ui.add(egui::Label::new(egui::RichText::new(text).size(FS_XS).color(color)).wrap());
        ui.add_space(8.0);
    };
    let read = state.registry.and_then(|id| store.registry(id).cloned());
    let result = match read {
        None => return faint(ui, "Reading the registry\u{2026}", palette.muted),
        Some(Err(why)) => return faint(ui, &why, palette.danger),
        Some(Ok(result)) => result,
    };
    if let Some(why) = &result.stale {
        faint(
            ui,
            &format!("The registry is unreachable ({why}), so this is the cached list."),
            palette.warn,
        );
    }
    crate::ui::forms::add_field(
        ui,
        egui::TextEdit::singleline(&mut state.registry_filter)
            .id(egui::Id::new("settings-registry-filter"))
            .hint_text(
                egui::RichText::new("Filter packages")
                    .size(FS_XS)
                    .color(palette.faint),
            )
            .font(egui::FontId::new(FS_XS, family(FAMILY_BODY_MEDIUM)))
            .desired_width(ui.available_width()),
        palette,
    );
    ui.add_space(8.0);
    let packages = filtered_packages(&result.packages, &state.registry_filter);
    if packages.is_empty() {
        let empty = if result.packages.is_empty() {
            "kura has no packages yet."
        } else {
            "No package matches."
        };
        faint(ui, empty, palette.muted);
    } else {
        boxed(ui, palette, |ui| {
            for package in packages {
                package_row(ui, store, snapshot, package, state, palette);
            }
        });
    }
    install_outcome(ui, store, snapshot, state, palette);
}

/// The packages whose name, description or tags hold every word of
/// `filter`, case-insensitively.
fn filtered_packages<'a>(
    packages: &'a [kage_client::wire::RegistryPackage],
    filter: &str,
) -> Vec<&'a kage_client::wire::RegistryPackage> {
    let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    packages
        .iter()
        .filter(|package| {
            let text = format!(
                "{} {} {}",
                package.name,
                package.description,
                package.tags.join(" ")
            )
            .to_lowercase();
            words.iter().all(|word| text.contains(word))
        })
        .collect()
}

/// One registry package: its name and what it asks for, its state, and
/// once opened its details and install button.
fn package_row(
    ui: &mut egui::Ui,
    store: &mut Store,
    snapshot: &Snapshot,
    package: &kage_client::wire::RegistryPackage,
    state: &mut PageState,
    palette: &Palette,
) {
    let expanded = state.registry_open.as_deref() == Some(package.name.as_str());
    let local = snapshot
        .installed_plugins
        .iter()
        .find(|plugin| plugin.name == package.name);
    let installed = local
        .filter(|plugin| plugin.from_registry)
        .and_then(|plugin| plugin.version.clone());
    let mut name = vec![Part::Mono(package.name.clone())];
    for cap in &package.capabilities {
        name.push(Part::Badge(cap.name.clone(), Tone::plain(palette)));
    }
    let trail = match (&installed, &package.version) {
        (Some(have), Some(latest)) if have != latest => {
            Trail::Badge(format!("update {latest}"), Tone::warn(palette))
        }
        (Some(have), _) => Trail::Badge(format!("installed {have}"), Tone::ok(palette)),
        (None, Some(latest)) => Trail::Mono(latest.clone()),
        (None, None) => Trail::Faint("unavailable".to_owned()),
    };
    let lead = Lead::Icon(if expanded {
        crate::icons::Icon::ChevronDown
    } else {
        crate::icons::Icon::ChevronRight
    });
    let detail = (!package.description.is_empty()).then_some(package.description.as_str());
    if list_row(
        ui,
        &format!("kura-{}", package.name),
        &lead,
        &name,
        detail,
        &trail,
        true,
        palette,
    ) == Hit::Row
    {
        state.registry_open = (!expanded).then(|| package.name.clone());
    }
    if !expanded {
        return;
    }
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 42,
            right: 42,
            top: 0,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let line = |ui: &mut egui::Ui, text: &str, color: egui::Color32| {
                ui.add(egui::Label::new(egui::RichText::new(text).size(FS_XS).color(color)).wrap());
            };
            if let Some(homepage) = &package.homepage {
                line(ui, homepage, palette.faint);
            }
            if let Some(why) = &package.unavailable {
                line(ui, why, palette.warn);
            }
            if package.capabilities.is_empty() {
                line(ui, "Asks for no capabilities.", palette.muted);
            } else {
                line(ui, "Asks for:", palette.muted);
                for cap in &package.capabilities {
                    line(ui, &format!("{}: {}", cap.name, cap.summary), palette.ink);
                }
            }
            let Some(latest) = &package.version else {
                return;
            };
            let action = match (&installed, local) {
                (Some(have), _) if have == latest => None,
                (Some(_), _) => Some((format!("Update to {latest}"), false)),
                (None, Some(_)) => Some(("Replace the local plugin".to_owned(), true)),
                (None, None) => Some((format!("Install {latest}"), false)),
            };
            if let Some((label, force)) = action
                && btn_sm(ui, &label, None, BtnTone::Primary, palette).clicked()
            {
                let id = store.install_package(&package.name, force);
                state.registry_install = Some((package.name.clone(), id));
            }
        });
}

/// How the last registry install went, with a button to grant the
/// capabilities it asks for that config.toml does not grant yet, or to
/// retry over what was in the way.
fn install_outcome(
    ui: &mut egui::Ui,
    store: &mut Store,
    snapshot: &Snapshot,
    state: &mut PageState,
    palette: &Palette,
) {
    let Some((name, id)) = state.registry_install.clone() else {
        return;
    };
    let text = |ui: &mut egui::Ui, text: &str, color: egui::Color32| {
        ui.add(egui::Label::new(egui::RichText::new(text).size(FS_XS).color(color)).wrap());
    };
    ui.add_space(6.0);
    match store.registry_install(id).cloned() {
        None => text(ui, &format!("Installing {name}\u{2026}"), palette.muted),
        Some(Err(why)) => {
            text(ui, &why, palette.danger);
            let local = snapshot.installed_plugins.iter().any(|p| p.name == name);
            if local && btn_sm(ui, "Replace anyway", None, BtnTone::Danger, palette).clicked() {
                state.registry_install = Some((name.clone(), store.install_package(&name, true)));
            }
        }
        Some(Ok(done)) => {
            let verb = match &done.previous {
                Some(previous) if previous != &done.version => {
                    format!("Updated {name} from {previous} to {}", done.version)
                }
                _ => format!("Installed {name} {}", done.version),
            };
            text(
                ui,
                &format!("{verb}. It loads with the next session."),
                palette.ok,
            );
            let granted = snapshot
                .plugins
                .capabilities
                .get(&name)
                .cloned()
                .unwrap_or_default();
            let missing: Vec<&String> = done
                .capabilities
                .iter()
                .filter(|cap| !granted.contains(cap))
                .collect();
            if !missing.is_empty() {
                let list = missing
                    .iter()
                    .map(|cap| cap.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                text(
                    ui,
                    &format!("It asks for {list}, which config.toml does not grant yet."),
                    palette.muted,
                );
                if btn_sm(
                    ui,
                    &format!("Grant {list}"),
                    None,
                    BtnTone::Primary,
                    palette,
                )
                .clicked()
                {
                    let mut all = granted;
                    all.extend(missing.into_iter().cloned());
                    store.config_set(
                        &["plugins", "capabilities", &name],
                        Some(serde_json::json!(all)),
                    );
                }
            }
        }
    }
    ui.add_space(8.0);
}

/// The install field with its buttons, and the install's outcome.
/// The field takes the keys the frame it shows; Enter installs and
/// Escape cancels, as the buttons do.
fn install_row(
    ui: &mut egui::Ui,
    store: &mut Store,
    state: &mut PageState,
    shown: bool,
    palette: &Palette,
) {
    if let Some(text) = state.plugin_input.as_mut() {
        let mut install = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let width = ui.available_width() - 150.0;
            let field = crate::ui::forms::add_field(
                ui,
                egui::TextEdit::singleline(text)
                    .id(egui::Id::new("settings-plugin-source"))
                    .hint_text(
                        egui::RichText::new("https://example.com/clock.lua")
                            .font(egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)))
                            .color(palette.faint),
                    )
                    .font(egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)))
                    .desired_width(width),
                palette,
            );
            if shown {
                field.request_focus();
            }
            let (enter, escape) = ui.input(|input| {
                (
                    input.key_pressed(egui::Key::Enter),
                    input.key_pressed(egui::Key::Escape),
                )
            });
            install = btn_sm(ui, "Install", None, BtnTone::Primary, palette).clicked()
                || (field.lost_focus() && enter);
            cancel = btn_sm(ui, "Cancel", None, BtnTone::Plain, palette).clicked() || escape;
        });
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "An https:// URL or a path on the engine's machine, to one .lua file. It is checked to compile, then loads with the next session.",
                )
                .size(FS_XS)
                .color(palette.faint),
            )
            .wrap(),
        );
        ui.add_space(10.0);
        if install && !text.trim().is_empty() {
            state.plugin_install = Some(store.install_plugin(text.trim(), false));
            state.plugin_input = None;
        } else if cancel {
            state.plugin_input = None;
        }
    }
    let line = match state
        .plugin_install
        .map(|id| store.write_outcome(id).cloned())
    {
        Some(None) => Some(("Installing\u{2026}".to_owned(), palette.muted)),
        Some(Some(Ok(()))) => Some((
            "Installed. It loads with the next session.".to_owned(),
            palette.ok,
        )),
        Some(Some(Err(why))) => Some((why, palette.danger)),
        None => None,
    };
    if let Some((text, color)) = line {
        ui.label(egui::RichText::new(text).size(FS_XS).color(color));
        ui.add_space(8.0);
    }
}

/// One plugin's row: its name, grants and state, a switch for whether
/// it loads, and once opened the capabilities it may be granted.
fn plugin_row(
    ui: &mut egui::Ui,
    store: &mut Store,
    snapshot: &Snapshot,
    row: &PluginRow,
    state: &mut PageState,
    palette: &Palette,
) {
    let expanded = state.plugin_open.as_deref() == Some(row.name.as_str());
    let mut name = vec![Part::Mono(row.name.clone())];
    for cap in &row.caps {
        name.push(Part::Badge(cap.clone(), Tone::plain(palette)));
    }
    let trail = match row.enabled {
        Some(on) => Trail::Switch { on, live: true },
        None => Trail::None,
    };
    let lead = Lead::Icon(if expanded {
        crate::icons::Icon::ChevronDown
    } else {
        crate::icons::Icon::ChevronRight
    });
    match list_row(
        ui,
        &format!("plugin-{}", row.name),
        &lead,
        &name,
        Some(&row.detail),
        &trail,
        true,
        palette,
    ) {
        Hit::Row => {
            state.plugin_open = (!expanded).then(|| row.name.clone());
        }
        Hit::Switch => {
            if let Some(on) = row.enabled {
                let list = toggled_disabled(snapshot, &row.name, !on);
                let value = (!list.is_empty()).then(|| serde_json::json!(list));
                store.config_set(&["plugins", "disabled"], value);
                if !snapshot.plugins.enabled.is_empty() {
                    store.config_set(&["plugins", "enabled"], None);
                }
            }
        }
        Hit::Nothing => {}
    }
    if !expanded {
        return;
    }
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 42,
            right: 42,
            top: 0,
            bottom: 12,
        })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "Capabilities granted in config.toml. A plugin still asks for each one before it gets it.",
                    )
                    .size(FS_XS)
                    .color(palette.muted),
                )
                .wrap(),
            );
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                for cap in CAPABILITIES {
                    let granted = row.caps.iter().any(|have| have == cap);
                    if grant_chip(ui, cap, granted, palette).clicked() {
                        let mut next: Vec<String> = row
                            .caps
                            .iter()
                            .filter(|have| *have != cap)
                            .cloned()
                            .collect();
                        if !granted {
                            next.push(cap.to_owned());
                        }
                        let value = (!next.is_empty()).then(|| serde_json::json!(next));
                        store.config_set(&["plugins", "capabilities", &row.name], value);
                    }
                }
            });
            if row.enabled.is_some()
                && btn_sm(ui, "Remove plugin", None, BtnTone::Danger, palette).clicked()
            {
                store.remove_plugin(&row.name);
            }
        });
}

/// One capability a plugin may be granted, accent-filled when it is.
fn grant_chip(ui: &mut egui::Ui, cap: &str, granted: bool, palette: &Palette) -> egui::Response {
    let ink = if granted {
        palette.ink_strong
    } else {
        palette.muted
    };
    let galley = ui.painter().layout_no_wrap(
        cap.to_owned(),
        egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
        ink,
    );
    let lead = if granted { 12.0 + 5.0 } else { 0.0 };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(8.0 + lead + galley.size().x + 8.0, 24.0),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, granted, cap));
    let (fill, line) = if granted {
        (palette.accent_soft, palette.accent)
    } else {
        (palette.surface, palette.line)
    };
    let radius = egui::CornerRadius::same(12);
    ui.painter().rect_filled(rect, radius, fill);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, line),
        egui::StrokeKind::Inside,
    );
    let mut x = rect.left() + 8.0;
    if granted {
        crate::icons::themed_image(crate::icons::Icon::Check, 12.0, ink).paint_snapped(
            ui,
            egui::Rect::from_min_size(egui::pos2(x, rect.center().y - 6.0), egui::vec2(12.0, 12.0)),
        );
        x += lead;
    }
    let h = galley.size().y;
    ui.painter()
        .galley(egui::pos2(x, rect.center().y - h / 2.0), galley, ink);
    response
}

#[cfg(test)]
mod tests {
    use super::{
        RuleAdd, Snapshot, filtered_packages, plugin_rows, provider_rows, rule_added,
        toggled_disabled,
    };

    #[test]
    fn an_added_glob_writes_the_whole_rule() {
        let config = serde_json::json!({"permissions": {"tools": {
            "shell": {"default": "ask", "allow": ["git status"], "deny": []}
        }}});
        let add = RuleAdd::Glob {
            tool: "shell".into(),
            deny: true,
        };
        let (path, value) = rule_added(&config, &add, " rm -rf * ").unwrap();
        assert_eq!(path, ["permissions", "tools", "shell"]);
        assert_eq!(
            value,
            serde_json::json!({"default": "ask", "allow": ["git status"], "deny": ["rm -rf *"]})
        );
        let (path, value) = rule_added(&config, &RuleAdd::Tool, "write").unwrap();
        assert_eq!(path, ["permissions", "tools", "write"]);
        assert_eq!(value, serde_json::json!({"default": "ask"}));
        assert!(rule_added(&config, &RuleAdd::Tool, "  ").is_none());
        let fresh = RuleAdd::Glob {
            tool: "read".into(),
            deny: false,
        };
        let (_, value) = rule_added(&config, &fresh, "src/**").unwrap();
        assert_eq!(value["default"], "allow");
    }

    #[test]
    fn plugins_join_the_installed_files_with_the_config() {
        let snapshot = Snapshot::parse(&serde_json::json!({
            "plugins": {
                "enabled": ["tokps"],
                "capabilities": {"tokps": ["session_write"]},
                "config": {"gone": {"k": "<redacted>"}}
            },
            "installedPlugins": [
                {"name": "tokps", "enabled": true},
                {"name": "clock", "enabled": false}
            ]
        }));
        let rows = plugin_rows(&snapshot);
        let lines: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| (row.name.as_str(), row.detail.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                ("clock", "disabled"),
                ("gone", "not installed \u{b7} has settings"),
                ("tokps", "loads \u{b7} capabilities granted in config.toml"),
            ]
        );
        assert_eq!(rows[2].caps, ["session_write"]);
        assert_eq!(rows[1].enabled, None);
        assert!(toggled_disabled(&snapshot, "clock", true).is_empty());
        assert_eq!(
            toggled_disabled(&snapshot, "tokps", false),
            ["clock", "tokps"]
        );
        let open = Snapshot::parse(&serde_json::json!({
            "plugins": {"disabled": ["gone"]},
            "installedPlugins": [{"name": "a", "enabled": true}, {"name": "b", "enabled": true}]
        }));
        assert_eq!(toggled_disabled(&open, "a", false), ["a", "gone"]);
        assert_eq!(toggled_disabled(&open, "gone", true), Vec::<String>::new());
    }

    #[test]
    fn registry_packages_filter_by_every_word_and_rows_show_versions() {
        let packages: Vec<kage_client::wire::RegistryPackage> =
            serde_json::from_value(serde_json::json!([
                {"name": "web", "description": "Search and read the web", "tags": ["net"]},
                {"name": "clock", "description": "Shows the time"}
            ]))
            .unwrap();
        let names = |filter: &str| -> Vec<String> {
            filtered_packages(&packages, filter)
                .into_iter()
                .map(|p| p.name.clone())
                .collect()
        };
        assert_eq!(names(""), ["web", "clock"]);
        assert_eq!(names("READ net"), ["web"]);
        assert!(names("web time").is_empty());

        let snapshot = Snapshot::parse(&serde_json::json!({
            "installedPlugins": [
                {"name": "web", "enabled": true, "version": "0.2.0", "fromRegistry": true}
            ]
        }));
        let rows = plugin_rows(&snapshot);
        assert_eq!(rows[0].detail, "loads \u{b7} 0.2.0 \u{b7} from kura");
    }

    #[test]
    fn providers_join_the_snapshot_with_the_offered_models() {
        let snapshot = Snapshot::parse(&serde_json::json!({
            "providers": {
                "custom": {"lab": {"base_url": "http://lab:8080", "api_key_env": "LAB_KEY",
                    "models": [{"id": "m1", "name": "Lab One", "context": 128000}]}},
                "deepseek": {"api_key_env": "DS", "headers": {"X-Org": "<redacted>"}}
            }
        }));
        let model: kage_client::wire::SessionConfigOption =
            serde_json::from_value(serde_json::json!({
                "id": "model", "name": "Model", "type": "select", "currentValue": "anthropic:a",
                "options": [
                    {"value": "anthropic:a", "name": "Claude A"},
                    {"value": "anthropic:b", "name": "Claude B"},
                    {"value": "lab:m1", "name": "Lab One"}
                ]
            }))
            .unwrap();
        let rows = provider_rows(&snapshot, Some(&model));
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["lab", "anthropic", "deepseek"]);
        assert_eq!(
            rows[0].models,
            ["Lab One \u{b7} 128k"],
            "the custom list wins"
        );
        assert_eq!(rows[1].models.len(), 2);
        assert_eq!(rows[1].key_env, None, "unknown, never guessed");
        assert_eq!(rows[2].key_env.as_deref(), Some("DS"));
        assert_eq!(rows[2].headers, ["X-Org"]);
    }

    #[test]
    fn a_snapshot_that_does_not_parse_reads_empty() {
        let snapshot = Snapshot::parse(&serde_json::json!({"providers": 3}));
        assert!(snapshot.mcp.servers.is_empty());
        assert!(provider_rows(&snapshot, None).is_empty());
    }
}
