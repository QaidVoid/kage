//! The browser entry: the same shell over a web-sys WebSocket.
//!
//! The crate compiles for wasm32-unknown-unknown and binds into the
//! static bundle with wasm-bindgen (`--target web`); the exact
//! commands, the serving headers and the measurements live in
//! `gui/SPIKE.md`. GPUI renders through `gpui-pre-web`: WebGPU when
//! the browser has it, an automatic WebGL2 canvas fallback when it
//! does not. The transport is [`WebTransport`]; the token rides the
//! `kage.<token>` subprotocol of the dial and never a query string.
//!
//! The page address carries the only configuration a browser page can
//! read: `?ws=` overrides the endpoint (default: `/acp` on the page's
//! own origin) and `?token=` carries the bearer token; without it the
//! page asks once through a prompt dialog. Both values configure the
//! page only, the WebSocket dial itself stays clean.

use std::borrow::Cow;

use wasm_bindgen::prelude::wasm_bindgen;

use gpui_kit::assets::Assets;
use gpui_kit::component::theme::Theme;
use gpui_kit::{App, AppContext, KeyBinding, TitlebarOptions, WindowOptions, px, size};

use crate::app::{Shell, ShellArgs};
use crate::theme;
use crate::transport::web::WebTransport;

/// The UI family, the one font the web text system names as its
/// fallback, bundled in `assets/fonts`.
const UI_FONT: &str = "Inter";

/// The monospace family for code and identifiers, bundled in
/// `assets/fonts`.
const MONO_FONT: &str = "JetBrains Mono";

/// The path of the ACP endpoint on the page's own origin.
const ACP_PATH: &str = "/acp";

/// Runs the shell in the page. Bound as the wasm-bindgen `start`
/// hook, so the generated glue calls it once the module loads and
/// the browser keeps driving it through its own event loop.
#[wasm_bindgen(start)]
pub fn start() {
    gpui_kit::platform::web_init();
    gpui_kit::application()
        .with_assets(Assets::default())
        .run(|cx: &mut App| {
            install_fonts(cx);
            gpui_kit::init(cx);
            theme::apply_shadow(cx);
            Theme::update(cx, |theme| {
                theme.font_family = UI_FONT.into();
                theme.mono_font_family = MONO_FONT.into();
            });
            cx.bind_keys([
                KeyBinding::new("ctrl-q", crate::app::Quit, None),
                KeyBinding::new("cmd-q", crate::app::Quit, None),
                KeyBinding::new("ctrl-n", crate::app::NewSession, None),
                KeyBinding::new("cmd-n", crate::app::NewSession, None),
                KeyBinding::new("ctrl-b", crate::app::ToggleWorkbench, None),
                KeyBinding::new("cmd-b", crate::app::ToggleWorkbench, None),
                KeyBinding::new("ctrl-\\", crate::app::ToggleSidebar, None),
                KeyBinding::new("cmd-\\", crate::app::ToggleSidebar, None),
                KeyBinding::new("ctrl-enter", crate::app::SendPrompt, None),
            ]);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("kage client".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(960.), px(640.))),
                ..Default::default()
            };
            let (handle, shell) = gpui_kit::open_window(options, cx, |window, cx| {
                let args = ShellArgs {
                    transport: Box::new(WebTransport::new(endpoint(), token())),
                    replay: false,
                    stream: false,
                };
                cx.new(|cx| Shell::new(args, window, cx))
            })
            .expect("failed to open the window");
            let shell_new = shell.clone();
            cx.on_action(move |_: &crate::app::Quit, cx| cx.quit());
            cx.on_action(move |_: &crate::app::NewSession, cx| {
                shell_new.update(cx, |shell, cx| shell.open_session(cx));
            });
            let shell_toggle_sidebar = shell.clone();
            cx.on_action(move |_: &crate::app::ToggleSidebar, cx| {
                shell_toggle_sidebar.update(cx, |shell, cx| shell.toggle_sidebar(cx));
            });
            let shell_toggle_workbench = shell.clone();
            cx.on_action(move |_: &crate::app::ToggleWorkbench, cx| {
                shell_toggle_workbench.update(cx, |shell, cx| shell.toggle_workbench(cx));
            });
            cx.on_action(move |_: &crate::app::SendPrompt, cx| {
                let _ = handle.update(cx, |root, window, cx| {
                    if let Ok(shell) = root.downcast::<Shell>() {
                        shell.update(cx, |shell, cx| shell.send_composer(window, cx));
                    }
                });
            });
        });
}

/// Loads the bundled fonts before anything lays out text: the web
/// text system starts with no fonts at all, and the first layout in
/// a family it cannot find would panic.
fn install_fonts(cx: &mut App) {
    cx.text_system()
        .add_fonts(vec![
            Cow::Borrowed(include_bytes!("../assets/fonts/Inter-Regular.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").as_slice()),
        ])
        .expect("the bundled fonts load");
}

/// The endpoint to dial: the page's `?ws=` value, else `/acp` on the
/// page's own origin with the scheme mapped to ws or wss.
fn endpoint() -> String {
    let search = page_search();
    if let Some(url) = query_parameter(&search, "ws") {
        return url;
    }
    web_sys::window()
        .map(|window| window.location())
        .and_then(|location| {
            let scheme = match location.protocol().ok()? {
                proto if proto == "https:" => "wss:",
                _ => "ws:",
            };
            let host = location.host().ok()?;
            Some(format!("{scheme}//{host}{ACP_PATH}"))
        })
        .unwrap_or_else(|| format!("ws://localhost:0{ACP_PATH}"))
}

/// The bearer token: the page's `?token=` value, else one ask
/// through a prompt dialog, else empty, which the endpoint refuses.
fn token() -> String {
    let search = page_search();
    if let Some(token) = query_parameter(&search, "token") {
        return token;
    }
    web_sys::window()
        .and_then(|window| window.prompt_with_message("kage token").ok().flatten())
        .unwrap_or_default()
}

/// The query part of the page address, empty without a window.
fn page_search() -> String {
    web_sys::window()
        .map(|window| window.location())
        .and_then(|location| location.search().ok())
        .unwrap_or_default()
}

/// The value of one query parameter of a search string, if present.
fn query_parameter(search: &str, name: &str) -> Option<String> {
    let query = search.strip_prefix('?').unwrap_or(search);
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}
