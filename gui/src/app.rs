//! The app shell: three resizable panels, the key router, and the
//! wiring between a transport and the store.
//!
//! The shell owns the transport and the composer; the panels are thin
//! views over the store. Keys route here: ctrl-n opens a session,
//! ctrl-b toggles the workbench, ctrl-\ toggles the sidebar, ctrl-q
//! quits, ctrl-enter sends the composer.

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::base::Selectable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Sizable as _, h_flex, h_resizable, resizable_panel, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Window, div, px,
};

use crate::store::{Command, Store};
use crate::theme::{CONTENT_W, PANEL_HEAD_H, SIDE_W, SP_4, SP_6, SP_8};
use crate::transport::{Event, Transport};
use crate::views::chrome::{
    FindBar, FindEvent, NoticeWatch, PaletteView, Toasts, WelcomeView, toasts_for_changes,
};
use crate::views::{
    ApprovalCard, ComposerView, DockEvent, DockRow, SidebarView, TranscriptView, WorkbenchView,
};
use kage_client::{Change, Frame};

/// The prompt the replay transcript was recorded with.
const REPLAY_PROMPT: &str = "fix the null check";

/// The workbench panel's open width, matching the web client's
/// default workbench width.
const WORKBENCH_W: f32 = 460.0;

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
    sidebar_visible: bool,
    workbench_visible: bool,
    streamed: usize,
}

impl Shell {
    /// Builds the shell, starts the transport, and pumps its events.
    pub fn new(mut args: ShellArgs, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cwd = working_dir();
        let store = cx.new(|_| Store::new(cwd, args.replay));
        let sidebar = cx.new(|_| SidebarView::new(store.clone()));
        let composer = cx.new(|cx| ComposerView::new(store.clone(), window, cx));
        let input = composer.read(cx).input().clone();
        let transcript = cx.new(|cx| TranscriptView::new(store.clone(), input.clone(), cx));
        let workbench = cx.new(|_| WorkbenchView::new(store.clone()));
        let dock = cx.new(|cx| DockRow::new(store.clone(), window, cx));
        let approval = cx.new(|cx| ApprovalCard::new(store.clone(), window, cx));
        let find = cx.new(|cx| FindBar::new(store.clone(), transcript.clone(), window, cx));
        let palette = cx.new(|cx| PaletteView::new(store.clone(), input.clone(), window, cx));
        let toasts = cx.new(|_| Toasts::new(store.clone()));
        let welcome = cx.new(|cx| WelcomeView::new(store.clone(), input.clone(), window, cx));

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

        let (events, incoming) = async_channel::unbounded();
        args.transport.start(events);

        cx.observe(&store, |shell, _, cx| shell.flush_outgoing(cx))
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
            sidebar_visible: true,
            workbench_visible: true,
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
        let commands = self.store.update(cx, |store, _| store.take_commands());
        if commands.is_empty() {
            return;
        }
        self.store.update(cx, |store, _| {
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

    /// Opens a session through the store.
    pub fn open_session(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.new_session();
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
        if self.store.read(cx).state().open_asks().is_empty() {
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
        self.sidebar_visible = !self.sidebar_visible;
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

    /// The composer row under the transcript.
    fn composer_row(&self, _cx: &Context<Self>) -> impl IntoElement {
        div().w_full().child(self.composer.clone())
    }

    /// The 48px panel head over the content: the restore-sidebar
    /// button when the sidebar is hidden and the workbench toggle at
    /// the far end, as in the web client's topbar.
    fn topbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let sidebar_visible = self.sidebar_visible;
        let workbench_visible = self.workbench_visible;
        h_flex()
            .h(px(PANEL_HEAD_H))
            .flex_none()
            .pl(px(14.))
            .pr(px(10.))
            .gap(px(SP_4))
            .when(!sidebar_visible, |bar| {
                bar.child(
                    Button::new("show-sidebar")
                        .icon(IconName::PanelLeft)
                        .xsmall()
                        .ghost()
                        .tooltip("Show sidebar (Ctrl \\)")
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                )
            })
            .child(div().flex_1())
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
                    .max_w(px(CONTENT_W))
                    .h_full()
                    .min_h_0()
                    .when(has_session, |column| column.child(self.transcript.clone()))
                    .when(!has_session, |column| column.child(self.welcome.clone())),
            )
    }

    /// The bottom band: dock, approval card and composer in a
    /// centered content-width column inside the design's padding.
    fn bottom_band(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .flex_none()
            .px(px(SP_8))
            .pb(px(SP_6))
            .flex()
            .justify_center()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(CONTENT_W))
                    .child(self.dock.clone())
                    .child(self.approval.clone())
                    .child(self.composer_row(cx)),
            )
    }

    /// The status bar under the panels.
    fn status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let store = self.store.read(cx);
        let state = store.state();
        let left = SharedString::from(format!(
            "{} | {} session(s) | {} recorded",
            store.connect().label(),
            state.sessions.len(),
            state.directory.len(),
        ));
        div()
            .h(px(26.))
            .px_3()
            .flex()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(12.))
            .text_color(theme.muted_foreground)
            .child(left)
            .child("ctrl-n new | ctrl-f find | ctrl-k palette | ctrl-b workbench | ctrl-\\ sidebar | ctrl-q quit")
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let sidebar_visible = self.sidebar_visible;
        let workbench_visible = self.workbench_visible;
        let has_session = self.store.read(cx).active_session().is_some();
        v_flex()
            .size_full()
            .relative()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                h_resizable("kage-shell")
                    .child(
                        resizable_panel()
                            .size(px(SIDE_W))
                            .size_range(px(160.)..px(420.))
                            .flex_none()
                            .visible(sidebar_visible)
                            .child(self.sidebar.clone()),
                    )
                    .child(
                        resizable_panel().child(
                            v_flex()
                                .size_full()
                                .min_h_0()
                                .when(has_session, |column| column.child(self.topbar(cx)))
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
                            .visible(workbench_visible)
                            .child(self.workbench.clone()),
                    ),
            )
            .child(self.status_bar(cx))
            .child(self.palette.clone())
            .child(self.toasts.clone())
    }
}
