//! The settings dialog: a section nav on the left and one page on the
//! right, over the shell like the other dialogs.
//!
//! Pages render thin over the store. The client's own choices (theme,
//! Enter behavior, the chat toggles, the archive) are preferences the
//! shell stores; everything about the engine is read from what the
//! connection reported and says unknown where it reported nothing.

use crate::icons::PaintSnapped as _;
use crate::prefs::Prefs;
use crate::store::Store;
use crate::theme::{
    FAMILY_BODY_BOLD, FAMILY_BODY_MEDIUM, FAMILY_BODY_REGULAR, FAMILY_BODY_SEMIBOLD,
    FAMILY_MONO_REGULAR, FS_SM, FS_XS, Palette, R_LG, R_MD, family,
};
use crate::ui::forms::ShowCrisp as _;
use crate::ui::settings_config as config;
use crate::ui::settings_config::{BtnTone, btn_sm};

/// The nav column's width.
const NAV_W: f32 = 224.0;
/// A theme card's width.
const THEME_CARD_W: f32 = 150.0;
/// The theme select's width.
const PICK_W: f32 = 220.0;
/// The Updates row's status and buttons column.
#[cfg(not(target_arch = "wasm32"))]
const UPDATES_W: f32 = 360.0;
/// The theme select menu's height cap.
const PICK_MENU_H: f32 = 280.0;

/// The pages of the settings dialog, in nav order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    /// Theme and input behavior.
    General,
    /// The engine's agent and session options.
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

    /// The icon the nav row leads with.
    #[must_use]
    pub fn icon(self) -> crate::icons::Icon {
        match self {
            Section::General => crate::icons::Icon::Settings2,
            Section::Agents => crate::icons::Icon::Bot,
            Section::Providers => crate::icons::Icon::Zap,
            Section::Mcp => crate::icons::Icon::Server,
            Section::Permissions => crate::icons::Icon::ShieldCheck,
            Section::Plugins => crate::icons::Icon::LayoutDashboard,
            Section::Connection => crate::icons::Icon::Network,
            Section::Keyboard => crate::icons::Icon::Command,
            Section::Archived => crate::icons::Icon::Archive,
            Section::About => crate::icons::Icon::Info,
        }
    }

    /// Whether the page reads the engine's configuration snapshot, so
    /// opening it asks for a fresh one.
    #[must_use]
    pub fn reads_config(self) -> bool {
        matches!(
            self,
            Section::Providers | Section::Mcp | Section::Permissions | Section::Plugins
        )
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
    Some(match name {
        "OpenPalette" => "Command palette",
        "NewSession" => "New session",
        "ToggleSidebar" => "Toggle sidebar",
        "ToggleWorkbench" => "Toggle workbench",
        "CycleMode" => "Cycle permission mode",
        "OpenSettings" => "Settings",
        "OpenFind" => "Find in chat",
        "SendPrompt" => "Send, or steer the running turn",
        "Quit" => "Quit",
        _ => return None,
    })
}

/// The name of `action`, as the binding table spells it.
fn action_name(action: crate::ui::keys::Action) -> &'static str {
    use crate::ui::keys::Action;
    match action {
        Action::Quit => "Quit",
        Action::NewSession => "NewSession",
        Action::ToggleSidebar => "ToggleSidebar",
        Action::ToggleWorkbench => "ToggleWorkbench",
        Action::SendPrompt => "SendPrompt",
        Action::OpenFind => "OpenFind",
        Action::FindNext => "FindNext",
        Action::FindPrev => "FindPrev",
        Action::FindClose => "FindClose",
        Action::OpenPalette => "OpenPalette",
        Action::OpenSettings => "OpenSettings",
        Action::OpenProviders => "OpenProviders",
        Action::ToggleSwarm => "ToggleSwarm",
        Action::TogglePlan => "TogglePlan",
        Action::CycleMode => "CycleMode",
        Action::SetGoal => "SetGoal",
        Action::CloseDialog => "CloseDialog",
        Action::ShowAgents => "ShowAgents",
        Action::ReviewChanges => "ReviewChanges",
    }
}

/// The shell's shortcut table: one row per labeled action, in binding
/// order, with the ctrl (or cmd) modifier spelled for this platform.
#[must_use]
pub fn shortcuts() -> Vec<Shortcut> {
    let bindings = crate::ui::keys::bindings();
    let mut rows: Vec<Shortcut> = Vec::new();
    for binding in &bindings {
        let Some(label) = action_label(action_name(binding.action)) else {
            continue;
        };
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

/// One keystroke as key caps: `ctrl-k` reads `Ctrl K`.
fn key_caps(binding: &crate::ui::keys::Binding) -> String {
    let mut caps: Vec<String> = Vec::new();
    if binding.mods.ctrl {
        caps.push(if cfg!(target_os = "macos") {
            "Cmd".to_owned()
        } else {
            "Ctrl".to_owned()
        });
    }
    if binding.mods.shift {
        caps.push("Shift".to_owned());
    }
    if binding.mods.alt {
        caps.push("Alt".to_owned());
    }
    let mut chars = binding.key.chars();
    caps.push(match (chars.next(), chars.next()) {
        (Some(c), None) => c.to_uppercase().collect(),
        _ => {
            let mut word = binding.key.to_owned();
            if let Some(first) = word.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            word
        }
    });
    caps.join(" ")
}

/// Which updater an install button is running, so a second click
/// cannot stack a second download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// The engine binary behind `kage rpc`, for a client that ships
    /// none beside it.
    Engine,
    /// This client and the engine beside it, which take effect at the
    /// next launch.
    Desktop,
}

/// What the settings dialog asks the shell to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsEvent {
    /// Apply and store this theme choice.
    Theme(crate::theme::ThemeChoice),
    /// Flip one boolean preference, named by the field it writes.
    Flip(&'static str),
    /// Store engine option `name` in the user config with `value`.
    EngineOption(String, serde_json::Value),
    /// Make `name` the theme System draws on a dark desktop, or on a
    /// light one.
    SystemTheme { dark: bool, name: String },
    /// Set the active session's permission mode to this value.
    Mode(String),
    /// Bring archived session `id` back to the sidebar.
    Restore(String),
    /// Open the form that edits this provider entry.
    ProviderForm(crate::ui::provider_form::Target),
    /// Open the form that edits this MCP server, or adds one.
    McpForm(Option<String>),
    /// Open the form that edits this ACP agent, or adds one.
    AcpForm(Option<String>),
    /// Open the provider directory picker; `named` reads one the user
    /// names instead of models.dev.
    Directory(bool),
    /// Follow this release channel from now on, and check it at once.
    Channel(crate::prefs::Channel),
    /// Run a release check now, past the daily throttle.
    CheckNow,
    /// Download the newest release of `which`.
    Install(Install),
    /// Open the releases page in the platform's browser.
    OpenReleases,
}

/// Which page of the settings dialog shows, and whether it is open.
#[derive(Debug)]
pub struct Settings {
    open: bool,
    section: Section,
    /// The forms open over their page, one at most.
    form: Option<Form>,
    /// What the config pages edit in place.
    pages: config::PageState,
}

/// A form open over a settings page.
#[derive(Debug)]
enum Form {
    /// The provider form, over what it edits.
    Provider(Box<crate::ui::provider_form::ProviderForm>),
    /// The MCP server form.
    Mcp(Box<crate::ui::mcp_form::McpForm>),
    /// The ACP agent form.
    Acp(Box<crate::ui::acp_form::AcpForm>),
    /// The provider directory picker.
    Directory(Box<crate::ui::directory_picker::DirectoryPicker>),
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

impl Settings {
    /// A closed settings dialog on the General page.
    #[must_use]
    pub fn new() -> Self {
        Self {
            open: false,
            section: Section::General,
            form: None,
            pages: config::PageState::default(),
        }
    }

    /// Whether the dialog shows.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Shows the dialog on `section`.
    pub fn open(&mut self, section: Section) {
        self.open = true;
        self.go(section);
    }

    /// Closes the dialog, and any form open over its page.
    pub fn close(&mut self) {
        self.open = false;
        self.form = None;
    }

    /// The page that shows.
    #[must_use]
    pub fn section(&self) -> Section {
        self.section
    }

    /// Shows `section`. A page read from the engine's configuration
    /// asks for a fresh snapshot, so an edit to config.toml shows on
    /// the next visit.
    pub fn go(&mut self, section: Section) {
        self.section = section;
        self.form = None;
        self.pages = config::PageState::default();
    }

    /// Opens `form` over the page that shows.
    fn open_form(&mut self, form: Form) {
        self.form = Some(form);
    }

    /// Closes the form over the page, if one shows.
    fn close_form(&mut self) {
        self.form = None;
    }

    /// Follows the store: settles what the open form's writes did.
    fn follow(&mut self, store: &Store) {
        match &mut self.form {
            Some(Form::Provider(form)) => {
                form.follow(store);
            }
            Some(Form::Mcp(form)) => {
                form.follow(store);
            }
            Some(Form::Acp(form)) => {
                form.follow(store);
            }
            Some(Form::Directory(picker)) => picker.follow(store),
            None => {}
        }
    }

    /// Draws the page that shows, or the form open over it. Returns
    /// what the form answered with.
    fn show(
        &mut self,
        ui: &mut egui::Ui,
        store: &mut Store,
        palette: &Palette,
        updates: &UpdateState,
        events: &mut Vec<SettingsEvent>,
    ) -> FormOutcome {
        self.follow(store);
        let form_frame = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui) -> FormOutcome| {
            page_frame(ui, "form", |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                add(ui)
            })
        };
        match &mut self.form {
            Some(Form::Provider(form)) => {
                form_frame(ui, &mut |ui| FormOutcome::Done(form.show(ui, store)))
            }
            Some(Form::Mcp(form)) => {
                form_frame(ui, &mut |ui| FormOutcome::Done(form.show(ui, store)))
            }
            Some(Form::Acp(form)) => {
                form_frame(ui, &mut |ui| FormOutcome::Done(form.show(ui, store)))
            }
            Some(Form::Directory(picker)) => {
                form_frame(ui, &mut |ui| match picker.show(ui, store) {
                    Some(picked) if picked.entry.is_null() => FormOutcome::Done(true),
                    Some(picked) => FormOutcome::Picked(picked),
                    None => FormOutcome::Done(false),
                })
            }
            None => {
                page(
                    ui,
                    store,
                    self.section,
                    &mut self.pages,
                    palette,
                    updates,
                    events,
                );
                FormOutcome::Done(false)
            }
        }
    }
}

