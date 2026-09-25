//! Slots: the fixed chrome regions and the components that fill them.
//!
//! Five slots exist: `header` (the top row, collapsed while empty),
//! `activity` (the working row above the input, collapsed while
//! empty), `input_pill` (the input's top rule), `footer` (the bottom
//! row) and `start` (the start card above the input while the
//! conversation is empty). `kage.api.slot_set(name, spec)` sets one,
//! and `nil` restores the spec `_defaults.lua` set. A row slot takes `{ left = items, right = items, sep = string? }`, and
//! `start` takes `{ lines = items }`, one line per item. An item is:
//!
//! * a built-in component name (see [`BUILTIN_COMPONENTS`]), which the
//!   host paints from its own state on every frame;
//! * a span table `{ text, hl?, fg?, bg?, bold?, ... }`;
//! * a Lua component `{ render = fn(ctx), events?, interval?, hl? }`.
//!
//! A Lua component's lines are retained here and recomputed on the
//! owner thread only when one of its `events` fires (an entry is an
//! event name, optionally followed by a space and a pattern, such as
//! `"user Tick"`), when its `interval` timer fires, on
//! `kage.api.redraw(slot)`, when its slot is set, and when the host
//! reports a new width through [`Slots::report`]. The render path only
//! reads the retained lines. Each recompute runs under
//! [`watchdog::RENDER_BUDGET`] with a [`UiState`] snapshot as `ctx`, and
//! sets the redraw flag when the lines changed. A failed recompute is
//! logged and keeps the previous lines.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use kage_core::ThinkingLevel;
use kage_core::protocol::{SessionState, Usage};
use kage_core::sync::lock;
use mlua::{Function, Lua, RegistryKey, Table, Value};

use crate::api::{LogLevel, SharedHostLog, json_to_lua};
use crate::autocmd;
use crate::capabilities::CurrentPlugin;
use crate::chrome::{ChromeLine, ChromeSpan, parse_lines, parse_span_table};
use crate::error::PluginError;
use crate::host::{LuaHost, WeakHost};
use crate::schedule;
use crate::stdlib::DEFAULTS_ENV;
use crate::watchdog;

/// Width Lua components see before the host reports one.
const DEFAULT_WIDTH: u16 = 80;

/// A named chrome region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotName {
    /// The top row, collapsed while it paints nothing.
    Header,
    /// The bottom row.
    Footer,
    /// The input's top rule.
    InputPill,
    /// The start card above the input while the conversation is empty.
    Start,
    /// The working row above the input, collapsed while it paints
    /// nothing.
    Activity,
}

/// Number of slots.
const SLOTS: usize = SlotName::ALL.len();

impl SlotName {
    /// Every slot, in index order.
    pub const ALL: [SlotName; 5] = [
        SlotName::Header,
        SlotName::Footer,
        SlotName::InputPill,
        SlotName::Start,
        SlotName::Activity,
    ];

    /// The name Lua uses for this slot.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            SlotName::Header => "header",
            SlotName::Footer => "footer",
            SlotName::InputPill => "input_pill",
            SlotName::Start => "start",
            SlotName::Activity => "activity",
        }
    }

    /// The slot called `name` in Lua.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|slot| slot.name() == name)
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Names of the built-in components the host implements. `sessions`
/// and `notices` paint only in `start`. `breadcrumb` paints the agent on
/// screen and nothing in the main view, where `title` paints instead.
pub const BUILTIN_COMPONENTS: &[&str] = &[
    "brand",
    "breadcrumb",
    "title",
    "model",
    "widgets",
    "search",
    "session",
    "working",
    "activity",
    "context",
    "tokens",
    "thinking",
    "permission",
    "mode",
    "hint",
    "cwd",
    "version",
    "sessions",
    "notices",
];

