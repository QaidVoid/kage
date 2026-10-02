//! The desktop launch: flags, transport choice and the native window.
//!
//! One window over one transport: `kage rpc` as a child by default,
//! `kage serve` over WebSocket with `--ws` and `--token`, or the
//! recorded golden transcript with `--replay`. `--smoke` quits after
//! a delay and prints what the window reached, which is how the
//! headless runs are automated. The browser build replaces this
//! module with [`crate::web`].

use std::time::Duration;

use gpui_kit::assets::Assets;
use gpui_kit::{App, AppContext, Entity, TitlebarOptions, WindowOptions, px, size};

use crate::app::{Shell, ShellArgs};
use crate::theme;
use crate::transport::Transport;
use crate::transport::replay::ReplayTransport;
use crate::transport::stdio::{Config as StdioConfig, StdioTransport};
use crate::transport::ws::WsTransport;

/// Which transport the shell connects through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Wire {
    /// Spawn `kage rpc`, with an optional explicit binary.
    #[default]
    Stdio,
    /// Dial `kage serve` over WebSocket.
    WebSocket {
        /// The `ws://` endpoint, path included.
        url: String,
        /// The bearer token of the endpoint.
        token: String,
    },
    /// Play the recorded golden transcript.
    Replay,
}

/// Command line switches for automated runs and transport choice.
#[derive(Debug, Default)]
struct Launch {
    wire: Wire,
    /// Quit this many milliseconds after launch, for smoke runs.
    smoke_millis: Option<u64>,
    /// Run the 30 updates per second stream on open.
    stream: bool,
    /// Why the switches do not fit together, if they do not.
    error: Option<String>,
    /// Raw flags, resolved by [`Launch::finalize`].
    ws_url: Option<String>,
    token: Option<String>,
    rpc_bin: Option<String>,
    replay: bool,
}

impl Launch {
    fn parse(args: impl Iterator<Item = String>) -> Self {
        let mut launch = Self::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--smoke" => {
                    let millis = args.peek().and_then(|value| value.parse::<u64>().ok());
                    if millis.is_some() {
                        args.next();
                    }
                    launch.smoke_millis = Some(millis.unwrap_or(0));
                }
                "--stream" => launch.stream = true,
                "--replay" => launch.replay = true,
                "--ws" => match args.next() {
                    Some(url) => launch.ws_url = Some(url),
                    None => launch.error = Some("--ws needs a ws:// URL".to_owned()),
                },
                "--token" => match args.next() {
                    Some(token) => launch.token = Some(token),
                    None => launch.error = Some("--token needs a value".to_owned()),
                },
                "--rpc-bin" => match args.next() {
                    Some(program) => launch.rpc_bin = Some(program),
                    None => launch.error = Some("--rpc-bin needs a path".to_owned()),
                },
                _ => {}
            }
        }
        launch.finalize()
    }

    /// Resolves the raw flags into the wire to use.
    fn finalize(mut self) -> Self {
        if self.error.is_some() {
            return self;
        }
        self.wire = if let Some(url) = self.ws_url.take() {
            match self.token.take() {
                Some(token) => Wire::WebSocket { url, token },
                None => {
                    self.error =
                        Some("--ws needs --token; the token never rides the URL".to_owned());
                    return self;
                }
            }
        } else if self.token.take().is_some() {
            self.error = Some("--token needs --ws".to_owned());
            return self;
        } else if self.replay {
            Wire::Replay
        } else {
            Wire::Stdio
        };
        self
    }

    /// The transport the launch selected. Stdio runs `--rpc-bin`, else
    /// the binary the setup screen saved, else `kage` from the PATH.
    fn transport(&self, saved: Option<&str>) -> Box<dyn Transport> {
        match &self.wire {
            Wire::Stdio => {
                let config = match self.rpc_bin.as_deref().or(saved) {
                    Some(program) => StdioConfig {
                        program: program.to_owned(),
                        args: vec!["rpc".to_owned()],
                    },
                    None => StdioConfig::engine(),
                };
                Box::new(StdioTransport::new(config))
            }
            Wire::WebSocket { url, token } => {
                Box::new(WsTransport::new(url.clone(), token.clone()))
            }
            Wire::Replay => Box::new(ReplayTransport::new()),
        }
    }
}