/// What a form drew back.
#[derive(Debug)]
enum FormOutcome {
    /// The form asked to close or not.
    Done(bool),
    /// The directory picker chose a provider.
    Picked(crate::ui::directory_picker::Picked),
}

/// The settings dialog over the shell: a nav column, one page, and a
/// close button. Draws nothing while closed.
#[derive(Default)]
pub struct SettingsView {
    settings: Settings,
}

impl SettingsView {
    /// A closed settings dialog.
    #[must_use]
    pub fn new() -> Self {
        Self {
            settings: Settings::new(),
        }
    }

    /// Whether the dialog shows.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.settings.is_open()
    }

    /// Shows the dialog on `section`.
    pub fn open(&mut self, section: Section) {
        self.settings.open(section);
    }

    /// Closes the dialog.
    pub fn close(&mut self) {
        self.settings.close();
    }

    /// The page that shows.
    #[must_use]
    pub fn section(&self) -> Section {
        self.settings.section()
    }

    /// Whether a layer over the page answers Escape itself: a form,
    /// an open menu or a field the page shows for one edit. Escape
    /// leaves that layer before it leaves the dialog, and the dialog's
    /// own keys wait for it.
    #[must_use]
    pub fn holds_escape(&self) -> bool {
        self.settings.form.is_some() || self.settings.pages.holds_escape()
    }

    /// Draws the dialog over `ui`, reporting what it asked for.
    /// `updates` is what the About page's release row reads.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        store: &mut Store,
        updates: &UpdateState,
    ) -> Vec<SettingsEvent> {
        if !self.settings.is_open() {
            return Vec::new();
        }
        // Escape leaves the form over the page first: the form is the
        // dialog's top layer, and the page reads the engine's
        // configuration again behind it.
        if self.settings.form.is_some() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.settings.close_form();
            if self.settings.section().reads_config() {
                store.ask_config();
            }
        }
        let palette = Palette::active_of(ui.ctx());
        let mut events = Vec::new();
        let mut close = false;
        let mut picked: Option<Section> = None;
        let mut outcome = FormOutcome::Done(false);
        egui::Area::new(egui::Id::new("kage-settings"))
            .anchor(egui::Align2::LEFT_TOP, egui::Vec2::ZERO)
            .order(egui::Order::Foreground)
            .interactable(true)
            .movable(false)
            .show(ui.ctx(), |ui| {
                let screen = ui.ctx().viewport_rect();
                ui.set_min_size(screen.size());
                let card = egui::Rect::from_center_size(
                    screen.center(),
                    egui::vec2(
                        960.0_f32.min(screen.width() * 0.96),
                        720.0_f32.min(screen.height() * 0.92),
                    ),
                );
                // The scrim covers the screen, so it must not swallow
                // the clicks the card's own widgets ask for: only a
                // click outside the card closes the dialog. Drawn
                // under the card, it answers only where the card is
                // not.
                let scrim = ui.allocate_response(screen.size(), egui::Sense::click());
                ui.painter().rect_filled(
                    scrim.rect,
                    egui::CornerRadius::ZERO,
                    egui::Color32::from_black_alpha(115),
                );
                if scrim.clicked() {
                    close = true;
                }
                // The card takes its own clicks before its contents draw
                // over it, so only the scrim around it closes the dialog.
                let _ = ui.interact(
                    card,
                    egui::Id::new("kage-settings-card"),
                    egui::Sense::click(),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
                    egui::Frame::NONE
                        .fill(palette.bg)
                        .stroke(egui::Stroke::new(1.0, palette.line))
                        .corner_radius(egui::CornerRadius::same(R_LG as u8))
                        .show_shadowed(ui, &palette.shadow_2, |ui| {
                            // The card's size counts its border, as the
                            // reference's box does.
                            let size = card.size() - egui::vec2(2.0, 2.0);
                            ui.set_min_size(size);
                            ui.set_max_size(size);
                            let inner = ui.max_rect();
                            let nav_rect = egui::Rect::from_min_size(
                                inner.min,
                                egui::vec2(NAV_W, inner.height()),
                            );
                            let page_rect = egui::Rect::from_min_max(
                                egui::pos2(nav_rect.right(), inner.top()),
                                inner.max,
                            );
                            let badge = store.prefs().update_badge;
                            ui.scope_builder(egui::UiBuilder::new().max_rect(nav_rect), |ui| {
                                nav(ui, &self.settings, badge, palette, &mut picked);
                            });
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .max_rect(page_rect)
                                    .layout(egui::Layout::top_down(egui::Align::Min)),
                                |ui| {
                                    outcome = self.settings.show(
                                        ui,
                                        store,
                                        palette,
                                        updates,
                                        &mut events,
                                    );
                                },
                            );
                            let close_rect = egui::Rect::from_min_size(
                                egui::pos2(page_rect.right() - 12.0 - 28.0, page_rect.top() + 12.0),
                                egui::vec2(28.0, 28.0),
                            );
                            ui.scope_builder(egui::UiBuilder::new().max_rect(close_rect), |ui| {
                                let response = crate::ui::forms::icon_btn(
                                    ui,
                                    "Close",
                                    crate::icons::Icon::X,
                                    28.0,
                                    14.0,
                                    R_MD,
                                    crate::ui::forms::IconTones {
                                        fill: egui::Color32::TRANSPARENT,
                                        hover_fill: palette.hover,
                                        ink: palette.muted,
                                        hover_ink: palette.ink,
                                    },
                                );
                                if response.clicked() {
                                    close = true;
                                }
                            });
                        });
                });
            });
        if let Some(section) = picked {
            self.settings.go(section);
            if section == Section::Agents {
                store.ask_engine_options();
            }
            if section == Section::About {
                store.update_prefs(|prefs| prefs.update_badge = false);
            }
        }
        for event in &events {
            match event {
                SettingsEvent::ProviderForm(target) => {
                    let config = store.config().cloned().unwrap_or(serde_json::Value::Null);
                    let form = crate::ui::provider_form::ProviderForm::new(target.clone(), &config);
                    self.settings.open_form(Form::Provider(Box::new(form)));
                }
                SettingsEvent::McpForm(name) => {
                    let config = store.config().cloned().unwrap_or(serde_json::Value::Null);
                    let snapshot = crate::ui::settings_config::Snapshot::parse(&config);
                    let server = name.as_deref().and_then(|name| snapshot.mcp_server(name));
                    let form = crate::ui::mcp_form::McpForm::new(name.as_deref().zip(server));
                    self.settings.open_form(Form::Mcp(Box::new(form)));
                }
                SettingsEvent::AcpForm(name) => {
                    let config = store.config().cloned().unwrap_or(serde_json::Value::Null);
                    let snapshot = crate::ui::settings_config::Snapshot::parse(&config);
                    let agent = name
                        .as_deref()
                        .and_then(|name| snapshot.acp_agent(name))
                        .cloned();
                    let form =
                        crate::ui::acp_form::AcpForm::new(name.as_deref().zip(agent.as_ref()));
                    self.settings.open_form(Form::Acp(Box::new(form)));
                }
                SettingsEvent::Directory(named) => {
                    let picker = crate::ui::directory_picker::DirectoryPicker::new(store, *named);
                    self.settings.open_form(Form::Directory(Box::new(picker)));
                }
                _ => {}
            }
        }
        events.retain(|event| {
            !matches!(
                event,
                SettingsEvent::ProviderForm(_)
                    | SettingsEvent::McpForm(_)
                    | SettingsEvent::AcpForm(_)
                    | SettingsEvent::Directory(_)
            )
        });
        match outcome {
            FormOutcome::Picked(picked) => {
                let config = store.config().cloned().unwrap_or(serde_json::Value::Null);
                let form = crate::ui::provider_form::ProviderForm::new(
                    crate::ui::provider_form::Target::Picked(picked.id, picked.entry),
                    &config,
                );
                self.settings.open_form(Form::Provider(Box::new(form)));
            }
            FormOutcome::Done(true) => {
                self.settings.close_form();
                if self.settings.section().reads_config() {
                    store.ask_config();
                }
            }
            FormOutcome::Done(false) => {}
        }
        if close {
            self.settings.close();
        }
        events
    }
}

