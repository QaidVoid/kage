//! The opt-in vim layer: normal-mode motions over the transcript rows,
//! folds, find keys, a `:` command line and a modeline.
//!
//! Off by default. Every binding here lives in the `VimNormal` key
//! context, which only exists while vim mode is on: the shell tracks
//! that focus on the chat column only then, so with the mode off no key
//! reaches these actions and nothing here renders.

use gpui_kit::component::h_flex;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{Div, KeyBinding, ParentElement as _, SharedString, Styled as _, div, px};

use crate::theme::{FONT_MONO, Palette, ThemeChoice};

gpui_kit::actions!(
    kage_vim,
    [
        /// Moves the cursor one row down.
        VimDown,
        /// Moves the cursor one row up.
        VimUp,
        /// Jumps to the first row.
        VimTop,
        /// Jumps to the last row.
        VimBottom,
        /// Moves the cursor half a page down.
        VimPageDown,
        /// Moves the cursor half a page up.
        VimPageUp,
        /// Opens or closes the row under the cursor.
        VimToggle,
        /// Opens every row.
        VimOpenAll,
        /// Closes every row.
        VimCloseAll,
        /// Focuses the composer.
        VimInsert,
        /// Opens find.
        VimFind,
        /// Steps to the next find match.
        VimNext,
        /// Steps to the previous find match.
        VimPrev,
        /// Opens the `:` command line.
        VimCommand,
        /// Interrupts the running turn.
        VimEscape,
        /// Leaves the composer for normal mode.
        VimLeaveInsert,
    ]
);

/// The key context the normal-mode bindings answer in.
pub const NORMAL: &str = "VimNormal";

/// How many rows Ctrl+D and Ctrl+U move.
pub const PAGE: usize = 6;

/// The normal-mode bindings, all in the [`NORMAL`] context.
#[must_use]
pub fn bindings() -> Vec<KeyBinding> {
    let ctx = Some(NORMAL);
    vec![
        KeyBinding::new("j", VimDown, ctx),
        KeyBinding::new("down", VimDown, ctx),
        KeyBinding::new("k", VimUp, ctx),
        KeyBinding::new("up", VimUp, ctx),
        KeyBinding::new("g g", VimTop, ctx),
        KeyBinding::new("shift-g", VimBottom, ctx),
        KeyBinding::new("ctrl-d", VimPageDown, ctx),
        KeyBinding::new("ctrl-u", VimPageUp, ctx),
        KeyBinding::new("z a", VimToggle, ctx),
        KeyBinding::new("z o", VimToggle, ctx),
        KeyBinding::new("z c", VimToggle, ctx),
        KeyBinding::new("enter", VimToggle, ctx),
        KeyBinding::new("z shift-r", VimOpenAll, ctx),
        KeyBinding::new("z shift-m", VimCloseAll, ctx),
        KeyBinding::new("i", VimInsert, ctx),
        KeyBinding::new("a", VimInsert, ctx),
        KeyBinding::new("/", VimFind, ctx),
        KeyBinding::new("n", VimNext, ctx),
        KeyBinding::new("shift-n", VimPrev, ctx),
        KeyBinding::new("shift-;", VimCommand, ctx),
        KeyBinding::new(":", VimCommand, ctx),
        KeyBinding::new("shift-:", VimCommand, ctx),
        KeyBinding::new("escape", VimEscape, ctx),
    ]
}

/// One `:` command, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `theme <name>`: switch themes; `None` names no known theme.
    Theme(Option<ThemeChoice>),
    /// `model <name>`: pick the model whose value or name matches.
    Model(String),
    /// `swarm on|off`.
    Swarm(bool),
    /// `plan on|off`.
    Plan(bool),
    /// `goal <text>` sets the goal; `goal` or `goal clear` clears it.
    Goal(Option<String>),
    /// `new`: the welcome pane.
    New,
    /// `settings` or `set`.
    Settings,
    /// `compact`: summarize older turns now.
    Compact,
    /// `noh`: clear the find highlight.
    NoHighlight,
    /// `q`: close the workbench.
    Quit,
    /// `help` or `h`: the keyboard page.
    Help,
    /// `vim on|off`.
    Vim(bool),
    /// An empty line.
    Nothing,
    /// Anything else, by its first word.
    Unknown(String),
}

/// Parses one `:` line.
#[must_use]
pub fn parse(line: &str) -> Command {
    let line = line.trim();
    let (word, arg) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let arg = arg.trim();
    let on = arg != "off";
    match word {
        "" => Command::Nothing,
        "theme" => Command::Theme(match arg.to_lowercase().as_str() {
            "" => None,
            "system" => Some(ThemeChoice::System),
            "shadow" | "kage" | "kage shadow" | "dark" => {
                Some(ThemeChoice::Named("kage-shadow".to_owned()))
            }
            "dawn" | "kage dawn" | "light" => Some(ThemeChoice::Named("kage-dawn".to_owned())),
            _ => Some(ThemeChoice::Named(arg.to_owned())),
        }),
        "model" => Command::Model(arg.to_owned()),
        "swarm" => Command::Swarm(on),
        "plan" => Command::Plan(on),
        "goal" => Command::Goal((!arg.is_empty() && arg != "clear").then(|| arg.to_owned())),
        "new" => Command::New,
        "settings" | "set" => Command::Settings,
        "compact" => Command::Compact,
        "noh" | "nohlsearch" => Command::NoHighlight,
        "q" | "quit" => Command::Quit,
        "help" | "h" => Command::Help,
        "vim" => Command::Vim(on),
        other => Command::Unknown(other.to_owned()),
    }
}

