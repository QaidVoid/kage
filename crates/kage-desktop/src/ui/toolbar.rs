//! The composer's toolbar: the plus popover, the mode, model and
//! thinking pickers, the context ring and the send or stop control.
//!
//! Each picker is an egui window over the pill that opens it, holding
//! a search field and a highlighted row. The rows come from the
//! store's config options and model catalog, so the toolbar names
//! only what the agent advertised and dims what it did not.

use crate::icons::Icon;
use crate::icons::PaintSnapped as _;
use crate::store::Store;
use crate::theme::{
    FAMILY_BODY_REGULAR, FAMILY_BODY_SEMIBOLD, FAMILY_MONO_REGULAR, FS_2XS, FS_SM, FS_XS, Palette,
    R_FULL, R_LG, R_MD, SP_2, family,
};
use crate::ui::forms::ShowCrisp as _;
use crate::ui::pickers;

/// The toolbar's height, the old row's 30px pills.
const PILL_H: f32 = 30.0;
/// The send and stop circle's diameter.
const SEND_D: f32 = 32.0;

/// The popover surface's width.
const MENU_W: f32 = 440.0;

/// The model picker's surface width.
const MODEL_W: f32 = 400.0;

/// The gauge card's height, for deciding which side of its pill it
/// opens on.
const GAUGE_H: f32 = 320.0;

/// The model picker's card width, as the reference draws it.
const PICKER_W: f32 = 380.0;

/// The mode picker's card width, as the reference draws it.
const MODE_PICKER_W: f32 = 440.0;

/// The model list's tallest scroll, as the reference caps it.
const PICKER_LIST_H: f32 = 360.0;

/// The model card's height below its list: the thinking level and the notes.
const PICKER_FOOT_H: f32 = 200.0;

/// The mode card's height, for picking the side it opens on.
const MODE_PICKER_H: f32 = 420.0;

/// The plus card's height, for picking the side it opens on.
const PLUS_H: f32 = 420.0;

/// Which picker popover shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picker {
    /// The permission mode list.
    Mode,
    /// The model and thinking list.
    Model,
}

/// What the toolbar's controls asked the shell to carry out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarEvent {
    /// Open the goal dialog.
    OpenGoal,
    /// Toggle plan mode.
    TogglePlan,
    /// Open the swarm confirmation.
    ConfirmSwarm,
    /// Turn swarm mode off.
    SwarmOff,
    /// Seed an at-mention in the composer's draft.
    SeedMention,
    /// Seed a slash command in the composer's draft.
    SeedSlash,
    /// Interrupt the run in flight.
    Cancel,
    /// Summarize older turns to free context.
    Compact,
    /// Send or queue the draft.
    Submit,
    /// Open the providers section of settings.
    OpenProviders,
}

/// The toolbar's state: which popover is open and each picker's
/// search field and highlight.
#[derive(Debug, Default)]
pub struct Toolbar {
    /// Which picker shows, if any.
    picker: Option<Picker>,
    /// The mode picker's query.
    mode_query: String,
    /// The model picker's query.
    model_query: String,
    /// The mode picker's highlighted row.
    mode_highlight: usize,
    /// The model picker's highlighted row.
    model_highlight: usize,
    /// Whether the plus popover shows.
    plus_open: bool,
    /// Whether the context gauge shows.
    gauge_open: bool,
    /// Whether the model picker still owes the current model its
    /// highlight and a scroll into view, as it does once it opens.
    reveal_model: bool,
}

impl Toolbar {
    /// A closed toolbar.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Which picker shows, if any.
    #[must_use]
    pub fn picker(&self) -> Option<Picker> {
        self.picker
    }

    /// The mode picker's query.
    pub fn mode_query(&mut self) -> &mut String {
        &mut self.mode_query
    }

    /// The model picker's query.
    pub fn model_query(&mut self) -> &mut String {
        &mut self.model_query
    }

    /// Opens `which`, closing the other, or closes it when it shows.
    pub fn toggle_picker(&mut self, which: Picker) {
        self.picker = match self.picker {
            Some(open) if open == which => None,
            _ => Some(which),
        };
        if self.picker == Some(Picker::Mode) {
            self.mode_query.clear();
        }
    }

    /// Closes both pickers.
    pub fn close_pickers(&mut self) {
        self.picker = None;
    }

    /// Whether the plus popover shows.
    #[must_use]
    pub fn plus_open(&self) -> bool {
        self.plus_open
    }

    /// Toggles the plus popover.
    pub fn toggle_plus(&mut self) {
        self.plus_open = !self.plus_open;
    }

    /// Closes the plus popover.
    pub fn close_plus(&mut self) {
        self.plus_open = false;
    }

    /// Whether the context gauge shows.
    #[must_use]
    pub fn gauge_open(&self) -> bool {
        self.gauge_open
    }

    /// Toggles the context gauge.
    pub fn toggle_gauge(&mut self) {
        self.gauge_open = !self.gauge_open;
    }

    /// Closes the context gauge.
    pub fn close_gauge(&mut self) {
        self.gauge_open = false;
    }

    /// The mode picker's highlighted row.
    pub const fn mode_highlight(&self) -> usize {
        self.mode_highlight
    }

    /// The model picker's highlighted row.
    pub const fn model_highlight(&self) -> usize {
        self.model_highlight
    }

    /// Moves the mode picker's highlight to `index`, as hovering a row
    /// does.
    pub fn set_mode_highlight(&mut self, index: usize) {
        self.mode_highlight = index;
    }

    /// Steps the mode picker's highlight.
    pub fn step_mode(&mut self, delta: isize, total: usize) {
        self.mode_highlight = pickers::step_highlight(self.mode_highlight, total, delta);
    }

    /// Steps the model picker's highlight.
    pub fn step_model(&mut self, delta: isize, total: usize) {
        self.model_highlight = pickers::step_highlight(self.model_highlight, total, delta);
    }

    /// Whether any popover over the toolbar shows, so the shell knows
    /// the composer's keys are not the composer's to answer.
    #[must_use]
    pub fn any_open(&self) -> bool {
        self.picker.is_some() || self.plus_open || self.gauge_open
    }
}

/// The active permission mode's display name: the mode option's row
/// for `active`, else the raw value.
#[must_use]
pub fn mode_label(store: &Store, active: &str) -> String {
    store
        .composer_option("mode")
        .and_then(|option| {
            option
                .options
                .iter()
                .find(|value| value.value == active)
                .map(|value| value.name.clone())
        })
        .unwrap_or_else(|| active.to_owned())
}

/// The model pill's label: the model option's row for its current
/// value, else the value's last path segment.
#[must_use]
pub fn model_label(store: &Store) -> String {
    let Some(option) = store.composer_option("model") else {
        return "default".to_owned();
    };
    option
        .options
        .iter()
        .find(|value| value.value == option.current_value)
        .map(|value| value.name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            option
                .current_value
                .rsplit('/')
                .next()
                .unwrap_or(&option.current_value)
                .to_owned()
        })
}

/// The thinking pill's trailing label, or `None` while thinking is
/// off or unadvertised.
#[must_use]
pub fn thinking_label(store: &Store) -> Option<String> {
    let option = store.composer_option("thinking")?;
    if option.current_value == "off" {
        return None;
    }
    Some(
        option
            .options
            .iter()
            .find(|value| value.value == option.current_value)
            .map(|value| value.name.clone())
            .unwrap_or_else(|| option.current_value.clone()),
    )
}

/// One part of a toolbar pill, in the order it reads left to right.
enum Part {
    /// An icon at its size and tint.
    Icon(Icon, f32, egui::Color32),
    /// A run of text in its font and color.
    Text(String, egui::FontId, egui::Color32),
    /// A square of this side the caller paints into.
    Room(f32),
}

