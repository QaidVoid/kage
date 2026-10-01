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
use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Sizable as _, h_flex, h_resizable, resizable_panel, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext, Context, Entity, InteractiveElement as _, IntoElement, KeyBinding,
    ParentElement as _, Render, SharedString, Styled as _, Window, div, px,
};

use crate::clock::unix_seconds;
use crate::store::{Command, Store, StoreHandle as _};
use crate::theme::{CONTENT_W, FS_SM, FS_XS, PANEL_HEAD_H, R_FULL, SIDE_W, SP_4, SP_6, SP_8};
use crate::transport::{Event, Transport};
use crate::views::chrome::{
    FindBar, FindEvent, NoticeWatch, PaletteView, Toasts, WelcomeView, toasts_for_changes,
};
use crate::views::{
    ApprovalCard, ComposerView, DockEvent, DockRow, SidebarView, TranscriptView, WorkbenchEvent,
    WorkbenchView,
};
use kage_client::{Change, Frame};

/// The prompt the replay transcript was recorded with.
const REPLAY_PROMPT: &str = "fix the null check";

/// The workbench panel's open width, matching the web client's
/// default workbench width.
const WORKBENCH_W: f32 = 460.0;

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
        OpenPalette
    ]
);

/// The shell's key bindings, shared by the native and browser builds.
#[must_use]
pub fn key_bindings() -> Vec<KeyBinding> {
    use crate::views::chrome::{
        FindClose, FindNext, FindPrev, PaletteClose, PaletteDown, PaletteRun, PaletteUp,
    };
    vec![
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
        // Inside the input's own context, so it wins over the toolkit's
        // outdent binding while the composer is focused.
        KeyBinding::new(
            "shift-tab",
            crate::views::composer::CycleMode,
            Some("Input"),
        ),
    ]
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
}

/// The window's root view.
pub struct Shell {
    store: Entity<Store>,
    transport: Box<dyn Transport>,
    composer: Entity<ComposerView>,
    sidebar: Entity<SidebarView>,
    transcript: Entity<TranscriptView>,
    workbench: Entity<WorkbenchView>,
    dock: Entity<DockRow>,
    approval: Entity<ApprovalCard>,
    find: Entity<FindBar>,
    palette: Entity<PaletteView>,
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
    streamed: usize,
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
        let store = cx.new(|_| Store::new(cwd, args.replay).with_link(link));
        let sidebar = cx.new(|_| SidebarView::new(store.clone()));
        let composer = cx.new(|cx| ComposerView::new(store.clone(), window, cx));
        let input = composer.read(cx).input().clone();
        // The other views that write this same textarea wait on the
        // composer's element laying out, because the composer is what
        // renders that element.
        let input_laid_out = composer.read(cx).input_laid_out();
        let transcript = cx.new(|cx| TranscriptView::new(store.clone(), input.clone(), cx));
        let workbench = cx.new(|_| WorkbenchView::new(store.clone()));
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
        args.transport.start(events);

        cx.observe(&store, |shell, _, cx| {
            shell.flush_outgoing(cx);
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
            workbench,
            dock,
            approval,
            find,
            palette,
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
            streamed: 0,
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
        if self.store.read(cx).active_asks().is_empty() {
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
                        .xsmall()
                        .ghost()
                        .tooltip("Show sidebar (Ctrl \\)")
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                )
            })
            .children(session.map(|session| {
                let name = project_name(session.cwd.as_deref());
                let title = session
                    .title
                    .clone()
                    .unwrap_or_else(|| "untitled session".to_owned());
                h_flex()
                    .id("topbar-crumbs")
                    .min_w_0()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(FS_SM))
                    .child(div().flex_none().text_color(p.muted).child(name))
                    .child(div().flex_none().text_color(p.ghost).child("/"))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(p.ink_strong)
                            .child(title),
                    )
                    .into_any_element()
            }))
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
                    .child(
                        Spinner::new()
                            .icon(IconName::LoaderCircle)
                            .color(p.accent)
                            .with_size(px(13.)),
                    )
                    .child(SharedString::from(format!("Working {label}")))
                    .into_any_element()
            }))
            .child(
                Button::new("toggle-workbench")
                    .icon(IconName::PanelRight)
                    .xsmall()
                    .ghost()
                    .selected(workbench_visible)
                    .tooltip("Workbench (Ctrl B)")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_workbench(cx))),
            )
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
            .flex()
            .justify_center()
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
                            .size_range(px(160.)..px(420.))
                            .flex_none()
                            .visible(sidebar_visible && !narrow)
                            .child(self.sidebar.clone()),
                    )
                    .child(
                        resizable_panel().child(
                            v_flex()
                                .size_full()
                                .min_h_0()
                                .child(self.topbar(cx))
                                .child(self.find.clone())
                                .child(self.content_column(cx))
                                .child(self.bottom_band(cx)),
                        ),
                    )
                    .child(
                        resizable_panel()
                            .size(px(WORKBENCH_W))
                            .size_range(px(200.)..px(520.))
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
                        .shadow(p.shadow_2.clone())
                        .child(self.workbench.clone()),
                )
            })
            .child(self.palette.clone())
            .child(self.toasts.clone())
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
    }
}