/// One entry of a [`SlotSpec`].
#[derive(Clone, Debug)]
pub enum SlotItem {
    /// A built-in component, one of [`BUILTIN_COMPONENTS`].
    Builtin(&'static str),
    /// A fixed span.
    Text(ChromeSpan),
    /// A component rendered by Lua.
    Lua(Arc<LuaComponent>),
}

impl PartialEq for SlotItem {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (SlotItem::Builtin(a), SlotItem::Builtin(b)) => a == b,
            (SlotItem::Text(a), SlotItem::Text(b)) => a == b,
            (SlotItem::Lua(a), SlotItem::Lua(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// The layout of one slot. Row slots use `left`, `right` and `sep`;
/// `start` uses `lines`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SlotSpec {
    /// Items painted from the left edge.
    pub left: Vec<SlotItem>,
    /// Items painted against the right edge.
    pub right: Vec<SlotItem>,
    /// Painted between two adjacent items that both produced output.
    pub sep: String,
    /// One line per item, for `start`.
    pub lines: Vec<SlotItem>,
}

impl SlotSpec {
    fn components(&self) -> impl Iterator<Item = &Arc<LuaComponent>> {
        self.left
            .iter()
            .chain(&self.right)
            .chain(&self.lines)
            .filter_map(|item| match item {
                SlotItem::Lua(component) => Some(component),
                _ => None,
            })
    }
}

/// The spec a slot paints when none is set: kage's built-in chrome.
/// `_defaults.lua` sets exactly these, except that its `start` tip is
/// picked at random and this one is the first tip.
#[must_use]
pub fn default_spec(slot: SlotName) -> Arc<SlotSpec> {
    Arc::clone(&DEFAULT_SPECS[slot.index()])
}

static DEFAULT_SPECS: LazyLock<[Arc<SlotSpec>; SLOTS]> = LazyLock::new(|| {
    let items = |names: &[&'static str]| names.iter().copied().map(SlotItem::Builtin).collect();
    [
        Arc::new(SlotSpec {
            left: items(&["breadcrumb", "title"]),
            right: items(&["widgets", "search"]),
            ..SlotSpec::default()
        }),
        Arc::new(SlotSpec {
            left: items(&["hint"]),
            right: items(&["model", "permission", "context", "tokens"]),
            sep: " \u{B7} ".to_owned(),
            ..SlotSpec::default()
        }),
        Arc::new(SlotSpec {
            left: items(&["working", "mode"]),
            right: items(&["thinking"]),
            ..SlotSpec::default()
        }),
        Arc::new(SlotSpec {
            lines: start_lines(),
            ..SlotSpec::default()
        }),
        Arc::new(SlotSpec {
            left: items(&["activity"]),
            ..SlotSpec::default()
        }),
    ]
});

/// The default start card: brand, the labeled rows, recent sessions,
/// notices and a tip, with blank lines between the groups.
fn start_lines() -> Vec<SlotItem> {
    let text = |text: &str, hl: Option<&str>| {
        SlotItem::Text(ChromeSpan {
            text: text.to_owned(),
            hl: hl.map(str::to_owned),
            ..ChromeSpan::default()
        })
    };
    let blank = || text("", None);
    vec![
        SlotItem::Builtin("brand"),
        blank(),
        SlotItem::Builtin("model"),
        SlotItem::Builtin("cwd"),
        SlotItem::Builtin("permission"),
        SlotItem::Builtin("thinking"),
        blank(),
        SlotItem::Builtin("sessions"),
        SlotItem::Builtin("notices"),
        text(
            "Tip: Press tab to queue a message while kage works, or enter to steer the running turn.",
            Some("KageMuted"),
        ),
    ]
}

/// Every slot's spec at one point in time. The default value holds no
/// spec, so every slot paints its [`default_spec`].
#[derive(Clone, Debug, Default)]
pub struct SlotSpecs([Option<Arc<SlotSpec>>; SLOTS]);

impl SlotSpecs {
    /// The spec to paint for `slot`: the one set, or [`default_spec`].
    #[must_use]
    pub fn get(&self, slot: SlotName) -> Arc<SlotSpec> {
        self.0[slot.index()]
            .clone()
            .unwrap_or_else(|| default_spec(slot))
    }
}

/// A slot component rendered by Lua, with its retained lines.
pub struct LuaComponent {
    render: RegistryKey,
    events: Vec<(String, Vec<String>)>,
    interval: Option<u64>,
    hl: Option<String>,
    origin: Arc<str>,
    lines: Mutex<Arc<[ChromeLine]>>,
}

impl std::fmt::Debug for LuaComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaComponent")
            .field("events", &self.events)
            .field("interval", &self.interval)
            .field("hl", &self.hl)
            .finish_non_exhaustive()
    }
}

impl LuaComponent {
    /// The lines of the last successful recompute. Never waits on Lua.
    #[must_use]
    pub fn lines(&self) -> Arc<[ChromeLine]> {
        Arc::clone(&lock(&self.lines))
    }