/// The nav column: the title and one row per section, on the sidebar
/// fill with its hairline at the right.
fn nav(
    ui: &mut egui::Ui,
    settings: &Settings,
    badge: bool,
    palette: &Palette,
    picked: &mut Option<Section>,
) {
    let rect = ui.max_rect();
    // Square, as the reference clips the dialog to a plain rectangle:
    // its fill shows past the rounded border, which paints on top.
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, palette.sidebar);
    ui.painter().vline(
        rect.right() - 0.5,
        rect.y_range(),
        egui::Stroke::new(1.0, palette.line),
    );
    let title = ui.painter().layout_no_wrap(
        "Settings".to_owned(),
        egui::FontId::new(16.0, family(FAMILY_BODY_BOLD)),
        palette.ink_strong,
    );
    let mut y = rect.top() + 16.0 + 4.0;
    let title_h = crate::theme::line_h(16.0);
    ui.painter().galley(
        egui::pos2(rect.left() + 20.0, y + (title_h - title.size().y) / 2.0),
        title,
        palette.ink_strong,
    );
    // The title's 14px bottom pad and the column's 2px gap.
    y += title_h + 14.0 + 2.0;
    for section in Section::ALL {
        let on = section == settings.section();
        let row = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 10.0, y),
            egui::vec2(rect.width() - 20.0, 36.0),
        );
        y += 36.0 + 2.0;
        let response = ui
            .interact(
                row,
                ui.id().with(("nav", section.label())),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, section.label())
        });
        if on {
            ui.painter()
                .rect_filled(row, egui::CornerRadius::same(R_MD as u8), palette.selected);
        } else if response.hovered() {
            ui.painter()
                .rect_filled(row, egui::CornerRadius::same(R_MD as u8), palette.hover);
        }
        let ink = if on {
            palette.ink_strong
        } else {
            palette.muted
        };
        let mid = row.center().y;
        crate::icons::themed_image(section.icon(), 14.0, ink).paint_snapped(
            ui,
            egui::Rect::from_min_size(
                egui::pos2(row.left() + 10.0, mid - 7.0),
                egui::vec2(14.0, 14.0),
            ),
        );
        let label = ui.painter().layout_no_wrap(
            section.label().to_owned(),
            egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
            ink,
        );
        let label_right = row.left() + 34.0 + label.size().x.ceil();
        let y = crate::theme::line_y(row.top(), row.height(), &label);
        ui.painter()
            .galley(egui::pos2(row.left() + 34.0, y), label, ink);
        if badge && section == Section::About {
            paint_badge(
                ui,
                egui::pos2(label_right + 10.0, mid),
                "new",
                FS_XS,
                palette.ink,
                palette,
            );
        }
        if response.clicked() {
            *picked = Some(section);
        }
    }
}

/// The page column's scroller: the reference's 32px sides, 22px top
/// and 32px foot around what `add` draws. Pages and the forms over
/// them share it.
fn page_frame<R>(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::ScrollArea::vertical()
        .id_salt(("settings-page", id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::NONE
                .inner_margin(egui::Margin {
                    left: 32,
                    right: 32,
                    top: 22,
                    bottom: 32,
                })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 0.0;
                    add(ui)
                })
                .inner
        })
        .inner
}

/// One page of the dialog, scrolling inside the page column.
fn page(
    ui: &mut egui::Ui,
    store: &mut Store,
    section: Section,
    pages: &mut config::PageState,
    palette: &Palette,
    updates: &UpdateState,
    events: &mut Vec<SettingsEvent>,
) {
    page_frame(ui, section.label(), |ui| {
        title(ui, section.label(), palette);
        match section {
            Section::General => general(ui, store, &mut pages.pick_open, palette, events),
            Section::Agents => agents(ui, store, palette, events),
            Section::Providers | Section::Mcp | Section::Permissions | Section::Plugins => {
                config_page(ui, store, section, pages, palette, events);
            }
            Section::Connection => connection(ui, store, palette),
            Section::Keyboard => keyboard(ui, store, palette),
            Section::Archived => archived(ui, store, palette, events),
            Section::About => about(ui, store, palette, updates, events),
        }
    });
}

/// One toggle row of the General page: its label, hint, the setting it
/// writes, and whether it is on.
type Flip<'a> = (&'a str, &'a str, &'a str, bool);

/// The General page: the theme cards and picks, then the input, chat
/// and session toggles, in the reference's order.
fn general(
    ui: &mut egui::Ui,
    store: &Store,
    pick_open: &mut Option<bool>,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    let prefs = store.prefs();
    let catalog = store.theme_catalog();
    let light = ui.ctx().theme() == egui::Theme::Light;
    group(ui, "Appearance", palette);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
        let choices = std::iter::once(crate::theme::ThemeChoice::System).chain(
            catalog
                .names()
                .into_iter()
                .map(crate::theme::ThemeChoice::Named),
        );
        for choice in choices {
            theme_card(ui, choice, prefs, &catalog, light, palette, events);
        }
    });
    ui.add_space(10.0);
    boxed(ui, palette, |ui| {
        system_pick_row(ui, true, &catalog, pick_open, palette, events);
        system_pick_row(ui, false, &catalog, pick_open, palette, events);
    });
    let flips: [(&str, &[Flip]); 3] = [
        (
            "Input",
            &[
                (
                    "Vim mode",
                    "Normal-mode motions over the transcript (j k gg G za / n), a : command line and a modeline. Esc leaves the composer.",
                    "vim",
                    prefs.vim,
                ),
                (
                    "Enter sends",
                    "Off: Enter adds a newline and Ctrl+Enter sends",
                    "enter_sends",
                    prefs.enter_sends,
                ),
            ],
        ),
        (
            "Chat",
            &[
                (
                    "Smooth streaming",
                    "Show a reply that arrives in bursts at a steady pace, a moment behind",
                    "smooth_stream",
                    prefs.smooth_stream,
                ),
                (
                    "Turn timeline rail",
                    "A minimap beside the transcript with ticks for turns, edits, approvals, failures and swarms",
                    "rail",
                    prefs.rail,
                ),
                (
                    "Context gauge",
                    "The context ring opens a gauge with Compact now",
                    "fuel",
                    prefs.fuel,
                ),
                (
                    "Swarm constellation",
                    "Swarm cards draw one star per worker, lit by state",
                    "constellation",
                    prefs.constellation,
                ),
            ],
        ),
        (
            "Sessions",
            &[
                (
                    "Group sessions by project",
                    "Show sessions under their project folder in the sidebar",
                    "group_by_project",
                    prefs.group_by_project,
                ),
                (
                    "Ask before enabling swarm",
                    "Confirm when turning swarm mode on",
                    "confirm_swarm",
                    prefs.confirm_swarm,
                ),
            ],
        ),
    ];
    for (heading, rows) in flips {
        group(ui, heading, palette);
        boxed(ui, palette, |ui| {
            for &(label, hint, field, on) in rows {
                let mut flipped = on;
                toggle_row(ui, label, hint, &mut flipped, palette);
                if flipped != on {
                    events.push(SettingsEvent::Flip(field));
                }
            }
        });
    }
}

/// The Agent & Sessions page: the engine's options, grouped as the
/// design groups them. Each row writes its option back to the user
/// config.
fn agents(ui: &mut egui::Ui, store: &Store, palette: &Palette, events: &mut Vec<SettingsEvent>) {
    let Some(options) = store.engine_options() else {
        note(ui, "Reading the engine's options\u{2026}", palette);
        return;
    };
    let find = |name: &str| options.iter().find(|option| option.name == name);
    let groups: [(&str, &[(&str, &str)]); 4] = [
        (
            "Session defaults",
            &[
                ("thinking_level", "Thinking by default"),
                ("compaction_threshold", "Compact at"),
                ("compaction_reserve_tokens", "Compaction reserve"),
                ("max_provider_retries", "Provider retries"),
            ],
        ),
        (
            "Subagents",
            &[
                ("agent_max_depth", "Max depth"),
                ("agent_max_running", "Max running"),
                ("agent_thinking", "Thinking"),
                ("agent_max_turns", "Turn limit"),
                ("agent_timeout", "Run timeout"),
                ("agent_budget", "Token budget"),
            ],
        ),
        (
            "Swarm",
            &[
                ("swarm_max_items", "Max items per swarm"),
                ("swarm_timeout_ms", "Worker timeout"),
            ],
        ),
        ("", &[]),
    ];
    for (title, rows) in groups.iter().take(3) {
        group(ui, title, palette);
        boxed(ui, palette, |ui| {
            for (name, label) in rows.iter() {
                let Some(option) = find(name) else {
                    continue;
                };
                let badge = option.configured.then_some("set in config");
                row_with(
                    ui,
                    label,
                    badge,
                    &option.doc,
                    |ui| option_control(ui, option, palette, events),
                    palette,
                );
            }
        });
    }
    note(
        ui,
        "These live in the user config.toml; a project config that sets the same key still wins. Changes apply to sessions started afterwards.",
        palette,
    );
}

/// The control of one engine option: segments for a choice, a stepper
/// for a number or a fraction, and the raw value for anything else.
fn option_control(
    ui: &mut egui::Ui,
    option: &kage_client::wire::OptionEntry,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    match option.kind.as_str() {
        "choice" => {
            let current = option.value.as_str().unwrap_or_default();
            if let Some(value) = choice_segments(ui, &option.values, current, palette) {
                events.push(SettingsEvent::EngineOption(
                    option.name.clone(),
                    serde_json::Value::from(value),
                ));
            }
        }
        "int" | "fraction" => {
            if let Some(value) = stepper(ui, option, palette) {
                events.push(SettingsEvent::EngineOption(option.name.clone(), value));
            }
        }
        _ => badge(ui, &option.value.to_string(), palette),
    }
}

/// The label a choice value reads as in its segment.
fn choice_label(value: &str) -> String {
    match value {
        "" => "Auto".to_owned(),
        "xhigh" => "XHigh".to_owned(),
        other => {
            let mut word = other.to_owned();
            if let Some(first) = word.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            word
        }
    }
}

/// A segmented choice on the soft fill, the chosen segment raised.
/// Gives back the value a click picked.
fn choice_segments(
    ui: &mut egui::Ui,
    values: &[String],
    current: &str,
    palette: &Palette,
) -> Option<String> {
    let font = egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR));
    let galleys: Vec<_> = values
        .iter()
        .map(|value| {
            let ink = if value == current {
                palette.ink_strong
            } else {
                palette.muted
            };
            ui.painter()
                .layout_no_wrap(choice_label(value), font.clone(), ink)
        })
        .collect();
    let width: f32 = galleys
        .iter()
        .map(|galley| galley.size().x.ceil() + 18.0)
        .sum::<f32>()
        + 2.0 * (values.len() as f32 + 1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(R_MD as u8), palette.fill);
    let mut x = rect.left() + 2.0;
    let mut picked = None;
    for (value, galley) in values.iter().zip(galleys) {
        let seg = egui::Rect::from_min_size(
            egui::pos2(x, rect.top() + 2.0),
            egui::vec2(galley.size().x.ceil() + 18.0, 24.0),
        );
        x = seg.right() + 2.0;
        let on = value == current;
        let response = ui
            .interact(seg, ui.id().with(("segment", value)), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::SelectableLabel,
                true,
                on,
                choice_label(value),
            )
        });
        if on {
            ui.painter()
                .rect_filled(seg, egui::CornerRadius::same(6), palette.raised);
        }
        let ink = if on {
            palette.ink_strong
        } else {
            palette.muted
        };
        let y = crate::theme::line_y(seg.top(), seg.height(), &galley);
        ui.painter()
            .galley(egui::pos2(seg.left() + 9.0, y), galley, ink);
        if response.clicked() && !on {
            picked = Some(value.clone());
        }
    }
    picked
}

