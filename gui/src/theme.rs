//! The kage web client design tokens, typed for the desktop shell.
//!
//! The values mirror the design's token sheet: two palettes (the dark
//! kage shadow set and its light kage-dawn counterpart), the type
//! scale, the spacing and radius scales, control and panel metrics,
//! and the shadow recipes. `apply_shadow` and `apply_dawn` install a
//! palette on the running app and map it onto the component library's
//! theme roles, so both frontends render the same colors as the web
//! client.

use std::borrow::Cow;
use std::sync::LazyLock;

use gpui_kit::base::motion::Easing;
use gpui_kit::component::theme::{ActiveTheme as _, Theme, ThemeColor, ThemeMode};
use gpui_kit::{App, BoxShadow, FontWeight, Hsla, WindowAppearance, px, rgba};
use serde::{Deserialize, Serialize};

/// The display family for brand moments and headings.
pub const FONT_DISPLAY: &str = "Schibsted Grotesk";

/// The body family for everything read as prose or UI text.
pub const FONT_BODY: &str = "Schibsted Grotesk";

/// The monospace family for code, identifiers and paths.
pub const FONT_MONO: &str = "JetBrains Mono";

/// Smallest text, badges and status pills.
pub const FS_2XS: f32 = 11.0;

/// Secondary small text, labels and hints.
pub const FS_XS: f32 = 12.0;

/// Small body text, sidebar rows and controls.
pub const FS_SM: f32 = 13.0;

/// The base body size of the whole app.
pub const FS_BASE: f32 = 14.0;

/// Emphasized body text and section titles.
pub const FS_LG: f32 = 16.0;

/// The relative line height of body text.
pub const LINE_HEIGHT: f32 = 1.5;

/// Tight tracking for brand marks, in em multiples of the font size.
pub const TRACKING_TIGHT: f32 = -0.03;

/// The wordmark's extra tight tracking, in em multiples.
pub const TRACKING_WORDMARK: f32 = -0.045;

/// Wide tracking for uppercase section labels, in em multiples.
pub const TRACKING_UPPER: f32 = 0.04;

/// Regular text weight.
pub const WEIGHT_REGULAR: FontWeight = FontWeight(400.0);

/// Semibold, used by section labels and row titles.
pub const WEIGHT_SEMIBOLD: FontWeight = FontWeight(600.0);

/// Bold, used by the brand and emphasized titles.
pub const WEIGHT_BOLD: FontWeight = FontWeight(700.0);

/// Extra bold, used by the welcome wordmark.
pub const WEIGHT_EXTRABOLD: FontWeight = FontWeight(800.0);

/// 2px.
pub const SP_1: f32 = 2.0;

/// 4px.
pub const SP_2: f32 = 4.0;

/// 6px.
pub const SP_3: f32 = 6.0;

/// 8px.
pub const SP_4: f32 = 8.0;

/// 12px.
pub const SP_5: f32 = 12.0;

/// 16px.
pub const SP_6: f32 = 16.0;

/// 20px.
pub const SP_7: f32 = 20.0;

/// 24px.
pub const SP_8: f32 = 24.0;

/// 32px.
pub const SP_9: f32 = 32.0;

/// 48px.
pub const SP_10: f32 = 48.0;

/// 4px, inputs and small chips.
pub const R_XS: f32 = 4.0;

/// 6px, small controls.
pub const R_SM: f32 = 6.0;

/// 8px, the general control radius.
pub const R_MD: f32 = 8.0;

/// 12px, cards and large buttons.
pub const R_LG: f32 = 12.0;

/// 16px, large surfaces.
pub const R_XL: f32 = 16.0;

/// 20px, extra large surfaces.
pub const R_2XL: f32 = 20.0;

/// 32px, the composer's pill shape.
pub const R_COMPOSER: f32 = 32.0;

/// Fully round, pills and dots.
pub const R_FULL: f32 = 999.0;

/// Icon-only control, 26px square.
pub const CTL_ICO: f32 = 26.0;

/// Small control height, 32px.
pub const CTL_SM: f32 = 32.0;

/// The panel head height, 48px.
pub const PANEL_HEAD_H: f32 = 48.0;

/// The workbench tab height, 28px.
pub const TAB_H: f32 = 28.0;

/// The sidebar's open width, 270px.
pub const SIDE_W: f32 = 270.0;

/// The centered content column width, 768px.
pub const CONTENT_W: f32 = 768.0;

/// The welcome block's width, 728px: the wordmark, the composer and the
/// cards all measure to this, narrower than the content column because the
/// welcome block is a card-like stack rather than a transcript.
pub const WELCOME_W: f32 = 728.0;

