//! The settings dialog: a section nav on the left and one page on the
//! right, over the shell like the other dialogs.
//!
//! Pages render thin over the store. The client's own choices (theme,
//! Enter behavior, the chat display toggles, the archive) are preferences the
//! shell stores; everything about the engine is read from what the
//! connection reported and says unknown where it reported nothing.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Bounds, Context, Div, ElementId, Entity, FocusHandle,
    Focusable, Hsla, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Pixels,
    Render, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use gpui_kit::component::input::{Escape as InputEscape, InputEvent, InputState};

use crate::prefs::Prefs;
use crate::store::{Store, StoreHandle as _};
use crate::theme::{
    FONT_MONO, FS_SM, FS_XS, Palette, R_FULL, R_LG, R_MD, ThemeChoice, WEIGHT_BOLD, WEIGHT_SEMIBOLD,
};
use crate::transport::State;
use crate::views::acp_form::AcpForm;
use crate::views::directory_picker::{DirectoryPicker, Picked};
use crate::views::kit::badge;
#[cfg(not(target_arch = "wasm32"))]
use crate::views::kit::{BtnTone, btn_sm};
use crate::views::mcp_form::{FormDone, McpForm};
use crate::views::provider_form::{ProviderForm, Target as ProviderTarget};
use crate::views::settings_config as config;

gpui_kit::actions!(kage_desktop, [SettingsClose]);

/// The width of a theme pick and its menu.
const PICK_W: f32 = 220.0;

/// How tall a theme pick menu grows before it scrolls.
const PICK_MENU_H: f32 = 280.0;

/// The pages of the settings dialog, in nav order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Theme and input behavior.
    General,
    /// The engine's agent, swarm and session defaults.
    Agents,
    /// The providers and their models.
    Providers,
    /// The MCP servers and their status.
    Mcp,
    /// The permission mode and the tool rules.
    Permissions,
    /// The plugin directory and grants.
    Plugins,
    /// The link to the engine.
    Connection,
    /// The shortcut table.
    Keyboard,
    /// Sessions taken out of the sidebar.
    Archived,
    /// The engine and protocol versions.
    About,
}

impl Section {
    /// Every section, in nav order.
    pub const ALL: [Section; 10] = [
        Section::General,
        Section::Agents,
        Section::Providers,
        Section::Mcp,
        Section::Permissions,
        Section::Plugins,
        Section::Connection,
        Section::Keyboard,
        Section::Archived,
        Section::About,
    ];

    /// The nav label and page title.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Agents => "Agent & Sessions",
            Section::Providers => "Model Providers",
            Section::Mcp => "MCP Servers",
            Section::Permissions => "Permissions",
            Section::Plugins => "Plugins",
            Section::Connection => "Connection",
            Section::Keyboard => "Keyboard",
            Section::Archived => "Archived Sessions",
            Section::About => "About",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Section::General => IconName::Settings2,
            Section::Agents => IconName::Bot,
            Section::Providers => IconName::Zap,
            Section::Mcp => IconName::Server,
            Section::Permissions => IconName::ShieldCheck,
            Section::Plugins => IconName::LayoutDashboard,
            Section::Connection => IconName::Network,
            Section::Keyboard => IconName::Command,
            Section::Archived => IconName::Archive,
            Section::About => IconName::Info,
        }
    }
}

/// One row of the shortcut table: what it does and its keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    /// What the keys do.
    pub label: &'static str,
    /// The keys, as the key caps read.
    pub keys: String,
}

/// The label of an app action the shortcut table lists, by its name.
fn action_label(name: &str) -> Option<&'static str> {
    Some(match name.rsplit("::").next()? {
        "OpenPalette" => "Command palette",
        "NewSession" => "New session",
        "ToggleSidebar" => "Toggle sidebar",
        "ToggleWorkbench" => "Toggle workbench",
        "OpenSettings" => "Settings",
        "OpenFind" => "Find in chat",
        "SendPrompt" => "Send, or steer the running turn",
        "CycleMode" => "Cycle permission mode",
        "Quit" => "Quit",
        _ => return None,
    })
}

