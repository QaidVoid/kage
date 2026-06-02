//! Interactive TUI event loop.
//!
//! [`App::run`] owns the [`Tui`] and a [`SharedBuffer`], polls crossterm
//! key events, drives [`InputState`], applies [`InputAction`]s to the
//! buffer, and redraws the screen ~30 times a second. Submitting a
//! prompt fires a `RunRequest` through the provided sink; the host is
//! responsible for spawning the agent loop on a worker thread and
//! pushing its events into the same `SharedBuffer` via [`TuiHooks`].

pub(crate) use std::io::Write;
pub(crate) use std::sync::mpsc::{Sender, TrySendError};
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::time::{Duration, Instant};

pub(crate) use kage_core::CancelFlag;
pub(crate) use ratatui::crossterm::event::{self, Event, KeyEventKind, MouseEventKind};

pub(crate) use crate::toast::{self, SharedToasts, Toast, ToastKind};

pub(crate) use crate::chord::Chord;
pub(crate) use crate::cmdline::{CommandLine, CommandLineEvent};
pub(crate) use crate::cmdparse::{EmptyResolver, Resolver};
pub(crate) use crate::command::{
    ArgSource, ArgSpec, BUILTIN_COMMANDS, CommandCategory, CommandSpec, OwnedArgSpec, PluginCommand,
};
pub(crate) use crate::error::TuiError;
pub(crate) use crate::events::SharedBuffer;
pub(crate) use crate::input::{InputAction, InputState, Mode, Pane};
pub(crate) use crate::layout::{input_height_for, split};
pub(crate) use crate::overlay::{
    CompletionAction, ContextAction, ContextMenu, ContextMenuOutcome, InputCompletion,
    OverlayAction, OverlayPicker, SessionTreeOverlay, SessionTreeSource, SettingsInit,
    SettingsOverlay, SlashContext, SlashPalette, file_completions, prefix_before_cursor,
};
pub(crate) use crate::picker::PickItem;
pub(crate) use crate::terminal::Tui;
pub(crate) use crate::view;

/// Lines scrolled per mouse wheel notch.
const MOUSE_SCROLL_LINES: i32 = 3;

/// Outcome of validating a command before execution.
///
/// [`CommandResult::Done`] means the command was dispatched (or the
/// command name was empty). [`CommandResult::ValidationError`] means
/// the argument schema rejected the input; the caller should keep the
/// cmdline open and display the error inline.
#[derive(Debug)]
pub(crate) enum CommandResult {
    Done(Option<AppExit>),
    ValidationError(String),
}

/// When `KAGE_DEBUG_KEYS` is set to a non-empty value, every press is
/// appended to the file at that path (or `$XDG_STATE_HOME/kage/keys.log`
/// when the value is `1`). Lets us diagnose terminal-specific quirks
/// like "Shift+Enter doesn't transmit" without instrumenting the host.
fn log_key_event(key: &ratatui::crossterm::event::KeyEvent) {
    let Ok(value) = std::env::var("KAGE_DEBUG_KEYS") else {
        return;
    };
    if value.is_empty() {
        return;
    }
    let path = if value == "1" {
        let Some(home) = std::env::var_os("XDG_STATE_HOME").or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = std::path::PathBuf::from(h);
                p.push(".local/state");
                p.into_os_string()
            })
        }) else {
            return;
        };
        let mut p = std::path::PathBuf::from(home);
        p.push("kage");
        let _ = std::fs::create_dir_all(&p);
        p.push("keys.log");
        p
    } else {
        std::path::PathBuf::from(value)
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            f,
            "{:?}  modifiers={:?}  kind={:?}",
            key.code, key.modifiers, key.kind
        );
    }
}