    /// Highlight group applied under the component's own span styles.
    #[must_use]
    pub fn hl(&self) -> Option<&str> {
        self.hl.as_deref()
    }
}

/// What Lua components receive as `ctx`, kept current by the host.
#[derive(Clone, Debug, PartialEq)]
pub struct UiState {
    /// Terminal width in columns.
    pub width: u16,
    /// Model, thinking level, permission override and whether a run is
    /// in flight.
    pub state: SessionState,
    /// Token and cost totals, once the engine reported any.
    pub usage: Option<Usage>,
    /// Active session id.
    pub session_id: String,
    /// Active session title, if any.
    pub session_title: Option<String>,
    /// Working directory.
    pub cwd: String,
    /// Editor mode (`normal`, `insert` or `visual`).
    pub mode: String,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            state: SessionState::default(),
            usage: None,
            session_id: String::new(),
            session_title: None,
            cwd: String::new(),
            mode: String::new(),
        }
    }
}

/// [`UiState`] shared by the host and the runtime.
pub type SharedUiState = Arc<Mutex<UiState>>;

struct Shared {
    specs: Mutex<SlotTable>,
    ui: SharedUiState,
    sink: SharedHostLog,
    redraw: Arc<AtomicBool>,
    current: CurrentPlugin,
}

#[derive(Default)]
struct SlotTable {
    set: [Option<Arc<SlotSpec>>; SLOTS],
    defaults: [Option<Arc<SlotSpec>>; SLOTS],
}

/// Autocmd and timer ids that feed the components of each slot's spec.
/// Lives in the Lua app data, so only the owner thread touches it.
#[derive(Default)]
struct Hooks([Vec<Hook>; SLOTS]);

enum Hook {
    Autocmd(i64),
    Timer(i64),
}

/// The host's handle to the slots: the specs to paint and the UI state
/// Lua components read.
#[derive(Clone)]
pub struct Slots {
    shared: Arc<Shared>,
    host: WeakHost,
}

impl std::fmt::Debug for Slots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slots").finish_non_exhaustive()
    }
}

impl Slots {
    /// The spec set for `slot`, or `None` when the slot paints
    /// [`default_spec`].
    #[must_use]
    pub fn spec(&self, slot: SlotName) -> Option<Arc<SlotSpec>> {
        lock(&self.shared.specs).set[slot.index()].clone()
    }

    /// Every slot's spec, for one frame.
    #[must_use]
    pub fn specs(&self) -> SlotSpecs {
        SlotSpecs(lock(&self.shared.specs).set.clone())
    }

    /// The UI state Lua components read.
    #[must_use]
    pub fn ui_state(&self) -> SharedUiState {
        Arc::clone(&self.shared.ui)
    }

    /// Record the terminal width and editor mode. A new width queues a
    /// recompute of every Lua component on the owner thread and returns
    /// at once.
    pub fn report(&self, width: u16, mode: &str) {
        let resized = {
            let mut ui = lock(&self.shared.ui);
            if ui.mode != mode {
                mode.clone_into(&mut ui.mode);
            }
            let resized = ui.width != width;
            ui.width = width;
            resized
        };
        if resized && let Ok(host) = self.host.upgrade() {
            let shared = Arc::clone(&self.shared);
            let _ = host.submit(move |lua| recompute_slots(lua, &shared, &SlotName::ALL));
        }
    }

