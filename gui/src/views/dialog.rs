//! The shell's modal dialogs: setting a goal, confirming swarm mode and
//! rewinding a session.
//!
//! One view the shell mounts over everything; it draws nothing while no
//! dialog is open. A dialog closes on Esc, on a click on the scrim, and
//! after its action.

use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use gpui_kit::prelude::FluentBuilder as _;
use kage_client::{Session, TranscriptItem, unquote_and_trim};

use crate::store::{Store, StoreHandle as _};
use crate::theme::{FONT_MONO, FS_SM, FS_XS, Palette, R_FULL, R_MD};
use crate::views::deferred::Deferred;
use crate::views::kit::{self, BtnTone};
use crate::views::workbench::{ChangeEntry, change_entries};

gpui_kit::actions!(kage_desktop, [DialogClose]);

/// The dialogs the shell can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    /// Set or clear the session goal.
    Goal,
    /// Confirm turning swarm mode on.
    ConfirmSwarm,
    /// Rewind the active session to before the prompt at this item.
    Rewind(usize),
    /// Pick the directory the next session opens in, by path.
    OpenFolder,
}

/// What a rewind to before a prompt drops from the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RewindPreview {
    /// The prompt the rewind goes back to.
    pub prompt: String,
    /// The prompts from it on.
    pub turns: usize,
    /// The transcript items from it on, turn boundaries aside.
    pub entries: usize,
    /// The tool calls among them.
    pub tools: usize,
    /// The subagents those calls started.
    pub agents: usize,
    /// The files the dropped calls and their subagents changed.
    pub files: Vec<ChangeEntry>,
}