/// One toolbar pill as the reference sets it: 30px tall, fully round,
/// `pad` either side and 6px between its parts, every part centered on
/// the pill's height. Answers the pill's response and the rects of its
/// rooms, in order.
fn pill(
    ui: &mut egui::Ui,
    open: bool,
    palette: &Palette,
    pad: f32,
    parts: Vec<Part>,
) -> (egui::Response, Vec<egui::Rect>) {
    const GAP: f32 = 6.0;
    let mut label = String::new();
    let laid: Vec<(Part, Option<std::sync::Arc<egui::Galley>>)> = parts
        .into_iter()
        .map(|part| {
            let galley = match &part {
                Part::Text(text, font, color) => {
                    if !label.is_empty() {
                        label.push(' ');
                    }
                    label.push_str(text);
                    Some(
                        ui.painter()
                            .layout_no_wrap(text.clone(), font.clone(), *color),
                    )
                }
                _ => None,
            };
            (part, galley)
        })
        .collect();
    let width_of = |part: &Part, galley: &Option<std::sync::Arc<egui::Galley>>| match part {
        Part::Icon(_, size, _) | Part::Room(size) => *size,
        Part::Text(..) => galley.as_ref().map_or(0.0, |galley| galley.size().x.ceil()),
    };
    let inner: f32 = laid
        .iter()
        .map(|(part, galley)| width_of(part, galley))
        .sum::<f32>()
        + GAP * laid.len().saturating_sub(1) as f32;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(pad + inner + pad, PILL_H), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    if open || response.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_FULL as u8), palette.hover);
    }
    let mid = rect.center().y;
    let mut x = rect.left() + pad;
    let mut rooms = Vec::new();
    for (part, galley) in laid {
        let width = width_of(&part, &galley);
        match part {
            Part::Icon(icon, size, tint) => {
                crate::icons::themed_image(icon, size, tint).paint_snapped(
                    ui,
                    egui::Rect::from_min_size(
                        egui::pos2(x, mid - size / 2.0),
                        egui::vec2(size, size),
                    ),
                );
            }
            Part::Text(_, _, color) => {
                if let Some(galley) = galley {
                    let y = crate::theme::line_y(rect.top(), PILL_H, &galley);
                    ui.painter().galley(egui::pos2(x, y), galley, color);
                }
            }
            Part::Room(side) => rooms.push(egui::Rect::from_min_size(
                egui::pos2(x, mid - side / 2.0),
                egui::vec2(side, side),
            )),
        }
        x += width + GAP;
    }
    (response, rooms)
}

/// One toolbar pill that only carries a text label, dimmed.
fn dimmed_pill(ui: &mut egui::Ui, label: &str, palette: &Palette, tip: &str) -> egui::Response {
    egui::Frame::NONE
        .corner_radius(egui::CornerRadius::same(R_FULL as u8))
        .inner_margin(egui::Margin::symmetric(9, 0))
        .show(ui, |ui| {
            ui.set_min_height(PILL_H);
            ui.label(
                egui::RichText::new(label)
                    .size(FS_XS + 1.0)
                    .color(palette.faint),
            );
        })
        .response
        .on_hover_text(tip)
}

/// Where the picker card `id` opens from its pill: 6px above the
/// pill, as the reference overlay hangs it, or below when the window
/// has no room above. The card's height is the one it drew last
/// frame, or `fallback` before it ever drew. Its left edge sits on the
/// pill's, or its right edge when `right` is set.
fn picker_anchor(
    ctx: &egui::Context,
    id: egui::Id,
    pill: egui::Rect,
    fallback: f32,
    right: bool,
) -> (egui::Pos2, egui::Align2) {
    let height = ctx
        .memory(|memory| memory.area_rect(id))
        .map_or(fallback, |rect| rect.height());
    let x = if right { pill.right() } else { pill.left() };
    let above = pill.top() - height - 6.0 >= ctx.content_rect().top();
    let (y, v) = if above {
        (pill.top() - 6.0, egui::Align::BOTTOM)
    } else {
        (pill.bottom() + 6.0, egui::Align::TOP)
    };
    let h = if right {
        egui::Align::RIGHT
    } else {
        egui::Align::LEFT
    };
    (egui::pos2(x, y), egui::Align2([h, v]))
}

/// Where the gauge card opens from its pill: its right edge on the
/// pill's, above the pill, or below it when the window has no room
/// above.
fn gauge_anchor(ctx: &egui::Context, pill: egui::Rect) -> (egui::Pos2, egui::Align2) {
    if pill.top() - GAUGE_H - SP_2 < ctx.content_rect().top() {
        (
            pill.right_bottom() + egui::vec2(0.0, SP_2),
            egui::Align2::RIGHT_TOP,
        )
    } else {
        (
            pill.right_top() - egui::vec2(0.0, SP_2),
            egui::Align2::RIGHT_BOTTOM,
        )
    }
}

/// Closes an open picker when the pointer clicks outside its card. A
/// click on the pill that opened it is not outside, since the pill
/// toggles the picker itself.
fn dismiss_outside(toolbar: &mut Toolbar, card: &egui::Response, pill: &egui::Response) {
    if card.clicked_elsewhere() && !pill.clicked() {
        toolbar.close_pickers();
    }
}

/// A small mono chip, as the picker rows and the gauge carry.
fn chip(ui: &mut egui::Ui, text: &str, palette: &Palette) -> egui::Response {
    let text = egui::RichText::new(text)
        .size(10.5)
        .family(family(FAMILY_MONO_REGULAR))
        .color(palette.muted);
    egui::Frame::NONE
        .fill(palette.fill)
        .corner_radius(egui::CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(6, 3))
        .show(ui, |ui| ui.label(text))
        .response
}

/// A picker section's label: the small caps row, as the reference
/// picker groups its list.
fn pop_label(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    let text = egui::RichText::new(text.to_uppercase())
        .size(FS_2XS)
        .family(family(FAMILY_BODY_SEMIBOLD))
        .color(palette.faint);
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 9,
            right: 9,
            top: 6,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.label(text);
        });
}

/// The on/off chip the mode rows carry.
fn onoff(ui: &mut egui::Ui, on: bool, palette: &Palette) {
    let (bg, fg) = if on {
        (palette.accent_soft, palette.accent)
    } else {
        (palette.fill, palette.faint)
    };
    let text = egui::RichText::new(if on { "on" } else { "off" })
        .size(FS_2XS)
        .family(family(FAMILY_MONO_REGULAR))
        .color(fg);
    egui::Frame::NONE
        .fill(bg)
        .corner_radius(egui::CornerRadius::same(R_FULL as u8))
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| ui.label(text));
}

/// The menu surface every popover draws on: the raised fill, the
/// hairline border and the flyout shadow.
pub(crate) fn surface(
    ui: &mut egui::Ui,
    palette: &Palette,
    width: f32,
    body: impl FnOnce(&mut egui::Ui),
) {
    surface_in(ui, palette, palette.menu, 5, width, body);
}

/// The menu surface on `fill`: the pickers draw on the page's own
/// background, as the reference's picker overlay does.
pub(crate) fn surface_in(
    ui: &mut egui::Ui,
    palette: &Palette,
    fill: egui::Color32,
    margin: i8,
    width: f32,
    body: impl FnOnce(&mut egui::Ui),
) {
    // A window clips to the size it remembers, which would cut the
    // shadow off in a hard-edged slab around the surface.
    ui.set_clip_rect(ui.ctx().content_rect());
    egui::Frame::NONE
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(R_LG as u8))
        .inner_margin(egui::Margin::same(margin))
        .show_shadowed(ui, &palette.shadow_menu, |ui| {
            ui.style_mut().interaction.selectable_labels = false;
            ui.allocate_ui_with_layout(
                egui::vec2(width, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                body,
            );
        });
}

/// A key cap, as the hint line draws it.
fn kbd(ui: &mut egui::Ui, label: &str, palette: &Palette) {
    let text = egui::RichText::new(label)
        .size(10.0)
        .family(family(FAMILY_MONO_REGULAR))
        .color(palette.faint);
    egui::Frame::NONE
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(5, 2))
        .show_crisp(ui, |ui| ui.label(text));
}

/// The toolbar row: the plus popover, the mode pill with its plan and
/// swarm chips, the compact and context controls, and the model pill.
/// Returns what the controls asked the shell to carry out.
pub fn show(
    ui: &mut egui::Ui,
    toolbar: &mut Toolbar,
    store: &mut Store,
    running: bool,
    has_text: bool,
) -> Vec<ToolbarEvent> {
    let mut events = Vec::new();
    // The row is as tall as its tallest control, the send circle, and
    // the pills center on it.
    let (row, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), SEND_D),
        egui::Sense::hover(),
    );
    let layout = egui::Layout::left_to_right(egui::Align::Center);
    ui.scope_builder(egui::UiBuilder::new().max_rect(row).layout(layout), |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(SP_2, 0.0);
        plus_button(ui, toolbar, store, &mut events);
        mode_button(ui, toolbar, store, &mut events);
        plan_chip(ui, store, &mut events);
        swarm_chip(ui, store, &mut events);
        ui.add_space(SP_2);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            send_button(ui, running, has_text, &mut events);
            // The send circle's own 4px margin on its left.
            ui.add_space(4.0);
            model_button(ui, toolbar, store, &mut events);
            compact_button(ui, store, running, &mut events);
            context_ring(ui, toolbar, store, running, &mut events);
        });
    });
    events
}

