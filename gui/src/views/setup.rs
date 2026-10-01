//! The setup screen: what the desktop app shows in place of the chat
//! while it cannot run `kage`.
//!
//! The app spawns `kage rpc` from the user's machine and never bundles
//! it, so a missing binary gets this screen: point at a `kage` you
//! have, or install one and check again. A chosen binary is probed with
//! `--version` first and refused when it is not `kage` or is older than
//! the minimum the gate checks. The web build runs nothing locally and
//! never shows this screen.

use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::gate::MINIMUM_KAGE_VERSION;
use crate::theme::{FONT_MONO, FS_SM, FS_XS, Palette, R_LG, WEIGHT_BOLD, WEIGHT_SEMIBOLD};
use crate::views::deferred::Deferred;
use crate::views::kit::{BtnTone, btn_sm};

/// The ways to install `kage`, with the command or page each names.
const INSTALL: [(&str, &str); 3] = [
    (
        "Cargo",
        "cargo install --locked --git https://github.com/QaidVoid/kage kage-cli",
    ),
    ("Nix", "nix profile install github:QaidVoid/kage"),
    (
        "Release archive",
        "https://github.com/QaidVoid/kage/releases/latest",
    ),
];

/// What the setup screen asks the shell to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupEvent {
    /// Run the `kage` at this path, which passed the probe.
    Use(String),
    /// Look for `kage` on the PATH again.
    Retry,
}

/// Probes `program` with `--version`: the version it reports, or why it
/// cannot be used.
pub fn probe(program: &str) -> Result<String, String> {
    let output = std::process::Command::new(program)
        .arg("--version")
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text
        .trim()
        .strip_prefix("kage ")
        .ok_or_else(|| format!("{program} is not kage"))?
        .to_owned();
    if crate::gate::is_below(&version, MINIMUM_KAGE_VERSION) {
        return Err(format!(
            "That kage is {version}; this app needs {MINIMUM_KAGE_VERSION} or newer."
        ));
    }
    Ok(version)
}

/// The setup screen.
pub struct SetupView {
    path: Entity<InputState>,
    /// The path a Browse picked, written once the field has laid out.
    picked: Deferred,
    /// Why the last chosen path was refused.
    error: Option<String>,
}

impl EventEmitter<SetupEvent> for SetupView {}