/// What rewinding `session` to before the prompt at item `prompt`
/// drops, or `None` when that item is no prompt.
pub(crate) fn rewind_preview(
    store: &Store,
    session: &Session,
    prompt: usize,
) -> Option<RewindPreview> {
    let text = session.prompt_ref(prompt)?.text.unwrap_or_default();
    let gone = &session.items[prompt..];
    let calls: Vec<&str> = gone
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ToolCall(call) => Some(call.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    let mut files = change_entries(gone);
    let mut agents = 0;
    for (id, agent) in &session.agents {
        if !agent
            .tool_call_id
            .as_deref()
            .is_some_and(|call| calls.contains(&call))
        {
            continue;
        }
        agents += 1;
        let Some(child) = store.state().session(id) else {
            continue;
        };
        for mut entry in change_entries(&child.items) {
            if let Some(known) = files.iter_mut().find(|known| known.path == entry.path) {
                known.add += entry.add;
                known.del += entry.del;
                continue;
            }
            entry.by = agent.name.clone();
            files.push(entry);
        }
    }
    Some(RewindPreview {
        prompt: text,
        turns: gone
            .iter()
            .filter(|item| matches!(item, TranscriptItem::User { .. }))
            .count(),
        entries: gone
            .iter()
            .filter(|item| !matches!(item, TranscriptItem::TurnEnd { .. }))
            .count(),
        tools: calls.len(),
        agents,
        files,
    })
}

/// A count chip of the rewind preview.
fn count_chip(label: String, fg: Hsla, bg: Hsla) -> impl IntoElement {
    div()
        .px(px(8.))
        .py(px(2.))
        .rounded(px(R_FULL))
        .bg(bg)
        .text_size(px(FS_XS))
        .text_color(fg)
        .child(label)
}

/// `n` and the noun, `one` when `n` is one and `many` otherwise.
fn counted(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `base` and `name` as one path the engine reads back, in the
/// separator style `base` uses. Trailing separators are trimmed, so
/// the join never doubles them.
fn join_folder(base: &str, name: &str) -> String {
    let base = base.trim_end_matches(['/', '\\']);
    let separator = if base.contains('\\') { '\\' } else { '/' };
    format!("{base}{separator}{name}")
}

/// The dialog layer over the shell.
pub struct DialogView {
    /// Where the focus goes when this closes.
    focus_return: crate::views::kit::FocusReturn,
    store: Entity<Store>,
    open: Option<DialogKind>,
    focus: FocusHandle,
    goal: Entity<InputState>,
    /// The goal text, held until the dialog's field has been laid out;
    /// see [`crate::views::deferred`].
    goal_mirror: Deferred,
    folder: Entity<InputState>,
    /// The folder path, held like the goal text.
    folder_mirror: Deferred,
    /// The folder listing asked for, until it answers.
    browsing: Option<u64>,
    /// The folders of the folder on show, or why they could not be
    /// listed.
    listing: Option<crate::store::FoldersRead>,
}

impl Focusable for DialogView {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DialogView {
    /// A dialog layer over `store`, closed.
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let goal = cx.new(|cx| {
            InputState::new(window, cx).placeholder("e.g. all retry tests pass three runs in a row")
        });
        cx.subscribe_in(&goal, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.save_goal(cx);
            }
        })
        .detach();
        let folder = cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/project"));
        cx.subscribe_in(
            &folder,
            window,
            |this, folder, event: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let typed = unquote_and_trim(&folder.read(cx).value()).to_owned();
                    this.browse(Some(&typed), cx);
                }
            },
        )
        .detach();
        cx.observe_in(&store, window, |this, store, window, cx| {
            let Some(id) = this.browsing else {
                return;
            };
            let Some(read) = store.read(cx).folders(id).cloned() else {
                return;
            };
            this.browsing = None;
            if let Ok(listing) = &read {
                let folder = this.folder.clone();
                this.folder_mirror.set(listing.path.clone(), |text| {
                    folder.update(cx, |state, cx| state.set_value(text, window, cx));
                });
            }
            this.listing = Some(read);
            cx.notify();
        })
        .detach();
        Self {
            focus_return: crate::views::kit::FocusReturn::default(),
            store,
            open: None,
            focus: cx.focus_handle(),
            goal,
            goal_mirror: Deferred::new(),
            folder,
            folder_mirror: Deferred::new(),
            browsing: None,
            listing: None,
        }
    }

    /// Whether a dialog shows.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Shows `kind`, taking the focus. Swarm mode turns on without the
    /// confirmation when the user switched it off.
    pub fn open(&mut self, kind: DialogKind, window: &mut Window, cx: &mut Context<Self>) {
        if kind == DialogKind::ConfirmSwarm && !self.store.read(cx).prefs().confirm_swarm {
            self.enable_swarm(None, cx);
            return;
        }
        self.open = Some(kind);
        self.focus_return.remember(window, cx);
        match kind {
            DialogKind::Goal => {
                let current = self.store.read(cx).active_session().and_then(|session| {
                    session
                        .config_options
                        .iter()
                        .find(|option| option.id == "goal")
                        .map(|option| option.current_value.clone())
                });
                let goal = self.goal.clone();
                self.goal_mirror.set(current.unwrap_or_default(), |text| {
                    goal.update(cx, |state, cx| state.set_value(text, window, cx));
                });
                goal.update(cx, |state, cx| state.focus(window, cx));
            }
            DialogKind::OpenFolder => {
                let current = self.store.read(cx).session_dir().map(str::to_owned);
                let folder = self.folder.clone();
                self.folder_mirror
                    .set(current.clone().unwrap_or_default(), |text| {
                        folder.update(cx, |state, cx| state.set_value(text, window, cx));
                    });
                folder.update(cx, |state, cx| state.focus(window, cx));
                self.listing = None;
                self.browse(current.as_deref(), cx);
            }
            DialogKind::ConfirmSwarm | DialogKind::Rewind(_) => window.focus(&self.focus, cx),
        }
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.open = None;
        self.focus_return.restore(cx);
        cx.notify();
    }

    fn save_goal(&mut self, cx: &mut Context<Self>) {
        let text = self.goal.read(cx).value().trim().to_owned();
        self.store.act(cx, |store| store.set_option("goal", &text));
        self.close(cx);
    }

    /// Lists the folders inside `path` on the engine's machine, the
    /// home folder without one; the answer replaces the listing.
    fn browse(&mut self, path: Option<&str>, cx: &mut Context<Self>) {
        self.browsing = Some(self.store.act(cx, |store| store.ask_folders(path)));
        cx.notify();
    }

    fn save_folder(&mut self, cx: &mut Context<Self>) {
        let path = unquote_and_trim(&self.folder.read(cx).value()).to_owned();
        self.store.act(cx, |store| store.set_project(Some(path)));
        self.close(cx);
    }

    fn enable_swarm(&mut self, permission: Option<String>, cx: &mut Context<Self>) {
        self.store.act(cx, |store| {
            if let Some(permission) = &permission {
                store.set_permission(permission);
            }
            store.set_option("swarm", "on")
        });
        self.close(cx);
    }

    fn rewind(&mut self, prompt: usize, cx: &mut Context<Self>) {
        self.store.act(cx, |store| store.rewind(prompt));
        self.close(cx);
    }

    fn rewind_body(
        &self,
        prompt: usize,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> (IconName, &'static str, AnyElement, AnyElement) {
        let view = cx.entity();
        let store = self.store.read(cx);
        let session = store.active_session();
        let preview = session.and_then(|session| rewind_preview(store, session, prompt));
        let title = session
            .and_then(|session| session.title.clone())
            .unwrap_or_else(|| "untitled session".to_owned());
        let mut body = v_flex().gap(px(12.));
        if let Some(preview) = &preview {
            body = body
                .child(
                    div()
                        .id("rewind-quote")
                        .max_h(px(96.))
                        .overflow_y_scroll()
                        .px(px(12.))
                        .py(px(10.))
                        .border_l_3()
                        .border_color(pal.accent)
                        .rounded_r(px(R_MD))
                        .bg(pal.surface)
                        .text_size(px(FS_SM))
                        .text_color(pal.ink_strong)
                        .child(SharedString::from(preview.prompt.clone())),
                )
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .child(count_chip(
                            counted(preview.turns, "turn", "turns"),
                            pal.muted,
                            pal.fill,
                        ))
                        .child(count_chip(
                            counted(preview.entries, "entry", "entries"),
                            pal.muted,
                            pal.fill,
                        ))
                        .child(count_chip(
                            counted(preview.tools, "tool call", "tool calls"),
                            pal.muted,
                            pal.fill,
                        ))
                        .when(preview.agents > 0, |chips| {
                            chips.child(count_chip(
                                counted(preview.agents, "agent", "agents"),
                                pal.done,
                                pal.done_soft,
                            ))
                        })
                        .child(if preview.files.is_empty() {
                            count_chip("0 files".to_owned(), pal.muted, pal.fill)
                        } else {
                            count_chip(
                                counted(preview.files.len(), "file", "files"),
                                pal.warn,
                                pal.warn_soft,
                            )
                        }),
                )
                .when(!preview.files.is_empty(), |body| {
                    let mut list = v_flex()
                        .border_1()
                        .border_color(pal.subtle)
                        .rounded(px(12.))
                        .overflow_hidden()
                        .child(
                            h_flex()
                                .gap(px(8.))
                                .px(px(12.))
                                .py(px(8.))
                                .bg(pal.fill)
                                .text_size(px(FS_XS))
                                .text_color(pal.muted)
                                .child(div().flex_1().child("Files stay as they are"))
                                .child("restoring them stays with the TUI rewind plugin"),
                        );
                    for file in &preview.files {
                        list = list.child(
                            h_flex()
                                .gap(px(8.))
                                .px(px(12.))
                                .py(px(6.))
                                .border_t_1()
                                .border_color(pal.subtle)
                                .text_size(px(12.))
                                .child(
                                    Icon::new(if file.created {
                                        IconName::FilePlus
                                    } else {
                                        IconName::File
                                    })
                                    .with_size(px(12.))
                                    .text_color(pal.faint),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(FONT_MONO)
                                        .child(SharedString::from(file.path.clone())),
                                )
                                .child(div().text_color(pal.ok).child(format!("+{}", file.add)))
                                .child(
                                    div().text_color(pal.danger).child(format!("-{}", file.del)),
                                ),
                        );
                    }
                    body.child(list)
                });
        }
        body = body.child(div().text_size(px(13.)).text_color(pal.muted).child(SharedString::from(
            format!(
                "The current state stays as \"{title} (before rewind)\", and this message goes back in the composer."
            ),
        )));
        let cancel = view.clone();
        let foot = h_flex()
            .gap(px(8.))
            .child(
                kit::btn_sm("rewind-cancel", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| cancel.update(cx, |this, cx| this.close(cx)))
                    .child("Cancel"),
            )
            .child(
                kit::btn_sm("rewind-go", BtnTone::Primary, pal)
                    .on_click(move |_, _, cx| view.update(cx, |this, cx| this.rewind(prompt, cx)))
                    .child(Icon::new(IconName::Undo2).with_size(px(12.)))
                    .child("Rewind"),
            )
            .into_any_element();
        (
            IconName::Undo2,
            "Rewind to this message?",
            body.into_any_element(),
            foot,
        )
    }

    /// The advertised name of permission mode `value`, or the value.
    fn mode_name(&self, value: &str, cx: &Context<Self>) -> String {
        self.store
            .read(cx)
            .active_session()
            .and_then(|session| {
                session
                    .config_options
                    .iter()
                    .find(|option| option.id == "mode")?
                    .options
                    .iter()
                    .find(|choice| choice.value == value)
                    .map(|choice| choice.name.clone())
            })
            .unwrap_or_else(|| value.to_owned())
    }

    fn goal_body(
        &self,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> (IconName, &'static str, AnyElement, AnyElement) {
        let view = cx.entity();
        let laid_out = self.goal_mirror.laid_out().flag();
        let release = cx.entity().downgrade();
        let body = v_flex()
            .gap(px(10.))
            .child(div().text_size(px(FS_SM)).text_color(pal.muted).child(
                "The agent keeps working until the goal is met, checking it after each turn.",
            ))
            .child(
                div()
                    .on_prepaint(move |_, _, cx| {
                        laid_out.set(true);
                        let _ = release.update(cx, |_, cx| cx.notify());
                    })
                    .child(kit::input(&self.goal)),
            )
            .into_any_element();
        let cancel = view.clone();
        let foot = h_flex()
            .gap(px(8.))
            .child(
                kit::btn_sm("goal-cancel", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| cancel.update(cx, |this, cx| this.close(cx)))
                    .child("Cancel"),
            )
            .child(
                kit::btn_sm("goal-save", BtnTone::Primary, pal)
                    .on_click(move |_, _, cx| view.update(cx, |this, cx| this.save_goal(cx)))
                    .child("Set goal"),
            )
            .into_any_element();
        (IconName::Target, "Set a goal", body, foot)
    }

    fn folder_body(
        &self,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> (IconName, &'static str, AnyElement, AnyElement) {
        let view = cx.entity();
        let laid_out = self.folder_mirror.laid_out().flag();
        let release = cx.entity().downgrade();
        let listed = self.listing.as_ref().and_then(|read| read.as_ref().ok());
        let parent = listed.and_then(|listing| listing.parent.clone());
        let home = listed.and_then(|listing| listing.home.clone());
        let nav = |id: &'static str, icon: IconName, tip: &'static str, to: Option<String>| {
            let view = view.clone();
            let enabled = to.is_some();
            kit::btn_sm(id, BtnTone::Plain, pal)
                .px(px(7.))
                .when(!enabled, |btn| btn.opacity(0.4))
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
                })
                .on_click(move |_, _, cx| {
                    if let Some(to) = &to {
                        view.update(cx, |this, cx| this.browse(Some(to), cx));
                    }
                })
                .child(Icon::new(icon).with_size(px(14.)))
        };
        let body = v_flex()
            .gap(px(10.))
            .child(
                div()
                    .text_size(px(FS_SM))
                    .text_color(pal.muted)
                    .child("A folder on the machine kage runs on. The next session opens there."),
            )
            .child(
                h_flex()
                    .gap(px(6.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .on_prepaint(move |_, _, cx| {
                                laid_out.set(true);
                                let _ = release.update(cx, |_, cx| cx.notify());
                            })
                            .child(kit::input(&self.folder)),
                    )
                    .child(nav(
                        "folder-up",
                        IconName::FolderUp,
                        "Up one folder",
                        parent,
                    ))
                    .child(nav("folder-home", IconName::House, "Home folder", home)),
            )
            .child(self.folder_list(pal, cx))
            .into_any_element();
        let cancel = view.clone();
        let foot = h_flex()
            .gap(px(8.))
            .child(
                kit::btn_sm("folder-cancel", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| cancel.update(cx, |this, cx| this.close(cx)))
                    .child("Cancel"),
            )
            .child(
                kit::btn_sm("folder-open", BtnTone::Primary, pal)
                    .on_click(move |_, _, cx| view.update(cx, |this, cx| this.save_folder(cx)))
                    .child("Open"),
            )
            .into_any_element();
        (IconName::FolderOpen, "Open folder", body, foot)
    }

    /// The folders inside the folder on show, each opening on a click,
    /// or why they could not be listed.
    fn folder_list(&self, pal: &Palette, cx: &Context<Self>) -> AnyElement {
        let note = |text: String| {
            div()
                .px(px(10.))
                .py(px(8.))
                .text_size(px(FS_SM))
                .text_color(pal.muted)
                .child(SharedString::from(text))
        };
        let list = v_flex()
            .id("folder-list")
            .h(px(260.))
            .overflow_y_scroll()
            .p(px(4.))
            .border_1()
            .border_color(pal.line)
            .rounded(px(R_MD))
            .bg(pal.surface);
        let listing = match &self.listing {
            None => {
                return list
                    .child(note("Listing\u{2026}".to_owned()))
                    .into_any_element();
            }
            Some(Err(why)) => return list.child(note(why.clone())).into_any_element(),
            Some(Ok(listing)) => listing,
        };
        if listing.folders.is_empty() {
            return list
                .child(note("No folders inside.".to_owned()))
                .into_any_element();
        }
        let view = cx.entity();
        let hover = pal.hover;
        let mut list = list.children(listing.folders.iter().enumerate().map(|(n, name)| {
            let view = view.clone();
            let to = join_folder(&listing.path, name);
            h_flex()
                .id(gpui_kit::ElementId::named_usize("folder", n))
                .gap(px(8.))
                .items_center()
                .px(px(8.))
                .py(px(5.))
                .rounded(px(crate::theme::R_SM))
                .cursor_pointer()
                .hover(move |row| row.bg(hover))
                .text_size(px(FS_SM))
                .text_color(pal.ink)
                .on_click(move |_, _, cx| {
                    view.update(cx, |this, cx| this.browse(Some(&to), cx));
                })
                .child(
                    Icon::new(IconName::Folder)
                        .with_size(px(14.))
                        .text_color(pal.muted),
                )
                .child(div().truncate().child(SharedString::from(name.clone())))
        }));
        if listing.truncated {
            list = list.child(note(
                "More folders than shown; type a path to go further.".to_owned(),
            ));
        }
        list.into_any_element()
    }

    fn swarm_body(
        &self,
        pal: &Palette,
        cx: &Context<Self>,
    ) -> (IconName, &'static str, AnyElement, AnyElement) {
        let view = cx.entity();
        let asks_always = self.store.read(cx).permission_mode().as_deref() == Some("ask");
        let default_name = self.mode_name("default", cx);
        let mut body = v_flex().gap(px(10.)).child(
            div()
                .text_size(px(FS_SM))
                .text_color(pal.ink)
                .child("The agent will split work into many parallel sub-agents, one per item, and merge their results."),
        );
        if asks_always {
            body = body.child(
                div()
                    .text_size(px(13.))
                    .text_color(pal.muted)
                    .child(SharedString::from(format!(
                        "You are in {}. Every worker edit would stop for approval, so swarms work best with {default_name}.",
                        self.mode_name("ask", cx)
                    ))),
            );
        }
        let cancel = view.clone();
        let mut foot = h_flex().gap(px(8.)).child(
            kit::btn_sm("swarm-cancel", BtnTone::Plain, pal)
                .on_click(move |_, _, cx| cancel.update(cx, |this, cx| this.close(cx)))
                .child("Cancel"),
        );
        if asks_always {
            let relax = view.clone();
            foot = foot.child(
                kit::btn_sm("swarm-relax", BtnTone::Plain, pal)
                    .on_click(move |_, _, cx| {
                        relax.update(cx, |this, cx| {
                            this.enable_swarm(Some("default".to_owned()), cx)
                        });
                    })
                    .child(SharedString::from(format!("Enable with {default_name}"))),
            );
        }
        foot = foot.child(
            kit::btn_sm("swarm-enable", BtnTone::Primary, pal)
                .on_click(move |_, _, cx| view.update(cx, |this, cx| this.enable_swarm(None, cx)))
                .child("Enable swarm"),
        );
        (
            IconName::Waypoints,
            "Enable swarm mode?",
            body.into_any_element(),
            foot.into_any_element(),
        )
    }
}

