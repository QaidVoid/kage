//! The settings pages read from the engine's `_kage/config/get`
//! snapshot: model providers, MCP servers, permissions and plugins.
//!
//! The pages say only what the snapshot and the live session carry. A
//! fact the snapshot leaves out reads unknown rather than guessed, and
//! nothing here writes config: the engine has no write method yet, so
//! the controls that would need one are left out.

use std::collections::BTreeMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Div, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use kage_client::wire::{McpServerStatus, SessionConfigOption};
use serde::Deserialize;

use crate::theme::{FONT_MONO, FS_SM, FS_XS, Palette, R_FULL, R_LG};

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

#[derive(Debug, Default, Deserialize)]
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

/// The Permissions page: the session's mode cards and the configured
/// rules. `on_mode` sets a mode by value.
#[must_use]
pub(crate) fn permissions_page(
    snapshot: &Snapshot,
    modes: Option<&SessionConfigOption>,
    current: Option<&str>,
    on_mode: impl Fn(String, &mut gpui_kit::App) + Clone + 'static,
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
    out.push(group("Tool rules", pal).into_any_element());
    let rules = &snapshot.permissions;
    if rules.tools.is_empty() && rules.mcp.is_empty() {
        out.push(
            note(
                "No rules configured: every tool follows the session's mode.",
                pal,
            )
            .into_any_element(),
        );
    } else {
        let header = h_flex()
            .px(px(16.))
            .py(px(8.))
            .gap(px(12.))
            .text_size(px(FS_XS))
            .text_color(pal.faint)
            .child(div().w(px(180.)).child("Tool"))
            .child(div().w(px(64.)).child("Default"))
            .child(div().flex_1().child("Allow"))
            .child(div().flex_1().child("Deny"));
        let mut table = boxed(pal).child(header);
        let globs = |list: &[String], deny: bool| {
            h_flex()
                .flex_1()
                .flex_wrap()
                .gap(px(4.))
                .children(list.iter().map(|glob| {
                    if deny {
                        badge(glob.clone(), pal.danger, pal.danger_soft, pal.danger_bd)
                            .font_family(FONT_MONO)
                    } else {
                        plain_badge(glob.clone(), pal).font_family(FONT_MONO)
                    }
                }))
        };
        for (tool, rule) in &rules.tools {
            table = table.child(
                h_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(12.))
                    .items_center()
                    .border_t_1()
                    .border_color(pal.subtle)
                    .child(div().w(px(180.)).child(mono(tool.clone(), pal)))
                    .child(h_flex().w(px(64.)).child(action_badge(&rule.default, pal)))
                    .child(globs(&rule.allow, false))
                    .child(globs(&rule.deny, true)),
            );
        }
        for (server, action) in &rules.mcp {
            let label = if server == "*" {
                "every MCP server".to_owned()
            } else {
                format!("mcp: {server}")
            };
            table = table.child(
                h_flex()
                    .px(px(16.))
                    .py(px(8.))
                    .gap(px(12.))
                    .items_center()
                    .border_t_1()
                    .border_color(pal.subtle)
                    .child(div().w(px(180.)).child(mono(label, pal)))
                    .child(h_flex().w(px(64.)).child(action_badge(action, pal)))
                    .child(div().flex_1())
                    .child(div().flex_1()),
            );
        }
        out.push(table.into_any_element());
    }
    out.push(
        note(
            if rules.confine_paths {
                "File tools are confined to the working directory. Rules live in config.toml under [permissions.tools.<tool>]; deny beats allow, and the mode decides what no rule matches."
            } else {
                "Rules live in config.toml under [permissions.tools.<tool>]; deny beats allow, and the mode decides what no rule matches."
            },
            pal,
        )
        .into_any_element(),
    );
    out
}

fn action_badge(action: &str, pal: &Palette) -> Div {
    match action {
        "deny" => badge("deny", pal.danger, pal.danger_soft, pal.danger_bd),
        "ask" => badge("ask", pal.warn, pal.warn_soft, pal.warn_bd),
        _ => badge("allow", pal.ok, pal.ok_soft, pal.ok_bd),
    }
}

/// Every plugin installed or named in the config, by name, with its
/// capability grants and a line saying whether it loads, whether it has
/// grants and whether it has settings.
fn plugin_rows(snapshot: &Snapshot) -> Vec<(String, Vec<String>, String)> {
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
            let state = match installed {
                Some(plugin) if plugin.enabled => "loads",
                Some(_) => "skipped by the allowlist",
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
            (name.clone(), caps, detail)
        })
        .collect()
}

/// The Plugins page: the directory and the allowlist, then every plugin
/// installed or named in the config, with whether it loads, its
/// capability grants and whether it has settings.
#[must_use]
pub(crate) fn plugins_page(snapshot: &Snapshot, pal: &Palette) -> Vec<AnyElement> {
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
        for (name, caps, detail) in rows {
            let mut line = h_flex()
                .gap(px(6.))
                .items_center()
                .flex_wrap()
                .child(mono(name, pal));
            for cap in caps {
                line = line.child(plain_badge(cap, pal));
            }
            list = list.child(list_row(
                Icon::new(IconName::Zap)
                    .with_size(px(14.))
                    .text_color(pal.faint),
                line,
                Some(SharedString::from(detail)),
                pal,
            ));
        }
        out.push(list.into_any_element());
    }
    out.push(
        note(
            "Lua plugins load from the plugin directory when a session opens. Blocks and widgets a plugin draws carry its name as an owner badge. Enabling and installing wait for _kage/config/set.",
            pal,
        )
        .into_any_element(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::{Snapshot, plugin_rows, provider_rows};

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
            .map(|(name, _, detail)| (name.as_str(), detail.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                ("clock", "skipped by the allowlist"),
                ("gone", "not installed \u{b7} has settings"),
                ("tokps", "loads \u{b7} capabilities granted in config.toml"),
            ]
        );
        assert_eq!(rows[2].1, ["session_write"]);
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
