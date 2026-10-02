//! The app shell: three resizable panels, the key router, and the
//! wiring between a transport and the store.
//!
//! The shell owns the transport and the composer; the panels are thin
//! views over the store. Keys route here: ctrl-n opens a session,
//! ctrl-b toggles the workbench, ctrl-\ toggles the sidebar, ctrl-q
//! quits, ctrl-enter sends the composer.

use std::collections::BTreeMap;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::base::Selectable as _;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape as InputEscape, Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Sizable as _, h_flex, h_resizable, resizable_panel, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Anchor, AnyElement, App, AppContext, ClickEvent, ClipboardItem, Context, Entity, FocusHandle,
    Focusable as _, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Pixels,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    px,
};

use crate::clock::unix_seconds;
use crate::prefs::Prefs;
use crate::store::{Command, Store, StoreHandle as _};
use crate::theme::{CONTENT_W, FS_SM, FS_XS, PANEL_HEAD_H, R_FULL, SIDE_W, SP_4, SP_6, SP_8};
use crate::transport::{Event, Transport};
use crate::views::chrome::{
    FindBar, FindEvent, NoticeWatch, PaletteView, ToastAction, ToastDraft, Toasts, WelcomeView,
    toasts_for_changes,
};
use crate::views::deferred::Deferred;
use crate::views::dialog::{DialogKind, DialogView};
use crate::views::settings::{Section, SettingsView};
use crate::views::sidebar::{RowState, row_state};
use crate::views::vim::{self, Modeline, VimLeaveInsert};
use crate::views::{
    ApprovalCard, ApprovalEvent, ComposerView, DockEvent, DockRow, SidebarView, TranscriptEvent,
    TranscriptView, WorkbenchEvent, WorkbenchView,
};
use kage_client::wire::NoticeTone;
use kage_client::{Change, Frame};

/// The prompt the replay transcript was recorded with.
const REPLAY_PROMPT: &str = "fix the null check";

/// The workbench panel's open width, matching the web client's
/// default workbench width.
const WORKBENCH_W: f32 = 460.0;

/// The narrowest the chat pane gets when a side panel is dragged wider.
const CHAT_MIN_W: f32 = 360.0;

/// The short name a session's directory carries in the crumbs: the
/// last path segment, or `local` when the session has none.
#[must_use]
pub(crate) fn project_name(cwd: Option<&str>) -> SharedString {
    let Some(cwd) = cwd else {
        return "local".into();
    };
    let trimmed = cwd.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if name.is_empty() {
        "local".into()
    } else {
        name.into()
    }
}

