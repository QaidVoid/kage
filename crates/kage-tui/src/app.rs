//! Interactive TUI event loop.
//!
//! [`App::run`] owns the [`Tui`] and a [`SharedBuffer`], polls crossterm
//! key events, drives [`InputState`], applies [`InputAction`]s to the
//! buffer, and redraws the screen ~30 times a second. Submitting a
//! prompt fires a `RunRequest` through the provided sink; the host is
//! responsible for spawning the agent loop on a worker thread and
//! pushing its events into the same `SharedBuffer` via [`TuiHooks`].

use std::io::Write;
use std::sync::mpsc::{Sender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use kage_core::CancelFlag;
use ratatui::crossterm::event::{self, Event, KeyEventKind, MouseEventKind};

use crate::toast::{self, SharedToasts, Toast, ToastKind};

use crate::chord::Chord;
use crate::cmdline::{CommandLine, CommandLineEvent};
use crate::cmdparse::{EmptyResolver, Resolver};
use crate::command::{
    ArgSource, ArgSpec, BUILTIN_COMMANDS, CommandCategory, CommandSpec, OwnedArgSpec, PluginCommand,
};
use crate::error::TuiError;
use crate::events::SharedBuffer;
use crate::input::{InputAction, InputState, Mode, Pane};
use crate::layout::{input_height_for, split};
use crate::overlay::{
    CompletionAction, InputCompletion, OverlayAction, OverlayPicker, SessionTreeOverlay,
    SessionTreeSource, SettingsInit, SettingsOverlay, SlashContext, SlashPalette, file_completions,
    prefix_before_cursor,
};
use crate::picker::PickItem;
use crate::terminal::Tui;
use crate::view;

/// Lines scrolled per mouse wheel notch.
const MOUSE_SCROLL_LINES: i32 = 3;

/// Outcome of validating a command before execution.
///
/// [`CommandResult::Done`] means the command was dispatched (or the
/// command name was empty). [`CommandResult::ValidationError`] means
/// the argument schema rejected the input; the caller should keep the
/// cmdline open and display the error inline.
#[derive(Debug)]
enum CommandResult {
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
    /// New user prompt to submit to the agent loop.
    Submit(String),
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
pub type SessionLister = Box<dyn Fn() -> Vec<PickItem> + Send + 'static>;

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
                .map(|f| f())
                .unwrap_or_default()
                .into_iter()
                .map(|item| item.value)
                .collect(),
            ArgSource::Custom(f) => f(),
        }
    }

    fn sessions(&self) -> Vec<String> {
        self.sessions
            .map(|f| f())
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
    /// Open `:settings` overlay. A modal sibling of [`Self::picker`];
    /// on resolve its edits are applied live and persisted.
    settings_overlay: Option<SettingsOverlay>,
    /// Open `:tree` session-forest browser, a modal sibling of the
    /// picker. On resolve it dispatches resume / fork / delete.
    session_tree: Option<SessionTreeOverlay>,
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
    /// Status bar context the host populates: live model id and a
    /// short session-id pill. Held as `Arc<Mutex<...>>` so the worker
    /// thread can update them out from under the renderer (model
    /// switches mid-session).
    status_model: Option<Arc<Mutex<String>>>,
    status_session_id: Option<String>,
    /// Plugin-registered command names + descriptions for palette
    /// display. Builtin names take precedence on collision.
    plugin_commands: Vec<(String, String)>,
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
    /// while a dragged release leaves the screen selection in place
    /// for the user to yank with `y`.
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

impl App {
    /// Construct an app that pushes prompts into `requests`. The
    /// receiver side is owned by the host's worker driver.
    #[must_use]
    pub fn new(buffer: SharedBuffer, requests: Sender<RunRequest>) -> Self {
        Self {
            buffer,
            input: InputState::new(),
            requests,
            model_choices: Vec::new(),
            picker: None,
            picker_kind: None,
            settings_overlay: None,
            session_tree: None,
            session_tree_source: None,
            session_lister: None,
            cmdline: None,
            slash_palette: None,
            status_model: None,
            status_session_id: None,
            plugin_commands: Vec::new(),
            plugin_command_specs: Vec::new(),
            plugin_keybindings: Vec::new(),
            plugin_widgets: Vec::new(),
            plugin_widget_texts: Vec::new(),
            plugin_status: None,
            plugin_status_cache: Vec::new(),
            plugin_usage: None,
            plugin_compact_request: None,
            plugin_session_list: None,
            plugin_fork_request: None,
            plugin_theme_state: None,
            plugin_theme_request: None,
            autocomplete_providers: Vec::new(),
            input_completion: None,
            completion_workdir: None,
            themes_dir: None,
            terminal_hooks: None,
            plugin_header: None,
            plugin_footer: None,
            plugin_header_lines: Vec::new(),
            plugin_footer_lines: Vec::new(),
            search_line: None,
            search_pattern: None,
            mouse_drag_anchor: None,
            pending_mouse_capture: None,
            screen_selection: None,
            captured_rows: std::collections::BTreeMap::new(),
            last_cursor_style: None,
            session_usage: None,
            cancel_flag: None,
            toasts: None,
            dialog_rx: None,
            plugin_overlay: None,
            active_dialog: None,
        }
    }

    /// Hand the App a shared session-usage snapshot. While set, the
    /// renderer reserves a one-row modeline below the input card and
    /// paints the snapshot's model + token totals + context-window
    /// fill. Pass `None` (or never call this) to keep the modeline
    /// collapsed.
    pub fn set_session_usage(&mut self, usage: crate::usage::SharedSessionUsage) {
        self.session_usage = Some(usage);
    }

    /// Register the host's cancellation flag so [`InputAction::Cancel`]
    /// and `:cancel` can flip it directly on the event-loop thread,
    /// bypassing the worker request queue. Without this, cancellation
    /// of an in-flight turn does not take effect until the turn ends
    /// naturally because the worker thread is blocked inside the
    /// agent loop and cannot drain its request channel.
    pub fn set_cancel_flag(&mut self, flag: CancelFlag) {
        self.cancel_flag = Some(flag);
    }

    /// Register the shared toast queue. While set, App-internal
    /// `notify(...)` calls and external sinks holding a clone of
    /// the same handle push into a top-right overlay. Without it
    /// `notify(...)` silently drops the message - toasts are
    /// decorative, never load-bearing.
    pub fn set_toasts(&mut self, toasts: SharedToasts) {
        self.toasts = Some(toasts);
    }

    /// Snapshot live (non-expired) toasts for one frame, dropping
    /// expired entries in the process. Returns `None` when no toast
    /// queue is registered or the lock is poisoned.
    fn live_toasts(&self) -> Vec<Toast> {
        let Some(handle) = &self.toasts else {
            return Vec::new();
        };
        let now = Instant::now();
        let _ = toast::prune_expired(handle, now);
        handle
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Earliest deadline at which a live toast will expire, used by
    /// the event loop to wake up just in time to repaint without
    /// waiting for an unrelated key event.
    fn next_toast_deadline(&self) -> Option<Instant> {
        let handle = self.toasts.as_ref()?;
        let q = handle.lock().ok()?;
        q.iter().map(|t| t.expires_at).min()
    }

    /// Trip the registered cancel flag if any, then forward a
    /// `RunRequest::Cancel` to the worker for any extra cleanup that
    /// arm performs (currently it just calls `.cancel()` again, which
    /// is idempotent - the channel send is a fallback for hosts that
    /// have not registered a flag via [`Self::set_cancel_flag`]).
    fn trip_cancel(&mut self) {
        if let Some(flag) = &self.cancel_flag {
            flag.cancel();
        }
        let _ = self.send_request(RunRequest::Cancel);
    }

    /// Whether the host has registered a session-usage handle. Used
    /// by the layout split to decide if the modeline row claims a
    /// line of vertical space.
    fn modeline_visible(&self) -> bool {
        self.session_usage.is_some()
    }

    /// Snapshot the session-usage handle, returning `None` when the
    /// host has not registered one or the lock is poisoned.
    fn session_usage_snapshot(&self) -> Option<crate::usage::SessionUsage> {
        self.session_usage
            .as_ref()
            .and_then(|h| h.lock().ok().map(|g| g.clone()))
    }

    /// Register the plugin commands the host wants exposed in the
    /// palette and on the `:` line. Names that collide with built-in
    /// specs are dropped; the host should log a warning at
    /// registration time.
    ///
    /// Builds one [`CommandSpec`] per plugin command, leaking the
    /// owned name, description, and per-arg schema into `&'static`
    /// storage so plugin commands participate in the same completion
    /// engine the builtins use. The leaked storage is bounded by the
    /// number of plugin commands the user installs.
    pub fn set_plugin_commands(&mut self, mut commands: Vec<PluginCommand>) {
        commands.retain(|c| crate::command::find_builtin_command(&c.name).is_none());
        self.plugin_command_specs.clear();
        for cmd in &commands {
            let name_static: &'static str = Box::leak(cmd.name.clone().into_boxed_str());
            let desc_static: &'static str =
                Box::leak(format!("{}  [plugin]", cmd.description).into_boxed_str());
            let args_owned: Vec<ArgSpec> = cmd.args.iter().map(leak_argspec).collect();
            let args_static: &'static [ArgSpec] = Box::leak(args_owned.into_boxed_slice());
            let spec: &'static CommandSpec = Box::leak(Box::new(CommandSpec {
                name: name_static,
                aliases: &[],
                description: desc_static,
                category: CommandCategory::Both,
                args: args_static,
                subcommands: &[],
            }));
            self.plugin_command_specs.push(spec);
        }
        self.plugin_commands = commands
            .into_iter()
            .map(|c| (c.name, c.description))
            .collect();
    }

    /// Hand the App a shared handle on the active `provider:model`
    /// string so the status bar reflects model switches in real time.
    pub fn set_status_model(&mut self, model: Arc<Mutex<String>>) {
        self.status_model = Some(model);
    }

    /// Set the short session-id pill shown on the right of the status
    /// bar.
    pub fn set_status_session_id(&mut self, short_id: String) {
        self.status_session_id = Some(short_id);
    }

    /// Replace the model list shown when the user opens the in-TUI
    /// picker. The host computes this from its provider registry +
    /// catalog.
    pub fn set_model_choices(&mut self, choices: Vec<PickItem>) {
        self.model_choices = choices;
    }

    /// Seed the prompt history with persisted entries (oldest first).
    /// Truncated to [`crate::input::HISTORY_MAX`] keeping the most
    /// recent.
    pub fn set_history(&mut self, entries: Vec<String>) {
        self.input.set_history(entries);
    }

    /// Register the closure that produces the session picker's items
    /// at the moment of opening. Without this, `Ctrl+R` is a no-op.
    pub fn set_session_lister(&mut self, lister: SessionLister) {
        self.session_lister = Some(lister);
    }

    /// Register the closure that produces the `:tree` session forest
    /// at open time. Without this, `:tree` reports it is unavailable.
    pub fn set_session_tree_source(&mut self, source: SessionTreeSource) {
        self.session_tree_source = Some(source);
    }

    /// Replace the list of plugin-supplied status-bar widgets.
    /// `render(width)` runs once per redraw inside the plugin runtime's
    /// Lua mutex; widgets that produce a non-empty string are painted
    /// on the right edge of the status bar in registration order.
    pub fn set_plugin_widgets(&mut self, widgets: Vec<Arc<kage_plugin::LuaWidget>>) {
        self.plugin_widgets = widgets;
    }

    /// Wire the shared status map populated by `kage.set_status` /
    /// `kage.clear_status`. Without this, those Lua calls still
    /// succeed inside the runtime but the host status bar never paints
    /// the values.
    pub fn set_plugin_status(&mut self, status: kage_plugin::SharedStatus) {
        self.plugin_status = Some(status);
    }

    /// Wire the shared usage snapshot read by `kage.context_usage()`.
    /// Without this, plugins always see `nil`.
    pub fn set_plugin_usage(&mut self, usage: kage_plugin::SharedUsage) {
        self.plugin_usage = Some(usage);
    }

    /// Wire the shared pending-compact slot populated by
    /// `kage.compact(prompt?)`. Without this, plugins can still call
    /// the API but the host never dispatches the requested compaction.
    pub fn set_plugin_compact_request(&mut self, request: kage_plugin::SharedCompactRequest) {
        self.plugin_compact_request = Some(request);
    }

    /// Wire the shared session list `kage.session.list()` reads from.
    /// Without this, plugins always see an empty list.
    pub fn set_plugin_session_list(&mut self, list: kage_plugin::SharedSessionList) {
        self.plugin_session_list = Some(list);
    }

    /// Wire the shared pending-fork slot populated by
    /// `kage.session.fork(at?)`. Without this, plugins can call the
    /// API but the host never performs the fork.
    pub fn set_plugin_fork_request(&mut self, request: kage_plugin::SharedForkRequest) {
        self.plugin_fork_request = Some(request);
    }

    /// Wire the theme snapshot and pending-switch slots so
    /// `kage.theme.*` can read the active theme / list and request a
    /// switch. Without these the read APIs see empty values and
    /// `kage.theme.set` is a no-op.
    pub fn set_plugin_theme(
        &mut self,
        state: kage_plugin::SharedThemeState,
        request: kage_plugin::SharedThemeRequest,
    ) {
        self.plugin_theme_state = Some(state);
        self.plugin_theme_request = Some(request);
    }

    /// Wire the header/footer chrome slots populated by
    /// `kage.ui.set_header` / `kage.ui.set_footer`. Each redraw the
    /// active renderer (if any) is called with the row width and its
    /// styled lines replace the built-in status bar / modeline. Without
    /// this the Lua calls still register a renderer but the host never
    /// paints it.
    pub fn set_plugin_chrome(
        &mut self,
        header: kage_plugin::SharedChrome,
        footer: kage_plugin::SharedChrome,
    ) {
        self.plugin_header = Some(header);
        self.plugin_footer = Some(footer);
    }

    /// Wire the autocomplete provider stack from
    /// `kage.add_autocomplete_provider`. Without this the Lua calls
    /// still register providers in the runtime but the input never
    /// queries them. Providers run synchronously inside the plugin
    /// runtime's Lua mutex on each prompt-input change.
    pub fn set_plugin_autocomplete(
        &mut self,
        providers: Vec<Arc<kage_plugin::LuaAutocompleteProvider>>,
    ) {
        self.autocomplete_providers = providers;
    }

    /// Wire the raw terminal-input hook list from
    /// `kage.on_terminal_input`. Without this the Lua calls still
    /// register hooks in the runtime but no key is ever offered to
    /// them. Hooks run synchronously inside the plugin runtime's Lua
    /// mutex, before every modal layer, on each keystroke.
    pub fn set_plugin_terminal_hooks(&mut self, hooks: kage_plugin::RegisteredTerminalHooks) {
        self.terminal_hooks = Some(hooks);
    }

    /// Apply the configured editor model at startup (and live from
    /// the settings dialog). `true` selects non-modal editing.
    pub fn set_editor_modeless(&mut self, on: bool) {
        self.input.set_modeless(on);
    }

    /// Set the workdir the built-in `@file` autocomplete lists under.
    /// Without this the `@file` fallback is disabled; plugin providers
    /// still function.
    pub fn set_workdir(&mut self, workdir: std::path::PathBuf) {
        self.completion_workdir = Some(workdir);
    }

    /// Point theme resolution at the user theme directory
    /// (`~/.config/kage/themes`). Without this only bundled themes
    /// resolve; with it, `<name>.toml` files there become selectable
    /// everywhere a bundled name is.
    pub fn set_themes_dir(&mut self, dir: std::path::PathBuf) {
        self.themes_dir = Some(dir);
    }

    /// Apply the configured startup theme (bundled name or a user
    /// `<name>.toml`). Silent on success; a bad name surfaces an
    /// inline error and leaves the default palette in place. Call
    /// after [`Self::set_themes_dir`] so user themes resolve.
    pub fn apply_startup_theme(&mut self, name: &str) {
        if name.is_empty() {
            return;
        }
        self.apply_theme_resolved(name, false);
    }

    /// Register the plugin keybindings the App should dispatch.
    /// `chords` are canonical strings from the plugin runtime; an
    /// entry that fails to parse is dropped (the runtime already
    /// validated the grammar, so this only guards internal drift).
    pub fn set_plugin_keybindings(&mut self, chords: Vec<String>) {
        self.plugin_keybindings = chords
            .into_iter()
            .filter_map(|c| Chord::parse(&c).map(|m| (m, c)))
            .collect();
    }

    /// Wire the channel the worker pushes blocking [`PluginDialog`]
    /// requests onto. Without this, `kage.ui.select` has nowhere to
    /// surface and the worker's send fails, which it treats as a
    /// cancel (the plugin call returns `nil`).
    pub fn set_plugin_dialog(&mut self, rx: std::sync::mpsc::Receiver<PluginDialog>) {
        self.dialog_rx = Some(rx);
    }

    fn refresh_plugin_widget_texts(&mut self, width: u16) {
        self.plugin_widget_texts = self
            .plugin_widgets
            .iter()
            .map(|w| w.render(width))
            .collect();
        self.plugin_header_lines = self
            .plugin_header
            .as_ref()
            .and_then(|slot| slot.lock().ok().and_then(|g| g.clone()))
            .map(|c| c.render(width))
            .unwrap_or_default();
        self.plugin_footer_lines = self
            .plugin_footer
            .as_ref()
            .and_then(|slot| slot.lock().ok().and_then(|g| g.clone()))
            .map(|c| c.render(width))
            .unwrap_or_default();
        self.plugin_status_cache.clear();
        if let Some(status) = self.plugin_status.as_ref()
            && let Ok(map) = status.lock()
        {
            self.plugin_status_cache
                .extend(map.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if let Some(usage_slot) = self.plugin_usage.as_ref()
            && let Some(snap) = self.session_usage_snapshot()
            && let Ok(mut slot) = usage_slot.lock()
        {
            *slot = serde_json::json!({
                "model": snap.model,
                "input_tokens": snap.input_tokens,
                "output_tokens": snap.output_tokens,
                "cache_read_tokens": snap.cache_read_tokens,
                "cache_write_tokens": snap.cache_write_tokens,
                "current_context": snap.current_context,
                "context_window": snap.context_window,
                "working": snap.working,
            });
        }
    }

    /// Drain any pending `kage.compact()` request and forward it as
    /// [`RunRequest::CompactNow`] to the worker. The optional prompt
    /// is currently advisory; PE.C.4 will wire it into the compact
    /// hook.
    fn drain_plugin_compact_request(&mut self) {
        let Some(slot) = self.plugin_compact_request.as_ref() else {
            return;
        };
        let pending = slot.lock().ok().and_then(|mut g| g.take());
        if pending.is_some() {
            let _ = self.send_request(RunRequest::CompactNow);
        }
    }

    /// Drain any pending `kage.session.fork()` request and forward it
    /// as [`RunRequest::ForkSession`] to the worker. The worker copies
    /// the current session through entry `at` into a fresh session
    /// file.
    fn drain_plugin_fork_request(&mut self) {
        let Some(slot) = self.plugin_fork_request.as_ref() else {
            return;
        };
        let pending = slot.lock().ok().and_then(|mut g| g.take());
        if let Some(at) = pending {
            let _ = self.send_request(RunRequest::ForkSession { at });
        }
    }

    /// Refresh the theme snapshot `kage.theme.*` reads, then drain a
    /// pending `kage.theme.set` and apply it on this thread (the same
    /// path as `:theme set`, so an unknown name surfaces an inline
    /// error rather than failing silently).
    fn drain_plugin_theme(&mut self) {
        let pending = self
            .plugin_theme_request
            .as_ref()
            .and_then(|slot| slot.lock().ok().and_then(|mut g| g.take()));
        if let Some(name) = pending {
            self.apply_theme_by_name(&name);
        }
        if let Some(state) = self.plugin_theme_state.as_ref()
            && let Ok(mut s) = state.lock()
        {
            s.current = crate::theme::current().name;
            s.available = crate::theme::Theme::available_names(self.themes_dir.as_deref());
        }
    }

    /// Drain one pending blocking [`PluginDialog`] and open its
    /// overlay. Skipped while another overlay (picker or an earlier
    /// plugin dialog) is up: the worker stays parked and the request
    /// is taken on a later tick once the screen is free (the bridge is
    /// single-slot, so at most one is queued). An empty item list
    /// resolves immediately to "cancelled" rather than opening a dead
    /// picker.
    fn drain_plugin_dialog(&mut self) {
        if self.picker.is_some() || self.plugin_overlay.is_some() {
            return;
        }
        let Some(rx) = self.dialog_rx.as_ref() else {
            return;
        };
        let Ok(dialog) = rx.try_recv() else {
            return;
        };
        match dialog {
            PluginDialog::Select {
                title,
                items,
                reply,
            } => {
                if items.is_empty() {
                    let _ = reply.send(None);
                    return;
                }
                let picks = items
                    .iter()
                    .enumerate()
                    .map(|(idx, item)| PickItem {
                        value: idx.to_string(),
                        label: item.label.clone(),
                        badge: None,
                    })
                    .collect();
                self.plugin_overlay = Some(Box::new(OverlayPicker::new(title, picks)));
                self.active_dialog = Some(PluginDialogState::Select { reply, items });
            }
            PluginDialog::Confirm {
                title,
                message,
                reply,
            } => {
                self.plugin_overlay = Some(Box::new(crate::overlay::ConfirmOverlay::new(
                    title, message,
                )));
                self.active_dialog = Some(PluginDialogState::Confirm { reply });
            }
            PluginDialog::Input {
                title,
                placeholder,
                reply,
            } => {
                let mut overlay = crate::overlay::InputOverlay::new(title);
                if let Some(hint) = placeholder {
                    overlay = overlay.with_placeholder(hint);
                }
                self.plugin_overlay = Some(Box::new(overlay));
                self.active_dialog = Some(PluginDialogState::Input { reply });
            }
            PluginDialog::Editor {
                title,
                prefill,
                reply,
            } => {
                let mut overlay = crate::overlay::EditorOverlay::new(title);
                if let Some(text) = prefill {
                    overlay = overlay.with_prefill(text);
                }
                self.plugin_overlay = Some(Box::new(overlay));
                self.active_dialog = Some(PluginDialogState::Editor { reply });
            }
        }
    }

    /// Refresh the session-list snapshot read by `kage.session.list`.
    /// Builds `[{id, value}]` entries from the registered
    /// [`SessionLister`]; called once per redraw.
    fn refresh_plugin_session_list(&mut self) {
        let Some(slot) = self.plugin_session_list.as_ref() else {
            return;
        };
        let Some(lister) = self.session_lister.as_ref() else {
            return;
        };
        let items = lister();
        let entries: Vec<serde_json::Value> = items
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.label,
                    "value": p.value,
                })
            })
            .collect();
        if let Ok(mut s) = slot.lock() {
            *s = entries;
        }
    }

    /// Drive the event loop until the user quits. Returns the exit
    /// reason. The caller is expected to drop the [`Tui`] (which
    /// restores the terminal) before printing anything to stdout.
    pub fn run(&mut self, tui: &mut Tui) -> Result<AppExit, TuiError> {
        // First frame is unconditional - we always paint once before
        // entering the steady-state event loop.
        let mut last_buffer_version = self.buffer_version();
        let mut needs_redraw = true;
        loop {
            if let Some(enable) = self.pending_mouse_capture.take() {
                tui.set_mouse_capture(enable);
            }
            self.drain_plugin_compact_request();
            self.drain_plugin_fork_request();
            self.drain_plugin_dialog();
            self.drain_plugin_theme();
            self.refresh_plugin_session_list();
            if needs_redraw {
                self.draw(tui)?;
                last_buffer_version = self.buffer_version();
                needs_redraw = false;
            }
            // Wake periodically to repaint streaming tool-call
            // timers ("running 1.2s") and to pick up worker-thread
            // mutations that race ahead of any input event. While
            // the agent is mid-turn we shorten the wake interval to
            // ~one spinner frame so the modeline tick stays smooth
            // even with no streaming deltas (e.g. waiting on a slow
            // first token from the provider).
            let tick = if self.is_working() || self.has_running_tool_call() {
                // 50ms keeps the spinner smooth and shaves the worst-
                // case redraw lag after `working` flips false (e.g.
                // after a cancel takes effect) so the user perceives
                // the spinner stopping as effectively instant rather
                // than tail-end-of-the-100ms-window.
                Duration::from_millis(50)
            } else {
                Duration::from_secs(1)
            };
            let mut deadline = Instant::now() + tick;
            // Toasts auto-expire on a wall-clock schedule independent
            // of key input; cap the poll deadline at the next toast
            // expiration and force a redraw each tick so the overlay
            // appears immediately when pushed from a worker thread
            // and disappears when its deadline fires, regardless of
            // whether the user pressed a key.
            if let Some(toast_deadline) = self.next_toast_deadline() {
                if toast_deadline < deadline {
                    deadline = toast_deadline;
                }
                needs_redraw = true;
            }
            while Instant::now() < deadline {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or_default();
                if event::poll(remaining)? {
                    // Any handled event might have altered something
                    // user-visible (cursor in picker, mode switch,
                    // input edit, scroll). Tracking each potential
                    // change site is brittle; mark for redraw and
                    // let the next iteration paint.
                    needs_redraw = true;
                    match event::read()? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            log_key_event(&key);
                            if let Some(exit) = self.dispatch_key(key) {
                                if let Some(state) = self.active_dialog.take() {
                                    let _ = state.reply().send(None);
                                }
                                return Ok(exit);
                            }
                        }
                        Event::Paste(text) => self.input.paste(&text),
                        Event::Mouse(mouse) => match mouse.kind {
                            MouseEventKind::ScrollUp => self.scroll_by(-MOUSE_SCROLL_LINES),
                            MouseEventKind::ScrollDown => self.scroll_by(MOUSE_SCROLL_LINES),
                            MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left) => {
                                self.mouse_down(mouse.row, mouse.column);
                            }
                            MouseEventKind::Drag(ratatui::crossterm::event::MouseButton::Left) => {
                                self.mouse_drag(mouse.row, mouse.column);
                            }
                            MouseEventKind::Up(ratatui::crossterm::event::MouseButton::Left) => {
                                self.mouse_up(mouse.row);
                            }
                            _ => {}
                        },
                        Event::Resize(_, _) => {
                            // Width changed; every cached height is
                            // measured against the prior width and is
                            // now stale.
                            if let Ok(mut buf) = self.buffer.lock() {
                                buf.invalidate_all_heights();
                            }
                        }
                        _ => {}
                    }
                    break;
                }
                // No event arrived; check the worker thread for
                // buffer mutations (streaming deltas, tool results)
                // and break out to repaint if the version moved.
                let v = self.buffer_version();
                if v != last_buffer_version {
                    needs_redraw = true;
                    break;
                }
            }
            // Periodic-wake fallthrough: if a streaming tool call is
            // in-flight, repaint anyway so the elapsed-time pill
            // ticks visibly. Cheap because the renderer's height
            // cache is hot at this point. Same for the modeline
            // spinner while the agent is mid-turn.
            if !needs_redraw && (self.has_running_tool_call() || self.is_working()) {
                needs_redraw = true;
            }
        }
    }

    /// True when the worker has marked the [`crate::usage::SessionUsage`]
    /// snapshot as `working`. The render path uses it to drive the
    /// modeline spinner; the event loop uses it to force periodic
    /// redraws so the spinner animates.
    fn is_working(&self) -> bool {
        self.session_usage
            .as_ref()
            .and_then(|h| h.lock().ok().map(|g| g.working))
            .unwrap_or(false)
    }

    /// Read the buffer's current mutation counter without holding
    /// the lock across the rest of the loop.
    fn buffer_version(&self) -> u64 {
        self.buffer.lock().map_or(0, |b| b.version())
    }

    /// True when there's at least one in-flight tool call (a
    /// `ToolCall` block whose matching `ToolResult` hasn't arrived).
    /// The renderer paints "running Xs" for these and we want it to
    /// tick even on an otherwise idle event loop.
    fn has_running_tool_call(&self) -> bool {
        let Ok(buf) = self.buffer.lock() else {
            return false;
        };
        let blocks = buf.blocks();
        let mut pending: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for b in blocks {
            if let crate::buffer::Block::ToolCall { call_id, .. } = b {
                pending.insert(call_id.as_str());
            }
        }
        for b in blocks {
            if let crate::buffer::Block::ToolResult { call_id, .. } = b {
                pending.remove(call_id.as_str());
            }
        }
        !pending.is_empty()
    }

    /// Emit a DECSCUSR cursor-shape escape if the desired shape for
    /// the current mode + pane focus differs from the last shape we
    /// emitted. Reapplying the same shape every frame causes some
    /// terminals (kitty, mlterm) to flicker the cursor briefly.
    fn sync_cursor_style(&mut self) {
        use ratatui::crossterm::cursor::SetCursorStyle;
        let pane_focused = self.input.focused_pane() == Pane::Input;
        let key = (self.input.mode(), pane_focused);
        if self.last_cursor_style == Some(key) {
            return;
        }
        // Buffer pane focused: cursor is hidden in the input card;
        // fall back to the user's shell-default shape so anywhere
        // ratatui happens to paint a cursor matches ambient style.
        // Visual + Input pane keeps the input cursor hidden during
        // buffer-cell visual selection, but we leave the shape as
        // Block so the next mode change starts from a sensible
        // default.
        let style = match key {
            (Mode::Insert, true) => SetCursorStyle::SteadyBar,
            (Mode::Normal | Mode::Visual, true) => SetCursorStyle::SteadyBlock,
            (_, false) => SetCursorStyle::DefaultUserShape,
        };
        let _ = ratatui::crossterm::execute!(std::io::stdout(), style);
        self.last_cursor_style = Some(key);
    }

    fn draw(&mut self, tui: &mut Tui) -> Result<(), TuiError> {
        self.sync_cursor_style();
        // compute_search_match_count locks self.buffer internally; do
        // it BEFORE we hold the lock or we'll deadlock the moment a
        // search is active.
        let search_match_count = self.compute_search_match_count();
        let render_width = tui.terminal().size().map_or(80, |r| r.width);
        self.refresh_plugin_widget_texts(render_width);
        let mut buffer = self.buffer.lock().expect("buffer mutex poisoned");
        let cmdline = self.cmdline.as_ref();
        let model_snapshot = self
            .status_model
            .as_ref()
            .and_then(|m| m.lock().ok().map(|g| g.clone()));
        let status = view::StatusCtx {
            model: model_snapshot.as_deref(),
            session_id: self.status_session_id.as_deref(),
            search_pattern: self.search_pattern.as_deref(),
            search_line: self.search_line.as_ref(),
            search_match_count,
            plugin_widgets: &self.plugin_widget_texts,
            plugin_status: &self.plugin_status_cache,
            plugin_header: &self.plugin_header_lines,
            plugin_footer: &self.plugin_footer_lines,
        };
        let screen_selection = self.screen_selection;
        let mut captured_rows = std::mem::take(&mut self.captured_rows);
        let session_usage = self.session_usage_snapshot();
        let live_toasts = self.live_toasts();
        let bottom = if self.modeline_visible() {
            crate::layout::STATUS_BOTTOM_LINES_DEFAULT
        } else {
            0
        };
        // The autocomplete popup yields to every modal layer; it only
        // paints during plain input editing.
        let show_completion = self.input_completion.is_some()
            && self.slash_palette.is_none()
            && self.cmdline.is_none()
            && self.search_line.is_none()
            && self.picker.is_none()
            && self.settings_overlay.is_none()
            && self.session_tree.is_none()
            && self.plugin_overlay.is_none();
        let picker = self.picker.as_mut();
        let settings_overlay = self.settings_overlay.as_mut();
        let session_tree = self.session_tree.as_mut();
        let plugin_overlay = self.plugin_overlay.as_mut();
        let slash_palette = self.slash_palette.as_ref();
        let input_completion = if show_completion {
            self.input_completion.as_ref()
        } else {
            None
        };
        let input = &self.input;
        tui.terminal().draw(|frame| {
            // Compute the input region size from the *visual* row
            // count after wrap, not the logical `\n` count, so a
            // long single line that overflows the body width grows
            // the input card instead of being silently clipped.
            let body_width = frame
                .area()
                .width
                .saturating_sub(2 + view::INPUT_GLYPH_WIDTH);
            let input_visual_lines = view::input_visual_row_count(input.text(), body_width);
            let input_height = input_height_for(input_visual_lines);
            let regions = split(frame.area(), input_height, bottom);
            view::render(
                frame,
                regions,
                &mut buffer,
                input,
                cmdline,
                &status,
                screen_selection,
                &mut captured_rows,
                session_usage.as_ref(),
                &live_toasts,
            );
            if let Some(picker) = picker {
                picker.render(frame, frame.area());
            }
            if let Some(settings) = settings_overlay {
                settings.render(frame, frame.area());
            }
            if let Some(tree) = session_tree {
                tree.render(frame, frame.area());
            }
            if let Some(palette) = slash_palette {
                palette.render(frame, regions);
                palette.place_cursor(frame, regions);
            }
            if let Some(completion) = input_completion {
                completion.render(frame, regions);
            }
            if let Some(overlay) = plugin_overlay {
                let modal = overlay.measure(frame.area());
                frame.render_widget(crate::opaque::OpaqueClear, modal);
                let theme = crate::theme::current();
                let ctx = crate::overlay::OverlayCtx {
                    theme: &theme,
                    viewport: frame.area(),
                };
                overlay.render(modal, frame.buffer_mut(), &ctx);
            }
        })?;
        self.captured_rows = captured_rows;
        Ok(())
    }

    fn dispatch_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<AppExit> {
        // Global escape hatches before passing to any modal layer.
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('q')) {
            return Some(AppExit::Quit);
        }

        // Raw plugin terminal-input hooks see the key before any modal
        // layer (but never before the ctrl+q hatch above, so a hook
        // cannot wedge the UI). A truthy return consumes it.
        if let Some(hooks) = self.terminal_hooks.as_ref() {
            let snapshot = hooks.lock().map(|h| h.clone()).unwrap_or_default();
            if !snapshot.is_empty() {
                let descriptor = key_event_to_json(key);
                if snapshot.iter().any(|hook| hook.handle(&descriptor)) {
                    return None;
                }
            }
        }

        // A blocking plugin dialog is the top-most modal layer: the
        // worker is parked waiting for its answer.
        if self.plugin_overlay.is_some() {
            return self.dispatch_plugin_overlay_key(key);
        }

        // When the picker overlay is open, it owns the keyboard.
        if self.picker.is_some() {
            return self.dispatch_picker_key(key);
        }

        // The settings dialog is a modal sibling of the picker.
        if self.settings_overlay.is_some() {
            return self.dispatch_settings_key(key);
        }

        // The `:tree` session browser is also a modal sibling.
        if self.session_tree.is_some() {
            return self.dispatch_session_tree_key(key);
        }

        // The slash palette is its own modal layer, taking precedence
        // over the cmdline and search line.
        if self.slash_palette.is_some() {
            return self.dispatch_slash_palette_key(key);
        }

        // The `:` command line is the next-most-modal layer.
        if self.cmdline.is_some() {
            return self.dispatch_cmdline_key(key);
        }

        // The `/` search line is also modal while open.
        if self.search_line.is_some() {
            return self.dispatch_search_key(key);
        }

        // Plugin keybindings win over builtin Normal/Insert handling
        // (last writer wins), but never over an open modal layer
        // above or the global quit hatch.
        if let Some(chord) = self
            .plugin_keybindings
            .iter()
            .find(|(matcher, _)| matcher.matches(&key))
            .map(|(_, chord)| chord.clone())
        {
            let _ = self.send_request(RunRequest::InvokePluginKeybinding { chord });
            return None;
        }

        // The autocomplete popup is non-modal: it only consumes its
        // own navigation/accept/dismiss keys. Anything else falls
        // through to normal editing and then re-queries the stack.
        if self.input_completion.is_some() {
            let action = self
                .input_completion
                .as_mut()
                .expect("input completion present")
                .handle_key(key);
            match action {
                CompletionAction::Navigated => return None,
                CompletionAction::Dismissed => {
                    self.input_completion = None;
                    return None;
                }
                CompletionAction::Accepted(item) => {
                    self.accept_completion(&item);
                    return None;
                }
                CompletionAction::PassThrough => {}
            }
        }

        let actions = self.input.handle_key(key);
        for action in actions {
            if let Some(exit) = self.apply(action) {
                return Some(exit);
            }
        }
        self.refresh_input_completion();
        None
    }

    /// Re-query the autocomplete provider stack from the current
    /// prompt text and rebuild the popup. A no-op (and closes any open
    /// popup) unless plugins registered providers and the user is
    /// actively typing in the input pane.
    fn refresh_input_completion(&mut self) {
        let has_sources =
            !self.autocomplete_providers.is_empty() || self.completion_workdir.is_some();
        if !has_sources
            || self.input.focused_pane() != Pane::Input
            || self.input.mode() != Mode::Insert
        {
            self.input_completion = None;
            return;
        }
        let text = self.input.text();
        let cursor = self.input.cursor();
        let prefix = prefix_before_cursor(text, cursor);
        let mut items = Vec::new();
        for provider in self.autocomplete_providers.iter().rev() {
            let got = provider.complete(prefix, text, cursor);
            if !got.is_empty() {
                items = got;
                break;
            }
        }
        if items.is_empty()
            && let Some(workdir) = self.completion_workdir.as_deref()
        {
            items = file_completions(workdir, prefix, cursor);
        }
        self.input_completion = InputCompletion::new(items);
    }

    /// Splice an accepted candidate into the input. Uses the item's
    /// explicit `range` when present, otherwise replaces the prefix
    /// span the host computed. Re-queries afterward so a provider can
    /// offer a follow-up (e.g. path segments).
    fn accept_completion(&mut self, item: &kage_plugin::AutocompleteItem) {
        let cursor = self.input.cursor();
        let (start, end) = if let Some((from, to)) = item.range {
            (from, to)
        } else {
            let plen = prefix_before_cursor(self.input.text(), cursor).len();
            (cursor.saturating_sub(plen), cursor)
        };
        self.input.splice(start, end, &item.value);
        self.input_completion = None;
        self.refresh_input_completion();
    }

    fn dispatch_search_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<AppExit> {
        let line = self.search_line.as_mut()?;
        match line.handle_key(key, &[], &EmptyResolver) {
            CommandLineEvent::Pending => None,
            CommandLineEvent::Cancelled => {
                self.search_line = None;
                None
            }
            CommandLineEvent::Submit(text) => {
                self.search_line = None;
                self.search_pattern = Some(text);
                self.jump_to_search_match(true);
                None
            }
        }
    }

    /// Build `(current_1_indexed, total)` for the right-edge match
    /// counter, or `None` when no search is active.
    fn compute_search_match_count(&self) -> Option<(usize, usize)> {
        let pattern = self.search_pattern.as_deref()?;
        let buf = self.buffer.lock().ok()?;
        let matches = buf.match_indices(pattern);
        let focus = buf.effective_focus().unwrap_or(usize::MAX);
        let current = matches
            .iter()
            .position(|i| *i == focus)
            .map_or(0, |p| p + 1);
        Some((current, matches.len()))
    }

    /// Jump focus to the next or previous block whose content matches
    /// the active search pattern. No-op when no pattern is set.
    fn jump_to_search_match(&mut self, forward: bool) {
        let Some(pattern) = self.search_pattern.clone() else {
            return;
        };
        if let Ok(mut buf) = self.buffer.lock() {
            let from = buf.effective_focus().unwrap_or(0);
            let next = if forward {
                buf.next_match(from, &pattern)
            } else {
                buf.prev_match(from, &pattern)
            };
            if let Some(n) = next {
                buf.set_focus(Some(n));
            }
        }
    }

    fn dispatch_cmdline_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let registry = cmdline_registry(&self.plugin_command_specs);
        let resolver = AppResolver {
            models: &self.model_choices,
            plugin_commands: &self.plugin_commands,
            sessions: self.session_lister.as_ref(),
            themes_dir: self.themes_dir.as_deref(),
        };
        let event = self
            .cmdline
            .as_mut()
            .map(|cl| cl.handle_key(key, &registry, &resolver));
        let event = event?;
        match event {
            CommandLineEvent::Pending => None,
            CommandLineEvent::Cancelled => {
                self.cmdline = None;
                None
            }
            CommandLineEvent::Submit(text) => {
                let result = self.run_command_validated(&text, &registry);
                match result {
                    CommandResult::Done(exit) => {
                        self.cmdline = None;
                        exit
                    }
                    CommandResult::ValidationError(msg) => {
                        if let Some(cl) = self.cmdline.as_mut() {
                            cl.set_error(msg);
                        }
                        None
                    }
                }
            }
        }
    }

    fn dispatch_slash_palette_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let action = self
            .slash_palette
            .as_mut()
            .map(|sp| crate::overlay::OverlayWidget::handle_key(sp, key))?;
        match action {
            OverlayAction::Stay | OverlayAction::PropagateKey => None,
            OverlayAction::Close => {
                self.slash_palette = None;
                None
            }
            OverlayAction::Resolve(value) => {
                let serde_json::Value::String(text) = value else {
                    self.slash_palette = None;
                    return None;
                };
                let registry = cmdline_registry(&self.plugin_command_specs);
                let result = self.run_command_validated(&text, &registry);
                match result {
                    CommandResult::Done(exit) => {
                        self.slash_palette = None;
                        exit
                    }
                    CommandResult::ValidationError(msg) => {
                        if let Some(sp) = self.slash_palette.as_mut() {
                            sp.set_error(msg);
                        }
                        None
                    }
                }
            }
        }
    }

    /// Validated command dispatch. Parses the argument string against
    /// the matched spec's schema and returns a [`CommandResult`]
    /// instead of pushing errors to the buffer. On
    /// [`CommandResult::ValidationError`], the caller keeps the
    /// cmdline open so the user can fix the input.
    fn run_command_validated(&mut self, line: &str, registry: &[&CommandSpec]) -> CommandResult {
        let mut parts = line.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();

        if head.is_empty() {
            return CommandResult::Done(None);
        }

        if let Some(spec) = crate::command::find_builtin_command(head) {
            let (target_spec, target_rest) = Self::resolve_subcommand_tree(spec, rest);
            if let Err(e) = crate::cmdparse::parse_input(target_spec, target_rest) {
                return CommandResult::ValidationError(e.to_string());
            }
            let exit = self.dispatch_builtin(spec.name, rest);
            return CommandResult::Done(exit);
        }

        if self.plugin_commands.iter().any(|(n, _)| n == head) {
            let _ = self.send_request(RunRequest::InvokePluginCommand {
                name: head.to_owned(),
                args: rest.to_owned(),
            });
            return CommandResult::Done(None);
        }

        let mut msg = format!("unknown command: {head}");
        if let Some(suggestion) = crate::cmdparse::suggest_command(registry, head) {
            msg = format!("{msg} (did you mean :{suggestion}?)");
        }
        CommandResult::ValidationError(msg)
    }

    /// Walk the subcommand tree for commands like `theme set <name>`.
    /// Returns the leaf spec and the remaining argument substring
    /// after consuming subcommand names. If no subcommand matches,
    /// returns the parent spec with the full `rest`.
    fn resolve_subcommand_tree<'a, 'b>(
        spec: &'a CommandSpec,
        rest: &'b str,
    ) -> (&'a CommandSpec, &'b str) {
        if spec.subcommands.is_empty() {
            return (spec, rest);
        }
        let mut parts = rest.splitn(2, char::is_whitespace);
        let first = parts.next().unwrap_or("");
        if let Some(sub) = spec.subcommand(first) {
            let sub_rest = parts.next().unwrap_or("").trim();
            return Self::resolve_subcommand_tree(sub, sub_rest);
        }
        (spec, rest)
    }

    /// Execute a built-in command by canonical name with the
    /// remaining unparsed argument string. The match is on the
    /// primary name; aliases were already resolved by
    /// [`Self::run_command`].
    fn dispatch_builtin(&mut self, name: &str, rest: &str) -> Option<AppExit> {
        match name {
            "quit" => Some(AppExit::Quit),
            "cancel" => {
                self.trip_cancel();
                None
            }
            "model" => {
                if rest.is_empty() {
                    self.push_error("model: usage `:model <provider:id>`");
                } else {
                    let _ = self.send_request(RunRequest::SwitchModel(rest.to_owned()));
                }
                None
            }
            "fold" => {
                if rest == "all" {
                    self.set_all_folds(true);
                } else {
                    self.push_error("fold: usage `:fold all`");
                }
                None
            }
            "unfold" => {
                if rest == "all" {
                    self.set_all_folds(false);
                } else {
                    self.push_error("unfold: usage `:unfold all`");
                }
                None
            }
            "theme" => {
                self.run_theme_command(rest);
                None
            }
            "mouse" => {
                self.run_mouse_command(rest);
                None
            }
            "help" => {
                self.push_help();
                None
            }
            "compact" => {
                let _ = self.send_request(RunRequest::CompactNow);
                None
            }
            "settings" => {
                self.open_settings();
                None
            }
            "tree" => {
                self.open_session_tree();
                None
            }
            "clone" => {
                let _ = self.send_request(RunRequest::CloneSession);
                None
            }
            "new" => {
                let _ = self.send_request(RunRequest::NewSession);
                None
            }
            "export" => {
                let dest = match rest.trim() {
                    "" => None,
                    path => Some(std::path::PathBuf::from(path)),
                };
                let _ = self.send_request(RunRequest::ExportSession(dest));
                None
            }
            "clear" => {
                if let Ok(mut buf) = self.buffer.lock() {
                    buf.clear();
                }
                None
            }
            _ => None,
        }
    }

    fn push_error(&mut self, msg: impl Into<String>) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:error", msg, false);
        }
    }

    /// Copy the active screen selection to the system clipboard via
    /// OSC52. Walks every captured row in the selection range,
    /// strips renderer-only decoration glyphs (rule chars), trims
    /// trailing whitespace per row, joins with `\n`, and clears the
    /// selection.
    fn yank_screen_selection(&mut self) {
        let text = self.extract_selection_text();
        if text.is_empty() {
            self.clear_selection();
            return;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&text);
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
        let _ = stdout.flush();
        self.clear_selection();
        self.notify(format!(
            "yanked {} chars to clipboard",
            text.chars().count()
        ));
    }

    fn clear_selection(&mut self) {
        self.screen_selection = None;
        self.captured_rows.clear();
    }

    /// Anchor a keyboard selection at the focused block's first
    /// visible row (or the viewport top if nothing's focused) and
    /// switch the mode to [`Mode::Visual`]. Subsequent
    /// [`InputAction::Visual*`] events move the cursor end.
    fn enter_visual_mode(&mut self) {
        let anchor = if let Ok(buf) = self.buffer.lock() {
            let area_x = buf.last_area_x();
            let area_y = buf.last_area_y();
            let virtual_top = buf.last_virtual_top();
            let row = buf
                .effective_focus()
                .and_then(|idx| buf.screen_top_of(idx))
                .unwrap_or(area_y);
            let vrow = virtual_top.saturating_add(usize::from(row.saturating_sub(area_y)));
            (vrow, area_x)
        } else {
            (0, 0)
        };
        self.captured_rows.clear();
        self.screen_selection = Some((anchor, anchor));
        self.input.switch_mode(Mode::Visual);
    }

    fn move_visual_cursor(&mut self, dvrow: i32, dcol: i32) {
        let Some((anchor, cursor)) = self.screen_selection else {
            return;
        };
        let (mut vrow, mut col) = cursor;
        if dvrow != 0 {
            let next = i64::try_from(vrow).unwrap_or(i64::MAX) + i64::from(dvrow);
            vrow = usize::try_from(next.max(0)).unwrap_or(0);
        }
        if dcol != 0 {
            let next = i32::from(col).saturating_add(dcol).max(0);
            col = u16::try_from(next).unwrap_or(u16::MAX);
        }
        self.screen_selection = Some((anchor, (vrow, col)));
        self.scroll_visual_cursor_into_view(vrow);
    }

    fn snap_visual_cursor_x(&mut self, target_col: i32) {
        let Some((anchor, cursor)) = self.screen_selection else {
            return;
        };
        let (vrow, _) = cursor;
        let col = if target_col <= 0 {
            0
        } else if let Ok(buf) = self.buffer.lock() {
            buf.last_area_width().saturating_sub(1)
        } else {
            0
        };
        self.screen_selection = Some((anchor, (vrow, col)));
    }

    /// Keep the visual cursor on screen by adjusting buffer scroll.
    /// Cursor above the viewport top scrolls up; below the bottom
    /// scrolls down. Otherwise no-op.
    fn scroll_visual_cursor_into_view(&mut self, cursor_vrow: usize) {
        if let Ok(mut buf) = self.buffer.lock() {
            let area_height = usize::from(buf.last_area_height());
            if area_height == 0 {
                return;
            }
            let visible_top = buf.last_virtual_top();
            let visible_bot = visible_top.saturating_add(area_height);
            let current_scroll = buf.scroll();
            if cursor_vrow < visible_top {
                let delta = visible_top - cursor_vrow;
                buf.set_scroll(current_scroll.saturating_add(delta));
            } else if cursor_vrow >= visible_bot {
                let delta = cursor_vrow + 1 - visible_bot;
                buf.set_scroll(current_scroll.saturating_sub(delta));
            }
        }
    }

    /// Yank the entire content of the currently focused block by
    /// projecting its screen rows onto captured cells. Limitation:
    /// only rows that have been visible (and thus captured) since
    /// the last selection clear contribute text - tall blocks the
    /// user hasn't scrolled fully through return only the visible
    /// portion. Auto-scroll on entering visual covers the keyboard
    /// path; for `Y` we just use whatever cells we have right now.
    fn yank_focused_block(&mut self) {
        // Force a fresh capture by walking the focused block's
        // screen rows (currently visible) and pulling text from
        // captured_rows.
        let Ok(buf) = self.buffer.lock() else {
            return;
        };
        let Some(idx) = buf.effective_focus() else {
            return;
        };
        let virtual_top = buf.last_virtual_top();
        let area_y = buf.last_area_y();
        let Some((top, bot)) = buf.screen_rows_of(idx) else {
            return;
        };
        drop(buf);
        let mut text = String::new();
        for screen_row in top..bot {
            let vrow = virtual_top.saturating_add(usize::from(screen_row.saturating_sub(area_y)));
            let Some(cells) = self.captured_rows.get(&vrow) else {
                continue;
            };
            let line: String = cells
                .iter()
                .filter(|cell| !cell.decoration)
                .map(|cell| cell.ch)
                .collect();
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(line.trim_end());
        }
        if text.is_empty() {
            return;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&text);
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
        let _ = stdout.flush();
        self.notify(format!(
            "yanked {} chars to clipboard",
            text.chars().count()
        ));
    }

    fn extract_selection_text(&self) -> String {
        let Some((anchor, cursor)) = self.screen_selection else {
            return String::new();
        };
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        let mut out = String::new();
        for vrow in start.0..=end.0 {
            let Some(grid_row) = self.captured_rows.get(&vrow) else {
                if !out.is_empty() {
                    out.push('\n');
                }
                continue;
            };
            let from_col = if vrow == start.0 {
                usize::from(start.1)
            } else {
                0
            };
            let to_col = if vrow == end.0 {
                usize::from(end.1).saturating_add(1)
            } else {
                grid_row.len()
            };
            let to_col = to_col.min(grid_row.len());
            if from_col >= to_col {
                if !out.is_empty() {
                    out.push('\n');
                }
                continue;
            }
            let slice: String = grid_row[from_col..to_col]
                .iter()
                .filter(|cell| !cell.decoration)
                .map(|cell| cell.ch)
                .collect();
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(slice.trim_end());
        }
        out
    }

    fn push_help(&mut self) {
        let mut lines = vec!["available commands:".to_owned()];
        for spec in crate::command::BUILTIN_COMMANDS {
            help_render_spec(&mut lines, spec, ":", 0);
        }
        let body = lines.join("\n");
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push_custom("kage:help", body, false);
        }
    }

    fn run_theme_command(&mut self, rest: &str) {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("");
        let sub_rest = parts.next().unwrap_or("").trim();
        match sub {
            "" | "current" => {
                let cur = crate::theme::current().name;
                self.notify(format!("theme: {cur} (try `:theme list`)"));
            }
            "list" => {
                let cur = crate::theme::current().name;
                let names = crate::theme::Theme::available_names(self.themes_dir.as_deref())
                    .iter()
                    .map(|n| {
                        if *n == cur {
                            format!("* {n}")
                        } else {
                            format!("  {n}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Ok(mut buf) = self.buffer.lock() {
                    buf.push_custom("kage:theme", format!("themes:\n{names}"), false);
                }
            }
            "set" => {
                if sub_rest.is_empty() {
                    self.push_error("theme set: usage `:theme set <name>`");
                    return;
                }
                self.apply_theme_by_name(sub_rest);
            }
            other => {
                self.push_error(format!(
                    "theme: unknown subcommand `{other}` (try list, set, current)"
                ));
            }
        }
    }

    fn apply_theme_by_name(&mut self, name: &str) {
        self.apply_theme_resolved(name, true);
    }

    /// Resolve `name` against the bundled set and the user theme
    /// directory, then make it the active palette. `announce` toasts
    /// the switch (`:theme set`, settings, plugin); startup passes
    /// `false`. Resolution failures (unknown name, unreadable file,
    /// bad TOML) surface inline rather than failing silently.
    fn apply_theme_resolved(&mut self, name: &str, announce: bool) {
        let theme = match crate::theme::Theme::resolve(name, self.themes_dir.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                self.push_error(format!("theme: {e}"));
                return;
            }
        };
        crate::theme::set_current(theme);
        if let Ok(mut buf) = self.buffer.lock() {
            // Force a fresh layout pass: every block's cached height
            // was measured against the prior theme's bubble
            // background, which doesn't change geometry but
            // invalidating is cheap and protects against future
            // theme-driven height tweaks (different rule glyph
            // widths, etc.).
            buf.invalidate_all_heights();
        }
        if announce {
            self.notify(format!("theme: {name}"));
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        let Some(toasts) = &self.toasts else {
            return;
        };
        toast::push_toast(
            toasts,
            Toast::with_kind(msg, ToastKind::Info, toast::DEFAULT_TOAST_DURATION),
        );
    }

    fn run_mouse_command(&mut self, rest: &str) {
        match rest {
            "off" | "disable" => {
                self.pending_mouse_capture = Some(false);
                self.notify("mouse capture off - drag selects via the terminal's native clipboard");
            }
            "on" | "enable" => {
                self.pending_mouse_capture = Some(true);
                self.notify("mouse capture on - drag selects blocks inside kage");
            }
            "toggle" | "" => {
                let now_enabled = !self.pending_mouse_capture.unwrap_or(true);
                self.pending_mouse_capture = Some(now_enabled);
                let state = if now_enabled { "on" } else { "off" };
                self.notify(format!("mouse capture {state}"));
            }
            other => {
                self.push_error(format!("mouse: unknown arg `{other}` (try on/off/toggle)"));
            }
        }
    }

    fn dispatch_picker_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<AppExit> {
        let picker = self.picker.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(picker, key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.picker = None;
                self.picker_kind = None;
            }
            OverlayAction::Resolve(value) => {
                let kind = self.picker_kind;
                self.picker = None;
                self.picker_kind = None;
                let serde_json::Value::String(value) = value else {
                    return None;
                };
                match kind {
                    Some(PickerKind::Model) => {
                        let _ = self.send_request(RunRequest::SwitchModel(value));
                    }
                    Some(PickerKind::Session) => {
                        let _ = self.send_request(RunRequest::ResumeSession(
                            std::path::PathBuf::from(value),
                        ));
                    }
                    None => {}
                }
            }
        }
        None
    }

    fn dispatch_settings_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.settings_overlay.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay, key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.settings_overlay = None;
            }
            OverlayAction::Resolve(value) => {
                self.settings_overlay = None;
                self.apply_settings(&value);
            }
        }
        None
    }

    /// Open the `:settings` dialog, seeding it from the loaded
    /// user/project config plus live state (active theme/model).
    fn open_settings(&mut self) {
        let workdir = self
            .completion_workdir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let cfg = match kage_core::config::Config::load_layered(&workdir) {
            Ok(c) => c,
            Err(e) => {
                self.push_error(format!("settings: config load failed: {e}"));
                return;
            }
        };
        let model = self
            .status_model
            .as_ref()
            .and_then(|m| m.lock().ok().map(|g| g.clone()))
            .unwrap_or_else(|| cfg.provider.default_model.clone());
        let init = SettingsInit {
            themes: crate::theme::Theme::available_names(self.themes_dir.as_deref()),
            theme: crate::theme::current().name,
            models: self.model_choices.iter().map(|p| p.value.clone()).collect(),
            model,
            mouse: self.pending_mouse_capture.unwrap_or(cfg.ui.mouse),
            threshold: cfg.loop_settings.compaction_threshold,
            keybindings: cfg
                .keybindings
                .bindings
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            editor_modeless: matches!(cfg.ui.editor, kage_core::config::EditorMode::Modeless),
        };
        self.settings_overlay = Some(SettingsOverlay::new(init));
    }

    /// Apply the settings-dialog result: live-switch theme / mouse /
    /// model, then persist the four fields to the user config file
    /// (comment-preserving). A persistence failure is surfaced, not
    /// swallowed.
    fn apply_settings(&mut self, value: &serde_json::Value) {
        let theme = value.get("theme").and_then(|v| v.as_str()).unwrap_or("");
        let model = value.get("model").and_then(|v| v.as_str()).unwrap_or("");
        let mouse = value.get("mouse").and_then(serde_json::Value::as_bool);
        let threshold = value
            .get("compaction_threshold")
            .and_then(serde_json::Value::as_f64);
        // `None` when the key is absent or unrecognized; only an
        // explicit "modeless"/"vim" changes anything.
        let editor_modeless = match value.get("editor").and_then(|v| v.as_str()) {
            Some("modeless") => Some(true),
            Some("vim") => Some(false),
            _ => None,
        };

        if !theme.is_empty() && theme != crate::theme::current().name {
            self.apply_theme_by_name(theme);
        }
        if let Some(mouse) = mouse {
            self.pending_mouse_capture = Some(mouse);
        }
        if let Some(modeless) = editor_modeless {
            // Live-apply: the input editor flips immediately.
            self.input.set_modeless(modeless);
        }
        let current_model = self
            .status_model
            .as_ref()
            .and_then(|m| m.lock().ok().map(|g| g.clone()));
        if !model.is_empty() && current_model.as_deref() != Some(model) {
            let _ = self.send_request(RunRequest::SwitchModel(model.to_owned()));
        }

        let Some(path) = kage_core::config::Config::default_path() else {
            self.push_error("settings: no home directory; not persisted");
            return;
        };
        let mut cfg = match kage_core::config::Config::load(&path) {
            Ok(c) => c,
            Err(e) => {
                self.push_error(format!("settings: config load failed: {e}"));
                return;
            }
        };
        if !theme.is_empty() {
            theme.clone_into(&mut cfg.ui.theme);
        }
        if !model.is_empty() {
            model.clone_into(&mut cfg.provider.default_model);
        }
        if let Some(mouse) = mouse {
            cfg.ui.mouse = mouse;
        }
        if let Some(t) = threshold {
            #[allow(clippy::cast_possible_truncation)]
            {
                cfg.loop_settings.compaction_threshold = t as f32;
            }
        }
        if let Some(modeless) = editor_modeless {
            cfg.ui.editor = if modeless {
                kage_core::config::EditorMode::Modeless
            } else {
                kage_core::config::EditorMode::Vim
            };
        }
        match cfg.save(&path) {
            Ok(()) => self.notify("settings saved"),
            Err(e) => self.push_error(format!("settings: save failed: {e}")),
        }
    }

    /// Open the `:tree` session browser, querying the wired source.
    fn open_session_tree(&mut self) {
        let Some(source) = self.session_tree_source.as_ref() else {
            self.push_error("tree: session browser unavailable");
            return;
        };
        let nodes = source();
        if nodes.is_empty() {
            self.notify("no sessions to browse yet");
            return;
        }
        self.session_tree = Some(SessionTreeOverlay::new(nodes));
    }

    fn dispatch_session_tree_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.session_tree.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay, key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.session_tree = None;
            }
            OverlayAction::Resolve(value) => {
                self.session_tree = None;
                let action = value.get("action").and_then(|v| v.as_str()).unwrap_or("");
                let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    return None;
                }
                let path = std::path::PathBuf::from(path);
                match action {
                    "resume" => {
                        let _ = self.send_request(RunRequest::ResumeSession(path));
                    }
                    "fork" => {
                        let _ = self.send_request(RunRequest::ForkSessionFile(path));
                    }
                    "delete" => {
                        let _ = self.send_request(RunRequest::DeleteSession(path));
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Drive the active plugin dialog overlay (`kage.ui.*`). The
    /// overlay owns its keys; on resolve/close the chosen value is
    /// sent back to the parked worker through [`Self::active_dialog`],
    /// mapped per the dialog kind, then the overlay is dismissed.
    fn dispatch_plugin_overlay_key(
        &mut self,
        key: ratatui::crossterm::event::KeyEvent,
    ) -> Option<AppExit> {
        let overlay = self.plugin_overlay.as_mut()?;
        match crate::overlay::OverlayWidget::handle_key(overlay.as_mut(), key) {
            OverlayAction::Stay | OverlayAction::PropagateKey => {}
            OverlayAction::Close => {
                self.plugin_overlay = None;
                if let Some(state) = self.active_dialog.take() {
                    let answer = state.cancelled();
                    let _ = state.reply().send(answer);
                }
            }
            OverlayAction::Resolve(value) => {
                self.plugin_overlay = None;
                if let Some(state) = self.active_dialog.take() {
                    let answer = state.resolved(&value);
                    let _ = state.reply().send(answer);
                }
            }
        }
        None
    }

    fn apply(&mut self, action: InputAction) -> Option<AppExit> {
        // Phase 9.10/9.11/9.17 will wire BeginCommand/BeginSearch/Yank;
        // for now they fall through to the silent EnterMode arm so the
        // modal state machine still cycles cleanly.
        match action {
            InputAction::Submit(text) => {
                if let Ok(mut buf) = self.buffer.lock() {
                    buf.push_user(text.clone());
                }
                let _ = self.send_request(RunRequest::Submit(text));
            }
            InputAction::Scroll(delta) => self.scroll_by(delta),
            InputAction::ScrollToTop => self.set_scroll(usize::MAX),
            InputAction::ScrollToBottom => self.set_scroll(0),
            InputAction::ToggleFold => self.toggle_last_fold(),
            InputAction::UnfoldAll => self.set_all_folds(false),
            InputAction::FoldAll => self.set_all_folds(true),
            InputAction::Cancel => {
                self.trip_cancel();
            }
            InputAction::OpenModelPicker => {
                if !self.model_choices.is_empty() {
                    self.picker = Some(OverlayPicker::new(
                        "Switch model",
                        self.model_choices.clone(),
                    ));
                    self.picker_kind = Some(PickerKind::Model);
                }
            }
            InputAction::OpenCommandPalette => {
                let registry = cmdline_registry(&self.plugin_command_specs);
                let ctx = SlashContext {
                    models: self.model_choices.iter().map(|p| p.value.clone()).collect(),
                    plugin_commands: self.plugin_commands.clone(),
                    sessions: self
                        .session_lister
                        .as_ref()
                        .map(|f| f())
                        .unwrap_or_default(),
                    themes: crate::theme::Theme::available_names(self.themes_dir.as_deref()),
                };
                let mut palette = SlashPalette::new(registry, ctx);
                palette.refresh();
                self.slash_palette = Some(palette);
            }
            InputAction::FocusPrev => {
                if let Ok(mut buf) = self.buffer.lock() {
                    buf.focus_prev_any();
                }
            }
            InputAction::FocusNext => {
                if let Ok(mut buf) = self.buffer.lock() {
                    buf.focus_next_any();
                }
            }
            InputAction::OpenSessionPicker => {
                if let Some(lister) = self.session_lister.as_ref() {
                    let items = lister();
                    if !items.is_empty() {
                        self.picker = Some(OverlayPicker::new("Resume session", items));
                        self.picker_kind = Some(PickerKind::Session);
                    }
                }
            }
            InputAction::BeginCommand => {
                self.cmdline = Some(CommandLine::new());
            }
            InputAction::EnterMode(_) => {}
            InputAction::Yank => self.yank_screen_selection(),
            InputAction::ClearSelection => self.clear_selection(),
            InputAction::EnterVisual => self.enter_visual_mode(),
            InputAction::VisualLeft => self.move_visual_cursor(0, -1),
            InputAction::VisualRight => self.move_visual_cursor(0, 1),
            InputAction::VisualUp => self.move_visual_cursor(-1, 0),
            InputAction::VisualDown => self.move_visual_cursor(1, 0),
            InputAction::VisualLineStart => self.snap_visual_cursor_x(0),
            InputAction::VisualLineEnd => self.snap_visual_cursor_x(i32::MAX),
            InputAction::YankFocusedBlock => self.yank_focused_block(),
            InputAction::BeginSearch => {
                self.search_line = Some(CommandLine::new());
            }
            InputAction::SearchNext => self.jump_to_search_match(true),
            InputAction::SearchPrev => self.jump_to_search_match(false),
            InputAction::CyclePane => {
                self.input.toggle_focused_pane();
            }
            InputAction::FocusPane(pane) => {
                self.input.set_focused_pane(pane);
            }
            InputAction::CycleThinkingLevel => {
                let _ = self.send_request(RunRequest::CycleThinkingLevel);
            }
        }
        None
    }

    fn send_request(&mut self, req: RunRequest) -> Result<(), TrySendError<RunRequest>> {
        match self.requests.send(req) {
            Ok(()) => Ok(()),
            Err(err) => Err(TrySendError::Disconnected(err.0)),
        }
    }

    /// Mouse left-button press: anchor a virtual-row selection at
    /// the click position. Any prior selection (and its captured
    /// text) is dropped. Focus snaps to whichever block sits under
    /// the click so subsequent keyboard gestures act on it.
    fn mouse_down(&mut self, row: u16, col: u16) {
        self.captured_rows.clear();
        if let Ok(mut buf) = self.buffer.lock() {
            let area_y = buf.last_area_y();
            let area_height = buf.last_area_height();
            if row < area_y || row >= area_y.saturating_add(area_height) {
                // Click landed outside the buffer rectangle. Anything
                // below the buffer is the input card or modeline; the
                // top status row is above. Clicks below the buffer
                // focus the input pane (vim-style window focus); top
                // status clicks leave focus alone.
                self.screen_selection = None;
                self.mouse_drag_anchor = None;
                if row >= area_y.saturating_add(area_height) {
                    self.input.set_focused_pane(Pane::Input);
                }
                return;
            }
            // Click inside the buffer area focuses the buffer pane.
            self.input.set_focused_pane(Pane::Buffer);
            let vrow = buf
                .last_virtual_top()
                .saturating_add(usize::from(row - area_y));
            self.screen_selection = Some(((vrow, col), (vrow, col)));
            if let Some(idx) = buf.block_at_screen_row(row) {
                buf.set_focus(Some(idx));
                self.mouse_drag_anchor = Some((row, idx, false));
            } else {
                self.mouse_drag_anchor = None;
            }
        }
    }

    /// Mouse drag while left-button is held: extend the selection
    /// cursor to the virtual-row under `(row, col)`. Drag rows
    /// outside the buffer area clamp to the closest visible row so
    /// sweeping past the input area still extends correctly.
    fn mouse_drag(&mut self, row: u16, col: u16) {
        let Some((anchor, _)) = self.screen_selection else {
            return;
        };
        if let Ok(buf) = self.buffer.lock() {
            let area_y = buf.last_area_y();
            let area_height = buf.last_area_height();
            if area_height == 0 {
                return;
            }
            let last_visible_row = area_y.saturating_add(area_height).saturating_sub(1);
            let clamped_row = row.clamp(area_y, last_visible_row);
            let vrow = buf
                .last_virtual_top()
                .saturating_add(usize::from(clamped_row - area_y));
            self.screen_selection = Some((anchor, (vrow, col)));
            if let Some((_, _, ref mut dragged)) = self.mouse_drag_anchor {
                *dragged = true;
            }
        }
    }

    /// Mouse left-button release: a non-dragged release on a block's
    /// header row toggles fold and clears the just-anchored
    /// zero-width selection; a dragged release leaves the selection
    /// in place so the user can `y` to copy it.
    fn mouse_up(&mut self, row: u16) {
        let Some((_down_row, anchor_idx, dragged)) = self.mouse_drag_anchor.take() else {
            return;
        };
        if dragged {
            return;
        }
        // Plain click: clear the zero-width selection we anchored on
        // press, then maybe toggle a fold on the header row.
        self.clear_selection();
        if let Ok(mut buf) = self.buffer.lock()
            && buf.screen_top_of(anchor_idx) == Some(row)
        {
            buf.toggle_fold(anchor_idx);
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        if let Ok(mut buf) = self.buffer.lock() {
            // Positive delta = move toward newest (decrement rows-up);
            // negative = move toward oldest (increment rows-up).
            let current = i64::try_from(buf.scroll()).unwrap_or(i64::MAX);
            let target = (current - i64::from(delta)).max(0);
            let clamped = usize::try_from(target).unwrap_or(0);
            buf.set_scroll(clamped);
        }
    }

    fn set_scroll(&mut self, scroll: usize) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.set_scroll(scroll);
        }
    }

    fn toggle_last_fold(&mut self) {
        if let Ok(mut buf) = self.buffer.lock() {
            if let Some(idx) = buf.effective_focus() {
                buf.toggle_fold(idx);
            }
        }
    }

    fn set_all_folds(&mut self, folded: bool) {
        if let Ok(mut buf) = self.buffer.lock() {
            buf.set_all_folded(folded);
        }
    }

    /// Read-only borrow of the input state. Tests use this to assert
    /// mode transitions without driving a real terminal.
    #[must_use]
    pub fn input(&self) -> &InputState {
        &self.input
    }

    /// Apply a key directly without going through crossterm. Used by
    /// tests and by external command handlers (Phase 9.10) that want to
    /// drive the modal state machine programmatically.
    pub fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Option<AppExit> {
        self.dispatch_key(key)
    }

    /// Force a redraw onto an arbitrary terminal. Tests use this with
    /// [`ratatui::backend::TestBackend`] to capture the rendered frame.
    pub fn render_into<B>(&mut self, terminal: &mut ratatui::Terminal<B>) -> Result<(), TuiError>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let search_match_count = self.compute_search_match_count();
        let render_width = terminal.size().map_or(80, |r| r.width);
        self.refresh_plugin_widget_texts(render_width);
        let mut buffer = self.buffer.lock().expect("buffer mutex poisoned");
        let session_usage = self.session_usage_snapshot();
        let live_toasts = self.live_toasts();
        let bottom = if self.modeline_visible() {
            crate::layout::STATUS_BOTTOM_LINES_DEFAULT
        } else {
            0
        };
        let model_snapshot = self
            .status_model
            .as_ref()
            .and_then(|m| m.lock().ok().map(|g| g.clone()));
        let cmdline = self.cmdline.as_ref();
        let status = view::StatusCtx {
            model: model_snapshot.as_deref(),
            session_id: self.status_session_id.as_deref(),
            search_pattern: self.search_pattern.as_deref(),
            search_line: self.search_line.as_ref(),
            search_match_count,
            plugin_widgets: &self.plugin_widget_texts,
            plugin_status: &self.plugin_status_cache,
            plugin_header: &self.plugin_header_lines,
            plugin_footer: &self.plugin_footer_lines,
        };
        let screen_selection = self.screen_selection;
        let mut captured_rows = std::mem::take(&mut self.captured_rows);
        let show_completion = self.input_completion.is_some()
            && self.slash_palette.is_none()
            && self.cmdline.is_none()
            && self.search_line.is_none()
            && self.picker.is_none()
            && self.settings_overlay.is_none()
            && self.session_tree.is_none()
            && self.plugin_overlay.is_none();
        let picker = self.picker.as_mut();
        let settings_overlay = self.settings_overlay.as_mut();
        let session_tree = self.session_tree.as_mut();
        let input_completion = if show_completion {
            self.input_completion.as_ref()
        } else {
            None
        };
        let input = &self.input;
        terminal
            .draw(|frame| {
                let body_width = frame
                    .area()
                    .width
                    .saturating_sub(2 + view::INPUT_GLYPH_WIDTH);
                let input_visual_lines = view::input_visual_row_count(input.text(), body_width);
                let input_height = input_height_for(input_visual_lines);
                let regions = split(frame.area(), input_height, bottom);
                view::render(
                    frame,
                    regions,
                    &mut buffer,
                    input,
                    cmdline,
                    &status,
                    screen_selection,
                    &mut captured_rows,
                    session_usage.as_ref(),
                    &live_toasts,
                );
                if let Some(picker) = picker {
                    picker.render(frame, frame.area());
                }
                if let Some(settings) = settings_overlay {
                    settings.render(frame, frame.area());
                }
                if let Some(tree) = session_tree {
                    tree.render(frame, frame.area());
                }
                if let Some(completion) = input_completion {
                    completion.render(frame, regions);
                }
            })
            .map_err(|err| TuiError::Io(std::io::Error::other(err.to_string())))?;
        self.captured_rows = captured_rows;
        Ok(())
    }
}

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
mod tests {
    use std::sync::mpsc;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::events::shared_buffer;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_q_exits_immediately() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let exit = app.handle_key(ctrl('q'));
        assert_eq!(exit, Some(AppExit::Quit));
    }

    #[test]
    fn submitting_a_prompt_pushes_user_block_and_request() {
        let buffer = shared_buffer();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(buffer.clone(), tx);
        // Default mode is Insert; type "hi" and press Enter.
        app.handle_key(key('h'));
        app.handle_key(key('i'));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let req = rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(req, RunRequest::Submit("hi".into()));
        let buf = buffer.lock().unwrap();
        assert!(matches!(
            buf.blocks().last(),
            Some(crate::buffer::Block::User { text }) if text == "hi"
        ));
        assert_eq!(app.input().mode(), Mode::Insert);
    }

    #[test]
    fn ctrl_c_in_normal_emits_cancel_request() {
        let buffer = shared_buffer();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        // Switch to Normal first; default is Insert.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(ctrl('c'));
        assert_eq!(rx.try_recv(), Ok(RunRequest::Cancel));
    }

    #[test]
    fn ctrl_c_flips_registered_cancel_flag_synchronously() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        // Switch to Normal first; default is Insert.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let flag = CancelFlag::new();
        app.set_cancel_flag(flag.clone());
        assert!(!flag.is_cancelled());
        app.handle_key(ctrl('c'));
        assert!(
            flag.is_cancelled(),
            "Ctrl-C should flip the cancel flag on the foreground thread"
        );
    }

    #[test]
    fn cancel_command_flips_registered_cancel_flag_synchronously() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let flag = CancelFlag::new();
        app.set_cancel_flag(flag.clone());
        let registry: Vec<&CommandSpec> = BUILTIN_COMMANDS.iter().collect();
        let result = app.run_command_validated("cancel", &registry);
        assert!(
            matches!(result, CommandResult::Done(None)),
            "expected Done(None), got {result:?}"
        );
        assert!(
            flag.is_cancelled(),
            ":cancel should flip the cancel flag on the foreground thread"
        );
    }

    #[test]
    fn render_into_paints_status_and_buffer() {
        let buffer = shared_buffer();
        if let Ok(mut buf) = buffer.lock() {
            buf.push_user("hello");
        }
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        app.render_into(&mut terminal).unwrap();
        let buf = terminal.backend().buffer();
        let mut found_user = false;
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            // The bubble pads the text with surrounding spaces; the
            // exact prefix glyph is renderer-internal.
            if row.contains(" hello ") {
                found_user = true;
            }
        }
        assert!(found_user);
    }

    #[test]
    fn plugin_header_replaces_builtin_status_bar() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval("kage.ui.set_header(function() return 'PLUGINHEADER' end)")
            .unwrap();
        app.set_plugin_chrome(rt.shared_header(), rt.shared_footer());
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        app.render_into(&mut terminal).unwrap();
        let rows = snapshot_rows(&terminal);
        assert!(rows[0].contains("PLUGINHEADER"), "top row: {:?}", rows[0]);
        assert!(
            !rows[0].contains("kage"),
            "builtin label leaked: {:?}",
            rows[0]
        );
    }

    #[test]
    fn plugin_footer_replaces_builtin_modeline() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.set_session_usage(crate::usage::shared_session_usage());
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval("kage.ui.set_footer(function() return 'PLUGINFOOTER' end)")
            .unwrap();
        app.set_plugin_chrome(rt.shared_header(), rt.shared_footer());
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        app.render_into(&mut terminal).unwrap();
        let rows = snapshot_rows(&terminal);
        let bottom = rows.last().unwrap();
        assert!(bottom.contains("PLUGINFOOTER"), "bottom row: {bottom:?}");
    }

    #[test]
    fn autocomplete_popup_opens_and_tab_accepts() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.add_autocomplete_provider({
                name = 'demo',
                complete = function(prefix, _ctx)
                    if prefix == '' or prefix:sub(-3) == 'bar' then return {} end
                    return { { value = prefix .. 'bar' } }
                end,
            })
            ",
        )
        .unwrap();
        app.set_plugin_autocomplete(rt.registered_autocomplete_providers());
        app.handle_key(key('f'));
        app.handle_key(key('o'));
        assert!(app.input_completion.is_some(), "popup should open");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input().text(), "fobar");
        assert!(app.input_completion.is_none(), "popup closes after accept");
    }

    #[test]
    fn autocomplete_respects_explicit_range() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.add_autocomplete_provider({
                name = 'at',
                complete = function(prefix, ctx)
                    if prefix:sub(1, 1) ~= '@' then return {} end
                    return { { value = '@README.md', range = { 0, ctx.cursor } } }
                end,
            })
            ",
        )
        .unwrap();
        app.set_plugin_autocomplete(rt.registered_autocomplete_providers());
        app.handle_key(key('@'));
        assert!(app.input_completion.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input().text(), "@README.md");
    }

    #[test]
    fn builtin_at_file_completion_without_plugins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.set_workdir(dir.path().to_path_buf());
        app.handle_key(key('@'));
        assert!(app.input_completion.is_some(), "@ opens file completion");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.input().text(), "@README.md");
    }

    #[test]
    fn terminal_input_hook_consumes_matching_key() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval(
            r"
            kage.on_terminal_input(function(ev)
                return ev.code == 'char' and ev.char == 'x'
            end)
            ",
        )
        .unwrap();
        app.set_plugin_terminal_hooks(rt.shared_terminal_hooks());
        app.handle_key(key('x'));
        assert_eq!(app.input().text(), "", "x consumed by hook");
        app.handle_key(key('y'));
        assert_eq!(app.input().text(), "y", "y passes through");
    }

    #[test]
    fn terminal_input_hook_cannot_block_ctrl_q() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval("kage.on_terminal_input(function() return true end)")
            .unwrap();
        app.set_plugin_terminal_hooks(rt.shared_terminal_hooks());
        assert_eq!(app.handle_key(ctrl('q')), Some(AppExit::Quit));
    }

    #[test]
    fn terminal_input_off_stops_consuming() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let rt = kage_plugin::PluginRuntime::new().unwrap();
        rt.eval(
            r"
            _G.off = kage.on_terminal_input(function() return true end)
            ",
        )
        .unwrap();
        app.set_plugin_terminal_hooks(rt.shared_terminal_hooks());
        app.handle_key(key('a'));
        assert_eq!(app.input().text(), "", "hook swallows everything");
        rt.eval("_G.off()").unwrap();
        app.handle_key(key('b'));
        assert_eq!(app.input().text(), "b", "off restores normal input");
    }

    #[test]
    fn autocomplete_inert_without_providers() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.handle_key(key('h'));
        app.handle_key(key('i'));
        assert!(app.input_completion.is_none());
        assert_eq!(app.input().text(), "hi");
    }

    #[test]
    fn tree_command_without_source_reports_unavailable() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer.clone(), tx);
        assert!(app.dispatch_builtin("tree", "").is_none());
        assert!(app.session_tree.is_none());
        let buf = buffer.lock().unwrap();
        assert!(matches!(
            buf.blocks().last(),
            Some(crate::buffer::Block::Custom { kind, .. }) if kind == "kage:error"
        ));
    }

    #[test]
    fn tree_command_opens_and_enter_dispatches_resume() {
        let buffer = shared_buffer();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.set_session_tree_source(Box::new(|| {
            vec![
                crate::overlay::SessionNode {
                    id: "root".into(),
                    path: "/s/root.jsonl".into(),
                    parent: None,
                    label: "root".into(),
                    is_current: true,
                },
                crate::overlay::SessionNode {
                    id: "child".into(),
                    path: "/s/child.jsonl".into(),
                    parent: Some("root".into()),
                    label: "child".into(),
                    is_current: false,
                },
            ]
        }));
        assert!(app.dispatch_builtin("tree", "").is_none());
        assert!(app.session_tree.is_some());
        // Selection starts on the current session (root); Enter resumes.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.session_tree.is_none());
        assert_eq!(
            rx.try_recv(),
            Ok(RunRequest::ResumeSession(std::path::PathBuf::from(
                "/s/root.jsonl"
            )))
        );
    }

    #[test]
    fn tree_d_key_dispatches_delete_request() {
        let buffer = shared_buffer();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.set_session_tree_source(Box::new(|| {
            vec![crate::overlay::SessionNode {
                id: "only".into(),
                path: "/s/only.jsonl".into(),
                parent: None,
                label: "only".into(),
                is_current: false,
            }]
        }));
        app.dispatch_builtin("tree", "");
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(
            rx.try_recv(),
            Ok(RunRequest::DeleteSession(std::path::PathBuf::from(
                "/s/only.jsonl"
            )))
        );
    }

    #[test]
    fn set_editor_modeless_flips_the_input_editor() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        // Default is vim-modal: Esc enters Normal.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.input().mode(), Mode::Normal);

        app.set_editor_modeless(true);
        assert!(app.input().is_modeless());
        // In modeless, Esc cancels the turn instead of switching modes.
        app.input.force_normal(); // prove set_modeless re-pins insert too
        app.set_editor_modeless(true);
        assert_eq!(app.input().mode(), Mode::Insert);
    }

    #[test]
    fn settings_command_opens_overlay_and_esc_closes_it() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        // `:settings` opens the modal (reads config read-only; never
        // writes, so this is safe in a test).
        assert!(app.dispatch_builtin("settings", "").is_none());
        assert!(app.settings_overlay.is_some());
        // While open it owns the keyboard; Esc cancels without persist.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.settings_overlay.is_none());
        assert_eq!(app.input().text(), "", "esc went to the overlay, not input");
    }

    fn snapshot_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buf = terminal.backend().buffer();
        let mut out = Vec::new();
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            out.push(row.trim_end().to_owned());
        }
        out
    }

    #[test]
    fn pasted_text_lands_in_input_area_with_newline_preserved() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.handle_key(key('i'));
        app.input.paste("first\nsecond");
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        app.render_into(&mut terminal).unwrap();
        let rows = snapshot_rows(&terminal);
        assert!(rows.iter().any(|r| r.contains("first")));
        assert!(rows.iter().any(|r| r.contains("second")));
    }

    #[test]
    fn scrolling_up_freezes_viewport_when_more_content_arrives() {
        let buffer = shared_buffer();
        if let Ok(mut buf) = buffer.lock() {
            for i in 0..20 {
                buf.push_user(format!("line{i}"));
            }
        }
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer.clone(), tx);
        // Default mode is Insert; switch to Normal for scrolling keys.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        // Stage C made k/j/G pane-aware: switch to buffer pane so
        // scrolling keys hit the buffer instead of moving the input
        // cursor.
        app.input.set_focused_pane(Pane::Buffer);
        // User scrolls up by 5 from the bottom.
        for _ in 0..5 {
            app.handle_key(key('k'));
        }
        let scroll_after_user = buffer.lock().unwrap().scroll();
        assert_eq!(scroll_after_user, 5);
        // Streaming delta arrives.
        if let Ok(mut buf) = buffer.lock() {
            buf.append_assistant_delta("new\nstreaming\ncontent");
        }
        // User's scroll position is preserved.
        assert_eq!(buffer.lock().unwrap().scroll(), 5);
        // Pressing G snaps back to bottom (auto-follow rearmed).
        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
        assert_eq!(buffer.lock().unwrap().scroll(), 0);
        assert!(buffer.lock().unwrap().is_following());
    }

    #[test]
    fn history_walk_replaces_input_text() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.set_history(vec!["older".into(), "newer".into()]);
        // Default mode is Insert; no need to press 'i'.
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input().text(), "newer");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input().text(), "older");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.input().text(), "newer");
    }

    #[test]
    fn fold_all_then_unfold_all_toggles_folds() {
        let buffer = shared_buffer();
        if let Ok(mut buf) = buffer.lock() {
            buf.append_thinking_delta("step one");
            buf.finish_streaming();
        }
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer.clone(), tx);
        // Default mode is Insert; switch to Normal for zM/zR.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        // zM folds all
        app.handle_key(key('z'));
        app.handle_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE));
        if let Ok(buf) = buffer.lock() {
            assert!(matches!(
                buf.blocks()[0],
                crate::buffer::Block::Thinking { folded: true, .. }
            ));
        }
        // zR opens all
        app.handle_key(key('z'));
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        if let Ok(buf) = buffer.lock() {
            assert!(matches!(
                buf.blocks()[0],
                crate::buffer::Block::Thinking { folded: false, .. }
            ));
        }
    }

    // --- PN.9 validation error tests ---

    fn builtin_registry() -> Vec<&'static CommandSpec> {
        BUILTIN_COMMANDS.iter().collect()
    }

    #[test]
    fn validated_unknown_command_returns_error_with_suggestion() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let registry = builtin_registry();
        let result = app.run_command_validated("quut", &registry);
        match result {
            CommandResult::ValidationError(msg) => {
                assert!(msg.contains("unknown command: quut"), "got {msg:?}");
                assert!(
                    msg.contains("did you mean"),
                    "should suggest closest match, got {msg:?}"
                );
            }
            other @ CommandResult::Done(_) => {
                panic!("expected ValidationError, got {other:?}");
            }
        }
    }

    #[test]
    fn validated_invalid_choice_returns_error() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let registry = builtin_registry();
        // "mouse mayb" is invalid: "mayb" is not in [on, off, toggle]
        let result = app.run_command_validated("mouse mayb", &registry);
        match result {
            CommandResult::ValidationError(msg) => {
                assert!(
                    msg.contains("state"),
                    "error should mention the arg name, got {msg:?}"
                );
            }
            other @ CommandResult::Done(_) => {
                panic!("expected ValidationError, got {other:?}");
            }
        }
    }

    #[test]
    fn validated_missing_required_arg_returns_error() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let registry = builtin_registry();
        // "model" without a required <id> argument
        let result = app.run_command_validated("model", &registry);
        match result {
            CommandResult::ValidationError(msg) => {
                assert!(
                    msg.contains("missing"),
                    "error should mention missing arg, got {msg:?}"
                );
            }
            other @ CommandResult::Done(_) => {
                panic!("expected ValidationError, got {other:?}");
            }
        }
    }

    #[test]
    fn validated_valid_command_returns_done() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let registry = builtin_registry();
        let result = app.run_command_validated("help", &registry);
        assert!(
            matches!(result, CommandResult::Done(_)),
            "expected Done, got {result:?}"
        );
    }

    #[test]
    fn validated_quit_returns_done_with_exit() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let registry = builtin_registry();
        let result = app.run_command_validated("quit", &registry);
        assert!(
            matches!(result, CommandResult::Done(Some(AppExit::Quit))),
            "expected Done(Some(Quit)), got {result:?}"
        );
    }

    #[test]
    fn validated_subcommand_validates_against_leaf_spec() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let registry = builtin_registry();
        // "theme set" without required <name> arg should error
        let result = app.run_command_validated("theme set", &registry);
        match result {
            CommandResult::ValidationError(msg) => {
                assert!(
                    msg.contains("missing"),
                    "error should mention missing arg, got {msg:?}"
                );
            }
            other @ CommandResult::Done(_) => {
                panic!("expected ValidationError, got {other:?}");
            }
        }
    }

    #[test]
    fn validated_empty_input_returns_done_none() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let registry = builtin_registry();
        let result = app.run_command_validated("", &registry);
        assert!(
            matches!(result, CommandResult::Done(None)),
            "expected Done(None), got {result:?}"
        );
    }

    #[test]
    fn validated_optional_arg_missing_is_ok() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let registry = builtin_registry();
        // "mouse" without arg is valid (optional arg)
        let result = app.run_command_validated("mouse", &registry);
        assert!(
            matches!(result, CommandResult::Done(_)),
            "mouse with no arg should be valid, got {result:?}"
        );
    }

    // --- PN.10 keystroke-level e2e tests ---
    //
    // These tests drive the modal state machine with raw `KeyEvent`s
    // and confirm that `:` and `/` both reach `run_command_validated`
    // through `dispatch_key`. They are the last gate against regressing
    // either pathway after the unification done in PN.6.

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(c));
        }
    }

    #[test]
    fn colon_keystrokes_dispatch_quit_handler() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        // Default mode is Insert; switch to Normal so `:` is bound.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let exit = app.handle_key(key(':'));
        assert!(exit.is_none());
        assert!(app.cmdline.is_some(), "':' should open the command line");
        type_str(&mut app, "quit");
        let exit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(exit, Some(AppExit::Quit));
        assert!(app.cmdline.is_none(), "successful submit closes cmdline");
    }

    #[test]
    fn slash_keystrokes_dispatch_quit_handler() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        // Default mode is Insert. `/` only opens the palette when the
        // input buffer is empty; that is the case for a fresh App.
        let exit = app.handle_key(key('/'));
        assert!(exit.is_none());
        assert!(
            app.slash_palette.is_some(),
            "'/' should open the slash palette"
        );
        type_str(&mut app, "quit");
        let exit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(exit, Some(AppExit::Quit));
        assert!(
            app.slash_palette.is_none(),
            "successful submit closes the palette"
        );
    }

    #[test]
    fn colon_tab_completes_to_lcp_and_opens_popup() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(key(':'));
        app.handle_key(key('m'));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let cl = app.cmdline.as_ref().expect("cmdline open");
        assert_eq!(cl.text(), "mo", "tab should extend to LCP of model/mouse");
        assert!(cl.popup_open(), "popup should be visible after LCP step");
        assert_eq!(cl.selected(), None, "LCP step does not pre-select a row");
    }

    #[test]
    fn slash_tab_completes_to_lcp_and_opens_popup() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.handle_key(key('/'));
        app.handle_key(key('m'));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let sp = app.slash_palette.as_ref().expect("palette open");
        let cl = sp.cmdline();
        assert_eq!(cl.text(), "mo", "tab should extend to LCP of model/mouse");
        assert!(cl.popup_open(), "popup should be visible after LCP step");
    }

    #[test]
    fn colon_bad_arg_keeps_cmdline_open_with_error() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(key(':'));
        type_str(&mut app, "mouse mayb");
        let exit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(exit.is_none());
        let cl = app.cmdline.as_ref().expect("cmdline stays open on bad arg");
        assert!(cl.error().is_some(), "validation error should be set");
    }

    #[test]
    fn slash_bad_arg_keeps_palette_open_with_error() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.handle_key(key('/'));
        type_str(&mut app, "mouse mayb");
        let exit = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(exit.is_none());
        let sp = app
            .slash_palette
            .as_ref()
            .expect("palette stays open on bad arg");
        assert!(
            sp.cmdline().error().is_some(),
            "validation error should be set on the palette"
        );
    }

    fn select_item(label: &str, value: serde_json::Value) -> kage_plugin::SelectItem {
        kage_plugin::SelectItem {
            label: label.to_owned(),
            value,
            detail: None,
        }
    }

    #[test]
    fn plugin_dialog_pick_sends_selected_item_value() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let (dtx, drx) = mpsc::channel();
        app.set_plugin_dialog(drx);
        let (reply_tx, reply_rx) = mpsc::channel();
        dtx.send(PluginDialog::Select {
            title: "Pick".to_owned(),
            items: vec![
                select_item("alpha", serde_json::json!("A")),
                select_item("beta", serde_json::json!(42)),
            ],
            reply: reply_tx,
        })
        .unwrap();

        app.drain_plugin_dialog();
        assert!(app.plugin_overlay.is_some());
        assert!(app.active_dialog.is_some());

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!(42)));
        assert!(app.plugin_overlay.is_none());
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn plugin_dialog_cancel_sends_none() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let (dtx, drx) = mpsc::channel();
        app.set_plugin_dialog(drx);
        let (reply_tx, reply_rx) = mpsc::channel();
        dtx.send(PluginDialog::Select {
            title: "Pick".to_owned(),
            items: vec![select_item("only", serde_json::json!("x"))],
            reply: reply_tx,
        })
        .unwrap();

        app.drain_plugin_dialog();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(reply_rx.recv().unwrap(), None);
        assert!(app.plugin_overlay.is_none());
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn plugin_dialog_empty_items_resolves_to_none_without_a_picker() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let (dtx, drx) = mpsc::channel();
        app.set_plugin_dialog(drx);
        let (reply_tx, reply_rx) = mpsc::channel();
        dtx.send(PluginDialog::Select {
            title: "Empty".to_owned(),
            items: Vec::new(),
            reply: reply_tx,
        })
        .unwrap();

        app.drain_plugin_dialog();

        assert!(app.plugin_overlay.is_none());
        assert_eq!(reply_rx.recv().unwrap(), None);
    }

    #[test]
    fn plugin_dialog_not_drained_while_another_overlay_is_open() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let (dtx, drx) = mpsc::channel();
        app.set_plugin_dialog(drx);
        app.picker = Some(OverlayPicker::new("busy", vec![PickItem::simple("x")]));
        app.picker_kind = Some(PickerKind::Model);
        let (reply_tx, _reply_rx) = mpsc::channel();
        dtx.send(PluginDialog::Select {
            title: "later".to_owned(),
            items: vec![select_item("a", serde_json::json!("a"))],
            reply: reply_tx,
        })
        .unwrap();

        app.drain_plugin_dialog();

        assert_eq!(app.picker_kind, Some(PickerKind::Model));
        assert!(app.plugin_overlay.is_none());
        assert!(app.active_dialog.is_none());
    }

    fn open_confirm(app: &mut App) -> std::sync::mpsc::Receiver<Option<serde_json::Value>> {
        let (dtx, drx) = mpsc::channel();
        app.set_plugin_dialog(drx);
        let (reply_tx, reply_rx) = mpsc::channel();
        dtx.send(PluginDialog::Confirm {
            title: "Delete?".to_owned(),
            message: "are you sure".to_owned(),
            reply: reply_tx,
        })
        .unwrap();
        app.drain_plugin_dialog();
        assert!(app.plugin_overlay.is_some());
        reply_rx
    }

    #[test]
    fn plugin_confirm_yes_resumes_with_true() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let reply_rx = open_confirm(&mut app);

        app.handle_key(key('y'));

        assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!(true)));
        assert!(app.plugin_overlay.is_none());
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn plugin_confirm_no_resumes_with_false() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let reply_rx = open_confirm(&mut app);

        app.handle_key(key('n'));

        assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!(false)));
    }

    #[test]
    fn plugin_confirm_cancel_resumes_with_false() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let reply_rx = open_confirm(&mut app);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!(false)));
        assert!(app.plugin_overlay.is_none());
        assert!(app.active_dialog.is_none());
    }

    fn open_input(app: &mut App) -> std::sync::mpsc::Receiver<Option<serde_json::Value>> {
        let (dtx, drx) = mpsc::channel();
        app.set_plugin_dialog(drx);
        let (reply_tx, reply_rx) = mpsc::channel();
        dtx.send(PluginDialog::Input {
            title: "Your name".to_owned(),
            placeholder: Some("e.g. Ada".to_owned()),
            reply: reply_tx,
        })
        .unwrap();
        app.drain_plugin_dialog();
        assert!(app.plugin_overlay.is_some());
        reply_rx
    }

    #[test]
    fn plugin_input_submit_resumes_with_text() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let reply_rx = open_input(&mut app);

        app.handle_key(key('A'));
        app.handle_key(key('d'));
        app.handle_key(key('a'));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!("Ada")));
        assert!(app.plugin_overlay.is_none());
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn plugin_input_cancel_resumes_with_nil() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let reply_rx = open_input(&mut app);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(reply_rx.recv().unwrap(), None);
        assert!(app.plugin_overlay.is_none());
        assert!(app.active_dialog.is_none());
    }

    fn open_editor(app: &mut App) -> std::sync::mpsc::Receiver<Option<serde_json::Value>> {
        let (dtx, drx) = mpsc::channel();
        app.set_plugin_dialog(drx);
        let (reply_tx, reply_rx) = mpsc::channel();
        dtx.send(PluginDialog::Editor {
            title: "Compose".to_owned(),
            prefill: Some("hi".to_owned()),
            reply: reply_tx,
        })
        .unwrap();
        app.drain_plugin_dialog();
        assert!(app.plugin_overlay.is_some());
        reply_rx
    }

    #[test]
    fn plugin_editor_ctrl_s_resumes_with_buffer() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let reply_rx = open_editor(&mut app);

        app.handle_key(key('!'));
        app.handle_key(ctrl('s'));

        assert_eq!(reply_rx.recv().unwrap(), Some(serde_json::json!("hi!")));
        assert!(app.plugin_overlay.is_none());
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn plugin_editor_cancel_resumes_with_nil() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let reply_rx = open_editor(&mut app);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(reply_rx.recv().unwrap(), None);
        assert!(app.plugin_overlay.is_none());
        assert!(app.active_dialog.is_none());
    }

    #[test]
    fn plugin_keybinding_dispatches_invoke_request() {
        let buffer = shared_buffer();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.set_plugin_keybindings(vec!["ctrl+g".to_owned()]);

        app.handle_key(ctrl('g'));

        assert_eq!(
            rx.try_recv(),
            Ok(RunRequest::InvokePluginKeybinding {
                chord: "ctrl+g".to_owned()
            })
        );
    }

    #[test]
    fn unbound_chord_does_not_dispatch_keybinding() {
        let buffer = shared_buffer();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.set_plugin_keybindings(vec!["ctrl+g".to_owned()]);

        app.handle_key(ctrl('h'));

        assert!(!matches!(
            rx.try_recv(),
            Ok(RunRequest::InvokePluginKeybinding { .. })
        ));
    }

    #[test]
    fn open_overlay_suppresses_plugin_keybinding() {
        let buffer = shared_buffer();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        app.set_plugin_keybindings(vec!["ctrl+g".to_owned()]);
        app.picker = Some(OverlayPicker::new("busy", vec![PickItem::simple("x")]));
        app.picker_kind = Some(PickerKind::Model);

        app.handle_key(ctrl('g'));

        assert!(rx.try_recv().is_err(), "picker should swallow the chord");
    }

    #[test]
    fn drain_plugin_theme_refreshes_snapshot_and_applies_request() {
        let buffer = shared_buffer();
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(buffer, tx);
        let state: kage_plugin::SharedThemeState =
            std::sync::Arc::new(std::sync::Mutex::new(kage_plugin::ThemeState::default()));
        let request: kage_plugin::SharedThemeRequest =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        app.set_plugin_theme(state.clone(), request.clone());

        // First drain with no request: snapshot is populated.
        app.drain_plugin_theme();
        {
            let s = state.lock().unwrap();
            assert!(!s.current.is_empty());
            assert!(s.available.iter().any(|n| n == "tokyo-night"));
        }

        // Queue a switch; next drain applies it on this thread.
        *request.lock().unwrap() = Some("tokyo-night".to_owned());
        app.drain_plugin_theme();
        assert_eq!(crate::theme::current().name, "tokyo-night");
        assert_eq!(state.lock().unwrap().current, "tokyo-night");
        assert!(request.lock().unwrap().is_none(), "request was drained");
    }
}
