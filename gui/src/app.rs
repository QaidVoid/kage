//! The app shell: three resizable panels, the key router, and the
//! wiring between a transport and the store.
//!
//! The shell owns the transport and the composer; the panels are thin
//! views over the store. Keys route here: ctrl-n opens a session,
//! ctrl-b toggles the workbench, ctrl-\ toggles the sidebar, ctrl-q
//! quits, ctrl-enter sends the composer.

use std::time::Duration;

use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{h_flex, h_resizable, resizable_panel, v_flex};
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Context, Entity, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, div, px,
};

use crate::store::{Command, Store};
use crate::transport::{Event, Transport};
use crate::views::{SidebarView, TranscriptView, WorkbenchView};
use kage_client::Frame;

/// The prompt the replay transcript was recorded with.
const REPLAY_PROMPT: &str = "fix the null check";

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
    [Quit, NewSession, ToggleSidebar, ToggleWorkbench, SendPrompt]
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
    composer: Entity<TextareaState>,
    sidebar: Entity<SidebarView>,
    transcript: Entity<TranscriptView>,
    workbench: Entity<WorkbenchView>,
    handle: AnyWindowHandle,
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
        let transcript = cx.new(|cx| TranscriptView::new(store.clone(), cx));
        let workbench = cx.new(|_| WorkbenchView::new(store.clone()));
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder("Message the agent; ctrl-enter sends")
        });
        composer.update(cx, |state, cx| state.focus(window, cx));

        let handle = window.window_handle();
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
            handle,
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
                self.store.update(cx, |store, cx| {
                    store.absorb(frame);
                    cx.notify();
                });
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

    /// Sends the composer text to the active session and clears the
    /// composer when it was accepted.
    pub fn send_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let sent = self.store.update(cx, |store, cx| {
            let sent = store.prompt(&text);
            cx.notify();
            sent
        });
        if sent {
            self.composer.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
            cx.notify();
        }
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
    fn composer_row(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let handle = self.handle;
        v_flex()
            .border_t_1()
            .border_color(theme.border)
            .p_3()
            .gap_2()
            .child(Textarea::new(&self.composer).h(px(96.)))
            .child(
                h_flex().justify_end().child(
                    Button::new("send")
                        .label("Send (ctrl-enter)")
                        .on_click(move |_, _, cx| {
                            let _ = handle.update(cx, |root, window, cx| {
                                let Ok(shell) = root.downcast::<Shell>() else {
                                    return;
                                };
                                shell.update(cx, |shell, cx| shell.send_composer(window, cx));
                            });
                        }),
                ),
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
            .child("ctrl-n new | ctrl-b workbench | ctrl-\\ sidebar | ctrl-q quit")
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let sidebar_visible = self.sidebar_visible;
        let workbench_visible = self.workbench_visible;
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                h_resizable("kage-shell")
                    .child(
                        resizable_panel()
                            .size(px(240.))
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
                                .child(self.transcript.clone())
                                .child(self.composer_row(cx)),
                        ),
                    )
                    .child(
                        resizable_panel()
                            .size(px(300.))
                            .size_range(px(200.)..px(520.))
                            .flex_none()
                            .visible(workbench_visible)
                            .child(self.workbench.clone()),
                    ),
            )
            .child(self.status_bar(cx))
    }
}