/// The welcome block's side padding, 24px, outside the column: the
/// composer, the cards and the wordmark all measure to
/// [`WELCOME_W`] wide whatever the pane does.
pub const WELCOME_PAD_X: f32 = 24.0;

/// The welcome block's top padding, 40px.
pub const WELCOME_PAD_TOP: f32 = 40.0;

/// The share of the viewport the welcome block's bottom pad takes, 12%:
/// the design pads the bottom by 12vh against 40px at the top, which is
/// what lifts the stack above the middle of the pane.
pub const WELCOME_PAD_BOTTOM_SHARE: f32 = 0.12;

/// Fast transition, in seconds.
pub const T_FAST: f32 = 0.12;

/// Medium transition, in seconds.
pub const T_MED: f32 = 0.16;

/// Slow transition, in seconds.
pub const T_SLOW: f32 = 0.26;

/// The shared motion curve of the design, cubic-bezier(0.16, 1, 0.3, 1).
pub fn ease() -> Easing {
    Easing::cubic_bezier(0.16, 1.0, 0.3, 1.0).expect("the design curve is valid")
}

/// The bundled family files. Both frontends load these before any
/// text is laid out, so the design families render even when the
/// machine has none of them installed.
/// Static per-weight faces. The engine selects a face by reported weight
/// and never applies variable-font axes, so a variable TTF would render
/// every request at its default instance.
const FONT_FILES: &[&[u8]] = &[
    include_bytes!("../assets/fonts/SchibstedGrotesk-Regular.ttf"),
    include_bytes!("../assets/fonts/SchibstedGrotesk-Medium.ttf"),
    include_bytes!("../assets/fonts/SchibstedGrotesk-SemiBold.ttf"),
    include_bytes!("../assets/fonts/SchibstedGrotesk-Bold.ttf"),
    include_bytes!("../assets/fonts/SchibstedGrotesk-ExtraBold.ttf"),
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Inter-Bold.ttf"),
    include_bytes!("../assets/fonts/Inter-ExtraBold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-SemiBold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-ExtraBold.ttf"),
];

/// Registers the three bundled families with the text system. Call
/// once at startup, before the first window opens.
pub fn install_fonts(cx: &mut App) {
    cx.text_system()
        .add_fonts(
            FONT_FILES
                .iter()
                .map(|bytes| Cow::Borrowed(*bytes))
                .collect::<Vec<Cow<'static, [u8]>>>(),
        )
        .expect("the bundled fonts load");
}

/// One agent avatar role: a soft fill behind, a palette hue in front.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AvatarColor {
    /// The chip fill, the theme's soft fill.
    pub bg: Hsla,
    /// The glyph color, one of the palette hues.
    pub fg: Hsla,
}

/// The eight agent avatar hues of the design.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Avatars {
    /// Warm yellow avatar hue.
    pub amber: AvatarColor,
    /// Red avatar hue.
    pub rose: AvatarColor,
    /// Orange avatar hue, the accent family.
    pub orange: AvatarColor,
    /// Teal green avatar hue.
    pub emerald: AvatarColor,
    /// Teal cyan avatar hue.
    pub cyan: AvatarColor,
    /// Blue avatar hue, the info family.
    pub sky: AvatarColor,
    /// Violet avatar hue, the swarm family.
    pub violet: AvatarColor,
    /// Pink avatar hue.
    pub pink: AvatarColor,
}