/// Request the host should act on. Either the user submitted a prompt
/// (the host runs the agent loop in a worker thread), the user asked
/// to cancel the in-flight turn, the user picked a different model,
/// or the user picked a prior session to resume into the current TUI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunRequest {
    /// New user prompt to submit to the agent loop, with any images
    /// the user attached (pasted/dragged path, `:attach`, or OS
    /// clipboard). The worker turns these into `Content::Image`
    /// blocks on the outgoing user message.
    Submit {
        /// Prompt text (may be empty if only images were attached).
        text: String,
        /// Queued image attachments, in attach order.
        images: Vec<crate::image::AttachedImage>,
    },
    /// Trip the agent loop's cancellation flag.
    Cancel,
    /// Switch to a different `provider:model` for subsequent turns.
    SwitchModel(String),
    /// Replay the session at the given path into the conversation
    /// buffer and pre-load its history into the agent context. The
    /// next [`RunRequest::Submit`] continues from that history.
    ResumeSession(std::path::PathBuf),
    /// Invoke a plugin-registered command by name with the trailing
    /// argument string. The host runs it on the worker thread (so the
    /// main thread keeps painting) and pushes its output as a custom
    /// block.
    InvokePluginCommand {
        /// Plugin command name (without the leading `/` or `:`).
        name: String,
        /// Whatever followed the command on the cmdline; an empty
        /// string when the command takes no arguments.
        args: String,
    },
    /// Force a compaction pass right now, regardless of token budget.
    /// The worker runs `maybe_compact` with the threshold lowered so it
    /// fires unconditionally, then emits the resulting `Compaction`
    /// event through the buffer/session hooks like an automatic pass.
    CompactNow,
    /// Plugin-initiated fork. `at` is an entry-id prefix or an empty
    /// string for "latest entry". The worker copies the current
    /// session up through that entry into a fresh session file and
    /// surfaces its id as a toast. The live session is left untouched:
    /// the fork is an independent snapshot, not a reseat.
    ForkSession {
        /// Entry-id prefix the fork should stop at, or empty for the
        /// most recent entry.
        at: String,
    },
    /// Duplicate the active session to a fresh id and reseat the
    /// runtime onto the copy. Unlike [`RunRequest::ForkSession`], the
    /// original file is frozen as a snapshot and every subsequent turn
    /// appends to the clone. History, model, and usage carry over
    /// unchanged because the copy is byte-identical through the last
    /// entry.
    CloneSession,
    /// Abandon the active conversation and start a fresh, empty
    /// session. The worker plans a new session file (deferred until
    /// the first prompt, like startup), clears the agent history and
    /// token budget, wipes the rendered buffer, and reseats
    /// `session_path` onto the new file. The model and system prompt
    /// carry over; the prior session file is left intact on disk.
    NewSession,
    /// Render the active session transcript to a Markdown file. `None`
    /// writes `<short-session-id>.md` in the working directory; `Some`
    /// uses the given path. The worker replays the session file (the
    /// source of truth) rather than the rendered buffer.
    ExportSession(Option<std::path::PathBuf>),
    /// Advance the active thinking level one step forward (the
    /// `Shift+Tab` cycle). The worker mutates the agent context's
    /// `thinking_level`, persists the change as a session entry, and
    /// fires the `thinking_level_select` plugin event.
    CycleThinkingLevel,
    /// Run the plugin keybinding whose canonical chord is `chord`. The
    /// worker invokes its handler through the coroutine bridge (so it
    /// may open `kage.ui.*` dialogs), like a plugin command.
    InvokePluginKeybinding {
        /// Canonical chord (e.g. `ctrl+shift+x`) identifying the
        /// registered binding.
        chord: String,
    },
    /// Fork the session file at the given path at its last entry into
    /// a fresh session, without reseating the runtime. Issued by the
    /// `:tree` browser's `f` so any session (not just the active one)
    /// can be branched.
    ForkSessionFile(std::path::PathBuf),
    /// Delete the session file at the given path. The worker refuses
    /// to delete the session that is currently active and surfaces a
    /// toast rather than orphaning the live writer. Issued by the
    /// `:tree` browser's `d`.
    DeleteSession(std::path::PathBuf),
    /// Plugin-initiated reseat from the `session_write` capability.
    /// `Session` resumes an existing session; `PendingFork` forks the
    /// live session at the carried entry then lands on the new branch
    /// (the rewind move). The worker consults the
    /// `session_before_switch` veto, then reseats the runtime onto the
    /// target so subsequent turns continue there.
    SwitchSession(kage_plugin::SwitchTarget),
    /// A plugin file changed on disk. The worker re-evaluates every
    /// `.lua` in the plugins directory and toasts the outcome. Chrome
    /// (`set_header`/`set_footer`), status, autocomplete, terminal
    /// hooks, and block renderers reattach automatically because they
    /// live in shared slots the runtime overwrites during load.
    /// Commands and keybindings the App cached at startup do not pick
    /// up new/removed entries until the next launch.
    ReloadPlugins,
}