/// A number's stepper: minus and plus around the value, in a hairline
/// box. Gives back the value a click set.
fn stepper(
    ui: &mut egui::Ui,
    option: &kage_client::wire::OptionEntry,
    palette: &Palette,
) -> Option<serde_json::Value> {
    let fraction = option.kind == "fraction";
    let current = option.value.as_f64().unwrap_or(0.0);
    let (step, shown) = match (fraction, option.name.as_str()) {
        (true, _) => (0.05, format!("{}%", (current * 100.0).round())),
        (false, "compaction_reserve_tokens") => (
            1_024.0,
            off_or(current, format!("{} tokens", current as i64)),
        ),
        (false, "swarm_timeout_ms") => (60_000.0, format!("{} min", current as i64 / 60_000)),
        (false, "agent_max_turns") => (10.0, off_or(current, current.to_string())),
        (false, "agent_timeout") => (
            60.0,
            off_or(current, format!("{} min", current as i64 / 60)),
        ),
        (false, "agent_budget") => (
            100_000.0,
            off_or(current, format!("{}k", current as i64 / 1000)),
        ),
        _ => (1.0, (current as i64).to_string()),
    };
    let (min, max) = if fraction {
        (0.0, 1.0)
    } else {
        (
            option.min.unwrap_or(i64::MIN) as f64,
            option.max.unwrap_or(i64::MAX) as f64,
        )
    };
    let value = ui.painter().layout_no_wrap(
        shown,
        egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
        palette.ink,
    );
    let value_w = value.size().x.ceil().max(64.0);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(26.0 + 4.0 + value_w + 4.0 + 26.0 + 2.0, 28.0),
        egui::Sense::hover(),
    );
    let radius = egui::CornerRadius::same(R_MD as u8);
    ui.painter().rect_filled(rect, radius, palette.bg);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, palette.line),
        egui::StrokeKind::Inside,
    );
    // The value centers in its cell between the buttons, inside the
    // border, on the reference's line.
    let x = crate::theme::snap(
        rect.left() + 1.0 + 26.0 + 4.0 + (value_w - value.size().x.ceil()) / 2.0,
    );
    let y = crate::theme::line_y(rect.top() + 1.0, 26.0, &value);
    ui.painter().galley(egui::pos2(x, y), value, palette.ink);
    let mut set = None;
    for (delta, icon, at) in [
        (-step, crate::icons::Icon::Minus, rect.left() + 1.0),
        (step, crate::icons::Icon::Plus, rect.right() - 1.0 - 26.0),
    ] {
        let next = (current + delta).clamp(min, max);
        let live = (next - current).abs() > f64::EPSILON;
        let button =
            egui::Rect::from_min_size(egui::pos2(at, rect.top() + 1.0), egui::vec2(26.0, 26.0));
        let response = ui.interact(
            button,
            ui.id().with(("step", &option.name, delta.to_bits())),
            if live {
                egui::Sense::click()
            } else {
                egui::Sense::hover()
            },
        );
        let label = if delta < 0.0 { "Less" } else { "More" };
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, live, label));
        let tint = if live { palette.muted } else { palette.ghost };
        crate::icons::themed_image(icon, 12.0, tint).paint_snapped(
            ui,
            egui::Rect::from_center_size(button.center(), egui::vec2(12.0, 12.0)),
        );
        if live && response.clicked() {
            set = Some(if fraction {
                serde_json::Value::from((next * 100.0).round() / 100.0)
            } else {
                serde_json::Value::from(next as i64)
            });
        }
    }
    set
}

/// `text` for a value, or "Off" when the value is zero.
fn off_or(value: f64, text: String) -> String {
    if value == 0.0 { "Off".to_owned() } else { text }
}

/// The row naming the theme System draws on a dark or a light
/// desktop, with its theme select.
fn system_pick_row(
    ui: &mut egui::Ui,
    dark: bool,
    catalog: &crate::themes::Catalog,
    pick_open: &mut Option<bool>,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    let (label, hint) = if dark {
        (
            "System on a dark desktop",
            "The theme System draws when the desktop is dark",
        )
    } else {
        (
            "System on a light desktop",
            "The theme System draws when the desktop is light",
        )
    };
    row(
        ui,
        label,
        hint,
        |ui| theme_select(ui, dark, catalog, pick_open, palette, events),
        palette,
    );
}

/// The theme select: a trigger with the current theme's swatch and
/// name, and while open a menu of every theme hung under it. The kage
/// theme of the desktop's mode clears the pick. `pick_open` names the
/// one menu that shows, so opening one closes the other.
fn theme_select(
    ui: &mut egui::Ui,
    dark: bool,
    catalog: &crate::themes::Catalog,
    pick_open: &mut Option<bool>,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    let id = egui::Id::new(("theme-select", dark));
    let mut open = *pick_open == Some(dark);
    let current = catalog.system(!dark);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(PICK_W, 32.0), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::ComboBox,
            true,
            crate::themes::label(&current),
        )
    });
    let border = if open {
        palette.accent
    } else if response.hovered() {
        palette.line_strong
    } else {
        palette.line
    };
    let radius = egui::CornerRadius::same(R_MD as u8);
    ui.painter().rect_filled(rect, radius, palette.surface);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );
    // 10px inside the 1px border, the parts centered on what is in.
    let mid = rect.center().y;
    let mut x = rect.left() + 11.0;
    if let Some((theme, _)) = crate::themes::resolve(&current, catalog, !dark) {
        x = paint_swatch(ui, egui::pos2(x, mid), theme, palette) + 9.0;
    }
    let chevron = egui::Rect::from_min_size(
        egui::pos2(rect.right() - 11.0 - 12.0, mid - 6.0),
        egui::vec2(12.0, 12.0),
    );
    let name = crate::ui::transcript::head::one_line(
        ui,
        crate::themes::label(&current),
        egui::TextFormat::simple(
            egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
            palette.ink,
        ),
        chevron.left() - 9.0 - x,
    );
    let y = crate::theme::line_y(rect.top() + 1.0, rect.height() - 2.0, &name);
    ui.painter().galley(egui::pos2(x, y), name, palette.ink);
    crate::icons::themed_image(crate::icons::Icon::ChevronDown, 12.0, palette.faint)
        .paint_snapped(ui, chevron);
    if response.clicked() {
        open = !open;
    }
    if open {
        let kage = if dark { "kage-shadow" } else { "kage-dawn" };
        let menu = egui::Area::new(id.with("menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.left_bottom() + egui::vec2(0.0, 4.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::NONE
                    .fill(palette.bg)
                    .stroke(egui::Stroke::new(1.0, palette.line))
                    .corner_radius(egui::CornerRadius::same(R_LG as u8))
                    .inner_margin(egui::Margin::same(4))
                    .show_shadowed(ui, &palette.shadow_menu, |ui| {
                        ui.set_width(PICK_W - 8.0);
                        ui.spacing_mut().item_spacing.y = 1.0;
                        egui::ScrollArea::vertical()
                            .max_height(PICK_MENU_H)
                            .show(ui, |ui| {
                                for name in catalog.names() {
                                    let Some((theme, _)) =
                                        crate::themes::resolve(&name, catalog, !dark)
                                    else {
                                        continue;
                                    };
                                    let on = name == current;
                                    if theme_option(ui, &name, theme, on, palette).clicked() {
                                        let pick = if name == kage { String::new() } else { name };
                                        events
                                            .push(SettingsEvent::SystemTheme { dark, name: pick });
                                        open = false;
                                    }
                                }
                            });
                    });
            });
        let escape = ui.input(|input| input.key_pressed(egui::Key::Escape));
        if escape || (menu.response.clicked_elsewhere() && !response.clicked()) {
            open = false;
        }
    }
    if open {
        *pick_open = Some(dark);
    } else if *pick_open == Some(dark) {
        *pick_open = None;
    }
}

/// One theme in the select's menu: its swatch, its name and the check
/// on the one in use.
fn theme_option(
    ui: &mut egui::Ui,
    name: &str,
    theme: &Palette,
    on: bool,
    palette: &Palette,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            true,
            on,
            crate::themes::label(name),
        )
    });
    let radius = egui::CornerRadius::same(R_MD as u8);
    if response.hovered() {
        ui.painter().rect_filled(rect, radius, palette.accent_soft);
    } else if on {
        ui.painter().rect_filled(rect, radius, palette.selected);
    }
    let mid = rect.center().y;
    let x = paint_swatch(ui, egui::pos2(rect.left() + 8.0, mid), theme, palette) + 9.0;
    let ink = if on { palette.ink_strong } else { palette.ink };
    let mut right = rect.right() - 8.0;
    if on {
        let check =
            egui::Rect::from_min_size(egui::pos2(right - 13.0, mid - 6.5), egui::vec2(13.0, 13.0));
        crate::icons::themed_image(crate::icons::Icon::Check, 13.0, palette.accent)
            .paint_snapped(ui, check);
        right = check.left() - 9.0;
    }
    let label = crate::ui::transcript::head::one_line(
        ui,
        crate::themes::label(name),
        egui::TextFormat::simple(egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)), ink),
        right - x,
    );
    ui.painter().galley(
        egui::pos2(x, crate::theme::line_y(mid, 0.0, &label)),
        label,
        ink,
    );
    response
}

