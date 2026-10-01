//! The settings pages read from the engine's `_kage/config/get`
//! snapshot: model providers, MCP servers, permissions and plugins.
//!
//! The pages say only what the snapshot and the live session carry. A
//! fact the snapshot leaves out reads unknown rather than guessed. Edits
//! go to the engine through `_kage/config/set`, and the snapshot it
//! answers with redraws the page.

use std::collections::BTreeMap;

use gpui_kit::assets::IconName;
use std::rc::Rc;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Div, Entity, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use kage_client::wire::{McpServerStatus, SessionConfigOption};
use serde::Deserialize;

use crate::store::{Store, StoreHandle as _};
use crate::theme::{FONT_MONO, FS_SM, FS_XS, Palette, R_FULL, R_LG};
use crate::views::kit::{BtnTone, btn_sm, switch};

/// The parts of the snapshot the pages read. Every field defaults, so a
/// section the engine leaves out reads as empty.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Snapshot {
    providers: Providers,
    mcp: Mcp,
    permissions: Permissions,
    plugins: Plugins,
    #[serde(rename = "installedPlugins")]
    installed_plugins: Vec<InstalledPlugin>,
}

impl Snapshot {
    /// Reads a snapshot answer; one that does not parse is empty.
    #[must_use]
    pub(crate) fn parse(value: &serde_json::Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
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

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct McpServer {
    command: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
    disabled: bool,
    oauth: Option<serde_json::Value>,
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
    enabled: Vec<String>,
    capabilities: BTreeMap<String, Vec<String>>,
    config: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct InstalledPlugin {
    name: String,
    enabled: bool,
}

/// One provider row: a configured one, or one the session's model
/// option names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderRow {
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
pub(crate) fn provider_rows(
    snapshot: &Snapshot,
    model: Option<&SessionConfigOption>,
) -> Vec<ProviderRow> {
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
                label: provider.to_owned(),
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
                    label: id.clone(),
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
    rows
}

fn badge(text: impl Into<SharedString>, fg: Hsla, bg: Hsla, line: Hsla) -> Div {
    div()
        .flex_none()
        .px(px(7.))
        .py(px(1.))
        .rounded(px(R_FULL))
        .border_1()
        .border_color(line)
        .bg(bg)
        .text_size(px(10.5))
        .text_color(fg)
        .child(text.into())
}

fn plain_badge(text: impl Into<SharedString>, pal: &Palette) -> Div {
    badge(text, pal.muted, pal.fill, pal.line)
}

fn group(text: impl Into<SharedString>, pal: &Palette) -> Div {
    h_flex()
        .gap(px(8.))
        .mt(px(22.))
        .mb(px(8.))
        .text_size(px(FS_SM))
        .font_weight(crate::theme::WEIGHT_SEMIBOLD)
        .text_color(pal.ink_strong)
        .child(text.into())
}

fn note(text: impl Into<SharedString>, pal: &Palette) -> Div {
    div()
        .my(px(8.))
        .mx(px(2.))
        .text_size(px(FS_XS))
        .text_color(pal.faint)
        .child(text.into())
}

fn boxed(pal: &Palette) -> Div {
    v_flex()
        .rounded(px(R_LG))
        .bg(pal.surface)
        .border_1()
        .border_color(pal.subtle)
        .overflow_hidden()
}

fn mono(text: impl Into<SharedString>, pal: &Palette) -> Div {
    div()
        .font_family(FONT_MONO)
        .text_size(px(FS_XS))
        .text_color(pal.ink)
        .child(text.into())
}

/// A list row: its leading mark, a name line with badges, a detail
/// line, and anything trailing.
fn list_row(lead: impl IntoElement, name: Div, detail: Option<SharedString>, pal: &Palette) -> Div {
    h_flex()
        .px(px(16.))
        .py(px(10.))
        .gap(px(12.))
        .items_center()
        .border_t_1()
        .border_color(pal.subtle)
        .child(lead)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(2.))
                .child(name)
                .children(detail.map(|detail| {
                    div()
                        .truncate()
                        .text_size(px(FS_XS))
                        .text_color(pal.muted)
                        .child(detail)
                })),
        )
}

fn initials(id: &str, pal: &Palette) -> Div {
    let letters: String = id
        .chars()
        .filter(char::is_ascii_alphabetic)
        .take(2)
        .collect::<String>()
        .to_uppercase();
    div()
        .size(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .bg(pal.fill)
        .text_size(px(10.5))
        .font_weight(crate::theme::WEIGHT_BOLD)
        .text_color(pal.muted)
        .child(letters)
}

/// The waiting state while the snapshot is on its way.
#[must_use]
pub(crate) fn waiting(pal: &Palette) -> Vec<AnyElement> {
    vec![note("Reading the engine's configuration\u{2026}", pal).into_any_element()]
}

/// The Model Providers page.
#[must_use]
pub(crate) fn providers_page(
    snapshot: &Snapshot,
    model: Option<&SessionConfigOption>,
    pal: &Palette,
) -> Vec<AnyElement> {
    let rows = provider_rows(snapshot, model);
    let mut list = boxed(pal);
    for row in &rows {
        let mut name = h_flex().gap(px(6.)).items_center().child(
            div()
                .text_size(px(FS_SM))
                .text_color(pal.ink)
                .child(row.label.clone()),
        );
        if let Some(protocol) = &row.protocol {
            name = name.child(plain_badge(protocol.clone(), pal));
        }
        if row.custom {
            name = name.child(plain_badge("config.toml", pal));
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
        let key = match &row.key_env {
            Some(env) if env.is_empty() => plain_badge("no key needed", pal),
            Some(env) => plain_badge(format!("key from {env}"), pal),
            None => badge("key source unknown", pal.faint, pal.fill, pal.subtle),
        };
        list = list.child(
            list_row(
                initials(&row.id, pal),
                name,
                Some(SharedString::from(parts.join(" \u{b7} "))),
                pal,
            )
            .child(key),
        );
    }
    let mut out = vec![];
    if rows.is_empty() {
        out.push(
            note(
                "No providers configured and no session open to list its models.",
                pal,
            )
            .into_any_element(),
        );
    } else {
        out.push(group(format!("Providers \u{b7} {}", rows.len()), pal).into_any_element());
        out.push(list.into_any_element());
    }
    out.push(
        note(
            "Custom providers and overrides come from config.toml; the others are the ones the session's model picker offers. Whether a key is present stays with the engine. Adding and editing providers waits for _kage/config/set.",
            pal,
        )
        .into_any_element(),
    );
    out
}

/// The MCP Servers page: configured servers with the live status the
/// active session reports.
#[must_use]
pub(crate) fn mcp_page(
    snapshot: &Snapshot,
    live: &BTreeMap<String, McpServerStatus>,
    pal: &Palette,
) -> Vec<AnyElement> {
    let servers = &snapshot.mcp.servers;
    if servers.is_empty() {
        return vec![
            note(
                "No MCP servers configured. Add them under [mcp.servers] in config.toml.",
                pal,
            )
            .into_any_element(),
        ];
    }
    let mut list = boxed(pal);
    for (name, server) in servers {
        let transport = if server.url.is_some() {
            "http"
        } else {
            "stdio"
        };
        let (word, fg, bg, line, dot) = match (server.disabled, live.get(name)) {
            (true, _) => ("disabled", pal.faint, pal.fill, pal.line, pal.faint),
            (false, Some(McpServerStatus::Connected)) => {
                ("connected", pal.ok, pal.ok_soft, pal.ok_bd, pal.ok)
            }
            (false, Some(McpServerStatus::Failed { .. })) => (
                "failed",
                pal.danger,
                pal.danger_soft,
                pal.danger_bd,
                pal.danger,
            ),
            (false, Some(McpServerStatus::NeedsAuth)) => {
                ("needs auth", pal.warn, pal.warn_soft, pal.warn_bd, pal.warn)
            }
            (false, Some(McpServerStatus::Starting)) => {
                ("starting", pal.warn, pal.warn_soft, pal.warn_bd, pal.warn)
            }
            (false, None) => ("status unknown", pal.faint, pal.fill, pal.subtle, pal.faint),
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
        let name_line = h_flex()
            .gap(px(6.))
            .items_center()
            .child(mono(name.clone(), pal))
            .child(plain_badge(transport, pal))
            .child(badge(word, fg, bg, line));
        let mut row = list_row(
            div().size(px(8.)).rounded(px(R_FULL)).bg(dot),
            name_line,
            Some(SharedString::from(target)),
            pal,
        );
        if !extras.is_empty() {
            row = row.child(
                div()
                    .max_w(px(240.))
                    .truncate()
                    .text_size(px(FS_XS))
                    .text_color(pal.faint)
                    .child(extras.join(" \u{b7} ")),
            );
        }
        list = list.child(row);
    }
    vec![
        list.into_any_element(),
        note(
            if snapshot.mcp.allow_sampling {
                "Servers start with the session. Sampling is allowed: a server may run completions on your default model."
            } else {
                "Servers start with the session. Header and environment values stay with the engine; only their names show here."
            },
            pal,
        )
        .into_any_element(),
    ]
}

/// What is being added to the permission rules while the shared text
/// field shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuleAdd {
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

/// What the permission rules edit with.
pub(crate) struct RuleEdits {
    /// The store writes go through.
    pub store: Entity<Store>,
    /// What is being added, while the text field shows.
    pub adding: Option<RuleAdd>,
    /// The text field a glob or a tool name is typed into.
    pub input: Option<Entity<InputState>>,
    /// Starts adding, or stops with `None`.
    pub on_add: OnRuleAdd,
}

/// Starts adding to the permission rules, or stops with `None`.
pub(crate) type OnRuleAdd = Rc<dyn Fn(Option<RuleAdd>, &mut Window, &mut App)>;

/// The actions a rule's default may take.
const ACTIONS: [&str; 3] = ["allow", "ask", "deny"];

/// The config path and value an added `text` writes, read against the
/// snapshot `config`: a new tool rule that asks, or the tool's rule with
/// one more glob. `None` when `text` is blank.
pub(crate) fn rule_added(
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

/// The Permissions page: the session's mode cards, then the configured
/// rules, editable in place. `on_mode` sets a mode by value.
#[must_use]
pub(crate) fn permissions_page(
    snapshot: &Snapshot,
    modes: Option<&SessionConfigOption>,
    current: Option<&str>,
    on_mode: impl Fn(String, &mut gpui_kit::App) + Clone + 'static,
    edits: &RuleEdits,
    pal: &'static Palette,
) -> Vec<AnyElement> {
    let mut out: Vec<AnyElement> = Vec::new();
    out.push(group("This session", pal).into_any_element());
    match modes {
        Some(option) if !option.options.is_empty() => {
            let mut cards = h_flex().gap(px(10.)).flex_wrap().items_start();
            for choice in &option.options {
                let on = current == Some(choice.value.as_str());
                let value = choice.value.clone();
                let on_mode = on_mode.clone();
                let line_strong = pal.line_strong;
                cards = cards.child(
                    v_flex()
                        .id(SharedString::from(format!("mode-card-{}", choice.value)))
                        .w(px(200.))
                        .min_h(px(84.))
                        .p(px(12.))
                        .gap(px(6.))
                        .rounded(px(R_LG))
                        .border_1()
                        .border_color(if on { pal.accent } else { pal.line })
                        .bg(pal.surface)
                        .when(!on, |card| {
                            card.hover(move |card| card.border_color(line_strong))
                        })
                        .on_click(move |_, _, cx| on_mode(value.clone(), cx))
                        .child(
                            h_flex()
                                .gap(px(6.))
                                .items_center()
                                .text_size(px(FS_SM))
                                .text_color(pal.ink_strong)
                                .child(div().flex_1().child(choice.name.clone()))
                                .when(on, |top| {
                                    top.child(
                                        Icon::new(IconName::Check)
                                            .with_size(px(14.))
                                            .text_color(pal.accent),
                                    )
                                }),
                        )
                        .children(choice.description.clone().map(|text| {
                            div().text_size(px(FS_XS)).text_color(pal.muted).child(text)
                        })),
                );
            }
            out.push(cards.into_any_element());
        }
        _ => out.push(
            note("Open a session to see and change its permission mode.", pal).into_any_element(),
        ),
    }
    let rules = &snapshot.permissions;
    let on_add = edits.on_add.clone();
    out.push(
        group("Tool rules", pal)
            .justify_between()
            .child(
                btn_sm("rule-add", BtnTone::Plain, pal)
                    .on_click(move |_, window, cx| on_add(Some(RuleAdd::Tool), window, cx))
                    .child(Icon::new(IconName::Plus).with_size(px(12.)))
                    .child("Add rule"),
            )
            .into_any_element(),
    );
    let store = edits.store.clone();
    let confine = !rules.confine_paths;
    let mut table = boxed(pal).child(
        list_row(
            Icon::new(IconName::FolderLock)
                .with_size(px(14.))
                .text_color(pal.faint),
            div()
                .text_size(px(FS_SM))
                .text_color(pal.ink)
                .child("Confine file tools to the working directory"),
            Some("Reads and writes outside the session's directory are refused".into()),
            pal,
        )
        .child(switch("rule-confine", rules.confine_paths, pal).on_click(
            move |_, _, cx| {
                store.act(cx, |store| {
                    store.config_set(
                        &["permissions", "confine_paths"],
                        Some(serde_json::json!(confine)),
                    );
                });
            },
        )),
    );
    if edits.adding == Some(RuleAdd::Tool)
        && let Some(input) = &edits.input
    {
        table = table.child(
            h_flex()
                .px(px(16.))
                .py(px(8.))
                .gap(px(12.))
                .items_center()
                .border_t_1()
                .border_color(pal.subtle)
                .child(div().w(px(180.)).child(Input::new(input).small()))
                .child(
                    div()
                        .text_size(px(FS_XS))
                        .text_color(pal.muted)
                        .child("Name the tool, such as shell or github__create_issue, then Enter"),
                ),
        );
    }
    if !rules.tools.is_empty() || !rules.mcp.is_empty() {
        table = table.child(
            h_flex()
                .px(px(16.))
                .py(px(8.))
                .gap(px(12.))
                .border_t_1()
                .border_color(pal.subtle)
                .text_size(px(FS_XS))
                .text_color(pal.faint)
                .child(div().w(px(180.)).child("Tool"))
                .child(div().w(px(132.)).child("Default"))
                .child(div().flex_1().child("Allow"))
                .child(div().flex_1().child("Deny"))
                .child(div().w(px(22.))),
        );
    }
    for (tool, rule) in &rules.tools {
        table = table.child(tool_rule_row(tool, rule, edits, pal));
    }
    for (server, action) in &rules.mcp {
        table = table.child(server_rule_row(server, action, edits, pal));
    }
    out.push(table.into_any_element());
    out.push(
        note(
            "Rules are saved to config.toml under [permissions] and apply to sessions opened after. Deny beats allow, and the mode decides what no rule matches.",
            pal,
        )
        .into_any_element(),
    );
    out
}

/// One tool's rule: its default, its allow and deny globs, and a remove
/// button, each writing the rule back when clicked.
fn tool_rule_row(tool: &str, rule: &ToolRules, edits: &RuleEdits, pal: &'static Palette) -> Div {
    let write = |rule: ToolRules| {
        let store = edits.store.clone();
        let tool = tool.to_owned();
        move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
            let value = rule_json(&rule);
            store.act(cx, |store| {
                store.config_set(&["permissions", "tools", &tool], Some(value.clone()));
            });
        }
    };
    let mut defaults = h_flex().w(px(132.)).gap(px(2.));
    for action in ACTIONS {
        let on = rule.default == action || (rule.default.is_empty() && action == "allow");
        let mut next = rule.clone();
        action.clone_into(&mut next.default);
        defaults = defaults.child(
            action_chip(format!("rule-{tool}-{action}"), action, on, pal).on_click(write(next)),
        );
    }
    let globs = |deny: bool| {
        let list = if deny { &rule.deny } else { &rule.allow };
        let mut cell = h_flex().flex_1().flex_wrap().gap(px(4.)).items_center();
        for (ix, glob) in list.iter().enumerate() {
            let mut next = rule.clone();
            if deny {
                next.deny.remove(ix);
            } else {
                next.allow.remove(ix);
            }
            cell = cell.child(
                glob_chip(format!("glob-{tool}-{deny}-{ix}"), glob, deny, pal)
                    .on_click(write(next)),
            );
        }
        let adding = RuleAdd::Glob {
            tool: tool.to_owned(),
            deny,
        };
        if edits.adding.as_ref() == Some(&adding)
            && let Some(input) = &edits.input
        {
            cell.child(div().w(px(140.)).child(Input::new(input).small()))
        } else {
            let on_add = edits.on_add.clone();
            cell.child(
                div()
                    .id(SharedString::from(format!("glob-add-{tool}-{deny}")))
                    .px(px(6.))
                    .rounded(px(R_FULL))
                    .border_1()
                    .border_dashed()
                    .border_color(pal.line)
                    .text_size(px(FS_XS))
                    .text_color(pal.faint)
                    .cursor_pointer()
                    .child("+ glob")
                    .on_click(move |_, window, cx| on_add(Some(adding.clone()), window, cx)),
            )
        }
    };
    let store = edits.store.clone();
    let path_tool = tool.to_owned();
    rule_row(pal)
        .child(div().w(px(180.)).child(mono(tool.to_owned(), pal)))
        .child(defaults)
        .child(globs(false))
        .child(globs(true))
        .child(
            remove_button(format!("rule-rm-{tool}"), pal).on_click(move |_, _, cx| {
                store.act(cx, |store| {
                    store.config_set(&["permissions", "tools", &path_tool], None);
                });
            }),
        )
}

/// One MCP server's fallback action, with a remove button.
fn server_rule_row(server: &str, action: &str, edits: &RuleEdits, pal: &'static Palette) -> Div {
    let label = if server == "*" {
        "every MCP server".to_owned()
    } else {
        format!("mcp: {server}")
    };
    let mut defaults = h_flex().w(px(132.)).gap(px(2.));
    for choice in ACTIONS {
        let store = edits.store.clone();
        let server = server.to_owned();
        defaults = defaults.child(
            action_chip(
                format!("mcp-rule-{server}-{choice}"),
                choice,
                action == choice,
                pal,
            )
            .on_click(move |_, _, cx| {
                store.act(cx, |store| {
                    store.config_set(
                        &["permissions", "mcp", &server],
                        Some(serde_json::json!(choice)),
                    );
                });
            }),
        );
    }
    let store = edits.store.clone();
    let path_server = server.to_owned();
    rule_row(pal)
        .child(div().w(px(180.)).child(mono(label, pal)))
        .child(defaults)
        .child(div().flex_1())
        .child(div().flex_1())
        .child(
            remove_button(format!("mcp-rule-rm-{server}"), pal).on_click(move |_, _, cx| {
                store.act(cx, |store| {
                    store.config_set(&["permissions", "mcp", &path_server], None);
                });
            }),
        )
}

fn rule_row(pal: &Palette) -> Div {
    h_flex()
        .px(px(16.))
        .py(px(8.))
        .gap(px(12.))
        .items_center()
        .border_t_1()
        .border_color(pal.subtle)
}

/// One choice of a rule's default, filled in its tone when chosen.
fn action_chip(id: String, action: &'static str, on: bool, pal: &Palette) -> Stateful<Div> {
    let (fg, bg, line) = match action {
        "deny" => (pal.danger, pal.danger_soft, pal.danger_bd),
        "ask" => (pal.warn, pal.warn_soft, pal.warn_bd),
        _ => (pal.ok, pal.ok_soft, pal.ok_bd),
    };
    div()
        .id(SharedString::from(id))
        .px(px(6.))
        .rounded(px(R_FULL))
        .border_1()
        .text_size(px(FS_XS))
        .cursor_pointer()
        .map(|chip| {
            if on {
                chip.text_color(fg).bg(bg).border_color(line)
            } else {
                chip.text_color(pal.faint)
                    .border_color(gpui_kit::transparent_black())
            }
        })
        .child(action)
}

/// One glob of a rule, removed when clicked.
fn glob_chip(id: String, glob: &str, deny: bool, pal: &Palette) -> Stateful<Div> {
    let chip = if deny {
        badge(glob.to_owned(), pal.danger, pal.danger_soft, pal.danger_bd)
    } else {
        plain_badge(glob.to_owned(), pal)
    };
    chip.id(SharedString::from(id))
        .flex()
        .items_center()
        .gap(px(4.))
        .font_family(FONT_MONO)
        .cursor_pointer()
        .child(Icon::new(IconName::X).with_size(px(10.)))
}

fn remove_button(id: String, pal: &Palette) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .w(px(22.))
        .flex()
        .justify_center()
        .text_color(pal.faint)
        .cursor_pointer()
        .child(Icon::new(IconName::Trash).with_size(px(13.)))
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
pub(crate) struct PluginRow {
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
            let enabled = snapshot
                .installed_plugins
                .iter()
                .find(|plugin| &plugin.name == name)
                .map(|plugin| plugin.enabled);
            let state = match enabled {
                Some(true) => "loads",
                Some(false) => "skipped by the allowlist",
                None => "not installed",
            };
            let detail = [
                Some(state),
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

/// The `[plugins] enabled` allowlist after turning plugin `name` on or
/// off. An empty list loads every plugin, so turning one off lists every
/// other installed plugin; names the list holds stay. `None` when the
/// list would end up empty, which would load every plugin instead of
/// none.
fn toggled_allowlist(snapshot: &Snapshot, name: &str, on: bool) -> Option<Vec<String>> {
    let mut list = if snapshot.plugins.enabled.is_empty() {
        snapshot
            .installed_plugins
            .iter()
            .map(|plugin| plugin.name.clone())
            .collect()
    } else {
        snapshot.plugins.enabled.clone()
    };
    list.retain(|entry| entry != name);
    if on {
        list.push(name.to_owned());
    }
    list.sort();
    (!list.is_empty()).then_some(list)
}

/// One plugin's row: its name, grants and state, a switch for whether
/// it loads, and once opened the capabilities it may be granted.
fn plugin_row(
    snapshot: &Snapshot,
    row: &PluginRow,
    expanded: bool,
    store: &Entity<Store>,
    on_open: impl Fn(Option<String>, &mut App) + Clone + 'static,
    pal: &'static Palette,
) -> AnyElement {
    let mut line = h_flex()
        .gap(px(6.))
        .items_center()
        .flex_wrap()
        .child(mono(row.name.clone(), pal));
    for cap in &row.caps {
        line = line.child(plain_badge(cap.clone(), pal));
    }
    let name = row.name.clone();
    let head = list_row(
        Icon::new(if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .with_size(px(14.))
        .text_color(pal.faint),
        line,
        Some(SharedString::from(row.detail.clone())),
        pal,
    )
    .id(SharedString::from(format!("plugin-{}", row.name)))
    .cursor_pointer()
    .on_click(move |_, _, cx| on_open((!expanded).then(|| name.clone()), cx))
    .when_some(row.enabled, |head, on| {
        let switch = switch(format!("plugin-on-{}", row.name), on, pal);
        let Some(list) = toggled_allowlist(snapshot, &row.name, !on) else {
            return head.child(
                switch
                    .opacity(0.45)
                    .cursor_default()
                    .tooltip(|window, cx| {
                        Tooltip::new("the last plugin the allowlist names stays on: an empty [plugins] enabled loads every plugin")
                            .build(window, cx)
                    })
                    .on_click(|_, _, cx| cx.stop_propagation()),
            );
        };
        let store = store.clone();
        head.child(switch.on_click(move |_, _, cx| {
            cx.stop_propagation();
            let list = list.clone();
            store.act(cx, |store| {
                store.config_set(&["plugins", "enabled"], Some(serde_json::json!(list)));
            });
        }))
    });
    if !expanded {
        return head.into_any_element();
    }
    let mut grants = h_flex().gap(px(6.)).flex_wrap();
    for cap in CAPABILITIES {
        let granted = row.caps.iter().any(|have| have == cap);
        let mut next: Vec<String> = row
            .caps
            .iter()
            .filter(|have| *have != cap)
            .cloned()
            .collect();
        if !granted {
            next.push(cap.to_owned());
        }
        let name = row.name.clone();
        let store = store.clone();
        grants = grants.child(
            h_flex()
                .id(SharedString::from(format!("grant-{}-{cap}", row.name)))
                .gap(px(5.))
                .px(px(8.))
                .h(px(24.))
                .items_center()
                .rounded(px(R_FULL))
                .border_1()
                .border_color(if granted { pal.accent } else { pal.line })
                .bg(if granted {
                    pal.accent_soft
                } else {
                    pal.surface
                })
                .cursor_pointer()
                .font_family(FONT_MONO)
                .text_size(px(FS_XS))
                .text_color(if granted { pal.ink_strong } else { pal.muted })
                .when(granted, |chip| {
                    chip.child(Icon::new(IconName::Check).with_size(px(12.)))
                })
                .child(cap)
                .on_click(move |_, _, cx| {
                    let value = (!next.is_empty()).then(|| serde_json::json!(next));
                    store.act(cx, |store| {
                        store.config_set(&["plugins", "capabilities", &name], value.clone());
                    });
                }),
        );
    }
    v_flex()
        .child(head)
        .child(
            v_flex()
                .px(px(42.))
                .pb(px(12.))
                .gap(px(8.))
                .child(
                    div()
                        .text_size(px(FS_XS))
                        .text_color(pal.muted)
                        .child("Capabilities granted in config.toml. A plugin still asks for each one before it gets it."),
                )
                .child(grants),
        )
        .into_any_element()
}

/// The Plugins page: the directory and the allowlist, then every plugin
/// installed or named in the config, with whether it loads, its
/// capability grants and whether it has settings.
#[must_use]
pub(crate) fn plugins_page(
    snapshot: &Snapshot,
    store: &Entity<Store>,
    open: Option<&str>,
    on_open: impl Fn(Option<String>, &mut App) + Clone + 'static,
    pal: &'static Palette,
) -> Vec<AnyElement> {
    let plugins = &snapshot.plugins;
    let dir = plugins
        .dir
        .clone()
        .unwrap_or_else(|| "~/.config/kage/plugins".to_owned());
    let mut out: Vec<AnyElement> = vec![
        boxed(pal)
            .child(
                list_row(
                    Icon::new(IconName::Folder)
                        .with_size(px(14.))
                        .text_color(pal.faint),
                    div()
                        .text_size(px(FS_SM))
                        .text_color(pal.ink)
                        .child("Plugin directory"),
                    None,
                    pal,
                )
                .child(mono(dir, pal)),
            )
            .child(list_row(
                Icon::new(IconName::ListTodo)
                    .with_size(px(14.))
                    .text_color(pal.faint),
                div()
                    .text_size(px(FS_SM))
                    .text_color(pal.ink)
                    .child("Loaded plugins"),
                Some(SharedString::from(if plugins.enabled.is_empty() {
                    "Every plugin in the directory loads".to_owned()
                } else {
                    format!("Only these load: {}", plugins.enabled.join(", "))
                })),
                pal,
            ))
            .into_any_element(),
    ];
    let rows = plugin_rows(snapshot);
    if !rows.is_empty() {
        out.push(group("Plugins", pal).into_any_element());
        let mut list = boxed(pal);
        for row in rows {
            let expanded = open == Some(row.name.as_str());
            list = list.child(plugin_row(
                snapshot,
                &row,
                expanded,
                store,
                on_open.clone(),
                pal,
            ));
        }
        out.push(list.into_any_element());
    }
    out.push(
        note(
            "Lua plugins load from the plugin directory when a session opens, so a change applies to the next session. Blocks and widgets a plugin draws carry its name as an owner badge.",
            pal,
        )
        .into_any_element(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::{RuleAdd, Snapshot, plugin_rows, provider_rows, rule_added, toggled_allowlist};

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
                ("clock", "skipped by the allowlist"),
                ("gone", "not installed \u{b7} has settings"),
                ("tokps", "loads \u{b7} capabilities granted in config.toml"),
            ]
        );
        assert_eq!(rows[2].caps, ["session_write"]);
        assert_eq!(rows[1].enabled, None);
        assert_eq!(
            toggled_allowlist(&snapshot, "clock", true).unwrap(),
            ["clock", "tokps"]
        );
        assert_eq!(toggled_allowlist(&snapshot, "tokps", false), None);
        let open = Snapshot::parse(&serde_json::json!({
            "installedPlugins": [{"name": "a", "enabled": true}, {"name": "b", "enabled": true}]
        }));
        assert_eq!(toggled_allowlist(&open, "a", false).unwrap(), ["b"]);
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