/// The plus pill and its popover: attach, mention, commands and the
/// mode toggles.
fn plus_button(
    ui: &mut egui::Ui,
    toolbar: &mut Toolbar,
    store: &Store,
    events: &mut Vec<ToolbarEvent>,
) -> Option<()> {
    let palette = Palette::active_of(ui.ctx());
    let open = toolbar.plus_open();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(PILL_H, PILL_H), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let lit = open || response.hovered();
    if lit {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_FULL as u8), palette.hover);
    }
    let glyph = egui::Rect::from_center_size(rect.center(), egui::vec2(16.0, 16.0));
    crate::icons::themed_image(
        Icon::Plus,
        16.0,
        if lit { palette.ink } else { palette.muted },
    )
    .paint_snapped(ui, glyph);
    let response = response.on_hover_text("Add");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Add"));
    if response.clicked() {
        toolbar.toggle_plus();
    }
    if !open {
        return Some(());
    }
    let goal = pickers::session_goal(store.active_session());
    let plan_on = store.plan_on();
    let swarm_on = store.swarm_on();
    let swarm_known = store.composer_option("swarm").is_some();
    let (at, pivot) = picker_anchor(
        ui.ctx(),
        egui::Id::new("plus-popover"),
        response.rect,
        PLUS_H,
        false,
    );
    let card = egui::Window::new("plus-popover")
        .id(egui::Id::new("plus-popover"))
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .fixed_pos(at)
        .pivot(pivot)
        .min_width(MENU_W)
        .order(egui::Order::Foreground)
        .frame(egui::Frame::NONE)
        .show(ui.ctx(), |ui| {
            surface(ui, palette, MENU_W, |ui| {
                ui.set_max_height(420.0);
                pop_label(ui, "Attach", palette);
                plus_row(
                    ui,
                    palette,
                    (Icon::Paperclip, "Files", "Upload files or images"),
                    Tail::Kbd("drop"),
                    Some("the prompt surface carries text only for now"),
                );
                let mention = plus_row(
                    ui,
                    palette,
                    (
                        Icon::AtSign,
                        "Mention",
                        "Project files from the session workdir",
                    ),
                    Tail::Kbd("@"),
                    None,
                );
                if mention.clicked() {
                    events.push(ToolbarEvent::SeedMention);
                    toolbar.close_plus();
                }
                let commands = plus_row(
                    ui,
                    palette,
                    (Icon::Slash, "Commands", "The commands the agent sent"),
                    Tail::Kbd("/"),
                    None,
                );
                if commands.clicked() {
                    events.push(ToolbarEvent::SeedSlash);
                    toolbar.close_plus();
                }
                pop_label(ui, "Modes", palette);
                let goal_row = plus_row(
                    ui,
                    palette,
                    (Icon::Target, "Goal", "Set a goal to keep pursuing"),
                    Tail::Kbd(if goal.is_some() { "set" } else { "none" }),
                    None,
                );
                if goal_row.clicked() {
                    events.push(ToolbarEvent::OpenGoal);
                    toolbar.close_plus();
                }
                let plan = plus_row(
                    ui,
                    palette,
                    (
                        Icon::PenLine,
                        "Plan",
                        if plan_on {
                            "Turn plan mode off"
                        } else {
                            "Turn plan mode on"
                        },
                    ),
                    Tail::OnOff(plan_on),
                    None,
                );
                if plan.clicked() {
                    events.push(ToolbarEvent::TogglePlan);
                    toolbar.close_plus();
                }
                if swarm_known {
                    let swarm = plus_row(
                        ui,
                        palette,
                        (
                            Icon::Waypoints,
                            "Swarm",
                            if swarm_on {
                                "Turn swarm mode off"
                            } else {
                                "Turn swarm mode on"
                            },
                        ),
                        Tail::OnOff(swarm_on),
                        None,
                    );
                    if swarm.clicked() {
                        if swarm_on {
                            events.push(ToolbarEvent::SwarmOff);
                        } else {
                            events.push(ToolbarEvent::ConfirmSwarm);
                        }
                        toolbar.close_plus();
                    }
                } else {
                    plus_row(
                        ui,
                        palette,
                        (Icon::Waypoints, "Swarm", "The agent sent no swarm option"),
                        Tail::None,
                        Some("the agent sent no swarm config option"),
                    );
                }
            });
        });
    let escape = ui.input(|input| input.key_pressed(egui::Key::Escape));
    if let Some(card) = card
        && (escape || (card.response.clicked_elsewhere() && !response.clicked()))
    {
        toolbar.close_plus();
    }
    Some(())
}

/// What a plus-menu row carries at its far end.
enum Tail {
    /// A key cap naming the row's shortcut or state.
    Kbd(&'static str),
    /// The on/off chip of a mode toggle.
    OnOff(bool),
    /// Nothing.
    None,
}

/// One plus-menu row: a filled icon chip in front of the label and
/// description, with the tail at the far end. A row with a `dimmed`
/// reason draws faint, answers no click and says why on hover.
/// Returns the row's response, so the caller wires its action.
fn plus_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    (icon, label, description): (Icon, &str, &str),
    tail: Tail,
    dimmed: Option<&str>,
) -> egui::Response {
    let id = egui::Id::new("plus-row").with(label);
    let hovered = dimmed.is_none() && ui.ctx().read_response(id).is_some_and(|r| r.hovered());
    let ink = if dimmed.is_some() {
        palette.faint
    } else {
        palette.ink
    };
    let inner = egui::Frame::NONE
        .fill(if hovered {
            palette.selected
        } else {
            egui::Color32::TRANSPARENT
        })
        .corner_radius(egui::CornerRadius::same(R_MD as u8))
        .inner_margin(egui::Margin::symmetric(7, 6))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (chip, _) =
                    ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::hover());
                ui.painter()
                    .rect_filled(chip, egui::CornerRadius::same(7), palette.fill);
                crate::icons::themed_image(
                    icon,
                    14.0,
                    if dimmed.is_some() {
                        palette.faint
                    } else {
                        palette.muted
                    },
                )
                .paint_snapped(
                    ui,
                    egui::Rect::from_center_size(chip.center(), egui::vec2(14.0, 14.0)),
                );
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(label).size(FS_XS + 1.0).color(ink));
                    ui.label(
                        egui::RichText::new(description)
                            .size(11.5)
                            .color(palette.faint),
                    );
                });
                // The tail sits in an 18px band, as the reference's chips
                // are 18 tall, so the row's height never stretches it.
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 18.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| match tail {
                        Tail::Kbd(cap) => kbd(ui, cap, palette),
                        Tail::OnOff(on) => onoff(ui, on, palette),
                        Tail::None => {}
                    },
                );
            });
        });
    let sense = if dimmed.is_some() {
        egui::Sense::hover()
    } else {
        egui::Sense::click()
    };
    let response = ui.interact(inner.response.rect, id, sense);
    match dimmed {
        Some(reason) => response.on_hover_text(reason),
        None => response.on_hover_cursor(egui::CursorIcon::PointingHand),
    }
}

/// The permission mode pill, listing the advertised values with the
/// active one marked.
fn mode_button(
    ui: &mut egui::Ui,
    toolbar: &mut Toolbar,
    store: &mut Store,
    _events: &mut Vec<ToolbarEvent>,
) -> Option<()> {
    let palette = Palette::active_of(ui.ctx());
    let open = toolbar.picker() == Some(Picker::Mode);
    let Some(option) = store.composer_option("mode") else {
        dimmed_pill(ui, "Mode", palette, "the agent sent no mode config option");
        return Some(());
    };
    let active = store
        .permission_mode()
        .unwrap_or_else(|| option.current_value.clone());
    let label = mode_label(store, &active);
    let tone = pickers::mode_tone(&active, palette);
    let text_color = tone.unwrap_or(palette.muted);
    let (response, _) = pill(
        ui,
        open,
        palette,
        9.0,
        vec![
            Part::Icon(pickers::mode_icon(&active), 14.0, text_color),
            Part::Text(
                label,
                egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
                text_color,
            ),
            Part::Icon(Icon::ChevronDown, 12.0, text_color.gamma_multiply(0.7)),
        ],
    );
    let response = response.on_hover_text("Permission mode (Shift+Tab cycles)");
    if response.clicked() {
        toolbar.toggle_picker(Picker::Mode);
        let rows = pickers::mode_rows(&option.options, "");
        toolbar.set_mode_highlight(rows.iter().position(|row| row.value == active).unwrap_or(0));
    }
    if !open {
        return Some(());
    }
    let (at, pivot) = picker_anchor(
        ui.ctx(),
        egui::Id::new("mode-picker"),
        response.rect,
        MODE_PICKER_H,
        false,
    );
    let card = egui::Window::new("mode-picker")
        .id(egui::Id::new("mode-picker"))
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .fixed_pos(at)
        .pivot(pivot)
        .min_width(MODE_PICKER_W)
        .order(egui::Order::Foreground)
        .frame(egui::Frame::NONE)
        .show(ui.ctx(), |ui| {
            if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                toolbar.close_pickers();
                return;
            }
            surface_in(ui, palette, palette.bg, 0, MODE_PICKER_W, |ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                let query = toolbar.mode_query().clone();
                mode_search_row(ui, toolbar, palette);
                if *toolbar.mode_query() != query {
                    toolbar.set_mode_highlight(0);
                }
                picker_rule(ui, palette);
                let rows = pickers::mode_rows(&option.options, &toolbar.mode_query().clone());
                let mut chosen = None;
                // The keys the reference picker answers while its query
                // holds the keyboard: the highlight steps, Enter chooses.
                let (up, down, run) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::ArrowUp),
                        i.key_pressed(egui::Key::ArrowDown),
                        i.key_pressed(egui::Key::Enter),
                    )
                });
                if up {
                    toolbar.step_mode(-1, rows.len());
                }
                if down {
                    toolbar.step_mode(1, rows.len());
                }
                if run && let Some(row) = rows.get(toolbar.mode_highlight()) {
                    chosen = Some(row.value.clone());
                }
                egui::Frame::NONE
                    .inner_margin(egui::Margin::same(5))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if rows.is_empty() {
                            ui.label(
                                egui::RichText::new("no modes match")
                                    .size(FS_XS)
                                    .color(palette.faint),
                            );
                        }
                        for (index, row) in rows.iter().enumerate() {
                            let on = row.value == active;
                            let response =
                                mode_row(ui, row, index == toolbar.mode_highlight(), on, palette);
                            if response.hovered() {
                                toolbar.set_mode_highlight(index);
                            }
                            if response.clicked() {
                                chosen = Some(row.value.clone());
                            }
                        }
                    });
                if let Some(verdicts) = rows
                    .get(toolbar.mode_highlight())
                    .and_then(|row| pickers::policy(&row.value))
                {
                    picker_rule(ui, palette);
                    policy_foot(ui, verdicts, palette);
                }
                if let Some(value) = chosen {
                    store.set_permission(&value);
                    toolbar.close_pickers();
                }
            });
        });
    if let Some(card) = card {
        dismiss_outside(toolbar, &card.response, &response);
    }
    Some(())
}