/// Every color token of one theme, named after the design's roles.
#[derive(Clone, Debug)]
pub struct Palette {
    /// App background.
    pub bg: Hsla,
    /// Sidebar and workbench background.
    pub sidebar: Hsla,
    /// Cards, panels and inputs.
    pub surface: Hsla,
    /// Raised elements above `surface`.
    pub raised: Hsla,
    /// Pressed-in areas.
    pub sunken: Hsla,
    /// The deepest level, used behind on-color glyphs.
    pub deep: Hsla,
    /// Wells that read as inset surfaces.
    pub well: Hsla,
    /// Barely-there fill for chips and pills.
    pub fill: Hsla,
    /// `fill` one step up, for hover.
    pub fill_hover: Hsla,
    /// Primary text.
    pub ink: Hsla,
    /// Emphasized text.
    pub ink_strong: Hsla,
    /// Secondary text.
    pub muted: Hsla,
    /// Tertiary text.
    pub faint: Hsla,
    /// Quaternary text and the faintest marks.
    pub ghost: Hsla,
    /// Hairline borders.
    pub line: Hsla,
    /// The faintest fills, below `line`.
    pub subtle: Hsla,
    /// Stronger borders, hovered states.
    pub line_strong: Hsla,
    /// Row and control hover fill.
    pub hover: Hsla,
    /// Selected row fill.
    pub selected: Hsla,
    /// Selected row fill on hover.
    pub selected_hover: Hsla,
    /// The warm lantern accent.
    pub accent: Hsla,
    /// Accent hover step.
    pub accent_hover: Hsla,
    /// Accent tint background.
    pub accent_soft: Hsla,
    /// Accent border.
    pub accent_bd: Hsla,
    /// Success green.
    pub ok: Hsla,
    /// Success tint background.
    pub ok_soft: Hsla,
    /// Success border.
    pub ok_bd: Hsla,
    /// Text on a solid success fill.
    pub ok_ink: Hsla,
    /// Warning yellow.
    pub warn: Hsla,
    /// Warning tint background.
    pub warn_soft: Hsla,
    /// Warning border.
    pub warn_bd: Hsla,
    /// Danger red.
    pub danger: Hsla,
    /// Danger tint background.
    pub danger_soft: Hsla,
    /// Danger border.
    pub danger_bd: Hsla,
    /// Swarm violet.
    pub done: Hsla,
    /// Violet tint background.
    pub done_soft: Hsla,
    /// Violet border.
    pub done_bd: Hsla,
    /// Informational blue.
    pub info: Hsla,
    /// The composer's background.
    pub composer_bg: Hsla,
    /// The composer's idle border.
    pub composer_line: Hsla,
    /// The composer's focused border.
    pub composer_focus_line: Hsla,
    /// The send button background.
    pub send_bg: Hsla,
    /// The send button hover background.
    pub send_bg_hover: Hsla,
    /// The glyph on the send button.
    pub send_icon: Hsla,
    /// The send button when disabled.
    pub send_bg_off: Hsla,
    /// The glyph on the disabled send button.
    pub send_icon_off: Hsla,
    /// The stop glyph, a softened danger red.
    pub stop_glyph: Hsla,
    /// Diff added lines.
    pub diff_add: Hsla,
    /// Diff added line background.
    pub diff_add_bg: Hsla,
    /// Diff deleted lines.
    pub diff_del: Hsla,
    /// Diff deleted line background.
    pub diff_del_bg: Hsla,
    /// The user bubble fill.
    pub bubble: Hsla,
    /// Text selection fill.
    pub selection: Hsla,
    /// Menus and popovers, slightly translucent.
    pub menu: Hsla,
    /// Inline code fill.
    pub code_inline: Hsla,
    /// Brand orb gradient start.
    pub orb_1: Hsla,
    /// Brand orb gradient end.
    pub orb_2: Hsla,
    /// The agent avatar hues.
    pub avatars: Avatars,
    /// Small ambient shadow.
    pub shadow_1: Vec<BoxShadow>,
    /// Large flyout shadow.
    pub shadow_2: Vec<BoxShadow>,
    /// Subtle input shadow.
    pub shadow_input: Vec<BoxShadow>,
    /// The send button's layered shadow.
    pub shadow_send: Vec<BoxShadow>,
    /// Menu shadow.
    pub shadow_menu: Vec<BoxShadow>,
}

/// Converts one hex color to the toolkit's color type.
///
/// A 6-digit literal is opaque; an 8-digit one carries alpha in its last
/// byte, as the design's own tokens do (`0xF2A65A42` is the accent at 26
/// percent). GPUI's [`rgba`] always reads four bytes, so passing a 6-digit
/// value straight through would take its blue as alpha and draw every
/// surface at that opacity: `0x0F0E13` became 7 percent, which is what
/// shifted the whole shell off the palette.
fn color(hex: u32) -> Hsla {
    let opaque = match hex.checked_shr(24) {
        Some(0) | None => (hex << 8) | 0xFF,
        Some(_) => hex,
    };
    rgba(opaque).into()
}

/// Builds one box shadow layer from the design's shadow recipe. Shadow
/// colors always carry alpha, so `hex` is read as all four bytes: black
/// at 55 percent is `0x0000008C`, which [`color`] would take for blue.
fn layer(hex: u32, x: f32, y: f32, blur: f32, spread: f32) -> BoxShadow {
    BoxShadow::new(px(x), px(y), rgba(hex).into())
        .blur_radius(px(blur))
        .spread_radius(px(spread))
}