/// Paints a theme's colors in small at `at` (left edge, vertical
/// center): its sidebar beside its background with a dot of its
/// accent. Gives back the swatch's right edge.
fn paint_swatch(ui: &egui::Ui, at: egui::Pos2, theme: &Palette, palette: &Palette) -> f32 {
    let rect = egui::Rect::from_min_size(egui::pos2(at.x, at.y - 8.0), egui::vec2(26.0, 16.0));
    let radius = egui::CornerRadius::same(4);
    ui.painter().rect_filled(rect, radius, theme.bg);
    // Inside the 1px border: the 9px sidebar strip, then the main
    // color with the accent dot centered on it.
    ui.painter().rect_filled(
        egui::Rect::from_min_size(rect.min + egui::vec2(1.0, 1.0), egui::vec2(9.0, 14.0)),
        egui::CornerRadius {
            nw: 3,
            sw: 3,
            ne: 0,
            se: 0,
        },
        theme.sidebar,
    );
    let main_center = egui::pos2(rect.left() + 10.0 + 15.0 / 2.0, rect.center().y);
    ui.painter().circle_filled(main_center, 3.0, theme.accent);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, palette.line),
        egui::StrokeKind::Inside,
    );
    rect.right()
}

/// One theme card: a swatch of its side, main and accent colors over
/// its name, outlined in the accent when chosen.
fn theme_card(
    ui: &mut egui::Ui,
    choice: crate::theme::ThemeChoice,
    prefs: &Prefs,
    catalog: &crate::themes::Catalog,
    light: bool,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    let resolve = |name: &str, light: bool| crate::themes::resolve(name, catalog, light);
    let (side, main, accent) = match &choice {
        crate::theme::ThemeChoice::System => {
            let (Some((dark, _)), Some((day, _))) = (
                resolve(&catalog.system(false), false),
                resolve(&catalog.system(true), true),
            ) else {
                return;
            };
            (
                dark.bg,
                day.bg,
                if light { day.accent } else { dark.accent },
            )
        }
        crate::theme::ThemeChoice::Named(name) => {
            let Some((theme, _)) = resolve(name, light) else {
                return;
            };
            (theme.bg, theme.surface, theme.accent)
        }
    };
    let on = prefs.theme == choice;
    let label_h = crate::theme::line_h(FS_XS);
    // 8px inside a 1px border around the swatch and the name.
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(THEME_CARD_W, 1.0 + 8.0 + 58.0 + 8.0 + label_h + 8.0 + 1.0),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let label = choice.label();
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, on, label.clone())
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
    let swatch = egui::Rect::from_min_size(
        rect.min + egui::vec2(9.0, 9.0),
        egui::vec2(rect.width() - 18.0, 58.0),
    );
    let side_w = swatch.width() * 0.28;
    let r = R_MD as u8;
    ui.painter().rect_filled(
        swatch,
        egui::CornerRadius {
            nw: 0,
            sw: 0,
            ne: r,
            se: r,
        },
        main,
    );
    ui.painter().rect_filled(
        egui::Rect::from_min_size(swatch.min, egui::vec2(side_w, swatch.height())),
        egui::CornerRadius {
            nw: r,
            sw: r,
            ne: 0,
            se: 0,
        },
        side,
    );
    let main_w = swatch.width() - side_w;
    ui.painter().rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(swatch.left() + side_w + 8.0, swatch.bottom() - 16.0),
            egui::vec2(main_w * 0.4, 8.0),
        ),
        egui::CornerRadius::same(4),
        accent,
    );
    let name = crate::ui::transcript::head::one_line(
        ui,
        label,
        egui::TextFormat::simple(
            egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
            palette.ink,
        ),
        rect.width() - 22.0,
    );
    let y = crate::theme::line_y(swatch.bottom() + 8.0, label_h, &name);
    ui.painter()
        .galley(egui::pos2(rect.left() + 11.0, y), name, palette.ink);
    if response.clicked() {
        events.push(SettingsEvent::Theme(choice));
    }
}

/// One page the configuration snapshot backs. The pages read the
/// active session, or the first open one, for its live facts.
fn config_page(
    ui: &mut egui::Ui,
    store: &mut Store,
    section: Section,
    pages: &mut config::PageState,
    palette: &Palette,
    events: &mut Vec<SettingsEvent>,
) {
    let Some(config) = store.config() else {
        config::waiting(ui, palette);
        return;
    };
    let snapshot = config::Snapshot::parse(config);
    let session = store
        .active_session()
        .or_else(|| {
            store
                .state()
                .sessions
                .values()
                .find(|session| session.opened && session.parent.is_none())
        })
        .cloned();
    let option = |id: &str| {
        session
            .as_ref()
            .and_then(|session| session.config_options.iter().find(|option| option.id == id))
            .cloned()
    };
    match section {
        Section::Providers => {
            config::providers_page(
                ui,
                &snapshot,
                option("model").as_ref(),
                pages,
                palette,
                events,
            );
        }
        Section::Mcp => {
            let live = session
                .as_ref()
                .map(|session| session.mcp.clone())
                .unwrap_or_default();
            config::mcp_page(ui, &snapshot, &live, palette, events);
        }
        Section::Permissions => {
            let modes = option("mode").map(|mut option| {
                option
                    .options
                    .retain(|choice| choice.value != crate::modes::PLAN_MODE);
                option
            });
            let current = store.permission_mode();
            config::permissions_page(
                ui,
                store,
                &snapshot,
                modes.as_ref(),
                current.as_deref(),
                pages,
                palette,
                events,
            );
        }
        Section::Plugins => config::plugins_page(ui, store, &snapshot, pages, palette),
        _ => {}
    }
}

/// The Connection page: the transport, the engine binary or endpoint,
/// and the link's live status.
fn connection(ui: &mut egui::Ui, store: &Store, palette: &Palette) {
    let link = store.link();
    let state = store.state();
    let (word, dot) = match store.connect() {
        crate::transport::State::Connected => ("Connected", palette.ok),
        crate::transport::State::Connecting => ("Connecting", palette.warn),
        crate::transport::State::Reconnecting { .. } => ("Reconnecting", palette.warn),
        crate::transport::State::Refused(_) => ("Refused", palette.danger),
        crate::transport::State::Closed => ("Closed", palette.faint),
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
    let open = state.sessions.values().filter(|s| s.opened).count();
    let detail = format!(
        "{} \u{b7} {version} \u{b7} {open} sessions{}",
        link.detail,
        if features.is_empty() {
            String::new()
        } else {
            format!(" \u{b7} {}", features.join(", "))
        }
    );
    let mono = |ui: &mut egui::Ui, text: String| {
        let galley = ui.painter().layout_no_wrap(
            text.clone(),
            egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
            palette.ink,
        );
        let line = crate::theme::line_h(FS_XS);
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(galley.size().x.ceil(), line),
            egui::Sense::hover(),
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &text));
        let y = crate::theme::line_y(crate::theme::snap(rect.top()), line, &galley);
        ui.painter()
            .galley(egui::pos2(rect.left(), y), galley, palette.ink);
    };
    group(ui, "Transport", palette);
    boxed(ui, palette, |ui| {
        row(
            ui,
            "Connect with",
            "Chosen when the client starts: the desktop app spawns kage rpc or dials a server, the browser attaches to the kage serve it came from",
            |ui| badge(ui, link.name, palette),
            palette,
        );
        #[cfg(not(target_arch = "wasm32"))]
        if link.name == "kage rpc" {
            let binary =
                match crate::transport::stdio::engine_program(store.prefs().kage_path.as_deref()) {
                    program if program == "kage" => "kage from the PATH".to_owned(),
                    program => program,
                };
            row(
                ui,
                "kage binary",
                "The engine this app spawns: the one shipped beside it, else the one the setup screen picked",
                |ui| mono(ui, binary),
                palette,
            );
        }
        row(
            ui,
            "Endpoint",
            "The token rides the kage.<token> subprotocol and is never shown",
            |ui| mono(ui, link.detail.clone()),
            palette,
        );
    });
    group(ui, "Status", palette);
    boxed(ui, palette, |ui| {
        status_row(ui, word, dot, &detail, palette);
    });
}

/// The link's status: the lamp and its word over the link's facts.
fn status_row(ui: &mut egui::Ui, word: &str, dot: egui::Color32, detail: &str, palette: &Palette) {
    // A settings row: the top border, 12px above and below, the lamp
    // and word over the link's facts.
    let word_h = crate::theme::line_h(FS_SM);
    let detail_h = crate::theme::line_h(FS_XS);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(
            ui.available_width(),
            (1.0 + 12.0 + word_h + detail_h + 12.0).max(58.0),
        ),
        egui::Sense::hover(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, word));
    row_rule(ui, rect, palette);
    let inner_top = rect.top() + 13.0;
    let inner_h = rect.bottom() - 12.0 - inner_top;
    let top = crate::theme::snap(inner_top + (inner_h - word_h - detail_h) / 2.0);
    let x = rect.left() + 16.0;
    let lamp = crate::theme::snap(top + (word_h - 8.0) / 2.0);
    ui.painter()
        .circle_filled(egui::pos2(x + 4.0, lamp + 4.0), 4.0, dot);
    let name = ui.painter().layout_no_wrap(
        word.to_owned(),
        egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
        palette.ink,
    );
    let y = crate::theme::line_y(top, word_h, &name);
    ui.painter()
        .galley(egui::pos2(x + 16.0, y), name, palette.ink);
    let detail = crate::ui::transcript::head::one_line(
        ui,
        detail.to_owned(),
        egui::TextFormat::simple(
            egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
            palette.muted,
        ),
        rect.width() - 32.0,
    );
    let y = crate::theme::line_y(top + word_h, detail_h, &detail);
    ui.painter().galley(egui::pos2(x, y), detail, palette.muted);
}