    /// Drop every spec. The owner-side hooks go with the reload that
    /// clears autocmds and timers.
    pub(crate) fn clear(&self) {
        *lock(&self.shared.specs) = SlotTable::default();
    }
}

/// Drop every hook id. Used by reload, which also clears the autocmds
/// and timers the ids point at.
pub(crate) fn clear_hooks(lua: &Lua) {
    if let Some(mut hooks) = lua.app_data_mut::<Hooks>() {
        *hooks = Hooks::default();
    }
}

/// Install `kage.api.slot_set` and `kage.api.redraw` and return the
/// host handle.
pub(crate) fn install(
    lua: &Lua,
    host: &LuaHost,
    sink: SharedHostLog,
    current: CurrentPlugin,
) -> Result<Slots, PluginError> {
    let shared = Arc::new(Shared {
        specs: Mutex::default(),
        ui: SharedUiState::default(),
        sink,
        redraw: host.redraw_flag(),
        current,
    });
    lua.set_app_data(Hooks::default());
    let api: Table = lua.globals().get::<Table>("kage")?.get("api")?;

    let state = Arc::clone(&shared);
    api.set(
        "slot_set",
        lua.create_function(move |lua, (name, spec): (String, Value)| {
            slot_set(lua, &state, &name, spec)
        })?,
    )?;

    let state = Arc::clone(&shared);
    api.set(
        "redraw",
        lua.create_function(move |lua, name: Option<String>| {
            match name {
                None => recompute_slots(lua, &state, &SlotName::ALL),
                Some(name) => recompute_slots(lua, &state, &[slot_named("redraw", &name)?]),
            }
            state.redraw.store(true, Ordering::SeqCst);
            Ok(())
        })?,
    )?;

    Ok(Slots {
        shared,
        host: host.downgrade(),
    })
}

fn fail(message: String) -> mlua::Error {
    mlua::Error::external(message)
}

fn slot_named(func: &str, name: &str) -> mlua::Result<SlotName> {
    SlotName::parse(name).ok_or_else(|| {
        fail(format!(
            "kage.api.{func}: unknown slot '{name}' (header, activity, input_pill, footer, start)"
        ))
    })
}

fn slot_set(lua: &Lua, shared: &Arc<Shared>, name: &str, spec: Value) -> mlua::Result<()> {
    let slot = slot_named("slot_set", name)?;
    let owner = lock(&shared.current).clone();
    let spec = match spec {
        Value::Nil => lock(&shared.specs).defaults[slot.index()].clone(),
        Value::Table(table) => {
            let origin: Arc<str> = owner
                .as_deref()
                .map(|o| format!(" from '{o}'"))
                .unwrap_or_default()
                .into();
            let spec = Arc::new(
                parse_spec(lua, slot, &table, &origin)
                    .map_err(|e| fail(format!("kage.api.slot_set: {e}")))?,
            );
            if owner.as_deref() == Some(DEFAULTS_ENV) {
                lock(&shared.specs).defaults[slot.index()] = Some(Arc::clone(&spec));
            }
            Some(spec)
        }
        _ => {
            return Err(fail(
                "kage.api.slot_set: spec must be a table or nil".to_owned(),
            ));
        }
    };
    lock(&shared.specs).set[slot.index()].clone_from(&spec);
    unhook(lua, slot)?;
    if let Some(spec) = spec {
        hook(lua, shared, slot, &spec)?;
        for component in spec.components() {
            recompute(lua, shared, component);
        }
    }
    shared.redraw.store(true, Ordering::SeqCst);
    Ok(())
}