impl Palette {
    /// The palette of the active theme mode.
    #[must_use]
    pub fn active(cx: &App) -> &'static Self {
        static SHADOW: LazyLock<Palette> = LazyLock::new(Palette::shadow);
        static DAWN: LazyLock<Palette> = LazyLock::new(Palette::dawn);
        if cx.theme().mode.is_dark() {
            &SHADOW
        } else {
            &DAWN
        }
    }

    /// The dark palette: ink surfaces with a violet undertone, one
    /// warm lantern accent, violet reserved for swarms.
    pub fn shadow() -> Self {
        Self {
            bg: color(0x0F0E13),
            sidebar: color(0x0B0A0E),
            surface: color(0x17151D),
            raised: color(0x221F2A),
            sunken: color(0x0B0A0E),
            deep: color(0x09080C),
            well: color(0x17151D),
            fill: color(0xE8E2F50D),
            fill_hover: color(0xE8E2F516),
            ink: color(0xECE8F6DB),
            ink_strong: color(0xF4F1FA),
            muted: color(0xECE8F694),
            faint: color(0xECE8F66B),
            ghost: color(0xECE8F640),
            line: color(0xE8E2F51C),
            subtle: color(0xE8E2F50D),
            line_strong: color(0xE8E2F530),
            hover: color(0xE8E2F50D),
            selected: color(0xE8E2F516),
            selected_hover: color(0xE8E2F51F),
            accent: color(0xF2A65A),
            accent_hover: color(0xF6B674),
            accent_soft: color(0xF2A65A1F),
            accent_bd: color(0xF2A65A57),
            ok: color(0x8BD49C),
            ok_soft: color(0x8BD49C1C),
            ok_bd: color(0x8BD49C4D),
            ok_ink: color(0x0B1A10),
            warn: color(0xE8C96A),
            warn_soft: color(0xE8C96A1C),
            warn_bd: color(0xE8C96A4D),
            danger: color(0xF2727F),
            danger_soft: color(0xF2727F1C),
            danger_bd: color(0xF2727F4D),
            done: color(0xA98BFA),
            done_soft: color(0xA98BFA1F),
            done_bd: color(0xA98BFA57),
            info: color(0x9AB4FF),
            composer_bg: color(0x15131B),
            composer_line: color(0xE8E2F51C),
            composer_focus_line: color(0xF2A65A6B),
            send_bg: color(0xF2A65A),
            send_bg_hover: color(0xF6B674),
            send_icon: color(0x1A1208),
            send_bg_off: color(0xE8E2F514),
            send_icon_off: color(0xE8E2F542),
            stop_glyph: color(0xF2727FBF),
            diff_add: color(0x8BD49C),
            diff_add_bg: color(0x8BD49C1F),
            diff_del: color(0xF2727F),
            diff_del_bg: color(0xF2727F1F),
            bubble: color(0x221F2A),
            selection: color(0xF2A65A42),
            menu: color(0x16141CF2),
            code_inline: color(0xE8E2F514),
            orb_1: color(0xF2A65A),
            orb_2: color(0xA98BFA),
            avatars: Avatars {
                amber: AvatarColor {
                    bg: color(0xE8E2F50D),
                    fg: color(0xE8C96A),
                },
                rose: AvatarColor {
                    bg: color(0xE8E2F50D),
                    fg: color(0xF2727F),
                },
                orange: AvatarColor {
                    bg: color(0xE8E2F50D),
                    fg: color(0xF2A65A),
                },
                emerald: AvatarColor {
                    bg: color(0xE8E2F50D),
                    fg: color(0x5FC9B0),
                },
                cyan: AvatarColor {
                    bg: color(0xE8E2F50D),
                    fg: color(0x7DD3D0),
                },
                sky: AvatarColor {
                    bg: color(0xE8E2F50D),
                    fg: color(0x9AB4FF),
                },
                violet: AvatarColor {
                    bg: color(0xE8E2F50D),
                    fg: color(0xA98BFA),
                },
                pink: AvatarColor {
                    bg: color(0xE8E2F50D),
                    fg: color(0xF29AC1),
                },
            },
            shadow_1: vec![layer(0x00000066, 0., 1., 2., 0.)],
            shadow_2: vec![layer(0x0000008C, 0., 16., 40., 0.)],
            shadow_input: vec![layer(0x00000012, 0., 5., 16., -4.)],
            shadow_send: vec![
                layer(0x00000061, 0., 7., 16., -13.),
                layer(0x00000012, 0., 1., 2., 0.),
            ],
            shadow_menu: vec![
                layer(0x00000099, 0., 16., 48., 0.),
                layer(0x00000066, 0., 2., 8., 0.),
            ],
        }
    }

    /// The light palette: ink on violet-tinted paper, mapped onto the
    /// same roles as the dark palette.
    pub fn dawn() -> Self {
        Self {
            bg: color(0xF7F5F9),
            sidebar: color(0xEEEBF2),
            surface: color(0xEFECF3),
            raised: color(0xE4DFEB),
            sunken: color(0xEEEBF2),
            deep: color(0xFFFFFF),
            well: color(0xEFECF3),
            fill: color(0x1A142E0B),
            fill_hover: color(0x1A142E12),
            ink: color(0x1A142EED),
            ink_strong: color(0x15121C),
            muted: color(0x1A142EB8),
            faint: color(0x1A142E7D),
            ghost: color(0x1A142E4D),
            line: color(0x1A142E1F),
            subtle: color(0x1A142E0D),
            line_strong: color(0x1A142E33),
            hover: color(0x1A142E0B),
            selected: color(0x1A142E12),
            selected_hover: color(0x1A142E1A),
            accent: color(0xA8580F),
            accent_hover: color(0x924E11),
            accent_soft: color(0xA8580F1A),
            accent_bd: color(0xA8580F4D),
            ok: color(0x2B7443),
            ok_soft: color(0x2B74431A),
            ok_bd: color(0x2B74434D),
            ok_ink: color(0x0B1A10),
            warn: color(0x836409),
            warn_soft: color(0x8364091A),
            warn_bd: color(0x8364094D),
            danger: color(0xBF3649),
            danger_soft: color(0xBF36491A),
            danger_bd: color(0xBF36494D),
            done: color(0x6A47CC),
            done_soft: color(0x6A47CC1A),
            done_bd: color(0x6A47CC4D),
            info: color(0x3456C2),
            composer_bg: color(0xFFFFFF),
            composer_line: color(0x1A142E1F),
            composer_focus_line: color(0xA8580F73),
            send_bg: color(0xA8580F),
            send_bg_hover: color(0x924E11),
            send_icon: color(0xFFFFFF),
            send_bg_off: color(0x1A142E14),
            send_icon_off: color(0x1A142E4D),
            stop_glyph: color(0xBF3649BF),
            diff_add: color(0x2B7443),
            diff_add_bg: color(0x2B74431F),
            diff_del: color(0xBF3649),
            diff_del_bg: color(0xBF36491F),
            bubble: color(0xE4DFEB),
            selection: color(0xA8580F33),
            menu: color(0xFFFFFFF2),
            code_inline: color(0x1A142E0F),
            orb_1: color(0xA8580F),
            orb_2: color(0x6A47CC),
            avatars: Avatars {
                amber: AvatarColor {
                    bg: color(0x1A142E0B),
                    fg: color(0x836409),
                },
                rose: AvatarColor {
                    bg: color(0x1A142E0B),
                    fg: color(0xBF3649),
                },
                orange: AvatarColor {
                    bg: color(0x1A142E0B),
                    fg: color(0xA8580F),
                },
                emerald: AvatarColor {
                    bg: color(0x1A142E0B),
                    fg: color(0x1C7563),
                },
                cyan: AvatarColor {
                    bg: color(0x1A142E0B),
                    fg: color(0x157A84),
                },
                sky: AvatarColor {
                    bg: color(0x1A142E0B),
                    fg: color(0x3456C2),
                },
                violet: AvatarColor {
                    bg: color(0x1A142E0B),
                    fg: color(0x6A47CC),
                },
                pink: AvatarColor {
                    bg: color(0x1A142E0B),
                    fg: color(0xAC3A7A),
                },
            },
            shadow_1: vec![layer(0x1A142E1A, 0., 1., 2., 0.)],
            shadow_2: vec![layer(0x1A142E29, 0., 12., 32., 0.)],
            shadow_input: vec![layer(0x1A142E14, 0., 5., 16., -4.)],
            shadow_send: vec![
                layer(0x1A142E4D, 0., 7., 16., -13.),
                layer(0x1A142E12, 0., 1., 2., 0.),
            ],
            shadow_menu: vec![
                layer(0x1A142E2E, 0., 12., 40., 0.),
                layer(0x1A142E14, 0., 2., 8., 0.),
            ],
        }
    }
}