/// A keystroke as key caps: `ctrl-k` reads `Ctrl K`.
fn key_caps(binding: &KeyBinding) -> String {
    binding
        .keystrokes()
        .iter()
        .map(|stroke| {
            let mut caps: Vec<String> = Vec::new();
            let m = stroke.modifiers();
            if m.control {
                caps.push("Ctrl".into());
            }
            if m.platform {
                caps.push("Cmd".into());
            }
            if m.alt {
                caps.push("Alt".into());
            }
            if m.shift {
                caps.push("Shift".into());
            }
            let key = stroke.key();
            let mut chars = key.chars();
            caps.push(match (chars.next(), chars.next()) {
                (Some(c), None) => c.to_uppercase().collect(),
                _ => {
                    let mut word = key.to_owned();
                    if let Some(first) = word.get_mut(0..1) {
                        first.make_ascii_uppercase();
                    }
                    word
                }
            });
            caps.join(" ")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The shortcut table of `bindings`: one row per labeled action, with
/// the platform's modifier only, in binding order.
#[must_use]
pub fn shortcuts(bindings: &[KeyBinding]) -> Vec<Shortcut> {
    let mac = cfg!(target_os = "macos");
    let uses = |binding: &KeyBinding, cmd: bool| {
        binding.keystrokes().iter().any(|stroke| {
            if cmd {
                stroke.modifiers().platform
            } else {
                stroke.modifiers().control
            }
        })
    };
    let mut rows: Vec<Shortcut> = Vec::new();
    for binding in bindings {
        let Some(label) = action_label(binding.action().name()) else {
            continue;
        };
        // Each platform reads its own modifier: Cmd on a Mac where a Cmd
        // binding exists, Ctrl everywhere else.
        let has_cmd = bindings
            .iter()
            .any(|other| other.action().name() == binding.action().name() && uses(other, true));
        if (!mac && uses(binding, true)) || (mac && has_cmd && uses(binding, false)) {
            continue;
        }
        if rows.iter().any(|row| row.label == label) {
            continue;
        }
        rows.push(Shortcut {
            label,
            keys: key_caps(binding),
        });
    }
    rows
}

/// The settings dialog over the shell.
pub struct SettingsView {
    /// Where the focus goes when this closes.
    focus_return: crate::views::kit::FocusReturn,
    /// The open theme pick menu: `Some(true)` for the dark desktop's.
    pick_open: Option<bool>,
    /// Where each theme pick trigger last laid out, dark first, so its
    /// menu hangs below it.
    pick_bounds: [Rc<Cell<Option<Bounds<Pixels>>>>; 2],
    store: Entity<Store>,
    open: bool,
    section: Section,
    focus: FocusHandle,
    /// The plugin whose capabilities show on the Plugins page.
    plugin_open: Option<String>,
    /// What is being added to the permission rules, while the rule
    /// field shows.
    rule_add: Option<config::RuleAdd>,
    /// The text field a rule's glob or tool name is typed into, made
    /// the first time one is added.
    rule_input: Option<Entity<InputState>>,
    /// The MCP server form, while one is open.
    mcp_form: Option<Entity<McpForm>>,
    /// Whether the Providers page shows the providers to add from.
    provider_choosing: bool,
    /// The provider form, while one is open.
    provider_form: Option<Entity<ProviderForm>>,
    /// The ACP agent form, while one is open.
    acp_form: Option<Entity<AcpForm>>,
    /// The provider directory picker, while one is open.
    directory: Option<Entity<DirectoryPicker>>,
    /// The plugin install field, while it shows.
    plugin_input: Option<Entity<InputState>>,
    /// The last plugin install sent.
    plugin_install: Option<u64>,
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SettingsView {
    /// A closed settings dialog over `store`.
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            focus_return: crate::views::kit::FocusReturn::default(),
            pick_open: None,
            pick_bounds: Default::default(),
            store,
            open: false,
            section: Section::General,
            focus: cx.focus_handle(),
            plugin_open: None,
            rule_add: None,
            rule_input: None,
            mcp_form: None,
            provider_choosing: false,
            provider_form: None,
            acp_form: None,
            directory: None,
            plugin_input: None,
            plugin_install: None,
        }
    }

    /// Whether the dialog shows.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Shows the dialog on `section`, taking the focus.
    pub fn open(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.store.act(cx, |store| {
            store.ask_themes();
            store.ask_config();
        });
        self.go(section, cx);
        self.focus_return.remember(window, cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Follows a click on the Providers page: to the providers to add
    /// from, back to the list, or into a provider's form.
    fn open_provider(
        &mut self,
        nav: config::ProviderNav,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match nav {
            config::ProviderNav::Choose | config::ProviderNav::Back => {
                self.provider_choosing = nav == config::ProviderNav::Choose;
                cx.notify();
                return;
            }
            config::ProviderNav::Open(target) => target,
            config::ProviderNav::Acp(name) => {
                self.open_acp(name, window, cx);
                return;
            }
            config::ProviderNav::Directory(named) => {
                let store = self.store.clone();
                let picker = cx.new(|cx| DirectoryPicker::new(store, named, window, cx));
                cx.subscribe(&picker, |this, _, _: &FormDone, cx| {
                    this.directory = None;
                    cx.notify();
                })
                .detach();
                cx.subscribe_in(&picker, window, |this, _, picked: &Picked, window, cx| {
                    this.directory = None;
                    let target = ProviderTarget::Import {
                        id: picked.id.clone(),
                        entry: picked.entry.clone(),
                    };
                    this.open_provider(config::ProviderNav::Open(target), window, cx);
                })
                .detach();
                self.directory = Some(picker);
                cx.notify();
                return;
            }
        };
        let config = self.store.read(cx).config().cloned().unwrap_or_default();
        let store = self.store.clone();
        let form = cx.new(|cx| ProviderForm::new(store, target, &config, window, cx));
        cx.subscribe(&form, |this, _, _: &FormDone, cx| {
            this.provider_form = None;
            this.provider_choosing = false;
            cx.notify();
        })
        .detach();
        self.provider_form = Some(form);
        cx.notify();
    }

    /// Shows the plugin install field, or hides it.
    fn show_plugin_input(&mut self, show: bool, window: &mut Window, cx: &mut Context<Self>) {
        if show {
            // A new field each time: one not shown yet cannot have its
            // text set on the web.
            let input = cx
                .new(|cx| InputState::new(window, cx).placeholder("https://example.com/clock.lua"));
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.install_plugin(window, cx);
                }
            })
            .detach();
            input.update(cx, |state, cx| state.focus(window, cx));
            self.plugin_input = Some(input);
            self.plugin_install = None;
        } else {
            self.plugin_input = None;
        }
        cx.notify();
    }

    /// Installs the plugin the install field names.
    fn install_plugin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.plugin_input else {
            return;
        };
        let source = input.read(cx).value().trim().to_owned();
        if source.is_empty() {
            return;
        }
        let id = self.store.update(cx, |store, cx| {
            cx.notify();
            store.install_plugin(&source, false)
        });
        self.plugin_install = Some(id);
        self.show_plugin_input(false, window, cx);
    }

    /// Opens the ACP agent form on agent `name`, or on a new agent.
    fn open_acp(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self
            .store
            .read(cx)
            .config()
            .map(config::Snapshot::parse)
            .unwrap_or_default();
        let existing = name
            .as_deref()
            .and_then(|name| Some((name, snapshot.acp_agent(name)?)));
        let store = self.store.clone();
        let form = cx.new(|cx| AcpForm::new(store, existing, window, cx));
        cx.subscribe(&form, |this, _, _: &FormDone, cx| {
            this.acp_form = None;
            this.provider_choosing = false;
            cx.notify();
        })
        .detach();
        self.acp_form = Some(form);
        cx.notify();
    }

    /// Opens the MCP server form on server `name`, or on a new server.
    fn edit_mcp(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self
            .store
            .read(cx)
            .config()
            .map(config::Snapshot::parse)
            .unwrap_or_default();
        let existing = name
            .as_deref()
            .and_then(|name| Some((name, snapshot.mcp_server(name)?)));
        let store = self.store.clone();
        let form = cx.new(|cx| McpForm::new(store, existing, window, cx));
        cx.subscribe(&form, |this, _, _: &FormDone, cx| {
            this.mcp_form = None;
            cx.notify();
        })
        .detach();
        self.mcp_form = Some(form);
        cx.notify();
    }

    /// Starts adding `add` to the permission rules in the rule field,
    /// or stops with `None`.
    fn add_rule(
        &mut self,
        add: Option<config::RuleAdd>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rule_add = add;
        if self.rule_add.is_some() {
            // A new field starts empty. Clearing one before its first
            // render would lay its text out in the default font, which
            // the web cannot resolve.
            if let Some(input) = &self.rule_input {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            } else {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("glob or tool"));
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::PressEnter { .. } => this.commit_rule(window, cx),
                        InputEvent::Blur => this.add_rule(None, window, cx),
                        _ => {}
                    }
                })
                .detach();
                self.rule_input = Some(input);
            }
            if let Some(input) = &self.rule_input {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
        }
        cx.notify();
    }

    /// Writes what the rule field holds and closes it.
    fn commit_rule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(add), Some(input)) = (self.rule_add.clone(), self.rule_input.clone()) else {
            return;
        };
        let text = input.read(cx).value().to_string();
        let added = self
            .store
            .read(cx)
            .config()
            .and_then(|config| config::rule_added(config, &add, &text));
        if let Some((path, value)) = added {
            self.store.update(cx, |store, cx| {
                let path: Vec<&str> = path.iter().map(String::as_str).collect();
                store.config_set(&path, Some(value));
                cx.notify();
            });
        }
        self.add_rule(None, window, cx);
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.focus_return.restore(cx);
        cx.notify();
    }

    /// Shows `section`. A page read from the engine's configuration
    /// asks for a fresh snapshot, so an edit to config.toml shows on the
    /// next visit.
    fn go(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        self.mcp_form = None;
        self.provider_choosing = false;
        self.provider_form = None;
        self.acp_form = None;
        self.directory = None;
        if matches!(
            section,
            Section::Providers | Section::Mcp | Section::Permissions | Section::Plugins
        ) {
            self.store.act(cx, Store::ask_config);
        }
        if section == Section::Agents {
            self.store.act(cx, Store::ask_engine_options);
        }
        cx.notify();
    }

    /// Applies and stores a theme choice.
    fn choose_theme(&mut self, choice: ThemeChoice, window: &mut Window, cx: &mut Context<Self>) {
        let stored = choice.clone();
        self.store
            .act(cx, |store| store.update_prefs(|prefs| prefs.theme = stored));
        cx.set_global(choice);
        crate::theme::apply_choice(cx, window.appearance());
        window.refresh();
    }

    /// Flips one boolean preference.
    fn flip(&self, cx: &mut App, field: fn(&mut Prefs) -> &mut bool) {
        self.store.act(cx, |store| {
            store.update_prefs(|prefs| {
                let value = field(prefs);
                *value = !*value;
            });
        });
    }

    fn nav(&self, pal: &Palette, cx: &Context<Self>) -> Div {
        let mut nav = v_flex()
            .w(px(224.))
            .flex_none()
            .h_full()
            .gap(px(2.))
            .px(px(10.))
            .py(px(16.))
            .border_r_1()
            .border_color(pal.line)
            .bg(pal.sidebar)
            .child(
                div()
                    .px(px(10.))
                    .pt(px(4.))
                    .pb(px(14.))
                    .font_weight(WEIGHT_BOLD)
                    .text_size(px(16.))
                    .text_color(pal.ink_strong)
                    .child("Settings"),
            );
        for section in Section::ALL {
            let on = section == self.section;
            let hover = pal.hover;
            nav = nav.child(
                h_flex()
                    .id(SharedString::from(format!("set-nav-{}", section.label())))
                    .h(px(36.))
                    .px(px(10.))
                    .gap(px(10.))
                    .items_center()
                    .rounded(px(R_MD))
                    .text_size(px(FS_SM))
                    .text_color(if on { pal.ink_strong } else { pal.muted })
                    .when(on, |row| row.bg(pal.selected))
                    .when(!on, |row| row.hover(move |row| row.bg(hover)))
                    .on_click(cx.listener(move |this, _, _, cx| this.go(section, cx)))
                    .child(Icon::new(section.icon()).with_size(px(14.)))
                    .child(section.label()),
            );
        }
        nav
    }

    fn page(&self, window: &Window, cx: &Context<Self>) -> Vec<AnyElement> {
        let pal = Palette::active(cx);
        let mut out = vec![title(self.section.label(), pal).into_any_element()];
        match self.section {
            Section::General => out.extend(self.general(window, pal, cx)),
            Section::Agents => out.extend(self.agents(pal, cx)),
            Section::Providers | Section::Mcp | Section::Permissions | Section::Plugins => {
                out.extend(self.config_page(pal, cx));
            }
            Section::Connection => out.extend(self.connection(pal, cx)),
            Section::Keyboard => out.extend(self.keyboard(pal, cx)),
            Section::Archived => out.extend(self.archived(pal, cx)),
            Section::About => out.extend(self.about(pal, cx)),
        }
        out
    }

    fn general(&self, window: &Window, pal: &Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let prefs = self.store.read(cx).prefs().clone();
        let mut out = self.appearance(window, &prefs, pal, cx);
        out.extend([
            group("Input", pal).into_any_element(),
            boxed(pal)
                .child(row(
                    "Vim mode",
                    "Normal-mode motions over the transcript (j k gg G za / n), a : command line and a modeline. Esc leaves the composer.",
                    self.toggle("set-vim", prefs.vim, |p| &mut p.vim, pal, cx),
                    pal,
                ))
                .child(row(
                    "Enter sends",
                    "Off: Enter adds a newline and Ctrl+Enter sends",
                    self.toggle(
                        "set-enter",
                        prefs.enter_sends,
                        |p| &mut p.enter_sends,
                        pal,
                        cx,
                    ),
                    pal,
                ))
                .into_any_element(),
            group("Chat", pal).into_any_element(),
            self.chat_rows(&prefs, pal, cx).into_any_element(),
            group("Sessions", pal).into_any_element(),
            boxed(pal)
                .child(row(
                    "Group sessions by project",
                    "Show sessions under their project folder in the sidebar",
                    self.toggle(
                        "set-group",
                        prefs.group_by_project,
                        |p| &mut p.group_by_project,
                        pal,
                        cx,
                    ),
                    pal,
                ))
                .child(row(
                    "Ask before enabling swarm",
                    "Confirm when turning swarm mode on",
                    self.toggle(
                        "set-swarm",
                        prefs.confirm_swarm,
                        |p| &mut p.confirm_swarm,
                        pal,
                        cx,
                    ),
                    pal,
                ))
                .into_any_element(),
        ]);
        out
    }

    /// The theme cards, then the themes a System choice draws on a dark
    /// and a light desktop.
    fn appearance(
        &self,
        window: &Window,
        prefs: &Prefs,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let light = matches!(
            window.appearance(),
            gpui_kit::WindowAppearance::Light | gpui_kit::WindowAppearance::VibrantLight
        );
        let catalog = cx
            .try_global::<crate::themes::Catalog>()
            .cloned()
            .unwrap_or_default();
        let resolve = |name: &str, light: bool| {
            crate::themes::resolve(name, &catalog, light).map(|(palette, _)| palette)
        };
        let mut cards = h_flex().flex_wrap().gap(px(10.));
        let choices = std::iter::once(ThemeChoice::System)
            .chain(catalog.names().into_iter().map(ThemeChoice::Named));
        for choice in choices {
            let swatch = match &choice {
                ThemeChoice::System => {
                    let (Some(dark), Some(day)) = (
                        resolve(&catalog.system(false), false),
                        resolve(&catalog.system(true), true),
                    ) else {
                        continue;
                    };
                    let accent = if light { day.accent } else { dark.accent };
                    (dark.bg, day.bg, accent)
                }
                ThemeChoice::Named(name) => {
                    let Some(palette) = resolve(name, light) else {
                        continue;
                    };
                    (palette.bg, palette.surface, palette.accent)
                }
            };
            cards = cards.child(self.theme_card(choice, swatch, prefs, pal, cx));
        }
        vec![
            group("Appearance", pal).into_any_element(),
            cards.into_any_element(),
            boxed(pal)
                .mt(px(10.))
                .child(row_el(
                    "System on a dark desktop",
                    "The theme System draws when the desktop is dark".into(),
                    self.system_picks(true, &catalog, Palette::active(cx), cx),
                    pal,
                ))
                .child(row_el(
                    "System on a light desktop",
                    "The theme System draws when the desktop is light".into(),
                    self.system_picks(false, &catalog, Palette::active(cx), cx),
                    pal,
                ))
                .into_any_element(),
        ]
    }

    /// One theme card: a swatch of its side, main and accent colors over
    /// its name, outlined when chosen.
    fn theme_card(
        &self,
        choice: ThemeChoice,
        (side, main, accent): (Hsla, Hsla, Hsla),
        prefs: &Prefs,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let on = prefs.theme == choice;
        let line_strong = pal.line_strong;
        let label = choice.label();
        v_flex()
            .id(SharedString::from(format!("theme-{label}")))
            .w(px(150.))
            .p(px(8.))
            .gap(px(8.))
            .rounded(px(R_LG))
            .border_1()
            .border_color(if on { pal.accent } else { pal.line })
            .bg(pal.surface)
            .when(!on, |card| {
                card.hover(move |card| card.border_color(line_strong))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.choose_theme(choice.clone(), window, cx);
            }))
            .child(
                h_flex()
                    .h(px(58.))
                    .rounded(px(R_MD))
                    .overflow_hidden()
                    .child(div().w(gpui_kit::relative(0.28)).h_full().bg(side))
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .bg(main)
                            .flex()
                            .items_end()
                            .p(px(8.))
                            .child(
                                div()
                                    .w(gpui_kit::relative(0.4))
                                    .h(px(8.))
                                    .rounded(px(4.))
                                    .bg(accent),
                            ),
                    ),
            )
            .child(
                div()
                    .px(px(2.))
                    .truncate()
                    .text_size(px(FS_XS))
                    .text_color(pal.ink)
                    .child(label),
            )
    }

    /// The pick that names the theme System draws on a dark or a light
    /// desktop: a trigger showing the current theme and, while open, a
    /// menu of every theme with a swatch of its colors. The kage theme
    /// of that mode clears the pick.
    fn system_picks(
        &self,
        dark: bool,
        catalog: &crate::themes::Catalog,
        pal: &'static Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let current = catalog.system(!dark);
        let open = self.pick_open == Some(dark);
        let bounds = self.pick_bounds[usize::from(!dark)].clone();
        let palette =
            |name: &str| crate::themes::resolve(name, catalog, !dark).map(|(palette, _)| palette);
        let line_strong = pal.line_strong;
        let mut trigger = h_flex()
            .id(if dark { "system-dark" } else { "system-light" })
            .w(px(PICK_W))
            .h(px(32.))
            .px(px(10.))
            .gap(px(9.))
            .items_center()
            .rounded(px(R_MD))
            .border_1()
            .border_color(if open { pal.accent } else { pal.line })
            .bg(pal.surface)
            .cursor_pointer()
            .when(!open, |row| {
                row.hover(move |row| row.border_color(line_strong))
            })
            .on_prepaint(move |at, _, _| bounds.set(Some(at)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.pick_open = (this.pick_open != Some(dark)).then_some(dark);
                cx.notify();
            }));
        if let Some(theme) = palette(&current) {
            trigger = trigger.child(swatch(theme, pal));
        }
        let trigger = trigger
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(FS_SM))
                    .text_color(pal.ink)
                    .child(crate::themes::label(&current)),
            )
            .child(
                Icon::new(IconName::ChevronDown)
                    .with_size(px(12.))
                    .text_color(pal.faint),
            );
        let menu = self.pick_bounds[usize::from(!dark)]
            .get()
            .filter(|_| open)
            .map(|at| self.pick_menu(dark, catalog, at, pal, cx));
        div().child(trigger).children(menu)
    }

    /// The open menu of a theme pick, hung below its trigger at `at`.
    fn pick_menu(
        &self,
        dark: bool,
        catalog: &crate::themes::Catalog,
        at: Bounds<Pixels>,
        pal: &'static Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let current = catalog.system(!dark);
        let kage = if dark { "kage-shadow" } else { "kage-dawn" };
        let mut rows = v_flex()
            .id(if dark {
                "pick-menu-dark"
            } else {
                "pick-menu-light"
            })
            .max_h(px(PICK_MENU_H))
            .overflow_y_scroll()
            .p(px(4.))
            .gap(px(1.));
        for name in catalog.names() {
            let Some((theme, _)) = crate::themes::resolve(&name, catalog, !dark) else {
                continue;
            };
            let on = name == current;
            let pick = (name != kage).then(|| name.clone());
            let hover = pal.accent_soft;
            rows = rows.child(
                h_flex()
                    .id(SharedString::from(format!("pick-{dark}-{name}")))
                    .h(px(32.))
                    .px(px(8.))
                    .gap(px(9.))
                    .items_center()
                    .rounded(px(R_MD))
                    .cursor_pointer()
                    .when(on, |row| row.bg(pal.selected))
                    .hover(move |row| row.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_open = None;
                        let pick = pick.clone();
                        this.store
                            .act(cx, |store| store.set_system_theme(dark, pick.as_deref()));
                    }))
                    .child(swatch(theme, pal))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(FS_SM))
                            .text_color(if on { pal.ink_strong } else { pal.ink })
                            .child(crate::themes::label(&name)),
                    )
                    .when(on, |row| {
                        row.child(
                            Icon::new(IconName::Check)
                                .with_size(px(13.))
                                .text_color(pal.accent),
                        )
                    }),
            );
        }
        let close = cx.entity();
        let mut surface = div()
            .occlude()
            .w(px(PICK_W))
            .bg(pal.bg)
            .border_1()
            .border_color(pal.line)
            .rounded(px(R_LG))
            .overflow_hidden()
            .on_mouse_down_out(move |event, _, cx| {
                if at.contains(&event.position) {
                    return;
                }
                close.update(cx, |this, cx| {
                    this.pick_open = None;
                    cx.notify();
                });
            })
            .child(rows);
        surface.style().box_shadow = Some(pal.shadow_menu.clone());
        // Above the settings overlay, which is deferred at priority 2.
        gpui_kit::deferred(
            gpui_kit::anchored()
                .anchor(gpui_kit::Anchor::TopLeft)
                .position(gpui_kit::point(at.left(), at.bottom() + px(4.)))
                .snap_to_window_with_margin(px(8.))
                .child(surface),
        )
        .with_priority(3)
    }

    /// One of the pages the configuration snapshot backs.
    fn config_page(&self, pal: &'static Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let store = self.store.read(cx);
        let Some(config) = store.config() else {
            return config::waiting(pal);
        };
        let snapshot = config::Snapshot::parse(config);
        let session = store.active_session().or_else(|| {
            store
                .state()
                .sessions
                .values()
                .find(|session| session.opened && session.parent.is_none())
        });
        let option = |id: &str| {
            session.and_then(|session| session.config_options.iter().find(|o| o.id == id))
        };
        match self.section {
            Section::Providers => {
                if let Some(form) = &self.provider_form {
                    return vec![form.clone().into_any_element()];
                }
                if let Some(form) = &self.acp_form {
                    return vec![form.clone().into_any_element()];
                }
                if let Some(picker) = &self.directory {
                    return vec![picker.clone().into_any_element()];
                }
                let view = cx.entity();
                config::providers_page(
                    &snapshot,
                    option("model"),
                    self.provider_choosing,
                    move |target, window, cx| {
                        view.update(cx, |this, cx| this.open_provider(target, window, cx));
                    },
                    pal,
                )
            }
            Section::Mcp => {
                let live = session
                    .map(|session| session.mcp.clone())
                    .unwrap_or_default();
                if let Some(form) = &self.mcp_form {
                    return vec![form.clone().into_any_element()];
                }
                let view = cx.entity();
                config::mcp_page(
                    &snapshot,
                    &live,
                    move |name, window, cx| {
                        view.update(cx, |this, cx| this.edit_mcp(name, window, cx));
                    },
                    pal,
                )
            }
            Section::Permissions => {
                let active = store.active_session();
                let modes = active
                    .and_then(|session| session.config_options.iter().find(|o| o.id == "mode"))
                    .map(|option| {
                        let mut option = option.clone();
                        option
                            .options
                            .retain(|choice| choice.value != crate::views::composer::PLAN_MODE);
                        option
                    });
                let current = store.permission_mode();
                let handle = self.store.clone();
                let view = cx.entity();
                let edits = config::RuleEdits {
                    store: self.store.clone(),
                    adding: self.rule_add.clone(),
                    input: self.rule_input.clone(),
                    on_add: std::rc::Rc::new(move |add, window, cx| {
                        view.update(cx, |this, cx| this.add_rule(add, window, cx));
                    }),
                };
                config::permissions_page(
                    &snapshot,
                    modes.as_ref(),
                    current.as_deref(),
                    move |value, cx| {
                        handle.act(cx, |store| store.set_permission(&value));
                    },
                    &edits,
                    pal,
                )
            }
            _ => {
                let view = cx.entity();
                let (toggle, submit) = (view.clone(), view.clone());
                let install = config::PluginInstall {
                    input: self.plugin_input.clone(),
                    status: self
                        .plugin_install
                        .map(|id| store.write_outcome(id).cloned()),
                    on_toggle: std::rc::Rc::new(move |show, window, cx| {
                        toggle.update(cx, |this, cx| this.show_plugin_input(show, window, cx));
                    }),
                    on_submit: std::rc::Rc::new(move |window, cx| {
                        submit.update(cx, |this, cx| this.install_plugin(window, cx));
                    }),
                };
                config::plugins_page(
                    &snapshot,
                    &self.store,
                    self.plugin_open.as_deref(),
                    move |name, cx| {
                        view.update(cx, |this, cx| {
                            this.plugin_open = name;
                            this.plugin_install = None;
                            cx.notify();
                        });
                    },
                    &install,
                    pal,
                )
            }
        }
    }

    fn connection(&self, pal: &Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let store = self.store.read(cx);
        let link = store.link();
        let state = store.state();
        let (word, dot) = match store.connect() {
            State::Connected => ("Connected", pal.ok),
            State::Connecting => ("Connecting", pal.warn),
            State::Reconnecting { .. } => ("Reconnecting", pal.warn),
            State::Refused(_) => ("Refused", pal.danger),
            State::Closed => ("Closed", pal.faint),
        };
        let version = state
            .agent
            .as_ref()
            .and_then(|agent| agent.version.clone())
            .map_or_else(|| "unknown".to_owned(), |v| format!("kage {v}"));
        let mut features: Vec<&str> = Vec::new();
        if state.steer_available() {
            features.push("steer");
        }
        if state
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.load_session)
        {
            features.push("session/load");
        }
        let detail = format!(
            "{} \u{b7} {version} \u{b7} {} sessions{}",
            link.detail,
            state.sessions.values().filter(|s| s.opened).count(),
            if features.is_empty() {
                String::new()
            } else {
                format!(" \u{b7} {}", features.join(", "))
            }
        );
        vec![
            group("Transport", pal).into_any_element(),
            boxed(pal)
                .child(row(
                    "Connect with",
                    "Chosen when the client starts: the desktop app spawns kage rpc or dials a server, the browser attaches to the kage serve it came from",
                    badge(link.name, pal),
                    pal,
                ))
                .when(link.name == "kage rpc", |rows| {
                    let binary = store
                        .prefs()
                        .kage_path
                        .clone()
                        .unwrap_or_else(|| "kage from the PATH".to_owned());
                    rows.child(row(
                        "kage binary",
                        "The engine this app spawns; the setup screen picks another when none runs",
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(FS_XS))
                            .text_color(pal.ink)
                            .child(SharedString::from(binary)),
                        pal,
                    ))
                })
                .child(row(
                    "Endpoint",
                    "The token rides the kage.<token> subprotocol and is never shown",
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(FS_XS))
                        .text_color(pal.ink)
                        .child(SharedString::from(link.detail.clone())),
                    pal,
                ))
                .into_any_element(),
            group("Status", pal).into_any_element(),
            boxed(pal)
                .child(row_el(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(div().size(px(8.)).rounded(px(R_FULL)).bg(dot))
                        .child(word),
                    SharedString::from(detail),
                    div(),
                    pal,
                ))
                .into_any_element(),
        ]
    }

    fn keyboard(&self, pal: &Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let enter_sends = self.store.read(cx).prefs().enter_sends;
        let composer: Vec<(&str, &str)> = if enter_sends {
            vec![
                ("Send", "Enter"),
                ("Newline", "Shift Enter"),
                ("Queue while running", "Enter"),
            ]
        } else {
            vec![
                ("Send", "Ctrl Enter"),
                ("Newline", "Enter"),
                ("Queue while running", "Ctrl Enter"),
            ]
        };
        let mut composer = composer;
        composer.push(("Interrupt", "Esc Esc"));
        composer.push(("Answer approval", "1 2 3"));
        let table = |rows: Vec<(String, String)>| {
            let mut table = boxed(pal);
            for (label, keys) in rows {
                table = table.child(
                    h_flex()
                        .px(px(16.))
                        .py(px(8.))
                        .border_t_1()
                        .border_color(pal.subtle)
                        .text_size(px(FS_SM))
                        .child(div().flex_1().text_color(pal.ink).child(label))
                        .child(
                            h_flex()
                                .gap(px(4.))
                                .children(keys.split(' ').map(|cap| key_cap(cap.to_owned(), pal))),
                        ),
                );
            }
            table
        };
        let app: Vec<(String, String)> = shortcuts(&crate::app::key_bindings())
            .into_iter()
            .map(|row| (row.label.to_owned(), row.keys))
            .collect();
        vec![
            group("Shortcuts", pal).into_any_element(),
            table(app).into_any_element(),
            group("Composer", pal).into_any_element(),
            table(
                composer
                    .into_iter()
                    .map(|(a, b)| (a.to_owned(), b.to_owned()))
                    .collect(),
            )
            .into_any_element(),
            group(
                if self.store.read(cx).prefs().vim {
                    "Vim mode (on)"
                } else {
                    "Vim mode (off; turn it on under General)"
                },
                pal,
            )
            .into_any_element(),
            table(
                [
                    ("Leave the composer", "Esc"),
                    ("Move between rows", "j k"),
                    ("First row", "g g"),
                    ("Last row", "Shift G"),
                    ("Half a page", "Ctrl D Ctrl U"),
                    ("Open or close a row", "z a"),
                    ("Open all, close all", "z R z M"),
                    ("Find, next, previous", "/ n N"),
                    ("Back to the composer", "i"),
                    ("Command line", ":"),
                ]
                .into_iter()
                .map(|(a, b)| (a.to_owned(), b.to_owned()))
                .collect(),
            )
            .into_any_element(),
            note(
                "Commands for the : line: theme <name>, model <name>, swarm on|off, plan on|off, goal <text>, new, settings, compact, noh, q, vim off.",
                pal,
            )
            .into_any_element(),
        ]
    }

    /// The engine's agent, swarm and session defaults, written into the
    /// user `config.toml` through the engine.
    fn agents(&self, pal: &Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(options) = self.store.read(cx).engine_options() else {
            return vec![note("Reading the engine's options\u{2026}", pal).into_any_element()];
        };
        let find = |name: &str| options.iter().find(|option| option.name == name);
        let group_box = |title: &'static str, rows: Vec<AnyElement>| {
            vec![
                group(title, pal).into_any_element(),
                boxed(pal).children(rows).into_any_element(),
            ]
        };
        let mut out = Vec::new();
        let defaults: Vec<AnyElement> = [
            ("thinking_level", "Thinking by default"),
            ("compaction_threshold", "Compact at"),
        ]
        .into_iter()
        .filter_map(|(name, label)| find(name).map(|option| self.option_row(label, option, pal)))
        .collect();
        out.extend(group_box("Session defaults", defaults));
        let agents: Vec<AnyElement> = [
            ("agent_max_depth", "Max depth"),
            ("agent_max_running", "Max running"),
            ("agent_max_turns", "Turn limit"),
            ("agent_timeout", "Run timeout"),
            ("agent_budget", "Token budget"),
        ]
        .into_iter()
        .filter_map(|(name, label)| find(name).map(|option| self.option_row(label, option, pal)))
        .collect();
        out.extend(group_box("Subagents", agents));
        let swarm: Vec<AnyElement> = [
            ("swarm_max_items", "Max items per swarm"),
            ("swarm_timeout_ms", "Worker timeout"),
        ]
        .into_iter()
        .filter_map(|(name, label)| find(name).map(|option| self.option_row(label, option, pal)))
        .collect();
        out.extend(group_box("Swarm", swarm));
        out.push(
            note(
                "These live in the user config.toml; a project config that sets the same key still wins. Changes apply to sessions started afterwards.",
                pal,
            )
            .into_any_element(),
        );
        out
    }

    /// One engine option as a labeled row with the control its kind
    /// takes: a stepper for numbers, segments for a choice.
    fn option_row(
        &self,
        label: &'static str,
        option: &kage_client::wire::OptionEntry,
        pal: &Palette,
    ) -> AnyElement {
        let store = self.store.clone();
        let name = option.name.clone();
        let hint = SharedString::from(option.doc.clone());
        let control = match option.kind.as_str() {
            "choice" => {
                let current = option.value.as_str().unwrap_or_default().to_owned();
                let mut seg = h_flex()
                    .p(px(2.))
                    .gap(px(2.))
                    .rounded(px(R_MD))
                    .bg(pal.fill);
                for value in &option.values {
                    let on = *value == current;
                    let store = store.clone();
                    let name = name.clone();
                    let chosen = value.clone();
                    let label = match value.as_str() {
                        "" => "Auto".to_owned(),
                        "xhigh" => "Max".to_owned(),
                        other => {
                            let mut word = other.to_owned();
                            if let Some(first) = word.get_mut(0..1) {
                                first.make_ascii_uppercase();
                            }
                            word
                        }
                    };
                    seg = seg.child(
                        div()
                            .id(SharedString::from(format!("opt-{name}-{value}")))
                            .h(px(24.))
                            .px(px(9.))
                            .flex()
                            .items_center()
                            .rounded(px(6.))
                            .text_size(px(FS_XS))
                            .text_color(if on { pal.ink_strong } else { pal.muted })
                            .when(on, |seg| seg.bg(pal.raised))
                            .on_click(move |_, _, cx| {
                                let value = serde_json::Value::from(chosen.as_str());
                                store.act(cx, |store| store.set_engine_option(&name, value));
                            })
                            .child(label),
                    );
                }
                seg.into_any_element()
            }
            "int" | "fraction" => {
                let (step, shown) = match (option.kind.as_str(), name.as_str()) {
                    ("fraction", _) => (
                        0.05,
                        format!("{}%", (option.value.as_f64().unwrap_or(0.) * 100.).round()),
                    ),
                    (_, "swarm_timeout_ms") => (
                        60_000.,
                        format!("{} min", option.value.as_i64().unwrap_or(0) / 60_000),
                    ),
                    (_, "agent_max_turns" | "agent_timeout" | "agent_budget") => {
                        let value = option.value.as_i64().unwrap_or(0);
                        let (step, shown) = match name.as_str() {
                            "agent_max_turns" => (10., value.to_string()),
                            "agent_timeout" => (60., format!("{} min", value / 60)),
                            _ => (100_000., format!("{}k", value / 1000)),
                        };
                        (step, if value == 0 { "Off".to_owned() } else { shown })
                    }
                    _ => (1., option.value.as_i64().unwrap_or(0).to_string()),
                };
                let (min, max) = match option.kind.as_str() {
                    "fraction" => (0., 1.),
                    _ => (
                        option.min.unwrap_or(i64::MIN) as f64,
                        option.max.unwrap_or(i64::MAX) as f64,
                    ),
                };
                let current = option.value.as_f64().unwrap_or(0.);
                let fraction = option.kind == "fraction";
                let stepper = |id: &str, delta: f64, icon: IconName| {
                    let store = store.clone();
                    let name = name.clone();
                    let next = (current + delta).clamp(min, max);
                    let disabled = (next - current).abs() < f64::EPSILON;
                    div()
                        .id(SharedString::from(format!("opt-{name}-{id}")))
                        .size(px(26.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(R_MD))
                        .text_color(if disabled { pal.ghost } else { pal.muted })
                        .when(!disabled, |button| {
                            button.on_click(move |_, _, cx| {
                                let value = if fraction {
                                    serde_json::Value::from((next * 100.).round() / 100.)
                                } else {
                                    serde_json::Value::from(next as i64)
                                };
                                store.act(cx, |store| store.set_engine_option(&name, value));
                            })
                        })
                        .child(Icon::new(icon).with_size(px(12.)))
                };
                h_flex()
                    .items_center()
                    .gap(px(4.))
                    .rounded(px(R_MD))
                    .border_1()
                    .border_color(pal.line)
                    .bg(pal.bg)
                    .child(stepper("down", -step, IconName::Minus))
                    .child(
                        div()
                            .min_w(px(64.))
                            .flex()
                            .justify_center()
                            .font_family(FONT_MONO)
                            .text_size(px(FS_XS))
                            .text_color(pal.ink)
                            .child(shown),
                    )
                    .child(stepper("up", step, IconName::Plus))
                    .into_any_element()
            }
            _ => badge(SharedString::from(option.value.to_string()), pal).into_any_element(),
        };
        row_el(
            h_flex()
                .gap(px(8.))
                .child(label)
                .when(option.configured, |line| {
                    line.child(badge("set in config", pal))
                }),
            hint,
            control,
            pal,
        )
        .into_any_element()
    }

    /// The Chat group of the General page: how the transcript and the
    /// composer draw.
    fn chat_rows(&self, prefs: &Prefs, pal: &Palette, cx: &Context<Self>) -> Div {
        boxed(pal)
            .child(row(
                "Smooth streaming",
                "Show a reply that arrives in bursts at a steady pace, a moment behind",
                self.toggle(
                    "set-smooth",
                    prefs.smooth_stream,
                    |p| &mut p.smooth_stream,
                    pal,
                    cx,
                ),
                pal,
            ))
            .child(row(
                "Turn timeline rail",
                "A minimap beside the transcript with ticks for turns, edits, approvals, failures and swarms",
                self.toggle("set-rail", prefs.rail, |p| &mut p.rail, pal, cx),
                pal,
            ))
            .child(row(
                "Context gauge",
                "The context ring opens a gauge with Compact now",
                self.toggle("set-fuel", prefs.fuel, |p| &mut p.fuel, pal, cx),
                pal,
            ))
            .child(row(
                "Swarm constellation",
                "Swarm cards draw one star per worker, lit by state",
                self.toggle(
                    "set-constellation",
                    prefs.constellation,
                    |p| &mut p.constellation,
                    pal,
                    cx,
                ),
                pal,
            ))
    }

    fn archived(&self, pal: &Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let store = self.store.read(cx);
        let ids: Vec<String> = store.prefs().archived.iter().cloned().collect();
        if ids.is_empty() {
            return vec![
                v_flex()
                    .items_center()
                    .gap(px(8.))
                    .py(px(48.))
                    .text_size(px(FS_SM))
                    .text_color(pal.faint)
                    .child(Icon::new(IconName::Archive).with_size(px(18.)))
                    .child("Nothing archived. Archive a session from its \u{2026} menu.")
                    .into_any_element(),
            ];
        }
        let mut list = boxed(pal);
        for id in ids {
            let title = store
                .session_title(&id)
                .unwrap_or("untitled session")
                .to_owned();
            let project = store
                .state()
                .directory
                .iter()
                .find(|info| info.session_id == id)
                .map(|info| crate::app::project_name(Some(&info.cwd)));
            let restore = self.store.clone();
            let restored = id.clone();
            list = list.child(
                h_flex()
                    .px(px(16.))
                    .py(px(10.))
                    .gap(px(12.))
                    .items_center()
                    .border_t_1()
                    .border_color(pal.subtle)
                    .child(
                        Icon::new(IconName::Archive)
                            .with_size(px(14.))
                            .text_color(pal.faint),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(FS_SM))
                                    .text_color(pal.ink)
                                    .child(title),
                            )
                            .children(project.map(|project| {
                                div()
                                    .text_size(px(FS_XS))
                                    .text_color(pal.muted)
                                    .child(project)
                            })),
                    )
                    .child(
                        crate::views::kit::btn_sm(
                            format!("restore-{id}"),
                            crate::views::kit::BtnTone::Plain,
                            pal,
                        )
                        .on_click(move |_, _, cx| {
                            restore.act(cx, |store| store.restore(&restored));
                        })
                        .child("Restore"),
                    ),
            );
        }
        vec![
            note(
                "The archive is this client's own mark; the session stays recorded on the engine.",
                pal,
            )
            .into_any_element(),
            list.into_any_element(),
        ]
    }

    fn about(&self, pal: &Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let state = self.store.read(cx).state();
        let engine = state
            .agent
            .as_ref()
            .and_then(|agent| agent.version.clone())
            .map_or_else(|| "engine unknown".to_owned(), |v| format!("engine {v}"));
        let protocol = state
            .protocol_version
            .map_or_else(|| "unknown".to_owned(), |v| format!("ACP v{v}"));
        let header = h_flex()
            .gap(px(14.))
            .items_center()
            .mt(px(8.))
            .mb(px(18.))
            .child(crate::views::eclipse::eclipse(48., None, pal))
            .child(
                v_flex()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(WEIGHT_BOLD)
                            .text_color(pal.ink_strong)
                            .child("kage"),
                    )
                    .child(div().text_size(px(FS_SM)).text_color(pal.muted).child(
                        SharedString::from(format!(
                            "desktop {}{} \u{b7} {engine}",
                            env!("CARGO_PKG_VERSION"),
                            match option_env!("KAGE_BUILD_SHA") {
                                Some(sha) => format!(" ({})", &sha[..sha.len().min(7)]),
                                None => String::new(),
                            }
                        )),
                    )),
            );
        let rows = boxed(pal).child(row(
            "Protocol",
            "The Agent Client Protocol with the _kage/* extensions",
            badge(SharedString::from(protocol), pal),
            pal,
        ));
        #[cfg(not(target_arch = "wasm32"))]
        let rows = rows.child(self.updates(pal, cx));
        vec![header.into_any_element(), rows.into_any_element()]
    }

    /// The Updates row: what the last release check saw for the
    /// engine and this client, with a manual re-check and the page a
    /// newer build ships on.
    #[cfg(not(target_arch = "wasm32"))]
    fn updates(&self, pal: &Palette, cx: &Context<Self>) -> Div {
        let store = self.store.read(cx);
        let prefs = store.prefs();
        let engine = store
            .state()
            .agent
            .as_ref()
            .and_then(|agent| agent.version.clone());
        let desktop = env!("CARGO_PKG_VERSION");
        let nightly = prefs.latest_nightly.as_ref();
        let (engine_line, desktop_line) = if prefs.channel == crate::prefs::Channel::Nightly {
            let engine_line = match nightly {
                Some(build) => format!(
                    "engine: nightly {} ({}) published",
                    build.date,
                    &build.commit[..build.commit.len().min(7)]
                ),
                None => "engine: no nightly seen yet".to_owned(),
            };
            let desktop_line = match nightly {
                Some(build)
                    if option_env!("KAGE_BUILD_SHA").is_some_and(|sha| sha == build.commit) =>
                {
                    format!("desktop {desktop}; on the {} nightly", build.date)
                }
                Some(build) => format!("desktop {desktop}; nightly {} available", build.date),
                None => "desktop: no nightly seen yet".to_owned(),
            };
            (engine_line, desktop_line)
        } else {
            let engine_line = match (&prefs.latest_cli_version, &engine) {
                (Some(latest), Some(now)) if crate::update::is_newer(latest, now) => {
                    format!("engine {now}; {latest} is available")
                }
                (Some(latest), Some(now)) => format!("engine {now}; current with {latest}"),
                (Some(latest), None) => format!("engine version unknown; {latest} is available"),
                (None, _) => "engine: no check has run yet".to_owned(),
            };
            let desktop_line = match &prefs.latest_desktop_version {
                Some(latest) if crate::update::is_newer(latest, desktop) => {
                    format!("desktop {desktop}; {latest} is available")
                }
                Some(latest) => format!("desktop {desktop}; current with {latest}"),
                None => "desktop: no check has run yet".to_owned(),
            };
            (engine_line, desktop_line)
        };
        let view = cx.entity();
        let nightly = prefs.channel == crate::prefs::Channel::Nightly;
        row(
            "Updates",
            "The public releases, checked at most once a day",
            h_flex()
                .gap(px(12.))
                .items_center()
                .child(
                    v_flex()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(FS_XS))
                                .text_color(pal.ink)
                                .child(SharedString::from(engine_line)),
                        )
                        .child(
                            div()
                                .text_size(px(FS_XS))
                                .text_color(pal.muted)
                                .child(SharedString::from(desktop_line)),
                        ),
                )
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(
                            btn_sm("channel-stable", Self::channel_tone(!nightly), pal)
                                .on_click({
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        view.update(cx, |this, cx| {
                                            this.set_channel(crate::prefs::Channel::Latest, cx)
                                        });
                                    }
                                })
                                .child("Stable"),
                        )
                        .child(
                            btn_sm("channel-nightly", Self::channel_tone(nightly), pal)
                                .on_click({
                                    let view = view.clone();
                                    move |_, _, cx| {
                                        view.update(cx, |this, cx| {
                                            this.set_channel(crate::prefs::Channel::Nightly, cx)
                                        });
                                    }
                                })
                                .child("Nightly"),
                        )
                        .child(
                            btn_sm("about-check", BtnTone::Plain, pal)
                                .on_click(move |_, _, cx| {
                                    view.update(cx, |this, cx| this.check_updates(cx));
                                })
                                .child("Check now"),
                        )
                        .child(
                            btn_sm("about-releases", BtnTone::Plain, pal)
                                .on_click(|_, _, _| crate::update::open_releases())
                                .child(Icon::new(IconName::ExternalLink).with_size(px(12.)))
                                .child("Releases"),
                        ),
                ),
            pal,
        )
    }

    /// The active channel's button reads as filled, the other as
    /// plain.
    #[cfg(not(target_arch = "wasm32"))]
    fn channel_tone(active: bool) -> BtnTone {
        if active {
            BtnTone::Primary
        } else {
            BtnTone::Plain
        }
    }

    /// Follows `channel` from now on and checks it right away, so the
    /// row tells the new line's truth without a relaunch.
    #[cfg(not(target_arch = "wasm32"))]
    fn set_channel(&self, channel: crate::prefs::Channel, cx: &mut Context<Self>) {
        let store = self.store.clone();
        store.update(cx, |store, _| {
            store.update_prefs(|prefs| prefs.channel = channel);
        });
        self.check_updates(cx);
    }

    /// Runs a release check now; the row refreshes when it lands.
    #[cfg(not(target_arch = "wasm32"))]
    fn check_updates(&self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn(async move |_, cx| crate::update::run_check(store, cx, true).await)
            .detach();
    }

    fn toggle(
        &self,
        id: &'static str,
        on: bool,
        field: fn(&mut Prefs) -> &mut bool,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let view = cx.entity();
        crate::views::kit::switch(id, on, pal).on_click(move |_, _, cx| {
            view.update(cx, |this, cx| this.flip(cx, field));
        })
    }
}