/// The directory new sessions open in. The browser has no filesystem,
/// so the read is desktop-only and sessions start without a directory
/// there.
#[cfg(not(target_arch = "wasm32"))]
fn working_dir() -> String {
    std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
fn working_dir() -> String {
    String::new()
}

gpui_kit::actions!(
    kage_desktop,
    [
        Quit,
        NewSession,
        ToggleSidebar,
        ToggleWorkbench,
        SendPrompt,
        OpenFind,
        OpenPalette,
        OpenSettings,
        OpenProviders,
        ToggleSwarm,
        TogglePlan,
        SetGoal,
        ShowAgents,
        ReviewChanges
    ]
);

/// The shell's key bindings, shared by the native and browser builds.
#[must_use]
pub fn key_bindings() -> Vec<KeyBinding> {
    use crate::views::chrome::{
        FindClose, FindNext, FindPrev, PaletteClose, PaletteDown, PaletteRun, PaletteUp,
    };
    let mut bindings = vec![
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("ctrl-n", NewSession, None),
        KeyBinding::new("cmd-n", NewSession, None),
        KeyBinding::new("ctrl-b", ToggleWorkbench, None),
        KeyBinding::new("cmd-b", ToggleWorkbench, None),
        KeyBinding::new("ctrl-\\", ToggleSidebar, None),
        KeyBinding::new("cmd-\\", ToggleSidebar, None),
        KeyBinding::new("ctrl-enter", SendPrompt, None),
        KeyBinding::new("ctrl-f", OpenFind, None),
        KeyBinding::new("cmd-f", OpenFind, None),
        KeyBinding::new("ctrl-k", OpenPalette, None),
        KeyBinding::new("cmd-k", OpenPalette, None),
        KeyBinding::new("ctrl-,", OpenSettings, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        // The find bar and the palette answer in their own key contexts,
        // so Enter, the arrows and Esc act only while their query holds
        // the focus.
        KeyBinding::new("enter", FindNext, Some("Find")),
        KeyBinding::new("shift-enter", FindPrev, Some("Find")),
        KeyBinding::new("escape", FindClose, Some("Find")),
        KeyBinding::new("up", PaletteUp, Some("Palette")),
        KeyBinding::new("down", PaletteDown, Some("Palette")),
        KeyBinding::new("enter", PaletteRun, Some("Palette")),
        KeyBinding::new("escape", PaletteClose, Some("Palette")),
        KeyBinding::new("escape", crate::views::dialog::DialogClose, Some("Dialog")),
        KeyBinding::new("up", crate::views::pickers::PickerUp, Some("Picker")),
        KeyBinding::new("down", crate::views::pickers::PickerDown, Some("Picker")),
        KeyBinding::new("enter", crate::views::pickers::PickerRun, Some("Picker")),
        KeyBinding::new("escape", crate::views::pickers::PickerClose, Some("Picker")),
        KeyBinding::new(
            "escape",
            crate::views::settings::SettingsClose,
            Some("Settings"),
        ),
        // Inside the input's own context, so it wins over the toolkit's
        // outdent binding while the composer is focused.
        KeyBinding::new(
            "shift-tab",
            crate::views::composer::CycleMode,
            Some("Input"),
        ),
    ];
    bindings.extend(crate::views::vim::bindings());
    bindings
}

/// Routes the actions that need no window. The shell's root element
/// handles the rest, since an action dispatched in a window cannot
/// update that same window from an app-level listener.
pub fn route_actions(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
}

/// What the shell is launched with.
pub struct ShellArgs {
    /// The transport to connect through, already built, not started.
    pub transport: Box<dyn Transport>,
    /// Whether the connection plays the recording, which wants the
    /// scripted prompt.
    pub replay: bool,
    /// Whether the 30 updates per second stream runs for measurement.
    pub stream: bool,
    /// The preferences stored at launch.
    pub prefs: Prefs,
}

/// The topbar's inline title editor.
struct Rename {
    editor: Entity<InputState>,
    /// The current title, written once the field has laid out; see
    /// [`crate::views::deferred`].
    title: Deferred,
    _events: Subscription,
}

/// The window's root view.
pub struct Shell {
    store: Entity<Store>,
    transport: Box<dyn Transport>,
    composer: Entity<ComposerView>,
    sidebar: Entity<SidebarView>,
    transcript: Entity<TranscriptView>,
    /// The turn timeline rail on the chat pane's right edge.
    rail: Entity<crate::views::transcript::RailView>,
    workbench: Entity<WorkbenchView>,
    dock: Entity<DockRow>,
    approval: Entity<ApprovalCard>,
    find: Entity<FindBar>,
    palette: Entity<PaletteView>,
    /// The modal dialog layer.
    dialog: Entity<DialogView>,
    /// The settings dialog.
    settings: Entity<SettingsView>,
    toasts: Entity<Toasts>,
    welcome: Entity<WelcomeView>,
    /// Counts the notice items each session held, so frames that add
    /// notices raise their toast once.
    notices: NoticeWatch,
    /// When the active turn began, per session, as Unix seconds. The
    /// client model carries only the in-flight flag, so the shell
    /// stamps the first frame that sees the flag up.
    turn_started: BTreeMap<String, i64>,
    sidebar_visible: bool,
    workbench_visible: bool,
    /// The sidebar floated over the content at the web client's narrow
    /// breakpoint, where it takes no column. Opening or creating a
    /// session closes it, as the web client does.
    side_float: bool,
    /// The viewport width as of the last rendered frame, so panel
    /// toggles off the render path can read the breakpoints.
    viewport_width: f32,
    /// The active session id as of the last store tick, to close the
    /// sidebar float when the selection moves.
    last_active: Option<String>,
    /// The inline title editor while the user renames the session.
    rename: Option<Rename>,
    /// The preferences revision last stored.
    saved_prefs: u64,
    /// Where each open session stood at the last store change, so a
    /// session out of view that moves raises its toast once.
    statuses: BTreeMap<String, RowState>,
    /// Normal mode's focus, on the chat column, while vim mode is on.
    vim_focus: FocusHandle,
    /// The open `:` line.
    vim_line: Option<(Entity<InputState>, Subscription)>,
    /// The channel transport events arrive on, kept so a new transport
    /// can take over when the setup screen picks a `kage`.
    #[cfg(not(target_arch = "wasm32"))]
    events: crate::transport::EventSender,
    /// The screen shown while the stdio transport finds no `kage`.
    #[cfg(not(target_arch = "wasm32"))]
    setup: Entity<crate::views::setup::SetupView>,
    streamed: usize,
    /// The boot splash over the window until the first handshake.
    splash: crate::views::splash::Splash,
}

impl Shell {
    /// Builds the shell, starts the transport, and pumps its events.
    pub fn new(args: ShellArgs, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(args, window, cx)
    }

    /// Builds every view of the shell and wires them to the store.
    fn build(mut args: ShellArgs, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cwd = working_dir();
        let link = args.transport.link();
        let prefs = std::mem::take(&mut args.prefs);
        let store = cx.new(|_| {
            Store::new(cwd, args.replay)
                .with_link(link)
                .with_prefs(prefs)
        });
        let sidebar = cx.new(|_| SidebarView::new(store.clone()));
        let dialog = cx.new(|cx| DialogView::new(store.clone(), window, cx));
        let settings = cx.new(|cx| SettingsView::new(store.clone(), cx));
        let composer = cx.new(|cx| ComposerView::new(store.clone(), dialog.clone(), window, cx));
        let input = composer.read(cx).input().clone();
        // The other views that write this same textarea wait on the
        // composer's element laying out, because the composer is what
        // renders that element.
        let input_laid_out = composer.read(cx).input_laid_out();
        let transcript = cx.new(|cx| TranscriptView::new(store.clone(), input.clone(), window, cx));
        let rail = cx.new(|cx| crate::views::transcript::RailView::new(transcript.clone(), cx));
        let workbench = cx.new(|cx| WorkbenchView::new(store.clone(), input.clone(), cx));
        let dock = cx.new(|cx| DockRow::new(store.clone(), window, cx));
        let approval = cx.new(|cx| ApprovalCard::new(store.clone(), window, cx));
        let find = cx.new(|cx| FindBar::new(store.clone(), transcript.clone(), window, cx));
        let palette = cx.new(|cx| {
            PaletteView::new(
                store.clone(),
                input.clone(),
                input_laid_out.clone(),
                window,
                cx,
            )
        });
        let toasts = cx.new(|_| Toasts::new(store.clone()));
        let welcome = cx.new(|_| {
            WelcomeView::new(
                store.clone(),
                input.clone(),
                composer.clone(),
                dialog.clone(),
                input_laid_out,
            )
        });

        cx.subscribe_in(
            &dock,
            window,
            |shell, _, event: &DockEvent, _, cx| match event {
                DockEvent::ScrollToPlan => shell
                    .transcript
                    .update(cx, |transcript, cx| transcript.scroll_to_plan(cx)),
                DockEvent::OpenBackgroundAgents => {
                    shell.workbench_visible = true;
                    shell
                        .workbench
                        .update(cx, |workbench, cx| workbench.open_background_agents(cx));
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &approval,
            window,
            |shell, _, event: &ApprovalEvent, window, cx| match event {
                ApprovalEvent::Released => {
                    let input = shell.composer.read(cx).input().clone();
                    input.update(cx, |state, cx| state.focus(window, cx));
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &transcript,
            window,
            |shell, _, event: &TranscriptEvent, window, cx| {
                if let TranscriptEvent::Rewind(prompt) = event {
                    shell.ask_rewind(*prompt, window, cx);
                    return;
                }
                shell.workbench_visible = true;
                shell.workbench.update(cx, |workbench, cx| match event {
                    TranscriptEvent::OpenFile(path) => workbench.open_file(path, cx),
                    TranscriptEvent::OpenChange(call) => workbench.open_change(call.clone(), cx),
                    TranscriptEvent::OpenBrowser => workbench.open_browser(cx),
                    TranscriptEvent::OpenAgent(id) => workbench.open_agent(id.clone(), window, cx),
                    TranscriptEvent::OpenAgents => workbench.open_agents(cx),
                    TranscriptEvent::Rewind(_) => {}
                });
                cx.notify();
            },
        )
        .detach();
        cx.subscribe_in(
            &find,
            window,
            |shell, _, event: &FindEvent, window, cx| match event {
                FindEvent::Closed => shell.on_find_closed(window, cx),
            },
        )
        .detach();
        cx.subscribe_in(
            &workbench,
            window,
            |shell, _, event: &WorkbenchEvent, window, cx| match event {
                WorkbenchEvent::Mention(path) => {
                    let path = path.clone();
                    shell.composer.update(cx, |composer, cx| {
                        composer.insert_mention(Some(&path), window, cx);
                    });
                }
            },
        )
        .detach();

        let (events, incoming) = async_channel::unbounded();
        args.transport.start(events.clone());
        #[cfg(not(target_arch = "wasm32"))]
        let setup = cx.new(|cx| crate::views::setup::SetupView::new(window, cx));
        #[cfg(not(target_arch = "wasm32"))]
        cx.subscribe_in(
            &setup,
            window,
            |shell, _, event: &crate::views::setup::SetupEvent, _, cx| {
                use crate::views::setup::SetupEvent;
                let program = match event {
                    SetupEvent::Use(program) => {
                        let saved = program.clone();
                        shell.store.act(cx, |store| {
                            store.update_prefs(|prefs| prefs.kage_path = Some(saved));
                        });
                        program.clone()
                    }
                    SetupEvent::Retry => "kage".to_owned(),
                };
                shell.restart_stdio(program, cx);
            },
        )
        .detach();

        cx.observe(&store, |shell, _, cx| {
            shell.flush_outgoing(cx);
            shell.save_prefs(cx);
            shell.raise_notes(cx);
            shell.watch_away(cx);
            let active = shell
                .store
                .read(cx)
                .active_session()
                .map(|session| session.id.clone());
            if active != shell.last_active {
                shell.last_active = active;
                shell.side_float = false;
                cx.notify();
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = incoming.recv().await {
                if this
                    .update(cx, |shell, cx| shell.on_transport(event, cx))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();

        if args.stream {
            let stream = cx.spawn(async move |this, cx| {
                let frame = Duration::from_secs_f64(1.0 / 30.0);
                loop {
                    cx.background_executor().timer(frame).await;
                    if this.update(cx, |shell, cx| shell.stream_chunk(cx)).is_err() {
                        return;
                    }
                }
            });
            stream.detach();
        }

        Self {
            store,
            transport: args.transport,
            composer,
            sidebar,
            transcript,
            rail,
            workbench,
            dock,
            approval,
            find,
            palette,
            dialog,
            settings,
            toasts,
            welcome,
            notices: NoticeWatch::default(),
            turn_started: BTreeMap::new(),
            sidebar_visible: true,
            // Closed at first, as the design has it: the workbench is a
            // panel the user asks for with Ctrl B, and opening it by
            // default narrows the transcript on every launch.
            workbench_visible: false,
            side_float: false,
            // Measured on the first frame; until then behave wide.
            viewport_width: f32::MAX,
            last_active: None,
            rename: None,
            saved_prefs: 0,
            statuses: BTreeMap::new(),
            vim_focus: cx.focus_handle(),
            vim_line: None,
            #[cfg(not(target_arch = "wasm32"))]
            events,
            #[cfg(not(target_arch = "wasm32"))]
            setup,
            streamed: 0,
            splash: crate::views::splash::Splash::Showing,
        }
    }

    /// Accepts one transport event into the store and carries out the
    /// commands it produced.
    fn on_transport(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Frame(frame) => {
                let changes = self.store.update(cx, |store, cx| {
                    let changes = store.absorb(frame);
                    cx.notify();
                    changes
                });
                self.raise_toasts(&changes, cx);
            }
            Event::State(state) => {
                self.store.update(cx, |store, cx| {
                    store.set_connect(state);
                    cx.notify();
                });
            }
        }
        self.track_turn(cx);
        let commands = self.store.update(cx, |store, _| store.take_commands());
        if commands.is_empty() {
            return;
        }
        self.store.act(cx, |store| {
            for command in commands {
                match command {
                    Command::Handshake { replay_sessions } => store.handshake(replay_sessions),
                    Command::NewSession => store.new_session(),
                    Command::ReplayPrompt => {
                        store.prompt(REPLAY_PROMPT);
                    }
                }
            }
        });
    }

    /// Stamps when a session's turn began, and drops the stamp when
    /// the turn ends, so the topbar can show a working duration.
    fn track_turn(&mut self, cx: &mut Context<Self>) {
        let live: Vec<(String, bool)> = self
            .store
            .read(cx)
            .state()
            .sessions
            .iter()
            .map(|(id, session)| (id.clone(), session.in_turn || session.running))
            .collect();
        let now = unix_seconds();
        for (id, running) in live {
            if running {
                self.turn_started.entry(id).or_insert(now);
            } else {
                self.turn_started.remove(&id);
            }
        }
        self.turn_started
            .retain(|id, _| self.store.read(cx).state().sessions.contains_key(id));
    }

    /// Raises toasts for the answered-elsewhere change and for any
    /// notice items the frames added.
    fn raise_toasts(&mut self, changes: &[Change], cx: &mut Context<Self>) {
        let mut drafts = toasts_for_changes(changes);
        drafts.extend(self.notices.scan(self.store.read(cx).state()));
        for change in changes {
            if let Change::Exported { markdown, .. } = change {
                cx.write_to_clipboard(ClipboardItem::new_string(markdown.clone()));
                drafts.push(ToastDraft {
                    tone: NoticeTone::Success,
                    text: "Copied the session as Markdown".to_owned(),
                    action: ToastAction::None,
                });
            }
        }

        if drafts.is_empty() {
            return;
        }
        self.toasts.update(cx, |toasts, cx| {
            for draft in drafts {
                toasts.push(draft, cx);
            }
        });
    }

    /// Drains the client's outgoing frames into the transport. Runs
    /// on every store change, so views that only talk to the store
    /// still reach the engine.
    fn flush_outgoing(&mut self, cx: &mut Context<Self>) {
        let frames = self.store.update(cx, |store, _| store.take_outgoing());
        for frame in frames {
            self.transport.send(frame);
        }
    }

    /// Leaves the active session: the welcome pane shows, and the
    /// next prompt opens the session it rides on. This is what the
    /// web client's New session does.
    pub fn show_welcome(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.show_welcome();
            cx.notify();
        });
    }

    /// Opens find over the transcript; the palette steps aside.
    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.update(cx, |palette, cx| palette.close(cx));
        self.find.update(cx, |find, cx| find.open(window, cx));
        cx.notify();
    }

    /// Opens the command palette; find steps aside.
    pub fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find.update(cx, |find, cx| find.close(window, cx));
        self.palette
            .update(cx, |palette, cx| palette.open(window, cx));
        cx.notify();
    }

    /// Hands the focus back to the composer after the find bar closed
    /// itself, unless an approval ask holds the focus.
    fn on_find_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.read(cx).prefs().vim {
            window.focus(&self.vim_focus, cx);
        } else if self.store.read(cx).active_asks().is_empty() {
            let input = self.composer.read(cx).input().clone();
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    /// Sends or queues the composer text on the active session.
    pub fn send_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer
            .update(cx, |composer, cx| composer.submit(false, window, cx));
    }

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        // At the web client's narrow breakpoint the sidebar takes no
        // column, so the toggle floats it over the content instead.
        if self.viewport_width <= 860. {
            self.side_float = !self.side_float;
        } else {
            self.sidebar_visible = !self.sidebar_visible;
        }
        cx.notify();
    }

    pub fn toggle_workbench(&mut self, cx: &mut Context<Self>) {
        self.workbench_visible = !self.workbench_visible;
        cx.notify();
    }

    /// Appends one synthetic agent chunk, for the 30 updates per
    /// second measurement against the real client path.
    fn stream_chunk(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.store.read(cx).active_id().map(str::to_owned) else {
            return;
        };
        self.streamed += 1;
        let text = format!("stream chunk {}\n", self.streamed);
        let frame = Frame::Notification {
            method: "session/update".to_owned(),
            params: serde_json::json!({
                "sessionId": id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                },
            }),
        };
        self.store.update(cx, |store, cx| {
            store.absorb(frame);
            cx.notify();
        });
    }

    /// The line automated runs print before quitting.
    #[must_use]
    pub fn smoke_line(&self, cx: &App) -> String {
        let store = self.store.read(cx);
        let state = store.state();
        let agent = state
            .agent
            .as_ref()
            .map(|agent| format!("{} {}", agent.name, agent.version.as_deref().unwrap_or("?")))
            .unwrap_or_else(|| "none".to_owned());
        let (items, used, size) = store
            .active_session()
            .map(|session| (session.items.len(), session.usage.used, session.usage.size))
            .unwrap_or((0, 0, 0));
        let connection = self.transport.connection_id();
        format!(
            "smoke: connect={} agent=({agent}) sessions={sessions} items={items} used={used}/{size} streamed={streamed} connection={connection}",
            store.connect().label(),
            sessions = state.sessions.len(),
            items = items,
            used = used,
            size = size,
            streamed = self.streamed,
            connection = connection.as_deref().unwrap_or("-"),
        )
    }

    /// The composer under the transcript, at the content width.
    ///
    /// Without a session there is no band and no call to this: the
    /// welcome column carries the composer at the narrower welcome width,
    /// so the wordmark reads into it rather than over the cards. One
    /// composer entity mounts in one place or the other, so the draft
    /// survives the move.
    fn composer_row(&self, _cx: &Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .max_w(px(CONTENT_W))
            .child(self.composer.clone())
    }

    /// The 48px panel head over the content: the crumbs (project and
    /// session title), the working pill while a turn runs, and the
    /// panel toggles at the far end, as the web client's topbar draws
    /// them. Without a session the bar carries the toggles alone.
    fn topbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::active(cx);
        let sidebar_visible = self.sidebar_visible;
        let side_float = self.side_float;
        let workbench_visible = self.workbench_visible;
        let session = self.store.read(cx).active_session();
        let now = unix_seconds();
        let has_session = session.is_some();
        let running = session.is_some_and(|session| session.in_turn || session.running);
        let forked =
            session.is_some_and(|session| self.store.read(cx).fork_parent(&session.id).is_some());
        let working = session
            .filter(|session| session.in_turn || session.running)
            .and_then(|session| self.turn_started.get(&session.id))
            .map(|started| crate::clock::duration(now - started));
        h_flex()
            .h(px(PANEL_HEAD_H))
            .flex_none()
            .pl(px(14.))
            .pr(px(10.))
            .gap(px(SP_4))
            // At the narrow breakpoint the sidebar takes no column, so
            // the web client keeps the show button up permanently.
            .when(side_float || !sidebar_visible, |bar| {
                bar.child(
                    Button::new("show-sidebar")
                        .icon(IconName::PanelLeft)
                        .ghost()
                        .tooltip("Show sidebar (Ctrl \\)")
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                )
            })
            .children(session.map(|session| {
                let name = project_name(session.cwd.as_deref());
                let title = self.store.read(cx).display_title(&session.id);
                h_flex()
                    .id("topbar-crumbs")
                    .min_w_0()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(FS_SM))
                    .child(div().flex_none().text_color(p.muted).child(name))
                    .child(div().flex_none().text_color(p.ghost).child("/"))
                    .child(self.title_crumb(title, cx))
                    .into_any_element()
            }))
            .when(forked, |bar| {
                bar.child(
                    h_flex()
                        .id("fork-pill")
                        .flex_none()
                        .h(px(24.))
                        .px(px(9.))
                        .gap(px(5.))
                        .items_center()
                        .rounded(px(R_FULL))
                        .bg(p.fill)
                        .text_size(px(FS_XS))
                        .text_color(p.muted)
                        .child(Icon::new(IconName::GitFork).with_size(px(12.)))
                        .child("fork"),
                )
            })
            .child(div().flex_1())
            .children(working.map(|label| {
                h_flex()
                    .id("working-pill")
                    .flex_none()
                    .h(px(24.))
                    .px(px(9.))
                    .gap(px(6.))
                    .items_center()
                    .rounded(px(R_FULL))
                    .bg(p.fill)
                    .text_size(px(FS_XS))
                    .text_color(p.muted)
                    .child(crate::views::eclipse::eclipse(
                        14.,
                        Some(crate::clock::epoch()),
                        p,
                    ))
                    .child(SharedString::from(format!("Working {label}")))
                    .into_any_element()
            }))
            .when(has_session, |bar| bar.child(self.session_menu(running, cx)))
            .child(
                Button::new("toggle-workbench")
                    .icon(IconName::PanelRight)
                    .ghost()
                    .selected(workbench_visible)
                    .tooltip("Workbench (Ctrl B)")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_workbench(cx))),
            )
    }

    /// Opens the settings dialog on `section`; find and the palette step
    /// aside.
    pub fn open_settings(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.update(cx, |palette, cx| palette.close(cx));
        self.settings
            .update(cx, |settings, cx| settings.open(section, window, cx));
    }

    /// Turns swarm mode off, or asks before turning it on.
    fn toggle_swarm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let on = self.store.read(cx).active_session().is_some_and(|session| {
            session
                .config_options
                .iter()
                .any(|option| option.id == "swarm" && option.current_value == "on")
        });
        if on {
            self.store.act(cx, |store| store.set_option("swarm", "off"));
        } else {
            self.dialog.update(cx, |dialog, cx| {
                dialog.open(DialogKind::ConfirmSwarm, window, cx);
            });
        }
    }

    /// Replaces the stdio transport with one that runs `program`.
    #[cfg(not(target_arch = "wasm32"))]
    fn restart_stdio(&mut self, program: String, cx: &mut Context<Self>) {
        use crate::transport::stdio::{Config, StdioTransport};
        self.transport.close();
        let mut transport = StdioTransport::new(Config {
            program,
            args: vec!["rpc".to_owned()],
        });
        transport.start(self.events.clone());
        self.transport = Box::new(transport);
        self.store.update(cx, |store, cx| {
            store.set_connect(crate::transport::State::Connecting);
            cx.notify();
        });
    }

    /// Whether the setup screen shows: the stdio transport could not
    /// run `kage`.
    #[cfg(not(target_arch = "wasm32"))]
    fn needs_setup(&self, cx: &Context<Self>) -> bool {
        let store = self.store.read(cx);
        store.link().name == "kage rpc"
            && matches!(store.connect(), crate::transport::State::Refused(_))
    }

    /// Moves the vim cursor by `delta` rows.
    fn vim_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.transcript.update(cx, |transcript, cx| {
            let to = vim::step(transcript.vim_cursor(), transcript.row_count(), delta);
            transcript.set_vim_cursor(to, cx);
        });
    }

    /// Puts the vim cursor on the first or the last row.
    fn vim_jump(&mut self, last: bool, cx: &mut Context<Self>) {
        self.transcript.update(cx, |transcript, cx| {
            let to = if last {
                transcript.row_count().checked_sub(1)
            } else {
                Some(0)
            };
            transcript.set_vim_cursor(to, cx);
        });
    }

    /// Opens the `:` line and focuses it.
    fn open_vim_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let line = cx.new(|cx| InputState::new(window, cx));
        line.update(cx, |state, cx| state.focus(window, cx));
        let events = cx.subscribe_in(
            &line,
            window,
            |shell, line, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let text = line.read(cx).value().to_string();
                    shell.close_vim_line(window, cx);
                    shell.run_vim(vim::parse(&text), window, cx);
                }
                InputEvent::Blur => shell.close_vim_line(window, cx),
                _ => {}
            },
        );
        self.vim_line = Some((line, events));
        cx.notify();
    }

    /// Closes the `:` line and hands the keys back to normal mode.
    fn close_vim_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vim_line.take().is_some() {
            window.focus(&self.vim_focus, cx);
            cx.notify();
        }
    }

    /// Runs one `:` command through the action its palette entry or
    /// control uses.
    fn run_vim(&mut self, command: vim::Command, window: &mut Window, cx: &mut Context<Self>) {
        let warn = |shell: &mut Self, text: String, cx: &mut Context<Self>| {
            shell.toasts.update(cx, |toasts, cx| {
                toasts.push(
                    ToastDraft {
                        tone: NoticeTone::Warn,
                        text,
                        action: ToastAction::None,
                    },
                    cx,
                );
            });
        };
        match command {
            vim::Command::Theme(Some(choice)) => {
                self.store
                    .act(cx, |store| store.update_prefs(|prefs| prefs.theme = choice));
                cx.set_global(choice);
                crate::theme::apply_choice(cx, window.appearance());
                window.refresh();
            }
            vim::Command::Theme(None) => {
                warn(self, "Unknown theme; try system, shadow or dawn".into(), cx);
            }
            vim::Command::Model(name) => {
                let wanted = name.to_lowercase();
                let choice = self
                    .store
                    .read(cx)
                    .composer_option("model")
                    .and_then(|option| {
                        option
                            .options
                            .into_iter()
                            .find(|choice| {
                                choice.value.to_lowercase() == wanted
                                    || choice.name.to_lowercase().contains(&wanted)
                            })
                            .filter(|_| !wanted.is_empty())
                    });
                match choice {
                    Some(choice) => {
                        self.store
                            .act(cx, |store| store.set_option("model", &choice.value));
                    }
                    None => warn(self, format!("No model matches \"{name}\""), cx),
                }
            }
            vim::Command::Swarm(on) => {
                let current = self
                    .store
                    .read(cx)
                    .composer_option("swarm")
                    .is_some_and(|option| option.current_value == "on");
                if on != current {
                    self.toggle_swarm(window, cx);
                }
            }
            vim::Command::Plan(on) => {
                self.store.act(cx, |store| {
                    if on {
                        store.enter_plan()
                    } else {
                        store.exit_plan()
                    }
                });
            }
            vim::Command::Goal(goal) => {
                let goal = goal.unwrap_or_default();
                self.store.act(cx, |store| store.set_option("goal", &goal));
            }
            vim::Command::New => self.show_welcome(cx),
            vim::Command::Settings => self.open_settings(Section::General, window, cx),
            vim::Command::Help => self.open_settings(Section::Keyboard, window, cx),
            vim::Command::Compact => {
                self.store.act(cx, Store::compact);
            }
            vim::Command::NoHighlight => {
                self.find.update(cx, |find, cx| find.close(window, cx));
            }
            vim::Command::Quit => {
                self.workbench_visible = false;
                cx.notify();
            }
            vim::Command::Vim(on) => {
                self.store
                    .act(cx, |store| store.update_prefs(|prefs| prefs.vim = on));
            }
            vim::Command::Nothing => {}
            vim::Command::Unknown(word) => {
                warn(self, format!("Not an editor command: {word}"), cx);
            }
        }
    }

    /// The `:` line and the modeline, while vim mode is on.
    fn vim_bar(&self, window: &Window, cx: &Context<Self>) -> Option<impl IntoElement> {
        let store = self.store.read(cx);
        if !store.prefs().vim {
            return None;
        }
        let p = crate::theme::Palette::active(cx);
        let session = store.active_session();
        let composer_focused = self
            .composer
            .read(cx)
            .input()
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let mode = if !store.active_asks().is_empty() {
            vim::Mode::Approval
        } else if composer_focused {
            vim::Mode::Insert
        } else {
            vim::Mode::Normal
        };
        let option = |id: &str| store.composer_option(id).map(|o| o.current_value);
        let model = option("model").map(|model| match option("thinking") {
            Some(level) => format!("{model}@{level}"),
            None => model,
        });
        let transcript = self.transcript.read(cx);
        let line = Modeline {
            mode: Some(mode),
            model,
            swarm: option("swarm").as_deref() == Some("on"),
            plan: store.plan_on(),
            todos: session
                .and_then(crate::views::dock::todos_state)
                .map(|todos| (todos.done, todos.total)),
            path: session.and_then(|session| session.cwd.clone()),
            cursor: transcript
                .vim_cursor()
                .map(|row| (row + 1, transcript.row_count())),
            usage: session.filter(|s| s.usage.size > 0).map(|session| {
                let usage = &session.usage;
                format!(
                    "{}% ({}/{})",
                    (usage.fill() * 100.0).round() as i64,
                    crate::views::agents::tokens(usage.used),
                    crate::views::agents::tokens(usage.size)
                )
            }),
            cost: store
                .active_id()
                .and_then(|id| store.tree_cost(id))
                .map(|cost| format!("{} {:.2}", cost.currency, cost.amount)),
            link: if store.connect().is_connected() {
                store.link().name.to_string()
            } else {
                store.connect().label().to_owned()
            },
        };
        let command_line = self.vim_line.as_ref().map(|(input, _)| {
            h_flex()
                .flex_none()
                .h(px(28.))
                .px(px(12.))
                .items_center()
                .font_family(crate::theme::FONT_MONO)
                .text_size(px(12.5))
                .bg(p.deep)
                .border_t_1()
                .border_color(p.line)
                .on_action(cx.listener(|shell, _: &InputEscape, window, cx| {
                    shell.close_vim_line(window, cx);
                }))
                .on_key_down(
                    cx.listener(|shell, event: &gpui_kit::KeyDownEvent, window, cx| {
                        let empty = shell
                            .vim_line
                            .as_ref()
                            .is_some_and(|(input, _)| input.read(cx).value().is_empty());
                        if event.keystroke.key == "backspace" && empty {
                            shell.close_vim_line(window, cx);
                        }
                    }),
                )
                .child(div().text_color(p.muted).child(":"))
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(input).appearance(false).small()),
                )
        });
        // Normal mode's keys answer on this bar, which holds no text
        // field, so no field inside it can lose a letter to a motion.
        Some(
            v_flex().flex_none().w_full().children(command_line).child(
                div()
                    .id("vim-bar")
                    .test_support()
                    .w_full()
                    .track_focus(&self.vim_focus)
                    .key_context(vim::NORMAL)
                    .on_action(cx.listener(|shell, _: &vim::VimDown, _, cx| shell.vim_move(1, cx)))
                    .on_action(cx.listener(|shell, _: &vim::VimUp, _, cx| shell.vim_move(-1, cx)))
                    .on_action(cx.listener(|shell, _: &vim::VimPageDown, _, cx| {
                        shell.vim_move(vim::PAGE as isize, cx);
                    }))
                    .on_action(cx.listener(|shell, _: &vim::VimPageUp, _, cx| {
                        shell.vim_move(-(vim::PAGE as isize), cx);
                    }))
                    .on_action(
                        cx.listener(|shell, _: &vim::VimTop, _, cx| shell.vim_jump(false, cx)),
                    )
                    .on_action(
                        cx.listener(|shell, _: &vim::VimBottom, _, cx| shell.vim_jump(true, cx)),
                    )
                    .on_action(cx.listener(|shell, _: &vim::VimToggle, _, cx| {
                        shell.transcript.update(cx, |t, cx| t.toggle_vim_row(cx));
                    }))
                    .on_action(cx.listener(|shell, _: &vim::VimOpenAll, _, cx| {
                        shell.transcript.update(cx, |t, cx| t.fold_all(true, cx));
                    }))
                    .on_action(cx.listener(|shell, _: &vim::VimCloseAll, _, cx| {
                        shell.transcript.update(cx, |t, cx| t.fold_all(false, cx));
                    }))
                    .on_action(cx.listener(|shell, _: &vim::VimInsert, window, cx| {
                        let input = shell.composer.read(cx).input().clone();
                        input.update(cx, |state, cx| state.focus(window, cx));
                        cx.notify();
                    }))
                    .on_action(cx.listener(|shell, _: &vim::VimFind, window, cx| {
                        shell.open_find(window, cx);
                    }))
                    .on_action(cx.listener(|shell, _: &vim::VimNext, _, cx| {
                        shell.find.update(cx, |find, cx| find.step(false, cx));
                    }))
                    .on_action(cx.listener(|shell, _: &vim::VimPrev, _, cx| {
                        shell.find.update(cx, |find, cx| find.step(true, cx));
                    }))
                    .on_action(cx.listener(|shell, _: &vim::VimCommand, window, cx| {
                        shell.open_vim_line(window, cx);
                    }))
                    .on_action(cx.listener(|shell, _: &vim::VimEscape, _, cx| {
                        let running = shell
                            .store
                            .read(cx)
                            .active_session()
                            .is_some_and(|session| session.running || session.in_turn);
                        if running {
                            shell.store.act(cx, Store::cancel);
                        }
                    }))
                    .child(line.render(p)),
            ),
        )
    }

    /// Toasts a session out of view that now needs an answer, has a
    /// plan to review, finished its turn or failed; a click opens it.
    fn watch_away(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let active = store.active_id().map(str::to_owned);
        let mut drafts = Vec::new();
        for (id, session) in &store.state().sessions {
            if !session.opened || session.parent.is_some() {
                continue;
            }
            let now = row_state(session);
            let was = self.statuses.insert(id.clone(), now);
            if was.is_none() || was == Some(now) || active.as_deref() == Some(id.as_str()) {
                continue;
            }
            let (tone, what) = match (was, now) {
                (_, RowState::Approve) => (NoticeTone::Info, "needs your approval"),
                (_, RowState::Review) => (NoticeTone::Info, "has a plan to review"),
                (_, RowState::Failed) => (NoticeTone::Error, "turn failed"),
                (Some(RowState::Running), RowState::Idle) => (NoticeTone::Success, "turn finished"),
                _ => continue,
            };
            let title = store.display_title(id);
            drafts.push(ToastDraft {
                tone,
                text: format!("{title}: {what}"),
                action: ToastAction::ActivateSession(id.clone()),
            });
        }
        if drafts.is_empty() {
            return;
        }
        self.toasts.update(cx, |toasts, cx| {
            for draft in drafts {
                toasts.push(draft, cx);
            }
        });
    }

    /// Toasts the messages the store left for the user.
    fn raise_notes(&mut self, cx: &mut Context<Self>) {
        let notes = self.store.update(cx, |store, _| store.take_notes());
        if notes.is_empty() {
            return;
        }
        self.toasts.update(cx, |toasts, cx| {
            for note in notes {
                toasts.push(
                    ToastDraft {
                        tone: note.tone,
                        text: note.text,
                        action: note
                            .undo_archive
                            .map_or(ToastAction::None, ToastAction::Restore),
                    },
                    cx,
                );
            }
        });
    }

    /// Stores the preferences when they moved since the last save.
    fn save_prefs(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if store.prefs_rev() == self.saved_prefs {
            return;
        }
        self.saved_prefs = store.prefs_rev();
        crate::prefs::save(store.prefs());
    }

    /// Opens the rewind dialog for the prompt at item `prompt`, or says
    /// why not while a turn runs.
    fn ask_rewind(&mut self, prompt: usize, window: &mut Window, cx: &mut Context<Self>) {
        let running = self
            .store
            .read(cx)
            .active_session()
            .is_some_and(|session| session.running || session.in_turn);
        if running {
            self.toasts.update(cx, |toasts, cx| {
                toasts.push(
                    ToastDraft {
                        tone: NoticeTone::Warn,
                        text: "Interrupt the running turn before rewinding".to_owned(),
                        action: ToastAction::None,
                    },
                    cx,
                );
            });
            return;
        }
        self.dialog.update(cx, |dialog, cx| {
            dialog.open(DialogKind::Rewind(prompt), window, cx);
        });
    }

    /// Opens the inline title editor on the active session's title.
    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self
            .store
            .read(cx)
            .active_session()
            .map(|session| session.title.clone().unwrap_or_default())
        else {
            return;
        };
        let editor = cx.new(|cx| InputState::new(window, cx));
        editor.update(cx, |state, cx| state.focus(window, cx));
        let pending = Deferred::new();
        pending.set(title, |_| {});
        let events = cx.subscribe_in(
            &editor,
            window,
            |shell, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    shell.finish_rename(true, window, cx);
                }
                _ => {}
            },
        );
        self.rename = Some(Rename {
            editor,
            title: pending,
            _events: events,
        });
        cx.notify();
    }

    /// Closes the title editor, saving its text when `save` is set.
    fn finish_rename(&mut self, save: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        if save && rename.title.is_laid_out() {
            let title = rename.editor.read(cx).value().to_string();
            self.store.act(cx, |store| store.rename(&title));
        }
        let input = self.composer.read(cx).input().clone();
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// The session title in the crumbs: the inline editor while
    /// renaming, else the title, which a double-click starts renaming.
    fn title_crumb(&self, title: String, cx: &Context<Self>) -> AnyElement {
        let p = crate::theme::Palette::active(cx);
        if let Some(rename) = &self.rename {
            let laid_out = rename.title.laid_out().flag();
            let release = cx.entity().downgrade();
            return div()
                .min_w(px(260.))
                .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                    this.finish_rename(false, window, cx);
                }))
                .on_prepaint(move |_, _, cx| {
                    if !laid_out.replace(true) {
                        let _ = release.update(cx, |_, cx| cx.notify());
                    }
                })
                .child(Input::new(&rename.editor).small())
                .into_any_element();
        }
        div()
            .id("topbar-title")
            .min_w_0()
            .truncate()
            .text_color(p.ink_strong)
            .tooltip(|window, cx| Tooltip::new("Double-click to rename").build(window, cx))
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                if event.click_count() == 2 {
                    this.start_rename(window, cx);
                }
            }))
            .child(title)
            .into_any_element()
    }

    /// The session actions behind the topbar's ellipsis.
    fn session_menu(&self, running: bool, cx: &Context<Self>) -> impl IntoElement {
        let store = self.store.clone();
        let pinned = self
            .store
            .read(cx)
            .active_id()
            .is_some_and(|id| self.store.read(cx).prefs().pinned.contains(id));
        let shell = cx.entity();
        Button::new("session-menu")
            .icon(IconName::Ellipsis)
            .ghost()
            .tooltip("Session actions")
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let act = |f: fn(&mut Store) -> bool| {
                    let store = store.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        store.act(cx, f);
                    }
                };
                let copy_id = store.clone();
                let close = store.clone();
                let pin = store.clone();
                let archive = store.clone();
                let shell = shell.clone();
                menu.min_w(px(220.))
                    .item(
                        PopupMenuItem::new("Rename")
                            .icon(IconName::Pencil)
                            .on_click(move |_, window, cx| {
                                shell.update(cx, |shell, cx| shell.start_rename(window, cx));
                            }),
                    )
                    .item(
                        PopupMenuItem::new(if pinned { "Unpin" } else { "Pin" })
                            .icon(if pinned {
                                IconName::PinOff
                            } else {
                                IconName::Pin
                            })
                            .on_click(move |_, _, cx| {
                                pin.act(cx, |store| {
                                    if let Some(id) = store.active_id().map(str::to_owned) {
                                        store.toggle_pin(&id);
                                    }
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new("Fork session")
                            .icon(IconName::GitFork)
                            .on_click(act(|store| store.fork(None))),
                    )
                    .item(
                        PopupMenuItem::new("Compact now")
                            .icon(IconName::Layers)
                            .disabled(running)
                            .on_click(act(Store::compact)),
                    )
                    .item(
                        PopupMenuItem::new("Export markdown")
                            .icon(IconName::ExternalLink)
                            .on_click(act(Store::export)),
                    )
                    .item(
                        PopupMenuItem::new("Copy session ID")
                            .icon(IconName::Copy)
                            .on_click(move |_, _, cx| {
                                if let Some(id) = copy_id.read(cx).active_id() {
                                    cx.write_to_clipboard(ClipboardItem::new_string(id.to_owned()));
                                }
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new("Close session")
                            .icon(IconName::X)
                            .on_click(move |_, _, cx| {
                                close.act(cx, Store::close_active);
                            }),
                    )
                    .item(
                        PopupMenuItem::new("Archive")
                            .icon(IconName::Archive)
                            .on_click(move |_, _, cx| {
                                archive.act(cx, |store| {
                                    if let Some(id) = store.active_id().map(str::to_owned) {
                                        store.archive(&id);
                                    }
                                });
                            }),
                    )
            })
    }

    /// The centered content column: the transcript scroller, or the
    /// welcome state, constrained to the design's content width and
    /// horizontally centered.
    ///
    /// Without a session the column takes no width of its own: the
    /// welcome pane centres its own 728px block inside the pane's 24px
    /// padding, and capping the column here as well would take the
    /// padding out of that 728 twice.
    fn content_column(&self, cx: &Context<Self>) -> impl IntoElement {
        let has_session = self.store.read(cx).active_session().is_some();
        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .relative()
            .flex()
            .justify_center()
            .when(has_session, |column| column.px(px(SP_8)))
            // The rail hangs on the pane's right edge, as the design
            // has it, clear of the centered column.
            .when(has_session, |column| column.child(self.rail.clone()))
            .child(
                div()
                    .w_full()
                    .when(has_session, |column| column.max_w(px(CONTENT_W)))
                    .h_full()
                    .min_h_0()
                    .when(has_session, |column| column.child(self.transcript.clone()))
                    .when(!has_session, |column| {
                        column.items_center().child(self.welcome.clone())
                    }),
            )
    }

    /// The bottom band: dock, approval card and composer in a
    /// centered content-width column inside the design's padding. Only a
    /// session has one; without a session the composer lives in the
    /// welcome column and the dock is not up yet.
    fn bottom_band(&self, cx: &Context<Self>) -> impl IntoElement {
        let has_session = self.store.read(cx).active_session().is_some();
        div()
            .w_full()
            .flex_none()
            .px(px(SP_8))
            .pb(px(SP_6))
            .flex()
            .justify_center()
            .when(!has_session, |band| band.h(px(0.)))
            .when(has_session, |band| {
                band.child(
                    v_flex()
                        .w_full()
                        .max_w(px(CONTENT_W))
                        .child(self.dock.clone())
                        .child(self.approval.clone())
                        .child(self.composer_row(cx)),
                )
            })
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(rename) = &self.rename {
            let editor = rename.editor.clone();
            rename.title.flush(|title| {
                editor.update(cx, |state, cx| {
                    state.set_value(title, window, cx);
                    state.select_all(window, cx);
                });
            });
        }
        {
            let store = self.store.read(cx);
            let answered = store.state().agent.is_some() || store.state().capabilities.is_some();
            self.splash.follow(store.connect(), answered);
        }
        let theme = cx.theme().colors;
        let sidebar_visible = self.sidebar_visible;
        let side_float = self.side_float;
        let workbench_visible = self.workbench_visible;
        let p = crate::theme::Palette::active(cx);
        // The web client's breakpoints: under 1180px the workbench
        // overlays instead of taking a column, under 860px the sidebar
        // does the same and the welcome cards stack.
        let width = window.viewport_size().width;
        self.viewport_width = width.into();
        let mid = width <= px(1180.);
        let narrow = width <= px(860.);
        v_flex()
            .size_full()
            .relative()
            .bg(theme.background)
            .text_color(theme.foreground)
            // The family is named on the root so every element below it
            // inherits it. Without this the shell's own text resolves
            // `.SystemUIFont`, which the web cannot load at all: the
            // render then panics on the first line it lays out.
            .font_family(cx.theme().font_family.clone())
            .on_action(cx.listener(|shell, _: &NewSession, _, cx| shell.show_welcome(cx)))
            .on_action(cx.listener(|shell, _: &ToggleSidebar, _, cx| shell.toggle_sidebar(cx)))
            .on_action(cx.listener(|shell, _: &ToggleWorkbench, _, cx| shell.toggle_workbench(cx)))
            .on_action(cx.listener(|shell, _: &SendPrompt, window, cx| {
                shell.send_composer(window, cx);
            }))
            .on_action(cx.listener(|shell, _: &OpenFind, window, cx| shell.open_find(window, cx)))
            .on_action(cx.listener(|shell, _: &OpenSettings, window, cx| {
                shell.open_settings(Section::General, window, cx);
            }))
            .on_action(cx.listener(|shell, _: &OpenProviders, window, cx| {
                shell.open_settings(Section::Providers, window, cx);
            }))
            .on_action(cx.listener(|shell, _: &VimLeaveInsert, window, cx| {
                if shell.store.read(cx).prefs().vim {
                    window.focus(&shell.vim_focus, cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|shell, _: &ToggleSwarm, window, cx| {
                shell.toggle_swarm(window, cx);
            }))
            .on_action(cx.listener(|shell, _: &TogglePlan, _, cx| {
                shell.store.act(cx, |store| {
                    if store.plan_on() {
                        store.exit_plan()
                    } else {
                        store.enter_plan()
                    }
                });
            }))
            .on_action(cx.listener(|shell, _: &SetGoal, window, cx| {
                shell.dialog.update(cx, |dialog, cx| {
                    dialog.open(DialogKind::Goal, window, cx);
                });
            }))
            .on_action(cx.listener(|shell, _: &ShowAgents, _, cx| {
                shell.workbench_visible = true;
                shell
                    .workbench
                    .update(cx, |workbench, cx| workbench.open_agents(cx));
                cx.notify();
            }))
            .on_action(cx.listener(|shell, _: &ReviewChanges, _, cx| {
                shell.workbench_visible = true;
                shell
                    .workbench
                    .update(cx, |workbench, cx| workbench.open_changes(cx));
                cx.notify();
            }))
            .on_action(cx.listener(|shell, _: &OpenPalette, window, cx| {
                shell.open_palette(window, cx);
            }))
            .child(
                // Every panel stays in the group at every width: the group
                // keys its sizes by position, so a column that joined later
                // would take a neighbour's size.
                h_resizable("kage-shell")
                    .child(
                        resizable_panel()
                            .size(px(SIDE_W))
                            .size_range(px(160.)..Pixels::MAX)
                            .flex_none()
                            .visible(sidebar_visible && !narrow)
                            .child(self.sidebar.clone()),
                    )
                    .child(
                        // The side panels grow without a cap, so the chat
                        // keeps a floor of its own to shrink to.
                        resizable_panel()
                            .size_range(px(CHAT_MIN_W)..Pixels::MAX)
                            .child(v_flex().size_full().min_h_0().child(self.topbar(cx)).map(
                                |panel| {
                                    #[cfg(not(target_arch = "wasm32"))]
                                    if self.needs_setup(cx) {
                                        return panel.child(
                                            div().flex_1().min_h_0().child(self.setup.clone()),
                                        );
                                    }
                                    panel
                                        .child(self.find.clone())
                                        .child(self.content_column(cx))
                                        .child(self.bottom_band(cx))
                                        .children(self.vim_bar(window, cx))
                                },
                            )),
                    )
                    .child(
                        resizable_panel()
                            .size(px(WORKBENCH_W))
                            .size_range(px(200.)..Pixels::MAX)
                            .flex_none()
                            .visible(workbench_visible && !mid)
                            .child(self.workbench.clone()),
                    ),
            )
            .when(narrow && side_float, |shell| {
                shell.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(SIDE_W))
                        .occlude()
                        .shadow(p.shadow_2.clone())
                        .child(self.sidebar.clone()),
                )
            })
            .when(mid && workbench_visible, |shell| {
                shell.child(
                    div()
                        .absolute()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(px(WORKBENCH_W))
                        .occlude()
                        .shadow(p.shadow_2.clone())
                        .child(self.workbench.clone()),
                )
            })
            .child(self.palette.clone())
            .child(self.dialog.clone())
            .child(self.settings.clone())
            .child(self.toasts.clone())
            .children(self.splash.element(window, p))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{ElementId, TestAppContext, VisualTestContext, Window, px};

    use super::{Shell, ShellArgs, project_name};
    use crate::transport::EventSender;

    #[test]
    fn project_names_take_the_last_path_segment() {
        assert_eq!(project_name(Some("/home/u/dev/kage")), "kage");
        assert_eq!(project_name(Some("/")), "local", "a bare root has no name");
        assert_eq!(project_name(None), "local");
    }

    /// A transport that never answers, so the shell stays in the state
    /// with no session: the welcome pane's state.
    struct Silent;

    impl crate::transport::Transport for Silent {
        fn link(&self) -> crate::transport::Link {
            crate::transport::Link::serve("ws://silent")
        }
        fn start(&mut self, _events: EventSender) {}
        fn send(&self, _frame: kage_client::Frame) {}
        fn close(&self) {}
    }

    fn args() -> ShellArgs {
        ShellArgs {
            transport: Box::new(Silent),
            replay: false,
            stream: false,
            prefs: crate::prefs::Prefs::default(),
        }
    }

    /// Renders the shell with no session and returns the pane the test
    /// measures in.
    fn shell_without_session(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(gpui_kit::init);
        let (_shell, visual) =
            cx.add_window_view(|window: &mut Window, cx| Shell::new(args(), window, cx));
        visual
    }

    /// The shell itself must not cap the welcome column, because the
    /// welcome pane already centres its own 728px block inside the
    /// pane's 24px padding. Capping here too takes that padding out of
    /// the 728 twice and the composer measures 680. The welcome pane's
    /// own tests cannot see this: they mount the pane directly, so the
    /// shell's column is not in the tree.
    #[gpui_kit::test]
    fn the_welcome_composer_measures_the_design_column_through_the_shell(cx: &mut TestAppContext) {
        let visual = shell_without_session(cx);
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let composer = visual.update(|window, _| {
            window
                .find(ElementId::Name("composer".into()))
                .bounds()
                .size
                .width
        });
        assert_eq!(
            composer,
            px(crate::theme::WELCOME_W),
            "the composer reaches the design's welcome width through the shell"
        );
    }

    /// The browser starts the canvas small and grows it once the page
    /// lays out, so the shell first renders without its side columns.
    /// The sidebar must still open at the design's width afterwards.
    #[gpui_kit::test]
    fn the_sidebar_keeps_its_width_after_a_narrow_start(cx: &mut TestAppContext) {
        let visual = shell_without_session(cx);
        for width in [300., 1024., 1440.] {
            visual.simulate_resize(gpui_kit::size(px(width), px(900.)));
            for _ in 0..3 {
                visual.update(|window, cx| window.draw(cx).clear(cx));
            }
        }
        let sidebar = visual.update(|window, _| {
            window
                .find(ElementId::Name("sidebar".into()))
                .bounds()
                .size
                .width
        });
        assert_eq!(sidebar, px(crate::theme::SIDE_W));
    }

    /// The keys reach the shell through the bindings both builds share:
    /// Ctrl+K opens the palette, Ctrl+Enter sends the composer and
    /// Ctrl+F opens find.
    #[gpui_kit::test]
    fn the_shared_bindings_reach_the_shell(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(super::key_bindings());
        });
        let (shell, visual) =
            cx.add_window_view(|window: &mut Window, cx| Shell::new(args(), window, cx));
        visual.update(|_, cx| super::route_actions(cx));
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.simulate_keystrokes("ctrl-k");
        let open = visual.update(|_, cx| shell.read(cx).palette.read(cx).is_open());
        assert!(open, "Ctrl+K opens the palette");
        visual.simulate_keystrokes("escape");
        visual.simulate_input("hi");
        visual.simulate_keystrokes("ctrl-enter");
        let pending = visual.update(|_, cx| shell.read(cx).store.read(cx).pending_prompt());
        assert!(pending, "Ctrl+Enter sends the welcome prompt");
        visual.simulate_keystrokes("ctrl-f");
        let find = visual.update(|_, cx| shell.read(cx).find.read(cx).is_open());
        assert!(find, "Ctrl+F opens find even from the composer");
        visual.simulate_keystrokes("escape");
        visual.simulate_keystrokes("ctrl-b");
        let workbench = visual.update(|_, cx| shell.read(cx).workbench_visible);
        assert!(workbench, "Ctrl+B opens the workbench");
    }

    /// Opens a shell with `vim` set, the keys bound and the actions
    /// routed, and draws it once.
    fn keyed_shell(
        cx: &mut TestAppContext,
        vim: bool,
    ) -> (gpui_kit::Entity<Shell>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(super::key_bindings());
        });
        let mut args = args();
        args.prefs.vim = vim;
        let (shell, visual) =
            cx.add_window_view(move |window: &mut Window, cx| Shell::new(args, window, cx));
        visual.update(|_, cx| super::route_actions(cx));
        visual.update(|window, cx| window.draw(cx).clear(cx));
        (shell, visual)
    }

    /// The composer's text as typed.
    fn composer_text(shell: &gpui_kit::Entity<Shell>, visual: &mut VisualTestContext) -> String {
        visual.update(|_, cx| {
            shell
                .read(cx)
                .composer
                .read(cx)
                .input()
                .read(cx)
                .value()
                .to_string()
        })
    }

    #[gpui_kit::test]
    fn with_vim_off_nothing_of_it_mounts_and_no_letter_is_taken(cx: &mut TestAppContext) {
        let (shell, visual) = keyed_shell(cx, false);
        assert!(
            visual.update(|window, _| window.try_find("vim-bar").is_none()),
            "no modeline without vim mode"
        );
        visual.simulate_input("jank");
        assert_eq!(composer_text(&shell, visual), "jank");
    }

    #[gpui_kit::test]
    fn with_vim_on_letters_still_type_and_esc_then_q_closes_the_workbench(cx: &mut TestAppContext) {
        let (shell, visual) = keyed_shell(cx, true);
        assert!(visual.update(|window, _| window.try_find("vim-bar").is_some()));
        visual.simulate_input("jank");
        assert_eq!(
            composer_text(&shell, visual),
            "jank",
            "insert mode types every letter, motions included"
        );
        visual.simulate_keystrokes("ctrl-b");
        assert!(visual.update(|_, cx| shell.read(cx).workbench_visible));
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.simulate_keystrokes("shift-;");
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            visual.update(|_, cx| shell.read(cx).vim_line.is_some()),
            ": opens the command line from normal mode"
        );
        visual.simulate_input("q");
        visual.simulate_keystrokes("enter");
        assert!(
            visual.update(|_, cx| !shell.read(cx).workbench_visible),
            ":q closes the workbench"
        );
        assert!(visual.update(|_, cx| shell.read(cx).vim_line.is_none()));
    }

    /// A stdio transport that cannot find `kage`.
    struct Missing;

    impl crate::transport::Transport for Missing {
        fn link(&self) -> crate::transport::Link {
            crate::transport::Link {
                name: "kage rpc",
                detail: "stdio".to_owned(),
            }
        }
        fn start(&mut self, events: EventSender) {
            let _ = events.try_send(crate::transport::Event::State(
                crate::transport::State::Refused("no kage on PATH".into()),
            ));
        }
        fn send(&self, _frame: kage_client::Frame) {}
        fn close(&self) {}
    }

    #[gpui_kit::test]
    fn a_missing_kage_shows_the_setup_screen_in_place_of_the_chat(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let mut args = args();
        args.transport = Box::new(Missing);
        let (_shell, visual) =
            cx.add_window_view(move |window: &mut Window, cx| Shell::new(args, window, cx));
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert!(visual.update(|window, _| window.try_find("setup").is_some()));
        assert!(
            visual.update(|window, _| window.try_find("composer").is_none()),
            "the chat steps aside"
        );
    }
}