/// Maps one palette onto the component library's theme roles. Roles
/// the design has no token for carry the nearest palette value: the
/// on-color glyph tokens (`send_icon`, `deep`) sit behind accent and
/// status fills, the composer's focus line drives the ring, and links
/// use the accent as the web client styles them.
fn theme_colors(p: &Palette, mode: ThemeMode) -> ThemeColor {
    let mut c = match mode {
        ThemeMode::Light => *ThemeColor::light(),
        _ => *ThemeColor::dark(),
    };
    c.background = p.bg;
    c.foreground = p.ink;
    c.border = p.line;
    c.window_border = p.raised;
    c.title_bar = p.bg;
    c.title_bar_border = p.line;
    c.status_bar = p.bg;
    c.status_bar_border = p.line;
    c.muted = p.surface;
    c.muted_foreground = p.muted;
    c.secondary = p.raised;
    c.secondary_foreground = p.ink_strong;
    c.secondary_hover = p.hover;
    c.secondary_active = p.selected;
    c.primary = p.accent;
    c.primary_foreground = p.send_icon;
    c.primary_hover = p.accent_hover;
    c.primary_active = p.accent_hover;
    c.accent = p.selected;
    c.accent_foreground = p.ink_strong;
    c.selection = p.selection;
    c.ring = p.composer_focus_line;
    c.caret = p.accent;
    c.link = p.accent;
    c.link_hover = p.accent_hover;
    c.input = p.surface;
    c.list = p.bg;
    c.list_even = p.surface;
    c.list_hover = p.hover;
    c.list_active = p.selected;
    c.list_active_border = p.accent_bd;
    c.popover = p.menu;
    c.popover_foreground = p.ink;
    c.overlay = p.menu;
    c.tab_bar = p.bg;
    c.tab = p.bg;
    c.tab_foreground = p.muted;
    c.tab_active = p.surface;
    c.tab_active_foreground = p.ink_strong;
    c.sidebar = p.sidebar;
    c.sidebar_foreground = p.muted;
    c.sidebar_border = p.line;
    c.sidebar_accent = p.selected;
    c.sidebar_accent_foreground = p.ink_strong;
    c.sidebar_primary = p.accent;
    c.scrollbar_thumb = p.line;
    c.scrollbar_thumb_hover = p.line_strong;
    c.danger = p.danger;
    c.danger_foreground = p.deep;
    c.warning = p.warn;
    c.warning_foreground = p.deep;
    c.success = p.ok;
    c.success_foreground = p.deep;
    c.info = p.info;
    c.info_foreground = p.deep;
    c.red = p.danger;
    c.green = p.ok;
    c.blue = p.info;
    c.yellow = p.warn;
    c.magenta = p.done;
    c.cyan = p.avatars.cyan.fg;
    c.red_light = p.ink_strong;
    c.green_light = p.ink_strong;
    c.blue_light = p.ink_strong;
    c.yellow_light = p.ink_strong;
    c.magenta_light = p.ink_strong;
    c.cyan_light = p.ink_strong;
    c
}

