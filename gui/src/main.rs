//! The kage desktop client.

mod theme;
mod window;

use std::time::Duration;

use gpui_kit::assets::Assets;
use gpui_kit::{App, KeyBinding};

gpui_kit::actions!(kage_desktop, [Quit]);

/// Command line switches for automated runs and measurements.
#[derive(Debug, Default)]
struct Launch {
    /// Quit this many milliseconds after launch, for smoke runs.
    smoke_millis: Option<u64>,
    /// Start the 30 updates per second stream on open.
    stream: bool,
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
                _ => {}
            }
        }
        launch
    }
}

fn main() {
    let launch = Launch::parse(std::env::args().skip(1));
    gpui_kit::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            theme::apply_shadow(cx);
            cx.bind_keys([
                KeyBinding::new("ctrl-q", Quit, None),
                KeyBinding::new("cmd-q", Quit, None),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            let spike = window::open(cx, &launch);
            if let Some(millis) = launch.smoke_millis {
                quit_after(cx, spike, Duration::from_millis(millis));
            }
        });
}

/// Schedule the process to exit after `delay`, letting a headless run
/// exercise the event loop and exit on its own. The final list counters go
/// to stdout so automated runs report what the window did.
fn quit_after(cx: &mut App, spike: window::SpikeHandle, delay: Duration) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(delay).await;
        cx.update(|cx| {
            let (rows, streamed) = spike.counts(cx);
            println!("smoke: rows={rows} streamed={streamed}");
            cx.quit();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::Launch;

    fn parse(args: &[&str]) -> Launch {
        Launch::parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn defaults_have_no_smoke_or_stream() {
        let launch = parse(&[]);
        assert_eq!(launch.smoke_millis, None);
        assert!(!launch.stream);
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
}