fn parse_spec(
    lua: &Lua,
    slot: SlotName,
    table: &Table,
    origin: &Arc<str>,
) -> Result<SlotSpec, String> {
    let items = |key: &str| -> Result<Vec<SlotItem>, String> {
        match table.get::<Value>(key).map_err(|e| e.to_string())? {
            Value::Nil => Ok(Vec::new()),
            Value::Table(list) => list
                .sequence_values::<Value>()
                .map(|item| parse_item(lua, &item.map_err(|e| e.to_string())?, origin))
                .collect(),
            _ => Err(format!("`{key}` must be a list")),
        }
    };
    let has = |key: &str| table.contains_key(key).unwrap_or(false);
    if slot == SlotName::Start {
        if has("left") || has("right") || has("sep") {
            return Err("`start` takes `lines`, not `left`, `right` or `sep`".to_owned());
        }
        return Ok(SlotSpec {
            lines: items("lines")?,
            ..SlotSpec::default()
        });
    }
    if has("lines") {
        return Err(format!(
            "`{}` takes `left`, `right` and `sep`, not `lines`",
            slot.name()
        ));
    }
    let sep = match table.get::<Value>("sep").map_err(|e| e.to_string())? {
        Value::Nil => String::new(),
        Value::String(s) => s.to_str().map_err(|e| e.to_string())?.to_owned(),
        _ => return Err("`sep` must be a string".to_owned()),
    };
    Ok(SlotSpec {
        left: items("left")?,
        right: items("right")?,
        sep,
        lines: Vec::new(),
    })
}

fn parse_item(lua: &Lua, item: &Value, origin: &Arc<str>) -> Result<SlotItem, String> {
    match item {
        Value::String(name) => {
            let name = name.to_str().map_err(|e| e.to_string())?;
            BUILTIN_COMPONENTS
                .iter()
                .find(|builtin| **builtin == &*name)
                .map(|builtin| SlotItem::Builtin(builtin))
                .ok_or_else(|| format!("unknown component '{}'", &*name))
        }
        Value::Table(table) if table.contains_key("render").unwrap_or(false) => {
            parse_component(lua, table, origin).map(|c| SlotItem::Lua(Arc::new(c)))
        }
        Value::Table(table) if table.contains_key("text").unwrap_or(false) => {
            Ok(SlotItem::Text(parse_span_table(table)))
        }
        _ => Err("an item is a component name, a span table or a table with `render`".to_owned()),
    }
}

fn parse_component(lua: &Lua, table: &Table, origin: &Arc<str>) -> Result<LuaComponent, String> {
    let str_err = |e: mlua::Error| e.to_string();
    let Value::Function(render) = table.get::<Value>("render").map_err(str_err)? else {
        return Err("`render` must be a function".to_owned());
    };
    let events = match table.get::<Value>("events").map_err(str_err)? {
        Value::Nil => Vec::new(),
        Value::Table(list) => list
            .sequence_values::<String>()
            .map(|entry| {
                let entry = entry.map_err(str_err)?;
                let (event, patterns) = match entry.split_once(' ') {
                    Some((event, pattern)) => (event, vec![pattern.to_owned()]),
                    None => (entry.as_str(), Vec::new()),
                };
                let patterns = autocmd::check_patterns(event, patterns)?;
                Ok((event.to_owned(), patterns))
            })
            .collect::<Result<_, String>>()?,
        _ => return Err("`events` must be a list of event names".to_owned()),
    };
    let interval = match table.get::<Value>("interval").map_err(str_err)? {
        Value::Nil => None,
        Value::Integer(ms) if ms > 0 => u64::try_from(ms).ok(),
        _ => return Err("`interval` must be a positive integer of milliseconds".to_owned()),
    };
    let hl = match table.get::<Value>("hl").map_err(str_err)? {
        Value::Nil => None,
        Value::String(s) => Some(s.to_str().map_err(str_err)?.to_owned()),
        _ => return Err("`hl` must be a highlight group name".to_owned()),
    };
    Ok(LuaComponent {
        render: lua.create_registry_value(render).map_err(str_err)?,
        events,
        interval,
        hl,
        origin: Arc::clone(origin),
        lines: Mutex::new(Arc::from(Vec::new())),
    })
}