/// Installs one palette as the theme of the running app and names the
/// bundled families and the metric tokens on it.
fn apply(palette: &Palette, mode: ThemeMode, cx: &mut App) {
    // Both families are named in the same edit as the colors, after the mode
    // change, so the library's own font probes find a real family to keep and
    // skip the `.SystemUIFont` lookup that the web cannot satisfy.
    Theme::change(mode, None, cx);
    Theme::update(cx, |theme| {
        theme.colors = theme_colors(palette, mode);
        theme.font_family = FONT_BODY.into();
        theme.mono_font_family = FONT_MONO.into();
        theme.font_size = px(FS_BASE);
        theme.mono_font_size = px(FS_SM);
        theme.radius = px(R_MD);
        theme.radius_lg = px(R_LG);
    });
}

/// Installs the dark kage theme as the theme of the running app.
pub fn apply_shadow(cx: &mut App) {
    apply(&Palette::shadow(), ThemeMode::Dark, cx);
}

/// Installs the light kage theme as the theme of the running app.
pub fn apply_dawn(cx: &mut App) {
    apply(&Palette::dawn(), ThemeMode::Light, cx);
}

/// The theme the user chose: one of the two kage themes, or System,
/// which follows the platform's appearance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    /// Dawn on a light desktop, shadow otherwise.
    #[default]
    System,
    /// The dark kage theme.
    Shadow,
    /// The light kage theme.
    Dawn,
}

impl gpui_kit::Global for ThemeChoice {}

impl ThemeChoice {
    /// The label the theme cards show.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Shadow => "kage shadow",
            Self::Dawn => "kage dawn",
        }
    }
}

/// Installs the theme the stored choice names, reading the platform's
/// `appearance` for System. Without a stored choice this is System.
pub fn apply_choice(cx: &mut App, appearance: WindowAppearance) {
    match cx.try_global::<ThemeChoice>().copied().unwrap_or_default() {
        ThemeChoice::System => apply_system(cx, appearance),
        ThemeChoice::Shadow => apply_shadow(cx),
        ThemeChoice::Dawn => apply_dawn(cx),
    }
}