/// Parses the flags, opens the window and runs until quit.
pub fn run() {
    let launch = Launch::parse(std::env::args().skip(1));
    if let Some(error) = &launch.error {
        eprintln!("kage-desktop: {error}");
        std::process::exit(2);
    }
    let replay = launch.wire == Wire::Replay;
    let stream = launch.stream;
    let smoke_millis = launch.smoke_millis;
    let prefs = crate::prefs::load();
    gpui_kit::application()
        .with_assets(crate::assets::KageAssets::new(Assets))
        .run(move |cx: &mut App| {
            theme::install_fonts(cx);
            gpui_kit::init(cx);
            cx.bind_keys(crate::app::key_bindings());
            cx.set_global(prefs.theme.clone());
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("kage client".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(960.), px(640.))),
                ..Default::default()
            };
            let args = ShellArgs {
                transport: launch.transport(prefs.kage_path.as_deref()),
                replay,
                stream,
                prefs,
            };
            let (_, shell) = gpui_kit::open_window(options, cx, move |window, cx| {
                // The window knows the platform appearance, so the theme is
                // chosen here rather than at `run`, and the observer keeps
                // it in step when the desktop flips.
                crate::theme::apply_choice(cx, window.appearance());
                let shell = cx.new(|cx| Shell::new(args, window, cx));
                window
                    .observe_window_appearance(|window, cx| {
                        crate::theme::apply_choice(cx, window.appearance());
                        window.refresh();
                    })
                    .detach();
                shell
            })
            .expect("failed to open the window");
            crate::app::route_actions(cx);

            if let Some(millis) = smoke_millis {
                quit_after(cx, shell, Duration::from_millis(millis));
            }
        });
}

/// Schedules the process to exit after `delay`, letting a headless run
/// exercise the event loop and exit on its own. The final store
/// counters go to stdout so automated runs report what happened.
fn quit_after(cx: &mut App, shell: Entity<Shell>, delay: Duration) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(delay).await;
        cx.update(|cx| {
            println!("{}", shell.read(cx).smoke_line(cx));
            cx.quit();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::{Launch, Wire};

    fn parse(args: &[&str]) -> Launch {
        Launch::parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn defaults_have_no_smoke_or_stream_and_speak_stdio() {
        let launch = parse(&[]);
        assert_eq!(launch.smoke_millis, None);
        assert!(!launch.stream);
        assert_eq!(launch.wire, Wire::Stdio);
        assert!(launch.rpc_bin.is_none());
    }

    #[test]
    fn smoke_takes_an_optional_millis_value() {
        assert_eq!(parse(&["--smoke", "2500"]).smoke_millis, Some(2500));
        assert_eq!(parse(&["--smoke"]).smoke_millis, Some(0));
    }

    #[test]
    fn stream_flag_stands_alone() {
        let launch = parse(&["--stream"]);
        assert!(launch.stream);
        assert_eq!(launch.smoke_millis, None);
    }

    #[test]
    fn replay_selects_the_recording() {
        assert_eq!(parse(&["--replay"]).wire, Wire::Replay);
    }

    #[test]
    fn ws_needs_a_token_and_the_token_needs_ws() {
        let launch = parse(&["--ws", "ws://127.0.0.1:7433/acp"]);
        assert!(launch.error.is_some(), "a tokenless ws is refused");

        let launch = parse(&["--token", "t"]);
        assert!(launch.error.is_some(), "a token without ws is refused");

        let launch = parse(&["--ws", "ws://127.0.0.1:7433/acp", "--token", "t"]);
        assert_eq!(
            launch.wire,
            Wire::WebSocket {
                url: "ws://127.0.0.1:7433/acp".to_owned(),
                token: "t".to_owned(),
            }
        );
    }

    #[test]
    fn rpc_bin_overrides_the_engine_binary() {
        assert_eq!(
            parse(&["--rpc-bin", "/opt/kage"]).rpc_bin.as_deref(),
            Some("/opt/kage")
        );
        assert_eq!(parse(&["--rpc-bin", "/opt/kage"]).wire, Wire::Stdio);
    }
}