/// The Keyboard page: the app bindings and the composer's keys.
fn keyboard(ui: &mut egui::Ui, store: &Store, palette: &Palette) {
    let enter_sends = store.prefs().enter_sends;
    let mut composer: Vec<(&str, &str)> = if enter_sends {
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
    composer.push(("Interrupt", "Esc Esc"));
    composer.push(("Answer approval", "1 2 3"));
    group(ui, "Shortcuts", palette);
    let app: Vec<(String, String)> = shortcuts()
        .into_iter()
        .map(|row| (row.label.to_owned(), row.keys))
        .collect();
    table(ui, &app, palette);
    group(ui, "Composer", palette);
    let owned: Vec<(String, String)> = composer
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect();
    table(ui, &owned, palette);
    let vim = if store.prefs().vim {
        "Vim mode (on)"
    } else {
        "Vim mode (off; turn it on under General)"
    };
    group(ui, vim, palette);
    let vim_keys = [
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
    ];
    let vim_rows: Vec<(String, String)> = vim_keys
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect();
    table(ui, &vim_rows, palette);
    note(
        ui,
        "Commands for the : line: theme <name>, model <name>, swarm on|off, plan on|off, goal <text>, new, settings, compact, noh, q, vim off.",
        palette,
    );
}

/// The Archived Sessions page: the archive, each with its project and
/// a restore button.
fn archived(ui: &mut egui::Ui, store: &Store, palette: &Palette, events: &mut Vec<SettingsEvent>) {
    let ids: Vec<String> = store.prefs().archived.iter().cloned().collect();
    if ids.is_empty() {
        ui.add_space(48.0);
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            crate::icons::icon(ui, crate::icons::Icon::Archive, 18.0, palette.faint);
            let text = "Nothing archived. Archive a session from its \u{2026} menu.";
            let galley = ui.painter().layout_no_wrap(
                text.to_owned(),
                egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
                palette.faint,
            );
            let line = crate::theme::line_h(FS_SM);
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(galley.size().x.ceil(), line),
                egui::Sense::hover(),
            );
            response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
            let y = crate::theme::line_y(rect.top(), line, &galley);
            ui.painter()
                .galley(egui::pos2(rect.left(), y), galley, palette.faint);
        });
        ui.add_space(48.0);
        return;
    }
    note(
        ui,
        "The archive is this client's own mark; the session stays recorded on the engine.",
        palette,
    );
    boxed(ui, palette, |ui| {
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
                .map(|info| crate::ui::sidebar::project_name(Some(&info.cwd)));
            if archived_row(ui, &title, project.as_deref(), palette) {
                events.push(SettingsEvent::Restore(id.clone()));
            }
        }
    });
}

/// One archived session: its icon, its title over its project, and the
/// restore button. Answers whether the button was clicked.
fn archived_row(ui: &mut egui::Ui, title: &str, project: Option<&str>, palette: &Palette) -> bool {
    let title_h = crate::theme::line_h(FS_SM);
    let project_h = if project.is_some() {
        crate::theme::line_h(FS_XS)
    } else {
        0.0
    };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 10.0 + title_h + project_h + 10.0),
        egui::Sense::hover(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, title));
    row_rule(ui, rect, palette);
    let mid = rect.center().y;
    crate::icons::themed_image(crate::icons::Icon::Archive, 14.0, palette.faint).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(rect.left() + 16.0, mid - 7.0),
            egui::vec2(14.0, 14.0),
        ),
    );
    let slot = egui::Rect::from_min_max(
        egui::pos2(rect.right() - 16.0 - 120.0, rect.top()),
        egui::pos2(rect.right() - 16.0, rect.bottom()),
    );
    let restore = ui
        .scope_builder(
            egui::UiBuilder::new()
                .max_rect(slot)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| btn_sm(ui, "Restore", None, BtnTone::Plain, palette),
        )
        .inner;
    let x = rect.left() + 16.0 + 14.0 + 12.0;
    let width = restore.rect.left() - 12.0 - x;
    let top = mid - (title_h + project_h) / 2.0;
    let name = crate::ui::transcript::head::one_line(
        ui,
        title.to_owned(),
        egui::TextFormat::simple(
            egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
            palette.ink,
        ),
        width,
    );
    ui.painter().galley(
        egui::pos2(x, top + (title_h - name.size().y) / 2.0),
        name,
        palette.ink,
    );
    if let Some(project) = project {
        ui.painter().text(
            egui::pos2(x, top + title_h + project_h / 2.0),
            egui::Align2::LEFT_CENTER,
            project,
            egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
            palette.muted,
        );
    }
    restore.clicked()
}

/// The About page: the mark and the versions, then the protocol and
/// the release check in one box.
fn about(
    ui: &mut egui::Ui,
    store: &Store,
    palette: &Palette,
    update_state: &UpdateState,
    events: &mut Vec<SettingsEvent>,
) {
    let state = store.state();
    let engine = state
        .agent
        .as_ref()
        .and_then(|agent| agent.version.clone())
        .map_or_else(|| "engine unknown".to_owned(), |v| format!("engine {v}"));
    let protocol = state
        .protocol_version
        .map_or_else(|| "unknown".to_owned(), |v| format!("ACP v{v}"));
    ui.add_space(8.0);
    // The mark beside the name over the versions, centered on each
    // other as the reference's row lays them.
    let name = ui.painter().layout_no_wrap(
        "kage".to_owned(),
        egui::FontId::new(18.0, family(FAMILY_BODY_BOLD)),
        palette.ink_strong,
    );
    let versions = ui.painter().layout_no_wrap(
        format!("desktop {} \u{b7} {engine}", env!("KAGE_VERSION")),
        egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
        palette.muted,
    );
    let (name_h, versions_h) = (crate::theme::line_h(18.0), crate::theme::line_h(FS_SM));
    let height = (name_h + versions_h).max(48.0);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "kage"));
    let mark = egui::Rect::from_min_size(
        egui::pos2(
            rect.left(),
            crate::theme::snap(rect.top() + (height - 48.0) / 2.0),
        ),
        crate::ui::mark::size(48.0),
    );
    crate::ui::mark::paint(ui, mark);
    let x = mark.right() + 14.0;
    let top = crate::theme::snap(rect.top() + (height - name_h - versions_h) / 2.0);
    let y = crate::theme::line_y(top, name_h, &name);
    ui.painter()
        .galley(egui::pos2(x, y), name, palette.ink_strong);
    let y = crate::theme::line_y(top + name_h, versions_h, &versions);
    ui.painter()
        .galley(egui::pos2(x, y), versions, palette.muted);
    ui.add_space(18.0);
    boxed(ui, palette, |ui| {
        row(
            ui,
            "Protocol",
            "The Agent Client Protocol with the _kage/* extensions",
            |ui| badge(ui, &protocol, palette),
            palette,
        );
        #[cfg(not(target_arch = "wasm32"))]
        updates(ui, store, palette, update_state, events);
    });
    #[cfg(target_arch = "wasm32")]
    let _ = (update_state, events);
}
/// What the release check and the installs in the Updates row are
/// doing right now. The shell owns it; the page only reads it, so a
/// second install cannot stack on a first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateState {
    /// Which install the in-flight download is for.
    pub installing: Option<Install>,
    /// Whether a release check is running, past the daily throttle.
    pub checking: bool,
    /// What the last install ended with: true when it worked.
    pub install_note: Option<(bool, String)>,
}

