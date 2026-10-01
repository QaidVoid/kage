//! The settings dialog: a section nav on the left and one page on the
//! right, over the shell like the other dialogs.
//!
//! Pages render thin over the store. The client's own choices (theme,
//! Enter behavior, the Lab toggles, the archive) are preferences the
//! shell stores; everything about the engine is read from what the
//! connection reported and says unknown where it reported nothing.

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Context, Div, ElementId, Entity, FocusHandle, Focusable, Hsla,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, SharedString,
    Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::prefs::Prefs;
use crate::store::{Store, StoreHandle as _};
use crate::theme::{
    FONT_MONO, FS_SM, FS_XS, Palette, R_FULL, R_LG, R_MD, ThemeChoice, WEIGHT_BOLD, WEIGHT_SEMIBOLD,
};
use crate::transport::State;
use crate::views::settings_config as config;

gpui_kit::actions!(kage_desktop, [SettingsClose]);

/// The pages of the settings dialog, in nav order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Theme and input behavior.
    General,
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
    /// Experiments behind toggles.
    Lab,
    /// Sessions taken out of the sidebar.
    Archived,
    /// The engine and protocol versions.
    About,
}

impl Section {
    /// Every section, in nav order.
    pub const ALL: [Section; 10] = [
        Section::General,
        Section::Providers,
        Section::Mcp,
        Section::Permissions,
        Section::Plugins,
        Section::Connection,
        Section::Keyboard,
        Section::Lab,
        Section::Archived,
        Section::About,
    ];

    /// The nav label and page title.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Providers => "Model Providers",
            Section::Mcp => "MCP Servers",
            Section::Permissions => "Permissions",
            Section::Plugins => "Plugins",
            Section::Connection => "Connection",
            Section::Keyboard => "Keyboard",
            Section::Lab => "Lab",
            Section::Archived => "Archived Sessions",
            Section::About => "About",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Section::General => IconName::Settings2,
            Section::Providers => IconName::Zap,
            Section::Mcp => IconName::Server,
            Section::Permissions => IconName::ShieldCheck,
            Section::Plugins => IconName::LayoutDashboard,
            Section::Connection => IconName::Network,
            Section::Keyboard => IconName::Command,
            Section::Lab => IconName::Lightbulb,
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
    store: Entity<Store>,
    open: bool,
    section: Section,
    focus: FocusHandle,
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
            store,
            open: false,
            section: Section::General,
            focus: cx.focus_handle(),
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
        self.go(section, cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }

    /// Shows `section`. A page read from the engine's configuration
    /// asks for a fresh snapshot, so an edit to config.toml shows on the
    /// next visit.
    fn go(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        if matches!(
            section,
            Section::Providers | Section::Mcp | Section::Permissions | Section::Plugins
        ) {
            self.store.act(cx, Store::ask_config);
        }
        cx.notify();
    }

    /// Applies and stores a theme choice.
    fn choose_theme(&mut self, choice: ThemeChoice, window: &mut Window, cx: &mut Context<Self>) {
        self.store
            .act(cx, |store| store.update_prefs(|prefs| prefs.theme = choice));
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
            Section::Providers | Section::Mcp | Section::Permissions | Section::Plugins => {
                out.extend(self.config_page(pal, cx));
            }
            Section::Connection => out.extend(self.connection(pal, cx)),
            Section::Keyboard => out.extend(self.keyboard(pal, cx)),
            Section::Lab => out.extend(self.lab(pal, cx)),
            Section::Archived => out.extend(self.archived(pal, cx)),
            Section::About => out.extend(self.about(pal, cx)),
        }
        out
    }

    fn general(&self, window: &Window, pal: &Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let prefs = self.store.read(cx).prefs().clone();
        let mut cards = h_flex().gap(px(10.));
        let light = matches!(
            window.appearance(),
            gpui_kit::WindowAppearance::Light | gpui_kit::WindowAppearance::VibrantLight
        );
        for choice in [ThemeChoice::System, ThemeChoice::Shadow, ThemeChoice::Dawn] {
            let (side, main, accent) = match choice {
                ThemeChoice::System => {
                    let (shadow, dawn) = (Palette::shadow(), Palette::dawn());
                    let (side, main) = (shadow.bg, dawn.bg);
                    let accent = if light { dawn.accent } else { shadow.accent };
                    (side, main, accent)
                }
                ThemeChoice::Shadow => {
                    let shadow = Palette::shadow();
                    (shadow.bg, shadow.surface, shadow.accent)
                }
                ThemeChoice::Dawn => {
                    let dawn = Palette::dawn();
                    (dawn.bg, dawn.surface, dawn.accent)
                }
            };
            let on = prefs.theme == choice;
            let line_strong = pal.line_strong;
            cards = cards.child(
                v_flex()
                    .id(SharedString::from(format!("theme-{}", choice.label())))
                    .flex_1()
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
                        this.choose_theme(choice, window, cx);
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
                            .text_size(px(FS_XS))
                            .text_color(pal.ink)
                            .child(choice.label()),
                    ),
            );
        }
        vec![
            group("Appearance", pal).into_any_element(),
            cards.into_any_element(),
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
        ]
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
            Section::Providers => config::providers_page(&snapshot, option("model"), pal),
            Section::Mcp => {
                let live = session
                    .map(|session| session.mcp.clone())
                    .unwrap_or_default();
                config::mcp_page(&snapshot, &live, pal)
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
                config::permissions_page(
                    &snapshot,
                    modes.as_ref(),
                    current.as_deref(),
                    move |value, cx| {
                        handle.act(cx, |store| store.set_permission(&value));
                    },
                    pal,
                )
            }
            _ => config::plugins_page(&snapshot, pal),
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

    fn lab(&self, pal: &Palette, cx: &Context<Self>) -> Vec<AnyElement> {
        let prefs = self.store.read(cx).prefs().clone();
        vec![
            note("Experiments that may change or go away.", pal).into_any_element(),
            boxed(pal)
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
                .child(row(
                    "Context gauge",
                    "The context ring opens a gauge with Compact now",
                    self.toggle("set-fuel", prefs.fuel, |p| &mut p.fuel, pal, cx),
                    pal,
                ))
                .into_any_element(),
        ]
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
        vec![
            h_flex()
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
                                "desktop {} \u{b7} {engine}",
                                env!("CARGO_PKG_VERSION")
                            )),
                        )),
                )
                .into_any_element(),
            boxed(pal)
                .child(row(
                    "Protocol",
                    "The Agent Client Protocol with the _kage/* extensions",
                    badge(SharedString::from(protocol), pal),
                    pal,
                ))
                .into_any_element(),
        ]
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
        div()
            .id(id)
            .w(px(36.))
            .h(px(20.))
            .flex_none()
            .rounded(px(R_FULL))
            .bg(if on { pal.accent } else { pal.fill_hover })
            .relative()
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                view.update(cx, |this, cx| this.flip(cx, field));
            })
            .child(
                div()
                    .absolute()
                    .top(px(2.))
                    .left(px(if on { 18. } else { 2. }))
                    .size(px(16.))
                    .rounded(px(R_FULL))
                    .bg(gpui_kit::white()),
            )
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
    v_flex()
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

fn badge(text: impl Into<SharedString>, pal: &Palette) -> Div {
    div()
        .px(px(7.))
        .py(px(1.))
        .rounded(px(R_FULL))
        .border_1()
        .border_color(pal.line)
        .bg(pal.fill)
        .text_size(px(10.5))
        .text_color(pal.muted)
        .child(text.into())
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
        div()
            .id("settings-scrim")
            .absolute()
            .inset_0()
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
            .child(card)
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