impl SetupView {
    /// A setup screen with an empty path field.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/kage"));
        cx.subscribe_in(&path, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.use_path(cx);
            }
        })
        .detach();
        Self {
            path,
            picked: Deferred::new(),
            error: None,
        }
    }

    /// Probes the typed path and hands it to the shell when it passes.
    fn use_path(&mut self, cx: &mut Context<Self>) {
        let program = self.path.read(cx).value().trim().to_owned();
        if program.is_empty() {
            self.error = Some("Enter the path to a kage binary.".to_owned());
            cx.notify();
            return;
        }
        match probe(&program) {
            Ok(_) => {
                self.error = None;
                cx.emit(SetupEvent::Use(program));
            }
            Err(why) => self.error = Some(why),
        }
        cx.notify();
    }

    /// Opens the platform's file picker and fills the field with the
    /// chosen file.
    fn browse(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose kage".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.first() else {
                return;
            };
            let path = path.display().to_string();
            let _ = this.update(cx, |this, cx| {
                this.picked.set(path, |_| {});
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for SetupView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self.path.clone();
        self.picked.flush(|text| {
            path.update(cx, |state, cx| state.set_value(text, window, cx));
        });
        let pal = Palette::active(cx);
        let laid_out = self.picked.laid_out().flag();
        let view = cx.entity();
        let group = |text: &'static str| {
            div()
                .mt(px(22.))
                .mb(px(8.))
                .text_size(px(FS_SM))
                .font_weight(WEIGHT_SEMIBOLD)
                .text_color(pal.ink_strong)
                .child(text)
        };
        let boxed = || {
            v_flex()
                .rounded(px(R_LG))
                .bg(pal.surface)
                .border_1()
                .border_color(pal.subtle)
                .overflow_hidden()
        };
        let use_view = view.clone();
        let browse_view = view.clone();
        let retry_view = view.clone();
        let mut install = boxed();
        for (ix, (name, command)) in INSTALL.iter().enumerate() {
            install = install.child(
                h_flex()
                    .px(px(16.))
                    .py(px(12.))
                    .gap(px(16.))
                    .items_center()
                    .when(ix > 0, |row| row.border_t_1().border_color(pal.subtle))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_size(px(FS_SM)).text_color(pal.ink).child(*name))
                            .child(
                                div()
                                    .font_family(FONT_MONO)
                                    .text_size(px(FS_XS))
                                    .text_color(pal.muted)
                                    .child(*command),
                            ),
                    )
                    .child(
                        btn_sm(format!("setup-copy-{ix}"), BtnTone::Plain, pal)
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    (*command).to_owned(),
                                ));
                            })
                            .child(Icon::new(IconName::Copy).with_size(px(12.)))
                            .child("Copy"),
                    ),
            );
        }
        install = install.child(
            h_flex()
                .px(px(16.))
                .py(px(12.))
                .gap(px(16.))
                .items_center()
                .border_t_1()
                .border_color(pal.subtle)
                .child(
                    v_flex()
                        .flex_1()
                        .child(
                            div()
                                .text_size(px(FS_SM))
                                .text_color(pal.ink)
                                .child("Installed it?"),
                        )
                        .child(
                            div()
                                .text_size(px(FS_XS))
                                .text_color(pal.muted)
                                .child("Check your PATH again."),
                        ),
                )
                .child(
                    btn_sm("setup-retry", BtnTone::Plain, pal)
                        .on_click(move |_, _, cx| {
                            retry_view.update(cx, |_, cx| cx.emit(SetupEvent::Retry));
                        })
                        .child(Icon::new(IconName::RefreshCw).with_size(px(12.)))
                        .child("Check again"),
                ),
        );
        div()
            .id("setup")
            .test_support()
            .size_full()
            .overflow_y_scroll()
            .flex()
            .justify_center()
            .child(
                v_flex()
                    .w(px(620.))
                    .py(px(48.))
                    .child(
                        v_flex()
                            .items_center()
                            .gap(px(10.))
                            .mb(px(12.))
                            .child(crate::views::eclipse::eclipse(44., None, pal))
                            .child(
                                div()
                                    .text_size(px(22.))
                                    .font_weight(WEIGHT_BOLD)
                                    .text_color(pal.ink_strong)
                                    .child("kage is not installed"),
                            )
                            .child(
                                div()
                                    .text_size(px(FS_SM))
                                    .text_color(pal.muted)
                                    .child(SharedString::from(format!(
                                        "This app runs kage rpc from your machine, and no kage was found on your PATH. It needs kage {MINIMUM_KAGE_VERSION} or newer."
                                    ))),
                            ),
                    )
                    .child(group("Use a kage you already have"))
                    .child(
                        boxed()
                            .child(
                                h_flex()
                                    .p(px(12.))
                                    .gap(px(8.))
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_1()
                                            .on_prepaint(move |_, _, _| laid_out.set(true))
                                            .child(Input::new(&self.path).small()),
                                    )
                                    .child(
                                        btn_sm("setup-browse", BtnTone::Plain, pal)
                                            .on_click(move |_, _, cx| {
                                                browse_view.update(cx, |this, cx| this.browse(cx));
                                            })
                                            .child("Browse\u{2026}"),
                                    )
                                    .child(
                                        btn_sm("setup-use", BtnTone::Primary, pal)
                                            .on_click(move |_, _, cx| {
                                                use_view.update(cx, |this, cx| this.use_path(cx));
                                            })
                                            .child("Use"),
                                    ),
                            )
                            .children(self.error.clone().map(|error| {
                                h_flex()
                                    .px(px(12.))
                                    .pb(px(10.))
                                    .gap(px(6.))
                                    .items_center()
                                    .text_size(px(FS_XS))
                                    .text_color(pal.danger)
                                    .child(Icon::new(IconName::TriangleAlert).with_size(px(12.)))
                                    .child(error)
                            })),
                    )
                    .child(group("Or install it yourself"))
                    .child(install)
                    .child(
                        div()
                            .mt(px(16.))
                            .text_size(px(FS_XS))
                            .text_color(pal.faint)
                            .child("kage runs as its own program and updates on its own, so this app and kage never have to ship together."),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::probe;

    #[test]
    fn a_path_that_does_not_run_is_refused_with_why() {
        let why = probe("/nonexistent/kage").unwrap_err();
        assert!(why.starts_with("cannot run /nonexistent/kage"), "{why}");
        let why = probe("true").unwrap_err();
        assert_eq!(why, "true is not kage");
    }
}