/// Outcome of [`App::run`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppExit {
    /// User pressed `Ctrl+Q` / `:q` to leave the TUI cleanly.
    Quit,
}

/// Which overlay picker is currently open. Determines how
/// [`PickerEvent::Picked`] is dispatched: a model id triggers a switch,
/// a session path triggers a resume. (Command picking moved off
/// `OverlayPicker` onto [`SlashPalette`] in PN.6.)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerKind {
    Model,
    Session,
}

/// A blocking plugin dialog the worker handed to the App to run.
///
/// A `kage.ui.*` call suspends the plugin coroutine on the worker
/// thread; the worker forwards this over a channel and parks on the
/// carried `reply`. The App hosts the matching [`OverlayWidget`],
/// then sends the answer back, and the worker resumes the coroutine
/// with it. `reply` carries `Some(value)` to resume with that JSON
/// value or `None` to resume with `nil`.
pub enum PluginDialog {
    /// `kage.ui.select`: pick one of `items`.
    Select {
        /// Picker title.
        title: String,
        /// Rows to choose from, in the plugin's order.
        items: Vec<kage_plugin::SelectItem>,
        /// Channel the App answers on.
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
    /// `kage.ui.confirm`: a yes/no question. Resumes with a boolean
    /// (cancel counts as `false`).
    Confirm {
        /// Overlay title.
        title: String,
        /// Body text explaining what is being confirmed.
        message: String,
        /// Channel the App answers on.
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
    /// `kage.ui.input`: a single-line text prompt. Resumes with the
    /// entered string, or `nil` on cancel.
    Input {
        /// Prompt title.
        title: String,
        /// Optional placeholder shown while the field is empty.
        placeholder: Option<String>,
        /// Channel the App answers on.
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
    /// `kage.ui.editor`: a multi-line text editor. Resumes with the
    /// final buffer, or `nil` on cancel.
    Editor {
        /// Editor title.
        title: String,
        /// Optional initial buffer contents.
        prefill: Option<String>,
        /// Channel the App answers on.
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
}

/// In-flight dialog bookkeeping: the reply channel plus how to turn an
/// [`OverlayAction`] outcome into the value the parked coroutine is
/// resumed with. One variant per `kage.ui.*` dialog kind.
enum PluginDialogState {
    /// `kage.ui.select`: the overlay resolves with a stringified item
    /// index; map it back to that item's plugin value.
    Select {
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
        items: Vec<kage_plugin::SelectItem>,
    },
    /// `kage.ui.confirm`: the overlay resolves with a JSON boolean;
    /// pass it straight through.
    Confirm {
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
    /// `kage.ui.input`: the overlay resolves with the entered string;
    /// pass it straight through.
    Input {
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
    /// `kage.ui.editor`: the overlay resolves with the final buffer;
    /// pass it straight through.
    Editor {
        reply: std::sync::mpsc::Sender<Option<serde_json::Value>>,
    },
}

impl PluginDialogState {
    /// The channel the parked worker is waiting on.
    fn reply(&self) -> &std::sync::mpsc::Sender<Option<serde_json::Value>> {
        match self {
            Self::Select { reply, .. }
            | Self::Confirm { reply }
            | Self::Input { reply }
            | Self::Editor { reply } => reply,
        }
    }

    /// Value to resume the coroutine with when the overlay resolved
    /// with `value`. `None` resumes with `nil`.
    fn resolved(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        match self {
            Self::Select { items, .. } => value
                .as_str()
                .and_then(|s| s.parse::<usize>().ok())
                .and_then(|idx| items.get(idx))
                .map(|item| item.value.clone()),
            Self::Confirm { .. } => Some(serde_json::Value::Bool(value.as_bool().unwrap_or(false))),
            Self::Input { .. } | Self::Editor { .. } => Some(value.clone()),
        }
    }

    /// Value to resume the coroutine with when the user dismissed the
    /// dialog (Esc / Ctrl+C). Select resumes with `nil`; confirm
    /// resumes with `false` so the call always returns a boolean.
    fn cancelled(&self) -> Option<serde_json::Value> {
        match self {
            Self::Select { .. } | Self::Input { .. } | Self::Editor { .. } => None,
            Self::Confirm { .. } => Some(serde_json::Value::Bool(false)),
        }
    }
}

/// Closure that returns the current set of resumable sessions on
/// demand. Listing happens on the main thread when the user presses
/// `Ctrl+R`, so a fresh scan reflects any sessions written elsewhere
/// since the TUI started.
///
/// The `bool` argument is `include_all`: `false` restricts the result
/// to sessions created in the current working directory (the picker
/// default), `true` returns every session (used by `kage.session.list`
/// and `:resume` completion, and by the in-picker "all dirs" toggle).
pub type SessionLister = Box<dyn Fn(bool) -> Vec<PickItem> + Send + 'static>;

/// Unified command registry the completion engine consumes: builtin
/// commands first, then any plugin-registered commands (built once at
/// `set_plugin_commands` time and stored as `&'static` refs).
fn cmdline_registry(plugin_specs: &[&'static CommandSpec]) -> Vec<&'static CommandSpec> {
    let mut out: Vec<&'static CommandSpec> = BUILTIN_COMMANDS.iter().collect();
    out.extend(plugin_specs.iter().copied());
    out
}

/// Translate an [`OwnedArgSpec`] entry (declared at runtime by a
/// plugin) into a static [`ArgSpec`] by leaking the owned name, hint,
/// and choice strings. Callers feed the resulting value into a leaked
/// slice; the lifetime is permanent for the process.
fn leak_argspec(owned: &OwnedArgSpec) -> ArgSpec {
    match owned {
        OwnedArgSpec::Text {
            name,
            optional,
            hint,
        } => ArgSpec::Rest {
            name: leak_str(name),
            optional: *optional,
            hint: leak_str(hint),
        },
        OwnedArgSpec::Choice {
            name,
            values,
            optional,
        } => {
            let leaked_values: Vec<&'static str> = values.iter().map(|v| leak_str(v)).collect();
            ArgSpec::Choice {
                name: leak_str(name),
                values: Box::leak(leaked_values.into_boxed_slice()),
                optional: *optional,
            }
        }
        OwnedArgSpec::Path { name, optional } => ArgSpec::Path {
            name: leak_str(name),
            optional: *optional,
        },
        OwnedArgSpec::Session { name, optional } => ArgSpec::SessionId {
            name: leak_str(name),
            optional: *optional,
        },
        OwnedArgSpec::Flag { name } => ArgSpec::Flag {
            name: leak_str(name),
        },
    }
}

fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

/// Recursive help renderer: pushes one line per command, then recurses
/// into each subcommand with an indented prefix so nested commands
/// appear under their parent.
fn help_render_spec(lines: &mut Vec<String>, spec: &CommandSpec, prefix: &str, depth: usize) {
    let aliases = if spec.aliases.is_empty() {
        String::new()
    } else {
        format!(" ({})", spec.aliases.join(", "))
    };
    let hints = crate::command::arg_hints_text(spec.args);
    let arg_hint = if hints.is_empty() {
        String::new()
    } else {
        format!(" {hints}")
    };
    let indent = "  ".repeat(depth + 1);
    lines.push(format!(
        "{indent}{prefix}{name}{aliases}{arg_hint}   {desc}",
        name = spec.name,
        desc = spec.description,
    ));
    for sub in spec.subcommands {
        help_render_spec(lines, sub, &format!("{prefix}{} ", spec.name), depth + 1);
    }
}

/// [`Resolver`] backed by the live App state: model choices and
/// plugin-registered commands the user has imported, plus the bundled
/// theme list and any session lister the host provided. Paths return
/// empty until PU.4 wires file-system completion.
struct AppResolver<'a> {
    models: &'a [PickItem],
    plugin_commands: &'a [(String, String)],
    sessions: Option<&'a SessionLister>,
    themes_dir: Option<&'a std::path::Path>,
}

impl Resolver for AppResolver<'_> {
    fn dynamic_choice(&self, source: &ArgSource) -> Vec<String> {
        match source {
            ArgSource::Models => self.models.iter().map(|p| p.value.clone()).collect(),
            ArgSource::Themes => crate::theme::Theme::available_names(self.themes_dir),
            ArgSource::PluginCommands => self
                .plugin_commands
                .iter()
                .map(|(n, _)| n.clone())
                .collect(),
            ArgSource::Sessions => self
                .sessions
                .map(|f| f(true))
                .unwrap_or_default()
                .into_iter()
                .map(|item| item.value)
                .collect(),
            ArgSource::Custom(f) => f(),
        }
    }

    fn sessions(&self) -> Vec<String> {
        self.sessions
            .map(|f| f(true))
            .unwrap_or_default()
            .into_iter()
            .map(|item| item.value)
            .collect()
    }
}

/// Runtime state for the interactive TUI loop.
pub struct App {
    buffer: SharedBuffer,
    input: InputState,
    requests: Sender<RunRequest>,
    /// Available `provider:model` ids the model picker offers. Empty
    /// when the host has not registered any models with the App.
    model_choices: Vec<PickItem>,
    /// Active modal overlay, if any. Drives both render and input
    /// routing while present.
    picker: Option<OverlayPicker>,
    /// Which picker is open, mirroring [`Self::picker`]. Used to
    /// dispatch the picked value to the right `RunRequest`.
    picker_kind: Option<PickerKind>,
    /// Scope of the open session picker: `false` (default) shows only
    /// this directory's sessions; `true` shows all. Toggled in-picker
    /// with `Ctrl+A`.
    session_scope_all: bool,
    /// Open `:settings` overlay. A modal sibling of [`Self::picker`];
    /// on resolve its edits are applied live and persisted.
    settings_overlay: Option<SettingsOverlay>,
    /// Open `:tree` session-forest browser, a modal sibling of the
    /// picker. On resolve it dispatches resume / fork / delete.
    session_tree: Option<SessionTreeOverlay>,
    /// Open right-click context menu, if any. A light modal layer:
    /// while present it owns the keyboard and intercepts mouse clicks
    /// (a click on a row runs its action, a click off it dismisses).
    context_menu: Option<ContextMenu>,
    /// Produces the session forest for `:tree`. Wired by the host;
    /// `None` disables the command.
    session_tree_source: Option<SessionTreeSource>,
    /// Provider of resumable sessions for the session picker. None
    /// disables the picker (Ctrl+R is a no-op).
    session_lister: Option<SessionLister>,
    /// Open `:` command line, if any. While present it owns key input
    /// and replaces the status bar's mode pill.
    cmdline: Option<CommandLine>,
    /// Open `/` slash palette overlay, if any. Wraps a [`CommandLine`]
    /// and renders as a centered modal; shares the parser, completer,
    /// and arg-editing flow with the `:` cmdline.
    slash_palette: Option<SlashPalette>,
    /// Open `/` search line, if any. Reuses the [`CommandLine`]
    /// widget; painted with a `/` prefix instead of `:`.
    search_line: Option<CommandLine>,
    /// The most recently submitted search pattern. While set, blocks
    /// containing the pattern render with a Match emphasis and `n` /
    /// `N` walk between them.
    search_pattern: Option<String>,
    /// Cached set of block indices matching `search_pattern`.
    /// Recomputed when the pattern or buffer version changes.
    search_match_set: std::collections::HashSet<usize>,
    /// Buffer version snapshot used to validate `search_match_set`.
    search_match_version: u64,
    /// Status bar context the host populates: live model id and a
    /// short session-id pill. Held as `Arc<Mutex<...>>` so the worker
    /// thread can update them out from under the renderer (model
    /// switches mid-session).
    status_model: Option<Arc<Mutex<String>>>,
    status_session_id: Option<String>,
    /// Plugin-registered command names + descriptions for palette
    /// display. Builtin names take precedence on collision.
    plugin_commands: Vec<(String, String)>,
    /// `(alias, canonical name)` for plugin commands. The cmdline
    /// resolves an alias to its canonical name before dispatch so the
    /// plugin runtime only ever needs to look a command up by name.
    plugin_command_aliases: Vec<(String, String)>,
    /// Canonical names of `kage.override_command` registrations. These
    /// may shadow a built-in and are dispatched ahead of it.
    plugin_command_overrides: Vec<String>,
    /// Synthetic `CommandSpec` entries built from `plugin_commands`
    /// at registration time. Stored as `&'static` via `Box::leak` so
    /// the completion engine can mix them with the static builtin
    /// registry. Cleared and re-built on every `set_plugin_commands`.
    plugin_command_specs: Vec<&'static CommandSpec>,
    /// Parsed plugin keybindings: `(matcher, canonical chord)`. A key
    /// matching one dispatches [`RunRequest::InvokePluginKeybinding`].
    /// Checked after modal layers but before builtin key handling so a
    /// plugin chord wins over the builtin binding for that key.
    plugin_keybindings: Vec<(Chord, String)>,
    /// Parsed `[keybindings]` config: `(matcher, chord text, command
    /// line)`. A matching key runs the command string through the
    /// same executor as the `:` cmdline. Checked before plugin
    /// keybindings so user config is authoritative. The chord text is
    /// kept for `:keybindings` to echo back.
    config_keybindings: Vec<(Chord, String, String)>,
    /// Status-bar widgets supplied by plugins via
    /// `kage.register_widget`. Each entry's `render(width)` runs once
    /// per redraw and the resulting string is painted on the right
    /// edge of the status bar.
    plugin_widgets: Vec<Arc<kage_plugin::LuaWidget>>,
    /// Per-frame cache of [`Self::plugin_widgets`] outputs. Lives on
    /// the App so [`view::StatusCtx`] can borrow it; rebuilt at the
    /// top of [`Self::render_into`].
    plugin_widget_texts: Vec<String>,
    /// Transient status entries populated by `kage.set_status` /
    /// `kage.clear_status`. Each redraw snapshots the map into
    /// [`Self::plugin_status_cache`].
    plugin_status: Option<kage_plugin::SharedStatus>,
    /// Per-frame snapshot of [`Self::plugin_status`]. Owned so the
    /// view layer can borrow without holding the plugin status mutex.
    plugin_status_cache: Vec<(String, String)>,
    /// JSON view of the live session usage so `kage.context_usage()`
    /// can return up-to-date numbers. The host updates the inner
    /// value at the same cadence as the modeline (per-render and
    /// after every turn).
    plugin_usage: Option<kage_plugin::SharedUsage>,
    /// Pending compact request flag populated by `kage.compact()`.
    /// Drained between event polls; a non-empty `Some` dispatches a
    /// [`RunRequest::CompactNow`] to the worker.
    plugin_compact_request: Option<kage_plugin::SharedCompactRequest>,
    /// Snapshot of resumable sessions exposed to `kage.session.list`.
    /// Refreshed from [`Self::session_lister`] each redraw.
    plugin_session_list: Option<kage_plugin::SharedSessionList>,
    /// Pending fork-request slot populated by `kage.session.fork`.
    /// Drained between event polls; the worker performs the fork.
    plugin_fork_request: Option<kage_plugin::SharedForkRequest>,
    /// Pending reseat slot populated by the `session_write`
    /// `kage.session.switch` / `fork_to`. Drained between event polls
    /// and relayed as [`RunRequest::SwitchSession`] to the worker.
    plugin_switch_request: Option<kage_plugin::SharedSwitchRequest>,
    /// Theme snapshot `kage.theme.current()` / `list()` read from.
    /// Refreshed each redraw with the active theme + bundled names.
    plugin_theme_state: Option<kage_plugin::SharedThemeState>,
    /// Pending `kage.theme.set` slot. Drained between event polls and
    /// applied on this (UI) thread, the same path as `:theme set`.
    plugin_theme_request: Option<kage_plugin::SharedThemeRequest>,
    /// Header-chrome slot populated by `kage.ui.set_header`. Snapshotted
    /// per redraw; when a renderer is present its styled lines replace
    /// the built-in status bar.
    /// Autocomplete providers from `kage.add_autocomplete_provider`,
    /// in registration order. Consulted in reverse (last registered
    /// wins) on each prompt-input change; the first provider that
    /// returns items populates [`Self::input_completion`].
    autocomplete_providers: Vec<Arc<kage_plugin::LuaAutocompleteProvider>>,
    /// Open input autocomplete popup, if the active provider returned
    /// candidates for the current prefix. `None` when closed.
    input_completion: Option<InputCompletion>,
    /// Workdir the built-in `@file` completion lists under. `None`
    /// disables that fallback (plugin providers still work).
    completion_workdir: Option<std::path::PathBuf>,
    /// User theme directory (`~/.config/kage/themes`). Names that are
    /// not bundled are resolved to `<name>.toml` here. `None` (tests,
    /// no home) restricts theme switching to the bundled set.
    themes_dir: Option<std::path::PathBuf>,
    /// Shared raw terminal-input hooks from `kage.on_terminal_input`.
    /// Snapshotted per keystroke (so an `off` takes effect at once)
    /// and offered each key before any modal layer; a truthy return
    /// consumes the event. `None` until wired.
    terminal_hooks: Option<kage_plugin::RegisteredTerminalHooks>,
    plugin_header: Option<kage_plugin::SharedChrome>,
    /// Footer-chrome slot populated by `kage.ui.set_footer`. Replaces
    /// the built-in modeline when a renderer is present.
    plugin_footer: Option<kage_plugin::SharedChrome>,
    /// Per-frame snapshot of the header renderer's output. Lives on the
    /// App so [`view::StatusCtx`] can borrow it without holding the
    /// chrome mutex; rebuilt by [`Self::refresh_plugin_widget_texts`].
    plugin_header_lines: Vec<kage_plugin::ChromeLine>,
    /// Per-frame snapshot of the footer renderer's output.
    plugin_footer_lines: Vec<kage_plugin::ChromeLine>,
    /// Pending request to toggle terminal mouse capture, applied by
    /// `run` between iterations. `None` means leave the capture state
    /// as-is. The indirection exists because `run_command` can't
    /// reach `Tui` directly; only [`Self::run`] holds it.
    pending_mouse_capture: Option<bool>,
    /// In-progress mouse gesture started by a left-button press. The
    /// tuple is `(down_row, down_block_idx, dragged)`. `dragged` flips
    /// to true the first time a drag event arrives; on `Up`, a
    /// non-dragged click on the block's header row toggles its fold,
    /// while a dragged release copies the highlighted selection
    /// straight to the clipboard (same path as `y`).
    mouse_drag_anchor: Option<(u16, usize, bool)>,
    /// Active screen selection in `(virtual_row, col)` coordinates,
    /// where `virtual_row` is the index of the row across the whole
    /// rendered buffer (independent of scroll position). Painted as
    /// a bg overlay over whatever the renderer drew so it covers
    /// tool blocks and chrome equally without changing layout.
    /// Tracking in virtual-row space lets the selection survive a
    /// scroll: rows that go off-screen stay selected, and their
    /// previously-captured text remains available for yank.
    screen_selection: Option<((usize, u16), (usize, u16))>,
    /// Cell snapshots accumulated for every virtual row the user has
    /// dragged through during the current selection. Indexed by
    /// virtual row; each entry stores the row's painted chars and
    /// per-cell decoration flag as captured when it was visible.
    /// Cleared on `MouseDown` (new selection) or after a yank. Lets
    /// `y` recover the full selected text even when part of the
    /// selection has scrolled off-screen.
    captured_rows: std::collections::BTreeMap<usize, Vec<view::CapturedCell>>,
    /// Last DECSCUSR cursor shape we emitted to the terminal, keyed
    /// by `(mode, pane_focused_on_input)`. Stored so [`Self::draw`]
    /// can skip the escape on frames where the cursor shape would be
    /// identical, avoiding a flicker on terminals that briefly hide
    /// the cursor when the style is reapplied.
    last_cursor_style: Option<(Mode, bool)>,
    /// Optional shared snapshot of the current session's running
    /// token totals + context window. When `Some`, the renderer
    /// claims a one-row modeline below the input card and paints
    /// `model :: in/out :: total/window (pct)`. Updated by the host
    /// worker thread after every turn.
    session_usage: Option<crate::usage::SharedSessionUsage>,
    /// Optional handle on the host's cancellation flag. When set,
    /// `Cancel` actions (Ctrl-C, `:cancel`) flip it synchronously on
    /// the foreground event-loop thread instead of going through
    /// the worker request channel - which is essential because the
    /// worker is busy inside `run_with_hooks` while the in-flight
    /// turn is what we want to cancel, so a queued `RunRequest::Cancel`
    /// would not fire until *after* the turn finishes naturally.
    cancel_flag: Option<CancelFlag>,
    /// Shared FIFO of user prompts queued while a run is in flight. A
    /// text-only `Submit` issued mid-run lands here so the agent loop
    /// picks it up at the next turn boundary via `Hooks::get_steering`,
    /// instead of buffering behind the next `RunRequest` and only
    /// firing after the whole run completes. `None` until the host
    /// registers a queue, in which case mid-run submits fall back to
    /// the channel path (today's behavior).
    steering: Option<crate::events::SharedSteering>,
    /// Shared queue of ephemeral toast notifications painted as a
    /// top-right overlay over the conversation buffer. The handle is
    /// cloned to whatever sinks need to push (the App's own
    /// `notify`, the host log sink for plugin `kage.notify`, etc.).
    /// When `None`, `notify(...)` is a silent no-op: toasts are
    /// decorative and never load-bearing.
    toasts: Option<SharedToasts>,
    /// Channel the worker pushes blocking [`PluginDialog`] requests
    /// onto (`kage.ui.select`). Drained between event polls; while a
    /// dialog is open the worker thread is parked awaiting the answer.
    dialog_rx: Option<std::sync::mpsc::Receiver<PluginDialog>>,
    /// The overlay hosting the current plugin dialog, if any. A
    /// trait object so every `kage.ui.*` dialog (picker, confirm,
    /// input, editor) shares one hosting path.
    plugin_overlay: Option<Box<dyn crate::overlay::OverlayWidget>>,
    /// Bookkeeping for the dialog currently in [`Self::plugin_overlay`]:
    /// where to send the answer and how to map the overlay's outcome.
    active_dialog: Option<PluginDialogState>,
}

mod actions;
mod events;
mod keys;
mod lifecycle;
mod overlays;
mod wiring;

/// Translate an absolute terminal `(row, col)` mouse position to a
/// Best-effort label for the current mode, exposed for the host's
/// status-bar widget.
#[must_use]
pub fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::Insert => "insert",
        Mode::Visual => "visual",
    }
}

/// Build the key descriptor a `kage.on_terminal_input` handler
/// receives. See `kage_plugin::terminal_input` for the schema.
fn key_event_to_json(key: ratatui::crossterm::event::KeyEvent) -> serde_json::Value {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    let mods = key.modifiers;
    let (code, ch): (String, Option<String>) = match key.code {
        KeyCode::Char(c) => ("char".to_owned(), Some(c.to_string())),
        KeyCode::F(n) => (format!("f{n}"), None),
        KeyCode::Enter => ("enter".to_owned(), None),
        KeyCode::Esc => ("esc".to_owned(), None),
        KeyCode::Tab => ("tab".to_owned(), None),
        KeyCode::BackTab => ("backtab".to_owned(), None),
        KeyCode::Backspace => ("backspace".to_owned(), None),
        KeyCode::Up => ("up".to_owned(), None),
        KeyCode::Down => ("down".to_owned(), None),
        KeyCode::Left => ("left".to_owned(), None),
        KeyCode::Right => ("right".to_owned(), None),
        KeyCode::Home => ("home".to_owned(), None),
        KeyCode::End => ("end".to_owned(), None),
        KeyCode::PageUp => ("pageup".to_owned(), None),
        KeyCode::PageDown => ("pagedown".to_owned(), None),
        KeyCode::Delete => ("delete".to_owned(), None),
        KeyCode::Insert => ("insert".to_owned(), None),
        _ => ("other".to_owned(), None),
    };
    serde_json::json!({
        "code": code,
        "char": ch,
        "ctrl": mods.contains(KeyModifiers::CONTROL),
        "alt": mods.contains(KeyModifiers::ALT),
        "shift": mods.contains(KeyModifiers::SHIFT),
    })
}

#[cfg(test)]
mod tests;