/// The search row over the mode picker: the glyph, the query field
/// and the key that cycles the modes.
fn mode_search_row(ui: &mut egui::Ui, toolbar: &mut Toolbar, palette: &Palette) {
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let size = egui::vec2(ui.available_width(), 20.0);
            let layout = egui::Layout::left_to_right(egui::Align::Center);
            ui.allocate_ui_with_layout(size, layout, |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                crate::icons::icon(ui, Icon::Search, 14.0, palette.faint);
                let chip_w = 64.0;
                let query = ui.add(
                    egui::TextEdit::singleline(toolbar.mode_query())
                        .id(egui::Id::new("mode-picker-query"))
                        .hint_text(egui::RichText::new("Permission mode").color(palette.faint))
                        .text_color(palette.ink)
                        .font(egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)))
                        .frame(egui::Frame::NONE)
                        .margin(egui::Margin::ZERO)
                        .desired_width(ui.available_width() - chip_w - 8.0),
                );
                query.request_focus();
                kbd(ui, "Shift Tab", palette);
            });
        });
}

/// A hairline across a picker, between its parts.
fn picker_rule(ui: &mut egui::Ui, palette: &Palette) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, palette.subtle);
}

/// One permission mode row: the glyph in its tile, the name in the
/// mode's tone over its description, the policy dots and the check
/// on the active mode.
fn mode_row(
    ui: &mut egui::Ui,
    row: &kage_client::wire::SessionConfigSelectOption,
    highlighted: bool,
    on: bool,
    palette: &Palette,
) -> egui::Response {
    let line = 1.618;
    let name_h = FS_SM * line;
    let desc_h = if row.description.is_some() {
        11.5 * line
    } else {
        0.0
    };
    let height = 7.0 + (name_h + desc_h).max(26.0) + 7.0;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, on, &row.name)
    });
    if highlighted {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(R_MD as u8), palette.selected);
    }
    let tone = pickers::mode_tone(&row.value, palette);
    let mid = rect.center().y;
    let tile = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 9.0, mid - 13.0),
        egui::vec2(26.0, 26.0),
    );
    ui.painter()
        .rect_filled(tile, egui::CornerRadius::same(7), palette.fill);
    crate::icons::themed_image(
        pickers::mode_icon(&row.value),
        14.0,
        tone.unwrap_or(palette.muted),
    )
    .paint_snapped(
        ui,
        egui::Rect::from_center_size(tile.center(), egui::vec2(14.0, 14.0)),
    );
    let mut right = rect.right() - 9.0;
    if on {
        let check =
            egui::Rect::from_min_size(egui::pos2(right - 14.0, mid - 7.0), egui::vec2(14.0, 14.0));
        crate::icons::themed_image(Icon::Check, 14.0, palette.accent).paint_snapped(ui, check);
        right = check.left() - 10.0;
    }
    if let Some(verdicts) = pickers::policy(&row.value) {
        let width = 6.0 * 7.0 + 5.0 * 3.0;
        for (n, verdict) in verdicts.iter().enumerate() {
            let dot = egui::Rect::from_min_size(
                egui::pos2(right - width + n as f32 * 10.0, mid - 3.5),
                egui::vec2(7.0, 7.0),
            );
            let color = if *verdict == pickers::Verdict::Runs {
                verdict.tone(palette).gamma_multiply(0.8)
            } else {
                verdict.tone(palette)
            };
            ui.painter()
                .rect_filled(dot, egui::CornerRadius::same(2), color);
        }
        right -= width + 10.0;
    }
    let text_x = tile.right() + 10.0;
    let top = mid - (name_h + desc_h) / 2.0;
    let name = ui.painter().layout_no_wrap(
        row.name.clone(),
        egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
        tone.unwrap_or(palette.ink),
    );
    ui.painter().galley(
        egui::pos2(text_x, top + (name_h - name.size().y) / 2.0),
        name,
        palette.ink,
    );
    if let Some(description) = &row.description {
        let format = egui::TextFormat::simple(
            egui::FontId::new(11.5, family(FAMILY_BODY_REGULAR)),
            palette.faint,
        );
        let galley =
            crate::ui::transcript::head::one_line(ui, description.clone(), format, right - text_x);
        ui.painter().galley(
            egui::pos2(text_x, top + name_h + (desc_h - galley.size().y) / 2.0),
            galley,
            palette.faint,
        );
    }
    response
}

/// What the highlighted mode does to each kind of tool call, over the
/// note that config rules still apply.
fn policy_foot(ui: &mut egui::Ui, verdicts: [pickers::Verdict; 6], palette: &Palette) {
    egui::Frame::NONE
        .fill(palette.fill)
        .inner_margin(egui::Margin::same(12))
        .corner_radius(egui::CornerRadius {
            nw: 0,
            ne: 0,
            sw: R_LG as u8,
            se: R_LG as u8,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 8.0;
            let column = ui.available_width() / 3.0;
            for pair in pickers::KINDS
                .iter()
                .zip(verdicts)
                .collect::<Vec<_>>()
                .chunks(3)
            {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 32.0),
                    egui::Sense::hover(),
                );
                for (n, (kind, verdict)) in pair.iter().enumerate() {
                    let left = rect.left() + n as f32 * column;
                    ui.painter().text(
                        egui::pos2(left, rect.top()),
                        egui::Align2::LEFT_TOP,
                        kind.to_uppercase(),
                        egui::FontId::new(10.5, family(FAMILY_BODY_REGULAR)),
                        palette.faint,
                    );
                    ui.painter().text(
                        egui::pos2(left, rect.top() + 16.0),
                        egui::Align2::LEFT_TOP,
                        verdict.word(),
                        egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
                        verdict.tone(palette),
                    );
                }
            }
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "Rules in config.toml still apply on top, and a configured \
                         deny refuses in every mode.",
                    )
                    .size(11.5)
                    .color(palette.muted),
                )
                .wrap(),
            );
        });
}

/// The plan mode chip, shown only while plan mode is on, with the
/// exit button that returns to the previous mode.
fn plan_chip(ui: &mut egui::Ui, store: &Store, events: &mut Vec<ToolbarEvent>) -> Option<()> {
    if !store.plan_on() {
        return Some(());
    }
    let palette = Palette::active_of(ui.ctx());
    if mode_chip(
        ui,
        palette,
        Icon::PenLine,
        "Plan",
        palette.accent,
        palette.accent_soft,
    ) {
        events.push(ToolbarEvent::TogglePlan);
    }
    Some(())
}

/// The swarm mode chip, shown only while swarm mode is on.
fn swarm_chip(ui: &mut egui::Ui, store: &Store, events: &mut Vec<ToolbarEvent>) -> Option<()> {
    if !store.swarm_on() {
        return Some(());
    }
    let palette = Palette::active_of(ui.ctx());
    if mode_chip(
        ui,
        palette,
        Icon::Waypoints,
        "Swarm",
        palette.done,
        palette.done_soft,
    ) {
        events.push(ToolbarEvent::SwarmOff);
    }
    Some(())
}