/// The Updates row: what the last release check saw for the engine and
/// this client, the channel choice, a manual re-check, and the install
/// buttons a newer build puts on screen.
#[cfg(not(target_arch = "wasm32"))]
fn updates(
    ui: &mut egui::Ui,
    store: &Store,
    palette: &Palette,
    update_state: &UpdateState,
    events: &mut Vec<SettingsEvent>,
) {
    let prefs = store.prefs();
    let engine = store
        .state()
        .agent
        .as_ref()
        .and_then(|agent| agent.version.clone());
    let desktop = env!("KAGE_VERSION");
    let nightly = prefs.channel == crate::prefs::Channel::Nightly;
    let (engine_line, desktop_line) = if nightly {
        let engine_line = match prefs.latest_nightly.as_ref() {
            Some(build) => format!(
                "engine: nightly {} ({}) published",
                build.date,
                &build.commit[..build.commit.len().min(7)]
            ),
            None => "engine: no nightly seen yet".to_owned(),
        };
        let desktop_line = match prefs.latest_nightly.as_ref() {
            Some(build) if option_env!("KAGE_BUILD_SHA").is_some_and(|sha| sha == build.commit) => {
                format!(
                    "desktop {}; on the {} nightly",
                    env!("KAGE_VERSION"),
                    build.date
                )
            }
            Some(build) => format!(
                "desktop {}; nightly {} available",
                env!("KAGE_VERSION"),
                build.date
            ),
            None => "desktop: no nightly seen yet".to_owned(),
        };
        (engine_line, desktop_line)
    } else {
        let engine_line = match (prefs.latest_version.as_deref(), engine.as_deref()) {
            (Some(latest), Some(now)) if kage_core::build::is_nightly(now) => {
                format!("engine {now}; the newest release is {latest}")
            }
            (Some(latest), Some(now)) if crate::update::reported_behind(latest, now) => {
                format!("engine {now}; {latest} is available")
            }
            (Some(latest), Some(now)) => format!("engine {now}; current with {latest}"),
            (Some(latest), None) => format!("engine version unknown; {latest} is available"),
            (None, _) => "engine: no check has run yet".to_owned(),
        };
        let desktop_line = match prefs.latest_version.as_deref() {
            Some(latest) if kage_core::build::is_nightly(env!("KAGE_VERSION")) => {
                format!("desktop {desktop}; the newest release is {latest}")
            }
            Some(latest) if crate::update::reported_behind(latest, desktop) => {
                format!("desktop {desktop}; {latest} is available")
            }
            Some(latest) => format!("desktop {desktop}; current with {latest}"),
            None => "desktop: no check has run yet".to_owned(),
        };
        (engine_line, desktop_line)
    };
    // An engine shipped beside this client updates with it, so only
    // one run from elsewhere gets its own install.
    let engine_behind = crate::transport::stdio::bundled_engine().is_none()
        && if nightly {
            prefs.latest_nightly.is_some()
        } else {
            matches!(
                (prefs.latest_version.as_deref(), engine.as_deref()),
                (Some(latest), Some(now)) if crate::update::reported_behind(latest, now)
            )
        };
    let desktop_behind = if nightly {
        prefs.latest_nightly.as_ref().is_some_and(|build| {
            option_env!("KAGE_BUILD_SHA").is_some_and(|sha| sha != build.commit)
        })
    } else {
        prefs
            .latest_version
            .as_deref()
            .is_some_and(|latest| crate::update::reported_behind(latest, desktop))
    };
    row(
        ui,
        "Updates",
        "The public releases, checked at most once a day",
        |ui| {
            // The reference sizes this control to its content: the wider
            // of the buttons and the status lines, the lines no wider
            // than 360px, each on its 19px line, 2px apart.
            let line_h = crate::theme::line_h(FS_XS);
            let mut lines = vec![
                (engine_line.clone(), palette.ink),
                (desktop_line.clone(), palette.muted),
            ];
            if let Some(why) = &prefs.check_error {
                lines.push((format!("last check failed: {why}"), palette.warn));
            }
            let galleys: Vec<_> = lines
                .into_iter()
                .map(|(text, color)| {
                    let mut job = egui::text::LayoutJob::single_section(
                        text,
                        egui::TextFormat {
                            font_id: egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
                            color,
                            line_height: Some(line_h),
                            ..Default::default()
                        },
                    );
                    job.wrap.max_width = UPDATES_W;
                    ui.painter().layout_job(job)
                })
                .collect();
            let check = if update_state.checking {
                "Checking\u{2026}"
            } else {
                "Check now"
            };
            let mut labels: Vec<(&str, Option<crate::icons::Icon>)> =
                vec![("Stable", None), ("Nightly", None), (check, None)];
            if engine_behind {
                labels.push(("Install engine", None));
            }
            if desktop_behind {
                labels.push(("Install desktop", None));
            }
            labels.push(("Releases", Some(crate::icons::Icon::ExternalLink)));
            let buttons_w = labels
                .iter()
                .map(|(label, icon)| {
                    crate::ui::settings_config::btn_sm_size(
                        &crate::ui::settings_config::btn_sm_galley(ui, label),
                        *icon,
                    )
                    .x
                })
                .sum::<f32>()
                + 8.0 * (labels.len() as f32 - 1.0);
            let text_w = galleys
                .iter()
                .map(|galley| galley.size().x.ceil())
                .fold(0.0, f32::max);
            let width = buttons_w.max(text_w);
            // Its full height up front, so the row centers all of it.
            let lines_h = galleys.iter().map(|galley| galley.size().y).sum::<f32>()
                + 2.0 * (galleys.len() as f32 - 1.0);
            let note_h = if update_state.install_note.is_some() {
                2.0 + line_h
            } else {
                0.0
            };
            let height = lines_h + 8.0 + 26.0 + note_h;
            let layout = egui::Layout::top_down(egui::Align::Min);
            ui.allocate_ui_with_layout(egui::vec2(width, height), layout, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing.y = 2.0;
                for galley in galleys {
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(width, galley.size().y),
                        egui::Sense::hover(),
                    );
                    let text = galley.text().to_owned();
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &text)
                    });
                    let shift = crate::theme::half_lead(&galley);
                    ui.painter().galley(
                        egui::pos2(rect.left(), rect.top() + shift),
                        galley,
                        egui::Color32::PLACEHOLDER,
                    );
                }
                let line = |ui: &mut egui::Ui, text: &str, color: egui::Color32| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).size(FS_XS).color(color))
                            .selectable(false)
                            .wrap(),
                    );
                };
                ui.add_space(8.0 - 2.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                    let tone = |on: bool| if on { BtnTone::Primary } else { BtnTone::Plain };
                    if btn_sm(ui, "Stable", None, tone(!nightly), palette).clicked() {
                        events.push(SettingsEvent::Channel(crate::prefs::Channel::Latest));
                    }
                    if btn_sm(ui, "Nightly", None, tone(nightly), palette).clicked() {
                        events.push(SettingsEvent::Channel(crate::prefs::Channel::Nightly));
                    }
                    if btn_sm(ui, check, None, BtnTone::Plain, palette).clicked() {
                        events.push(SettingsEvent::CheckNow);
                    }
                    if engine_behind {
                        let label = if update_state.installing == Some(Install::Engine) {
                            "Installing\u{2026}"
                        } else {
                            "Install engine"
                        };
                        if btn_sm(ui, label, None, BtnTone::Primary, palette).clicked() {
                            events.push(SettingsEvent::Install(Install::Engine));
                        }
                    }
                    if desktop_behind {
                        let label = if update_state.installing == Some(Install::Desktop) {
                            "Installing\u{2026}"
                        } else {
                            "Install desktop"
                        };
                        if btn_sm(ui, label, None, BtnTone::Primary, palette).clicked() {
                            events.push(SettingsEvent::Install(Install::Desktop));
                        }
                    }
                    let releases = btn_sm(
                        ui,
                        "Releases",
                        Some(crate::icons::Icon::ExternalLink),
                        BtnTone::Plain,
                        palette,
                    );
                    if releases.clicked() {
                        events.push(SettingsEvent::OpenReleases);
                    }
                });
                if let Some((ok, text)) = &update_state.install_note {
                    line(ui, text, if *ok { palette.ok } else { palette.warn });
                }
            });
        },
        palette,
    );
}

/// The page title: the reference's 17px bold over its 34px band, clear
/// of the close button at the right.
fn title(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    ui.add_space(2.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(17.0, family(FAMILY_BODY_BOLD)),
        palette.ink_strong,
    );
    let y = crate::theme::line_y(rect.top(), rect.height(), &galley);
    ui.painter()
        .galley(egui::pos2(rect.left(), y), galley, palette.ink_strong);
    ui.add_space(6.0);
}

/// A group heading above a page section.
pub(crate) fn group(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    ui.add_space(22.0);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), crate::theme::line_h(FS_SM)),
        egui::Sense::hover(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(FS_SM, family(FAMILY_BODY_SEMIBOLD)),
        palette.ink_strong,
    );
    let y = crate::theme::line_y(rect.top(), rect.height(), &galley);
    ui.painter()
        .galley(egui::pos2(rect.left(), y), galley, palette.ink_strong);
    ui.add_space(8.0);
}

/// A faint note under a page section.
pub(crate) fn note(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    ui.add_space(8.0);
    let width = ui.available_width() - 4.0;
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: egui::FontId::new(FS_XS, family(FAMILY_BODY_REGULAR)),
            color: palette.faint,
            line_height: Some(crate::theme::line_h(FS_XS)),
            ..Default::default()
        },
    );
    job.wrap.max_width = width;
    let galley = ui.painter().layout_job(job);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), galley.size().y),
        egui::Sense::hover(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let shift = crate::theme::half_lead(&galley);
    ui.painter().galley(
        egui::pos2(rect.left() + 2.0, rect.top() + shift),
        galley,
        palette.faint,
    );
    ui.add_space(8.0);
}

/// The rounded box rows sit in; rows after the first carry a hairline.
pub(crate) fn boxed(ui: &mut egui::Ui, palette: &Palette, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.subtle))
        .corner_radius(egui::CornerRadius::same(R_LG as u8))
        .show_crisp(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui);
        });
}

/// The hairline every row carries at its top; under the box's own
/// border the first row's doubles it, as the reference draws it.
pub(crate) fn row_rule(ui: &egui::Ui, rect: egui::Rect, palette: &Palette) {
    ui.painter().hline(
        rect.x_range(),
        rect.top() + 0.5,
        egui::Stroke::new(1.0, palette.subtle),
    );
}

/// Lays out one settings row: the label over its wrapped hint at the
/// left, at least 58px tall and taller when the hint wraps, leaving
/// `reserve` at the right for the control. Gives back the row's
/// response under `sense` and the slot the control goes in.
#[allow(clippy::too_many_arguments)]
fn row_frame(
    ui: &mut egui::Ui,
    label: &str,
    badge: Option<&str>,
    hint: &str,
    reserve: f32,
    control_h: f32,
    sense: egui::Sense,
    palette: &Palette,
) -> (egui::Response, egui::Rect) {
    let width = ui.available_width();
    let text_w = (width - 32.0 - 16.0 - reserve).max(80.0);
    let wrapped = |ui: &egui::Ui, text: &str, size: f32, color: egui::Color32| {
        let format = egui::TextFormat {
            font_id: egui::FontId::new(size, family(FAMILY_BODY_REGULAR)),
            color,
            line_height: Some(crate::theme::line_h(size)),
            ..Default::default()
        };
        let mut job = egui::text::LayoutJob::single_section(text.to_owned(), format);
        job.wrap.max_width = text_w;
        ui.painter().layout_job(job)
    };
    let name = wrapped(ui, label, FS_SM, palette.ink);
    let help = (!hint.is_empty()).then(|| wrapped(ui, hint, FS_XS, palette.muted));
    let text_h = name.size().y + help.as_ref().map_or(0.0, |help| help.size().y);
    // The reference's row: a 1px top border and 12px above and below,
    // at least 58px, with the text and the control centered on what
    // is inside.
    let content_h = text_h.max(control_h);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, (1.0 + 12.0 + content_h + 12.0).max(58.0)),
        sense,
    );
    row_rule(ui, rect, palette);
    let inner = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 16.0, rect.top() + 13.0),
        egui::pos2(rect.right() - 16.0, rect.bottom() - 12.0),
    );
    let mut y = crate::theme::snap(inner.center().y - text_h / 2.0);
    let name_h = name.size().y;
    let name_w = name.size().x.ceil();
    let shift = crate::theme::half_lead(&name);
    ui.painter()
        .galley(egui::pos2(inner.left(), y + shift), name, palette.ink);
    if let Some(badge) = badge {
        paint_badge(
            ui,
            egui::pos2(
                inner.left() + name_w + 8.0,
                y + crate::theme::line_h(FS_SM) / 2.0,
            ),
            badge,
            10.5,
            palette.muted,
            palette,
        );
    }
    y += name_h;
    if let Some(help) = help {
        let shift = crate::theme::half_lead(&help);
        ui.painter()
            .galley(egui::pos2(inner.left(), y + shift), help, palette.muted);
    }
    // Under the border to the row's foot: centered on the same line
    // as the padded inside, and the row's full height, so the control's
    // scope leaves the cursor below the row.
    let slot = egui::Rect::from_min_max(
        egui::pos2(inner.left() + text_w + 16.0, rect.top() + 1.0),
        egui::pos2(inner.right(), rect.bottom()),
    );
    (response, slot)
}