fn hook(lua: &Lua, shared: &Arc<Shared>, slot: SlotName, spec: &SlotSpec) -> mlua::Result<()> {
    let mut hooks = Vec::new();
    for component in spec.components() {
        let weak = Arc::downgrade(component);
        let state = Arc::clone(shared);
        let callback = lua.create_function(move |lua, _: mlua::MultiValue| {
            if let Some(component) = weak.upgrade() {
                recompute(lua, &state, &component);
            }
            Ok(())
        })?;
        let origin: Arc<str> = format!(" \"{} slot\"{}", slot.name(), component.origin).into();
        for (event, patterns) in &component.events {
            let id = autocmd::add(
                lua,
                event,
                patterns.clone(),
                callback.clone(),
                Arc::clone(&origin),
            )?;
            hooks.push(Hook::Autocmd(id));
        }
        if let Some(ms) = component.interval {
            let id = schedule::every(lua, "slot component", ms, callback, &shared.current)?;
            hooks.push(Hook::Timer(id));
        }
    }
    if let Some(mut all) = lua.app_data_mut::<Hooks>() {
        all.0[slot.index()] = hooks;
    }
    Ok(())
}

fn unhook(lua: &Lua, slot: SlotName) -> mlua::Result<()> {
    let hooks = match lua.app_data_mut::<Hooks>() {
        Some(mut all) => std::mem::take(&mut all.0[slot.index()]),
        None => return Ok(()),
    };
    for hook in hooks {
        match hook {
            Hook::Autocmd(id) => autocmd::remove(lua, id)?,
            Hook::Timer(id) => schedule::cancel(lua, id)?,
        }
    }
    Ok(())
}

fn recompute_slots(lua: &Lua, shared: &Shared, slots: &[SlotName]) {
    let specs = lock(&shared.specs).set.clone();
    for slot in slots {
        if let Some(spec) = &specs[slot.index()] {
            for component in spec.components() {
                recompute(lua, shared, component);
            }
        }
    }
}

fn recompute(lua: &Lua, shared: &Shared, component: &LuaComponent) {
    let ui = lock(&shared.ui).clone();
    match render(lua, component, &ui) {
        Ok(lines) => {
            let mut current = lock(&component.lines);
            if **current != *lines {
                *current = lines.into();
                shared.redraw.store(true, Ordering::SeqCst);
            }
        }
        Err(err) => lock(&shared.sink).log(
            LogLevel::Error,
            &format!("slot component{}: {err}", component.origin),
        ),
    }
}

fn render(
    lua: &Lua,
    component: &LuaComponent,
    ui: &UiState,
) -> Result<Vec<ChromeLine>, PluginError> {
    let func: Function = lua.registry_value(&component.render)?;
    let ctx = context(lua, ui)?;
    watchdog::run(lua, watchdog::RENDER_BUDGET, || func.call::<Value>(ctx))
        .map(|value| parse_lines(&value))
}

fn context(lua: &Lua, ui: &UiState) -> mlua::Result<Table> {
    let json = |value: serde_json::Result<serde_json::Value>| {
        json_to_lua(lua, &value.map_err(mlua::Error::external)?)
    };
    let ctx = lua.create_table()?;
    ctx.raw_set("width", ui.width)?;
    ctx.raw_set("model", ui.state.model.as_str())?;
    let thinking = ui
        .state
        .thinking_effective
        .map_or("off", ThinkingLevel::as_str);
    ctx.raw_set("thinking", thinking)?;
    ctx.raw_set("thinking_auto", ui.state.thinking.is_none())?;
    ctx.raw_set(
        "permission_mode",
        json(serde_json::to_value(ui.state.permission_mode))?,
    )?;
    ctx.raw_set("working", ui.state.working)?;
    ctx.raw_set("usage", json(serde_json::to_value(ui.usage))?)?;
    let session = lua.create_table()?;
    session.raw_set("id", ui.session_id.as_str())?;
    session.raw_set("title", ui.session_title.as_deref())?;
    ctx.raw_set("session", session)?;
    ctx.raw_set("cwd", ui.cwd.as_str())?;
    ctx.raw_set("mode", ui.mode.as_str())?;
    Ok(ctx)
}

#[cfg(test)]
mod tests;