/// One active-mode chip, as the reference sets it: a 26px pill on the
/// mode's tint holding its icon, its label and the round exit button
/// at its right end. Answers whether the exit was pressed.
fn mode_chip(
    ui: &mut egui::Ui,
    palette: &Palette,
    icon: Icon,
    label: &str,
    fg: egui::Color32,
    bg: egui::Color32,
) -> bool {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::new(FS_XS, family(FAMILY_BODY_SEMIBOLD)),
        fg,
    );
    let size = egui::vec2(9.0 + 12.0 + 5.0 + galley.size().x + 5.0 + 18.0 + 4.0, 26.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let mid = rect.center().y;
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(R_FULL as u8), bg);
    crate::icons::themed_image(icon, 12.0, fg).paint_snapped(
        ui,
        egui::Rect::from_min_size(
            egui::pos2(rect.left() + 9.0, mid - 6.0),
            egui::vec2(12.0, 12.0),
        ),
    );
    let text_x = rect.left() + 9.0 + 12.0 + 5.0;
    ui.painter().galley(
        egui::pos2(text_x, crate::theme::line_y(mid, 0.0, &galley)),
        galley,
        fg,
    );
    let exit = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 4.0 - 9.0, mid),
        egui::vec2(18.0, 18.0),
    );
    let response = ui
        .interact(
            exit,
            ui.id().with(("mode-exit", label)),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Exit mode");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Exit mode"));
    let tint = if response.hovered() {
        ui.painter().rect_filled(
            exit,
            egui::CornerRadius::same(R_FULL as u8),
            palette.fill_hover,
        );
        fg
    } else {
        fg.gamma_multiply(0.7)
    };
    crate::icons::themed_image(Icon::X, 12.0, tint).paint_snapped(
        ui,
        egui::Rect::from_center_size(exit.center(), egui::vec2(12.0, 12.0)),
    );
    response.clicked()
}

/// The model and thinking pill: one control naming the model's label
/// and the thinking level it runs at. Its menu lists the catalog's
/// models and the thinking levels the agent advertised.
fn model_button(
    ui: &mut egui::Ui,
    toolbar: &mut Toolbar,
    store: &mut Store,
    events: &mut Vec<ToolbarEvent>,
) -> Option<()> {
    let palette = Palette::active_of(ui.ctx());
    let open = toolbar.picker() == Some(Picker::Model);
    let model = store.composer_option("model");
    let thinking = store.composer_option("thinking");
    if model.is_none() && thinking.is_none() {
        dimmed_pill(
            ui,
            "Model",
            palette,
            "the agent sent no model or thinking config option",
        );
        return Some(());
    }
    let label = model_label(store);
    let level = thinking_label(store);
    let ink = if level.is_some() {
        palette.ink
    } else {
        palette.muted
    };
    let font = egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR));
    let mut parts = vec![Part::Text(label, font.clone(), ink)];
    if let Some(level) = &level {
        parts.push(Part::Text(format!("\u{b7} {level}"), font, palette.faint));
    }
    parts.push(Part::Icon(Icon::ChevronDown, 12.0, ink.gamma_multiply(0.7)));
    let (response, _) = pill(ui, open, palette, 9.0, parts);
    let response = response.on_hover_text("Model and thinking");
    if response.clicked() {
        toolbar.toggle_picker(Picker::Model);
        if toolbar.picker() == Some(Picker::Model) {
            toolbar.model_query().clear();
            store.ask_models();
            toolbar.reveal_model = true;
        }
    }
    if !open {
        return Some(());
    }
    let starred: Vec<String> = store.prefs().starred_models.iter().cloned().collect();
    // The picker card: the search row over a scrolling list, the
    // thinking level and its notes under a separator, as the reference
    // picker stacks them.
    let (at, pivot) = picker_anchor(
        ui.ctx(),
        egui::Id::new("model-picker"),
        response.rect,
        PICKER_LIST_H + PICKER_FOOT_H,
        true,
    );
    let card = egui::Window::new("model-picker")
        .id(egui::Id::new("model-picker"))
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .fixed_pos(at)
        .pivot(pivot)
        .min_width(PICKER_W)
        .order(egui::Order::Foreground)
        .frame(egui::Frame::NONE)
        .show(ui.ctx(), |ui| {
            surface_in(ui, palette, palette.bg, 5, PICKER_W, |ui| {
                let catalog = store.models().is_some();
                let fallback = store
                    .composer_option("model")
                    .map(|option| option.options)
                    .unwrap_or_default();
                let mut sections = pickers::model_sections(
                    store.models(),
                    Some(fallback),
                    &starred,
                    &toolbar.model_query().clone(),
                );
                pickers::append_external_agents(
                    &mut sections,
                    &store.external_agent_names(),
                    toolbar.model_query(),
                );
                let rows = pickers::flat_rows(&sections);
                let total = rows.len();

                // The keys the reference picker answers, while the
                // search field holds focus: the highlight steps, Enter
                // chooses, Escape leaves.
                let (up, down, run, leave) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::ArrowUp),
                        i.key_pressed(egui::Key::ArrowDown),
                        i.key_pressed(egui::Key::Enter),
                        i.key_pressed(egui::Key::Escape),
                    )
                });
                if leave {
                    toolbar.close_pickers();
                    return;
                }
                let mut reveal = up || down;
                if std::mem::take(&mut toolbar.reveal_model) {
                    let current = store
                        .composer_option("model")
                        .map(|option| option.current_value);
                    toolbar.model_highlight = rows
                        .iter()
                        .position(|row| Some(&row.id) == current.as_ref())
                        .unwrap_or(0);
                    reveal = true;
                }
                if up {
                    toolbar.step_model(-1, total);
                }
                if down {
                    toolbar.step_model(1, total);
                }
                let highlight = if total == 0 {
                    0
                } else {
                    toolbar.model_highlight().min(total - 1)
                };
                if run
                    && total > 0
                    && let Some(row) = rows.get(highlight)
                {
                    store.set_option("model", &row.id);
                    toolbar.close_pickers();
                }

                // The search row: the lens over a borderless field, a
                // separator under both.
                let search = egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 12,
                        top: 10,
                        bottom: 10,
                    })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            crate::icons::icon(ui, Icon::Search, 14.0, palette.faint);
                            let query = ui.add(
                                egui::TextEdit::singleline(toolbar.model_query())
                                    .hint_text("Search models...")
                                    .desired_width(ui.available_width())
                                    .frame(egui::Frame::NONE)
                                    .id(egui::Id::new("model-picker-query")),
                            );
                            query.request_focus();
                            query
                        })
                        .inner
                    });
                if search.inner.changed() {
                    toolbar.model_highlight = 0;
                }
                let line = search.response.rect;
                ui.painter().hline(
                    line.left()..=line.right(),
                    line.bottom(),
                    egui::Stroke::new(1.0, palette.subtle),
                );

                // The list scrolls; the card around it keeps its size.
                egui::ScrollArea::vertical()
                    .max_height(PICKER_LIST_H)
                    .min_scrolled_height(PICKER_LIST_H)
                    .show(ui, |ui| {
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::same(5))
                            .show(ui, |ui| {
                                let mut index = 0;
                                for section in &sections {
                                    pop_label(ui, &section.label, palette);
                                    for row in &section.rows {
                                        let highlighted = index == highlight;
                                        let top = ui.cursor().top();
                                        let ask = model_row(ui, row, highlighted, store, palette);
                                        if highlighted && reveal {
                                            let rows = top..=ui.cursor().top();
                                            ui.scroll_to_rect(
                                                egui::Rect::from_x_y_ranges(
                                                    ui.max_rect().x_range(),
                                                    rows,
                                                ),
                                                None,
                                            );
                                        }
                                        match ask {
                                            RowAsk::Star => store.toggle_star(&row.id),
                                            RowAsk::Choose => {
                                                store.set_option("model", &row.id);
                                                toolbar.close_pickers();
                                            }
                                            RowAsk::None => {}
                                        }
                                        index += 1;
                                    }
                                }
                                if index == 0 {
                                    egui::Frame::NONE.inner_margin(egui::Margin::same(14)).show(
                                        ui,
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new(if catalog {
                                                    "No models match"
                                                } else {
                                                    "Loading models..."
                                                })
                                                .size(FS_SM)
                                                .color(palette.faint),
                                            );
                                        },
                                    );
                                }
                            });
                    });

                // The foot: thinking level, the cache note, the
                // providers door, under a separator.
                let foot_top = ui.cursor().top();
                let foot = egui::Frame::NONE
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        if let Some(thinking) = store.composer_option("thinking") {
                            thinking_row(ui, store, palette, thinking.current_value.as_str());
                        }
                        ui.label(
                            egui::RichText::new(
                                "Switching the model or thinking level mid-session \
                                 invalidates the prompt cache.",
                            )
                            .size(FS_2XS)
                            .color(palette.faint),
                        );
                        let providers = egui::Frame::NONE
                            .fill(palette.fill)
                            .corner_radius(egui::CornerRadius::same(R_MD as u8))
                            .inner_margin(egui::Margin::symmetric(9, 5))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    crate::icons::icon(ui, Icon::Settings, 12.0, palette.muted);
                                    ui.label(
                                        egui::RichText::new("Manage providers")
                                            .size(FS_XS)
                                            .color(palette.ink),
                                    );
                                });
                            })
                            .response;
                        let id = egui::Id::new("model-providers");
                        if ui
                            .interact(providers.rect, id, egui::Sense::click())
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            toolbar.close_pickers();
                            events.push(ToolbarEvent::OpenProviders);
                        }
                    });
                let line = foot.response.rect;
                ui.painter().hline(
                    line.left()..=line.right(),
                    foot_top,
                    egui::Stroke::new(1.0, palette.subtle),
                );
            });
        });
    if let Some(card) = card {
        dismiss_outside(toolbar, &card.response, &response);
    }
    Some(())
}