impl Render for DialogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let goal = self.goal.clone();
        self.goal_mirror.flush(|text| {
            goal.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        let folder = self.folder.clone();
        self.folder_mirror.flush(|text| {
            folder.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        let Some(kind) = self.open else {
            return div().into_any_element();
        };
        let pal = Palette::active(cx);
        let (icon, title, body, foot) = match kind {
            DialogKind::Goal => self.goal_body(pal, cx),
            DialogKind::ConfirmSwarm => self.swarm_body(pal, cx),
            DialogKind::Rewind(prompt) => self.rewind_body(prompt, pal, cx),
            DialogKind::OpenFolder => self.folder_body(pal, cx),
        };
        let (icon_fg, icon_bg) = match kind {
            DialogKind::Goal | DialogKind::Rewind(_) | DialogKind::OpenFolder => {
                (pal.accent, pal.accent_soft)
            }
            DialogKind::ConfirmSwarm => (pal.done, pal.done_soft),
        };
        let scrim_close = cx.entity();
        let mut card = v_flex()
            .id("dialog")
            .track_focus(&self.focus)
            .key_context("Dialog")
            .on_action(cx.listener(|this, _: &DialogClose, _, cx| this.close(cx)))
            .w(px(match kind {
                DialogKind::Goal | DialogKind::OpenFolder => 480.,
                DialogKind::ConfirmSwarm => 460.,
                DialogKind::Rewind(_) => 560.,
            }))
            .bg(pal.bg)
            .border_1()
            .border_color(pal.line)
            .rounded(px(16.))
            .overflow_hidden()
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                cx.stop_propagation()
            })
            .child(
                h_flex()
                    .gap(px(10.))
                    .items_center()
                    .px(px(18.))
                    .pt(px(16.))
                    .pb(px(8.))
                    .child(
                        div()
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(R_MD))
                            .bg(icon_bg)
                            .text_color(icon_fg)
                            .child(Icon::new(icon).with_size(px(14.))),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(16.))
                            .text_color(pal.ink_strong)
                            .child(title),
                    ),
            )
            .child(div().px(px(18.)).py(px(8.)).child(body))
            .child(
                h_flex()
                    .justify_end()
                    .px(px(18.))
                    .pt(px(12.))
                    .pb(px(16.))
                    .child(foot),
            );
        card.style().box_shadow = Some(pal.shadow_2.clone());
        let scrim = div()
            .id("dialog-scrim")
            .absolute()
            .inset_0()
            .occlude()
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
            .child(card);
        // Above Settings, which can open a dialog.
        gpui_kit::deferred(scrim)
            .with_priority(3)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext, Window};
    use kage_client::{Frame, RequestId};

    use kage_client::wire::{ContentBlock, DiffContent, ToolCallContent, ToolCallStatus, ToolKind};
    use kage_client::{Session, Subagent, ToolCallItem, TranscriptItem};

    use super::{DialogKind, DialogView, rewind_preview};
    use crate::store::{Command, Store};
    use crate::transport::State;

    /// A store with session `s1` in Ask mode and swarm mode off.
    fn asking_store() -> Store {
        let mut store = Store::new("/w", false);
        store.set_connect(State::Connected);
        for command in store.take_commands() {
            if let Command::Handshake { replay_sessions } = command {
                store.handshake(replay_sessions);
            }
        }
        let _ = store.take_outgoing();
        store.absorb(Frame::Success {
            id: RequestId::Number(1),
            result: serde_json::json!({"protocolVersion": 1, "agentCapabilities": {}}),
        });
        let _ = store.take_outgoing();
        store.new_session();
        let opened = store
            .take_outgoing()
            .into_iter()
            .find_map(|frame| match frame {
                Frame::Request { id, method, .. } if method == "session/new" => Some(id),
                _ => None,
            })
            .expect("the session opens");
        store.absorb(Frame::Success {
            id: opened,
            result: serde_json::json!({
                "sessionId": "s1",
                "configOptions": [
                    {"id": "mode", "name": "Mode", "type": "select", "currentValue": "ask",
                     "options": [{"value": "default", "name": "Default"}, {"value": "ask", "name": "Ask"}]},
                    {"id": "swarm", "name": "Swarm", "type": "select", "currentValue": "off",
                     "options": [{"value": "off", "name": "Off"}, {"value": "on", "name": "On"}]},
                ],
            }),
        });
        let _ = store.take_outgoing();
        store
    }

    #[gpui_kit::test]
    fn enabling_swarm_from_ask_relaxes_the_mode_first(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = cx.new(|_| asking_store());
        let (dialog, visual) = cx
            .add_window_view(|window: &mut Window, cx| DialogView::new(store.clone(), window, cx));
        visual.update(|window, cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.open(DialogKind::ConfirmSwarm, window, cx);
                assert!(dialog.is_open());
                dialog.enable_swarm(Some("default".to_owned()), cx);
                assert!(!dialog.is_open(), "the dialog closes after its action");
            });
        });
        let sent: Vec<(String, String)> = visual.update(|_, cx| {
            store.update(cx, |store, _| {
                store
                    .take_outgoing()
                    .into_iter()
                    .filter_map(|frame| match frame {
                        Frame::Request { params, .. } => Some((
                            params["configId"].as_str()?.to_owned(),
                            params["value"].as_str()?.to_owned(),
                        )),
                        _ => None,
                    })
                    .collect()
            })
        });
        assert_eq!(
            sent,
            [
                ("mode".to_owned(), "default".to_owned()),
                ("swarm".to_owned(), "on".to_owned())
            ]
        );
    }

    fn call(id: &str, kind: ToolKind) -> TranscriptItem {
        TranscriptItem::ToolCall(ToolCallItem {
            tool_call_id: id.to_owned(),
            title: "a call".to_owned(),
            kind,
            status: ToolCallStatus::Completed,
            input: None,
            swarm: None,
            content: Vec::new(),
            raw_output: None,
            took_ms: None,
        })
    }

    fn prompt(text: &str) -> TranscriptItem {
        TranscriptItem::User {
            content: vec![ContentBlock::text(text)],
            steered: false,
        }
    }

    #[test]
    fn folder_navigation_joins_in_the_base_s_separator_style() {
        use super::join_folder;
        assert_eq!(join_folder("C:\\work\\proj", "sub"), "C:\\work\\proj\\sub");
        assert_eq!(join_folder("C:/work/proj/", "sub"), "C:/work/proj/sub");
        assert_eq!(
            join_folder("\\\\server\\share\\", "sub"),
            "\\\\server\\share\\sub"
        );
        assert_eq!(join_folder("/home/u", "sub"), "/home/u/sub");
        assert_eq!(join_folder("/home/u/", "sub"), "/home/u/sub");
    }

    #[gpui_kit::test]
    fn a_quoted_and_spaced_folder_saves_unquoted(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = cx.new(|_| asking_store());
        let (dialog, visual) = cx
            .add_window_view(|window: &mut Window, cx| DialogView::new(store.clone(), window, cx));
        visual.update(|window, cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.open(DialogKind::OpenFolder, window, cx);
            });
            let folder = dialog.read(cx).folder.clone();
            folder.update(cx, |state, cx| {
                state.set_value("  \"/opt/my dir\"  ", window, cx)
            });
            dialog.update(cx, |dialog, cx| dialog.save_folder(cx));
        });
        let dir = visual.update(|_, cx| store.read(cx).session_dir().map(str::to_owned));
        assert_eq!(dir.as_deref(), Some("/opt/my dir"));
    }

    #[gpui_kit::test]
    fn a_folder_that_cleans_to_empty_sets_nothing(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let store = cx.new(|_| asking_store());
        let (dialog, visual) = cx
            .add_window_view(|window: &mut Window, cx| DialogView::new(store.clone(), window, cx));
        visual.update(|window, cx| {
            dialog.update(cx, |dialog, cx| {
                dialog.open(DialogKind::OpenFolder, window, cx);
            });
            let folder = dialog.read(cx).folder.clone();
            folder.update(cx, |state, cx| state.set_value("\"\"", window, cx));
            dialog.update(cx, |dialog, cx| dialog.save_folder(cx));
        });
        let dir = visual.update(|_, cx| store.read(cx).session_dir().map(str::to_owned));
        assert_eq!(
            dir.as_deref(),
            Some("/w"),
            "the cleaned-empty folder fell through"
        );
    }

    #[test]
    fn the_rewind_preview_counts_what_follows_the_prompt() {
        let mut session = Session::new("s1");
        let mut edit = call("t2", ToolKind::Edit);
        if let TranscriptItem::ToolCall(item) = &mut edit {
            item.content = vec![ToolCallContent::Diff(DiffContent {
                path: "src/lib.rs".into(),
                old_text: Some("a\n".into()),
                new_text: "b\nc\n".into(),
            })];
        }
        session.items = vec![
            prompt("first"),
            call("t1", ToolKind::Read),
            prompt("second"),
            edit,
            call("t3", ToolKind::Other),
            prompt("third"),
        ];
        session.agents.insert(
            "c1".into(),
            Subagent {
                tool_call_id: Some("t3".into()),
                ..Subagent::default()
            },
        );
        session.agents.insert(
            "c0".into(),
            Subagent {
                tool_call_id: Some("t1".into()),
                ..Subagent::default()
            },
        );
        let store = Store::new("/w", false);

        let preview = rewind_preview(&store, &session, 2).unwrap();
        assert_eq!(preview.prompt, "second");
        assert_eq!(
            (
                preview.turns,
                preview.entries,
                preview.tools,
                preview.agents
            ),
            (2, 4, 2, 1)
        );
        assert_eq!(preview.files.len(), 1);
        assert_eq!(preview.files[0].path, "src/lib.rs");
        assert!(
            rewind_preview(&store, &session, 1).is_none(),
            "a tool call is no prompt"
        );
    }
}
