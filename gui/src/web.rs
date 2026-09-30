//! The browser entry: the same shell over a web-sys WebSocket.
//!
//! The crate compiles for wasm32-unknown-unknown and binds into the
//! static bundle with wasm-bindgen (`--target web`); the exact
//! commands and the serving headers live in `gui/SPIKE.md`. GPUI
//! renders through `gpui-pre-web`: WebGPU when the browser has it, an
//! automatic WebGL2 canvas fallback when it does not. The transport is
//! [`WebTransport`]; the token rides the `kage.<token>` subprotocol of
//! the dial and never a query string.
//!
//! The page's `boot.js` draws the connection form first and hands the
//! server URL and the token over through `window.__kageConnect` before
//! this module loads; there is no token URL parameter. The `?ws=`
//! query parameter stays as a developer override for the endpoint,
//! which without it defaults to `/acp` on the page's own origin.

use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast as _, JsValue};

use gpui_kit::assets::Assets;
use gpui_kit::{App, AppContext, KeyBinding, TitlebarOptions, WindowOptions, px, size};

use crate::app::{Shell, ShellArgs};
use crate::theme;
use crate::transport::web::WebTransport;

#[wasm_bindgen]
extern "C" {
    /// The connection values the boot form collected, stored on the
    /// window before this module loads.
    #[wasm_bindgen(thread_local_v2, js_namespace = window, js_name = __kageConnect)]
    static KAGE_CONNECT: JsValue;

    type Handoff;

    /// The server URL the form sent.
    #[wasm_bindgen(method, getter)]
    fn server(this: &Handoff) -> Option<String>;

    /// The token the form sent.
    #[wasm_bindgen(method, getter)]
    fn token(this: &Handoff) -> Option<String>;
}

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
            theme::install_fonts(cx);
            gpui_kit::init(cx);
            theme::apply_shadow(cx);
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
            let (server, token) = connection();
            let (handle, shell) = gpui_kit::open_window(options, cx, |window, cx| {
                let args = ShellArgs {
                    transport: Box::new(WebTransport::new(server, token)),
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

/// The endpoint and token the boot form handed over: the form's
/// values win, the endpoint falls back to the page's `?ws=` override,
/// else `/acp` on the page's own origin with the scheme mapped to ws
/// or wss. Without the form there is no token, and the endpoint
/// refuses the dial until one is entered.
fn connection() -> (String, String) {
    KAGE_CONNECT.with(|value| {
        let handed = value.is_object().then(|| value.unchecked_ref::<Handoff>());
        let server = handed
            .and_then(|handoff| handoff.server())
            .filter(|server| !server.is_empty())
            .unwrap_or_else(endpoint);
        let token = handed
            .and_then(|handoff| handoff.token())
            .unwrap_or_default();
        (server, token)
    })
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