/// One catalog row: the provider tile, the name over its slug, the
/// context and cost chips and the star, as the reference row carries
/// them. The whole row chooses; the star toggles on its own.
fn model_row(
    ui: &mut egui::Ui,
    row: &pickers::ModelRow,
    highlighted: bool,
    store: &Store,
    palette: &Palette,
) -> RowAsk {
    let starred = store.prefs().starred_models.contains(&row.id);
    let current = store
        .composer_option("model")
        .is_some_and(|option| option.current_value == row.id);
    let row_id = egui::Id::new("model-row").with(row.id.as_str());
    let hovered = ui.ctx().read_response(row_id).is_some_and(|r| r.hovered());
    let mut star_asked = false;
    let frame = egui::Frame::NONE
        .fill(if highlighted || hovered {
            palette.selected
        } else {
            egui::Color32::TRANSPARENT
        })
        .corner_radius(egui::CornerRadius::same(R_MD as u8))
        .inner_margin(egui::Margin {
            left: 9,
            right: 9,
            top: 7,
            bottom: 7,
        })
        .show(ui, |ui| {
            // The row spans the list's whole width.
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                // The chips' width, measured so the name over the slug
                // takes everything the chips leave, the way the
                // reference's flex row shares the row.
                let mut right_w = 16.0_f32;
                if let Some((input, output)) = row.cost {
                    right_w += 6.0 + chip_text_w(ui, &format!("${input:.1}/${output:.1}"));
                }
                if let Some(context) = row.context {
                    right_w += 6.0 + chip_text_w(ui, &pickers::window_size(context));
                }
                let middle_w = (ui.available_width() - 22.0 - 20.0 - right_w).max(80.0);
                // The provider tile: a fixed 22 square, the letters
                // centered in it.
                ui.allocate_ui_with_layout(
                    egui::vec2(22.0, 22.0),
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| {
                        egui::Frame::NONE
                            .fill(palette.fill)
                            .corner_radius(egui::CornerRadius::same(6))
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(pickers::provider_tile(&row.provider))
                                        .size(10.0)
                                        .family(family(FAMILY_MONO_REGULAR))
                                        .color(palette.muted),
                                );
                            });
                    },
                );
                // The name over the slug, the block that truncates.
                let mut detail = row.model.clone();
                if row.images {
                    detail.push_str(" \u{b7} reads images");
                }
                ui.allocate_ui_with_layout(
                    egui::vec2(middle_w, 36.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.label(
                                egui::RichText::new(row.name.clone())
                                    .size(FS_SM)
                                    .color(palette.ink),
                            );
                            if current {
                                crate::icons::icon(ui, Icon::Check, 13.0, palette.accent);
                            }
                        });
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(detail)
                                    .size(FS_2XS)
                                    .family(family(FAMILY_MONO_REGULAR))
                                    .color(palette.faint),
                            )
                            .truncate(),
                        );
                    },
                );
                // The star, the cost and the context window, flush
                // with the row's right edge.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let star = egui::Frame::NONE
                        .inner_margin(egui::Margin::same(2))
                        .corner_radius(egui::CornerRadius::same(4))
                        .show(ui, |ui| {
                            crate::icons::icon(
                                ui,
                                if starred { Icon::StarFill } else { Icon::Star },
                                12.0,
                                if starred { palette.warn } else { palette.ghost },
                            );
                        })
                        .response;
                    let star_id = egui::Id::new("model-star").with(row.id.as_str());
                    if ui
                        .interact(star.rect, star_id, egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        star_asked = true;
                    }
                    if let Some((input, output)) = row.cost {
                        ui.label(
                            egui::RichText::new(format!("${input:.1}/${output:.1}"))
                                .size(FS_2XS)
                                .family(family(FAMILY_MONO_REGULAR))
                                .color(palette.faint),
                        );
                    }
                    if let Some(context) = row.context {
                        chip(ui, &pickers::window_size(context), palette);
                    }
                });
            });
        })
        .response;
    if star_asked {
        return RowAsk::Star;
    }
    if ui
        .interact(frame.rect, row_id, egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        return RowAsk::Choose;
    }
    RowAsk::None
}

/// The painted width of a chip's text, its padding aside, so row
/// layouts can reserve for it.
fn chip_text_w(ui: &egui::Ui, text: &str) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::new(10.5, family(FAMILY_MONO_REGULAR)),
        egui::Color32::WHITE,
    );
    galley.size().x + 12.0
}

/// What one row's click asked for: the star alone, the whole row, or
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAsk {
    /// The star was clicked: toggle it, keep the picker open.
    Star,
    /// The row was clicked: choose the model, close the picker.
    Choose,
    /// Nothing was clicked.
    None,
}

/// The thinking level row: the bulb over its label, then one segment
/// per level, the ones the chosen model does not take dimmed, as the
/// reference foot draws them.
fn thinking_row(ui: &mut egui::Ui, store: &mut Store, palette: &Palette, current: &str) {
    let Some(option) = store.composer_option("thinking") else {
        return;
    };
    let takes = store
        .composer_option("model")
        .and_then(|option| pickers::model_levels(store, &option.current_value));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        crate::icons::icon(ui, Icon::Lightbulb, 12.0, palette.muted);
        ui.label(
            egui::RichText::new("Thinking level")
                .size(FS_XS)
                .color(palette.muted),
        );
    });
    let count = option.options.len().max(1);
    let seg_w = (ui.available_width() - 4.0 - (count as f32 - 1.0) * 2.0) / count as f32;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for level in &option.options {
            let on = level.value == current;
            let enabled = level.value == "default"
                || takes.as_ref().is_none_or(|levels| {
                    levels.contains(&level.value) || (levels.is_empty() && level.value == "off")
                });
            let ink = if on {
                palette.ink_strong
            } else {
                palette.muted
            };
            let ink = if enabled {
                ink
            } else {
                ink.gamma_multiply(0.35)
            };
            let seg = egui::Frame::NONE
                .fill(if on { palette.raised } else { palette.fill })
                .corner_radius(egui::CornerRadius::same(6))
                .show(ui, |ui| {
                    ui.set_min_width(seg_w);
                    ui.set_max_width(seg_w);
                    ui.set_min_height(24.0);
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new(pickers::level_label(level))
                                .size(FS_XS)
                                .color(ink),
                        );
                    });
                })
                .response;
            let id = egui::Id::new("thinking-segment").with(level.value.as_str());
            if enabled
                && !on
                && ui
                    .interact(seg.rect, id, egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
            {
                store.set_option("thinking", &level.value);
            }
        }
    });
}

/// Compact, offered once the context passes the 80 percent the engine
/// compacts at, while no turn runs.
fn compact_button(
    ui: &mut egui::Ui,
    store: &Store,
    running: bool,
    events: &mut Vec<ToolbarEvent>,
) -> Option<()> {
    let session = store.active_session()?;
    if !pickers::compacts_soon(session.usage.used, session.usage.size) || running {
        return Some(());
    }
    let palette = Palette::active_of(ui.ctx());
    let (response, _) = pill(
        ui,
        false,
        palette,
        9.0,
        vec![
            Part::Icon(crate::icons::Icon::Layers, 14.0, palette.warn),
            Part::Text(
                "Compact".to_owned(),
                egui::FontId::new(FS_XS + 1.0, family(FAMILY_BODY_REGULAR)),
                palette.warn,
            ),
        ],
    );
    if response.clicked() {
        events.push(ToolbarEvent::Compact);
    }
    let _ = response.on_hover_text("Summarize older turns to free context");
    Some(())
}

/// The context ring's outer diameter.
const RING_D: f32 = 16.0;

