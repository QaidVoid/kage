//! The boot splash: the orbiting eclipse mark, the kage wordmark and a
//! connecting line over the whole window while the first connection is
//! made, faded out once the engine answers the handshake.
//!
//! Only the first connection shows it. A first attempt that fails takes
//! it down at once, so the shell's own connection status, or the setup
//! screen, is what the user sees.

use std::time::Duration;

use gpui_kit::component::v_flex;
use gpui_kit::{
    AnyElement, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _,
    Window, div, px,
};
use web_time::Instant;

use crate::theme::{FS_SM, Palette};
use crate::transport::State;

/// How long the splash takes to fade out.
const FADE: Duration = Duration::from_millis(300);

/// Where the boot splash stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Splash {
    /// Up while the first connection is made.
    Showing,
    /// Fading out since this instant.
    Fading(Instant),
    /// Gone for good.
    Gone,
}

impl Splash {
    /// Moves on with the connection: fades once `connect` is up and the
    /// handshake was `answered`, and goes at once when the first
    /// attempt failed.
    pub(crate) fn follow(&mut self, connect: &State, answered: bool) {
        match self {
            Self::Showing => match connect {
                State::Connected if answered => *self = Self::Fading(Instant::now()),
                State::Connecting | State::Connected => {}
                State::Refused(_) | State::Reconnecting { .. } | State::Closed => {
                    *self = Self::Gone
                }
            },
            Self::Fading(since) if since.elapsed() >= FADE => *self = Self::Gone,
            Self::Fading(_) | Self::Gone => {}
        }
    }

    /// The splash as it draws now, over the whole window, or nothing
    /// once it is gone.
    pub(crate) fn element(&self, window: &mut Window, pal: &Palette) -> Option<AnyElement> {
        let opacity = match self {
            Self::Showing => 1.0,
            Self::Fading(since) => {
                window.request_animation_frame();
                1.0 - (since.elapsed().as_secs_f32() / FADE.as_secs_f32()).min(1.0)
            }
            Self::Gone => return None,
        };
        Some(
            v_flex()
                .id("splash")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .items_center()
                .justify_center()
                .gap(px(14.))
                .bg(pal.bg)
                .opacity(opacity)
                .child(crate::views::eclipse::eclipse(
                    44.,
                    Some(crate::clock::epoch()),
                    pal,
                ))
                .child(
                    div()
                        .text_size(px(40.))
                        .font_weight(FontWeight::EXTRA_BOLD)
                        .text_color(pal.ink_strong)
                        .child("kage"),
                )
                .child(
                    div()
                        .text_size(px(FS_SM))
                        .text_color(pal.muted)
                        .child("Connecting\u{2026}"),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Splash;
    use crate::transport::State;

    #[test]
    fn the_splash_fades_once_the_handshake_answers() {
        let mut splash = Splash::Showing;
        splash.follow(&State::Connecting, false);
        assert_eq!(splash, Splash::Showing);
        splash.follow(&State::Connected, false);
        assert_eq!(splash, Splash::Showing, "up but not answered yet");
        splash.follow(&State::Connected, true);
        assert!(matches!(splash, Splash::Fading(_)));
    }

    #[test]
    fn a_failed_first_attempt_takes_the_splash_down() {
        for state in [
            State::Refused("bad token".into()),
            State::Reconnecting {
                attempt: 1,
                delay: Duration::from_secs(1),
            },
            State::Closed,
        ] {
            let mut splash = Splash::Showing;
            splash.follow(&state, false);
            assert_eq!(splash, Splash::Gone, "{state:?}");
        }
    }
}