/// One labeled row with its control drawn by `control` at the far end.
/// The control's width from the last frame decides how wide the text
/// may run.
pub(crate) fn row(
    ui: &mut egui::Ui,
    label: &str,
    hint: &str,
    control: impl FnOnce(&mut egui::Ui),
    palette: &Palette,
) {
    row_with(ui, label, None, hint, control, palette);
}

/// A row as [`row`] draws it, with an optional badge after its label.
pub(crate) fn row_with(
    ui: &mut egui::Ui,
    label: &str,
    badge: Option<&str>,
    hint: &str,
    control: impl FnOnce(&mut egui::Ui),
    palette: &Palette,
) {
    let id = ui.id().with(("settings-row", label));
    let (reserve, control_h) = ui
        .data(|data| data.get_temp::<(f32, f32)>(id))
        .unwrap_or((120.0, 0.0));
    let (response, slot) = row_frame(
        ui,
        label,
        badge,
        hint,
        reserve,
        control_h,
        egui::Sense::hover(),
        palette,
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
    // The control draws in a band of last frame's height centered on
    // the slot, a child of the row's own room, so it moves no cursor
    // and a tall control centers on the row as a short one does.
    let center = (slot.top() + slot.bottom()) / 2.0;
    let top = crate::theme::snap(center - control_h / 2.0);
    let band = egui::Rect::from_min_max(
        egui::pos2(slot.left(), top),
        egui::pos2(slot.right(), top + control_h),
    );
    let mut slot_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(band)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    slot_ui.spacing_mut().item_spacing.x = 8.0;
    control(&mut slot_ui);
    let used = slot_ui.min_rect().size();
    ui.data_mut(|data| data.insert_temp(id, (used.x, used.y)));
}

/// Paints the design's switch, a 36 by 20 track with its knob at the
/// end, right-aligned and centered in `slot`.
pub(crate) fn paint_switch(ui: &egui::Ui, slot: egui::Rect, on: bool, palette: &Palette) {
    let rect = egui::Rect::from_min_size(
        egui::pos2(
            slot.right() - 36.0,
            crate::theme::snap(slot.center().y - 10.0),
        ),
        egui::vec2(36.0, 20.0),
    );
    let track = if on {
        palette.accent
    } else {
        palette.fill_hover
    };
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(10), track);
    let knob = egui::pos2(rect.left() + if on { 26.0 } else { 10.0 }, rect.center().y);
    ui.painter().circle_filled(knob, 8.0, egui::Color32::WHITE);
}

/// One toggle row: the label, its hint and the switch. The whole row
/// answers, and the new value is for the caller to compare against
/// the stored one.
pub(crate) fn toggle_row(
    ui: &mut egui::Ui,
    label: &str,
    hint: &str,
    on: &mut bool,
    palette: &Palette,
) {
    let (response, slot) = row_frame(
        ui,
        label,
        None,
        hint,
        36.0,
        20.0,
        egui::Sense::click(),
        palette,
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let state = *on;
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, state, label));
    paint_switch(ui, slot, *on, palette);
    if response.clicked() {
        *on = !*on;
    }
}

/// Paints a rounded badge with its left edge at `at.x`, centered on
/// `at.y`, and gives back where it landed.
pub(crate) fn paint_badge(
    ui: &egui::Ui,
    at: egui::Pos2,
    text: &str,
    size: f32,
    ink: egui::Color32,
    palette: &Palette,
) -> egui::Rect {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(size, family(FAMILY_BODY_MEDIUM)),
        ink,
    );
    // The reference's badge: 7px and a 1px border either side, a pixel
    // and the border above and below one line of text.
    let line = crate::theme::line_h(size);
    let height = line + 4.0;
    let rect = egui::Rect::from_min_size(
        egui::pos2(at.x, crate::theme::snap(at.y - height / 2.0)),
        egui::vec2(galley.size().x.ceil() + 16.0, height),
    );
    let radius = egui::CornerRadius::same((rect.height() / 2.0) as u8);
    ui.painter().rect_filled(rect, radius, palette.fill);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, palette.line),
        egui::StrokeKind::Inside,
    );
    let y = crate::theme::line_y(rect.top() + 2.0, line, &galley);
    ui.painter()
        .galley(egui::pos2(rect.left() + 8.0, y), galley, ink);
    rect
}

/// A shortcut table: one row per entry, the keys as caps at the right.
fn table(ui: &mut egui::Ui, rows: &[(String, String)], palette: &Palette) {
    boxed(ui, palette, |ui| {
        for (label, keys) in rows {
            // The top border, 8px above and below, and one 21px line:
            // the label's, and the caps'.
            let line = crate::theme::line_h(FS_SM);
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 1.0 + 8.0 + line + 8.0),
                egui::Sense::hover(),
            );
            response
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
            row_rule(ui, rect, palette);
            let top = rect.top() + 9.0;
            let name = ui.painter().layout_no_wrap(
                label.clone(),
                egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
                palette.ink,
            );
            let y = crate::theme::line_y(top, line, &name);
            ui.painter()
                .galley(egui::pos2(rect.left() + 16.0, y), name, palette.ink);
            let mut right = rect.right() - 16.0;
            for cap in keys.split(' ').rev() {
                right =
                    key_cap_at(ui, egui::pos2(right, top + line / 2.0), cap, palette).left() - 4.0;
            }
        }
    });
}

/// One key cap with its right edge at `at.x`, centered on `at.y`: a
/// hairline box over the soft fill, in the mono family. Gives back
/// where it landed.
fn key_cap_at(ui: &egui::Ui, at: egui::Pos2, text: &str, palette: &Palette) -> egui::Rect {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(10.5, family(FAMILY_MONO_REGULAR)),
        palette.muted,
    );
    // 5px and a 1px border either side, a pixel and the border above
    // and below one line.
    let line = crate::theme::line_h(10.5);
    let size = egui::vec2(galley.size().x.ceil() + 12.0, line + 4.0);
    let rect = egui::Rect::from_min_size(
        egui::pos2(at.x - size.x, crate::theme::snap(at.y - size.y / 2.0)),
        size,
    );
    let id = ui
        .id()
        .with(("key-cap", text, rect.min.x.to_bits(), rect.min.y.to_bits()));
    ui.interact(rect, id, egui::Sense::hover())
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    let radius = egui::CornerRadius::same(4);
    ui.painter().rect_filled(rect, radius, palette.fill);
    ui.painter().rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, palette.line),
        egui::StrokeKind::Inside,
    );
    let y = crate::theme::line_y(rect.top() + 2.0, line, &galley);
    ui.painter()
        .galley(egui::pos2(rect.left() + 6.0, y), galley, palette.muted);
    rect
}

/// One outlined badge drawn at the cursor.
fn badge(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(10.5, family(FAMILY_BODY_MEDIUM)),
        palette.muted,
    );
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(
            galley.size().x.ceil() + 16.0,
            crate::theme::line_h(10.5) + 4.0,
        ),
        egui::Sense::hover(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    paint_badge(ui, rect.left_center(), text, 10.5, palette.muted, palette);
}

#[cfg(test)]
mod tests {
    use super::{Section, Settings, SettingsEvent, shortcuts};

    #[test]
    fn every_section_has_a_label_and_an_icon() {
        for section in Section::ALL {
            assert!(!section.label().is_empty(), "{section:?} has a label");
            let _ = section.icon();
        }
        assert_eq!(Section::ALL.len(), 10);
    }

    #[test]
    fn the_config_sections_ask_for_a_snapshot() {
        for section in Section::ALL {
            let reads = matches!(
                section,
                Section::Providers | Section::Mcp | Section::Permissions | Section::Plugins
            );
            assert_eq!(section.reads_config(), reads, "{section:?}");
        }
        assert!(Section::Providers.reads_config());
        assert!(!Section::General.reads_config());
    }

    #[test]
    fn the_dialog_opens_on_a_section_and_closes() {
        let mut settings = Settings::new();
        assert!(!settings.is_open());
        settings.open(Section::Providers);
        assert!(settings.is_open());
        assert_eq!(settings.section(), Section::Providers);
        settings.go(Section::About);
        assert_eq!(settings.section(), Section::About);
        settings.close();
        assert!(!settings.is_open());
    }

    #[test]
    fn the_shortcut_table_lists_every_app_binding_once() {
        let rows = shortcuts();
        let labels: Vec<&str> = rows.iter().map(|row| row.label).collect();
        for wanted in [
            "Command palette",
            "New session",
            "Toggle sidebar",
            "Toggle workbench",
            "Settings",
            "Find in chat",
            "Send, or steer the running turn",
            "Quit",
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
            .expect("the palette is bound");
        let expected = if cfg!(target_os = "macos") {
            "Cmd K"
        } else {
            "Ctrl K"
        };
        assert_eq!(palette.keys, expected);
        let settings = rows
            .iter()
            .find(|row| row.label == "Settings")
            .expect("settings is bound");
        let expected = if cfg!(target_os = "macos") {
            "Cmd ,"
        } else {
            "Ctrl ,"
        };
        assert_eq!(settings.keys, expected);
    }

    #[test]
    fn the_events_name_their_preference_field() {
        assert_eq!(
            SettingsEvent::Flip("enter_sends"),
            SettingsEvent::Flip("enter_sends")
        );
        assert_ne!(
            SettingsEvent::Flip("enter_sends"),
            SettingsEvent::Flip("fuel")
        );
    }
}