/// Paints the context ring into `rect`: a faint track in `color` and
/// the used share over it, clockwise from twelve o'clock.
fn paint_ring(ui: &egui::Ui, rect: egui::Rect, fill: f64, color: egui::Color32) {
    let stroke = 2.0;
    let center = rect.center();
    let radius = RING_D / 2.0 - stroke / 2.0;
    ui.painter().circle_stroke(
        center,
        radius,
        egui::Stroke::new(stroke, color.gamma_multiply(0.25)),
    );
    let steps = 48;
    let lit = ((fill.clamp(0.0, 1.0) * f64::from(steps)).round() as usize).max(1);
    let points: Vec<egui::Pos2> = (0..=lit)
        .map(|step| {
            let angle =
                std::f32::consts::TAU * step as f32 / steps as f32 - std::f32::consts::FRAC_PI_2;
            center + radius * egui::vec2(angle.cos(), angle.sin())
        })
        .collect();
    ui.painter()
        .add(egui::Shape::line(points, egui::Stroke::new(stroke, color)));
}

/// The context ring with its percent. The ring turns warn at 80
/// percent, where the engine compacts on its own.
fn context_ring(
    ui: &mut egui::Ui,
    toolbar: &mut Toolbar,
    store: &Store,
    running: bool,
    events: &mut Vec<ToolbarEvent>,
) -> Option<()> {
    let session = store.active_session()?;
    let palette = Palette::active_of(ui.ctx());
    let (used, size) = (session.usage.used, session.usage.size);
    let fill = pickers::fill_fraction(used, size);
    let percent = pickers::fill_percent(used, size);
    let hot = pickers::compacts_soon(used, size);
    let ring = if hot { palette.warn } else { palette.accent };
    let (response, rooms) = pill(
        ui,
        false,
        palette,
        6.0,
        vec![
            Part::Room(RING_D),
            Part::Text(
                format!("{percent}%"),
                egui::FontId::new(FS_2XS, family(FAMILY_MONO_REGULAR)),
                palette.faint,
            ),
        ],
    );
    let response = response.on_hover_text("Context");
    let ring_rect = rooms.first().copied().unwrap_or(egui::Rect::NOTHING);
    paint_ring(ui, ring_rect, fill, ring);
    if store.prefs().fuel && response.clicked() {
        toolbar.toggle_gauge();
    }
    let tip = if size > 0 {
        format!(
            "Context: {} of {} tokens ({percent}%)",
            pickers::tokens(used),
            pickers::tokens(size)
        )
    } else {
        "Context: the agent sent no usage yet".to_owned()
    };
    if !toolbar.gauge_open() {
        return Some(());
    }
    let turns = session
        .items
        .iter()
        .filter(|item| matches!(item, kage_client::TranscriptItem::User { .. }))
        .count();
    let messages = session
        .items
        .iter()
        .filter(|item| {
            matches!(
                item,
                kage_client::TranscriptItem::User { .. }
                    | kage_client::TranscriptItem::Assistant { .. }
            )
        })
        .count();
    let cost = store
        .active_id()
        .and_then(|id| store.tree_cost(id))
        .map(|cost| format!("{} {:.2}", cost.currency, cost.amount))
        .unwrap_or_else(|| "unknown".to_owned());
    let model = crate::modes::select_option(session, "model")
        .and_then(|option| {
            option
                .options
                .iter()
                .find(|choice| choice.value == option.current_value)
                .map(|choice| choice.name.clone())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let mut rows = vec![vec![
        ("Spent", cost),
        ("Model", model),
        ("Turns", turns.to_string()),
    ]];
    match session.usage.tokens {
        Some(totals) => {
            rows.push(vec![
                ("Messages", messages.to_string()),
                ("Input", pickers::tokens(totals.input)),
                ("Output", pickers::tokens(totals.output)),
            ]);
            rows.push(vec![
                ("Cache read", pickers::tokens(totals.cache_read)),
                ("Cache write", pickers::tokens(totals.cache_write)),
                ("Cache hits", cache_hits(&totals)),
            ]);
        }
        None => rows.push(vec![("Messages", messages.to_string())]),
    }
    let (at, pivot) = gauge_anchor(ui.ctx(), response.rect);
    let card = egui::Window::new("context-gauge")
        .id(egui::Id::new("context-gauge"))
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .fixed_pos(at)
        .pivot(pivot)
        .min_width(MODEL_W)
        .order(egui::Order::Foreground)
        .frame(egui::Frame::NONE)
        .show(ui.ctx(), |ui| {
            surface(ui, palette, MODEL_W, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(14, 12))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 10.0;
                        gauge_head(ui, used, size, percent, hot, palette);
                        gauge_cells(ui, fill, hot, palette);
                        gauge_legend(ui, size, palette);
                        let (rule, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 1.0),
                            egui::Sense::hover(),
                        );
                        ui.painter()
                            .rect_filled(rule, egui::CornerRadius::ZERO, palette.subtle);
                        for row in &rows {
                            gauge_stats(ui, row, palette);
                        }
                        let row = egui::vec2(ui.available_width(), 28.0);
                        let rtl = egui::Layout::right_to_left(egui::Align::Center);
                        ui.allocate_ui_with_layout(row, rtl, |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            let compact = ui.add_enabled(
                                !running,
                                egui::Button::image_and_text(
                                    crate::icons::themed_image(Icon::Layers, 12.0, palette.ink),
                                    egui::RichText::new("Compact now")
                                        .size(FS_XS)
                                        .color(palette.ink),
                                )
                                .fill(palette.fill)
                                .stroke(egui::Stroke::new(1.0, palette.line))
                                .corner_radius(egui::CornerRadius::same(R_MD as u8))
                                .min_size(egui::vec2(0.0, 26.0)),
                            );
                            if compact.clicked() {
                                events.push(ToolbarEvent::Compact);
                            }
                            let note = ui.available_width();
                            let layout = egui::Layout::top_down(egui::Align::Min);
                            ui.allocate_ui_with_layout(egui::vec2(note, 0.0), layout, |ui| {
                                ui.set_width(note);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(
                                            "The engine compacts on its own at 80%, \
                                             summarizing older turns.",
                                        )
                                        .size(FS_2XS)
                                        .color(palette.faint),
                                    )
                                    .wrap(),
                                );
                            });
                        });
                    });
            });
        });
    let escape = ui.input(|input| input.key_pressed(egui::Key::Escape));
    if let Some(card) = card
        && (escape || (card.response.clicked_elsewhere() && !response.clicked()))
    {
        toolbar.close_gauge();
    }
    let _ = response.on_hover_text(tip);
    Some(())
}

/// The twenty cells the gauge draws, the sixteenth marked as the
/// compaction point.
fn gauge_cells(ui: &mut egui::Ui, fill: f64, hot: bool, palette: &Palette) {
    let width = ui.available_width();
    let gap = 3.0;
    let cell = (width - gap * 19.0) / 20.0;
    let height = 14.0;
    let (rect, _) = ui.allocate_at_least(egui::vec2(width, height), egui::Sense::hover());
    for (index, lit) in pickers::gauge_cells(fill) {
        let x = rect.left() + index as f32 * (cell + gap);
        let cell_rect =
            egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(cell, height));
        let color = match (lit, hot) {
            (false, _) => palette.fill_hover,
            (true, true) => palette.warn,
            (true, false) => palette.accent,
        };
        ui.painter()
            .rect_filled(cell_rect, egui::CornerRadius::same(3), color);
        if index == pickers::COMPACT_CELL {
            ui.painter().vline(
                x,
                egui::Rangef::new(rect.top(), rect.bottom()),
                egui::Stroke::new(2.0, palette.ink_strong),
            );
        }
    }
}

/// The gauge's head: the title, the tokens in use over the window,
/// and the percent chip, warm once compaction is near.
fn gauge_head(ui: &mut egui::Ui, used: u64, size: u64, percent: i64, hot: bool, palette: &Palette) {
    let (rect, head) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
    head.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "Context"));
    let mid = rect.center().y;
    let title = ui.painter().layout_no_wrap(
        "Context".to_owned(),
        egui::FontId::new(FS_SM, family(FAMILY_BODY_SEMIBOLD)),
        palette.ink_strong,
    );
    ui.painter().galley(
        egui::pos2(rect.left(), crate::theme::line_y(mid, 0.0, &title)),
        title,
        palette.ink_strong,
    );
    let (bg, fg) = if hot {
        (palette.warn_soft, palette.warn)
    } else {
        (palette.fill, palette.muted)
    };
    let chip_text = ui.painter().layout_no_wrap(
        format!("{percent}%"),
        egui::FontId::new(FS_2XS, family(FAMILY_BODY_REGULAR)),
        fg,
    );
    let chip = egui::Rect::from_min_max(
        egui::pos2(rect.right() - chip_text.size().x - 14.0, mid - 9.0),
        egui::pos2(rect.right(), mid + 9.0),
    );
    ui.painter()
        .rect_filled(chip, egui::CornerRadius::same(R_FULL as u8), bg);
    ui.painter()
        .galley(chip.center() - chip_text.size() / 2.0, chip_text, fg);
    let totals = ui.painter().layout_no_wrap(
        format!("{} / {}", pickers::tokens(used), pickers::tokens(size)),
        egui::FontId::new(FS_XS, family(FAMILY_MONO_REGULAR)),
        palette.muted,
    );
    ui.painter().galley(
        egui::pos2(
            chip.left() - 8.0 - totals.size().x,
            crate::theme::line_y(mid, 0.0, &totals),
        ),
        totals,
        palette.muted,
    );
}