/// Where the keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Keys move over the transcript.
    Normal,
    /// The composer has the keys.
    Insert,
    /// An approval waits; the digits answer it.
    Approval,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Normal => "-- NORMAL --",
            Mode::Insert => "-- INSERT --",
            Mode::Approval => "-- APPROVAL --",
        }
    }
}

/// What the modeline shows.
#[derive(Debug, Clone, Default)]
pub struct Modeline {
    /// The mode slot.
    pub mode: Option<Mode>,
    /// The model and thinking level, `model@level`.
    pub model: Option<String>,
    /// Whether swarm mode is on.
    pub swarm: bool,
    /// Whether plan mode is on.
    pub plan: bool,
    /// Todos done and total.
    pub todos: Option<(usize, usize)>,
    /// The session's working directory.
    pub path: Option<String>,
    /// The cursor row and the row total, one based.
    pub cursor: Option<(usize, usize)>,
    /// Context percent, tokens used and the window.
    pub usage: Option<String>,
    /// The session cost.
    pub cost: Option<String>,
    /// The link state.
    pub link: String,
}

impl Modeline {
    /// The modeline bar.
    pub fn render(&self, pal: &Palette) -> Div {
        let mode = self.mode.unwrap_or(Mode::Normal);
        let label = div()
            .font_weight(crate::theme::WEIGHT_BOLD)
            .map(|label| match mode {
                Mode::Normal => label.text_color(pal.ink_strong),
                Mode::Insert => label.text_color(pal.ok),
                Mode::Approval => label
                    .px(px(6.))
                    .rounded(px(3.))
                    .bg(pal.ok)
                    .text_color(pal.sidebar),
            })
            .child(mode.label());
        let item = |text: String| div().child(SharedString::from(text));
        h_flex()
            .flex_none()
            .h(px(26.))
            .px(px(12.))
            .gap(px(12.))
            .items_center()
            .font_family(FONT_MONO)
            .text_size(px(11.5))
            .text_color(pal.muted)
            .border_t_1()
            .border_color(pal.line)
            .bg(pal.sidebar)
            .child(label)
            .children(self.model.clone().map(item))
            .when(self.swarm, |line| {
                line.child(div().text_color(pal.done).child("swarm"))
            })
            .when(self.plan, |line| {
                line.child(div().text_color(pal.accent).child("plan"))
            })
            .children(
                self.todos
                    .map(|(done, total)| item(format!("todo {done}/{total}"))),
            )
            .children(self.path.clone().map(item))
            .child(
                h_flex()
                    .ml_auto()
                    .gap(px(12.))
                    .children(
                        self.cursor
                            .map(|(row, total)| item(format!("{row}/{total}"))),
                    )
                    .children(self.usage.clone().map(item))
                    .children(self.cost.clone().map(item))
                    .child(item(self.link.clone())),
            )
    }
}

/// The cursor row after one motion from `at` over `total` rows. With no
/// cursor yet, moving down starts at the top and moving up at the
/// bottom.
#[must_use]
pub fn step(at: Option<usize>, total: usize, delta: isize) -> Option<usize> {
    if total == 0 {
        return None;
    }
    let last = total - 1;
    let next = match at {
        Some(at) => at.saturating_add_signed(delta).min(last),
        None if delta < 0 => last,
        None => 0,
    };
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::{Command, parse, step};
    use crate::theme::ThemeChoice;

    #[test]
    fn the_command_table_parses_like_the_prototype() {
        assert_eq!(
            parse("theme dawn"),
            Command::Theme(Some(ThemeChoice::Named("kage-dawn".into())))
        );
        assert_eq!(
            parse("theme kimi-dark"),
            Command::Theme(Some(ThemeChoice::Named("kimi-dark".into())))
        );
        assert_eq!(parse("theme"), Command::Theme(None));
        assert_eq!(parse("swarm off"), Command::Swarm(false));
        assert_eq!(parse("swarm"), Command::Swarm(true));
        assert_eq!(parse("plan on"), Command::Plan(true));
        assert_eq!(
            parse("goal tests pass"),
            Command::Goal(Some("tests pass".into()))
        );
        assert_eq!(parse("goal clear"), Command::Goal(None));
        assert_eq!(parse("  "), Command::Nothing);
        assert_eq!(parse("q"), Command::Quit);
        assert_eq!(parse("wq"), Command::Unknown("wq".into()));
    }

    #[test]
    fn motions_clamp_and_start_from_the_ends() {
        assert_eq!(step(None, 5, 1), Some(0));
        assert_eq!(step(None, 5, -1), Some(4));
        assert_eq!(step(Some(4), 5, 6), Some(4));
        assert_eq!(step(Some(2), 5, -6), Some(0));
        assert_eq!(step(Some(0), 0, 1), None);
    }
}