fn title(text: &'static str, pal: &Palette) -> Div {
    div()
        .min_h(px(34.))
        .mr(px(40.))
        .mt(px(2.))
        .mb(px(6.))
        .flex()
        .items_center()
        .text_size(px(17.))
        .font_weight(WEIGHT_BOLD)
        .text_color(pal.ink_strong)
        .child(text)
}

/// A theme's colors in small: its sidebar beside its background, with
/// a dot of its accent.
fn swatch(theme: &Palette, pal: &Palette) -> Div {
    h_flex()
        .flex_none()
        .w(px(26.))
        .h(px(16.))
        .rounded(px(4.))
        .overflow_hidden()
        .border_1()
        .border_color(pal.line)
        .child(div().w(px(9.)).h_full().bg(theme.sidebar))
        .child(
            div()
                .flex_1()
                .h_full()
                .bg(theme.bg)
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(6.)).rounded(px(3.)).bg(theme.accent)),
        )
}

fn group(text: &'static str, pal: &Palette) -> Div {
    div()
        .mt(px(22.))
        .mb(px(8.))
        .text_size(px(FS_SM))
        .font_weight(WEIGHT_SEMIBOLD)
        .text_color(pal.ink_strong)
        .child(text)
}

fn note(text: &'static str, pal: &Palette) -> Div {
    div()
        .my(px(8.))
        .mx(px(2.))
        .text_size(px(FS_XS))
        .text_color(pal.faint)
        .child(text)
}