/// The scale under the gauge cells: empty, the compaction mark, full.
fn gauge_legend(ui: &mut egui::Ui, size: u64, palette: &Palette) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), egui::Sense::hover());
    let font = egui::FontId::new(FS_2XS, family(FAMILY_BODY_REGULAR));
    for (text, align) in [
        ("0".to_owned(), egui::Align2::LEFT_CENTER),
        ("compacts at 80%".to_owned(), egui::Align2::CENTER_CENTER),
        (pickers::tokens(size), egui::Align2::RIGHT_CENTER),
    ] {
        let at = match align {
            egui::Align2::LEFT_CENTER => rect.left_center(),
            egui::Align2::RIGHT_CENTER => rect.right_center(),
            _ => rect.center(),
        };
        ui.painter()
            .text(at, align, text, font.clone(), palette.faint);
    }
}

/// One row of gauge stats in equal columns: a faint label over its
/// value, the value cut short when it runs out of room.
fn gauge_stats(ui: &mut egui::Ui, stats: &[(&str, String)], palette: &Palette) {
    let gap = 8.0;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 34.0), egui::Sense::hover());
    let column = (width - gap * 2.0) / 3.0;
    for (n, (label, value)) in stats.iter().enumerate() {
        let left = rect.left() + n as f32 * (column + gap);
        ui.painter().text(
            egui::pos2(left, rect.top()),
            egui::Align2::LEFT_TOP,
            *label,
            egui::FontId::new(FS_2XS, family(FAMILY_BODY_REGULAR)),
            palette.faint,
        );
        let mut job = egui::text::LayoutJob::simple_singleline(
            value.clone(),
            egui::FontId::new(FS_SM, family(FAMILY_BODY_REGULAR)),
            palette.ink,
        );
        job.wrap.max_width = column;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('\u{2026}');
        let galley = ui.painter().layout_job(job);
        ui.painter()
            .galley(egui::pos2(left, rect.top() + 16.0), galley, palette.ink);
    }
}

/// The share of the prompt the provider served from its cache, as a
/// percent, or `none` before any prompt went out.
fn cache_hits(totals: &kage_client::wire::UsageTokens) -> String {
    let prompt = totals.input + totals.cache_read + totals.cache_write;
    if prompt == 0 {
        return "none".to_owned();
    }
    format!("{:.0}%", totals.cache_read as f64 * 100.0 / prompt as f64)
}

/// The send and stop control: a 32px circle that interrupts while a
/// run is in flight and the input is empty, and sends otherwise.
fn send_button(
    ui: &mut egui::Ui,
    running: bool,
    has_text: bool,
    events: &mut Vec<ToolbarEvent>,
) -> Option<()> {
    let palette = Palette::active_of(ui.ctx());
    if running && !has_text {
        let response = crate::ui::forms::icon_btn(
            ui,
            "Interrupt",
            Icon::Square,
            SEND_D,
            14.0,
            R_FULL,
            crate::ui::forms::IconTones {
                fill: palette.fill_hover,
                hover_fill: palette.danger_soft,
                ink: palette.ink,
                hover_ink: palette.danger,
            },
        );
        if response.clicked() {
            events.push(ToolbarEvent::Cancel);
        }
        let _ = response.on_hover_text("Interrupt (Esc Esc)");
        return Some(());
    }
    let (bg, fg) = if has_text {
        (palette.send_bg, palette.send_icon)
    } else {
        (palette.send_bg_off, palette.send_icon_off)
    };
    let shadow = ui.painter().add(egui::Shape::Noop);
    let response = crate::ui::forms::icon_btn(
        ui,
        "Send",
        Icon::ArrowUp,
        SEND_D,
        14.0,
        R_FULL,
        crate::ui::forms::IconTones {
            fill: bg,
            hover_fill: if has_text { palette.send_bg_hover } else { bg },
            ink: fg,
            hover_ink: fg,
        },
    );
    if has_text {
        let radius = egui::CornerRadius::same((SEND_D / 2.0) as u8);
        ui.painter().set(
            shadow,
            crate::shapes::shadow(response.rect, radius, &palette.shadow_send),
        );
    }
    if response.clicked() && has_text {
        events.push(ToolbarEvent::Submit);
    }
    let _ = response.on_hover_text(if running {
        "Queue (Enter) or steer (Ctrl+Enter)"
    } else {
        "Send (Enter)"
    });

    Some(())
}

/// The line under the composer: the key help as key caps, or the
/// interrupt hint, with the session cost at the far end.
pub fn hint_line(ui: &mut egui::Ui, running: bool, enter_sends: bool, steer: bool, cost: &str) {
    let palette = Palette::active_of(ui.ctx());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        if !enter_sends {
            kbd(ui, "Ctrl Enter", palette);
            ui.label(
                egui::RichText::new(if running { "queue" } else { "send" })
                    .size(FS_2XS)
                    .color(palette.faint),
            );
            kbd(ui, "Enter", palette);
            ui.label(
                egui::RichText::new("newline")
                    .size(FS_2XS)
                    .color(palette.faint),
            );
        } else if running {
            kbd(ui, "Enter", palette);
            ui.label(
                egui::RichText::new("queue")
                    .size(FS_2XS)
                    .color(palette.faint),
            );
            if steer {
                kbd(ui, "Ctrl Enter", palette);
                ui.label(
                    egui::RichText::new("steer")
                        .size(FS_2XS)
                        .color(palette.faint),
                );
            } else {
                ui.label(
                    egui::RichText::new("steering not advertised")
                        .size(FS_2XS)
                        .color(palette.faint),
                );
            }
            kbd(ui, "Esc Esc", palette);
            ui.label(
                egui::RichText::new("interrupt")
                    .size(FS_2XS)
                    .color(palette.faint),
            );
        } else {
            kbd(ui, "Enter", palette);
            ui.label(
                egui::RichText::new("send")
                    .size(FS_2XS)
                    .color(palette.faint),
            );
            kbd(ui, "Shift Enter", palette);
            ui.label(
                egui::RichText::new("newline")
                    .size(FS_2XS)
                    .color(palette.faint),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !cost.is_empty() {
                ui.label(egui::RichText::new(cost).size(FS_2XS).color(palette.faint));
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::{Toolbar, mode_label, model_label, thinking_label};
    use crate::ui::pickers;

    #[test]
    fn the_toolbar_opens_one_picker_at_a_time() {
        let mut toolbar = Toolbar::new();
        assert_eq!(toolbar.picker(), None);
        toolbar.toggle_picker(super::Picker::Mode);
        assert_eq!(toolbar.picker(), Some(super::Picker::Mode));
        toolbar.toggle_picker(super::Picker::Model);
        assert_eq!(toolbar.picker(), Some(super::Picker::Model));
        toolbar.toggle_picker(super::Picker::Model);
        assert_eq!(toolbar.picker(), None, "the same pill closes it");
        toolbar.toggle_picker(super::Picker::Mode);
        toolbar.close_pickers();
        assert_eq!(toolbar.picker(), None);
        assert!(!toolbar.any_open());
    }

    #[test]
    fn the_plus_and_gauge_popovers_toggle() {
        let mut toolbar = Toolbar::new();
        toolbar.toggle_plus();
        assert!(toolbar.plus_open());
        assert!(toolbar.any_open());
        toolbar.close_plus();
        assert!(!toolbar.plus_open());
        toolbar.toggle_gauge();
        assert!(toolbar.gauge_open());
        assert!(toolbar.any_open());
    }

    #[test]
    fn the_highlight_steps_and_clamps() {
        let mut toolbar = Toolbar::new();
        toolbar.step_mode(1, 3);
        assert_eq!(toolbar.mode_highlight(), 1);
        toolbar.step_mode(5, 3);
        assert_eq!(toolbar.mode_highlight(), 2, "the last row stays");
        toolbar.step_mode(-1, 3);
        assert_eq!(toolbar.mode_highlight(), 1);
        toolbar.step_model(1, 0);
        assert_eq!(toolbar.model_highlight(), 0, "no rows, no highlight");
    }

    #[test]
    fn the_labels_name_the_active_values() {
        let store = crate::store::Store::new("/w", false);
        assert_eq!(
            model_label(&store),
            "default",
            "no option, the default word"
        );
        assert_eq!(thinking_label(&store), None, "no option, no level");
        assert_eq!(mode_label(&store, "ask"), "ask", "no option, the raw value");
        assert_eq!(pickers::session_goal(None), None);
    }
}