/// Installs whichever palette the platform's appearance asks for: dawn on a
/// light desktop, shadow otherwise. This is the System default: no stored
/// choice, so the platform decides.
pub fn apply_system(cx: &mut App, appearance: WindowAppearance) {
    let light = matches!(
        appearance,
        WindowAppearance::Light | WindowAppearance::VibrantLight
    );
    if light {
        apply_dawn(cx);
    } else {
        apply_shadow(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::Rgba;

    #[test]
    fn a_six_digit_token_is_opaque() {
        // The design's fills are RGB triples, and GPUI's `rgba` reads a
        // fourth byte as alpha. Passing a 6-digit value straight through
        // drew `bg` at 7 percent, which shifted the whole shell off the
        // palette, so the conversion has to supply the missing byte.
        for hex in [0x0F0E13, 0x0B0A0E, 0x17151D, 0xF2A65A, 0x8BD49C] {
            assert_eq!(color(hex).a, 1.0, "{hex:#010x} must be opaque");
        }
        // And the channels are the ones asked for. The palette stores
        // Hsla, so the round trip costs a fraction of a step per channel;
        // a whole step would be a real shift.
        let bg = Rgba::from(color(0x0F0E13));
        let one_step = 1.0 / 255.0;
        for (got, want) in [(bg.r, 15), (bg.g, 14), (bg.b, 19)] {
            assert!(
                (got - want as f32 / 255.0).abs() < one_step,
                "bg keeps its own channels: {got} against {want}"
            );
        }
    }

    #[test]
    fn an_eight_digit_token_keeps_its_alpha() {
        // The design writes translucency deliberately: `line` is a
        // hairline at 11 percent, the focus ring at 42, and body ink at
        // 86. Those have to survive the conversion, or every soft fill
        // lands solid.
        assert_eq!(color(0xE8E2F51C).a, 0x1C as f32 / 255.0, "line");
        assert_eq!(color(0xF2A65A42).a, 0x42 as f32 / 255.0, "selection");
        assert_eq!(color(0xECE8F6DB).a, 0xDB as f32 / 255.0, "ink");
        assert_eq!(color(0xF2A65A).a, 1.0, "the 6-digit accent beside it");
    }

    #[gpui_kit::test]
    fn the_system_default_follows_the_appearance(cx: &mut gpui_kit::TestAppContext) {
        // Dawn on a light desktop, shadow otherwise, which is what E7.1
        // step 3 asks for.
        for (appearance, dawn) in [
            (WindowAppearance::Light, true),
            (WindowAppearance::VibrantLight, true),
            (WindowAppearance::Dark, false),
            (WindowAppearance::VibrantDark, false),
        ] {
            cx.update(|app| {
                gpui_kit::init(app);
                apply_system(app, appearance);
                let theme = app.theme();
                assert_eq!(
                    theme.mode,
                    if dawn {
                        ThemeMode::Light
                    } else {
                        ThemeMode::Dark
                    },
                    "{appearance:?} must pick the matching palette"
                );
                assert_eq!(
                    theme.colors.background.a, 1.0,
                    "{appearance:?} must paint an opaque background"
                );
            });
        }
    }

    #[test]
    fn the_theme_fills_are_opaque() {
        // Every large surface is a 6-digit token, so one that reached the
        // window translucent would show the backdrop through the whole
        // app rather than on a single element.
        let p = Palette::shadow();
        for (name, token) in [
            ("bg", p.bg),
            ("sidebar", p.sidebar),
            ("surface", p.surface),
            ("raised", p.raised),
            ("sunken", p.sunken),
            ("deep", p.deep),
            ("well", p.well),
            ("composer_bg", p.composer_bg),
            ("bubble", p.bubble),
        ] {
            assert_eq!(token.a, 1.0, "{name} must be opaque");
        }
    }

    #[test]
    fn shadow_palette_matches_the_design_tokens() {
        let p = Palette::shadow();
        assert_eq!(p.bg, color(0x0F0E13));
        assert_eq!(p.accent, color(0xF2A65A));
        assert_eq!(p.ink, color(0xECE8F6DB));
        assert_eq!(p.line, color(0xE8E2F51C));
        assert_eq!(p.selection, color(0xF2A65A42));
        assert_eq!(p.menu, color(0x16141CF2));
        assert_eq!(p.avatars.violet.fg, color(0xA98BFA));
    }

    #[test]
    fn dawn_palette_matches_the_design_tokens() {
        let p = Palette::dawn();
        assert_eq!(p.bg, color(0xF7F5F9));
        assert_eq!(p.accent, color(0xA8580F));
        assert_eq!(p.ink, color(0x1A142EED));
        assert_eq!(p.selection, color(0xA8580F33));
        assert_eq!(p.menu, color(0xFFFFFFF2));
        assert_eq!(p.avatars.emerald.fg, color(0x1C7563));
    }

    #[test]
    fn theme_roles_carry_the_palette() {
        let p = Palette::shadow();
        let dark = theme_colors(&p, ThemeMode::Dark);
        assert_eq!(dark.background, p.bg);
        assert_eq!(dark.primary, p.accent);
        assert_eq!(dark.sidebar, p.sidebar);
        assert_eq!(dark.ring, p.composer_focus_line);

        let dawn = Palette::dawn();
        let light = theme_colors(&dawn, ThemeMode::Light);
        assert_eq!(light.background, dawn.bg);
        assert_eq!(light.primary, dawn.accent);
    }

    #[test]
    fn scale_and_metrics_match_the_design_tokens() {
        assert_eq!(FS_2XS, 11.0);
        assert_eq!(FS_BASE, 14.0);
        assert_eq!(FS_LG, 16.0);
        assert_eq!(LINE_HEIGHT, 1.5);
        assert_eq!(SP_1, 2.0);
        assert_eq!(SP_10, 48.0);
        assert_eq!(R_XS, 4.0);
        assert_eq!(R_COMPOSER, 32.0);
        assert_eq!(R_FULL, 999.0);
        assert_eq!(CTL_ICO, 26.0);
        assert_eq!(CTL_SM, 32.0);
        assert_eq!(PANEL_HEAD_H, 48.0);
        assert_eq!(TAB_H, 28.0);
        assert_eq!(SIDE_W, 270.0);
        assert_eq!(CONTENT_W, 768.0);
        assert_eq!(WELCOME_W, 728.0);
        assert_eq!(T_FAST, 0.12);
        assert_eq!(T_SLOW, 0.26);
    }

    #[test]
    fn families_and_weights_are_the_design_families() {
        assert_eq!(FONT_DISPLAY, "Schibsted Grotesk");
        assert_eq!(FONT_BODY, "Schibsted Grotesk");
        assert_eq!(FONT_MONO, "JetBrains Mono");
        assert_eq!(WEIGHT_EXTRABOLD, FontWeight(800.0));
    }

    #[test]
    fn body_and_mono_name_bundled_families_not_the_virtual_ones() {
        // The component library skips its `.SystemUIFont` probe only when
        // the theme already names a family. Naming a virtual or empty
        // family hands the probe back a family with no web fallback, and
        // the app panics on its first text layout instead of booting.
        assert_ne!(FONT_BODY, ".SystemUIFont");
        assert_ne!(FONT_MONO, ".SystemUIFont");
        assert!(!FONT_BODY.is_empty());
        assert!(!FONT_MONO.is_empty());
        assert!(
            FONT_FILES.iter().all(|bytes| !bytes.is_empty()),
            "each bundled family has bytes to register"
        );
    }

    /// The virtual family GPUI and the component library both default to.
    /// A mode change writes it back over the theme, so `apply` has to name
    /// the bundled family after the change and never before it.
    const VIRTUAL_UI_FONT: &str = ".SystemUIFont";

    #[test]
    fn dark_shadows_are_black_not_blue() {
        let shadow = super::Palette::shadow();
        let color = shadow.shadow_2[0].color;
        assert!(color.l < 0.01, "{color:?}");
        assert!((color.a - 0x8C as f32 / 255.).abs() < 0.01, "{color:?}");
    }

    #[gpui_kit::test]
    fn the_installed_theme_carries_the_bundled_families(cx: &mut App) {
        // `apply_shadow` runs the library's real write path: the mode change
        // reloads the registered theme, which carries `.SystemUIFont`, and
        // only the edit after it survives. A theme left carrying the virtual
        // family looks correct until the first text is laid out, and then
        // takes the whole window down, so assert against the live theme
        // rather than against the constants this module happens to name.
        install_fonts(cx);
        gpui_kit::init(cx);
        apply_shadow(cx);
        let theme = cx.theme();
        assert_eq!(theme.font_family.as_ref(), FONT_BODY);
        assert_eq!(theme.mono_font_family.as_ref(), FONT_MONO);
        assert_ne!(theme.font_family.as_ref(), VIRTUAL_UI_FONT);
        assert_ne!(theme.mono_font_family.as_ref(), VIRTUAL_UI_FONT);
    }

    #[gpui_kit::test]
    fn the_light_theme_carries_the_bundled_families_too(cx: &mut App) {
        // The same has to hold for the dawn palette: a light mode change
        // reloads the light theme, which is a second chance to write the
        // virtual family back.
        install_fonts(cx);
        gpui_kit::init(cx);
        apply_dawn(cx);
        let theme = cx.theme();
        assert_eq!(theme.font_family.as_ref(), FONT_BODY);
        assert_eq!(theme.mono_font_family.as_ref(), FONT_MONO);
    }
}