/// The rounded box rows sit in; rows after the first carry a hairline.
fn boxed(pal: &Palette) -> Div {
    // The page scrolls; a box keeps its height rather than shrinking.
    v_flex()
        .flex_none()
        .rounded(px(R_LG))
        .bg(pal.surface)
        .border_1()
        .border_color(pal.subtle)
        .overflow_hidden()
}

/// One labeled row with its control at the far end.
fn row(label: &'static str, hint: &'static str, control: impl IntoElement, pal: &Palette) -> Div {
    row_el(div().child(label), SharedString::from(hint), control, pal)
}

fn row_el(
    label: impl IntoElement,
    hint: SharedString,
    control: impl IntoElement,
    pal: &Palette,
) -> Div {
    h_flex()
        .min_h(px(58.))
        .px(px(16.))
        .py(px(12.))
        .gap(px(16.))
        .items_center()
        .border_t_1()
        .border_color(pal.subtle)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(FS_SM)).text_color(pal.ink).child(label))
                .child(div().text_size(px(FS_XS)).text_color(pal.muted).child(hint)),
        )
        .child(
            h_flex()
                .flex_none()
                .gap(px(8.))
                .items_center()
                .child(control),
        )
}

fn key_cap(text: String, pal: &Palette) -> Div {
    div()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(4.))
        .border_1()
        .border_color(pal.line)
        .bg(pal.fill)
        .font_family(FONT_MONO)
        .text_size(px(10.5))
        .text_color(pal.muted)
        .child(text)
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let pal = Palette::active(cx);
        let page = self.page(window, cx);
        let scrim_close = cx.entity();
        let close = cx.entity();
        let mut card = h_flex()
            .id("settings")
            .track_focus(&self.focus)
            .key_context("Settings")
            .on_action(cx.listener(|this, _: &SettingsClose, _, cx| this.close(cx)))
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                this.add_rule(None, window, cx);
            }))
            .w(px(960.))
            .h(px(720.))
            .max_h(gpui_kit::relative(0.92))
            .max_w(gpui_kit::relative(0.96))
            .bg(pal.bg)
            .border_1()
            .border_color(pal.line)
            .rounded(px(16.))
            .overflow_hidden()
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .child(self.nav(pal, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .relative()
                    .child(
                        v_flex()
                            .id(ElementId::Name(
                                format!("set-scroll-{}", self.section.label()).into(),
                            ))
                            .size_full()
                            .overflow_y_scroll()
                            .px(px(32.))
                            .pt(px(22.))
                            .pb(px(32.))
                            .children(page),
                    )
                    .child(
                        div()
                            .id("settings-close")
                            .absolute()
                            .top(px(12.))
                            .right(px(12.))
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(R_MD))
                            .text_color(pal.muted)
                            .hover(|row| row.bg(gpui_kit::transparent_black()))
                            .on_click(move |_, _, cx| close.update(cx, |this, cx| this.close(cx)))
                            .child(Icon::new(IconName::X).with_size(px(14.))),
                    ),
            );
        card.style().box_shadow = Some(pal.shadow_2.clone());
        let scrim = div()
            .id("settings-scrim")
            .absolute()
            .inset_0()
            .occlude()
            .bg(Hsla {
                h: 0.,
                s: 0.,
                l: 0.,
                a: 0.45,
            })
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(gpui_kit::MouseButton::Left, move |_, _, cx| {
                scrim_close.update(cx, |this, cx| this.close(cx));
            })
            .child(card);
        // Deferred, so it paints and hit-tests above everything the shell
        // draws, popovers included.
        gpui_kit::deferred(scrim)
            .with_priority(2)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::shortcuts;

    #[test]
    fn the_shortcut_table_lists_every_app_binding_once() {
        let rows = shortcuts(&crate::app::key_bindings());
        let labels: Vec<&str> = rows.iter().map(|row| row.label).collect();
        for wanted in [
            "Command palette",
            "New session",
            "Toggle sidebar",
            "Toggle workbench",
            "Settings",
            "Find in chat",
            "Cycle permission mode",
        ] {
            assert_eq!(
                labels.iter().filter(|label| **label == wanted).count(),
                1,
                "{wanted} in {labels:?}"
            );
        }
        let palette = rows
            .iter()
            .find(|row| row.label == "Command palette")
            .unwrap();
        let expected = if cfg!(target_os = "macos") {
            "Cmd K"
        } else {
            "Ctrl K"
        };
        assert_eq!(palette.keys, expected);
    }
}
